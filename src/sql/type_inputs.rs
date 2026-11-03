//! Once-bound SQL type inputs, owned by the SQL source that was analyzed.

use super::{
    ast::{Collation, Expr},
    eval::{SqlError, sqlstate},
    types::Datum,
};
use crate::mem::{arena::Arena, buffer::FixedBuf};
use crate::sql_err;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentNode {
    Values { row: usize, value: usize },
    Update { assignment: usize },
    OnConflict { assignment: usize },
    Merge { when: usize, value: usize },
    SelectOutput { output: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AssignmentSite {
    pub statement_position: usize,
    pub node: AssignmentNode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    CreateRoutineValidation {
        statement: usize,
    },
    RootCommand {
        statement: usize,
    },
    SqlRoutineCallSite {
        routine_oid: i32,
        definition_version: u64,
        callsite: usize,
    },
    AssignmentRoutineCallSite {
        routine_oid: i32,
        definition_version: u64,
        site: AssignmentSite,
    },
    PlpgsqlBodyExpression {
        routine_oid: i32,
        definition_version: u64,
        expression: usize,
    },
    UnlocatedRoutineBody {
        routine_oid: i32,
        definition_version: u64,
    },
}

impl Source {
    pub(crate) fn routine_definition(self) -> Option<(i32, u64)> {
        match self {
            Self::SqlRoutineCallSite {
                routine_oid,
                definition_version,
                ..
            }
            | Self::AssignmentRoutineCallSite {
                routine_oid,
                definition_version,
                ..
            }
            | Self::PlpgsqlBodyExpression {
                routine_oid,
                definition_version,
                ..
            }
            | Self::UnlocatedRoutineBody {
                routine_oid,
                definition_version,
            } => Some((routine_oid, definition_version)),
            Self::RootCommand { .. } | Self::CreateRoutineValidation { .. } => None,
        }
    }
}

#[derive(Clone, Copy)]
struct ActiveSource {
    source: Source,
    text: *const str,
    bindings: *const [Binding<'static>],
}

thread_local! {
    static ACTIVE_SOURCE: core::cell::Cell<Option<ActiveSource>> = const { core::cell::Cell::new(None) };
}

struct ActiveSourceGuard<'a> {
    previous: Option<ActiveSource>,
    _borrow: core::marker::PhantomData<(&'a str, &'a [Binding<'a>])>,
    _not_send: core::marker::PhantomData<*mut ()>,
}

impl Drop for ActiveSourceGuard<'_> {
    fn drop(&mut self) {
        ACTIVE_SOURCE.with(|active| active.set(self.previous));
    }
}

fn enter_source<'a>(
    source: Source,
    text: &'a str,
    bindings: &'a [Binding<'a>],
) -> ActiveSourceGuard<'a> {
    let previous = ACTIVE_SOURCE.with(|active| {
        active.replace(Some(ActiveSource {
            source,
            text: text as *const str,
            bindings: bindings as *const [Binding<'a>] as *const [Binding<'static>],
        }))
    });
    ActiveSourceGuard {
        previous,
        _borrow: core::marker::PhantomData,
        _not_send: core::marker::PhantomData,
    }
}

pub(crate) fn with_source<R>(
    source: Source,
    text: &str,
    bindings: &[Binding<'_>],
    execute: impl FnOnce() -> R,
) -> R {
    let _source = enter_source(source, text, bindings);
    execute()
}

pub(crate) fn active_source() -> Option<Source> {
    ACTIVE_SOURCE.with(|active| active.get().map(|source| source.source))
}

/// An active source is borrowed only while its guard is on this thread.
/// Returned descriptors and payloads are copied into the destination owner.
pub(crate) fn bindings_for_active_source<'a>(
    source_slice: &str,
    arena: &'a Arena,
) -> Result<Option<&'a [Binding<'a>]>, SqlError> {
    let Some(active) = ACTIVE_SOURCE.with(core::cell::Cell::get) else {
        return Ok(None);
    };
    // SAFETY: enter_source borrows these values until its non-Send guard is
    // dropped. No reference into that scoped owner escapes this function.
    let (text, bindings) = unsafe { (&*active.text, &*active.bindings) };
    let start = text.as_ptr() as usize;
    let end = start.checked_add(text.len()).ok_or_else(invalid)?;
    let selected_start = source_slice.as_ptr() as usize;
    let selected_end = selected_start
        .checked_add(source_slice.len())
        .ok_or_else(invalid)?;
    if selected_start < start || selected_end > end {
        return Ok(None);
    }
    let selected = bindings_for_source(text, bindings, source_slice, arena)?;
    let output = arena
        .alloc_slice_with(selected.len(), |_| Binding {
            kind: InputKind::ExplicitCast,
            start: 0,
            end: 0,
            type_oid: 0,
            type_mod: -1,
            collation: None,
            encoded: &[],
        })
        .map_err(|_| super::query::arena_full_pub())?;
    for (output, binding) in output.iter_mut().zip(selected) {
        let encoded = arena
            .alloc_slice_copy(binding.encoded)
            .map_err(|_| super::query::arena_full_pub())?;
        *output = Binding {
            encoded,
            ..*binding
        };
    }
    Ok(Some(output))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum InputKind {
    ExplicitCast,
    ImplicitLiteral,
}

impl InputKind {
    const fn code(self) -> u32 {
        match self {
            Self::ExplicitCast => 0,
            Self::ImplicitLiteral => 1,
        }
    }
    const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::ExplicitCast),
            1 => Some(Self::ImplicitLiteral),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CandidateTarget<'a> {
    TypeName(&'a str),
    ContextualOid(i32),
}

#[derive(Clone, Copy)]
pub(crate) struct Candidate<'a> {
    pub start: usize,
    pub end: usize,
    pub target: CandidateTarget<'a>,
    pub kind: InputKind,
    pub type_mod: i32,
    pub expression: &'a Expr<'a>,
}

#[derive(Clone, Copy)]
pub(crate) struct Binding<'a> {
    pub kind: InputKind,
    pub start: usize,
    pub end: usize,
    pub type_oid: i32,
    pub type_mod: i32,
    pub collation: Option<Collation>,
    pub encoded: &'a [u8],
}

/// Array input invokes each domain element's input conversion during SQL
/// analysis. A scalar domain cast instead checks its constraints at runtime.
pub(crate) fn target_oid(
    storage: &crate::storage::Storage,
    txid: u32,
    type_name: &str,
) -> Option<i32> {
    let oid = super::catalog::user_type_oid(storage, txid, type_name)?;
    matches!(
        crate::storage::NamedTypeKind::from_oid(oid),
        Some((crate::storage::NamedTypeKind::Domain, _, true))
    )
    .then_some(oid)
}

pub(crate) fn candidate_target_oid(
    storage: &crate::storage::Storage,
    txid: u32,
    candidate: &Candidate<'_>,
) -> Option<i32> {
    match candidate.target {
        CandidateTarget::TypeName(name) => target_oid(storage, txid, name),
        CandidateTarget::ContextualOid(oid) => match crate::storage::NamedTypeKind::from_oid(oid) {
            Some((crate::storage::NamedTypeKind::Domain, creation, true))
                if storage
                    .domain_base_type_for_oid(super::types::oid::domain_oid(creation), txid)
                    .is_some() =>
            {
                Some(oid)
            }
            _ => None,
        },
    }
}

pub(crate) fn contextual_candidate<'a>(
    literals: &[(&'a Expr<'a>, super::parser::NamespaceSource)],
    expression: &Expr<'_>,
    type_oid: i32,
    type_mod: i32,
) -> Result<Option<Candidate<'a>>, SqlError> {
    let Expr::Str(value) = expression else {
        return Ok(None);
    };
    let (expression, source) = literals
        .iter()
        .find_map(|(literal, source)| {
            let Expr::Str(candidate) = literal else {
                return None;
            };
            (core::ptr::eq(value.as_ptr(), candidate.as_ptr()) && value.len() == candidate.len())
                .then_some((*literal, *source))
        })
        .ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "SQL type input has no source occurrence"
            )
        })?;
    Ok(Some(Candidate {
        start: source.start,
        end: source.end,
        target: CandidateTarget::ContextualOid(type_oid),
        kind: InputKind::ImplicitLiteral,
        type_mod,
        expression,
    }))
}

/// Read-only consumers restore a complete admitted image, never re-evaluate
/// a missing input or silently accept an image from another type lifetime.
pub(crate) fn validate_restored(
    parser: &super::parser::Parser<'_>,
    storage: &crate::storage::Storage,
    txid: u32,
) -> Result<(), SqlError> {
    validate_restored_with(parser, |name| Ok(target_oid(storage, txid, name)))
}

pub(crate) fn validate_restored_with(
    parser: &super::parser::Parser<'_>,
    mut resolve: impl FnMut(&str) -> Result<Option<i32>, SqlError>,
) -> Result<(), SqlError> {
    validate_restored_candidates(parser, |candidate| match candidate.target {
        CandidateTarget::TypeName(name) => resolve(name),
        CandidateTarget::ContextualOid(oid) => Ok(Some(oid)),
    })
}

pub(crate) fn validate_restored_candidates(
    parser: &super::parser::Parser<'_>,
    mut resolve: impl FnMut(&Candidate<'_>) -> Result<Option<i32>, SqlError>,
) -> Result<(), SqlError> {
    if !parser.type_inputs_complete() {
        return Err(invalid());
    }
    for candidate in parser.type_input_candidates() {
        if resolve(candidate)?.is_some() {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "SQL type input has no admitted value"
            ));
        }
    }
    for (candidate, binding) in parser.restored_type_inputs() {
        if resolve(candidate)? != Some(binding.type_oid) {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "bound SQL type input target identity changed"
            ));
        }
    }
    Ok(())
}

const MAGIC: &[u8; 4] = b"TIP2";
const HEADER: usize = 8;
const RECORD: usize = 44;

fn full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "bound SQL type inputs exceed their fixed owner capacity"
    )
}
fn invalid() -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "invalid bound SQL type input image"
    )
}

// The current input boundary binds serialized arrays. Validate the existing
// projected payload before its trusted decoder can inspect any byte offsets.
fn validate_value(encoded: &[u8]) -> Result<(), SqlError> {
    if encoded.len() < 3 || encoded[..2] != [1, 0] {
        return Err(invalid());
    }
    match encoded[2] {
        0 if encoded.len() == 3 => Ok(()),
        200 if encoded.len() >= 13 => {
            let descriptor = crate::storage::CachedArrayElement::from_bytes(
                encoded[3..9].try_into().expect("checked array descriptor"),
            )
            .ok_or_else(invalid)?;
            let length =
                u32::from_le_bytes(encoded[9..13].try_into().expect("checked array header"))
                    as usize;
            let raw = encoded
                .get(13..)
                .filter(|raw| raw.len() == length)
                .ok_or_else(invalid)?;
            super::array::shape(raw).ok_or_else(invalid)?;
            let ctype = descriptor.element().to_coltype();
            for payload in super::array::element_payloads(raw) {
                if payload.len() < 3 || payload[..2] != [1, 0] || payload[2] & !1 != 0 {
                    return Err(invalid());
                }
                let length = if payload[2] == 1 {
                    0
                } else {
                    crate::storage::rowenc::encoded_value_len(&payload[3..], ctype)
                        .map_err(|_| invalid())?
                };
                if length.checked_add(3) != Some(payload.len()) {
                    return Err(invalid());
                }
                crate::storage::rowenc::decode(payload, &[ctype], &mut [Datum::Null])
                    .map_err(|_| invalid())?;
            }
            Ok(())
        }
        _ => Err(invalid()),
    }
}

fn validate_binding(binding: Binding<'_>) -> Result<(), SqlError> {
    if binding.start >= binding.end || binding.type_oid <= 0 {
        return Err(invalid());
    }
    validate_value(binding.encoded)?;
    if binding.encoded[2] == 200 {
        let descriptor = crate::storage::CachedArrayElement::from_bytes(
            binding.encoded[3..9]
                .try_into()
                .expect("validated array descriptor"),
        )
        .ok_or_else(invalid)?;
        if descriptor.element().array_oid() != binding.type_oid {
            return Err(invalid());
        }
    }
    Ok(())
}

pub(crate) fn encoded_len(bindings: &[Binding<'_>]) -> Result<usize, SqlError> {
    let _ = u32::try_from(bindings.len()).map_err(|_| full())?;
    let mut length = HEADER;
    let mut previous = None;
    for binding in bindings {
        let key = (binding.start, binding.end, binding.kind);
        if binding.start >= binding.end
            || binding.type_oid <= 0
            || binding.encoded.len() < 3
            || binding.encoded[..2] != [1, 0]
            || previous.is_some_and(|prior| prior >= key)
        {
            return Err(invalid());
        }
        validate_binding(*binding)?;
        previous = Some(key);
        length = length
            .checked_add(RECORD)
            .and_then(|n| n.checked_add(binding.encoded.len()))
            .ok_or_else(full)?;
    }
    Ok(length)
}

#[cfg(test)]
fn capture_value<'a>(
    candidate: Candidate<'_>,
    type_oid: i32,
    collation: Option<Collation>,
    value: Datum<'_>,
    arena: &'a Arena,
) -> Result<Binding<'a>, SqlError> {
    let length = super::exec::projected_row_len(&[value])?;
    let encoded = arena
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| super::query::arena_full_pub())?;
    super::exec::encode_projected_into(&[value], encoded)?;
    Ok(Binding {
        kind: candidate.kind,
        start: candidate.start,
        end: candidate.end,
        type_oid,
        type_mod: candidate.type_mod,
        collation,
        encoded,
    })
}

/// Appends atomically; the source owner's configured buffer bounds all values.
pub(crate) fn encode(bindings: &[Binding<'_>], output: &mut FixedBuf) -> Result<(), SqlError> {
    let length = encoded_len(bindings)?;
    if length > output.capacity() - output.len() {
        return Err(full());
    }
    encode_into(bindings, output.writable())?;
    output.advance(length);
    Ok(())
}

pub(crate) fn encode_into(bindings: &[Binding<'_>], output: &mut [u8]) -> Result<usize, SqlError> {
    let length = encoded_len(bindings)?;
    let bytes = output.get_mut(..length).ok_or_else(full)?;
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4..8].copy_from_slice(&(bindings.len() as u32).to_le_bytes());
    let mut at = HEADER;
    for binding in bindings {
        bytes[at..at + 8].copy_from_slice(&(binding.start as u64).to_le_bytes());
        bytes[at + 8..at + 16].copy_from_slice(&(binding.end as u64).to_le_bytes());
        bytes[at + 16..at + 20].copy_from_slice(&binding.type_oid.to_le_bytes());
        bytes[at + 20..at + 24].copy_from_slice(&binding.type_mod.to_le_bytes());
        bytes[at + 24..at + 28].copy_from_slice(&binding.kind.code().to_le_bytes());
        bytes[at + 28..at + 32]
            .copy_from_slice(&u32::from(binding.collation.is_some()).to_le_bytes());
        bytes[at + 32..at + 36]
            .copy_from_slice(&binding.collation.map_or(0, Collation::code).to_le_bytes());
        bytes[at + 36..at + 44].copy_from_slice(&(binding.encoded.len() as u64).to_le_bytes());
        at += RECORD;
        bytes[at..at + binding.encoded.len()].copy_from_slice(binding.encoded);
        at += binding.encoded.len();
    }
    Ok(length)
}

fn read_record<'a>(bytes: &'a [u8], at: &mut usize) -> Result<Binding<'a>, SqlError> {
    let record_end = at.checked_add(RECORD).ok_or_else(invalid)?;
    let record = bytes.get(*at..record_end).ok_or_else(invalid)?;
    let number = |offset| {
        u64::from_le_bytes(
            record[offset..offset + 8]
                .try_into()
                .expect("checked record"),
        )
    };
    let start = usize::try_from(number(0)).map_err(|_| invalid())?;
    let end = usize::try_from(number(8)).map_err(|_| invalid())?;
    let type_oid = i32::from_le_bytes(record[16..20].try_into().expect("checked record"));
    let type_mod = i32::from_le_bytes(record[20..24].try_into().expect("checked record"));
    let kind = InputKind::from_code(u32::from_le_bytes(
        record[24..28].try_into().expect("checked record"),
    ))
    .ok_or_else(invalid)?;
    let present = u32::from_le_bytes(record[28..32].try_into().expect("checked record"));
    let code = u32::from_le_bytes(record[32..36].try_into().expect("checked record"));
    let collation = match present {
        0 if code == 0 => None,
        1 => Some(Collation::from_code(code).ok_or_else(invalid)?),
        _ => return Err(invalid()),
    };
    let length = usize::try_from(number(36)).map_err(|_| invalid())?;
    let end_at = record_end.checked_add(length).ok_or_else(invalid)?;
    let encoded = bytes.get(record_end..end_at).ok_or_else(invalid)?;
    *at = end_at;
    Ok(Binding {
        kind,
        start,
        end,
        type_oid,
        type_mod,
        collation,
        encoded,
    })
}

fn validate_image(bytes: &[u8]) -> Result<usize, SqlError> {
    if bytes.is_empty() {
        return Ok(0);
    }
    if bytes.len() < HEADER || &bytes[..4] != MAGIC {
        return Err(invalid());
    }
    let count = u32::from_le_bytes(bytes[4..8].try_into().expect("checked header")) as usize;
    if count > (bytes.len() - HEADER) / RECORD {
        return Err(invalid());
    }
    let mut previous = None;
    let mut at = HEADER;
    for _ in 0..count {
        let binding = read_record(bytes, &mut at)?;
        validate_binding(binding)?;
        let key = (binding.start, binding.end, binding.kind);
        if previous.is_some_and(|prior| prior >= key) {
            return Err(invalid());
        }
        previous = Some(key);
    }
    if at != bytes.len() {
        return Err(invalid());
    }
    Ok(count)
}

/// Validate the complete image before invoking any consumer. Catalog recovery
/// can inspect source/type identities without reserving temporary descriptors.
pub(crate) fn visit<'a>(
    bytes: &'a [u8],
    mut each: impl FnMut(Binding<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let count = validate_image(bytes)?;
    let mut at = HEADER;
    for _ in 0..count {
        each(read_record(bytes, &mut at)?)?;
    }
    Ok(())
}

/// Payload bytes borrow the owner; only actual binding descriptors use arena.
pub(crate) fn decode<'a>(bytes: &'a [u8], arena: &'a Arena) -> Result<&'a [Binding<'a>], SqlError> {
    let count = validate_image(bytes)?;
    if count == 0 {
        return Ok(&[]);
    }
    let bindings = arena
        .alloc_slice_with(count, |_| Binding {
            kind: InputKind::ExplicitCast,
            start: 0,
            end: 0,
            type_oid: 0,
            type_mod: -1,
            collation: None,
            encoded: &[],
        })
        .map_err(|_| super::query::arena_full_pub())?;
    let mut at = HEADER;
    for binding in bindings.iter_mut() {
        *binding = read_record(bytes, &mut at)?;
    }
    Ok(bindings)
}

/// Project only bindings whose original syntax belongs to this exact source
/// slice. Equal SQL text in a different allocation is a different source.
pub(crate) fn bindings_for_source<'a>(
    root_text: &str,
    bindings: &[Binding<'a>],
    source_slice: &str,
    arena: &'a Arena,
) -> Result<&'a [Binding<'a>], SqlError> {
    let start = (source_slice.as_ptr() as usize)
        .checked_sub(root_text.as_ptr() as usize)
        .ok_or_else(invalid)?;
    let end = start
        .checked_add(source_slice.len())
        .filter(|end| *end <= root_text.len())
        .ok_or_else(invalid)?;
    if !root_text.is_char_boundary(start) || !root_text.is_char_boundary(end) {
        return Err(invalid());
    }
    for binding in bindings {
        if binding.end > root_text.len()
            || !root_text.is_char_boundary(binding.start)
            || !root_text.is_char_boundary(binding.end)
        {
            return Err(invalid());
        }
        if binding.start < end
            && binding.end > start
            && !(binding.start >= start && binding.end <= end)
        {
            return Err(invalid());
        }
    }
    let count = bindings
        .iter()
        .filter(|binding| binding.start >= start && binding.end <= end)
        .count();
    let mut selected = bindings
        .iter()
        .filter(|binding| binding.start >= start && binding.end <= end);
    arena
        .alloc_slice_with(count, |_| {
            let binding = *selected.next().expect("counted source bindings");
            Binding {
                start: binding.start - start,
                end: binding.end - start,
                ..binding
            }
        })
        .map(|bindings| &*bindings)
        .map_err(|_| super::query::arena_full_pub())
}

/// A syntax-only rename can move occurrences but cannot execute their input
/// functions again. Match raw grammar occurrences before publishing new text.
pub(crate) fn rebase<'a>(
    old_source: &str,
    new_source: &str,
    kind: super::parser::StoredSqlKind,
    bindings: &[Binding<'a>],
    mut resolve: impl FnMut(&str) -> Result<Option<i32>, SqlError>,
    arena: &'a Arena,
) -> Result<&'a [Binding<'a>], SqlError> {
    if bindings.is_empty() {
        return Ok(&[]);
    }
    encoded_len(bindings)?;
    let old_source = arena
        .alloc_str(old_source)
        .map_err(|_| super::query::arena_full_pub())?;
    let new_source = arena
        .alloc_str(new_source)
        .map_err(|_| super::query::arena_full_pub())?;
    let old = super::parser::type_input_sources(old_source, kind, arena)?;
    let new = super::parser::type_input_sources(new_source, kind, arena)?;
    if old.literals.len() != new.literals.len() {
        return Err(invalid());
    }
    let output = arena
        .alloc_slice_copy(bindings)
        .map_err(|_| super::query::arena_full_pub())?;
    for binding in output.iter_mut() {
        match binding.kind {
            InputKind::ExplicitCast => {
                let prior = old
                    .casts
                    .iter()
                    .find(|candidate| {
                        (candidate.start, candidate.end) == (binding.start, binding.end)
                    })
                    .ok_or_else(invalid)?;
                let Expr::Cast { operand, .. } = prior.expression else {
                    return Err(invalid());
                };
                let literal = old
                    .literals
                    .iter()
                    .position(|(literal, _)| core::ptr::eq(*literal, *operand))
                    .ok_or_else(invalid)?;
                let next_literal = new.literals[literal].0;
                let next = new.casts.iter().find(|candidate|
                    matches!(candidate.expression, Expr::Cast { operand, .. } if core::ptr::eq(*operand, next_literal)))
                    .ok_or_else(invalid)?;
                let (
                    Expr::Cast {
                        operand: Expr::Str(old_value),
                        ..
                    },
                    Expr::Cast {
                        operand: Expr::Str(new_value),
                        ..
                    },
                ) = (prior.expression, next.expression)
                else {
                    return Err(invalid());
                };
                let target = if prior.target == next.target {
                    // An unchanged written name belongs to the captured type,
                    // even after that name has been renamed or reused.
                    Some(binding.type_oid)
                } else {
                    match next.target {
                        CandidateTarget::TypeName(name) => resolve(name)?,
                        CandidateTarget::ContextualOid(oid) => Some(oid),
                    }
                };
                if old_value != new_value
                    || prior.type_mod != binding.type_mod
                    || next.type_mod != binding.type_mod
                    || target != Some(binding.type_oid)
                {
                    return Err(invalid());
                }
                binding.start = next.start;
                binding.end = next.end;
            }
            InputKind::ImplicitLiteral => {
                let index = old
                    .literals
                    .iter()
                    .position(|(_, span)| (span.start, span.end) == (binding.start, binding.end))
                    .ok_or_else(invalid)?;
                let (old_value, _) = old.literals[index];
                let (new_value, span) = new.literals[index];
                let (Expr::Str(old_value), Expr::Str(new_value)) = (old_value, new_value) else {
                    return Err(invalid());
                };
                if old_value != new_value {
                    return Err(invalid());
                }
                binding.start = span.start;
                binding.end = span.end;
            }
        }
    }
    encoded_len(output)?;
    Ok(output)
}

pub(crate) fn value<'a>(encoded: &'a [u8], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    validate_value(encoded)?;
    let mut output = [Datum::Null];
    super::exec::decode_projected_row_into(encoded, arena, &mut output)?;
    Ok(output[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;
    use crate::sql::{
        ast::{SelectItem, Stmt},
        parser::Parser,
        types::{ArrElem, ColType},
    };

    #[test]
    fn source_rebase_keeps_captured_types_when_uncached_casts_are_inserted() {
        let mut budget = Budget::new(64 << 10);
        let arena = Arena::new(&mut budget, "rebase captured type input", 64 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old = "SELECT 'a', '{7}'::integer[]";
            let kind = super::super::parser::StoredSqlKind::Statements;
            let syntax = super::super::parser::type_input_sources(old, kind, &arena).unwrap();
            let raw = super::super::array::build(&[Datum::Int4(7)], &arena).unwrap();
            let binding = capture_value(
                syntax.casts[0],
                ColType::Array(ArrElem::Int4).oid(),
                None,
                Datum::Array {
                    element: ArrElem::Int4,
                    raw,
                },
                &arena,
            )
            .unwrap();
            let new = "SELECT 'a'::text, '{7}'::integer[]";
            let rebased = rebase(
                old,
                new,
                kind,
                &[binding],
                |_| panic!("unchanged target uses captured lifetime"),
                &arena,
            )
            .unwrap();
            let syntax = super::super::parser::type_input_sources(new, kind, &arena).unwrap();
            assert_eq!(rebased[0].start, syntax.casts[1].start);
            assert_eq!(rebased[0].end, syntax.casts[1].end);
            assert_eq!(rebased[0].type_oid, binding.type_oid);
            assert_eq!(rebased[0].encoded, binding.encoded);
            let changed = "SELECT 'a'::text, '{8}'::integer[]";
            assert_eq!(
                rebase(old, changed, kind, &[binding], |_| Ok(None), &arena)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
        });
    }

    #[test]
    fn input_images_restore_distinct_occurrences_and_nominal_metadata_without_allocating() {
        let mut budget = Budget::new(65536);
        let source = Arena::new(&mut budget, "source", 8192).unwrap();
        let values = Arena::new(&mut budget, "values", 8192).unwrap();
        let restored = Arena::new(&mut budget, "restored", 8192).unwrap();
        let mut owner = FixedBuf::new(&mut budget, "prepared_bytes", 4096).unwrap();
        let sql = "SELECT '{11}'::integer[], CAST(E'{11}' AS integer[])";
        crate::mem::guard::forbid_alloc(|| {
            let mut parser = Parser::new(sql, &source).unwrap();
            parser.capture_type_inputs();
            parser.next_stmt().unwrap().unwrap();
            let candidates = parser.type_input_candidates();
            assert_eq!(candidates.len(), 2);
            assert!(candidates[0].start < candidates[1].start);
            let bindings = [
                capture_value(
                    candidates[0],
                    ColType::Array(ArrElem::Int4).oid(),
                    None,
                    Datum::Array {
                        element: ArrElem::Int4,
                        raw: super::super::array::build(&[Datum::Int4(11)], &values).unwrap(),
                    },
                    &values,
                )
                .unwrap(),
                capture_value(
                    candidates[1],
                    ColType::Array(ArrElem::Int4).oid(),
                    Some(Collation::None),
                    Datum::Array {
                        element: ArrElem::Int4,
                        raw: super::super::array::build(&[Datum::Int4(22)], &values).unwrap(),
                    },
                    &values,
                )
                .unwrap(),
            ];
            encode(&bindings, &mut owner).unwrap();
            let bound = decode(owner.readable(), &restored).unwrap();
            assert_eq!(bound.len(), 2);
            let mut parser = Parser::new(sql, &restored).unwrap();
            parser.set_type_inputs(bound);
            let Some(Stmt::Select(select)) = parser.next_stmt().unwrap() else {
                panic!("SELECT")
            };
            assert!(parser.type_input_candidates().is_empty());
            for (index, item) in select.items.iter().enumerate() {
                let SelectItem::Expr { expression, .. } = item else {
                    panic!("expression")
                };
                let Expr::BoundLiteral {
                    encoded,
                    type_oid,
                    type_mod,
                    collation,
                    source,
                } = expression
                else {
                    panic!("bound input")
                };
                assert_eq!(*type_oid, ColType::Array(ArrElem::Int4).oid());
                assert_eq!(*type_mod, -1);
                assert_eq!(*collation, bindings[index].collation);
                assert!(matches!(source, Expr::Cast { .. }));
                let Datum::Array { element, raw } = value(encoded, &restored).unwrap() else {
                    panic!("array")
                };
                let Some(Datum::Int4(actual)) = super::super::array::get(raw, element, 0) else {
                    panic!("int4")
                };
                assert_eq!(actual, if index == 0 { 11 } else { 22 });
            }
        });
    }

    #[test]
    fn implicit_literal_restore_keeps_explicit_casts_and_copied_origins_distinct() {
        let mut budget = Budget::new(32768);
        let arena = Arena::new(&mut budget, "input", 8192).unwrap();
        let restored = Arena::new(&mut budget, "restored", 8192).unwrap();
        let sql = "SELECT '{11}', '{11}'::integer[]";
        crate::mem::guard::forbid_alloc(|| {
            let mut parser = Parser::new(sql, &arena).unwrap();
            parser.capture_type_inputs();
            parser.next_stmt().unwrap().unwrap();
            let literals = parser.string_literals();
            let copied =
                super::super::ast::duplicate_expression(literals[0].0, &arena, &[]).unwrap();
            let candidate =
                contextual_candidate(literals, copied, ColType::Array(ArrElem::Int4).oid(), -1)
                    .unwrap()
                    .unwrap();
            assert_eq!(candidate.kind, InputKind::ImplicitLiteral);
            let datum = Datum::Array {
                element: ArrElem::Int4,
                raw: super::super::array::build(&[Datum::Int4(11)], &arena).unwrap(),
            };
            let bindings = [
                capture_value(
                    candidate,
                    ColType::Array(ArrElem::Int4).oid(),
                    None,
                    datum,
                    &arena,
                )
                .unwrap(),
                capture_value(
                    parser.type_input_candidates()[0],
                    ColType::Array(ArrElem::Int4).oid(),
                    None,
                    datum,
                    &arena,
                )
                .unwrap(),
            ];
            let mut parser = Parser::new(sql, &restored).unwrap();
            parser.set_type_inputs(&bindings);
            let Some(Stmt::Select(select)) = parser.next_stmt().unwrap() else {
                panic!("SELECT")
            };
            assert!(parser.type_inputs_complete());
            assert!(parser.type_input_candidates().is_empty());
            let SelectItem::Expr {
                expression: first, ..
            } = select.items[0]
            else {
                panic!("literal")
            };
            let SelectItem::Expr {
                expression: second, ..
            } = select.items[1]
            else {
                panic!("cast")
            };
            assert!(matches!(
                first,
                Expr::BoundLiteral {
                    source: Expr::Str(_),
                    ..
                }
            ));
            assert!(matches!(
                second,
                Expr::BoundLiteral {
                    source: Expr::Cast { .. },
                    ..
                }
            ));
        });
    }

    #[test]
    fn source_projection_requires_the_exact_owner_and_rebases_occurrences() {
        let mut budget = Budget::new(16384);
        let arena = Arena::new(&mut budget, "input", 4096).unwrap();
        let root = "FIRST; SECOND";
        let source = &root[7..];
        let unrelated = arena.alloc_str(source).unwrap();
        let raw = super::super::array::build(&[Datum::Int4(1)], &arena).unwrap();
        let datum = Datum::Array {
            element: ArrElem::Int4,
            raw,
        };
        let length = super::super::exec::projected_row_len(&[datum]).unwrap();
        let encoded = arena.alloc_slice_with(length, |_| 0u8).unwrap();
        super::super::exec::encode_projected_into(&[datum], encoded).unwrap();
        let first = Binding {
            kind: InputKind::ExplicitCast,
            start: 0,
            end: 5,
            type_oid: ColType::Array(ArrElem::Int4).oid(),
            type_mod: -1,
            collation: None,
            encoded,
        };
        let second = Binding {
            start: 7,
            end: 13,
            ..first
        };
        crate::mem::guard::forbid_alloc(|| {
            let projected = bindings_for_source(root, &[first, second], source, &arena).unwrap();
            assert_eq!(projected.len(), 1);
            assert_eq!((projected[0].start, projected[0].end), (0, 6));
            assert_eq!(projected[0].encoded, encoded);
            let Err(error) = bindings_for_source(root, &[first, second], unrelated, &arena) else {
                panic!("different source owner accepted")
            };
            assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        });
    }

    #[test]
    fn active_source_projection_detaches_payload_and_restores_nested_authority() {
        let mut budget = Budget::new(16384);
        let mut source = Arena::new(&mut budget, "source", 4096).unwrap();
        let destination = Arena::new(&mut budget, "destination", 4096).unwrap();
        let root = Source::RootCommand { statement: 2 };
        let nested = Source::SqlRoutineCallSite {
            routine_oid: 62001,
            definition_version: 3,
            callsite: 1,
        };
        let text = "SELECT input";
        let raw = super::super::array::build(&[Datum::Int4(7)], &source).unwrap();
        let datum = Datum::Array {
            element: ArrElem::Int4,
            raw,
        };
        let length = super::super::exec::projected_row_len(&[datum]).unwrap();
        let encoded = source.alloc_slice_with(length, |_| 0u8).unwrap();
        super::super::exec::encode_projected_into(&[datum], encoded).unwrap();
        let bindings = [Binding {
            kind: InputKind::ExplicitCast,
            start: 7,
            end: 12,
            type_oid: ColType::Array(ArrElem::Int4).oid(),
            type_mod: -1,
            collation: None,
            encoded,
        }];
        let detached = crate::mem::guard::forbid_alloc(|| {
            with_source(root, text, &bindings, || {
                assert_eq!(active_source(), Some(root));
                with_source(nested, "nested", &[], || {
                    assert_eq!(active_source(), Some(nested))
                });
                assert_eq!(active_source(), Some(root));
                let foreign = destination.alloc_str(text).unwrap();
                assert!(
                    bindings_for_active_source(foreign, &destination)
                        .unwrap()
                        .is_none()
                );
                bindings_for_active_source(&text[7..], &destination)
                    .unwrap()
                    .unwrap()
            })
        });
        assert_eq!(active_source(), None);
        source.reset();
        crate::mem::guard::forbid_alloc(|| {
            let Datum::Array { element, raw } = value(detached[0].encoded, &destination).unwrap()
            else {
                panic!("array")
            };
            assert!(matches!(
                super::super::array::get(raw, element, 0),
                Some(Datum::Int4(7))
            ));
            assert!(
                bindings_for_active_source(text, &destination)
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn encoded_input_capacity_refusal_preserves_existing_owner_bytes() {
        let mut budget = Budget::new(16384);
        let arena = Arena::new(&mut budget, "input", 4096).unwrap();
        let mut owner = FixedBuf::new(&mut budget, "prepared_bytes", 32).unwrap();
        assert!(owner.append(b"prior"));
        let raw = super::super::array::build(&[Datum::Int4(1)], &arena).unwrap();
        let datum = Datum::Array {
            element: ArrElem::Int4,
            raw,
        };
        let length = super::super::exec::projected_row_len(&[datum]).unwrap();
        let encoded = arena.alloc_slice_with(length, |_| 0u8).unwrap();
        super::super::exec::encode_projected_into(&[datum], encoded).unwrap();
        let binding = Binding {
            kind: InputKind::ExplicitCast,
            start: 1,
            end: 8,
            type_oid: ColType::Array(ArrElem::Int4).oid(),
            type_mod: -1,
            collation: None,
            encoded,
        };
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                encode(&[binding], &mut owner).unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(owner.readable(), b"prior");
        });
    }

    #[test]
    fn input_images_reject_duplicate_occurrence_keys_and_truncated_records() {
        let mut budget = Budget::new(16384);
        let arena = Arena::new(&mut budget, "input", 4096).unwrap();
        let mut owner = FixedBuf::new(&mut budget, "prepared_bytes", 256).unwrap();
        let raw = super::super::array::build(&[Datum::Int4(1)], &arena).unwrap();
        let datum = Datum::Array {
            element: ArrElem::Int4,
            raw,
        };
        let length = super::super::exec::projected_row_len(&[datum]).unwrap();
        let encoded = arena.alloc_slice_with(length, |_| 0u8).unwrap();
        super::super::exec::encode_projected_into(&[datum], encoded).unwrap();
        let binding = Binding {
            kind: InputKind::ExplicitCast,
            start: 1,
            end: 8,
            type_oid: ColType::Array(ArrElem::Int4).oid(),
            type_mod: -1,
            collation: None,
            encoded,
        };
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                encode(&[binding, binding], &mut owner)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            assert!(owner.is_empty());
            encode(&[binding], &mut owner).unwrap();
            for end in 1..owner.len() {
                assert!(decode(&owner.readable()[..end], &arena).is_err());
            }
        });
    }
}
