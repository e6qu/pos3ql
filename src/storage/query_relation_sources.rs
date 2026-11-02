//! Stored SQL retains relation lifetimes independently of their current names.

use super::type_references::CatalogOrigin;
use super::{
    CatalogDdlState, CatalogIdentity, DatabaseOid, DependencyClass, ResolvedRelation, SqlError,
    Storage, StoredDependencyIdentity, StoredQueryDependency, current_database,
};
use crate::sql::eval::sqlstate;
use crate::sql_err;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceIdentity {
    Captured {
        origin: CatalogOrigin,
        database: DatabaseOid,
        creation: CatalogIdentity,
    },
    Stored(CatalogIdentity),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelationReference(ReferenceIdentity);

impl RelationReference {
    pub(crate) fn from_storage(created_at: u64) -> Option<Self> {
        CatalogIdentity::parse(created_at).map(|creation| Self(ReferenceIdentity::Stored(creation)))
    }

    pub(crate) fn created_at(self) -> u64 {
        match self.0 {
            ReferenceIdentity::Captured { creation, .. } | ReferenceIdentity::Stored(creation) => {
                creation.created_at()
            }
        }
    }

    fn capture(
        storage: &Storage,
        database: DatabaseOid,
        created_at: u64,
    ) -> Result<Self, SqlError> {
        Ok(Self(ReferenceIdentity::Captured {
            origin: storage.catalog_sources.origin(),
            database,
            creation: CatalogIdentity::require(created_at)?,
        }))
    }

    fn matches(self, storage: &Storage, database: DatabaseOid, created_at: u64) -> bool {
        matches!(self.0, ReferenceIdentity::Captured { origin, database: expected, creation }
            if origin == storage.catalog_sources.origin() && database == expected && creation.created_at() == created_at)
    }

    fn permits_rebind(self, storage: &Storage, database: DatabaseOid) -> bool {
        match self.0 {
            ReferenceIdentity::Stored(_) => true,
            ReferenceIdentity::Captured {
                origin,
                database: expected,
                ..
            } => origin == storage.catalog_sources.origin() && expected == database,
        }
    }
}

fn invalid() -> SqlError {
    sql_err!(
        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "stored relation binding is unresolved, replaced or no longer visible"
    )
}

fn reference(dependency: &StoredQueryDependency) -> Result<RelationReference, SqlError> {
    match (dependency.class, dependency.identity) {
        (DependencyClass::Table, StoredDependencyIdentity::Table(reference))
        | (DependencyClass::View, StoredDependencyIdentity::View(reference)) => Ok(reference),
        _ => Err(invalid()),
    }
}

impl Storage {
    pub(crate) fn capture_stored_query_relation(
        &self,
        relation: ResolvedRelation,
        transaction: u32,
    ) -> Result<StoredDependencyIdentity, SqlError> {
        match relation {
            ResolvedRelation::Table(slot) => {
                let table = self.tables.get(slot).ok_or_else(invalid)?;
                let definition = super::TableDefinitionRead::visible(
                    &table.definition,
                    &self.pending_table_defs,
                    transaction,
                );
                let identity = definition.identity();
                if identity.database != current_database()
                    || !identity.existence.visible_to(transaction)
                {
                    return Err(invalid());
                }
                Ok(StoredDependencyIdentity::Table(RelationReference::capture(
                    self,
                    identity.database,
                    identity.created_at,
                )?))
            }
            ResolvedRelation::View(slot) => {
                let catalog = self.view_rule_catalog();
                let view = catalog.views.get(slot).ok_or_else(invalid)?;
                if view.database != current_database() || !view.visible_to(transaction) {
                    return Err(invalid());
                }
                Ok(StoredDependencyIdentity::View(RelationReference::capture(
                    self,
                    view.database,
                    view.created_at,
                )?))
            }
            ResolvedRelation::Catalog => Err(invalid()),
        }
    }

    pub(crate) fn resolve_stored_query_relation(
        &self,
        dependency: &StoredQueryDependency,
        transaction: u32,
    ) -> Result<ResolvedRelation, SqlError> {
        let source = reference(dependency)?;
        let slot = usize::from(dependency.slot);
        match dependency.class {
            DependencyClass::Table => {
                let table = self.tables.get(slot).ok_or_else(invalid)?;
                let definition = super::TableDefinitionRead::visible(
                    &table.definition,
                    &self.pending_table_defs,
                    transaction,
                );
                let identity = definition.identity();
                if identity.database != current_database()
                    || !identity.existence.visible_to(transaction)
                    || !source.matches(self, identity.database, identity.created_at)
                {
                    return Err(invalid());
                }
                Ok(ResolvedRelation::Table(slot))
            }
            DependencyClass::View => {
                let catalog = self.view_rule_catalog();
                let view = catalog.views.get(slot).ok_or_else(invalid)?;
                if view.database != current_database()
                    || !view.visible_to(transaction)
                    || !source.matches(self, view.database, view.created_at)
                {
                    return Err(invalid());
                }
                Ok(ResolvedRelation::View(slot))
            }
            _ => Err(invalid()),
        }
    }

    pub(super) fn rebind_stored_query_relation(
        &self,
        mut dependency: StoredQueryDependency,
        transaction: u32,
    ) -> Result<StoredQueryDependency, SqlError> {
        let source = reference(&dependency)?;
        if !source.permits_rebind(self, current_database()) {
            return Err(invalid());
        }
        let mut found = None;
        match dependency.class {
            DependencyClass::Table => {
                for (slot, table) in self.tables.iter().enumerate() {
                    let definition = super::TableDefinitionRead::visible(
                        &table.definition,
                        &self.pending_table_defs,
                        transaction,
                    );
                    let identity = definition.identity();
                    if identity.database != current_database()
                        || identity.created_at != source.created_at()
                        || !identity.existence.visible_to(transaction)
                    {
                        continue;
                    }
                    if found
                        .replace((slot, definition.schema, definition.name))
                        .is_some()
                    {
                        return Err(invalid());
                    }
                }
            }
            DependencyClass::View => {
                let catalog = self.view_rule_catalog();
                for (slot, view) in catalog.views.iter().enumerate() {
                    if view.database != current_database()
                        || view.created_at != source.created_at()
                        || !view.visible_to(transaction)
                    {
                        continue;
                    }
                    if found
                        .replace((
                            slot,
                            view.schema_for(transaction),
                            view.name_for(transaction),
                        ))
                        .is_some()
                    {
                        return Err(invalid());
                    }
                }
            }
            _ => return Err(invalid()),
        }
        let (slot, schema, name) = found.ok_or_else(invalid)?;
        let relation = if dependency.class == DependencyClass::Table {
            ResolvedRelation::Table(slot)
        } else {
            ResolvedRelation::View(slot)
        };
        dependency.slot = u16::try_from(slot).map_err(|_| invalid())?;
        dependency.identity = self.capture_stored_query_relation(relation, transaction)?;
        dependency.schema = schema;
        dependency.name = name;
        Ok(dependency)
    }

    pub(super) fn clone_stored_query_relation(
        &self,
        dependency: &mut StoredQueryDependency,
        source_database: DatabaseOid,
    ) -> Result<(), SqlError> {
        if !matches!(
            dependency.class,
            DependencyClass::Table | DependencyClass::View
        ) {
            return Ok(());
        }
        let source = reference(dependency)?;
        let slot = usize::from(dependency.slot);
        let (database, creation, state) = match dependency.class {
            DependencyClass::Table => {
                let identity = self.tables.get(slot).ok_or_else(invalid)?.identity();
                (identity.database, identity.created_at, identity.existence)
            }
            DependencyClass::View => {
                let catalog = self.view_rule_catalog();
                let view = catalog.views.get(slot).ok_or_else(invalid)?;
                (view.database, view.created_at, view.ddl_state)
            }
            _ => return Err(invalid()),
        };
        if database != source_database
            || state != CatalogDdlState::Present
            || !source.matches(self, database, creation)
        {
            return Err(invalid());
        }
        let stored = RelationReference::from_storage(creation).ok_or_else(invalid)?;
        dependency.identity = if dependency.class == DependencyClass::Table {
            StoredDependencyIdentity::Table(stored)
        } else {
            StoredDependencyIdentity::View(stored)
        };
        dependency.slot = u16::MAX;
        Ok(())
    }
}
