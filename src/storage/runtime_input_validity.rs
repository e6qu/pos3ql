//! Definition and namespace ownership of cached SQL string-body inputs.

use crate::mem::lease_pool::SharedSlice;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

use super::{
    DatabaseOid, PathEntry, RelationReference, Storage, StoredDependencyIdentity,
    StoredQueryDependencies, TableDefinitionRevision, TypeReference,
};

#[derive(Clone, Copy)]
struct Namespace {
    entry: PathEntry,
    created_at: u64,
}

#[derive(Clone, Copy)]
enum Dependency {
    Table {
        slot: u16,
        source: RelationReference,
        revision: TableDefinitionRevision,
        pending_revision: u64,
    },
    View {
        slot: u16,
        source: RelationReference,
        revision: u64,
    },
    Routine {
        oid: i32,
        revision: u64,
    },
    Type {
        source: TypeReference,
        revision: u64,
    },
}

#[derive(Clone)]
pub(crate) struct RuntimeInputValidity {
    global: super::plan_invalidations::PlanInvalidationStamp,
    local: crate::sql::plan_invalidations::LocalPlanStamp,
    database: DatabaseOid,
    namespaces: SharedSlice<Namespace>,
    explicit_catalog: bool,
    dependencies: SharedSlice<Dependency>,
    role: Option<(super::RoleOid, bool)>,
}

impl Storage {
    pub(crate) fn capture_runtime_input_validity(
        &self,
        dependencies: &StoredQueryDependencies,
        txn: &crate::sql::txn::TxnState,
    ) -> Result<RuntimeInputValidity, SqlError> {
        let transaction = txn.txid;
        let full = |error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error);
        let path = self.path();
        let mut namespaces = self
            .metadata_pool
            .lease::<Namespace>(path.entries().len())
            .map_err(full)?;
        for entry in path.entries() {
            let created_at = match entry {
                PathEntry::Schema(slot) => {
                    self.schema_def_for(usize::from(*slot), transaction)
                        .created_at
                }
                _ => 0,
            };
            namespaces
                .push_reserved(Namespace {
                    entry: *entry,
                    created_at,
                })
                .map_err(full)?;
        }
        let count = dependencies
            .entries()
            .iter()
            .filter(|entry| {
                matches!(
                    entry.identity,
                    StoredDependencyIdentity::Table(_)
                        | StoredDependencyIdentity::View(_)
                        | StoredDependencyIdentity::RoutineOid(_)
                        | StoredDependencyIdentity::Type(_)
                )
            })
            .count();
        let mut selected = self
            .metadata_pool
            .lease::<Dependency>(count)
            .map_err(full)?;
        let mut depends_on_roles = false;
        for entry in dependencies.entries() {
            let slot = usize::from(entry.slot);
            let selected_entry = match entry.identity {
                StoredDependencyIdentity::Table(source) => {
                    let definition = self.table_def(slot, transaction);
                    depends_on_roles |= definition.row_level_security.enabled;
                    let (revision, pending_revision) = definition.revisions();
                    Some(Dependency::Table {
                        slot: entry.slot,
                        source,
                        revision,
                        pending_revision,
                    })
                }
                StoredDependencyIdentity::View(source) => {
                    let view = self.view(slot);
                    let revision = view
                        .pending_definition
                        .as_ref()
                        .filter(|pending| pending.txid == transaction)
                        .map_or(view.version, |pending| pending.version);
                    Some(Dependency::View {
                        slot: entry.slot,
                        source,
                        revision,
                    })
                }
                StoredDependencyIdentity::RoutineOid(oid) => Some(Dependency::Routine {
                    oid,
                    revision: self.routine_for(slot, transaction).version,
                }),
                StoredDependencyIdentity::Type(source) => {
                    let catalog = self
                        .type_catalog
                        .lock()
                        .expect("type catalog lock poisoned");
                    Some(Dependency::Type {
                        source,
                        revision: catalog.type_definition_version(source.slot(), transaction),
                    })
                }
                _ => None,
            };
            if let Some(entry) = selected_entry {
                selected.push_reserved(entry).map_err(full)?;
            }
        }
        let role = if depends_on_roles {
            let role = self.current_role_slot(transaction).ok_or_else(|| {
                sql_err!(sqlstate::UNDEFINED_OBJECT, "SQL plan role no longer exists")
            })?;
            Some((
                self.role(role).oid(),
                crate::sql::guc::active_row_security(),
            ))
        } else {
            None
        };
        Ok(RuntimeInputValidity {
            global: self
                .plan_invalidations
                .stamp(super::current_database(), depends_on_roles)?,
            local: txn.plan_invalidation_stamp(depends_on_roles)?,
            database: super::current_database(),
            namespaces: namespaces.freeze(),
            explicit_catalog: path.explicit_catalog(),
            dependencies: selected.freeze(),
            role,
        })
    }
}

impl RuntimeInputValidity {
    pub(crate) fn matches(&self, storage: &Storage, transaction: u32) -> Result<bool, SqlError> {
        if !storage.plan_invalidations.matches(self.global)? || !self.local.matches()? {
            return Ok(false);
        }
        if self.database != super::current_database() {
            return Ok(false);
        }
        let path = storage.path();
        if path.explicit_catalog() != self.explicit_catalog
            || path.entries().len() != self.namespaces.len()
        {
            return Ok(false);
        }
        for (entry, selected) in path.entries().iter().zip(self.namespaces.iter()) {
            if *entry != selected.entry
                || matches!(entry, PathEntry::Schema(slot)
                if storage.schema_def_for(usize::from(*slot), transaction).created_at != selected.created_at)
            {
                return Ok(false);
            }
        }
        if let Some((role, row_security)) = self.role
            && (storage
                .current_role_slot(transaction)
                .map(|slot| storage.role(slot).oid())
                != Some(role)
                || row_security != crate::sql::guc::active_row_security())
        {
            return Ok(false);
        }
        for dependency in self.dependencies.iter() {
            let matches = match *dependency {
                Dependency::Table {
                    slot,
                    source,
                    revision,
                    pending_revision,
                } => {
                    let definition = storage.table_def(usize::from(slot), transaction);
                    definition.identity().existence.visible_to(transaction)
                        && definition.identity().created_at == source.created_at()
                        && definition.revisions() == (revision, pending_revision)
                }
                Dependency::View {
                    slot,
                    source,
                    revision,
                } => {
                    let view = storage.view(usize::from(slot));
                    view.visible_to(transaction)
                        && view.created_at == source.created_at()
                        && view
                            .pending_definition
                            .as_ref()
                            .filter(|pending| pending.txid == transaction)
                            .map_or(view.version, |pending| pending.version)
                            == revision
                }
                Dependency::Routine { oid, revision } => storage
                    .routine_slot_by_oid(oid, transaction)
                    .is_some_and(|slot| storage.routine_for(slot, transaction).version == revision),
                Dependency::Type { source, revision } => {
                    let catalog = storage
                        .type_catalog
                        .lock()
                        .expect("type catalog lock poisoned");
                    catalog
                        .resolve_source_reference(source, self.database, transaction)?
                        .is_some()
                        && catalog.type_definition_version(source.slot(), transaction) == revision
                }
            };
            if !matches {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
