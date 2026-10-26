//! Readers retain complete type definitions independently of live catalog slots.

use super::spill_generations::StatementDefinitionIndex;
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
    version: u64,
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

    fn reference(
        self,
        storage: &Storage,
        transaction: u32,
    ) -> Result<Option<TypeReference>, SqlError> {
        if let Self::Bound(reference) = self {
            return Ok(Some(reference));
        }
        let catalog = storage
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let snapshot = catalog
            .type_snapshot(self.slot(), transaction)?
            .filter(|snapshot| snapshot.identity.database() == super::current_database());
        snapshot
            .map(|snapshot| TypeReference::from_bound_name(snapshot.name))
            .transpose()
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
            version: catalog.type_definition_version(source.identity.slot(), transaction),
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
        && image.not_null_name
            == pending.map_or(source.not_null_name, |pending| pending.spec.not_null_name)
        && image.default_expr == default
        && image.checks().len() == checks.len()
        && image.checks().iter().zip(checks).all(|(left, right)| {
            left.name == right.name
                && left.expression == right.expression
                && left.validation == right.validation
        })
}

fn same_enum(image: &EnumDefinition, source: &super::EnumDef, safe_member_count: usize) -> bool {
    image.schema == source.schema
        && image.name == source.name
        && image.ownership == source.ownership
        && image.ddl_state == source.ddl_state
        && image.member_count == source.member_count as usize
        && image.safe_member_count == safe_member_count
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
        selected: Option<StatementDefinitionIndex<'_, DomainDef>>,
    ) -> Result<Option<DefinitionImageHandle<TypeImageKey, DomainDef>>, SqlError> {
        self.validate_storage(storage)?;
        if !matches!(target.slot(), NamedTypeSlot::Domain(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not a domain"
            ));
        }
        let image = if let Some(index) = selected {
            self.domains.handle_at(index.index(self)?)
        } else {
            let Some(reference) = target.reference(storage, transaction)? else {
                return Ok(None);
            };
            self.domains
                .handle(self.domain(storage, reference, transaction)?)
        };
        // SAFETY: the caller's statement retains this chain, including cache hits.
        target.validate(unsafe { image.key().source() }, self.database)?;
        Ok(Some(image))
    }

    pub(super) fn enum_image(
        &self,
        storage: &Storage,
        target: TypeDefinitionTarget,
        transaction: u32,
        selected: Option<StatementDefinitionIndex<'_, EnumDefinition>>,
    ) -> Result<Option<DefinitionImageHandle<TypeImageKey, EnumDefinition>>, SqlError> {
        self.validate_storage(storage)?;
        if !matches!(target.slot(), NamedTypeSlot::Enum(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not an enum"
            ));
        }
        let image = if let Some(index) = selected {
            self.enums.handle_at(index.index(self)?)
        } else {
            let Some(reference) = target.reference(storage, transaction)? else {
                return Ok(None);
            };
            self.enums
                .handle(self.enumeration(storage, reference, transaction)?)
        };
        // SAFETY: the caller's statement retains this chain, including cache hits.
        target.validate(unsafe { image.key().source() }, self.database)?;
        Ok(Some(image))
    }

    pub(super) fn composite_image(
        &self,
        storage: &Storage,
        target: TypeDefinitionTarget,
        transaction: u32,
        selected: Option<StatementDefinitionIndex<'_, CompositeDef>>,
    ) -> Result<Option<DefinitionImageHandle<TypeImageKey, CompositeDef>>, SqlError> {
        self.validate_storage(storage)?;
        if !matches!(target.slot(), NamedTypeSlot::Composite(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not a composite"
            ));
        }
        let image = if let Some(index) = selected {
            self.composites.handle_at(index.index(self)?)
        } else {
            let Some(reference) = target.reference(storage, transaction)? else {
                return Ok(None);
            };
            self.composites
                .handle(self.composite(storage, reference, transaction)?)
        };
        // SAFETY: the caller's statement retains this chain, including cache hits.
        target.validate(unsafe { image.key().source() }, self.database)?;
        Ok(Some(image))
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
            candidate.source == key.source
                && candidate.version == key.version
                && same_domain(definition, source, transaction)
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
        let safe_member_count = catalog.enums[slot].safe_member_count_for(transaction);
        if let Some((_, _, definition)) = self.enums.find(|candidate, definition| {
            candidate.source == key.source
                && candidate.version == key.version
                && same_enum(definition, &source, safe_member_count)
        }) {
            return Ok(definition);
        }
        self.enums.insert(
            key,
            EnumDefinition {
                image: EnumImageRead::capture(&storage.enum_member_images, source.member_image),
                member_count: source.member_count as usize,
                safe_member_count,
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
            candidate.source == key.source
                && candidate.version == key.version
                && same_composite(definition, source, transaction)
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
    /// Capture the complete referenced type graph before rows can wait for I/O.
    /// The returned statement reference keeps all selected definitions alive.
    pub(crate) fn select_type_definition_graph(
        &self,
        mut roots: impl Iterator<Item = super::UserTypeName>,
        transaction: u32,
        arena: &crate::mem::arena::Arena,
    ) -> Result<Option<super::StatementFileLease>, SqlError> {
        let Some(first) = roots.next() else {
            return Ok(None);
        };
        let owner = self.spill_roots.type_definition_reader()?;
        let scope = owner.enter()?;
        let mark = arena.mark();
        let result = (|| {
            let capacity = self
                .domain_count()
                .saturating_add(self.enum_count())
                .saturating_add(self.composite_count());
            let queue = arena.alloc_slice_with(capacity, |_| None).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "type definition graph exceeds the statement arena"
                )
            })?;
            let mut count = 0;
            for name in core::iter::once(first).chain(roots) {
                self.queue_type_definition(queue, &mut count, name, transaction)?;
            }
            let mut position = 0;
            while position < count {
                let reference = queue[position].expect("queued type reference is initialized");
                let target = TypeDefinitionTarget::Bound(reference);
                match reference.slot() {
                    NamedTypeSlot::Domain(_) => {
                        let source = self
                            .read_domain_definition(target, transaction)?
                            .expect("a bound type read reports absence as an error");
                        for child in [source.base_domain, source.base_user_type]
                            .into_iter()
                            .flatten()
                        {
                            self.queue_type_definition(queue, &mut count, child, transaction)?;
                        }
                    }
                    NamedTypeSlot::Enum(_) => {
                        self.read_enum_definition(target, transaction)?;
                    }
                    NamedTypeSlot::Composite(_) => {
                        let source = self
                            .read_composite_definition(target, transaction)?
                            .expect("a bound type read reports absence as an error");
                        for child in source.active_fields().filter_map(|field| field.user_type) {
                            self.queue_type_definition(queue, &mut count, child, transaction)?;
                        }
                    }
                }
                position += 1;
            }
            Ok::<(), SqlError>(())
        })();
        // SAFETY: only the temporary queue borrows this scratch. Definition
        // images and the returned statement owner reside in startup pools.
        unsafe {
            arena.rewind_to(mark);
        }
        result?;
        drop(scope);
        Ok(Some(owner))
    }

    fn queue_type_definition(
        &self,
        queue: &mut [Option<TypeReference>],
        count: &mut usize,
        name: super::UserTypeName,
        transaction: u32,
    ) -> Result<(), SqlError> {
        let reference = TypeReference::from_bound_name(name)?;
        if let Some(previous) = queue[..*count]
            .iter()
            .flatten()
            .find(|entry| entry.slot() == reference.slot())
        {
            if *previous != reference {
                // Captured and recovered spellings can name the same source.
                // Validate either spelling against the selected lifetime.
                let target = TypeDefinitionTarget::Bound(reference);
                match reference.slot() {
                    NamedTypeSlot::Domain(_) => {
                        self.read_domain_definition(target, transaction)?;
                    }
                    NamedTypeSlot::Enum(_) => {
                        self.read_enum_definition(target, transaction)?;
                    }
                    NamedTypeSlot::Composite(_) => {
                        self.read_composite_definition(target, transaction)?;
                    }
                }
            }
            return Ok(());
        }
        let Some(entry) = queue.get_mut(*count) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "type definition graph exceeds its configured source slots"
            ));
        };
        *entry = Some(reference);
        *count += 1;
        Ok(())
    }

    pub(crate) fn read_domain_definition(
        &self,
        target: TypeDefinitionTarget,
        transaction: u32,
    ) -> Result<Option<TypeDefinitionRead<DomainDef>>, SqlError> {
        if !matches!(target.slot(), NamedTypeSlot::Domain(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not a domain"
            ));
        }
        let reader = self.spill_roots.type_definition_reader()?;
        reader.type_definition(self, target, transaction, |images, selected| {
            images.domain_image(self, target, transaction, selected)
        })
    }

    pub(crate) fn read_enum_definition(
        &self,
        target: TypeDefinitionTarget,
        transaction: u32,
    ) -> Result<Option<TypeDefinitionRead<EnumDefinition>>, SqlError> {
        if !matches!(target.slot(), NamedTypeSlot::Enum(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not an enum"
            ));
        }
        let reader = self.spill_roots.type_definition_reader()?;
        reader.type_definition(self, target, transaction, |images, selected| {
            images.enum_image(self, target, transaction, selected)
        })
    }

    pub(crate) fn read_composite_definition(
        &self,
        target: TypeDefinitionTarget,
        transaction: u32,
    ) -> Result<Option<TypeDefinitionRead<CompositeDef>>, SqlError> {
        if !matches!(target.slot(), NamedTypeSlot::Composite(_)) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "selected type source is not a composite"
            ));
        }
        let reader = self.spill_roots.type_definition_reader()?;
        reader.type_definition(self, target, transaction, |images, selected| {
            images.composite_image(self, target, transaction, selected)
        })
    }

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

    fn fixture(versions: usize) -> Storage {
        let mut config = test_config();
        config.max_domains = 1;
        config.max_enums = 1;
        config.max_composites = 1;
        config.max_catalog_versions_per_object = versions;
        config.query_workspace_slots = 1;
        let mut budget = test_budget(&config);
        Storage::new(&config, &mut budget, 0).unwrap()
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
                        not_null_name: None,
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
                        identity: None,
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
        .map(super::super::TypeDdlUndo::slot)
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
                let dropped = storage
                    .stage_type_drop(
                        crate::storage::NamedTypeKind::Domain,
                        "public",
                        "reader_source",
                        0,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(dropped.slot(), slot);
                storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
            }
            NamedTypeSlot::Enum(slot) => {
                let dropped = storage
                    .stage_type_drop(
                        crate::storage::NamedTypeKind::Enum,
                        "public",
                        "reader_source",
                        0,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(dropped.slot(), slot);
                storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
            }
            NamedTypeSlot::Composite(slot) => {
                let dropped = storage
                    .stage_type_drop(
                        crate::storage::NamedTypeKind::Composite,
                        "public",
                        "reader_source",
                        0,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(dropped.slot(), slot);
                storage.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Commit);
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
            let mut storage = fixture(1);
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
            let mut storage = fixture(1);
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
            let mut first = fixture(1);
            let mut other = fixture(1);
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

    #[test]
    fn statement_type_readers_keep_selected_versions_and_nested_readers_select_current_values() {
        let mut storage = fixture(1);
        create(&mut storage, NamedTypeSlot::Composite(0)).unwrap();
        let target = TypeDefinitionTarget::Slot(NamedTypeSlot::Composite(0));
        let outer = storage.spill_roots.retain_statement().unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let scope = outer.enter().unwrap();
            let old = storage
                .read_composite_definition(target, 0)
                .unwrap()
                .unwrap();
            assert_eq!(old.fields()[0].name.as_str(), "before");
            drop(scope);
            let mut altered = storage.composite_for(0, 0);
            altered.fields[0].name = SqlName::parse("after").unwrap();
            storage
                .stage_composite_alter(storage.prepare_composite_edit(0, 7).unwrap().1, altered, 7)
                .unwrap();
            storage
                .commit_composite_alter(storage.prepare_composite_edit(0, 7).unwrap().1)
                .unwrap();

            let scope = outer.enter().unwrap();
            assert_eq!(
                storage
                    .read_composite_definition(target, 0)
                    .unwrap()
                    .unwrap()
                    .fields()[0]
                    .name
                    .as_str(),
                "before"
            );
            for _ in 0..100 {
                let child = storage.spill_roots.retain_statement().unwrap();
                let child_scope = child.enter().unwrap();
                let current = storage
                    .read_composite_definition(target, 0)
                    .unwrap()
                    .unwrap();
                assert_eq!(current.fields()[0].name.as_str(), "after");
                assert_eq!(old.fields()[0].name.as_str(), "before");
                drop(child_scope);
            }
            drop(scope);
            drop(outer);
            drop(storage);
            assert_eq!(old.fields()[0].name.as_str(), "before");
        });
    }

    #[test]
    fn child_type_reader_retains_its_parent_after_scopes_and_storage_are_removed() {
        let mut storage = fixture(1);
        create(&mut storage, NamedTypeSlot::Domain(0)).unwrap();
        let target = TypeDefinitionTarget::Slot(NamedTypeSlot::Domain(0));
        let outer = storage.spill_roots.retain_statement().unwrap();
        let child = storage.spill_roots.retain_statement().unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let outer_scope = outer.enter().unwrap();
            let child_scope = child.enter().unwrap();
            let read = storage.read_domain_definition(target, 0).unwrap().unwrap();
            drop(child_scope);
            drop(outer_scope);
            drop(outer);
            drop(child);
            drop(storage);
            assert!(read.not_null);
            assert_eq!(read.checks()[0].expression.as_str(), "VALUE > 0");
        });
    }

    #[test]
    fn statement_type_absence_is_stable_and_a_new_transaction_reselects() {
        let mut storage = fixture(1);
        let statement = storage.spill_roots.retain_statement().unwrap();
        let target = TypeDefinitionTarget::Slot(NamedTypeSlot::Domain(0));
        crate::mem::guard::forbid_alloc(|| {
            let scope = statement.enter().unwrap();
            assert!(storage.read_domain_definition(target, 0).unwrap().is_none());
            drop(scope);
            create(&mut storage, NamedTypeSlot::Domain(0)).unwrap();
            let bound = TypeDefinitionTarget::Bound(reference(&storage, NamedTypeSlot::Domain(0)));
            let scope = statement.enter().unwrap();
            assert!(storage.read_domain_definition(target, 0).unwrap().is_none());
            assert_eq!(
                storage
                    .read_domain_definition(bound, 0)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::UNDEFINED_OBJECT
            );
            assert_eq!(
                storage
                    .read_enum_definition(target, 0)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::DATATYPE_MISMATCH
            );
            assert!(
                storage
                    .read_domain_definition(bound, 7)
                    .unwrap()
                    .unwrap()
                    .not_null
            );
            drop(scope);
        });
    }

    #[test]
    fn selected_enum_keeps_transaction_order_and_new_label_safety_after_commit() {
        use crate::sql::types::{Datum, EnumMemberId};
        let mut storage = fixture(1);
        create(&mut storage, NamedTypeSlot::Enum(0)).unwrap();
        let members = [
            EnumMember {
                identity: None,
                label: SqlName::parse("before").unwrap(),
                legacy_label: Some(SqlName::parse("before").unwrap()),
                sort: 1.0,
            },
            EnumMember {
                identity: None,
                label: SqlName::parse("after").unwrap(),
                legacy_label: None,
                sort: 2.0,
            },
        ];
        storage
            .stage_enum_alter(
                storage.prepare_enum_edit(0, 7).unwrap().1,
                SqlName::parse("public").unwrap(),
                SqlName::parse("reader_source").unwrap(),
                &members,
                7,
            )
            .unwrap();
        let target = TypeDefinitionTarget::Slot(NamedTypeSlot::Enum(0));
        let statement = storage.spill_roots.retain_statement().unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let scope = statement.enter().unwrap();
            let selected = storage.read_enum_definition(target, 7).unwrap().unwrap();
            assert!(!selected.label_is_uncommitted("before"));
            assert!(selected.label_is_uncommitted("after"));
            let value = |member, label| Datum::Enum {
                slot: 0,
                member: Some(EnumMemberId::from_ordinal(member).unwrap()),
                sort: 0.0,
                label,
            };
            assert!(
                crate::sql::eval::compare_datums_collated(
                    &storage,
                    7,
                    crate::sql::ast::Collation::None,
                    &value(0, "before"),
                    &value(1, "after")
                )
                .unwrap()
                .is_lt()
            );
            let identified = value(0, "before");
            let legacy = Datum::Enum {
                slot: 0,
                member: None,
                sort: 0.0,
                label: "before",
            };
            assert_eq!(
                crate::sql::eval::hash_key_catalog(&storage, 7, &[legacy], &[0]).unwrap(),
                crate::sql::eval::hash_key_catalog(&storage, 7, &[identified], &[0]).unwrap()
            );
            drop(scope);
            storage
                .commit_enum_alter(storage.prepare_enum_edit(0, 7).unwrap().1)
                .unwrap();
            assert!(selected.label_is_uncommitted("after"));
            let scope = statement.enter().unwrap();
            let child = storage.spill_roots.retain_statement().unwrap();
            let child_scope = child.enter().unwrap();
            assert!(
                !storage
                    .read_enum_definition(target, 0)
                    .unwrap()
                    .unwrap()
                    .label_is_uncommitted("after")
            );
            drop(child_scope);
            drop(scope);
        });
    }

    #[test]
    fn exhausted_statement_type_images_release_cleanly_and_retry_without_more_memory() {
        let mut storage = fixture(1);
        create(&mut storage, NamedTypeSlot::Composite(0)).unwrap();
        let target = TypeDefinitionTarget::Slot(NamedTypeSlot::Composite(0));
        let outer = storage.spill_roots.retain_statement().unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let scope = outer.enter().unwrap();
            let old = storage
                .read_composite_definition(target, 0)
                .unwrap()
                .unwrap();
            drop(scope);
            for (name, succeeds) in [("second", true), ("third", false)] {
                let mut altered = storage.composite_for(0, 0);
                altered.fields[0].name = SqlName::parse(name).unwrap();
                storage
                    .stage_composite_alter(
                        storage.prepare_composite_edit(0, 7).unwrap().1,
                        altered,
                        7,
                    )
                    .unwrap();
                storage
                    .commit_composite_alter(storage.prepare_composite_edit(0, 7).unwrap().1)
                    .unwrap();
                let scope = outer.enter().unwrap();
                let child = storage.spill_roots.retain_statement().unwrap();
                let child_scope = child.enter().unwrap();
                let read = storage.read_composite_definition(target, 0);
                if succeeds {
                    assert_eq!(read.unwrap().unwrap().fields()[0].name.as_str(), name);
                } else {
                    assert_eq!(
                        read.err().unwrap().sqlstate,
                        sqlstate::PROGRAM_LIMIT_EXCEEDED
                    );
                }
                drop(child_scope);
                drop(scope);
            }
            assert_eq!(old.fields()[0].name.as_str(), "before");
            drop(old);
            drop(outer);
            assert_eq!(
                storage
                    .read_composite_definition(target, 0)
                    .unwrap()
                    .unwrap()
                    .fields()[0]
                    .name
                    .as_str(),
                "third"
            );
        });
    }

    #[test]
    fn type_graph_capture_retains_nested_sources_before_any_value_is_decoded() {
        let mut storage = fixture(1);
        let schema = SqlName::parse("public").unwrap();
        create(&mut storage, NamedTypeSlot::Enum(0)).unwrap();
        storage
            .create_domain(
                schema,
                SqlName::parse("wrapped_reader").unwrap(),
                DomainSpec {
                    base_domain: None,
                    base_user_type: Some(super::super::UserTypeName::declaration(
                        schema,
                        SqlName::parse("reader_source").unwrap(),
                    )),
                    base: ColType::Enum(0),
                    base_type_mod: -1,
                    not_null: false,
                    not_null_name: None,
                    default_expr: None,
                    checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                    n_checks: 0,
                },
                0,
            )
            .unwrap()
            .slot();
        let mut fields = [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS];
        fields[0] = CompositeFieldDef {
            name: SqlName::parse("status").unwrap(),
            attribute_number: 1,
            ctype: ColType::Enum(0),
            dropped: false,
            user_type: Some(super::super::UserTypeName::declaration(
                schema,
                SqlName::parse("wrapped_reader").unwrap(),
            )),
            ..CompositeFieldDef::EMPTY
        };
        storage
            .create_composite(
                schema,
                SqlName::parse("record_reader").unwrap(),
                CompositeSpec {
                    fields,
                    n_fields: 1,
                },
                0,
            )
            .unwrap()
            .slot();
        let bound = storage
            .type_catalog
            .lock()
            .unwrap()
            .type_snapshot(NamedTypeSlot::Composite(0), 0)
            .unwrap()
            .unwrap()
            .name;
        let enumeration = reference(&storage, NamedTypeSlot::Enum(0));
        let mut budget = Budget::new(1025);
        let tiny = crate::mem::arena::Arena::new(&mut budget, "tiny type graph", 1).unwrap();
        let arena = crate::mem::arena::Arena::new(&mut budget, "type graph", 1024).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                storage
                    .select_type_definition_graph(core::iter::once(bound), 0, &tiny)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let graph = storage
                .select_type_definition_graph(core::iter::once(bound), 0, &arena)
                .unwrap()
                .unwrap();
            retire(&mut storage, NamedTypeSlot::Enum(0));
            assert_eq!(
                create(&mut storage, NamedTypeSlot::Enum(0))
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            let scope = graph.enter().unwrap();
            let selected = storage
                .read_enum_definition(TypeDefinitionTarget::Bound(enumeration), 0)
                .unwrap()
                .unwrap();
            assert_eq!(selected.members().next().unwrap().label.as_str(), "before");
            drop(scope);
            drop(graph);
            assert_eq!(selected.members().next().unwrap().label.as_str(), "before");
            drop(selected);
            create(&mut storage, NamedTypeSlot::Enum(0)).unwrap();
        });
    }

    #[test]
    fn restored_type_identity_rejects_duplicate_births_without_leaking_images() {
        let mut storage = fixture(1);
        let schema = SqlName::parse("public").unwrap();
        let name = SqlName::parse("original_domain").unwrap();
        let enumeration = SqlName::parse("restored_enum").unwrap();
        let spec = DomainSpec {
            base_domain: None,
            base_user_type: None,
            base: ColType::Int4,
            base_type_mod: -1,
            not_null: false,
            not_null_name: None,
            default_expr: None,
            checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
            n_checks: 0,
        };
        crate::mem::guard::forbid_alloc(|| {
            storage
                .create_domain_from_manifest(schema, name, spec, 41)
                .unwrap();
            let original = reference(&storage, NamedTypeSlot::Domain(0));
            let version = storage
                .type_catalog
                .lock()
                .unwrap()
                .versions
                .get(NamedTypeSlot::Domain(0));
            storage.enum_replay_members(1).unwrap()[0] = EnumMember {
                identity: super::super::EnumLabelIdentity::parse(43),
                label: SqlName::parse("saved").unwrap(),
                legacy_label: None,
                sort: 1.0,
            };
            for _ in 0..32 {
                assert_eq!(
                    storage
                        .finish_enum_replay(schema, enumeration, 1, 41)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.catalog_identity_sequence(), 41);
                assert_eq!(reference(&storage, NamedTypeSlot::Domain(0)), original);
                let catalog = storage.type_catalog.lock().unwrap();
                assert_eq!(catalog.versions.get(NamedTypeSlot::Domain(0)), version);
                assert_eq!(
                    catalog.enums[0].ddl_state,
                    super::super::CatalogDdlState::Absent
                );
                assert_eq!(
                    catalog
                        .identities
                        .lookup(super::super::current_database(), 41),
                    Some(NamedTypeSlot::Domain(0))
                );
            }
            assert_eq!(
                storage
                    .finish_enum_replay(schema, enumeration, 1, 42)
                    .unwrap(),
                0
            );
            assert_eq!(storage.enum_for(0, 0).sort_of("saved"), Some(1.0));
            assert_eq!(storage.catalog_identity_sequence(), 43);
            let catalog = storage.type_catalog.lock().unwrap();
            assert_eq!(
                catalog
                    .identities
                    .lookup(super::super::current_database(), 41),
                Some(NamedTypeSlot::Domain(0))
            );
            assert_eq!(
                catalog
                    .identities
                    .lookup(super::super::current_database(), 42),
                Some(NamedTypeSlot::Enum(0))
            );
        });
    }

    #[test]
    fn type_creation_range_exhaustion_and_invalid_restore_preserve_existing_sources() {
        let maximum = super::super::type_identities::MAX_TYPE_CREATION;
        for source in SOURCES {
            let mut storage = fixture(1);
            create(&mut storage, source).unwrap();
            let original = reference(&storage, source);
            storage.restore_catalog_identity_sequence(maximum);
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    create(&mut storage, source).unwrap_err().sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_eq!(reference(&storage, source), original);
                assert_eq!(storage.catalog_identity_sequence(), maximum);
                for rejected in [0, maximum + 1, u64::MAX] {
                    assert_eq!(
                        storage
                            .restore_type_identity(source, source, rejected)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                    assert_eq!(reference(&storage, source), original);
                    assert_eq!(storage.catalog_identity_sequence(), maximum);
                }
                storage
                    .restore_type_identity(source, source, maximum)
                    .unwrap();
                let captured = reference(&storage, source);
                assert_ne!(captured, original);
                let catalog = storage.type_catalog.lock().unwrap();
                let selected = catalog.type_snapshot(source, 0).unwrap().unwrap();
                assert_eq!(selected.identity.created_at(), maximum);
                assert_eq!(
                    catalog
                        .identities
                        .lookup(selected.identity.database(), maximum),
                    Some(source)
                );
                assert!(
                    catalog
                        .resolve_source_reference(original, selected.identity.database(), 0)
                        .unwrap()
                        .is_none()
                );
                assert!(
                    catalog
                        .resolve_source_reference(captured, selected.identity.database(), 0)
                        .unwrap()
                        .is_some()
                );
            });
        }
    }

    #[test]
    fn type_recovery_installs_saved_birth_when_the_catalog_sequence_is_exhausted() {
        for manifest in [false, true] {
            for source in SOURCES {
                let mut storage = fixture(1);
                let schema = SqlName::parse("public").unwrap();
                let name = SqlName::parse("reader_source").unwrap();
                storage.restore_catalog_identity_sequence(u64::MAX);
                let created_at = 41;
                crate::mem::guard::forbid_alloc(|| {
                    let slot = match source {
                        NamedTypeSlot::Domain(_) => {
                            let spec = DomainSpec {
                                base_domain: None,
                                base_user_type: None,
                                base: ColType::Int4,
                                base_type_mod: -1,
                                not_null: false,
                                not_null_name: None,
                                default_expr: None,
                                checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                                n_checks: 0,
                            };
                            if manifest {
                                storage.create_domain_from_manifest(schema, name, spec, created_at)
                            } else {
                                storage.create_domain_from_journal(schema, name, spec, created_at)
                            }
                            .unwrap()
                        }
                        NamedTypeSlot::Enum(_) => {
                            storage.enum_replay_members(1).unwrap()[0] = EnumMember {
                                identity: super::super::EnumLabelIdentity::parse(42),
                                label: SqlName::parse("before").unwrap(),
                                legacy_label: None,
                                sort: 1.0,
                            };
                            storage
                                .finish_enum_replay(schema, name, 1, created_at)
                                .unwrap()
                        }
                        NamedTypeSlot::Composite(_) => {
                            let mut fields = [CompositeFieldDef::EMPTY; MAX_COMPOSITE_FIELDS];
                            fields[0] = CompositeFieldDef {
                                name: SqlName::parse("before").unwrap(),
                                attribute_number: 1,
                                ctype: ColType::Int4,
                                ..CompositeFieldDef::EMPTY
                            };
                            let spec = CompositeSpec {
                                fields,
                                n_fields: 1,
                            };
                            if manifest {
                                storage
                                    .create_composite_from_manifest(schema, name, spec, created_at)
                            } else {
                                storage.create_composite_at(0, schema, name, spec, created_at)
                            }
                            .unwrap()
                        }
                    };
                    assert_eq!(slot, source.index());
                    let captured = reference(&storage, source);
                    let catalog = storage.type_catalog.lock().unwrap();
                    let selected = catalog.type_snapshot(source, 0).unwrap().unwrap();
                    assert_eq!(selected.identity.created_at(), created_at);
                    assert_eq!(
                        catalog
                            .identities
                            .lookup(selected.identity.database(), created_at),
                        Some(source)
                    );
                    assert!(catalog.versions.get(source) >= created_at);
                    assert!(
                        catalog
                            .resolve_source_reference(captured, selected.identity.database(), 0)
                            .unwrap()
                            .is_some()
                    );
                    assert_eq!(storage.catalog_identity_sequence(), u64::MAX);
                    drop(catalog);
                    assert_eq!(
                        create(&mut storage, source).unwrap_err().sqlstate,
                        sqlstate::PROGRAM_LIMIT_EXCEEDED
                    );
                    assert_eq!(reference(&storage, source), captured);
                    assert_eq!(storage.catalog_identity_sequence(), u64::MAX);
                });
            }
        }
    }

    #[test]
    fn restored_type_identity_rejects_unpublished_retired_and_mismatched_sources_atomically() {
        for source in SOURCES {
            let mut storage = fixture(1);
            create(&mut storage, source).unwrap();
            let original = reference(&storage, source);
            let (database, created_at, version) = {
                let catalog = storage.type_catalog.lock().unwrap();
                let selected = catalog.type_snapshot(source, 0).unwrap().unwrap();
                (
                    selected.identity.database(),
                    selected.identity.created_at(),
                    catalog.versions.get(source),
                )
            };
            let sequence = storage.catalog_identity_sequence();
            crate::mem::guard::forbid_alloc(|| {
                let bad_family = match source {
                    NamedTypeSlot::Domain(_) => NamedTypeSlot::Enum(0),
                    _ => NamedTypeSlot::Domain(0),
                };
                let out_of_bounds = match source {
                    NamedTypeSlot::Domain(_) => NamedTypeSlot::Domain(1),
                    NamedTypeSlot::Enum(_) => NamedTypeSlot::Enum(1),
                    NamedTypeSlot::Composite(_) => NamedTypeSlot::Composite(1),
                };
                for (target, stamp) in [
                    (source, 0),
                    (bad_family, created_at),
                    (out_of_bounds, created_at),
                ] {
                    assert_eq!(
                        storage
                            .restore_type_identity(source, target, stamp)
                            .unwrap_err()
                            .sqlstate,
                        sqlstate::DATA_EXCEPTION
                    );
                    assert_eq!(reference(&storage, source), original);
                    let catalog = storage.type_catalog.lock().unwrap();
                    assert_eq!(
                        catalog.identities.lookup(database, created_at),
                        Some(source)
                    );
                    assert_eq!(catalog.versions.get(source), version);
                    assert_eq!(storage.catalog_identity_sequence(), sequence);
                }
                storage
                    .select_database_for_recovery(super::super::DatabaseOid::TEMPLATE1)
                    .unwrap();
                assert_eq!(
                    storage
                        .restore_type_identity(source, source, created_at + 100)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                storage.select_database_for_recovery(database).unwrap();
                assert_eq!(reference(&storage, source), original);
                {
                    let mut catalog = storage.type_catalog.lock().unwrap();
                    match source {
                        NamedTypeSlot::Domain(slot) => {
                            catalog.domains[slot].ddl_state =
                                super::super::CatalogDdlState::PendingCreate { txid: 77 }
                        }
                        NamedTypeSlot::Enum(slot) => {
                            catalog.enums[slot].ddl_state =
                                super::super::CatalogDdlState::PendingCreate { txid: 77 }
                        }
                        NamedTypeSlot::Composite(slot) => {
                            catalog.composites[slot].ddl_state =
                                super::super::CatalogDdlState::PendingCreate { txid: 77 }
                        }
                    }
                }
                assert_eq!(
                    storage
                        .restore_type_identity(source, source, created_at + 100)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                {
                    let mut catalog = storage.type_catalog.lock().unwrap();
                    match source {
                        NamedTypeSlot::Domain(slot) => {
                            catalog.domains[slot].ddl_state = super::super::CatalogDdlState::Present
                        }
                        NamedTypeSlot::Enum(slot) => {
                            catalog.enums[slot].ddl_state = super::super::CatalogDdlState::Present
                        }
                        NamedTypeSlot::Composite(slot) => {
                            catalog.composites[slot].ddl_state =
                                super::super::CatalogDdlState::Present
                        }
                    }
                }
                assert_eq!(reference(&storage, source), original);
                retire(&mut storage, source);
                assert_eq!(
                    storage
                        .restore_type_identity(source, source, created_at + 100)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                let catalog = storage.type_catalog.lock().unwrap();
                assert_eq!(
                    catalog.identities.lookup(database, created_at),
                    Some(source)
                );
                assert_eq!(catalog.identities.lookup(database, created_at + 100), None);
                assert_eq!(storage.catalog_identity_sequence(), sequence);
            });
        }
    }

    #[test]
    fn retained_type_identity_allows_exact_replay_and_rejects_reassignment() {
        for source in SOURCES {
            let mut storage = fixture(1);
            create(&mut storage, source).unwrap();
            let reference = reference(&storage, source);
            let created_at = storage
                .type_catalog
                .lock()
                .unwrap()
                .type_snapshot(source, 0)
                .unwrap()
                .unwrap()
                .identity
                .created_at();
            let images = storage.type_definition_images();
            crate::mem::guard::forbid_alloc(|| {
                retain(&images, &storage, reference).unwrap();
                storage
                    .restore_type_identity(source, source, created_at)
                    .unwrap();
                assert_eq!(
                    storage
                        .restore_type_identity(source, source, created_at + 1)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::OBJECT_IN_USE
                );
                retain(&images, &storage, reference).unwrap();
                drop(images);
                storage
                    .restore_type_identity(source, source, created_at + 1)
                    .unwrap();
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
    fn selected_pending_type_definitions_survive_rollback_without_changing_new_readers() {
        for source in SOURCES {
            let mut storage = fixture(1);
            create(&mut storage, source).unwrap();
            let outer = storage.spill_roots.retain_statement().unwrap();
            let target = TypeDefinitionTarget::Slot(source);
            crate::mem::guard::forbid_alloc(|| {
                let scope = outer.enter().unwrap();
                match source {
                    NamedTypeSlot::Domain(_) => {
                        let prior = storage
                            .stage_domain_alter(
                                storage.prepare_domain_edit(0, 7).unwrap().1,
                                DomainSpec {
                                    base_domain: None,
                                    base_user_type: None,
                                    base: ColType::Int8,
                                    base_type_mod: -1,
                                    not_null: false,
                                    not_null_name: None,
                                    default_expr: None,
                                    checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                                    n_checks: 0,
                                },
                                7,
                            )
                            .unwrap();
                        let selected = storage.read_domain_definition(target, 7).unwrap().unwrap();
                        storage.rollback_domain_alter(prior);
                        assert_eq!(selected.base, ColType::Int8);
                        assert!(!selected.not_null);
                        assert!(selected.pending_definition.is_none());
                        let child = storage.spill_roots.retain_statement().unwrap();
                        let child_scope = child.enter().unwrap();
                        let current = storage.read_domain_definition(target, 7).unwrap().unwrap();
                        assert_eq!(current.base, ColType::Int4);
                        assert!(current.not_null);
                        drop(child_scope);
                    }
                    NamedTypeSlot::Enum(_) => {
                        let undo = storage
                            .stage_enum_alter(
                                storage.prepare_enum_edit(0, 7).unwrap().1,
                                SqlName::parse("public").unwrap(),
                                SqlName::parse("reader_source").unwrap(),
                                &[EnumMember {
                                    identity: None,
                                    label: SqlName::parse("pending").unwrap(),
                                    legacy_label: None,
                                    sort: 1.0,
                                }],
                                7,
                            )
                            .unwrap();
                        let selected = storage.read_enum_definition(target, 7).unwrap().unwrap();
                        storage.rollback_enum_alter(undo);
                        assert_eq!(selected.members().next().unwrap().label.as_str(), "pending");
                        let child = storage.spill_roots.retain_statement().unwrap();
                        let child_scope = child.enter().unwrap();
                        assert_eq!(
                            storage
                                .read_enum_definition(target, 7)
                                .unwrap()
                                .unwrap()
                                .members()
                                .next()
                                .unwrap()
                                .label
                                .as_str(),
                            "before"
                        );
                        drop(child_scope);
                    }
                    NamedTypeSlot::Composite(_) => {
                        let mut definition = storage.composite_for(0, 0);
                        definition.fields[0].name = SqlName::parse("pending").unwrap();
                        let prior = storage
                            .stage_composite_alter(
                                storage.prepare_composite_edit(0, 7).unwrap().1,
                                definition,
                                7,
                            )
                            .unwrap();
                        let selected = storage
                            .read_composite_definition(target, 7)
                            .unwrap()
                            .unwrap();
                        storage.rollback_composite_alter(prior);
                        assert_eq!(selected.fields()[0].name.as_str(), "pending");
                        assert!(selected.pending_definition.is_none());
                        let child = storage.spill_roots.retain_statement().unwrap();
                        let child_scope = child.enter().unwrap();
                        assert_eq!(
                            storage
                                .read_composite_definition(target, 7)
                                .unwrap()
                                .unwrap()
                                .fields()[0]
                                .name
                                .as_str(),
                            "before"
                        );
                        drop(child_scope);
                    }
                }
                drop(scope);
            });
        }
    }

    fn check_type_undo_lifetimes<Undo: Copy>(
        source: NamedTypeSlot,
        select: impl Fn(&Storage, u32) -> super::super::TypeEditTarget,
        stage: impl Fn(&Storage, super::super::TypeEditTarget, &str) -> Result<Undo, SqlError>,
        commit: impl Fn(&mut Storage, Undo),
        rollback: impl Fn(&Storage, Undo),
        label: impl Fn(&Storage) -> SqlName,
    ) {
        let mut first = fixture(2);
        let mut other = fixture(2);
        create(&mut first, source).unwrap();
        create(&mut other, source).unwrap();
        other.begin_transaction_identity(7).unwrap();
        other.begin_transaction_identity(9).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let original = select(&first, 7);
            let catalog_sequence = first.catalog_identity_sequence();
            assert_eq!(
                stage(&first, select(&first, 0), "11")
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let earlier = stage(&first, original, "11").unwrap();
            assert_eq!(
                stage(&first, original, "22").err().unwrap().sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            assert_eq!(
                stage(&other, original, "22").err().unwrap().sqlstate,
                sqlstate::UNDEFINED_OBJECT
            );
            let latest = stage(&first, select(&first, 7), "22").unwrap();
            assert_eq!(first.catalog_identity_sequence(), catalog_sequence);
            commit(&mut first, earlier);
            assert_eq!(label(&first).as_str(), "22");
            let foreign = stage(&other, select(&other, 7), "11").unwrap();
            commit(&mut other, earlier);
            assert_eq!(label(&other).as_str(), "11");
            assert_eq!(
                other
                    .stage_type_drop(source.kind(), "public", "reader_source", 9)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            assert!(other.type_catalog.try_lock().is_ok());
            rollback(&other, earlier);
            assert_eq!(label(&other).as_str(), "11");
            rollback(&other, foreign);
            other.wake_transaction_waiters(7);
            assert_eq!(label(&other).as_str(), "before");
            let dropped = other
                .stage_type_drop(source.kind(), "public", "reader_source", 9)
                .unwrap()
                .unwrap();
            assert_eq!(dropped.slot(), 0);
            assert_eq!(
                stage(&other, select(&other, 7), "22")
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::INTERNAL_LOCK_WAIT
            );
            assert!(other.type_catalog.try_lock().is_ok());
            other.finish_type_ddl(dropped, crate::storage::TypeDdlDecision::Rollback);
            assert_eq!(label(&other).as_str(), "before");

            rollback(&first, earlier);
            assert_eq!(label(&first).as_str(), "22");
            rollback(&first, latest);
            assert_eq!(label(&first).as_str(), "11");
            rollback(&first, earlier);
            assert_eq!(label(&first).as_str(), "before");

            retire(&mut first, source);
            create(&mut first, source).unwrap();
            assert_eq!(
                stage(&first, original, "22").err().unwrap().sqlstate,
                sqlstate::UNDEFINED_OBJECT
            );
            let replacement = stage(&first, select(&first, 7), "22").unwrap();
            commit(&mut first, earlier);
            rollback(&first, earlier);
            assert_eq!(label(&first).as_str(), "22");
            rollback(&first, replacement);
            assert_eq!(label(&first).as_str(), "before");
        });
    }

    #[test]
    fn type_edit_undo_rejects_superseded_replaced_and_foreign_sources() {
        check_type_undo_lifetimes(
            NamedTypeSlot::Domain(0),
            |storage, transaction| storage.prepare_domain_edit(0, transaction).unwrap().1,
            |storage, target, marker| {
                storage.stage_domain_alter(
                    target,
                    DomainSpec {
                        base_domain: None,
                        base_user_type: None,
                        base: ColType::Int4,
                        base_type_mod: -1,
                        not_null: false,
                        not_null_name: None,
                        default_expr: Some(crate::util::StackStr::from_str(marker)),
                        checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                        n_checks: 0,
                    },
                    7,
                )
            },
            |storage, undo| storage.commit_domain_alter(undo.target()).unwrap(),
            |storage, undo| storage.rollback_domain_alter(undo),
            |storage| match storage.domain_for(0, 7).default_expr {
                Some(expression) => SqlName::parse(expression.as_str()).unwrap(),
                None => SqlName::parse("before").unwrap(),
            },
        );
        check_type_undo_lifetimes(
            NamedTypeSlot::Enum(0),
            |storage, transaction| storage.prepare_enum_edit(0, transaction).unwrap().1,
            |storage, target, marker| {
                storage.stage_enum_alter(
                    target,
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("reader_source").unwrap(),
                    &[EnumMember {
                        identity: None,
                        label: SqlName::parse(marker).unwrap(),
                        legacy_label: None,
                        sort: 1.0,
                    }],
                    7,
                )
            },
            |storage, undo| storage.commit_enum_alter(undo.target()).unwrap(),
            |storage, undo| storage.rollback_enum_alter(undo),
            |storage| storage.enum_for(0, 7).members().next().unwrap().label,
        );
        check_type_undo_lifetimes(
            NamedTypeSlot::Composite(0),
            |storage, transaction| storage.prepare_composite_edit(0, transaction).unwrap().1,
            |storage, target, marker| {
                let mut definition = storage.composite_for(0, 7);
                definition.fields[0].name = SqlName::parse(marker).unwrap();
                storage.stage_composite_alter(target, definition, 7)
            },
            |storage, undo| storage.commit_composite_alter(undo.target()).unwrap(),
            |storage, undo| storage.rollback_composite_alter(undo),
            |storage| storage.composite_for(0, 7).fields()[0].name,
        );
    }
}
