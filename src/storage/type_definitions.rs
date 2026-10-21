//! Readers retain complete type definitions independently of live catalog slots.

use std::sync::Arc;

use super::definition_pool::{DefinitionImagePool, DefinitionImages, DefinitionKey};
use super::type_names::NamedTypeSlot;
use super::type_references::TypeIdentity;
use super::{
    CompositeDef, DomainDef, EnumDefinition, EnumImageRead, Storage, TypeCatalog, TypeReference,
};
use crate::config::Config;
use crate::mem::budget::{Budget, BudgetError};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy)]
struct TypeImageKey {
    source: TypeIdentity,
    transaction: u32,
}

impl DefinitionKey for TypeImageKey {
    fn index(self) -> usize {
        self.source.slot().index()
    }
}

impl TypeImageKey {
    fn matches(
        self,
        reference: TypeReference,
        transaction: u32,
        database: super::DatabaseOid,
    ) -> bool {
        self.transaction == transaction && reference.refers_to(self.source, database)
    }

    fn select(
        catalog: &TypeCatalog,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<Self, SqlError> {
        let source = catalog
            .resolve_source_reference(reference, super::current_database(), transaction)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "captured type source no longer exists"
                )
            })?;
        Ok(Self {
            source: source.identity,
            transaction,
        })
    }
}

type DomainPool = DefinitionImagePool<TypeImageKey, DomainDef>;
type EnumPool = DefinitionImagePool<TypeImageKey, EnumDefinition>;
type CompositePool = DefinitionImagePool<TypeImageKey, CompositeDef>;

pub(super) struct TypeDefinitionImagePools {
    domains: Arc<DomainPool>,
    enums: Arc<EnumPool>,
    composites: Arc<CompositePool>,
}

fn capacity(config: &Config, sources: usize) -> usize {
    sources
        .saturating_mul(config.max_catalog_versions_per_object.saturating_add(1))
        .saturating_mul(config.query_workspace_slots)
}

impl TypeDefinitionImagePools {
    pub(super) fn budget_bytes(config: &Config) -> usize {
        DomainPool::budget_bytes(capacity(config, config.max_domains), config.max_domains)
            .saturating_add(EnumPool::budget_bytes(
                capacity(config, config.max_enums),
                config.max_enums,
            ))
            .saturating_add(CompositePool::budget_bytes(
                capacity(config, config.max_composites),
                config.max_composites,
            ))
    }

    pub(super) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        Ok(Self {
            domains: DomainPool::new(
                budget,
                capacity(config, config.max_domains),
                config.max_domains,
                "domain definition image pool",
            )?,
            enums: EnumPool::new(
                budget,
                capacity(config, config.max_enums),
                config.max_enums,
                "enum definition image pool",
            )?,
            composites: CompositePool::new(
                budget,
                capacity(config, config.max_composites),
                config.max_composites,
                "composite definition image pool",
            )?,
        })
    }

    pub(super) fn retains(&self, slot: NamedTypeSlot) -> bool {
        match slot {
            NamedTypeSlot::Domain(index) => self.domains.retains(index),
            NamedTypeSlot::Enum(index) => self.enums.retains(index),
            NamedTypeSlot::Composite(index) => self.composites.retains(index),
        }
    }
}

pub(crate) struct TypeDefinitionImages {
    database: super::DatabaseOid,
    domains: DefinitionImages<TypeImageKey, DomainDef>,
    enums: DefinitionImages<TypeImageKey, EnumDefinition>,
    composites: DefinitionImages<TypeImageKey, CompositeDef>,
}

impl TypeDefinitionImages {
    pub(super) fn new(pools: &TypeDefinitionImagePools, database: super::DatabaseOid) -> Self {
        Self {
            database,
            domains: DefinitionImages::new(&pools.domains),
            enums: DefinitionImages::new(&pools.enums),
            composites: DefinitionImages::new(&pools.composites),
        }
    }

    fn validate_storage(&self, storage: &Storage) -> Result<(), SqlError> {
        let pools = &storage.type_definition_images;
        if !self.domains.belongs_to(&pools.domains)
            || !self.enums.belongs_to(&pools.enums)
            || !self.composites.belongs_to(&pools.composites)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "type definition reader belongs to different storage"
            ));
        }
        if self.database != super::current_database() {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "type definition reader belongs to a different database"
            ));
        }
        Ok(())
    }

    pub(crate) fn retained_domain(
        &self,
        reference: TypeReference,
        transaction: u32,
    ) -> Option<&DomainDef> {
        self.domains
            .find(|key| key.matches(reference, transaction, self.database))
            .map(|(_, value)| value)
    }

    pub(crate) fn domain(
        &self,
        storage: &Storage,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<&DomainDef, SqlError> {
        self.validate_storage(storage)?;
        if let Some(definition) = self.retained_domain(reference, transaction) {
            return Ok(definition);
        }
        let NamedTypeSlot::Domain(slot) = reference.slot() else {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "captured type source is not a domain"
            ));
        };
        let catalog = storage
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let key = TypeImageKey::select(&catalog, reference, transaction)?;
        let source = &catalog.domains[slot];
        self.domains.copy(key, source, |definition| {
            if let Some(pending) = source
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
            {
                if let Some(identity) = pending.identity {
                    definition.schema = identity.schema;
                    definition.name = identity.name;
                }
                definition.base_domain = pending.spec.base_domain;
                definition.base_user_type = pending.spec.base_user_type;
                definition.base = pending.spec.base;
                definition.base_type_mod = pending.spec.base_type_mod;
                definition.not_null = pending.spec.not_null;
                definition.default_expr = pending.spec.default_expr;
                definition.checks.copy_from_slice(&pending.spec.checks);
                definition.n_checks = pending.spec.n_checks;
            }
            definition.pending_definition = None;
        })
    }

    pub(crate) fn retained_enum(
        &self,
        reference: TypeReference,
        transaction: u32,
    ) -> Option<&EnumDefinition> {
        self.enums
            .find(|key| key.matches(reference, transaction, self.database))
            .map(|(_, value)| value)
    }

    pub(crate) fn enumeration(
        &self,
        storage: &Storage,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<&EnumDefinition, SqlError> {
        self.validate_storage(storage)?;
        if let Some(definition) = self.retained_enum(reference, transaction) {
            return Ok(definition);
        }
        let NamedTypeSlot::Enum(slot) = reference.slot() else {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "captured type source is not an enum"
            ));
        };
        let catalog = storage
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let key = TypeImageKey::select(&catalog, reference, transaction)?;
        let source = catalog.enums[slot].definition_for(transaction);
        self.enums.insert(
            key,
            EnumDefinition {
                image: EnumImageRead::capture(&storage.enum_member_images, source.member_image),
                member_count: source.member_count as usize,
                database: source.database,
                created_at: source.created_at,
                schema: source.schema,
                name: source.name,
                ownership: source.ownership,
                ddl_state: source.ddl_state,
            },
        )
    }

    pub(crate) fn retained_composite(
        &self,
        reference: TypeReference,
        transaction: u32,
    ) -> Option<&CompositeDef> {
        self.composites
            .find(|key| key.matches(reference, transaction, self.database))
            .map(|(_, value)| value)
    }

    pub(crate) fn composite(
        &self,
        storage: &Storage,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<&CompositeDef, SqlError> {
        self.validate_storage(storage)?;
        if let Some(definition) = self.retained_composite(reference, transaction) {
            return Ok(definition);
        }
        let NamedTypeSlot::Composite(slot) = reference.slot() else {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "captured type source is not a composite"
            ));
        };
        let catalog = storage
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let key = TypeImageKey::select(&catalog, reference, transaction)?;
        let source = &catalog.composites[slot];
        self.composites.copy(key, source, |definition| {
            if let Some(pending) = source
                .pending_definition
                .as_ref()
                .filter(|pending| pending.txid == transaction)
            {
                definition.schema = pending.schema;
                definition.name = pending.name;
                definition.fields.copy_from_slice(&pending.fields);
                definition.n_fields = pending.n_fields;
            }
            definition.pending_definition = None;
        })
    }
}

impl Storage {
    pub(crate) fn type_definition_images(&self) -> TypeDefinitionImages {
        TypeDefinitionImages::new(&self.type_definition_images, super::current_database())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{test_budget, test_config};
    use super::super::{
        CheckConstraint, ColType, CompositeFieldDef, CompositeSpec, ConstraintValidation,
        DomainSpec, EnumMember, EnumSpec, MAX_COMPOSITE_FIELDS, MAX_DOMAIN_CHECKS, SqlName,
    };
    use super::*;

    const SOURCES: [NamedTypeSlot; 3] = [
        NamedTypeSlot::Domain(0),
        NamedTypeSlot::Enum(0),
        NamedTypeSlot::Composite(0),
    ];

    fn fixture() -> Storage {
        let mut config = test_config();
        config.max_domains = 1;
        config.max_enums = 1;
        config.max_composites = 1;
        config.max_catalog_versions_per_object = 1;
        config.query_workspace_slots = 1;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget).unwrap()
    }

    fn create(storage: &mut Storage, source: NamedTypeSlot) -> Result<usize, SqlError> {
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse("reader_source").unwrap();
        match source {
            NamedTypeSlot::Domain(_) => {
                let mut checks = [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS];
                checks[0] = CheckConstraint {
                    name: SqlName::parse("positive").unwrap(),
                    expression: crate::util::StackStr::from_str("VALUE > 0"),
                    validation: ConstraintValidation::EnforcedValidated,
                };
                storage.create_domain(
                    schema,
                    name,
                    DomainSpec {
                        base_domain: None,
                        base_user_type: None,
                        base: ColType::Int4,
                        base_type_mod: -1,
                        not_null: true,
                        default_expr: None,
                        checks,
                        n_checks: 1,
                    },
                    0,
                )
            }
            NamedTypeSlot::Enum(_) => storage.create_enum(
                schema,
                name,
                EnumSpec {
                    members: &[EnumMember {
                        label: SqlName::parse("before").unwrap(),
                        legacy_label: None,
                        sort: 1.0,
                    }],
                },
                0,
            ),
            NamedTypeSlot::Composite(_) => {
                let mut fields = [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS];
                fields[0] = CompositeFieldDef {
                    name: SqlName::parse("before").unwrap(),
                    attribute_number: 1,
                    ctype: ColType::Int4,
                    dropped: false,
                    ..CompositeFieldDef::EMPTY
                };
                storage.create_composite(
                    schema,
                    name,
                    CompositeSpec {
                        fields,
                        n_fields: 1,
                    },
                    0,
                )
            }
        }
    }

    fn reference(storage: &Storage, source: NamedTypeSlot) -> TypeReference {
        let catalog = storage.type_catalog.lock().unwrap();
        TypeReference::from_bound_name(catalog.type_snapshot(source, 0).unwrap().unwrap().name)
            .unwrap()
    }

    fn retain(
        images: &TypeDefinitionImages,
        storage: &Storage,
        reference: TypeReference,
    ) -> Result<(), SqlError> {
        match reference.slot() {
            NamedTypeSlot::Domain(_) => {
                images.domain(storage, reference, 0)?;
            }
            NamedTypeSlot::Enum(_) => {
                images.enumeration(storage, reference, 0)?;
            }
            NamedTypeSlot::Composite(_) => {
                images.composite(storage, reference, 0)?;
            }
        }
        Ok(())
    }

    fn retire(storage: &mut Storage, source: NamedTypeSlot) {
        match source {
            NamedTypeSlot::Domain(slot) => {
                assert_eq!(
                    storage.drop_domain("public", "reader_source", 0).unwrap(),
                    Some(slot)
                );
                storage.commit_domain_drop(slot);
            }
            NamedTypeSlot::Enum(slot) => {
                assert_eq!(
                    storage.drop_enum("public", "reader_source", 0).unwrap(),
                    Some(slot)
                );
                storage.commit_enum_drop(slot);
            }
            NamedTypeSlot::Composite(slot) => {
                assert_eq!(
                    storage
                        .drop_composite("public", "reader_source", 0)
                        .unwrap(),
                    Some(slot)
                );
                storage.commit_composite_drop(slot);
            }
        }
    }

    fn assert_retained(images: &TypeDefinitionImages, reference: TypeReference) {
        match reference.slot() {
            NamedTypeSlot::Domain(_) => {
                let definition = images.retained_domain(reference, 0).unwrap();
                assert_eq!(definition.name.as_str(), "reader_source");
                assert!(definition.not_null);
                assert_eq!(definition.checks()[0].expression.as_str(), "VALUE > 0");
            }
            NamedTypeSlot::Enum(_) => {
                let definition = images.retained_enum(reference, 0).unwrap();
                assert_eq!(definition.name.as_str(), "reader_source");
                assert_eq!(
                    definition.members().next().unwrap().label.as_str(),
                    "before"
                );
            }
            NamedTypeSlot::Composite(_) => {
                let definition = images.retained_composite(reference, 0).unwrap();
                assert_eq!(definition.name.as_str(), "reader_source");
                assert_eq!(definition.fields()[0].name.as_str(), "before");
            }
        }
    }

    #[test]
    fn type_definition_images_keep_retired_slots_unavailable_until_the_reader_releases() {
        for source in SOURCES {
            let mut storage = fixture();
            create(&mut storage, source).unwrap();
            let reference = reference(&storage, source);
            let images = storage.type_definition_images();
            crate::mem::guard::forbid_alloc(|| {
                retain(&images, &storage, reference).unwrap();
                assert!(storage.type_catalog.try_lock().is_ok());
                retire(&mut storage, source);
                assert_retained(&images, reference);
                assert_eq!(
                    create(&mut storage, source).unwrap_err().sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_retained(&images, reference);
                drop(images);
                assert_eq!(create(&mut storage, source).unwrap(), source.index());
                let replacement = storage.type_definition_images();
                assert_eq!(
                    retain(&replacement, &storage, reference)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::UNDEFINED_OBJECT
                );
            });
        }
    }

    #[test]
    fn retained_type_definitions_remain_readable_after_storage_is_removed() {
        for source in SOURCES {
            let mut storage = fixture();
            create(&mut storage, source).unwrap();
            let reference = reference(&storage, source);
            let images = storage.type_definition_images();
            crate::mem::guard::forbid_alloc(|| {
                retain(&images, &storage, reference).unwrap();
                retire(&mut storage, source);
                drop(storage);
                assert_retained(&images, reference);
                drop(images);
            });
        }
    }

    #[test]
    fn type_definition_images_reject_foreign_storage_without_consuming_capacity() {
        for source in SOURCES {
            let mut first = fixture();
            let mut other = fixture();
            create(&mut first, source).unwrap();
            create(&mut other, source).unwrap();
            let first_reference = reference(&first, source);
            let other_reference = reference(&other, source);
            let images = first.type_definition_images();
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    retain(&images, &other, other_reference)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::INTERNAL_ERROR
                );
                assert_eq!(
                    retain(&images, &first, other_reference)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATATYPE_MISMATCH
                );
                retain(&images, &first, first_reference).unwrap();
                assert_retained(&images, first_reference);
            });
        }
    }
}
