//! Bounded reverse type dependencies used by ALTER DOMAIN validation.

use super::type_names::NamedTypeSlot;
use super::type_references::{CatalogOrigin, TypeIdentity};
use super::{ColType, SqlName, Storage, TableDef, TypeCatalog, TypeReference, UserTypeName};
use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Usage {
    Unrelated,
    Value,
    Container,
}

#[derive(Clone, Copy)]
struct Source {
    identity: TypeIdentity,
    version: u64,
    base: ColType,
    usage: Usage,
}

#[derive(Clone, Copy)]
struct Edge {
    dependent: usize,
    container: bool,
    next: Option<usize>,
}

pub(crate) struct DomainValidationDependencies<'arena> {
    origin: CatalogOrigin,
    database: super::DatabaseOid,
    name: SqlName,
    domains: usize,
    sources: &'arena [Option<Source>],
}

fn full() -> SqlError {
    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "domain dependency graph exceeds the statement arena")
}

fn index(slot: NamedTypeSlot, domains: usize) -> Option<usize> {
    match slot {
        NamedTypeSlot::Domain(slot) => Some(slot),
        NamedTypeSlot::Composite(slot) => domains.checked_add(slot),
        NamedTypeSlot::Enum(_) => None,
    }
}

struct Builder<'catalog, 'scratch> {
    catalog: &'catalog TypeCatalog,
    transaction: u32,
    domains: usize,
    sources: &'scratch [Option<Source>],
    heads: &'scratch mut [Option<usize>],
    edges: &'scratch mut [Option<Edge>],
    used: usize,
}

impl Builder<'_, '_> {
    fn link(
        &mut self,
        parent: UserTypeName,
        dependent: usize,
        declared: ColType,
        composite_field: bool,
    ) -> Result<(), SqlError> {
        let source = self.catalog.resolve_source_reference(
            TypeReference::from_bound_name(parent)?,
            super::current_database(),
            self.transaction,
        )?.ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "domain dependency source no longer exists"))?;
        let Some(parent) = index(source.identity.slot(), self.domains) else { return Ok(()) };
        let selected = self.sources.get(parent).and_then(Option::as_ref).ok_or_else(|| {
            sql_err!(sqlstate::UNDEFINED_OBJECT, "domain dependency source is not visible")
        })?;
        if selected.identity != source.identity {
            return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "domain dependency source has been replaced"));
        }
        let edge = self.edges.get_mut(self.used).ok_or_else(full)?;
        *edge = Some(Edge {
            dependent,
            container: composite_field || declared != selected.base,
            next: self.heads[parent],
        });
        self.heads[parent] = Some(self.used);
        self.used += 1;
        Ok(())
    }
}

impl Storage {
    pub(crate) fn domain_validation_dependencies<'arena>(
        &self,
        target: usize,
        transaction: u32,
        arena: &'arena Arena,
    ) -> Result<DomainValidationDependencies<'arena>, SqlError> {
        let catalog = self.type_catalog.lock().expect("type catalog lock poisoned");
        let domains = catalog.domains.len();
        let count = domains.checked_add(catalog.composites.len()).ok_or_else(full)?;
        let sources = arena.alloc_slice_with(count, |_| None).map_err(|_| full())?;
        let heads = arena.alloc_slice_with(count, |_| None).map_err(|_| full())?;
        let mut edges_needed = 0usize;
        for (position, selected) in sources.iter_mut().enumerate() {
            let slot = if position < domains {
                NamedTypeSlot::Domain(position)
            } else {
                NamedTypeSlot::Composite(position - domains)
            };
            let Some(source) = catalog.type_snapshot(slot, transaction)?
                .filter(|source| source.identity.database() == super::current_database())
            else { continue };
            *selected = Some(Source {
                identity: source.identity,
                version: catalog.type_definition_version(slot, transaction),
                base: source.base,
                usage: Usage::Unrelated,
            });
            let edges = match slot {
                NamedTypeSlot::Domain(slot) => {
                    let definition = catalog.domains[slot].definition_for(transaction);
                    usize::from(definition.base_domain.is_some())
                        + usize::from(definition.base_user_type.is_some())
                }
                NamedTypeSlot::Composite(slot) => catalog.composites[slot]
                    .definition_for(transaction).active_fields()
                    .filter(|field| field.user_type.is_some()).count(),
                NamedTypeSlot::Enum(_) => unreachable!(),
            };
            edges_needed = edges_needed.checked_add(edges).ok_or_else(full)?;
        }
        let edges = arena.alloc_slice_with(edges_needed, |_| None).map_err(|_| full())?;
        let queue = arena.alloc_slice_with(count.checked_mul(2).ok_or_else(full)?, |_| 0usize)
            .map_err(|_| full())?;
        {
        let mut builder = Builder {
            catalog: &catalog,
            transaction,
            domains,
            sources,
            heads,
            edges,
            used: 0,
        };
        for (slot, source) in sources.iter().enumerate().take(domains) {
            if source.is_none() { continue }
            let definition = catalog.domains[slot].definition_for(transaction);
            for parent in [definition.base_domain, definition.base_user_type].into_iter().flatten() {
                builder.link(parent, slot, definition.base, false)?;
            }
        }
        for (slot, source) in sources.iter().enumerate().skip(domains) {
            if source.is_none() { continue }
            let definition = catalog.composites[slot - domains].definition_for(transaction);
            for field in definition.active_fields() {
                if let Some(parent) = field.user_type {
                    builder.link(parent, slot, field.ctype, true)?;
                }
            }
        }
        }
        let target = sources.get_mut(target).filter(|_| target < domains)
            .and_then(Option::as_mut).ok_or_else(|| {
            sql_err!(sqlstate::UNDEFINED_OBJECT, "domain validation target is not visible")
        })?;
        let root = target.identity.slot().index();
        target.usage = Usage::Value;
        let name = catalog.domains[root].definition_for(transaction).name;
        let mut pending = 1;
        let mut next = 0;
        queue[0] = root;
        while next < pending {
            let source = queue[next];
            next += 1;
            let usage = sources[source].expect("queued type source").usage;
            let mut edge = heads[source];
            while let Some(position) = edge {
                let dependency = edges[position].expect("linked type dependency");
                let wanted = if dependency.container || usage == Usage::Container {
                    Usage::Container
                } else {
                    Usage::Value
                };
                let dependent = sources[dependency.dependent].as_mut().expect("visible dependent source");
                if dependent.usage == Usage::Unrelated
                    || (dependent.usage == Usage::Value && wanted == Usage::Container)
                {
                    dependent.usage = wanted;
                    *queue.get_mut(pending).ok_or_else(full)? = dependency.dependent;
                    pending += 1;
                }
                edge = dependency.next;
            }
        }
        Ok(DomainValidationDependencies {
            origin: self.catalog_sources.origin(),
            database: super::current_database(),
            name,
            domains,
            sources,
        })
    }
}

impl DomainValidationDependencies<'_> {
    pub(crate) fn columns(
        &self,
        storage: &Storage,
        definition: &TableDef,
        transaction: u32,
    ) -> Result<[bool; super::MAX_COLUMNS], SqlError> {
        if storage.catalog_sources.origin() != self.origin || super::current_database() != self.database {
            return Err(sql_err!(sqlstate::INTERNAL_ERROR, "domain dependency graph belongs to different storage or database"));
        }
        let catalog = storage.type_catalog.lock().expect("type catalog lock poisoned");
        let mut affected = [false; super::MAX_COLUMNS];
        for (column, definition_column) in definition.columns().iter().enumerate() {
            let Some(name) = definition_column.user_type else { continue };
            let source = catalog.resolve_source_reference(
                TypeReference::from_bound_name(name)?,
                self.database,
                transaction,
            )?.ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "domain column source no longer exists"))?;
            let Some(position) = index(source.identity.slot(), self.domains) else { continue };
            let selected = self.sources.get(position).and_then(Option::as_ref).ok_or_else(|| {
                sql_err!(sqlstate::UNDEFINED_OBJECT, "domain column source was not captured")
            })?;
            if selected.identity != source.identity {
                return Err(sql_err!(sqlstate::UNDEFINED_OBJECT, "domain column source has been replaced"));
            }
            if selected.version != catalog.type_definition_version(source.identity.slot(), transaction) {
                return Err(sql_err!(sqlstate::SERIALIZATION_FAILURE, "type definition changed while preparing domain validation"));
            }
            if selected.usage == Usage::Container
                || (selected.usage == Usage::Value && definition_column.ctype != selected.base)
            {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "cannot alter type \"{}\" because column \"{}.{}\" uses it",
                    self.name.as_str(), definition.name.as_str(), definition_column.name.as_str()
                ));
            }
            affected[column] = selected.usage == Usage::Value;
        }
        Ok(affected)
    }
}
