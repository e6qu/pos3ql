//! Array values: a rectangular shape and row-encoded elements.

use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql_err;
use crate::storage::rowenc;

use super::eval::{SqlError, sqlstate};
use super::types::{ArrElem, Datum};

pub const MAX_ELEMENTS: usize = 1024;
pub const MAX_DIMENSIONS: usize = 6;

const MAGIC: [u8; 4] = *b"AR01";
const PREFIX: usize = 7;
pub const EMPTY: &[u8] = b"AR01\x00\x00\x00";

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
            if dimension == 0 {
                return Ok(Self::empty());
            }
            let dimension = u16::try_from(dimension).map_err(|_| array_too_large())?;
            count = count
                .checked_mul(usize::from(dimension))
                .ok_or_else(array_too_large)?;
            if count > MAX_ELEMENTS {
                return Err(array_too_large());
            }
            result.dimensions[index] = dimension;
            result.lower_bounds[index] = lower_bound;
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

/// Serializes a one-dimensional array with PostgreSQL's default lower bound.
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
    let header = PREFIX + shape.dimension_count() * 6;
    let mut total = header;
    for item in items {
        total = total
            .checked_add(4 + rowenc::encoded_len(core::slice::from_ref(item)))
            .ok_or_else(array_too_large)?;
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
        let length = rowenc::encoded_len(core::slice::from_ref(item));
        out[at..at + 4].copy_from_slice(&(length as u32).to_le_bytes());
        at += 4;
        rowenc::encode(core::slice::from_ref(item), &mut out[at..at + length]);
        at += length;
    }
    Ok(out)
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
            let mut out = [Datum::Null; 1];
            rowenc::decode(raw.get(at..at + length)?, &schema, &mut out).ok()?;
            return Some(out[0]);
        }
        at += length;
    }
    None
}

pub fn parse_literal<'a>(
    text: &'a str,
    element: ArrElem,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let mut parser = LiteralParser::new(text.trim(), element, arena);
    let (supplied_bounds, supplied_count) = parser.bounds()?;
    let mut items = [Datum::Null; MAX_ELEMENTS];
    let mut dimensions = [0usize; MAX_DIMENSIONS];
    let levels = parser.level(&mut items, &mut dimensions, 0)?;
    parser.space();
    if parser.at != parser.text.len() {
        return Err(parser.bad());
    }
    if levels == 0 || items[..parser.count].is_empty() {
        return build(&[], arena);
    }
    if supplied_count > 0 {
        if supplied_count != levels {
            return Err(parser.bad());
        }
        for (index, &(lower, upper)) in supplied_bounds[..supplied_count].iter().enumerate() {
            let width = upper
                .checked_sub(lower)
                .and_then(|width| width.checked_add(1))
                .ok_or_else(|| parser.bad())?;
            if usize::try_from(width).ok() != Some(dimensions[index]) {
                return Err(parser.bad());
            }
        }
    }
    let mut lowers = [1i32; MAX_DIMENSIONS];
    for (index, &(lower, _)) in supplied_bounds[..supplied_count].iter().enumerate() {
        lowers[index] = lower;
    }
    build_shaped(
        &items[..parser.count],
        Shape::new(&dimensions[..levels], &lowers[..levels])?,
        arena,
    )
}

struct LiteralParser<'a> {
    text: &'a str,
    at: usize,
    element: ArrElem,
    arena: &'a Arena,
    count: usize,
}

impl<'a> LiteralParser<'a> {
    fn new(text: &'a str, element: ArrElem, arena: &'a Arena) -> Self {
        Self {
            text,
            at: 0,
            element,
            arena,
            count: 0,
        }
    }

    fn bad(&self) -> SqlError {
        sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "malformed array literal: \"{}\"",
            self.text
        )
    }

    fn space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }

    fn bounds(&mut self) -> Result<([(i32, i32); MAX_DIMENSIONS], usize), SqlError> {
        let mut result = [(0, 0); MAX_DIMENSIONS];
        let mut count = 0;
        loop {
            self.space();
            if self.text.as_bytes().get(self.at) != Some(&b'[') {
                break;
            }
            if count == MAX_DIMENSIONS {
                return Err(self.bad());
            }
            self.at += 1;
            let lower = self.number()?;
            if self.text.as_bytes().get(self.at) != Some(&b':') {
                return Err(self.bad());
            }
            self.at += 1;
            let upper = self.number()?;
            if self.text.as_bytes().get(self.at) != Some(&b']') {
                return Err(self.bad());
            }
            self.at += 1;
            result[count] = (lower, upper);
            count += 1;
        }
        if count > 0 {
            if self.text.as_bytes().get(self.at) != Some(&b'=') {
                return Err(self.bad());
            }
            self.at += 1;
        }
        Ok((result, count))
    }

    fn number(&mut self) -> Result<i32, SqlError> {
        self.space();
        let start = self.at;
        if self
            .text
            .as_bytes()
            .get(self.at)
            .is_some_and(|byte| *byte == b'-' || *byte == b'+')
        {
            self.at += 1;
        }
        let digits = self.at;
        while self
            .text
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_digit)
        {
            self.at += 1;
        }
        if self.at == digits {
            return Err(self.bad());
        }
        self.text[start..self.at].parse().map_err(|_| self.bad())
    }

    fn level(
        &mut self,
        items: &mut [Datum<'a>; MAX_ELEMENTS],
        dimensions: &mut [usize; MAX_DIMENSIONS],
        depth: usize,
    ) -> Result<usize, SqlError> {
        if depth == MAX_DIMENSIONS {
            return Err(self.bad());
        }
        self.space();
        if self.text.as_bytes().get(self.at) != Some(&b'{') {
            return Err(self.bad());
        }
        self.at += 1;
        self.space();
        let start_count = self.count;
        let mut width = 0usize;
        let mut child_levels = None;
        if self.text.as_bytes().get(self.at) != Some(&b'}') {
            loop {
                self.space();
                let nested = self.text.as_bytes().get(self.at) == Some(&b'{');
                if nested {
                    let levels = self.level(items, dimensions, depth + 1)?;
                    if child_levels
                        .replace(levels)
                        .is_some_and(|previous| previous != levels)
                    {
                        return Err(self.bad());
                    }
                } else {
                    if child_levels.is_some() {
                        return Err(self.bad());
                    }
                    self.value(items)?;
                }
                width += 1;
                self.space();
                match self.text.as_bytes().get(self.at) {
                    Some(b',') => self.at += 1,
                    Some(b'}') => break,
                    _ => return Err(self.bad()),
                }
            }
        }
        self.at += 1;
        if dimensions[depth] != 0 && dimensions[depth] != width {
            return Err(self.bad());
        }
        dimensions[depth] = width;
        let levels = child_levels.unwrap_or(0) + 1;
        if width == 0 && depth > 0 {
            return Err(self.bad());
        }
        if self.count == start_count && width > 0 && child_levels.is_none() {
            return Err(self.bad());
        }
        Ok(levels)
    }

    fn value(&mut self, items: &mut [Datum<'a>; MAX_ELEMENTS]) -> Result<(), SqlError> {
        if self.count == MAX_ELEMENTS {
            return Err(array_too_large());
        }
        self.space();
        let bytes = self.text.as_bytes();
        let (value, quoted) = if bytes.get(self.at) == Some(&b'\"') {
            self.at += 1;
            let mut output = crate::util::StackStr::<1024>::new();
            loop {
                let Some(&byte) = bytes.get(self.at) else {
                    return Err(self.bad());
                };
                self.at += 1;
                match byte {
                    b'\"' => break,
                    b'\\' => {
                        let Some(&escaped) = bytes.get(self.at) else {
                            return Err(self.bad());
                        };
                        self.at += 1;
                        output.write_char(escaped as char).map_err(|_| self.bad())?;
                    }
                    _ => output.write_char(byte as char).map_err(|_| self.bad())?,
                }
            }
            (
                self.arena
                    .alloc_str(output.as_str())
                    .map_err(|_| arena_full())?,
                true,
            )
        } else {
            let start = self.at;
            while let Some(&byte) = bytes.get(self.at) {
                if byte == b',' || byte == b'}' {
                    break;
                }
                if byte == b'{' {
                    return Err(self.bad());
                }
                self.at += 1;
            }
            (self.text[start..self.at].trim(), false)
        };
        items[self.count] = if !quoted && value.eq_ignore_ascii_case("null") {
            Datum::Null
        } else {
            super::eval::cast_to(Datum::Text(value), self.element.to_coltype(), self.arena)?
        };
        self.count += 1;
        Ok(())
    }
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
    let mut index = 0;
    write_level(f, element, raw, shape, 0, &mut index)
}

fn write_level(
    f: &mut core::fmt::Formatter<'_>,
    element: ArrElem,
    raw: &[u8],
    shape: Shape,
    depth: usize,
    index: &mut usize,
) -> core::fmt::Result {
    f.write_str("{")?;
    for member in 0..shape.dimension(depth).unwrap() {
        if member > 0 {
            f.write_str(",")?;
        }
        if depth + 1 == shape.dimension_count() {
            match get(raw, element, *index) {
                Some(datum) => super::types::write_array_elem(f, &datum)?,
                None => f.write_str("NULL")?,
            }
            *index += 1;
        } else {
            write_level(f, element, raw, shape, depth + 1, index)?;
        }
    }
    f.write_str("}")
}
