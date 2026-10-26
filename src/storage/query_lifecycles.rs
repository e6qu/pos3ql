//! Stored-query edits and undo retain their selected object and revision.

use super::catalog_sources::{
    CatalogDdlDecision, CatalogDdlOperation, CatalogDdlUndo, CatalogEditTarget,
    CatalogObjectIdentity, CatalogObjectSlot,
};
use super::{
    CatalogDdlState, PendingRuleDefinition, PendingViewDefinition, SqlError, Storage,
    current_database, sqlstate,
};
use crate::sql_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewCatalogIdentity {
    slot: u16,
    created_at: u64,
    return_rule: u16,
    return_rule_created_at: u64,
}

impl ViewCatalogIdentity {
    pub(crate) const ENCODED_BYTES: usize = 20;

    pub(crate) fn new(
        slot: u16,
        created_at: u64,
        return_rule: u16,
        return_rule_created_at: u64,
    ) -> Option<Self> {
        (super::RelationIdentity::parse(created_at).is_some()
            && return_rule_created_at != 0
            && created_at != return_rule_created_at)
            .then_some(Self {
                slot,
                created_at,
                return_rule,
                return_rule_created_at,
            })
    }

    pub(crate) fn slot(self) -> usize {
        usize::from(self.slot)
    }
    pub(crate) fn created_at(self) -> u64 {
        self.created_at
    }
    pub(crate) fn return_rule_slot(self) -> usize {
        usize::from(self.return_rule)
    }
    pub(crate) fn return_rule_created_at(self) -> u64 {
        self.return_rule_created_at
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuleAlterUndo {
    pub(super) target: CatalogEditTarget,
    pub(super) prior: Option<PendingRuleDefinition>,
}

impl RuleAlterUndo {
    pub(crate) fn slot(self) -> usize {
        self.target.slot()
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "Rule undo stays in the startup-reserved transaction log"
)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum RulePublication {
    Created(CatalogDdlUndo),
    Altered(RuleAlterUndo),
}

impl RulePublication {
    pub(crate) fn slot(self) -> usize {
        match self {
            Self::Created(undo) => undo.slot(),
            Self::Altered(undo) => undo.slot(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewAlterUndo {
    pub(super) target: CatalogEditTarget,
    pub(super) prior: Option<PendingViewDefinition>,
    pub(super) return_rule: Option<RuleAlterUndo>,
}

impl ViewAlterUndo {
    pub(crate) fn slot(self) -> usize {
        self.target.slot()
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "View undo stays inline in the startup-reserved transaction log"
)]
#[derive(Debug, Clone, Copy)]
pub(crate) enum ViewPublication {
    Created(CatalogDdlUndo),
    Altered(ViewAlterUndo),
}

impl ViewPublication {
    pub(crate) fn slot(self) -> usize {
        match self {
            Self::Created(undo) => undo.slot(),
            Self::Altered(undo) => undo.slot(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingMatviewPopulation {
    pub(super) transaction: u32,
    pub(super) version: u64,
    pub(super) populated: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MatviewPopulationUndo {
    target: CatalogEditTarget,
    prior: Option<PendingMatviewPopulation>,
}

impl MatviewPopulationUndo {
    pub(crate) fn slot(self) -> usize {
        self.target.slot()
    }
}

impl Storage {
    pub(crate) fn view_catalog_identity(&self, slot: usize) -> ViewCatalogIdentity {
        let catalog = self.view_rule_catalog();
        let view = catalog.views[slot];
        let rule = catalog.rules[usize::from(view.return_rule)];
        ViewCatalogIdentity::new(
            slot as u16,
            view.created_at,
            view.return_rule,
            rule.created_at,
        )
        .expect("live view and return rule have distinct identities")
    }

    pub(crate) fn replay_view(
        &mut self,
        identity: ViewCatalogIdentity,
        schema: super::SqlName,
        name: super::SqlName,
        definition: super::ViewDefinition,
    ) -> Result<(), SqlError> {
        let slot = identity.slot();
        let rule_slot = identity.return_rule_slot();
        if self.find_schema_visible(schema.as_str(), 0).is_none() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal view schema is unavailable"
            ));
        }
        let initial_owner = self.initial_ownership(0)?;
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::ViewDefinition,
            None,
        );
        let view = owners.catalog.views.get(slot).copied().ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal view slot is out of range"
            )
        })?;
        let rule = owners
            .catalog
            .rules
            .get(rule_slot)
            .copied()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "journal return-rule slot is out of range"
                )
            })?;
        let replacing = view.ddl_state != CatalogDdlState::Absent;
        if replacing
            && (view.database != current_database()
                || view.created_at != identity.created_at()
                || view.return_rule != rule_slot as u16
                || view.schema != schema
                || view.name != name
                || view.ddl_state != CatalogDdlState::Present
                || view.pending_definition.is_some())
            || if replacing {
                rule.database != current_database()
                    || rule.created_at != identity.return_rule_created_at()
                    || rule.ddl_state != CatalogDdlState::Present
                    || rule.pending.is_some()
                    || !rule.definition.is_view_return()
                    || rule.definition.target != super::RuleTarget::View(slot as u16)
            } else {
                rule.ddl_state != CatalogDdlState::Absent
            }
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal view identity is unavailable"
            ));
        }
        if owners
            .catalog
            .views
            .iter()
            .enumerate()
            .any(|(other, view)| {
                other != slot
                    && view.database == current_database()
                    && view.ddl_state != CatalogDdlState::Absent
                    && view.schema == schema
                    && view.name == name
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal contains a duplicate view name"
            ));
        }
        if owners
            .catalog
            .rules
            .iter()
            .enumerate()
            .any(|(other, rule)| {
                other != rule_slot
                    && rule.database == current_database()
                    && rule.ddl_state != CatalogDdlState::Absent
                    && rule.created_at == identity.return_rule_created_at()
            })
        {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "journal contains a duplicate return-rule identity"
            ));
        }
        self.require_row_type_name(
            owners.types.as_deref().expect("view recovery owns types"),
            schema,
            name,
            0,
            replacing.then_some(super::RuleTarget::View(slot as u16)),
            Some(&owners.catalog),
        )?;
        owners.write_query_image(
            self,
            self.committed_dependency_image(super::StoredQueryDependencyOwner::Rule(
                rule_slot as u16,
            )),
            definition.query.dependencies.view(),
            0,
        )?;
        if !replacing {
            Self::clear_object_acl_entries_locked(
                owners
                    .permissions
                    .as_mut()
                    .expect("view recovery owns permissions"),
                super::AccessObject {
                    class: super::AccessClass::View,
                    slot: slot as u16,
                },
            );
        }
        let mut rule_definition = super::RuleDefinition::view_return(slot, &definition.query);
        if replacing {
            rule_definition.enabled = rule.definition.enabled;
        }
        owners.catalog.rules[rule_slot] = super::RuleDef {
            database: current_database(),
            created_at: identity.return_rule_created_at(),
            version: identity.return_rule_created_at(),
            ddl_version: identity.return_rule_created_at(),
            pending_parent_drop_txid: None,
            definition: rule_definition,
            pending: None,
            ddl_state: CatalogDdlState::Present,
        };
        owners.catalog.views[slot] = super::ViewDef {
            database: current_database(),
            created_at: identity.created_at(),
            schema,
            name,
            persistence: definition.persistence,
            return_rule: rule_slot as u16,
            options: definition.options,
            columns: definition.columns,
            ownership: if replacing {
                view.ownership
            } else {
                initial_owner
            },
            version: identity.created_at(),
            ddl_version: identity.created_at(),
            pending_definition: None,
            ddl_state: CatalogDdlState::Present,
        };
        drop(owners);
        self.catalog_sequence
            .observe(identity.created_at().max(identity.return_rule_created_at()));
        Ok(())
    }

    pub(super) fn stage_query_comment_drop_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, super::ViewRuleCatalog>,
        source: CatalogObjectIdentity,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let (schema, name, rule_subid) = match source.slot() {
            CatalogObjectSlot::View(slot) => {
                let view = owners.catalog.views[slot];
                (
                    view.schema_for(transaction),
                    view.name_for(transaction),
                    None,
                )
            }
            CatalogObjectSlot::Rule(slot) => {
                let rule = owners.catalog.rules[slot].definition_for(transaction);
                (
                    super::SqlName::EMPTY,
                    rule.name,
                    Some(rule.target.comment_subid()),
                )
            }
            CatalogObjectSlot::Table(_)
            | CatalogObjectSlot::MaterializedView(_)
            | CatalogObjectSlot::Sequence(_)
            | CatalogObjectSlot::Routine(_) => {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "comment source is not a view or rewrite rule"
                ));
            }
        };
        let comments = owners
            .comments
            .as_mut()
            .expect("query retirement owns comments");
        self.stage_query_comment_drop(comments, source, schema, name, rule_subid, transaction)
    }

    fn stage_query_comment_drop(
        &self,
        comments: &mut super::CommentCatalog,
        source: CatalogObjectIdentity,
        schema: super::SqlName,
        name: super::SqlName,
        rule_subid: Option<u32>,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let matches = |comment: &super::CommentEntry| {
            let class_matches = match rule_subid {
                Some(subid) => comment.class == super::CommentClass::Rule && comment.subid == subid,
                None => matches!(
                    comment.class,
                    super::CommentClass::Relation | super::CommentClass::Type
                ),
            };
            class_matches
                && comment.matches_to(
                    Some(current_database()),
                    comment.class,
                    schema.as_str(),
                    name.as_str(),
                    comment.subid,
                    transaction,
                )
        };
        self.stage_catalog_comment_drop(comments, source, transaction, matches)
    }

    fn rollback_query_comment_drop_in(
        &self,
        comments: &mut super::CommentCatalog,
        source: CatalogObjectIdentity,
        transaction: u32,
    ) {
        for comment in comments.entries.iter_mut().filter(|comment| {
            comment
                .pending_drop
                .is_some_and(|pending| pending.txid == transaction && pending.source == source)
        }) {
            comment.pending_drop = None;
        }
    }

    fn rule_ddl_matches(&self, rule: &super::RuleDef, undo: CatalogDdlUndo) -> bool {
        matches!(undo.source.slot(), CatalogObjectSlot::Rule(_))
            && undo
                .source
                .matches(self, rule.database, rule.created_at, rule.ddl_state)
            && match undo.operation {
                CatalogDdlOperation::Create => {
                    rule.ddl_state.pending_txid() == Some(undo.transaction)
                        && matches!(
                            rule.ddl_state,
                            CatalogDdlState::PendingCreate { .. }
                                | CatalogDdlState::PendingCreateDrop { .. }
                        )
                }
                CatalogDdlOperation::Drop => {
                    rule.ddl_state.pending_drop_txid() == Some(undo.transaction)
                        && rule.ddl_version == undo.version
                }
            }
    }

    fn retire_rule_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, super::ViewRuleCatalog>,
        slot: usize,
    ) {
        let rule = owners.catalog.rules[slot];
        let source = self.catalog_object_identity(
            CatalogObjectSlot::Rule(slot),
            rule.database,
            rule.created_at,
        );
        for comment in owners
            .comments
            .as_mut()
            .expect("rule retirement owns comments")
            .entries
            .iter_mut()
            .filter(|comment| {
                comment.pending_drop.map_or_else(
                    || rule.ddl_state.pending_drop_txid().is_none(),
                    |pending| pending.source == source,
                ) && comment.used
                    && comment.database == Some(current_database())
                    && comment.class == super::CommentClass::Rule
                    && comment.name == rule.definition.name
                    && comment.subid == rule.definition.target.comment_subid()
            })
        {
            *comment = super::CommentEntry::empty();
        }
        let mut next = rule.pending.map(|pending| pending.dependency_slot);
        while let Some(pending_slot) = next {
            let pending = &mut owners.dependencies.pending[pending_slot as usize];
            next = pending.previous;
            *pending = super::PendingStoredQueryDependencies::EMPTY;
        }
        let image =
            self.committed_dependency_image(super::StoredQueryDependencyOwner::Rule(slot as u16));
        owners.dependencies.counts[image] = 0;
        owners.catalog.rules[slot] = super::RuleDef::EMPTY;
    }

    pub(crate) fn discard_rule_create(&self, publication: RulePublication) {
        let RulePublication::Created(undo) = publication else {
            return;
        };
        let CatalogObjectSlot::Rule(slot) = undo.source.slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        if owners
            .catalog
            .rules
            .get(slot)
            .is_some_and(|rule| self.rule_ddl_matches(rule, undo))
        {
            self.retire_rule_in(&mut owners, slot);
        }
    }

    pub(crate) fn finish_rule_publication(
        &mut self,
        publication: RulePublication,
        prior_table_rule_txid: Option<u32>,
        decision: CatalogDdlDecision,
    ) {
        let undo = match publication {
            RulePublication::Created(undo) => undo,
            RulePublication::Altered(undo) => {
                match decision {
                    CatalogDdlDecision::Commit => self.commit_rule_alter(undo),
                    CatalogDdlDecision::Rollback => self.rollback_rule_alter(undo),
                }
                return;
            }
        };
        let CatalogObjectSlot::Rule(slot) = undo.source.slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(rule) = owners
            .catalog
            .rules
            .get(slot)
            .copied()
            .filter(|rule| self.rule_ddl_matches(rule, undo))
        else {
            return;
        };
        match decision {
            CatalogDdlDecision::Commit => {
                self.publish_pending_rule_in(&mut owners, slot, undo.transaction);
                owners.catalog.rules[slot].ddl_state = rule.ddl_state.commit_create();
            }
            CatalogDdlDecision::Rollback => self.retire_rule_in(&mut owners, slot),
        }
        drop(owners);
        if let super::RuleTarget::Table(table) = rule.definition.target {
            let mut state = self.tables[usize::from(table)].definition.write();
            match decision {
                CatalogDdlDecision::Commit => {
                    state.mark_rules_present();
                    if state.pending_has_rules_txid == Some(undo.transaction) {
                        state.pending_has_rules_txid = None;
                    }
                }
                CatalogDdlDecision::Rollback => {
                    if state.pending_has_rules_txid == Some(undo.transaction) {
                        state.pending_has_rules_txid = prior_table_rule_txid;
                    }
                }
            }
        }
    }

    pub(crate) fn drop_rule(
        &self,
        target: CatalogEditTarget,
        transaction: u32,
    ) -> Result<CatalogDdlUndo, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let catalog = &mut owners.catalog;
        let slot = self.validate_rule_edit(catalog, target, transaction)?;
        let version = self.catalog_sequence.next()?;
        self.stage_query_comment_drop_in(&mut owners, target.source, transaction)?;
        let rule = &mut owners.catalog.rules[slot];
        let prior_version = rule.ddl_version;
        rule.ddl_state = rule.ddl_state.drop_by(transaction);
        rule.ddl_version = version;
        Ok(CatalogDdlUndo {
            source: target.source,
            transaction,
            operation: CatalogDdlOperation::Drop,
            version,
            prior_version,
        })
    }

    pub(crate) fn commit_rule_drop(&self, undo: CatalogDdlUndo) {
        let CatalogObjectSlot::Rule(slot) = undo.source.slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        if undo.operation == CatalogDdlOperation::Drop
            && owners
                .catalog
                .rules
                .get(slot)
                .is_some_and(|rule| self.rule_ddl_matches(rule, undo))
        {
            self.retire_rule_in(&mut owners, slot);
        }
    }

    pub(crate) fn rollback_rule_drop(&self, undo: CatalogDdlUndo) {
        let CatalogObjectSlot::Rule(slot) = undo.source.slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let catalog = &mut owners.catalog;
        let Some(rule) = catalog
            .rules
            .get_mut(slot)
            .filter(|rule| self.rule_ddl_matches(rule, undo))
        else {
            return;
        };
        if undo.operation != CatalogDdlOperation::Drop {
            return;
        }
        rule.ddl_state = rule.ddl_state.rollback_drop(undo.transaction);
        rule.ddl_version = undo.prior_version;
        self.rollback_query_comment_drop_in(
            owners
                .comments
                .as_mut()
                .expect("rule rollback owns comments"),
            undo.source,
            undo.transaction,
        );
    }

    pub(crate) fn validate_rule_publication(
        &self,
        publication: RulePublication,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let catalog = self.view_rule_catalog();
        let valid = match publication {
            RulePublication::Created(undo) => {
                undo.transaction == transaction
                    && catalog
                        .rules
                        .get(undo.slot())
                        .is_some_and(|rule| self.rule_ddl_matches(rule, undo))
            }
            RulePublication::Altered(undo) => {
                undo.target.transaction == transaction
                    && catalog.rules.get(undo.slot()).is_some_and(|rule| {
                        undo.target.source.matches(
                            self,
                            rule.database,
                            rule.created_at,
                            rule.ddl_state,
                        ) && rule.pending.is_some_and(|pending| {
                            pending.txid == transaction && pending.version >= undo.target.version
                        })
                    })
            }
        };
        if !valid {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "rewrite rule publication changed before commit"
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_rule_drop(
        &self,
        undo: CatalogDdlUndo,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let catalog = self.view_rule_catalog();
        if undo.transaction != transaction
            || undo.operation != CatalogDdlOperation::Drop
            || !catalog
                .rules
                .get(undo.slot())
                .is_some_and(|rule| self.rule_ddl_matches(rule, undo))
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "rewrite rule drop changed before commit"
            ));
        }
        Ok(())
    }

    pub(crate) fn prepare_rule_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<CatalogEditTarget, SqlError> {
        let catalog = self.view_rule_catalog();
        let rule = catalog
            .rules
            .get(slot)
            .filter(|rule| rule.database == current_database() && rule.visible_to(transaction))
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "rewrite rule no longer exists"))?;
        if let Some(owner) = rule
            .pending
            .map(|pending| pending.txid)
            .or_else(|| rule.ddl_state.pending_txid())
            .filter(|owner| *owner != transaction)
        {
            let name = rule.definition.name;
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
        }
        Ok(CatalogEditTarget {
            source: self.catalog_object_identity(
                CatalogObjectSlot::Rule(slot),
                rule.database,
                rule.created_at,
            ),
            transaction,
            version: rule.pending.map_or(rule.version, |pending| pending.version),
        })
    }

    pub(super) fn validate_rule_edit(
        &self,
        catalog: &super::ViewRuleCatalog,
        target: CatalogEditTarget,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let slot = target.rule_slot()?;
        let rule = catalog
            .rules
            .get(slot)
            .filter(|rule| {
                target
                    .source
                    .matches(self, rule.database, rule.created_at, rule.ddl_state)
                    && rule.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "rewrite rule source has been replaced"
                )
            })?;
        if target.transaction != transaction
            || rule
                .pending
                .is_some_and(|pending| pending.txid != transaction)
            || rule.pending.map_or(rule.version, |pending| pending.version) != target.version
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "rewrite rule changed before publication"
            ));
        }
        Ok(slot)
    }

    fn publish_pending_rule_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, super::ViewRuleCatalog>,
        slot: usize,
        transaction: u32,
    ) {
        let Some(pending) = owners.catalog.rules[slot]
            .pending
            .filter(|pending| pending.txid == transaction)
        else {
            return;
        };
        let old = owners.catalog.rules[slot].definition;
        self.commit_pending_dependencies_in(
            &mut owners.dependencies,
            super::StoredQueryDependencyOwner::Rule(slot as u16),
            pending.dependency_slot,
        );
        owners.catalog.rules[slot].definition = pending.definition;
        owners.catalog.rules[slot].version = pending.version;
        owners.catalog.rules[slot].pending = None;
        for comment in owners
            .comments
            .as_mut()
            .expect("rule publication owns comments")
            .entries
            .iter_mut()
            .filter(|comment| {
                comment.used
                    && comment.database == Some(current_database())
                    && comment.class == super::CommentClass::Rule
                    && comment.name == old.name
                    && comment.subid == old.target.comment_subid()
                    && comment
                        .pending_identity
                        .is_some_and(|identity| identity.txid == transaction)
            })
        {
            let identity = comment
                .pending_identity
                .take()
                .expect("selected comment identity");
            comment.schema = identity.schema;
            comment.name = identity.name;
        }
    }

    pub(crate) fn commit_rule_alter(&self, undo: RuleAlterUndo) {
        let target = undo.target;
        let Ok(slot) = target.rule_slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(rule) = owners.catalog.rules.get(slot).copied().filter(|rule| {
            target
                .source
                .matches(self, rule.database, rule.created_at, rule.ddl_state)
                && rule.pending.is_some_and(|pending| {
                    pending.txid == target.transaction && pending.version == target.version
                })
        }) else {
            return;
        };
        if rule.definition.is_view_return()
            && let super::RuleTarget::View(view) = rule.definition.target
            && (owners.catalog.views[usize::from(view)]
                .pending_definition
                .is_some_and(|pending| pending.txid == target.transaction)
                || owners.catalog.views[usize::from(view)]
                    .ddl_state
                    .pending_txid()
                    == Some(target.transaction))
        {
            return;
        }
        self.publish_pending_rule_in(&mut owners, slot, target.transaction);
    }

    pub(crate) fn rollback_rule_alter(&self, undo: RuleAlterUndo) {
        let target = undo.target;
        let Ok(slot) = target.rule_slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(rule) = owners.catalog.rules.get(slot).copied().filter(|rule| {
            target
                .source
                .matches(self, rule.database, rule.created_at, rule.ddl_state)
        }) else {
            return;
        };
        let Some(current) = rule.pending.filter(|pending| {
            pending.txid == target.transaction && pending.version == target.version
        }) else {
            return;
        };
        let restored = undo
            .prior
            .map_or(rule.definition, |pending| pending.definition);
        owners.dependencies.pending[current.dependency_slot as usize] =
            super::PendingStoredQueryDependencies::EMPTY;
        owners.catalog.rules[slot].pending = undo.prior;
        for comment in owners
            .comments
            .as_mut()
            .expect("rule undo owns comments")
            .entries
            .iter_mut()
            .filter(|comment| {
                comment.used
                    && comment.database == Some(current_database())
                    && comment.class == super::CommentClass::Rule
                    && comment.name == rule.definition.name
                    && comment.subid == rule.definition.target.comment_subid()
                    && comment
                        .pending_identity
                        .is_some_and(|identity| identity.txid == target.transaction)
            })
        {
            comment.pending_identity =
                (restored.name != comment.name).then_some(super::PendingCommentIdentity {
                    txid: target.transaction,
                    schema: super::SqlName::EMPTY,
                    name: restored.name,
                });
        }
    }

    pub(crate) fn validate_view_publication(
        &self,
        publication: ViewPublication,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let (source, owner) = match publication {
            ViewPublication::Created(undo) => (undo.source, undo.transaction),
            ViewPublication::Altered(undo) => (undo.target.source, undo.target.transaction),
        };
        let slot = source.view_slot().ok_or_else(|| {
            sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "publication source is not a view"
            )
        })?;
        let catalog = self.view_rule_catalog();
        let view = catalog
            .views
            .get(slot)
            .filter(|view| source.matches(self, view.database, view.created_at, view.ddl_state))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "view publication source has been replaced"
                )
            })?;
        let valid = match publication {
            ViewPublication::Created(undo) => {
                undo.operation == CatalogDdlOperation::Create
                    && view.ddl_state.pending_txid() == Some(transaction)
                    && (matches!(view.ddl_state, CatalogDdlState::PendingCreateDrop { .. })
                        || matches!(view.ddl_state, CatalogDdlState::PendingCreate { .. })
                            && view.ddl_version == undo.version)
            }
            ViewPublication::Altered(undo) => {
                view.pending_definition.as_ref().is_some_and(|pending| {
                    pending.txid == transaction && pending.version >= undo.target.version
                })
            }
        };
        if owner != transaction || !valid {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "view publication changed before commit"
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_view_drop(
        &self,
        undo: CatalogDdlUndo,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let catalog = self.view_rule_catalog();
        if undo.transaction != transaction || self.matches_view_drop(&catalog, undo).is_none() {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "view drop source changed before commit"
            ));
        }
        Ok(())
    }

    pub(crate) fn validate_matview_population(
        &self,
        undo: MatviewPopulationUndo,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let target = undo.target;
        let CatalogObjectSlot::MaterializedView(slot) = target.source.slot() else {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "population source is not a materialized view"
            ));
        };
        let catalog = self.matview_catalog();
        let view = catalog
            .get(slot)
            .filter(|view| {
                target
                    .source
                    .matches(self, view.database, view.created_at, view.ddl_state)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "materialized view publication source has been replaced"
                )
            })?;
        if target.transaction != transaction
            || !view.pending_population.is_some_and(|pending| {
                pending.transaction == transaction && pending.version >= target.version
            })
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "materialized view population changed before commit"
            ));
        }
        Ok(())
    }

    pub(crate) fn drop_view(
        &self,
        schema: &str,
        name: &str,
        transaction: u32,
    ) -> Result<Option<CatalogDdlUndo>, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let catalog = &mut owners.catalog;
        let mut selected = None;
        let mut blocked = None;
        for (slot, view) in catalog.views.iter().enumerate() {
            if view.database != current_database() || view.ddl_state == CatalogDdlState::Absent {
                continue;
            }
            let matches = |owner| {
                view.schema_for(owner).as_str() == schema && view.name_for(owner).as_str() == name
            };
            if matches(0)
                || view
                    .pending_definition
                    .is_some_and(|pending| matches(pending.txid))
            {
                blocked = view
                    .pending_definition
                    .map(|pending| pending.txid)
                    .or_else(|| view.ddl_state.pending_txid())
                    .filter(|owner| *owner != transaction);
                if blocked.is_some() {
                    break;
                }
            }
            if view.visible_to(transaction) && matches(transaction) {
                selected = Some(slot);
            }
        }
        if let Some(owner) = blocked {
            drop(owners);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name));
        }
        let Some(slot) = selected else {
            return Ok(None);
        };
        let target = super::RuleTarget::View(slot as u16);
        let blocked = catalog
            .rules
            .iter()
            .filter(|rule| {
                rule.database == current_database()
                    && rule.ddl_state != CatalogDdlState::Absent
                    && rule.definition_for(transaction).target == target
            })
            .find_map(|rule| {
                rule.pending
                    .map(|pending| pending.txid)
                    .or_else(|| rule.ddl_state.pending_txid())
                    .filter(|owner| *owner != transaction)
            });
        if let Some(owner) = blocked {
            drop(owners);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name));
        }
        let view = catalog.views[slot];
        let version = self.catalog_sequence.next()?;
        let source = self.catalog_object_identity(
            CatalogObjectSlot::View(slot),
            view.database,
            view.created_at,
        );
        self.stage_query_comment_drop_in(&mut owners, source, transaction)?;
        let catalog = &mut owners.catalog;
        catalog.views[slot].ddl_state = view.ddl_state.drop_by(transaction);
        catalog.views[slot].ddl_version = version;
        for rule in catalog.rules.iter_mut().filter(|rule| {
            rule.database == current_database()
                && rule.visible_to(transaction)
                && rule.definition_for(transaction).target == target
        }) {
            rule.ddl_state = rule.ddl_state.drop_by(transaction);
            rule.pending_parent_drop_txid = Some(transaction);
        }
        Ok(Some(CatalogDdlUndo {
            source: self.catalog_object_identity(
                CatalogObjectSlot::View(slot),
                view.database,
                view.created_at,
            ),
            transaction,
            operation: CatalogDdlOperation::Drop,
            version,
            prior_version: view.ddl_version,
        }))
    }

    fn matches_view_drop(
        &self,
        catalog: &super::ViewRuleCatalog,
        undo: CatalogDdlUndo,
    ) -> Option<usize> {
        let slot = undo.source.view_slot()?;
        let view = catalog.views.get(slot)?;
        (undo.operation == CatalogDdlOperation::Drop
            && undo
                .source
                .matches(self, view.database, view.created_at, view.ddl_state)
            && view.ddl_version == undo.version
            && view.ddl_state.pending_txid() == Some(undo.transaction)
            && matches!(
                view.ddl_state,
                CatalogDdlState::PendingDrop { .. } | CatalogDdlState::PendingCreateDrop { .. }
            ))
        .then_some(slot)
    }

    pub(crate) fn commit_view_drop(&mut self, undo: CatalogDdlUndo) {
        let selected = self.matches_view_drop(&self.view_rule_catalog(), undo);
        if selected.is_some() {
            self.retire_view_source(undo.source);
        }
    }

    pub(crate) fn rollback_view_drop(&self, undo: CatalogDdlUndo) {
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let catalog = &mut owners.catalog;
        let Some(slot) = self.matches_view_drop(catalog, undo) else {
            return;
        };
        catalog.rollback_target_rules(super::RuleTarget::View(slot as u16), undo.transaction);
        catalog.views[slot].ddl_state = catalog.views[slot]
            .ddl_state
            .rollback_drop(undo.transaction);
        catalog.views[slot].ddl_version = undo.prior_version;
        self.rollback_query_comment_drop_in(
            owners
                .comments
                .as_mut()
                .expect("view rollback owns comments"),
            undo.source,
            undo.transaction,
        );
    }

    pub(crate) fn require_populated_materialized_view(
        &self,
        table: usize,
        transaction: u32,
    ) -> Result<(), SqlError> {
        if self.matviews_visible_to(transaction).any(|(_, view)| {
            usize::from(view.backing_table) == table && !view.populated_for(transaction)
        }) {
            return Err(sql_err!(
                sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "materialized view \"{}\" has not been populated",
                self.table_def(table, transaction).name.as_str()
            ));
        }
        Ok(())
    }

    pub(crate) fn prepare_matview_population(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<CatalogEditTarget, SqlError> {
        let catalog = self.matview_catalog();
        let view = catalog
            .get(slot)
            .filter(|view| view.database == current_database() && view.visible_to(transaction))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "materialized view no longer exists"
                )
            })?;
        if let Some(pending) = view
            .pending_population
            .filter(|pending| pending.transaction != transaction)
        {
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(
                transaction,
                pending.transaction,
                "materialized view population",
            ));
        }
        Ok(CatalogEditTarget {
            source: self.catalog_object_identity(
                CatalogObjectSlot::MaterializedView(slot),
                view.database,
                view.created_at,
            ),
            transaction,
            version: view
                .pending_population
                .filter(|pending| pending.transaction == transaction)
                .map_or(view.population_version, |pending| pending.version),
        })
    }

    pub(crate) fn stage_matview_population(
        &self,
        target: CatalogEditTarget,
        populated: bool,
        transaction: u32,
    ) -> Result<MatviewPopulationUndo, SqlError> {
        let CatalogObjectSlot::MaterializedView(slot) = target.source.slot() else {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "population edit source is not a materialized view"
            ));
        };
        let mut catalog = self.matview_catalog();
        let view = catalog
            .get_mut(slot)
            .filter(|view| {
                target
                    .source
                    .matches(self, view.database, view.created_at, view.ddl_state)
                    && view.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "materialized view edit source has been replaced"
                )
            })?;
        if let Some(pending) = view
            .pending_population
            .filter(|pending| pending.transaction != transaction)
        {
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(
                transaction,
                pending.transaction,
                "materialized view population",
            ));
        }
        let current_version = view
            .pending_population
            .map_or(view.population_version, |pending| pending.version);
        if target.transaction != transaction || current_version != target.version {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "materialized view changed after refresh preparation"
            ));
        }
        let version = self.catalog_sequence.next()?;
        let prior = view.pending_population.replace(PendingMatviewPopulation {
            transaction,
            version,
            populated,
        });
        Ok(MatviewPopulationUndo {
            target: CatalogEditTarget { version, ..target },
            prior,
        })
    }

    pub(crate) fn finish_matview_population(
        &self,
        undo: MatviewPopulationUndo,
        decision: CatalogDdlDecision,
    ) {
        let target = undo.target;
        let CatalogObjectSlot::MaterializedView(slot) = target.source.slot() else {
            return;
        };
        let mut catalog = self.matview_catalog();
        let Some(view) = catalog.get_mut(slot).filter(|view| {
            target
                .source
                .matches(self, view.database, view.created_at, view.ddl_state)
        }) else {
            return;
        };
        let Some(pending) = view.pending_population.filter(|pending| {
            pending.transaction == target.transaction && pending.version == target.version
        }) else {
            return;
        };
        match decision {
            CatalogDdlDecision::Commit => {
                view.populated = pending.populated;
                view.population_version = pending.version;
                view.pending_population = None;
            }
            CatalogDdlDecision::Rollback => view.pending_population = undo.prior,
        }
    }

    pub(crate) fn prepare_view_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<CatalogEditTarget, SqlError> {
        let catalog = self.view_rule_catalog();
        let view = catalog
            .views
            .get(slot)
            .filter(|view| view.database == current_database() && view.visible_to(transaction))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "view edit source no longer exists"
                )
            })?;
        let target = self.view_edit_target(slot, view, transaction);
        if let Some(owner) = view
            .pending_definition
            .as_ref()
            .map(|pending| pending.txid)
            .filter(|owner| *owner != transaction)
        {
            let name = view.name;
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name.as_str()));
        }
        Ok(target)
    }

    pub(crate) fn view_edit_target(
        &self,
        slot: usize,
        view: &super::ViewDef,
        transaction: u32,
    ) -> CatalogEditTarget {
        CatalogEditTarget {
            source: self.catalog_object_identity(
                CatalogObjectSlot::View(slot),
                view.database,
                view.created_at,
            ),
            transaction,
            version: view
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
                .map_or(view.version, |pending| pending.version),
        }
    }

    pub(super) fn validate_view_edit(
        &self,
        catalog: &super::ViewRuleCatalog,
        target: CatalogEditTarget,
        transaction: u32,
    ) -> Result<usize, SqlError> {
        let slot = target.view_slot()?;
        let view = catalog
            .views
            .get(slot)
            .filter(|view| {
                target
                    .source
                    .matches(self, view.database, view.created_at, view.ddl_state)
                    && view.visible_to(transaction)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "view edit source has been replaced"
                )
            })?;
        if target.transaction != transaction {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "view edit belongs to another transaction"
            ));
        }
        if let Some(pending) = view
            .pending_definition
            .as_ref()
            .filter(|pending| pending.txid != transaction)
        {
            return Err(self.catalog_ddl_wait_error(transaction, pending.txid, view.name.as_str()));
        }
        let current = self.view_edit_target(slot, view, transaction);
        if current.version != target.version {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "view definition changed after edit preparation"
            ));
        }
        Ok(slot)
    }

    pub(crate) fn replace_view_definition(
        &self,
        target: CatalogEditTarget,
        definition: super::ViewDefinition,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::ViewDefinition,
            None,
        );
        self.replace_view_in(&mut owners, target, definition, transaction)
    }

    pub(super) fn replace_view_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, super::ViewRuleCatalog>,
        target: CatalogEditTarget,
        definition: super::ViewDefinition,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        let slot = self.validate_view_edit(&owners.catalog, target, transaction)?;
        let current = owners.catalog.views[slot];
        let rule_slot = usize::from(current.return_rule);
        let rule = owners.catalog.rules[rule_slot];
        if let Some(pending) = rule.pending.filter(|pending| pending.txid != transaction) {
            return Err(self.catalog_ddl_wait_error(
                transaction,
                pending.txid,
                current.name.as_str(),
            ));
        }
        let version = self.catalog_sequence.next()?;
        let mut rule_definition = rule.definition_for(transaction);
        rule_definition.source = definition.query.sql;
        rule_definition.creation_path = definition.query.creation_path;
        rule_definition.condition = None;
        rule_definition.actions = [super::RuleTextSpan::EMPTY; super::MAX_RULE_ACTIONS];
        rule_definition.actions[0] = super::RuleTextSpan {
            start: 0,
            len: definition.query.sql.as_str().len() as u16,
        };
        rule_definition.action_count = 1;
        rule_definition.returning_action = None;
        let dependency_slot = owners.allocate_query_image(
            self,
            super::StoredQueryDependencyOwner::Rule(rule_slot as u16),
            rule.pending.map(|pending| pending.dependency_slot),
            definition.query.dependencies.view(),
            transaction,
        )?;
        owners.catalog.rules[rule_slot].pending = Some(PendingRuleDefinition {
            txid: transaction,
            version,
            definition: rule_definition,
            dependency_slot,
        });
        owners.catalog.views[slot].pending_definition = Some(PendingViewDefinition {
            txid: transaction,
            version,
            schema: current.schema_for(transaction),
            name: current.name_for(transaction),
            options: definition.options,
            columns: definition.columns,
        });
        Ok(ViewAlterUndo {
            target: CatalogEditTarget { version, ..target },
            prior: current.pending_definition,
            return_rule: Some(RuleAlterUndo {
                target: CatalogEditTarget {
                    source: self.catalog_object_identity(
                        CatalogObjectSlot::Rule(rule_slot),
                        rule.database,
                        rule.created_at,
                    ),
                    transaction,
                    version,
                },
                prior: rule.pending,
            }),
        })
    }

    fn stage_view_metadata(
        &self,
        target: CatalogEditTarget,
        transaction: u32,
        edit: impl FnOnce(&mut PendingViewDefinition),
    ) -> Result<ViewAlterUndo, SqlError> {
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::ViewMetadataEdit,
            None,
        );
        let slot = self.validate_view_edit(&owners.catalog, target, transaction)?;
        let current = owners.catalog.views[slot];
        let prior = current.pending_definition;
        let mut pending = prior.unwrap_or(PendingViewDefinition {
            txid: transaction,
            version: current.version,
            schema: current.schema,
            name: current.name,
            options: current.options,
            columns: current.columns,
        });
        let before = (pending.schema, pending.name);
        edit(&mut pending);
        self.require_row_type_name(
            owners.types.as_deref().expect("view edits own type names"),
            pending.schema,
            pending.name,
            transaction,
            Some(super::RuleTarget::View(slot as u16)),
            Some(&owners.catalog),
        )?;
        let comments = owners.comments.as_mut().expect("view edits own comments");
        self.validate_view_comment_move(comments, before, transaction)?;
        pending.version = self.catalog_sequence.next()?;
        self.stage_view_comments(
            comments,
            before,
            (pending.schema, pending.name),
            transaction,
        );
        owners.catalog.views[slot].pending_definition = Some(pending);
        Ok(ViewAlterUndo {
            target: CatalogEditTarget {
                version: pending.version,
                ..target
            },
            prior,
            return_rule: None,
        })
    }

    pub(crate) fn stage_view_schema(
        &self,
        target: CatalogEditTarget,
        schema: super::SqlName,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| definition.schema = schema)
    }

    pub(crate) fn stage_view_rename(
        &self,
        target: CatalogEditTarget,
        name: super::SqlName,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| definition.name = name)
    }

    pub(crate) fn stage_view_options(
        &self,
        target: CatalogEditTarget,
        options: super::ViewOptions,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| {
            definition.options = options
        })
    }

    pub(crate) fn stage_view_columns(
        &self,
        target: CatalogEditTarget,
        columns: super::ViewColumns,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| {
            definition.columns = columns
        })
    }

    fn validate_view_comment_move(
        &self,
        comments: &super::CommentCatalog,
        before: (super::SqlName, super::SqlName),
        transaction: u32,
    ) -> Result<(), SqlError> {
        if comments.entries.iter().any(|comment| {
            comment.used
                && comment.database == Some(current_database())
                && matches!(
                    comment.class,
                    super::CommentClass::Relation | super::CommentClass::Type
                )
                && comment.schema == before.0
                && comment.name == before.1
                && comment
                    .pending_identity
                    .is_some_and(|pending| pending.txid != transaction)
        }) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "view comment identity is owned by another transaction"
            ));
        }
        Ok(())
    }

    fn stage_view_comments(
        &self,
        comments: &mut super::CommentCatalog,
        before: (super::SqlName, super::SqlName),
        after: (super::SqlName, super::SqlName),
        transaction: u32,
    ) {
        if before == after {
            return;
        }
        for comment in comments.entries.iter_mut().filter(|comment| {
            comment.used
                && comment.database == Some(current_database())
                && matches!(
                    comment.class,
                    super::CommentClass::Relation | super::CommentClass::Type
                )
                && ((comment.schema == before.0 && comment.name == before.1)
                    || comment.pending_identity.is_some_and(|identity| {
                        identity.txid == transaction && (identity.schema, identity.name) == before
                    }))
        }) {
            comment.pending_identity = ((comment.schema, comment.name) != after).then_some(
                super::PendingCommentIdentity {
                    txid: transaction,
                    schema: after.0,
                    name: after.1,
                },
            );
        }
    }

    pub(crate) fn commit_view_alter(&self, undo: ViewAlterUndo) {
        let target = undo.target;
        let Ok(slot) = target.view_slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(view) = owners.catalog.views.get(slot).copied() else {
            return;
        };
        if !target
            .source
            .matches(self, view.database, view.created_at, view.ddl_state)
        {
            return;
        }
        let Some(pending) = view.pending_definition.filter(|pending| {
            pending.txid == target.transaction && pending.version == target.version
        }) else {
            return;
        };
        self.publish_pending_view_in(&mut owners, slot, pending);
    }

    fn publish_pending_view_in(
        &self,
        owners: &mut super::type_contracts::CatalogDependencyOwners<'_, super::ViewRuleCatalog>,
        slot: usize,
        pending: PendingViewDefinition,
    ) {
        let view = owners.catalog.views[slot];
        let rule_slot = usize::from(view.return_rule);
        self.publish_pending_rule_in(owners, rule_slot, pending.txid);
        let current = &mut owners.catalog.views[slot];
        current.schema = pending.schema;
        current.name = pending.name;
        current.options = pending.options;
        current.columns = pending.columns;
        current.version = pending.version;
        current.pending_definition = None;
        for comment in owners
            .comments
            .as_mut()
            .expect("view edits own comments")
            .entries
            .iter_mut()
            .filter(|comment| {
                comment.used
                    && comment.database == Some(current_database())
                    && matches!(
                        comment.class,
                        super::CommentClass::Relation | super::CommentClass::Type
                    )
                    && comment.schema == view.schema
                    && comment.name == view.name
                    && comment
                        .pending_identity
                        .is_some_and(|identity| identity.txid == pending.txid)
            })
        {
            let identity = comment
                .pending_identity
                .take()
                .expect("selected comment identity");
            comment.schema = identity.schema;
            comment.name = identity.name;
        }
    }

    pub(crate) fn finish_view_publication(
        &self,
        publication: ViewPublication,
        decision: CatalogDdlDecision,
    ) {
        let undo = match publication {
            ViewPublication::Created(undo) => undo,
            ViewPublication::Altered(undo) => {
                match decision {
                    CatalogDdlDecision::Commit => self.commit_view_alter(undo),
                    CatalogDdlDecision::Rollback => self.rollback_view_alter(undo),
                }
                return;
            }
        };
        let CatalogObjectSlot::View(slot) = undo.source.slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::CatalogLifecycle,
            None,
        );
        let Some(view) = owners.catalog.views.get(slot).copied() else {
            return;
        };
        let committing_create_drop = decision == CatalogDdlDecision::Commit
            && view.ddl_state
                == (CatalogDdlState::PendingCreateDrop {
                    txid: undo.transaction,
                });
        if undo.operation != CatalogDdlOperation::Create
            || !undo
                .source
                .matches(self, view.database, view.created_at, view.ddl_state)
            || !committing_create_drop
                && (view.ddl_state
                    != (CatalogDdlState::PendingCreate {
                        txid: undo.transaction,
                    })
                    || view.ddl_version != undo.version)
        {
            return;
        }
        let rule_slot = usize::from(view.return_rule);
        match decision {
            CatalogDdlDecision::Commit => {
                if let Some(pending) = view
                    .pending_definition
                    .filter(|pending| pending.txid == undo.transaction)
                {
                    self.publish_pending_view_in(&mut owners, slot, pending);
                }
                self.publish_pending_rule_in(&mut owners, rule_slot, undo.transaction);
                owners.catalog.views[slot].ddl_state = view.ddl_state.commit_create();
                let ownership = &mut owners.catalog.views[slot].ownership;
                if let Some(pending) = ownership
                    .pending
                    .filter(|pending| pending.txid == undo.transaction)
                {
                    ownership.owner = pending.owner;
                    ownership.pending = None;
                }
                let state = owners.catalog.rules[rule_slot].ddl_state;
                owners.catalog.rules[rule_slot].ddl_state = state.commit_create();
            }
            CatalogDdlDecision::Rollback => {
                Self::clear_object_acl_entries_locked(
                    owners
                        .permissions
                        .as_mut()
                        .expect("view retirement owns permissions"),
                    super::AccessObject {
                        class: super::AccessClass::View,
                        slot: slot as u16,
                    },
                );
                let rule = owners.catalog.rules[rule_slot];
                let mut next = rule.pending.map(|pending| pending.dependency_slot);
                while let Some(slot) = next {
                    let pending = &mut owners.dependencies.pending[slot as usize];
                    next = pending.previous;
                    *pending = super::PendingStoredQueryDependencies::EMPTY;
                }
                let image = self.committed_dependency_image(
                    super::StoredQueryDependencyOwner::Rule(rule_slot as u16),
                );
                owners.dependencies.counts[image] = 0;
                owners.catalog.rules[rule_slot] = super::RuleDef::EMPTY;
                owners.catalog.views[slot] = super::ViewDef::EMPTY;
            }
        }
    }

    pub(crate) fn rollback_view_alter(&self, undo: ViewAlterUndo) {
        let target = undo.target;
        let Ok(slot) = target.view_slot() else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::MetadataCommit,
            None,
        );
        let Some(view) = owners.catalog.views.get(slot).copied() else {
            return;
        };
        if !target
            .source
            .matches(self, view.database, view.created_at, view.ddl_state)
        {
            return;
        }
        let Some(pending) = view.pending_definition.filter(|pending| {
            pending.txid == target.transaction && pending.version == target.version
        }) else {
            return;
        };
        if let Some(rule_undo) = undo.return_rule {
            let rule_slot = rule_undo.slot();
            let rule = owners.catalog.rules[rule_slot];
            if rule_undo
                .target
                .source
                .matches(self, rule.database, rule.created_at, rule.ddl_state)
                && let Some(current) = rule.pending.filter(|pending| {
                    pending.txid == target.transaction
                        && pending.version == rule_undo.target.version
                })
            {
                owners.dependencies.pending[current.dependency_slot as usize] =
                    super::PendingStoredQueryDependencies::EMPTY;
                owners.catalog.rules[rule_slot].pending = rule_undo.prior;
            }
        }
        owners.catalog.views[slot].pending_definition = undo.prior;
        let restored = owners.catalog.views[slot];
        self.stage_view_comments(
            owners.comments.as_mut().expect("view edits own comments"),
            (pending.schema, pending.name),
            restored.names_for(target.transaction),
            target.transaction,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{test_budget, test_config, test_view_definition};
    use crate::storage::{SqlName, StoredQueryDependencies};

    fn fixture(views: usize) -> Storage {
        let mut config = test_config();
        config.max_views = views;
        config.max_rules = views;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
    }

    fn name(value: &str) -> SqlName {
        SqlName::parse(value).unwrap()
    }

    fn create(storage: &Storage, value: &str, transaction: u32) -> ViewPublication {
        storage
            .create_view(
                name("public"),
                name(value),
                test_view_definition(StoredQueryDependencies::EMPTY),
                false,
                transaction,
            )
            .unwrap()
    }

    #[test]
    fn query_lifecycle_replay_preserves_positions_and_rejects_reused_identities() {
        let mut storage = fixture(3);
        let identity = ViewCatalogIdentity::new(2, 7, 2, 8).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .replay_view(
                        identity,
                        name("missing_schema"),
                        name("restored_view"),
                        test_view_definition(StoredQueryDependencies::EMPTY)
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.view(2).ddl_state, CatalogDdlState::Absent);
            storage
                .replay_view(
                    identity,
                    name("public"),
                    name("restored_view"),
                    test_view_definition(StoredQueryDependencies::EMPTY),
                )
                .unwrap();
            for invalid in [
                ViewCatalogIdentity::new(2, 11, 2, 12).unwrap(),
                ViewCatalogIdentity::new(2, 7, 1, 8).unwrap(),
                ViewCatalogIdentity::new(3, 7, 2, 8).unwrap(),
                ViewCatalogIdentity::new(2, 7, 3, 8).unwrap(),
            ] {
                assert_eq!(
                    storage
                        .replay_view(
                            invalid,
                            name("public"),
                            name("restored_view"),
                            test_view_definition(StoredQueryDependencies::EMPTY)
                        )
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.view_catalog_identity(2), identity);
                assert_eq!(storage.view_sql(2).as_str(), "SELECT 1");
            }
            assert_eq!(
                storage
                    .replay_view(
                        ViewCatalogIdentity::new(1, 9, 1, 8).unwrap(),
                        name("public"),
                        name("duplicate_rule_identity"),
                        test_view_definition(StoredQueryDependencies::EMPTY)
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(storage.view(1).ddl_state, CatalogDdlState::Absent);
            let mut replacement = test_view_definition(StoredQueryDependencies::EMPTY);
            replacement.query.sql = crate::util::StackStr::from_str("SELECT 2");
            storage
                .replay_view(identity, name("public"), name("restored_view"), replacement)
                .unwrap();
            assert_eq!(storage.view_catalog_identity(2), identity);
            assert_eq!(storage.view_sql(2).as_str(), "SELECT 2");
            assert_eq!(storage.view(0).ddl_state, CatalogDdlState::Absent);
        });
    }

    #[test]
    fn query_lifecycle_view_creation_and_edit_do_not_touch_reused_slots() {
        let storage = fixture(1);
        let original = create(&storage, "original_view", 7);
        let selected = storage.prepare_view_edit(original.slot(), 7).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.finish_view_publication(original, CatalogDdlDecision::Rollback);
            let replacement = create(&storage, "replacement_view", 9);
            assert_eq!(replacement.slot(), original.slot());
            storage.finish_view_publication(original, CatalogDdlDecision::Commit);
            storage.finish_view_publication(original, CatalogDdlDecision::Rollback);
            assert!(storage.find_view("public", "replacement_view", 9).is_some());
            assert!(storage.find_view("public", "replacement_view", 0).is_none());
            assert_eq!(
                storage
                    .stage_view_rename(selected, name("stolen_name"), 7)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::UNDEFINED_TABLE
            );
            assert!(storage.find_view("public", "replacement_view", 9).is_some());
            storage.finish_view_publication(replacement, CatalogDdlDecision::Commit);
            assert!(storage.find_view("public", "replacement_view", 0).is_some());
        });
    }

    #[test]
    fn query_lifecycle_view_edits_and_undo_follow_one_revision() {
        let storage = fixture(1);
        let created = create(&storage, "original_view", 7);
        storage.finish_view_publication(created, CatalogDdlDecision::Commit);
        crate::mem::guard::forbid_alloc(|| {
            let selected = storage.prepare_view_edit(created.slot(), 9).unwrap();
            let first = storage
                .stage_view_rename(selected, name("first_view"), 9)
                .unwrap();
            assert_eq!(
                storage
                    .stage_view_rename(selected, name("stale_view"), 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let current = storage.prepare_view_edit(created.slot(), 9).unwrap();
            let second = storage
                .stage_view_rename(current, name("second_view"), 9)
                .unwrap();
            storage.rollback_view_alter(first);
            assert!(storage.find_view("public", "second_view", 9).is_some());
            storage.rollback_view_alter(second);
            assert!(storage.find_view("public", "first_view", 9).is_some());
            storage.rollback_view_alter(first);
            assert!(storage.find_view("public", "original_view", 9).is_some());
            let current = storage.prepare_view_edit(created.slot(), 9).unwrap();
            let third = storage
                .stage_view_rename(current, name("third_view"), 9)
                .unwrap();
            storage.rollback_view_alter(second);
            storage.commit_view_alter(second);
            assert!(storage.find_view("public", "third_view", 9).is_some());
            storage.commit_view_alter(third);
            assert!(storage.find_view("public", "third_view", 0).is_some());
        });
    }

    #[test]
    fn query_lifecycle_view_drop_undo_cannot_finalize_a_later_drop() {
        let mut storage = fixture(1);
        let created = create(&storage, "original_view", 7);
        storage.finish_view_publication(created, CatalogDdlDecision::Commit);
        crate::mem::guard::forbid_alloc(|| {
            let first = storage
                .drop_view("public", "original_view", 9)
                .unwrap()
                .unwrap();
            storage.rollback_view_drop(first);
            let second = storage
                .drop_view("public", "original_view", 9)
                .unwrap()
                .unwrap();
            storage.commit_view_drop(first);
            storage.rollback_view_drop(first);
            assert!(storage.find_view("public", "original_view", 0).is_some());
            assert!(storage.find_view("public", "original_view", 9).is_none());
            storage.commit_view_drop(second);
            let replacement = create(&storage, "replacement_view", 11);
            assert_eq!(replacement.slot(), created.slot());
            storage.commit_view_drop(second);
            storage.rollback_view_drop(second);
            assert!(
                storage
                    .find_view("public", "replacement_view", 11)
                    .is_some()
            );
            storage.finish_view_publication(replacement, CatalogDdlDecision::Rollback);
        });
    }

    #[test]
    fn query_lifecycle_foreign_view_handles_reject_different_catalog_bounds() {
        let first = fixture(2);
        let _filler = create(&first, "filler_view", 7);
        let selected = create(&first, "selected_view", 9);
        let target = first.prepare_view_edit(selected.slot(), 9).unwrap();
        let mut second = fixture(1);
        let replacement = create(&second, "replacement_view", 11);
        crate::mem::guard::forbid_alloc(|| {
            second.finish_view_publication(selected, CatalogDdlDecision::Commit);
            second.finish_view_publication(selected, CatalogDdlDecision::Rollback);
            assert_eq!(
                second
                    .stage_view_rename(target, name("foreign_view"), 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::UNDEFINED_TABLE
            );
            let dropped = first
                .drop_view("public", "selected_view", 9)
                .unwrap()
                .unwrap();
            second.commit_view_drop(dropped);
            second.rollback_view_drop(dropped);
            assert!(second.find_view("public", "replacement_view", 11).is_some());
            second.finish_view_publication(replacement, CatalogDdlDecision::Rollback);
        });
    }

    #[test]
    fn query_lifecycle_rule_edits_and_finalizers_reject_replaced_sources() {
        let mut storage = fixture(2);
        let view = create(&storage, "rule_parent", 7);
        storage.finish_view_publication(view, CatalogDdlDecision::Commit);
        let definition = super::super::RuleDefinition {
            name: name("first_rule"),
            target: super::super::RuleTarget::View(view.slot() as u16),
            event: super::super::RewriteEvent::Insert,
            ..super::super::RuleDefinition::EMPTY
        };
        let dependencies = StoredQueryDependencies::EMPTY;
        crate::mem::guard::forbid_alloc(|| {
            let created = storage
                .create_rule(definition, dependencies.view(), false, 9)
                .unwrap();
            let stale = storage.prepare_rule_edit(created.slot(), 9).unwrap();
            storage.finish_rule_publication(created, None, CatalogDdlDecision::Rollback);
            let replacement = storage
                .create_rule(definition, dependencies.view(), false, 11)
                .unwrap();
            assert_eq!(replacement.slot(), created.slot());
            storage.finish_rule_publication(created, None, CatalogDdlDecision::Commit);
            storage.discard_rule_create(created);
            assert_eq!(
                storage
                    .alter_rule(
                        stale,
                        super::super::RuleMetadataChange::Rename(name("stolen_rule")),
                        9
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::UNDEFINED_OBJECT
            );
            storage.finish_rule_publication(replacement, None, CatalogDdlDecision::Commit);
            let selected = storage.prepare_rule_edit(replacement.slot(), 13).unwrap();
            let (first, _) = storage
                .alter_rule(
                    selected,
                    super::super::RuleMetadataChange::Rename(name("second_rule")),
                    13,
                )
                .unwrap();
            assert_eq!(
                storage
                    .alter_rule(
                        selected,
                        super::super::RuleMetadataChange::Rename(name("stale_rule")),
                        13
                    )
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let selected = storage.prepare_rule_edit(replacement.slot(), 13).unwrap();
            let (second, _) = storage
                .alter_rule(
                    selected,
                    super::super::RuleMetadataChange::Rename(name("third_rule")),
                    13,
                )
                .unwrap();
            storage.rollback_rule_alter(first);
            assert_eq!(
                storage.rule(replacement.slot()).definition_for(13).name,
                name("third_rule")
            );
            storage.rollback_rule_alter(second);
            storage.rollback_rule_alter(first);
            let dropped = storage
                .drop_rule(
                    storage.prepare_rule_edit(replacement.slot(), 13).unwrap(),
                    13,
                )
                .unwrap();
            storage.rollback_rule_drop(dropped);
            let current = storage
                .drop_rule(
                    storage.prepare_rule_edit(replacement.slot(), 13).unwrap(),
                    13,
                )
                .unwrap();
            storage.commit_rule_drop(dropped);
            storage.rollback_rule_drop(dropped);
            assert!(!storage.rule(replacement.slot()).visible_to(13));
            storage.commit_rule_drop(current);
            let final_rule = storage
                .create_rule(definition, dependencies.view(), false, 15)
                .unwrap();
            storage.commit_rule_drop(current);
            storage.rollback_rule_alter(second);
            assert!(storage.rule(final_rule.slot()).visible_to(15));
            storage.finish_rule_publication(final_rule, None, CatalogDdlDecision::Rollback);
        });
    }
}
