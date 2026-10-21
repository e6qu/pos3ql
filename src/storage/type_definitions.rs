//! Readers retain complete type definitions independently of live catalog slots.

use std::sync::Arc;

use super::definition_pool::{
    DefinitionImageHandle, DefinitionImagePool, DefinitionImages, DefinitionKey,
};
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
pub(super) struct TypeImageKey {
    source: TypeIdentity,
}

impl TypeImageKey {
    pub(super) fn source(self) -> TypeIdentity {
        self.source
    }
}

#[derive(Clone, Copy)]
pub(crate) enum TypeDefinitionTarget {
    Slot(NamedTypeSlot),
    Bound(TypeReference),
}

impl TypeDefinitionTarget {
    pub(super) fn slot(self) -> NamedTypeSlot {
        match self {
            Self::Slot(slot) => slot,
            Self::Bound(reference) => reference.slot(),
        }
    }

    fn reference(self, storage: &Storage, transaction: u32) -> Result<TypeReference, SqlError> {
        if let Self::Bound(reference) = self {
            return Ok(reference);
        }
        let catalog = storage
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let snapshot = catalog
            .type_snapshot(self.slot(), transaction)?
            .filter(|snapshot| snapshot.identity.database() == super::current_database())
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "type source no longer exists"))?;
        TypeReference::from_bound_name(snapshot.name)
    }

    pub(super) fn validate(
        self,
        source: TypeIdentity,
        database: super::DatabaseOid,
    ) -> Result<(), SqlError> {
        let matches = match self {
            Self::Slot(slot) => source.slot() == slot && source.database() == database,
            Self::Bound(reference) => reference.refers_to(source, database),
        };
        if matches {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "selected type source does not match its captured lifetime"
            ))
        }
    }
}

pub(crate) struct TypeDefinitionRead<Definition> {
    _lease: super::spill_generations::StatementFileLease,
    image: DefinitionImageHandle<TypeImageKey, Definition>,
}

impl<Definition> TypeDefinitionRead<Definition> {
    /// The statement reference must retain the root owning this image chain.
    pub(super) unsafe fn new(
        lease: super::spill_generations::StatementFileLease,
        image: DefinitionImageHandle<TypeImageKey, Definition>,
    ) -> Self {
        Self {
            _lease: lease,
            image,
        }
    }
}

impl<Definition> core::ops::Deref for TypeDefinitionRead<Definition> {
    type Target = Definition;
    fn deref(&self) -> &Self::Target {
        // SAFETY: the owning statement reference keeps the immutable cell alive.
        unsafe { self.image.definition() }
    }
}

impl DefinitionKey for TypeImageKey {
    fn index(self) -> usize {
        self.source.slot().index()
    }
}

impl TypeImageKey {
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

fn same_type_name(left: Option<super::UserTypeName>, right: Option<super::UserTypeName>) -> bool {
    left == right
        && left.map(|name| (name.schema, name.name)) == right.map(|name| (name.schema, name.name))
}

fn same_domain(image: &DomainDef, source: &DomainDef, transaction: u32) -> bool {
    let pending = source
        .pending_definition
        .as_ref()
        .filter(|pending| pending.txid == transaction);
    let (schema, name) = pending
        .and_then(|pending| pending.identity)
        .map_or((source.schema, source.name), |identity| {
            (identity.schema, identity.name)
        });
    let (base_domain, base_user_type, base, type_mod, not_null, default, checks) = match pending {
        Some(pending) => (
            pending.spec.base_domain,
            pending.spec.base_user_type,
            pending.spec.base,
            pending.spec.base_type_mod,
            pending.spec.not_null,
            pending.spec.default_expr,
            &pending.spec.checks[..pending.spec.n_checks],
        ),
        None => (
            source.base_domain,
            source.base_user_type,
            source.base,
            source.base_type_mod,
            source.not_null,
            source.default_expr,
            source.checks(),
        ),
    };
    image.schema == schema
        && image.name == name
        && image.ownership == source.ownership
        && image.ddl_state == source.ddl_state
        && same_type_name(image.base_domain, base_domain)
        && same_type_name(image.base_user_type, base_user_type)
        && image.base == base
        && image.base_type_mod == type_mod
        && image.not_null == not_null
        && image.default_expr == default
        && image.checks().len() == checks.len()
        && image.checks().iter().zip(checks).all(|(left, right)| {
            left.name == right.name
                && left.expression == right.expression
                && left.validation == right.validation
        })
}

fn same_enum(image: &EnumDefinition, source: &super::EnumDef) -> bool {
    image.schema == source.schema
        && image.name == source.name
        && image.ownership == source.ownership
        && image.ddl_state == source.ddl_state
        && image.member_count == source.member_count as usize
        && image.image.selects(source.member_image)
}

fn same_composite(image: &CompositeDef, source: &CompositeDef, transaction: u32) -> bool {
    let pending = source
        .pending_definition
        .as_ref()
        .filter(|pending| pending.txid == transaction);
    let (schema, name) = pending.map_or((source.schema, source.name), |pending| {
        (pending.schema, pending.name)
    });
    let fields = source.fields_for(transaction);
    image.schema == schema
        && image.name == name
        && image.ownership == source.ownership
        && image.ddl_state == source.ddl_state
        && image.n_fields == fields.len()
        && image.fields().iter().zip(fields).all(|(left, right)| {
            left.attribute_number == right.attribute_number
                && left.name == right.name
                && left.ctype == right.ctype
                && left.type_mod == right.type_mod
                && left.collation == right.collation
                && same_type_name(left.user_type, right.user_type)
                && left.dropped == right.dropped
                && left.not_null == right.not_null
        })
}

impl TypeDefinitionImages {
    pub(super) fn domain_image(
        &self,
        storage: &Storage,
        target: TypeDefinitionTarget,
        transaction: u32,
        selected: Option<usize>,
    ) -> Result<DefinitionImageHandle<TypeImageKey, DomainDef>, SqlError> {
        self.validate_storage(storage)?;
        if !matches!(target.slot(), NamedTypeSlot::Domain(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not a domain"
            ));
        }
        let image = if let Some(index) = selected {
            self.domains.handle_at(index)
        } else {
            let reference = target.reference(storage, transaction)?;
            self.domains
                .handle(self.domain(storage, reference, transaction)?)
        };
        // SAFETY: the caller's statement retains this chain, including cache hits.
        target.validate(unsafe { image.key().source() }, self.database)?;
        Ok(image)
    }

    pub(super) fn enum_image(
        &self,
        storage: &Storage,
        target: TypeDefinitionTarget,
        transaction: u32,
        selected: Option<usize>,
    ) -> Result<DefinitionImageHandle<TypeImageKey, EnumDefinition>, SqlError> {
        self.validate_storage(storage)?;
        if !matches!(target.slot(), NamedTypeSlot::Enum(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not an enum"
            ));
        }
        let image = if let Some(index) = selected {
            self.enums.handle_at(index)
        } else {
            let reference = target.reference(storage, transaction)?;
            self.enums
                .handle(self.enumeration(storage, reference, transaction)?)
        };
        // SAFETY: the caller's statement retains this chain, including cache hits.
        target.validate(unsafe { image.key().source() }, self.database)?;
        Ok(image)
    }

    pub(super) fn composite_image(
        &self,
        storage: &Storage,
        target: TypeDefinitionTarget,
        transaction: u32,
        selected: Option<usize>,
    ) -> Result<DefinitionImageHandle<TypeImageKey, CompositeDef>, SqlError> {
        self.validate_storage(storage)?;
        if !matches!(target.slot(), NamedTypeSlot::Composite(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not a composite"
            ));
        }
        let image = if let Some(index) = selected {
            self.composites.handle_at(index)
        } else {
            let reference = target.reference(storage, transaction)?;
            self.composites
                .handle(self.composite(storage, reference, transaction)?)
        };
        // SAFETY: the caller's statement retains this chain, including cache hits.
        target.validate(unsafe { image.key().source() }, self.database)?;
        Ok(image)
    }

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

    pub(crate) fn domain(
        &self,
        storage: &Storage,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<&DomainDef, SqlError> {
        self.validate_storage(storage)?;
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
        if let Some((_, _, definition)) = self.domains.find(|candidate, definition| {
            candidate.source == key.source && same_domain(definition, source, transaction)
        }) {
            return Ok(definition);
        }
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

    pub(crate) fn enumeration(
        &self,
        storage: &Storage,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<&EnumDefinition, SqlError> {
        self.validate_storage(storage)?;
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
        if let Some((_, _, definition)) = self.enums.find(|candidate, definition| {
            candidate.source == key.source && same_enum(definition, &source)
        }) {
            return Ok(definition);
        }
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

    pub(crate) fn composite(
        &self,
        storage: &Storage,
        reference: TypeReference,
        transaction: u32,
    ) -> Result<&CompositeDef, SqlError> {
        self.validate_storage(storage)?;
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
        if let Some((_, _, definition)) = self.composites.find(|candidate, definition| {
            candidate.source == key.source && same_composite(definition, source, transaction)
        }) {
            return Ok(definition);
        }
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

    enum SelectedType<'reader> {
        Domain(&'reader DomainDef),
        Enum(&'reader EnumDefinition),
        Composite(&'reader CompositeDef),
    }

    fn capture<'reader>(
        images: &'reader TypeDefinitionImages,
        storage: &Storage,
        reference: TypeReference,
    ) -> Result<SelectedType<'reader>, SqlError> {
        match reference.slot() {
            NamedTypeSlot::Domain(_) => images
                .domain(storage, reference, 0)
                .map(SelectedType::Domain),
            NamedTypeSlot::Enum(_) => images
                .enumeration(storage, reference, 0)
                .map(SelectedType::Enum),
            NamedTypeSlot::Composite(_) => images
                .composite(storage, reference, 0)
                .map(SelectedType::Composite),
        }
    }

    fn assert_retained(selected: &SelectedType<'_>) {
        match selected {
            SelectedType::Domain(definition) => {
                assert_eq!(definition.name.as_str(), "reader_source");
                assert!(definition.not_null);
                assert_eq!(definition.checks()[0].expression.as_str(), "VALUE > 0");
            }
            SelectedType::Enum(definition) => {
                assert_eq!(definition.name.as_str(), "reader_source");
                assert_eq!(
                    definition.members().next().unwrap().label.as_str(),
                    "before"
                );
            }
            SelectedType::Composite(definition) => {
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
                let selected = capture(&images, &storage, reference).unwrap();
                assert!(storage.type_catalog.try_lock().is_ok());
                retire(&mut storage, source);
                assert_retained(&selected);
                assert_eq!(
                    create(&mut storage, source).unwrap_err().sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_retained(&selected);
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
                let selected = capture(&images, &storage, reference).unwrap();
                retire(&mut storage, source);
                drop(storage);
                assert_retained(&selected);
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
                    sqlstate::UNDEFINED_OBJECT
                );
                let selected = capture(&images, &first, first_reference).unwrap();
                assert_retained(&selected);
            });
        }
    }
}
