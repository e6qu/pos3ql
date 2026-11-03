//! Immutable checked type-input values follow the stored SQL source image.

use std::sync::Arc;

use crate::mem::{
    arena::Arena,
    lease_pool::{LeasePool, SharedSlice},
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
        StoredTypeInputs::restore_validated(
            bytes,
            source,
            |binding| self.validate_stored_input_type(binding, txid),
            &self.metadata_pool,
        )
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
