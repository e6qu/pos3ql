//! Immutable checked type-input values follow the stored SQL source image.

use std::sync::Arc;

use crate::mem::{
    arena::Arena,
    lease_pool::{LeasePool, MutableSlice, SharedSlice},
};
use crate::sql::{
    eval::{SqlError, sqlstate},
    type_inputs::{self, Binding},
};
use crate::sql_err;

const MAGIC: &[u8; 4] = b"STI1";
const HEADER: usize = 8;
const SECTION: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum StoredInputSection {
    Query,
    RuleSource,
    PolicyUsing,
    PolicyCheck,
    RoutineBody,
    ParameterDefault(u16),
    TableDefault(u16),
    ViewDefault(u16),
    DomainDefault,
}

impl StoredInputSection {
    fn code(self) -> (u16, u16) {
        match self {
            Self::Query => (0, 0),
            Self::RuleSource => (1, 0),
            Self::PolicyUsing => (2, 0),
            Self::PolicyCheck => (3, 0),
            Self::RoutineBody => (4, 0),
            Self::ParameterDefault(index) => (5, index),
            Self::TableDefault(attribute) => (6, attribute),
            Self::ViewDefault(attribute) => (7, attribute),
            Self::DomainDefault => (8, 0),
        }
    }

    fn from_code(code: u16, index: u16) -> Option<Self> {
        match (code, index) {
            (0, 0) => Some(Self::Query),
            (1, 0) => Some(Self::RuleSource),
            (2, 0) => Some(Self::PolicyUsing),
            (3, 0) => Some(Self::PolicyCheck),
            (4, 0) => Some(Self::RoutineBody),
            (5, index) if usize::from(index) < super::MAX_ROUTINE_ARGUMENTS => {
                Some(Self::ParameterDefault(index))
            }
            (6, attribute)
                if attribute > 0 && usize::from(attribute) <= super::MAX_RELATION_COLUMNS =>
            {
                Some(Self::TableDefault(attribute))
            }
            (7, attribute)
                if attribute > 0 && usize::from(attribute) <= super::MAX_RELATION_COLUMNS =>
            {
                Some(Self::ViewDefault(attribute))
            }
            (8, 0) => Some(Self::DomainDefault),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StoredInputSource<'a> {
    pub section: StoredInputSection,
    pub sql: &'a str,
    pub bindings: &'a [Binding<'a>],
}

#[derive(Clone, Debug)]
pub struct StoredTypeInputs {
    bytes: SharedSlice<u8>,
}

pub(crate) static EMPTY_STORED_TYPE_INPUTS: StoredTypeInputs = StoredTypeInputs::EMPTY;

pub(super) enum SchemaInputRename {
    Query {
        image: usize,
        inputs: StoredTypeInputs,
    },
    Domain {
        slot: usize,
        pending: bool,
        inputs: StoredTypeInputs,
    },
}

impl AsRef<StoredTypeInputs> for StoredTypeInputs {
    fn as_ref(&self) -> &StoredTypeInputs {
        self
    }
}

impl StoredTypeInputs {
    pub(crate) const EMPTY: Self = Self {
        bytes: SharedSlice::EMPTY,
    };

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn references_type(&self, kind: super::NamedTypeKind, creation: u64) -> bool {
        let mut found = false;
        self.visit(|_, input| {
            found |= matches!(super::NamedTypeKind::from_oid(input.type_oid), Some((input_kind, input_creation, _))
                if input_kind == kind && input_creation.created_at() == creation);
            Ok(())
        }).expect("immutable type input images are validated before publication");
        found
    }

    pub(crate) fn validate_encoded(bytes: &[u8]) -> Result<(), SqlError> {
        visit_sections(bytes, |_, _| Ok(()))
    }

    fn decode(bytes: &[u8], pool: &Arc<LeasePool>) -> Result<Self, SqlError> {
        Self::validate_encoded(bytes)?;
        if bytes.is_empty() {
            return Ok(Self::EMPTY);
        }
        let mut owned = pool.lease::<u8>(bytes.len()).map_err(metadata_full)?;
        for byte in bytes {
            owned.push_reserved(*byte).map_err(metadata_full)?;
        }
        Ok(Self {
            bytes: owned.freeze(),
        })
    }

    pub(crate) fn detached_bindings<'a>(
        &self,
        section: StoredInputSection,
        arena: &'a Arena,
    ) -> Result<&'a [Binding<'a>], SqlError> {
        let mut selected = &[][..];
        visit_sections(self.bytes(), |found, bytes| {
            if found == section {
                selected = bytes;
            }
            Ok(())
        })?;
        let bytes = arena
            .alloc_slice_copy(selected)
            .map_err(|_| crate::sql::query::arena_full_pub())?;
        type_inputs::decode(bytes, arena)
    }

    pub(crate) fn detached_sections<'a>(
        &self,
        arena: &'a Arena,
    ) -> Result<&'a [(StoredInputSection, &'a [Binding<'a>])], SqlError> {
        let bytes = arena
            .alloc_slice_copy(self.bytes())
            .map_err(|_| crate::sql::query::arena_full_pub())?;
        decode_sections(bytes, arena)
    }

    pub(crate) fn borrowed_sections<'a>(
        &'a self,
        arena: &'a Arena,
    ) -> Result<&'a [(StoredInputSection, &'a [Binding<'a>])], SqlError> {
        decode_sections(self.bytes(), arena)
    }

    pub(super) fn relocated_sources(
        &self,
        sources: &[StoredInputSource<'_>],
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        if self.matches_sources(sources)? {
            return Ok(self.clone());
        }
        let mut found = 0;
        visit_sections(self.bytes(), |section, map| {
            let source = sources
                .iter()
                .find(|source| source.section == section)
                .ok_or_else(invalid)?;
            let mut index = 0;
            type_inputs::visit(map, |binding| {
                let next = source.bindings.get(index).ok_or_else(invalid)?;
                if (
                    binding.kind,
                    binding.type_oid,
                    binding.type_mod,
                    binding.collation,
                ) != (next.kind, next.type_oid, next.type_mod, next.collation)
                    || binding.encoded != next.encoded
                {
                    return Err(invalid());
                }
                index += 1;
                Ok(())
            })?;
            if index != source.bindings.len() {
                return Err(invalid());
            }
            found += 1;
            Ok(())
        })?;
        if found
            != sources
                .iter()
                .filter(|source| !source.bindings.is_empty())
                .count()
        {
            return Err(invalid());
        }
        Self::from_validated_sources(sources, |_| Ok(()), pool)
    }

    pub(crate) fn matches_sources(
        &self,
        sources: &[StoredInputSource<'_>],
    ) -> Result<bool, SqlError> {
        let mut source_index = 0;
        let mut same = true;
        visit_sections(self.bytes(), |section, map| {
            while sources
                .get(source_index)
                .is_some_and(|source| source.bindings.is_empty())
            {
                source_index += 1;
            }
            let Some(source) = sources.get(source_index) else {
                same = false;
                return Ok(());
            };
            same &= source.section == section;
            let mut index = 0;
            type_inputs::visit(map, |binding| {
                same &= source.bindings.get(index).is_some_and(|other| {
                    (
                        binding.start,
                        binding.end,
                        binding.kind,
                        binding.type_oid,
                        binding.type_mod,
                        binding.collation,
                    ) == (
                        other.start,
                        other.end,
                        other.kind,
                        other.type_oid,
                        other.type_mod,
                        other.collation,
                    ) && binding.encoded == other.encoded
                });
                index += 1;
                Ok(())
            })?;
            same &= index == source.bindings.len();
            source_index += 1;
            Ok(())
        })?;
        while sources
            .get(source_index)
            .is_some_and(|source| source.bindings.is_empty())
        {
            source_index += 1;
        }
        Ok(same && source_index == sources.len())
    }

    pub(crate) fn from_sources(
        sources: &[StoredInputSource<'_>],
        dependencies: super::StoredQueryDependencyView<'_>,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        Self::from_validated_sources(
            sources,
            |binding| validate_target(binding, dependencies),
            pool,
        )
    }

    fn from_validated_sources(
        sources: &[StoredInputSource<'_>],
        mut validate: impl FnMut(Binding<'_>) -> Result<(), SqlError>,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        let mut length = HEADER;
        let mut count = 0u32;
        let mut previous = None;
        for source in sources {
            if StoredInputSection::from_code(source.section.code().0, source.section.code().1)
                .is_none()
                || previous.is_some_and(|prior| prior >= source.section)
            {
                return Err(invalid());
            }
            previous = Some(source.section);
            type_inputs::encoded_len(source.bindings)?;
            for binding in source.bindings {
                validate_span(source.sql, *binding)?;
                validate(*binding)?;
            }
            if source.bindings.is_empty() {
                continue;
            }
            count = count.checked_add(1).ok_or_else(invalid)?;
            let map_length = type_inputs::encoded_len(source.bindings)?;
            length = length
                .checked_add(SECTION)
                .and_then(|length| length.checked_add(map_length))
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "stored SQL type inputs exceed the metadata byte capacity"
                    )
                })?;
        }
        if count == 0 {
            return Ok(Self::EMPTY);
        }
        let mut bytes = pool.lease::<u8>(length).map_err(metadata_full)?;
        for _ in 0..length {
            bytes.push_reserved(0).map_err(metadata_full)?;
        }
        let output = bytes.as_mut_slice();
        output[..4].copy_from_slice(MAGIC);
        output[4..8].copy_from_slice(&count.to_le_bytes());
        let mut at = HEADER;
        for source in sources.iter().filter(|source| !source.bindings.is_empty()) {
            let length = type_inputs::encoded_len(source.bindings)?;
            let (code, index) = source.section.code();
            output[at..at + 2].copy_from_slice(&code.to_le_bytes());
            output[at + 2..at + 4].copy_from_slice(&index.to_le_bytes());
            output[at + 4..at + 12].copy_from_slice(&(length as u64).to_le_bytes());
            at += SECTION;
            type_inputs::encode_into(source.bindings, &mut output[at..at + length])?;
            at += length;
        }
        Ok(Self {
            bytes: bytes.freeze(),
        })
    }

    /// Restores checked values without invoking their type input or CHECK expressions.
    pub(crate) fn restore<'source>(
        bytes: &[u8],
        mut source: impl FnMut(StoredInputSection) -> Option<&'source str>,
        dependencies: super::StoredQueryDependencyView<'_>,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        Self::restore_validated(
            bytes,
            &mut source,
            |binding| validate_target(binding, dependencies),
            pool,
        )
    }

    fn restore_validated<'source>(
        bytes: &[u8],
        mut source: impl FnMut(StoredInputSection) -> Option<&'source str>,
        mut validate: impl FnMut(Binding<'_>) -> Result<(), SqlError>,
        pool: &Arc<LeasePool>,
    ) -> Result<Self, SqlError> {
        visit_sections(bytes, |section, map| {
            let sql = source(section).ok_or_else(invalid)?;
            let sql = if section == StoredInputSection::Query {
                crate::sql::enum_values::query_binding_source(sql)?
            } else {
                sql
            };
            type_inputs::visit(map, |binding| {
                validate_span(sql, binding)?;
                validate(binding)
            })
        })?;
        Self::decode(bytes, pool)
    }

    pub(crate) fn visit(
        &self,
        mut visit: impl FnMut(StoredInputSection, Binding<'_>) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        visit_sections(self.bytes(), |section, map| {
            type_inputs::visit(map, |binding| visit(section, binding))
        })
    }
}

impl PartialEq for StoredTypeInputs {
    fn eq(&self, other: &Self) -> bool {
        self.bytes() == other.bytes()
    }
}
impl Eq for StoredTypeInputs {}

fn decode_sections<'a>(
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<&'a [(StoredInputSection, &'a [Binding<'a>])], SqlError> {
    StoredTypeInputs::validate_encoded(bytes)?;
    if bytes.is_empty() {
        return Ok(&[]);
    }
    let count =
        u32::from_le_bytes(bytes[4..8].try_into().expect("validated section header")) as usize;
    let output = arena
        .alloc_slice_with(count, |_| (StoredInputSection::Query, &[][..]))
        .map_err(|_| crate::sql::query::arena_full_pub())?;
    let mut index = 0;
    visit_sections(bytes, |section, map| {
        output[index] = (section, type_inputs::decode(map, arena)?);
        index += 1;
        Ok(())
    })?;
    Ok(output)
}

fn visit_sections<'a>(
    bytes: &'a [u8],
    mut visit: impl FnMut(StoredInputSection, &'a [u8]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if bytes.is_empty() {
        return Ok(());
    }
    if bytes.len() < HEADER || bytes.get(..4) != Some(MAGIC.as_slice()) {
        return Err(invalid());
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().expect("checked header")) as usize;
    if count == 0 || count > (bytes.len() - HEADER) / SECTION {
        return Err(invalid());
    }
    let mut at = HEADER;
    let mut previous = None;
    for _ in 0..count {
        let record = bytes.get(at..at + SECTION).ok_or_else(invalid)?;
        let section = StoredInputSection::from_code(
            u16::from_le_bytes(record[..2].try_into().expect("checked section")),
            u16::from_le_bytes(record[2..4].try_into().expect("checked section")),
        )
        .ok_or_else(invalid)?;
        if previous.is_some_and(|prior| prior >= section) {
            return Err(invalid());
        }
        previous = Some(section);
        let length = usize::try_from(u64::from_le_bytes(
            record[4..12].try_into().expect("checked section"),
        ))
        .map_err(|_| invalid())?;
        at += SECTION;
        let end = at.checked_add(length).ok_or_else(invalid)?;
        let map = bytes.get(at..end).ok_or_else(invalid)?;
        if map.is_empty() {
            return Err(invalid());
        }
        type_inputs::visit(map, |_| Ok(()))?;
        visit(section, map)?;
        at = end;
    }
    if at != bytes.len() {
        return Err(invalid());
    }
    Ok(())
}

fn validate_span(source: &str, binding: Binding<'_>) -> Result<(), SqlError> {
    if binding.start >= binding.end
        || binding.end > source.len()
        || !source.is_char_boundary(binding.start)
        || !source.is_char_boundary(binding.end)
    {
        return Err(invalid());
    }
    Ok(())
}

fn validate_target(
    binding: Binding<'_>,
    dependencies: super::StoredQueryDependencyView<'_>,
) -> Result<(), SqlError> {
    if let Some((kind, creation, _)) = super::NamedTypeKind::from_oid(binding.type_oid) {
        if !dependencies.entries().iter().any(|dependency| {
            matches!(dependency.identity, super::StoredDependencyIdentity::Type(reference)
                if reference.kind() == kind && reference.creation() == creation)
        }) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "stored SQL type input has no matching captured type dependency"
            ));
        }
    } else if crate::sql::types::ColType::from_oid(binding.type_oid).is_none() {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "invalid stored SQL type-input image"
    )
}

fn metadata_full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "stored SQL type inputs: {}",
        error
    )
}

fn validate_representation(
    binding: Binding<'_>,
    target: crate::sql::types::ColType,
) -> Result<(), SqlError> {
    if binding.encoded[2] == 200 {
        let descriptor = super::CachedArrayElement::from_bytes(
            binding.encoded[3..9]
                .try_into()
                .expect("validated array descriptor"),
        )
        .ok_or_else(invalid)?;
        if target != crate::sql::types::ColType::Array(descriptor.element()) {
            return Err(invalid());
        }
    }
    Ok(())
}

fn validate_definition_inputs<'source>(
    inputs: &StoredTypeInputs,
    mut source: impl FnMut(StoredInputSection) -> Option<&'source str>,
    txid: u32,
    catalog: Option<&super::TypeCatalog>,
) -> Result<(), SqlError> {
    inputs.visit(|section, binding| {
        let source = source(section).ok_or_else(invalid)?;
        crate::sql::enum_values::checks::with_default_display(source, |display| {
            validate_span(display, binding)
        })?;
        let catalog = catalog.ok_or_else(invalid)?;
        let target = super::Storage::routine_result_for_oid_in(catalog, binding.type_oid, txid)
            .ok_or_else(invalid)?;
        validate_representation(binding, target.ctype)
    })
}

impl super::Storage {
    fn validate_template_input_oid(
        &self,
        oid: i32,
        source: super::DatabaseOid,
        transaction: u32,
    ) -> Result<Option<(super::NamedTypeSlot, super::NamedTypeSlot)>, SqlError> {
        let Some((kind, creation, array)) = super::NamedTypeKind::from_oid(oid) else {
            return Ok(None);
        };
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        let source_ref = catalog
            .source_reference_by_creation(kind, creation, source)
            .ok_or_else(invalid)?;
        let source_type = catalog
            .resolve_source_reference(source_ref, source, 0)?
            .ok_or_else(invalid)?;
        let target_database = super::current_database();
        let target_ref = catalog
            .source_reference_by_creation(kind, creation, target_database)
            .ok_or_else(invalid)?;
        let target_type = catalog
            .resolve_source_reference(target_ref, target_database, transaction)?
            .ok_or_else(invalid)?;
        // Database cloning preserves creation numbers and SQL OIDs, while
        // selecting fresh catalog slots. Verify both qualified maps before
        // sharing a cached value whose bytes deliberately contain no slots.
        if source_type.oid(array) != oid
            || target_type.oid(array) != oid
            || source_type.base != target_type.base
        {
            return Err(invalid());
        }
        Ok(Some((
            source_type.identity.slot(),
            target_type.identity.slot(),
        )))
    }

    fn validate_template_input_value(
        &self,
        value: crate::sql::types::Datum<'_>,
        source: super::DatabaseOid,
        transaction: u32,
    ) -> Result<(), SqlError> {
        use crate::sql::types::{Datum, oid};
        match value {
            Datum::Enum { slot, member, .. } => {
                let Some((super::NamedTypeSlot::Enum(before), super::NamedTypeSlot::Enum(after))) =
                    self.validate_template_input_oid(oid::enum_oid(slot), source, transaction)?
                else {
                    return Err(invalid());
                };
                let member = member.ok_or_else(invalid)?;
                let original = self
                    .enum_for(before, 0)
                    .members()
                    .nth(member.ordinal() as usize)
                    .ok_or_else(invalid)?;
                let copied = self
                    .enum_for(after, transaction)
                    .members()
                    .nth(member.ordinal() as usize)
                    .ok_or_else(invalid)?;
                if original.identity.is_none() || original.identity != copied.identity {
                    return Err(invalid());
                }
            }
            Datum::CompositeText { slot, .. } => {
                self.validate_template_input_oid(oid::composite_oid(slot), source, transaction)?;
            }
            Datum::Composite { slot, fields } => {
                self.validate_template_input_oid(oid::composite_oid(slot), source, transaction)?;
                for field in fields {
                    self.validate_template_input_oid(field.type_oid, source, transaction)?;
                    self.validate_template_input_value(field.value, source, transaction)?;
                }
            }
            Datum::Record(fields) => {
                for field in fields {
                    self.validate_template_input_oid(field.type_oid, source, transaction)?;
                    self.validate_template_input_value(field.value, source, transaction)?;
                }
            }
            Datum::Array { element, raw } => {
                self.validate_template_input_oid(element.array_oid(), source, transaction)?;
                self.validate_template_input_oid(element.to_coltype().oid(), source, transaction)?;
                for value in crate::sql::array::elements(raw, element) {
                    self.validate_template_input_value(value, source, transaction)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(super) fn validate_template_type_inputs(
        &self,
        inputs: &StoredTypeInputs,
        source: super::DatabaseOid,
        transaction: u32,
    ) -> Result<(), SqlError> {
        inputs.visit(|_, binding| {
            self.validate_template_input_oid(binding.type_oid, source, transaction)?;
            let target = self
                .routine_result_for_oid(binding.type_oid, transaction)
                .ok_or_else(invalid)?;
            validate_representation(binding, target.ctype)?;
            if binding.encoded[2] == 0 {
                return Ok(());
            }
            let descriptor = super::CachedArrayElement::from_bytes(
                binding.encoded[3..9]
                    .try_into()
                    .expect("validated input descriptor"),
            )
            .ok_or_else(invalid)?;
            let element = descriptor.element();
            self.validate_template_input_oid(element.array_oid(), source, transaction)?;
            self.validate_template_input_oid(element.to_coltype().oid(), source, transaction)?;
            for payload in crate::sql::array::element_payloads(&binding.encoded[13..]) {
                let mut value = [crate::sql::types::Datum::Null];
                super::rowenc::decode(payload, &[element.to_coltype()], &mut value)
                    .map_err(|_| invalid())?;
                self.validate_template_input_value(value[0], source, transaction)?;
            }
            Ok(())
        })
    }

    fn prepare_schema_query_inputs<'source>(
        &self,
        image: usize,
        source: impl FnMut(
            StoredInputSection,
        ) -> Option<(
            &'source str,
            &'source str,
            crate::sql::parser::StoredSqlKind,
        )>,
        names: (super::SqlName, super::SqlName),
        arena: &Arena,
    ) -> Result<Option<StoredTypeInputs>, SqlError> {
        let mark = arena.mark();
        let result: Result<Option<StoredTypeInputs>, SqlError> = (|| {
            let mut dependencies = self.snapshot_dependency_image(image, arena)?;
            if dependencies.type_inputs().bytes().is_empty() {
                return Ok(None);
            }
            dependencies.rename_schema(names.0, names.1);
            let inputs = self.relocate_stored_type_inputs(
                dependencies.type_inputs(),
                source,
                dependencies.view(),
                arena,
            )?;
            Ok((inputs != *dependencies.type_inputs()).then_some(inputs))
        })();
        // Only immutable pool roots escape this parser scratch scope.
        unsafe { arena.rewind_to(mark) };
        result
    }

    pub(super) fn prepare_schema_input_renames(
        &self,
        old: super::SqlName,
        new: super::SqlName,
        arena: &Arena,
    ) -> Result<MutableSlice<SchemaInputRename>, SqlError> {
        use super::{CatalogDdlState, StoredQueryDependencyOwner};
        use crate::sql::parser::StoredSqlKind;
        let mut plans = self.metadata_pool.lease(0).map_err(metadata_full)?;
        for (slot, rule) in self.rule_entries().filter(|(_, rule)| {
            rule.database == super::current_database() && rule.ddl_state != CatalogDdlState::Absent
        }) {
            for (image, original) in core::iter::once((
                self.committed_dependency_image(StoredQueryDependencyOwner::Rule(slot as u16)),
                rule.definition,
            ))
            .chain(rule.pending.map(|pending| {
                (
                    self.stored_query_dependency_pending_base + pending.dependency_slot as usize,
                    pending.definition,
                )
            })) {
                let mut replacement = original;
                replacement.rename_schema(old, new, arena)?;
                if let Some(inputs) = self.prepare_schema_query_inputs(
                    image,
                    |section| {
                        let pair = match section {
                            StoredInputSection::Query if original.is_view_return() => (
                                original.action_sql().next()?,
                                replacement.action_sql().next()?,
                            ),
                            StoredInputSection::RuleSource => {
                                (original.source.as_str(), replacement.source.as_str())
                            }
                            _ => return None,
                        };
                        Some((pair.0, pair.1, StoredSqlKind::Statements))
                    },
                    (old, new),
                    arena,
                )? {
                    plans
                        .push(SchemaInputRename::Query { image, inputs })
                        .map_err(metadata_full)?;
                }
            }
        }
        for (slot, routine) in (super::RoutineIter {
            catalog: &self.routines,
            next_slot: 0,
        })
        .filter(|(_, routine)| {
            routine.database == super::current_database()
                && routine.ddl_state != CatalogDdlState::Absent
        }) {
            for (image, original) in core::iter::once((
                self.committed_dependency_image(StoredQueryDependencyOwner::Routine(slot as u16)),
                routine.definition_for(0),
            ))
            .chain(routine.pending_definition.map(|pending| {
                (
                    self.stored_query_dependency_pending_base + pending.dependency_slot as usize,
                    routine.definition_for(pending.txid),
                )
            })) {
                let mut body = original.body;
                if original.body_kind != super::RoutineBodyKind::String {
                    super::rename_schema_qualified_sql(
                        &mut body,
                        &mut [],
                        old,
                        new,
                        original.body_kind.bound_sql_kind()?,
                        arena,
                    )?;
                }
                if let Some(inputs) = self.prepare_schema_query_inputs(
                    image,
                    |section| match section {
                        StoredInputSection::RoutineBody => Some((
                            original.body.as_str(),
                            body.as_str(),
                            original.body_kind.bound_sql_kind().ok()?,
                        )),
                        StoredInputSection::ParameterDefault(index) => {
                            let source = original
                                .parameters
                                .get(usize::from(index))?
                                .mode
                                .default_sql()?;
                            Some((source, source, StoredSqlKind::Expression))
                        }
                        _ => None,
                    },
                    (old, new),
                    arena,
                )? {
                    plans
                        .push(SchemaInputRename::Query { image, inputs })
                        .map_err(metadata_full)?;
                }
            }
        }
        for slot in 0..self.matview_count() {
            let view = self.matview(slot);
            if view.database != super::current_database()
                || view.ddl_state == CatalogDdlState::Absent
            {
                continue;
            }
            for (image, original) in core::iter::once((
                self.matview_dependency_image(slot),
                view.sql,
            ))
            .chain(view.pending_query.map(|pending| {
                (
                    self.stored_query_dependency_pending_base + pending.dependency_slot as usize,
                    pending.sql,
                )
            })) {
                let mut replacement = original;
                super::rename_schema_qualified_sql(
                    &mut replacement,
                    &mut [],
                    old,
                    new,
                    StoredSqlKind::Statements,
                    arena,
                )?;
                if let Some(inputs) = self.prepare_schema_query_inputs(
                    image,
                    |section| {
                        (section == StoredInputSection::Query).then_some((
                            original.as_str(),
                            replacement.as_str(),
                            StoredSqlKind::Statements,
                        ))
                    },
                    (old, new),
                    arena,
                )? {
                    plans
                        .push(SchemaInputRename::Query { image, inputs })
                        .map_err(metadata_full)?;
                }
            }
        }
        for (slot, policy) in (super::PolicyIter {
            catalog: &self.policy_catalog,
            next_slot: 0,
        })
        .filter(|(_, policy)| {
            policy.database == super::current_database()
                && policy.ddl_state != CatalogDdlState::Absent
        }) {
            for (image, original) in core::iter::once((
                self.committed_dependency_image(StoredQueryDependencyOwner::Policy(slot as u16)),
                policy.definition,
            ))
            .chain(policy.pending_definition.map(|pending| {
                (
                    self.stored_query_dependency_pending_base + pending.dependency_slot as usize,
                    pending.definition,
                )
            })) {
                let mut replacement = original;
                for source in [&mut replacement.using, &mut replacement.with_check]
                    .into_iter()
                    .flatten()
                {
                    super::rename_schema_qualified_sql(
                        source,
                        &mut [],
                        old,
                        new,
                        StoredSqlKind::Expression,
                        arena,
                    )?;
                }
                if let Some(inputs) = self.prepare_schema_query_inputs(
                    image,
                    |section| {
                        let pair = match section {
                            StoredInputSection::PolicyUsing => {
                                (original.using.as_ref()?, replacement.using.as_ref()?)
                            }
                            StoredInputSection::PolicyCheck => (
                                original.with_check.as_ref()?,
                                replacement.with_check.as_ref()?,
                            ),
                            _ => return None,
                        };
                        Some((pair.0.as_str(), pair.1.as_str(), StoredSqlKind::Expression))
                    },
                    (old, new),
                    arena,
                )? {
                    plans
                        .push(SchemaInputRename::Query { image, inputs })
                        .map_err(metadata_full)?;
                }
            }
        }
        for (slot, domain) in (super::DomainIter {
            catalog: &self.type_catalog,
            next_slot: 0,
        })
        .filter(|(_, domain)| {
            domain.database == super::current_database()
                && domain.ddl_state != CatalogDdlState::Absent
        }) {
            for (pending, transaction, source, inputs) in core::iter::once((
                false,
                domain.ddl_state.pending_txid().unwrap_or(0),
                domain.default_expr.as_ref(),
                &domain.default_inputs,
            ))
            .chain(domain.pending_definition.as_ref().map(|pending| {
                (
                    true,
                    pending.txid,
                    pending.spec.default_expr.as_ref(),
                    &pending.spec.default_inputs,
                )
            })) {
                if inputs.bytes().is_empty() {
                    continue;
                }
                let source = source.ok_or_else(invalid)?;
                let mark = arena.mark();
                let result: Result<(), SqlError> = (|| {
                    let mut replacement = *source;
                    crate::sql::enum_values::checks::rename_default_schema(
                        &mut replacement,
                        &mut [],
                        old,
                        new,
                        arena,
                    )?;
                    let before =
                        crate::sql::enum_values::checks::default_display(source.as_str(), arena)?;
                    let after = crate::sql::enum_values::checks::default_display(
                        replacement.as_str(),
                        arena,
                    )?;
                    let bindings = inputs.borrowed_sections(arena)?;
                    let mut sources = crate::mem::arena::ArenaList::new(arena);
                    for &(section, bindings) in bindings {
                        if section != StoredInputSection::DomainDefault {
                            return Err(invalid());
                        }
                        let bindings = type_inputs::rebase(
                            before,
                            after,
                            StoredSqlKind::Expression,
                            bindings,
                            |name| {
                                let mut original = crate::util::StackStr::<260>::from_str(name);
                                if original.is_truncated() {
                                    return Err(invalid());
                                }
                                super::rename_schema_qualified_sql(
                                    &mut original,
                                    &mut [],
                                    new,
                                    old,
                                    StoredSqlKind::TypeName,
                                    arena,
                                )?;
                                Ok(type_inputs::target_oid(
                                    self,
                                    transaction,
                                    original.as_str(),
                                ))
                            },
                            arena,
                        )?;
                        sources
                            .push(StoredInputSource {
                                section,
                                sql: after,
                                bindings,
                            })
                            .map_err(|_| crate::sql::query::arena_full_pub())?;
                    }
                    let replacement =
                        inputs.relocated_sources(sources.as_slice(), &self.metadata_pool)?;
                    if replacement != *inputs {
                        plans
                            .push(SchemaInputRename::Domain {
                                slot,
                                pending,
                                inputs: replacement,
                            })
                            .map_err(metadata_full)?;
                    }
                    Ok(())
                })();
                unsafe { arena.rewind_to(mark) };
                result?;
            }
        }
        Ok(plans)
    }

    pub(super) fn apply_schema_input_renames(&self, mut plans: MutableSlice<SchemaInputRename>) {
        while let Some(plan) = plans.pop() {
            match plan {
                SchemaInputRename::Query { image, inputs } => {
                    self.stored_query_dependencies
                        .lock()
                        .expect("stored-query dependency catalog lock poisoned")
                        .type_inputs[image] = inputs;
                }
                SchemaInputRename::Domain {
                    slot,
                    pending,
                    inputs,
                } => {
                    let mut catalog = self
                        .type_catalog
                        .lock()
                        .expect("type catalog lock poisoned");
                    let domain = &mut catalog.domains[slot];
                    if pending {
                        domain
                            .pending_definition
                            .as_mut()
                            .expect("exclusive schema recovery preserves pending sources")
                            .spec
                            .default_inputs = inputs;
                    } else {
                        domain.default_inputs = inputs;
                    }
                }
            }
        }
    }

    pub(crate) fn relocate_stored_type_inputs<'source>(
        &self,
        inputs: &StoredTypeInputs,
        mut source: impl FnMut(
            StoredInputSection,
        ) -> Option<(
            &'source str,
            &'source str,
            crate::sql::parser::StoredSqlKind,
        )>,
        dependencies: super::StoredQueryDependencyView<'_>,
        arena: &Arena,
    ) -> Result<StoredTypeInputs, SqlError> {
        if inputs.bytes().is_empty() {
            return Ok(inputs.clone());
        }
        let sections = inputs.borrowed_sections(arena)?;
        let mut relocated = crate::mem::arena::ArenaList::new(arena);
        for &(section, bindings) in sections {
            let (old, new, kind) = source(section).ok_or_else(invalid)?;
            let (old, new) = if section == StoredInputSection::Query {
                (
                    crate::sql::enum_values::query_binding_source(old)?,
                    crate::sql::enum_values::query_binding_source(new)?,
                )
            } else {
                (old, new)
            };
            let bindings = type_inputs::rebase(
                old,
                new,
                kind,
                bindings,
                |name| Ok(crate::sql::query::captured_type_oid(name, dependencies)),
                arena,
            )?;
            relocated
                .push(StoredInputSource {
                    section,
                    sql: new,
                    bindings,
                })
                .map_err(|_| crate::sql::query::arena_full_pub())?;
        }
        inputs.relocated_sources(relocated.as_slice(), &self.metadata_pool)
    }

    pub(super) fn validate_definition_type_inputs<'source>(
        &self,
        inputs: &StoredTypeInputs,
        source: impl FnMut(StoredInputSection) -> Option<&'source str>,
        txid: u32,
        catalog: Option<&super::TypeCatalog>,
    ) -> Result<(), SqlError> {
        validate_definition_inputs(inputs, source, txid, catalog)
    }
    pub(crate) fn freeze_runtime_type_inputs(
        &self,
        source: &str,
        bindings: &[Binding<'_>],
        txid: u32,
    ) -> Result<SharedSlice<u8>, SqlError> {
        for binding in bindings {
            if binding.end > source.len()
                || !source.is_char_boundary(binding.start)
                || !source.is_char_boundary(binding.end)
            {
                return Err(invalid());
            }
            self.validate_stored_input_type(*binding, txid)?;
        }
        let length = type_inputs::encoded_len(bindings)?;
        let mut bytes = self
            .metadata_pool
            .lease::<u8>(length)
            .map_err(metadata_full)?;
        for _ in 0..length {
            bytes.push_reserved(0).map_err(metadata_full)?;
        }
        type_inputs::encode_into(bindings, bytes.as_mut_slice())?;
        Ok(bytes.freeze())
    }

    pub(crate) fn capture_definition_type_inputs(
        &self,
        sources: &[StoredInputSource<'_>],
        txid: u32,
    ) -> Result<StoredTypeInputs, SqlError> {
        StoredTypeInputs::from_validated_sources(
            sources,
            |binding| self.validate_stored_input_type(binding, txid),
            &self.metadata_pool,
        )
    }
    pub(crate) fn capture_stored_type_inputs(
        &self,
        sources: &[StoredInputSource<'_>],
        dependencies: super::StoredQueryDependencyView<'_>,
    ) -> Result<StoredTypeInputs, SqlError> {
        StoredTypeInputs::from_sources(sources, dependencies, &self.metadata_pool)
    }
    pub(crate) fn decode_stored_type_inputs(
        &self,
        bytes: &[u8],
    ) -> Result<StoredTypeInputs, SqlError> {
        StoredTypeInputs::decode(bytes, &self.metadata_pool)
    }

    pub(super) fn validate_table_type_inputs<B: super::DefinitionBacking>(
        &self,
        definition: &super::TableDefinition<B>,
        txid: u32,
        catalog: Option<&super::TypeCatalog>,
    ) -> Result<(), SqlError> {
        Self::validate_table_type_input_root(
            definition,
            definition.type_inputs.as_ref(),
            txid,
            catalog,
        )
    }

    fn validate_table_type_input_root<B: super::DefinitionBacking>(
        definition: &super::TableDefinition<B>,
        inputs: &StoredTypeInputs,
        txid: u32,
        catalog: Option<&super::TypeCatalog>,
    ) -> Result<(), SqlError> {
        validate_definition_inputs(
            inputs,
            |section| {
                let StoredInputSection::TableDefault(attribute) = section else {
                    return None;
                };
                let column = definition.attribute_column(attribute)?;
                definition.columns.as_ref()[column]
                    .default
                    .expression()
                    .map(|sql| sql.as_str())
            },
            txid,
            catalog,
        )
    }

    pub(crate) fn restore_table_type_inputs(
        &self,
        definition: &mut super::TableDefBuilder,
        bytes: &[u8],
        txid: u32,
    ) -> Result<(), SqlError> {
        let inputs = self.decode_stored_type_inputs(bytes)?;
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        Self::validate_table_type_input_root(definition, &inputs, txid, Some(&catalog))?;
        definition.type_inputs = inputs;
        Ok(())
    }

    pub(crate) fn restore_definition_type_inputs<'source>(
        &self,
        bytes: &[u8],
        source: impl FnMut(StoredInputSection) -> Option<&'source str>,
        txid: u32,
    ) -> Result<StoredTypeInputs, SqlError> {
        let inputs = self.decode_stored_type_inputs(bytes)?;
        let catalog = self
            .type_catalog
            .lock()
            .expect("type catalog lock poisoned");
        validate_definition_inputs(&inputs, source, txid, Some(&catalog))?;
        Ok(inputs)
    }

    fn validate_stored_input_type(&self, binding: Binding<'_>, txid: u32) -> Result<(), SqlError> {
        let target = self
            .routine_result_for_oid(binding.type_oid, txid)
            .ok_or_else(invalid)?;
        validate_representation(binding, target.ctype)
    }

    pub(crate) fn restore_stored_type_inputs<'source>(
        &self,
        bytes: &[u8],
        source: impl FnMut(StoredInputSection) -> Option<&'source str>,
        dependencies: super::StoredQueryDependencyView<'_>,
        txid: u32,
    ) -> Result<StoredTypeInputs, SqlError> {
        visit_sections(bytes, |_, map| {
            type_inputs::visit(map, |binding| {
                validate_target(binding, dependencies)?;
                self.validate_stored_input_type(binding, txid)
            })
        })?;
        StoredTypeInputs::restore(bytes, source, dependencies, &self.metadata_pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::{budget::Budget, lease_pool::Purpose};
    use crate::sql::types::{ArrElem, ColType, Datum};

    fn binding<'a>(source: &str, arena: &'a Arena) -> Binding<'a> {
        let raw = crate::sql::array::build(&[Datum::Int4(41)], arena).unwrap();
        let encoded = crate::sql::exec::encode_projected_pub(
            &[Datum::Array {
                element: ArrElem::Int4,
                raw,
            }],
            arena,
        )
        .unwrap();
        Binding {
            kind: type_inputs::InputKind::ExplicitCast,
            start: 7,
            end: source.len(),
            type_oid: ColType::Array(ArrElem::Int4).oid(),
            type_mod: -1,
            collation: None,
            encoded,
        }
    }

    #[test]
    fn immutable_stored_inputs_share_capacity_and_survive_the_pool_owner() {
        let mut budget = Budget::new(8192);
        let arena = Arena::new(&mut budget, "input scratch", 4096).unwrap();
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 256, 1).unwrap();
        let source = "SELECT '{41}'::integer[]";
        let input = binding(source, &arena);
        let dependencies = super::super::StoredQueryDependencyView::new(&[]);
        crate::mem::guard::forbid_alloc(|| {
            let first = StoredTypeInputs::from_sources(
                &[StoredInputSource {
                    section: StoredInputSection::Query,
                    sql: source,
                    bindings: &[input],
                }],
                dependencies,
                &pool,
            )
            .unwrap();
            let reader = first.clone();
            assert_eq!(pool.lease::<u8>(1).unwrap_err().available_leases, 0);
            drop(first);
            let decoded = reader
                .detached_bindings(StoredInputSection::Query, &arena)
                .unwrap();
            assert_eq!(decoded.len(), 1);
            assert_eq!(decoded[0].encoded, input.encoded);
            drop(pool);
            assert!(!reader.bytes().is_empty());
            drop(reader);
        });
    }

    #[test]
    fn restored_inputs_reject_bad_spans_and_images_without_losing_existing_roots() {
        let mut budget = Budget::new(16384);
        let arena = Arena::new(&mut budget, "input validation", 8192).unwrap();
        let pool = LeasePool::new(&mut budget, Purpose::TABLE_METADATA, 256, 2).unwrap();
        let source = "SELECT '{41}'::integer[]";
        let input = binding(source, &arena);
        let dependencies = super::super::StoredQueryDependencyView::new(&[]);
        crate::mem::guard::forbid_alloc(|| {
            let first = StoredTypeInputs::from_sources(
                &[StoredInputSource {
                    section: StoredInputSection::Query,
                    sql: source,
                    bindings: &[input],
                }],
                dependencies,
                &pool,
            )
            .unwrap();
            let before = arena.used();
            assert_eq!(
                StoredTypeInputs::restore(first.bytes(), |_| Some("SELECT"), dependencies, &pool)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(arena.used(), before);
            assert_eq!(
                StoredTypeInputs::restore(b"old format", |_| Some(source), dependencies, &pool)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert_eq!(arena.used(), before);
            let restored =
                StoredTypeInputs::restore(first.bytes(), |_| Some(source), dependencies, &pool)
                    .unwrap();
            assert_eq!(first, restored);
            assert_eq!(pool.lease::<u8>(1).unwrap_err().available_leases, 0);
            drop(first);
            drop(restored);
            assert_eq!(pool.lease::<u8>(256).unwrap().capacity(), 256);
        });
    }
}
