//! Stored-query edits and undo retain their selected object and revision.

use super::type_references::CatalogOrigin;
use super::{
    CatalogDdlState, DatabaseOid, PendingRuleDefinition, PendingViewDefinition, SqlError, Storage,
    current_database, sqlstate,
};
use crate::sql_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueryCatalogSlot {
    View(usize),
    Rule(usize),
    MaterializedView(usize),
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
        Storage::new(&config, &mut budget).unwrap()
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
    fn query_lifecycle_view_creation_and_edit_do_not_touch_reused_slots() {
        let storage = fixture(1);
        let original = create(&storage, "original_view", 7);
        let selected = storage.prepare_view_edit(original.slot(), 7).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            storage.finish_view_publication(original, QueryDdlDecision::Rollback);
            let replacement = create(&storage, "replacement_view", 9);
            assert_eq!(replacement.slot(), original.slot());
            storage.finish_view_publication(original, QueryDdlDecision::Commit);
            storage.finish_view_publication(original, QueryDdlDecision::Rollback);
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
            storage.finish_view_publication(replacement, QueryDdlDecision::Commit);
            assert!(storage.find_view("public", "replacement_view", 0).is_some());
        });
    }

    #[test]
    fn query_lifecycle_view_edits_and_undo_follow_one_revision() {
        let storage = fixture(1);
        let created = create(&storage, "original_view", 7);
        storage.finish_view_publication(created, QueryDdlDecision::Commit);
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
        storage.finish_view_publication(created, QueryDdlDecision::Commit);
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
            storage.finish_view_publication(replacement, QueryDdlDecision::Rollback);
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
            second.finish_view_publication(selected, QueryDdlDecision::Commit);
            second.finish_view_publication(selected, QueryDdlDecision::Rollback);
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
            second.finish_view_publication(replacement, QueryDdlDecision::Rollback);
        });
    }
}

impl QueryCatalogSlot {
    fn index(self) -> usize {
        match self {
            Self::View(slot) | Self::Rule(slot) | Self::MaterializedView(slot) => slot,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueryIdentity {
    origin: CatalogOrigin,
    database: DatabaseOid,
    slot: QueryCatalogSlot,
    created_at: u64,
}

impl QueryIdentity {
    pub(super) fn view_slot(self) -> Option<usize> {
        match self.slot {
            QueryCatalogSlot::View(slot) => Some(slot),
            _ => None,
        }
    }

    pub(super) fn matches(
        self,
        storage: &Storage,
        database: DatabaseOid,
        created_at: u64,
        state: CatalogDdlState,
    ) -> bool {
        self.origin == storage.catalog_sources.origin()
            && self.database == current_database()
            && self.database == database
            && self.created_at == created_at
            && state != CatalogDdlState::Absent
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct QueryEditTarget {
    pub(super) source: QueryIdentity,
    pub(super) transaction: u32,
    pub(super) version: u64,
}

impl QueryEditTarget {
    pub(crate) fn slot(self) -> usize {
        self.source.slot.index()
    }

    pub(super) fn view_slot(self) -> Result<usize, SqlError> {
        match self.source.slot {
            QueryCatalogSlot::View(slot) => Ok(slot),
            _ => Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "edit source is not a view"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueryDdlOperation {
    Create,
    Drop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryDdlUndo {
    pub(super) source: QueryIdentity,
    pub(super) transaction: u32,
    pub(super) operation: QueryDdlOperation,
    pub(super) version: u64,
    pub(super) prior_version: u64,
}

impl QueryDdlUndo {
    pub(crate) fn slot(self) -> usize {
        self.source.slot.index()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuleAlterUndo {
    pub(super) target: QueryEditTarget,
    pub(super) prior: Option<PendingRuleDefinition>,
}

impl RuleAlterUndo {
    pub(crate) fn slot(self) -> usize {
        self.target.slot()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewAlterUndo {
    pub(super) target: QueryEditTarget,
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
    Created(QueryDdlUndo),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryDdlDecision {
    Commit,
    Rollback,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingMatviewPopulation {
    pub(super) transaction: u32,
    version: u64,
    pub(super) populated: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MatviewPopulationUndo {
    target: QueryEditTarget,
    prior: Option<PendingMatviewPopulation>,
}

impl MatviewPopulationUndo {
    pub(crate) fn slot(self) -> usize {
        self.target.slot()
    }
}

impl Storage {
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
                undo.operation == QueryDdlOperation::Create
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
        undo: QueryDdlUndo,
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
        let QueryCatalogSlot::MaterializedView(slot) = target.source.slot else {
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
    ) -> Result<Option<QueryDdlUndo>, SqlError> {
        let mut catalog = self.view_rule_catalog();
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
            drop(catalog);
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
            drop(catalog);
            return Err(self.catalog_ddl_wait_error(transaction, owner, name));
        }
        let view = catalog.views[slot];
        let version = self.catalog_sequence.next()?;
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
        Ok(Some(QueryDdlUndo {
            source: self.query_identity(
                QueryCatalogSlot::View(slot),
                view.database,
                view.created_at,
            ),
            transaction,
            operation: QueryDdlOperation::Drop,
            version,
            prior_version: view.ddl_version,
        }))
    }

    fn matches_view_drop(
        &self,
        catalog: &super::ViewRuleCatalog,
        undo: QueryDdlUndo,
    ) -> Option<usize> {
        let slot = undo.source.view_slot()?;
        let view = catalog.views.get(slot)?;
        (undo.operation == QueryDdlOperation::Drop
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

    pub(crate) fn commit_view_drop(&mut self, undo: QueryDdlUndo) {
        let selected = self.matches_view_drop(&self.view_rule_catalog(), undo);
        if selected.is_some() {
            self.retire_view_source(undo.source);
        }
    }

    pub(crate) fn rollback_view_drop(&self, undo: QueryDdlUndo) {
        let mut catalog = self.view_rule_catalog();
        let Some(slot) = self.matches_view_drop(&catalog, undo) else {
            return;
        };
        catalog.rollback_target_rules(super::RuleTarget::View(slot as u16), undo.transaction);
        catalog.views[slot].ddl_state = catalog.views[slot]
            .ddl_state
            .rollback_drop(undo.transaction);
        catalog.views[slot].ddl_version = undo.prior_version;
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
    ) -> Result<QueryEditTarget, SqlError> {
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
        Ok(QueryEditTarget {
            source: self.query_identity(
                QueryCatalogSlot::MaterializedView(slot),
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
        target: QueryEditTarget,
        populated: bool,
        transaction: u32,
    ) -> Result<MatviewPopulationUndo, SqlError> {
        let QueryCatalogSlot::MaterializedView(slot) = target.source.slot else {
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
            target: QueryEditTarget { version, ..target },
            prior,
        })
    }

    pub(crate) fn finish_matview_population(
        &self,
        undo: MatviewPopulationUndo,
        decision: QueryDdlDecision,
    ) {
        let target = undo.target;
        let QueryCatalogSlot::MaterializedView(slot) = target.source.slot else {
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
            QueryDdlDecision::Commit => {
                view.populated = pending.populated;
                view.population_version = pending.version;
                view.pending_population = None;
            }
            QueryDdlDecision::Rollback => view.pending_population = undo.prior,
        }
    }

    pub(super) fn query_identity(
        &self,
        slot: QueryCatalogSlot,
        database: DatabaseOid,
        created_at: u64,
    ) -> QueryIdentity {
        QueryIdentity {
            origin: self.catalog_sources.origin(),
            database,
            slot,
            created_at,
        }
    }

    pub(crate) fn prepare_view_edit(
        &self,
        slot: usize,
        transaction: u32,
    ) -> Result<QueryEditTarget, SqlError> {
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
    ) -> QueryEditTarget {
        QueryEditTarget {
            source: self.query_identity(
                QueryCatalogSlot::View(slot),
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
        target: QueryEditTarget,
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
        target: QueryEditTarget,
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
        target: QueryEditTarget,
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
            target: QueryEditTarget { version, ..target },
            prior: current.pending_definition,
            return_rule: Some(RuleAlterUndo {
                target: QueryEditTarget {
                    source: self.query_identity(
                        QueryCatalogSlot::Rule(rule_slot),
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
        target: QueryEditTarget,
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
            target: QueryEditTarget {
                version: pending.version,
                ..target
            },
            prior,
            return_rule: None,
        })
    }

    pub(crate) fn stage_view_schema(
        &self,
        target: QueryEditTarget,
        schema: super::SqlName,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| definition.schema = schema)
    }

    pub(crate) fn stage_view_rename(
        &self,
        target: QueryEditTarget,
        name: super::SqlName,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| definition.name = name)
    }

    pub(crate) fn stage_view_options(
        &self,
        target: QueryEditTarget,
        options: super::ViewOptions,
        transaction: u32,
    ) -> Result<ViewAlterUndo, SqlError> {
        self.stage_view_metadata(target, transaction, |definition| {
            definition.options = options
        })
    }

    pub(crate) fn stage_view_columns(
        &self,
        target: QueryEditTarget,
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
            super::type_contracts::CatalogPublicationInput::ViewMetadataCommit,
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
        if let Some(rule) = owners.catalog.rules[rule_slot]
            .pending
            .filter(|rule| rule.txid == pending.txid)
        {
            self.commit_pending_dependencies_in(
                &mut owners.dependencies,
                super::StoredQueryDependencyOwner::Rule(rule_slot as u16),
                rule.dependency_slot,
            );
            owners.catalog.rules[rule_slot].definition = rule.definition;
            owners.catalog.rules[rule_slot].version = rule.version;
            owners.catalog.rules[rule_slot].pending = None;
        }
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
        decision: QueryDdlDecision,
    ) {
        let undo = match publication {
            ViewPublication::Created(undo) => undo,
            ViewPublication::Altered(undo) => {
                match decision {
                    QueryDdlDecision::Commit => self.commit_view_alter(undo),
                    QueryDdlDecision::Rollback => self.rollback_view_alter(undo),
                }
                return;
            }
        };
        let QueryCatalogSlot::View(slot) = undo.source.slot else {
            return;
        };
        let mut owners = self.catalog_dependency_owners(
            &self.view_rule_catalog,
            super::type_contracts::CatalogPublicationInput::ViewLifecycle,
            None,
        );
        let Some(view) = owners.catalog.views.get(slot).copied() else {
            return;
        };
        let committing_create_drop = decision == QueryDdlDecision::Commit
            && view.ddl_state
                == (CatalogDdlState::PendingCreateDrop {
                    txid: undo.transaction,
                });
        if undo.operation != QueryDdlOperation::Create
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
            QueryDdlDecision::Commit => {
                if let Some(pending) = view
                    .pending_definition
                    .filter(|pending| pending.txid == undo.transaction)
                {
                    self.publish_pending_view_in(&mut owners, slot, pending);
                }
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
            QueryDdlDecision::Rollback => {
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
            super::type_contracts::CatalogPublicationInput::ViewMetadataCommit,
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
