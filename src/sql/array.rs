//! Array values: a rectangular shape and row-encoded elements.

use crate::mem::arena::Arena;
use crate::sql_err;
use crate::storage::rowenc;

use super::eval::{SqlError, sqlstate};
use super::types::{ArrElem, Datum};

/// The durable array header stores its element count as an unsigned 16-bit
/// value. Execution derives temporary slices from the actual count, so this
/// representation boundary is the only element-count ceiling.
pub const MAX_ELEMENTS: usize = u16::MAX as usize;
pub const MAX_DIMENSIONS: usize = 6;

const MAGIC: [u8; 4] = *b"AR01";
const PREFIX: usize = 7;
pub const EMPTY: &[u8] = b"AR01\x00\x00\x00";

/// PostgreSQL's array inputs include catalog vectors with zero-based bounds.
/// This view reads their native bytes without copying a vector per output row.
#[derive(Clone, Copy)]
pub(crate) enum Input<'a> {
    Array { element: ArrElem, raw: &'a [u8] },
    Int2Vector { raw: &'a [u8], lower: i32 },
    OidVector { raw: &'a [u8], lower: i32 },
}

impl<'a> Input<'a> {
    pub(crate) fn from_datum(value: Datum<'a>) -> Option<Self> {
        match value {
            Datum::Array { element, raw } => Some(Self::Array { element, raw }),
            Datum::Int2Vector { raw, lower_bound } => Some(Self::Int2Vector {
                raw,
                lower: lower_bound,
            }),
            Datum::OidVector { raw, lower_bound } => Some(Self::OidVector {
                raw,
                lower: lower_bound,
            }),
            _ => None,
        }
    }

    pub(crate) fn element_count(self) -> usize {
        match self {
            Self::Array { raw, .. } => len(raw),
            Self::Int2Vector { raw, .. } => raw.len() / 2,
            Self::OidVector { raw, .. } => raw.len() / 4,
        }
    }

    pub(crate) fn get(self, index: usize) -> Option<Datum<'a>> {
        match self {
            Self::Array { element, raw } => get(raw, element, index),
            Self::Int2Vector { raw, .. } => {
                let start = index.checked_mul(2)?;
                Some(Datum::Int2(i16::from_le_bytes(
                    raw.get(start..start.checked_add(2)?)?.try_into().ok()?,
                )))
            }
            Self::OidVector { raw, .. } => {
                let start = index.checked_mul(4)?;
                Some(Datum::Oid(u32::from_le_bytes(
                    raw.get(start..start.checked_add(4)?)?.try_into().ok()?,
                )))
            }
        }
    }

    pub(crate) fn bounds(self, dimension: usize) -> Result<Option<(usize, i32, i32)>, SqlError> {
        let lower = match self {
            Self::Array { raw, .. } => {
                let shape = shape(raw).ok_or_else(invalid_shape)?;
                return Ok(shape.dimension(dimension).map(|length| {
                    (
                        length,
                        shape.lower_bound(dimension).expect("known array dimension"),
                        shape
                            .upper_bound(dimension)
                            .expect("validated array bounds"),
                    )
                }));
            }
            Self::Int2Vector { lower, .. } | Self::OidVector { lower, .. } if dimension == 0 => {
                lower
            }
            _ => return Ok(None),
        };
        let length = self.element_count();
        let upper = i32::try_from(length)
            .ok()
            .and_then(|length| lower.checked_add(length - 1))
            .ok_or_else(array_too_large)?;
        Ok(Some((length, lower, upper)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    dimensions: [u16; MAX_DIMENSIONS],
    lower_bounds: [i32; MAX_DIMENSIONS],
    dimension_count: usize,
    element_count: usize,
}

impl Shape {
    pub fn empty() -> Self {
        Self {
            dimensions: [0; MAX_DIMENSIONS],
            lower_bounds: [0; MAX_DIMENSIONS],
            dimension_count: 0,
            element_count: 0,
        }
    }

    pub fn one(element_count: usize) -> Result<Self, SqlError> {
        if element_count == 0 {
            return Ok(Self::empty());
        }
        Self::new(&[element_count], &[1])
    }

    pub fn new(dimensions: &[usize], lower_bounds: &[i32]) -> Result<Self, SqlError> {
        if dimensions.len() != lower_bounds.len()
            || dimensions.len() > MAX_DIMENSIONS
            || dimensions.is_empty() && !lower_bounds.is_empty()
        {
            return Err(invalid_shape());
        }
        if dimensions.is_empty() {
            return Ok(Self::empty());
        }
        let mut result = Self::empty();
        let mut count = 1usize;
        for (index, (&dimension, &lower_bound)) in dimensions.iter().zip(lower_bounds).enumerate() {
            let dimension = u16::try_from(dimension).map_err(|_| array_too_large())?;
            // PostgreSQL requires the exclusive upper bound to fit in int32.
            if lower_bound.checked_add(i32::from(dimension)).is_none() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "array lower bound is too large: {lower_bound}"
                ));
            }
            count = count
                .checked_mul(usize::from(dimension))
                .ok_or_else(array_too_large)?;
            if count > MAX_ELEMENTS {
                return Err(array_too_large());
            }
            result.dimensions[index] = dimension;
            result.lower_bounds[index] = lower_bound;
        }
        if count == 0 {
            return Ok(Self::empty());
        }
        result.dimension_count = dimensions.len();
        result.element_count = count;
        Ok(result)
    }

    pub fn dimension_count(self) -> usize {
        self.dimension_count
    }

    pub fn dimension(self, index: usize) -> Option<usize> {
        (index < self.dimension_count).then(|| usize::from(self.dimensions[index]))
    }

    pub fn lower_bound(self, index: usize) -> Option<i32> {
        (index < self.dimension_count).then(|| self.lower_bounds[index])
    }

    pub fn upper_bound(self, index: usize) -> Option<i32> {
        let lower = self.lower_bound(index)?;
        let length = i32::try_from(self.dimension(index)?).ok()?;
        lower.checked_add(length - 1)
    }

    pub fn element_count(self) -> usize {
        self.element_count
    }

    pub fn without_first(self) -> Result<Self, SqlError> {
        if self.dimension_count <= 1 {
            return Ok(Self::empty());
        }
        let mut dimensions = [0usize; MAX_DIMENSIONS];
        let mut lower_bounds = [0i32; MAX_DIMENSIONS];
        for index in 1..self.dimension_count {
            dimensions[index - 1] = self.dimension(index).unwrap();
            lower_bounds[index - 1] = self.lower_bound(index).unwrap();
        }
        Self::new(
            &dimensions[..self.dimension_count - 1],
            &lower_bounds[..self.dimension_count - 1],
        )
    }

    pub fn with_first(self, dimension: usize, lower_bound: i32) -> Result<Self, SqlError> {
        if self.dimension_count == MAX_DIMENSIONS {
            return Err(array_too_large());
        }
        let mut dimensions = [0usize; MAX_DIMENSIONS];
        let mut lower_bounds = [0i32; MAX_DIMENSIONS];
        dimensions[0] = dimension;
        lower_bounds[0] = lower_bound;
        for index in 0..self.dimension_count {
            dimensions[index + 1] = self.dimension(index).unwrap();
            lower_bounds[index + 1] = self.lower_bound(index).unwrap();
        }
        Self::new(
            &dimensions[..self.dimension_count + 1],
            &lower_bounds[..self.dimension_count + 1],
        )
    }

    pub fn sliced_first(self, dimension: usize) -> Result<Self, SqlError> {
        let mut dimensions = [0usize; MAX_DIMENSIONS];
        let lower_bounds = [1i32; MAX_DIMENSIONS];
        dimensions[0] = dimension;
        for (target, source) in dimensions[1..self.dimension_count]
            .iter_mut()
            .zip(self.dimensions[1..self.dimension_count].iter())
        {
            *target = usize::from(*source);
        }
        Self::new(
            &dimensions[..self.dimension_count],
            &lower_bounds[..self.dimension_count],
        )
    }
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "array value exceeds the statement arena"
    )
}

fn array_too_large() -> SqlError {
    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array value too large")
}

fn invalid_shape() -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid array dimensions"
    )
}

/// Reserves an exact temporary element slice from statement memory. Array
/// producers use this choke point instead of embedding a narrower stack
/// capacity than the durable format accepts.
pub(crate) fn alloc_items<'a>(
    arena: &'a Arena,
    len: usize,
) -> Result<&'a mut [Datum<'a>], SqlError> {
    if len > MAX_ELEMENTS {
        return Err(array_too_large());
    }
    arena
        .alloc_slice_with(len, |_| Datum::Null)
        .map_err(|_| arena_full())
}

/// Serializes a rank-one array with PostgreSQL's default lower bound.
pub fn build<'a>(items: &[Datum], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    build_shaped(items, Shape::one(items.len())?, arena)
}

/// Serializes items under their supplied rectangular shape.
pub fn build_shaped<'a>(
    items: &[Datum],
    shape: Shape,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    if items.len() != shape.element_count() {
        return Err(invalid_shape());
    }
    // A named composite enters an array in its durable text form, which only
    // the catalog can produce; the element's caller must normalize it first.
    if items
        .iter()
        .any(|item| matches!(item, Datum::Composite { .. }))
    {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "named composite array element was not normalized by the catalog"
        ));
    }
    let header = PREFIX + shape.dimension_count() * 6;
    let mut total = header;
    // A record array's elements use the projected encoding, which keeps each
    // record's structure; its NULL elements share that encoding.
    let records = items.iter().any(|item| matches!(item, Datum::Record(_)));
    let projected = |item: &Datum| matches!(item, Datum::Record(_)) || records && item.is_null();
    for item in items {
        let length = if projected(item) {
            2 + crate::sql::exec::projected_value_len(item)?
        } else {
            rowenc::encoded_len(core::slice::from_ref(item))
        };
        total = total.checked_add(4 + length).ok_or_else(array_too_large)?;
    }
    let out = arena
        .alloc_slice_with(total, |_| 0u8)
        .map_err(|_| arena_full())?;
    out[..4].copy_from_slice(&MAGIC);
    out[4] = shape.dimension_count() as u8;
    out[5..7].copy_from_slice(&(items.len() as u16).to_le_bytes());
    let mut at = PREFIX;
    for index in 0..shape.dimension_count() {
        out[at..at + 2].copy_from_slice(&(shape.dimension(index).unwrap() as u16).to_le_bytes());
        out[at + 2..at + 6].copy_from_slice(&shape.lower_bound(index).unwrap().to_le_bytes());
        at += 6;
    }
    for item in items {
        let record = projected(item);
        let length = if record {
            2 + crate::sql::exec::projected_value_len(item)?
        } else {
            rowenc::encoded_len(core::slice::from_ref(item))
        };
        out[at..at + 4].copy_from_slice(&(length as u32).to_le_bytes());
        at += 4;
        if record {
            crate::sql::exec::encode_projected_into(
                core::slice::from_ref(item),
                &mut out[at..at + length],
            )?;
        } else {
            rowenc::encode(core::slice::from_ref(item), &mut out[at..at + length]);
        }
        at += length;
    }
    Ok(out)
}

/// Combines array values into one array with a new leading dimension.
/// PostgreSQL has no array-of-array value: every member must have the same
/// element identity and shape.
pub fn stack<'a>(members: &[Datum<'a>], arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
    let Some(Datum::Array { element, raw }) = members.iter().find(|member| !member.is_null())
    else {
        return Err(sql_err!(
            sqlstate::NULL_VALUE_NOT_ALLOWED,
            "cannot accumulate null arrays"
        ));
    };
    let child = shape(raw).expect("array datum invariant");
    if child.dimension_count() == 0 {
        return Err(sql_err!(
            sqlstate::ARRAY_SUBSCRIPT_ERROR,
            "cannot accumulate empty arrays"
        ));
    }
    let result_shape = child.with_first(members.len(), 1)?;
    let flattened = arena
        .alloc_slice_with(result_shape.element_count(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let mut at = 0usize;
    for member in members {
        let Datum::Array {
            element: member_element,
            raw: member_raw,
        } = *member
        else {
            return Err(sql_err!(
                sqlstate::NULL_VALUE_NOT_ALLOWED,
                "cannot accumulate null arrays"
            ));
        };
        let member_shape = shape(member_raw).expect("array datum invariant");
        if member_element != *element || member_shape != child {
            return Err(sql_err!(
                sqlstate::ARRAY_SUBSCRIPT_ERROR,
                "cannot accumulate arrays of different dimensionality"
            ));
        }
        for value in elements(member_raw, member_element) {
            flattened[at] = value;
            at += 1;
        }
    }
    Ok(Datum::Array {
        element: *element,
        raw: build_shaped(flattened, result_shape, arena)?,
    })
}

/// Returns the self-describing shape only when the entire blob is valid.
pub fn shape(raw: &[u8]) -> Option<Shape> {
    if raw.len() < PREFIX || raw[..4] != MAGIC {
        return None;
    }
    let dimension_count = usize::from(raw[4]);
    if dimension_count > MAX_DIMENSIONS || raw.len() < PREFIX + dimension_count * 6 {
        return None;
    }
    let stored_count = usize::from(u16::from_le_bytes(raw[5..7].try_into().ok()?));
    let mut dimensions = [0usize; MAX_DIMENSIONS];
    let mut lower_bounds = [0i32; MAX_DIMENSIONS];
    let mut at = PREFIX;
    for index in 0..dimension_count {
        dimensions[index] = usize::from(u16::from_le_bytes(raw[at..at + 2].try_into().ok()?));
        lower_bounds[index] = i32::from_le_bytes(raw[at + 2..at + 6].try_into().ok()?);
        at += 6;
    }
    let shape = Shape::new(
        &dimensions[..dimension_count],
        &lower_bounds[..dimension_count],
    )
    .ok()?;
    if shape.element_count() != stored_count {
        return None;
    }
    for _ in 0..stored_count {
        let length =
            usize::try_from(u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?)).ok()?;
        at = at.checked_add(4 + length)?;
        if at > raw.len() {
            return None;
        }
    }
    (at == raw.len()).then_some(shape)
}

pub fn len(raw: &[u8]) -> usize {
    shape(raw)
        .expect("array datum must carry a canonical valid shape")
        .element_count()
}

fn payload_offset(shape: Shape) -> usize {
    PREFIX + shape.dimension_count() * 6
}

pub fn get<'a>(raw: &'a [u8], element: ArrElem, index: usize) -> Option<Datum<'a>> {
    let shape = shape(raw).expect("array datum must carry a canonical valid shape");
    if index >= shape.element_count() {
        return None;
    }
    let schema = [element.to_coltype()];
    let mut at = payload_offset(shape);
    for current in 0..shape.element_count() {
        let length = u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?) as usize;
        at += 4;
        if current == index {
            if element == ArrElem::Record {
                return Some(
                    record_element(raw.get(at..at + length)?).map_or(Datum::Null, |v| v.datum()),
                );
            }
            let mut out = [Datum::Null; 1];
            rowenc::decode(raw.get(at..at + length)?, &schema, &mut out).ok()?;
            return Some(out[0]);
        }
        at += length;
    }
    None
}

/// Walks a validated array's sequential payload once. Array values do not
/// carry an offset table, so bulk operations must use this iterator rather
/// than repeated indexed lookup, which would rescan the prefix for every
/// element.
#[derive(Clone, Copy)]
pub struct Elements<'a> {
    payloads: ElementPayloads<'a>,
    element: ArrElem,
}

/// One record array element's encoded value, `None` for NULL. Elements use
/// the projected encoding; an array built from NULLs alone holds row-codec
/// NULLs, the only payload whose single tag position reads 1 (a record's tag
/// never does).
pub(crate) fn record_element(payload: &[u8]) -> Option<crate::sql::exec::ProjectedValue<'_>> {
    let value = crate::sql::exec::ProjectedValue::column(payload, 0);
    (payload.get(2) != Some(&1) && !value.is_null()).then_some(value)
}

/// Walks an array's elements like [`elements`], but rebuilds record
/// elements with their fields (arena-allocated) instead of their rendered
/// text, for consumers that compare or access fields.
pub(crate) fn structured_elements<'a>(
    raw: &'a [u8],
    element: ArrElem,
    arena: &'a Arena,
) -> impl Iterator<Item = Result<Datum<'a>, SqlError>> + 'a {
    let records = element == ArrElem::Record;
    element_payloads(raw)
        .zip(elements(raw, element))
        .map(move |(payload, value)| {
            if records && record_element(payload).is_some() {
                crate::sql::exec::decode_projected_col_record(payload, 0, arena)
            } else {
                Ok(value)
            }
        })
}

pub fn elements(raw: &[u8], element: ArrElem) -> Elements<'_> {
    Elements {
        payloads: element_payloads(raw),
        element,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ElementPayloads<'a> {
    raw: &'a [u8],
    remaining: usize,
    at: usize,
}

pub(crate) fn element_payloads(raw: &[u8]) -> ElementPayloads<'_> {
    let shape = shape(raw).expect("array datum must carry a canonical valid shape");
    ElementPayloads {
        raw,
        remaining: shape.element_count(),
        at: payload_offset(shape),
    }
}

impl<'a> Iterator for ElementPayloads<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let length =
            u32::from_le_bytes(self.raw.get(self.at..self.at + 4)?.try_into().ok()?) as usize;
        self.at += 4;
        let payload = self.raw.get(self.at..self.at + length)?;
        self.at += length;
        self.remaining -= 1;
        Some(payload)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for ElementPayloads<'_> {}

impl<'a> Iterator for Elements<'a> {
    type Item = Datum<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let payload = self.payloads.next()?;
        if self.element == ArrElem::Record {
            return Some(record_element(payload).map_or(Datum::Null, |v| v.datum()));
        }
        let schema = [self.element.to_coltype()];
        let mut out = [Datum::Null; 1];
        rowenc::decode(payload, &schema, &mut out).ok()?;
        Some(out[0])
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.payloads.size_hint()
    }
}

impl ExactSizeIterator for Elements<'_> {}

/// Reads an array literal as PostgreSQL's `array_in` does
/// (`src/backend/utils/adt/arrayfuncs.c`), with its errors and DETAILs.
///
/// A first pass reads the syntax and counts the elements, so their slice is
/// reserved at its exact size before any is converted. When that pass finds
/// the literal malformed, the elements before the fault are converted first,
/// because PostgreSQL converts each element as it reads it and reports a bad
/// element ahead of a later syntax error.
pub fn parse_literal<'a>(
    text: &'a str,
    element: ArrElem,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let layout = match Reader::new(text, element, arena).read(&mut Pass::Count) {
        Ok(layout) => layout,
        Err(Fault::Error(error)) => return Err(error),
        Err(Fault::Malformed(_)) => {
            let fault = Reader::new(text, element, arena)
                .read(&mut Pass::Check)
                .err()
                .expect("a malformed literal stays malformed");
            return Err(fault.error(text));
        }
    };
    if layout.items == 0 {
        return build(&[], arena);
    }
    let items = alloc_items(arena, layout.items)?;
    Reader::new(text, element, arena)
        .read(&mut Pass::Fill(items))
        .map_err(|fault| fault.error(text))?;
    let mut dimensions = [0usize; MAX_DIMENSIONS];
    for (dimension, &width) in dimensions
        .iter_mut()
        .zip(&layout.widths[..layout.dimensions])
    {
        *dimension = usize::try_from(width).expect("a read dimension is counted");
    }
    build_shaped(
        items,
        Shape::new(
            &dimensions[..layout.dimensions],
            &layout.lower_bounds[..layout.dimensions],
        )?,
        arena,
    )
}

/// What a pass over an array literal does with its elements.
enum Pass<'p, 'a> {
    /// Reads the syntax only.
    Count,
    /// Converts each element and discards it.
    Check,
    /// Converts the elements into the slice the counting pass sized.
    Fill(&'p mut [Datum<'a>]),
}

/// How an array literal is malformed: the DETAIL of PostgreSQL's
/// `malformed array literal` error.
#[derive(Clone, Copy)]
enum Malformed {
    MustStart,
    MissingAfterDimensions(&'static str),
    ContentsMustStart,
    Junk,
    Unexpected(u8),
    UnexpectedElement,
    IncorrectlyQuoted,
    EndOfInput,
    BracketWithoutDimension,
    MissingDimensionValue,
    DimensionsDoNotMatch,
    SubArraysDoNotMatch,
}

enum Fault {
    Malformed(Malformed),
    Error(SqlError),
}

impl From<SqlError> for Fault {
    fn from(error: SqlError) -> Self {
        Self::Error(error)
    }
}

impl Fault {
    fn error(self, text: &str) -> SqlError {
        let malformed = match self {
            Self::Error(error) => return error,
            Self::Malformed(malformed) => malformed,
        };
        let error = sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "malformed array literal: \"{}\"",
            text
        );
        let detail = |detail: core::fmt::Arguments<'_>| super::eval::with_detail(error, detail);
        match malformed {
            Malformed::MustStart => detail(format_args!(
                "Array value must start with \"{{\" or dimension information."
            )),
            Malformed::MissingAfterDimensions(what) => {
                detail(format_args!("Missing \"{what}\" after array dimensions."))
            }
            Malformed::ContentsMustStart => {
                detail(format_args!("Array contents must start with \"{{\"."))
            }
            Malformed::Junk => detail(format_args!("Junk after closing right brace.")),
            Malformed::Unexpected(byte) => detail(format_args!(
                "Unexpected \"{}\" character.",
                char::from(byte)
            )),
            Malformed::UnexpectedElement => detail(format_args!("Unexpected array element.")),
            Malformed::IncorrectlyQuoted => {
                detail(format_args!("Incorrectly quoted array element."))
            }
            Malformed::EndOfInput => detail(format_args!("Unexpected end of input.")),
            Malformed::BracketWithoutDimension => detail(format_args!(
                "\"[\" must introduce explicitly-specified array dimensions."
            )),
            Malformed::MissingDimensionValue => {
                detail(format_args!("Missing array dimension value."))
            }
            Malformed::DimensionsDoNotMatch => detail(format_args!(
                "Specified array dimensions do not match array contents."
            )),
            Malformed::SubArraysDoNotMatch => detail(format_args!(
                "Multidimensional arrays must have sub-arrays with matching dimensions."
            )),
        }
    }
}

/// The dimensions and element count of a well-formed literal.
struct Layout {
    dimensions: usize,
    widths: [i32; MAX_DIMENSIONS],
    lower_bounds: [i32; MAX_DIMENSIONS],
    items: usize,
}

/// One token of an array's contents (`ArrayToken`).
enum ArrayToken {
    LevelStart,
    LevelEnd,
    Delimiter,
    /// An element's source between `start` and `end`: inside its quotes,
    /// or unquoted with its trailing whitespace still included.
    Element {
        start: usize,
        end: usize,
        quoted: bool,
        escaped: bool,
    },
}

/// `scanner_isspace`.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

struct Reader<'a> {
    text: &'a [u8],
    at: usize,
    element: ArrElem,
    arena: &'a Arena,
}

impl<'a> Reader<'a> {
    fn new(text: &'a str, element: ArrElem, arena: &'a Arena) -> Self {
        Self {
            text: text.as_bytes(),
            at: 0,
            element,
            arena,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.at += 1;
        }
    }

    fn read(&mut self, pass: &mut Pass<'_, 'a>) -> Result<Layout, Fault> {
        let mut widths = [-1i32; MAX_DIMENSIONS];
        let mut lower_bounds = [1i32; MAX_DIMENSIONS];
        let specified = self.read_dimensions(&mut widths, &mut lower_bounds)?;
        if specified == 0 {
            if self.peek() != Some(b'{') {
                return Err(Fault::Malformed(Malformed::MustStart));
            }
        } else {
            if self.peek() != Some(b'=') {
                return Err(Fault::Malformed(Malformed::MissingAfterDimensions("=")));
            }
            self.at += 1;
            self.skip_spaces();
            if self.peek() != Some(b'{') {
                return Err(Fault::Malformed(Malformed::ContentsMustStart));
            }
        }
        let (dimensions, items) = self.read_contents(specified, &mut widths, pass)?;
        while let Some(byte) = self.peek() {
            self.at += 1;
            if !is_space(byte) {
                return Err(Fault::Malformed(Malformed::Junk));
            }
        }
        Ok(Layout {
            dimensions,
            widths,
            lower_bounds,
            items,
        })
    }

    /// `ReadArrayDimensions`: the `[m:n]` or `[n]` items before the
    /// contents; returns how many there were.
    fn read_dimensions(
        &mut self,
        widths: &mut [i32; MAX_DIMENSIONS],
        lower_bounds: &mut [i32; MAX_DIMENSIONS],
    ) -> Result<usize, Fault> {
        let mut count = 0;
        loop {
            // Whitespace is allowed between dimension items, not within them.
            self.skip_spaces();
            if self.peek() != Some(b'[') {
                return Ok(count);
            }
            self.at += 1;
            if count >= MAX_DIMENSIONS {
                return Err(too_many_dimensions().into());
            }
            let Some(first) = self.read_dimension_integer()? else {
                return Err(Fault::Malformed(Malformed::BracketWithoutDimension));
            };
            let upper = if self.peek() == Some(b':') {
                lower_bounds[count] = first;
                self.at += 1;
                let Some(upper) = self.read_dimension_integer()? else {
                    return Err(Fault::Malformed(Malformed::MissingDimensionValue));
                };
                upper
            } else {
                lower_bounds[count] = 1;
                first
            };
            if self.peek() != Some(b']') {
                return Err(Fault::Malformed(Malformed::MissingAfterDimensions("]")));
            }
            self.at += 1;
            if upper < lower_bounds[count] {
                return Err(sql_err!(
                    sqlstate::ARRAY_SUBSCRIPT_ERROR,
                    "upper bound cannot be less than lower bound"
                )
                .into());
            }
            if upper == i32::MAX {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "array upper bound is too large: {upper}"
                )
                .into());
            }
            widths[count] = upper
                .checked_sub(lower_bounds[count])
                .and_then(|width| width.checked_add(1))
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "array size exceeds the maximum allowed ({})",
                        POSTGRESQL_MAX_ARRAY_SIZE
                    )
                })?;
            count += 1;
        }
    }

    /// `ReadDimensionInt`: an integer read as `strtol` reads it, without
    /// leading whitespace; `None` when no digits follow.
    fn read_dimension_integer(&mut self) -> Result<Option<i32>, Fault> {
        let start = self.at;
        let mut at = start;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            at += 1;
        }
        let digits = at;
        while self.text.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == digits {
            return Ok(None);
        }
        self.at = at;
        // Only ASCII signs and digits were read.
        let number = unsafe { core::str::from_utf8_unchecked(&self.text[start..at]) };
        number.parse::<i32>().map(Some).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "array bound is out of integer range"
            )
            .into()
        })
    }

    /// `ReadArrayStr`: the braced contents, checked against `specified`
    /// dimensions or deducing them into `widths`. Returns the dimension and
    /// element counts.
    fn read_contents(
        &mut self,
        specified: usize,
        widths: &mut [i32; MAX_DIMENSIONS],
        pass: &mut Pass<'_, 'a>,
    ) -> Result<(usize, usize), Fault> {
        let mut dimensions = specified;
        let mut frozen = specified != 0;
        let mut expect_delimiter = false;
        let mut level = 0usize;
        let mut counts = [0i32; MAX_DIMENSIONS];
        let mut items = 0usize;
        let mismatch = Fault::Malformed(if specified != 0 {
            Malformed::DimensionsDoNotMatch
        } else {
            Malformed::SubArraysDoNotMatch
        });
        loop {
            match self.read_token()? {
                ArrayToken::LevelStart => {
                    if expect_delimiter {
                        return Err(Fault::Malformed(Malformed::Unexpected(b'{')));
                    }
                    if level >= MAX_DIMENSIONS {
                        return Err(too_many_dimensions().into());
                    }
                    counts[level] = 0;
                    level += 1;
                    if level > dimensions {
                        if frozen {
                            return Err(mismatch);
                        }
                        dimensions = level;
                    }
                }
                ArrayToken::LevelEnd => {
                    // A right brace may end an empty sub-array; otherwise it
                    // stands where a delimiter is expected.
                    if counts[level - 1] > 0 && !expect_delimiter {
                        return Err(Fault::Malformed(Malformed::Unexpected(b'}')));
                    }
                    level -= 1;
                    if level > 0 {
                        counts[level - 1] += 1;
                    }
                    if widths[level] < 0 {
                        widths[level] = counts[level];
                    } else if counts[level] != widths[level] {
                        return Err(mismatch);
                    }
                    expect_delimiter = true;
                }
                ArrayToken::Delimiter => {
                    if !expect_delimiter {
                        return Err(Fault::Malformed(Malformed::Unexpected(
                            self.element.delimiter(),
                        )));
                    }
                    expect_delimiter = false;
                }
                ArrayToken::Element {
                    start,
                    end,
                    quoted,
                    escaped,
                } => {
                    if expect_delimiter {
                        return Err(Fault::Malformed(Malformed::UnexpectedElement));
                    }
                    if items == MAX_ELEMENTS {
                        return Err(array_too_large().into());
                    }
                    self.convert(start, end, quoted, escaped, items, pass)?;
                    items += 1;
                    // After an element the dimension count is fixed, and
                    // every element is at the same depth.
                    frozen = true;
                    if level != dimensions {
                        return Err(mismatch);
                    }
                    counts[level - 1] += 1;
                    expect_delimiter = true;
                }
            }
            if level == 0 {
                return Ok((dimensions, items));
            }
        }
    }

    /// `ReadArrayToken`.
    fn read_token(&mut self) -> Result<ArrayToken, Fault> {
        let delimiter = self.element.delimiter();
        loop {
            let Some(byte) = self.peek() else {
                return Err(Fault::Malformed(Malformed::EndOfInput));
            };
            match byte {
                b'{' => {
                    self.at += 1;
                    return Ok(ArrayToken::LevelStart);
                }
                b'}' => {
                    self.at += 1;
                    return Ok(ArrayToken::LevelEnd);
                }
                b'"' => {
                    self.at += 1;
                    return self.quoted_element();
                }
                _ if byte == delimiter => {
                    self.at += 1;
                    return Ok(ArrayToken::Delimiter);
                }
                _ if is_space(byte) => self.at += 1,
                _ => return self.unquoted_element(),
            }
        }
    }

    fn quoted_element(&mut self) -> Result<ArrayToken, Fault> {
        let delimiter = self.element.delimiter();
        let start = self.at;
        let mut escaped = false;
        loop {
            match self.peek() {
                None => return Err(Fault::Malformed(Malformed::EndOfInput)),
                Some(b'\\') => {
                    escaped = true;
                    self.at += 1;
                    if self.peek().is_none() {
                        return Err(Fault::Malformed(Malformed::EndOfInput));
                    }
                    self.at += 1;
                }
                Some(b'"') => {
                    let end = self.at;
                    self.at += 1;
                    // Only whitespace may separate the closing quote from a
                    // delimiter or brace.
                    while let Some(byte) = self.peek() {
                        if byte == delimiter || byte == b'}' || byte == b'{' {
                            return Ok(ArrayToken::Element {
                                start,
                                end,
                                quoted: true,
                                escaped,
                            });
                        }
                        if !is_space(byte) {
                            return Err(Fault::Malformed(Malformed::IncorrectlyQuoted));
                        }
                        self.at += 1;
                    }
                    return Err(Fault::Malformed(Malformed::EndOfInput));
                }
                Some(_) => self.at += 1,
            }
        }
    }

    fn unquoted_element(&mut self) -> Result<ArrayToken, Fault> {
        let delimiter = self.element.delimiter();
        let start = self.at;
        let mut escaped = false;
        loop {
            match self.peek() {
                None => return Err(Fault::Malformed(Malformed::EndOfInput)),
                Some(b'{') => return Err(Fault::Malformed(Malformed::Unexpected(b'{'))),
                Some(b'"') => return Err(Fault::Malformed(Malformed::IncorrectlyQuoted)),
                Some(b'\\') => {
                    escaped = true;
                    self.at += 1;
                    if self.peek().is_none() {
                        return Err(Fault::Malformed(Malformed::EndOfInput));
                    }
                    self.at += 1;
                }
                Some(byte) if byte == delimiter || byte == b'}' => {
                    return Ok(ArrayToken::Element {
                        start,
                        end: self.at,
                        quoted: false,
                        escaped,
                    });
                }
                Some(_) => self.at += 1,
            }
        }
    }

    /// Converts the element at `index` whose source is `start..end`.
    fn convert(
        &self,
        start: usize,
        end: usize,
        quoted: bool,
        escaped: bool,
        index: usize,
        pass: &mut Pass<'_, 'a>,
    ) -> Result<(), Fault> {
        let target = match pass {
            Pass::Count => return Ok(()),
            Pass::Check => None,
            Pass::Fill(items) => Some(&mut items[index]),
        };
        let mark = self.arena.mark();
        let datum = match self.element_value(start, end, quoted, escaped)? {
            None => Datum::Null,
            Some(value) => {
                super::eval::cast_to(Datum::Text(value), self.element.to_coltype(), self.arena)?
            }
        };
        match target {
            Some(item) => *item = datum,
            // SAFETY: a checked element keeps no reference into the arena.
            None => unsafe { self.arena.rewind_to(mark) },
        }
        Ok(())
    }

    /// The element's text with its backslash escapes removed and, unquoted,
    /// its trailing unescaped whitespace trimmed; `None` for an unquoted
    /// `NULL`.
    fn element_value(
        &self,
        start: usize,
        end: usize,
        quoted: bool,
        escaped: bool,
    ) -> Result<Option<&'a str>, SqlError> {
        let source = &self.text[start..end];
        let mut kept = source.len();
        if !quoted {
            while kept > 0 && is_space(source[kept - 1]) {
                kept -= 1;
            }
        }
        if !escaped {
            // Element bounds are ASCII bytes of UTF-8 text.
            let value = unsafe { core::str::from_utf8_unchecked(&source[..kept]) };
            return Ok((quoted || !value.eq_ignore_ascii_case("NULL")).then_some(value));
        }
        let output = self
            .arena
            .alloc_slice_with(source.len(), |_| 0u8)
            .map_err(|_| arena_full())?;
        let mut length = 0;
        let mut significant = 0;
        let mut at = 0;
        while at < source.len() {
            let mut byte = source[at];
            let literal = byte == b'\\';
            if literal {
                at += 1;
                byte = source[at];
            }
            output[length] = byte;
            length += 1;
            if quoted || literal || !is_space(byte) {
                significant = length;
            }
            at += 1;
        }
        // Escapes copy whole UTF-8 sequences byte by byte.
        let value = core::str::from_utf8(&output[..significant])
            .expect("an array element of UTF-8 text stays UTF-8");
        Ok(Some(value))
    }
}

/// PostgreSQL's `MaxArraySize`, which its array size errors name.
const POSTGRESQL_MAX_ARRAY_SIZE: usize = 0x3FFF_FFFF / core::mem::size_of::<u64>();

fn too_many_dimensions() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "number of array dimensions exceeds the maximum allowed ({})",
        MAX_DIMENSIONS
    )
}

pub fn write(f: &mut core::fmt::Formatter<'_>, element: ArrElem, raw: &[u8]) -> core::fmt::Result {
    let shape = shape(raw).expect("array datum must carry a canonical valid shape");
    if shape.dimension_count() == 0 {
        return f.write_str("{}");
    }
    if (0..shape.dimension_count()).any(|index| shape.lower_bound(index) != Some(1)) {
        for index in 0..shape.dimension_count() {
            write!(
                f,
                "[{}:{}]",
                shape.lower_bound(index).unwrap(),
                shape.upper_bound(index).unwrap()
            )?;
        }
        f.write_str("=")?;
    }
    let mut elements = elements(raw, element);
    write_level(f, shape, 0, &mut elements)
}

/// Renders already-decoded elements under an existing shape. Catalog-aware
/// output uses this after expanding historical composite layouts; rebuilding
/// the array blob would incorrectly require transient composites to become
/// durable row values again.
pub(crate) fn format_shaped<'a>(
    items: &[Datum],
    shape: Shape,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if items.len() != shape.element_count() {
        return Err(invalid_shape());
    }
    struct Shaped<'a> {
        items: &'a [Datum<'a>],
        shape: Shape,
    }
    impl core::fmt::Display for Shaped<'_> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            if self.shape.dimension_count() == 0 {
                return f.write_str("{}");
            }
            if (0..self.shape.dimension_count())
                .any(|index| self.shape.lower_bound(index) != Some(1))
            {
                for index in 0..self.shape.dimension_count() {
                    write!(
                        f,
                        "[{}:{}]",
                        self.shape.lower_bound(index).unwrap(),
                        self.shape.upper_bound(index).unwrap()
                    )?;
                }
                f.write_str("=")?;
            }
            fn level(
                f: &mut core::fmt::Formatter<'_>,
                shaped: &Shaped<'_>,
                depth: usize,
                index: &mut usize,
            ) -> core::fmt::Result {
                f.write_str("{")?;
                for member in 0..shaped.shape.dimension(depth).unwrap() {
                    if member > 0 {
                        f.write_str(",")?;
                    }
                    if depth + 1 == shaped.shape.dimension_count() {
                        super::types::write_array_elem(f, &shaped.items[*index], b',')?;
                        *index += 1;
                    } else {
                        level(f, shaped, depth + 1, index)?;
                    }
                }
                f.write_str("}")
            }
            let mut index = 0;
            level(f, self, 0, &mut index)
        }
    }
    arena
        .alloc_str_display(Shaped { items, shape })
        .map_err(|_| arena_full())
}

fn write_level(
    f: &mut core::fmt::Formatter<'_>,
    shape: Shape,
    depth: usize,
    elements: &mut Elements<'_>,
) -> core::fmt::Result {
    f.write_str("{")?;
    for member in 0..shape.dimension(depth).unwrap() {
        if member > 0 {
            f.write_str(match elements.element.delimiter() {
                b',' => ",",
                b';' => ";",
                _ => unreachable!("array delimiter is a closed ArrElem property"),
            })?;
        }
        if depth + 1 == shape.dimension_count() {
            let datum = elements.next().expect("array datum carries every member");
            super::types::write_array_elem(f, &datum, elements.element.delimiter())?;
        } else {
            write_level(f, shape, depth + 1, elements)?;
        }
    }
    f.write_str("}")
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    #[test]
    fn quoted_utf8_elements_preserve_their_source_bytes() {
        let mut budget = Budget::new(1 << 16);
        let arena = Arena::new(&mut budget, "array UTF-8", 1 << 12).unwrap();
        let raw = parse_literal("{\"bé\",\"\\\\x\"}", ArrElem::Text, &arena).unwrap();
        assert_eq!(get(raw, ArrElem::Text, 0), Some(Datum::Text("bé")));
        assert_eq!(get(raw, ArrElem::Text, 1), Some(Datum::Text("\\x")));
    }

    #[test]
    fn shape_rejects_overflowing_bounds_before_subscript_evaluation() {
        assert_eq!(
            Shape::new(&[1], &[i32::MAX]).unwrap_err().sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            Shape::new(&[2], &[i32::MAX - 1]).unwrap_err().sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            Shape::new(&[0, 1], &[0, i32::MAX]).unwrap_err().sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            Shape::new(&[1], &[i32::MAX - 1]).unwrap().upper_bound(0),
            Some(i32::MAX - 1)
        );
        assert_eq!(
            Shape::new(&[1], &[i32::MIN]).unwrap().lower_bound(0),
            Some(i32::MIN)
        );
    }

    #[test]
    fn element_limit_matches_the_durable_count_width() {
        assert_eq!(
            Shape::one(MAX_ELEMENTS).unwrap().element_count(),
            u16::MAX as usize
        );
        assert_eq!(
            Shape::one(MAX_ELEMENTS + 1).unwrap_err().sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
    }
}
