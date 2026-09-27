//! PostgreSQL geometric text and binary values.

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::{GeometryKind, PgFloat8};
use crate::sql_err;
pub(crate) const EPSILON: f64 = 1e-6;

pub(crate) fn index_bounds(
    value: crate::sql::types::Datum<'_>,
) -> Result<crate::store::SpatialBounds, SqlError> {
    use crate::sql::types::Datum;
    use crate::store::SpatialBounds;
    let (kind, text) = match value {
        Datum::Null => return Ok(SpatialBounds::Empty),
        Datum::Geometry { kind, text } => (kind, text),
        _ => return Ok(SpatialBounds::Unbounded),
    };
    if !matches!(
        kind,
        GeometryKind::Point | GeometryKind::Box | GeometryKind::Polygon | GeometryKind::Circle
    ) {
        return Ok(SpatialBounds::Unbounded);
    }
    let mut values = [0.0; 3];
    let mut count = 0usize;
    let mut bounds = SpatialBounds::Empty;
    let mut x = None;
    visit_values(kind, text, |value| {
        if !value.is_finite() {
            bounds = SpatialBounds::Unbounded;
        }
        if count < values.len() {
            values[count] = value;
        }
        count += 1;
        if kind != GeometryKind::Circle {
            if let Some(x) = x.take() {
                bounds = bounds.union(SpatialBounds::new(x, value, x, value));
            } else {
                x = Some(value);
            }
        }
        Ok(())
    })?;
    if matches!(bounds, SpatialBounds::Unbounded) {
        return Ok(bounds);
    }
    if kind == GeometryKind::Circle {
        let radius = values[2].abs();
        bounds = SpatialBounds::new(
            values[0] - radius,
            values[1] - radius,
            values[0] + radius,
            values[1] + radius,
        );
    }
    Ok(bounds)
}

/// A node encloses every descendant key. These tests may retain false
/// positives, never discard a key accepted by PostgreSQL's fuzzy geometry.
pub(crate) fn index_bounds_intersect(
    bounds: crate::store::SpatialBounds,
    search: crate::store::SpatialBounds,
    operator: crate::sql::ast::BinaryOp,
) -> bool {
    use crate::sql::ast::BinaryOp;
    use crate::store::SpatialBounds;
    match (bounds, search) {
        (SpatialBounds::Empty, _) | (_, SpatialBounds::Empty) => false,
        (SpatialBounds::Finite(bounds), SpatialBounds::Finite(search)) => {
            let [minimum_x, minimum_y, maximum_x, maximum_y] = bounds.coordinates();
            let [x, y, end_x, end_y] = search.coordinates();
            let left_limit = (x + EPSILON).next_up();
            let right_limit = (end_x - EPSILON).next_down();
            let below_limit = (y + EPSILON).next_up();
            let above_limit = (end_y - EPSILON).next_down();
            let x = (x - EPSILON).next_down();
            let y = (y - EPSILON).next_down();
            let end_x = (end_x + EPSILON).next_up();
            let end_y = (end_y + EPSILON).next_up();
            match operator {
                BinaryOp::Shl => minimum_x <= left_limit,
                BinaryOp::Shr => maximum_x >= right_limit,
                BinaryOp::NotRightOf => minimum_x <= end_x,
                BinaryOp::NotLeftOf => maximum_x >= x,
                BinaryOp::Below | BinaryOp::BelowPoint => minimum_y <= below_limit,
                BinaryOp::Above | BinaryOp::AbovePoint => maximum_y >= above_limit,
                BinaryOp::NotAbove => minimum_y <= end_y,
                BinaryOp::NotBelow => maximum_y >= y,
                BinaryOp::Same
                | BinaryOp::Contains
                | BinaryOp::ContainedBy
                | BinaryOp::Overlaps => {
                    minimum_x <= end_x && maximum_x >= x && minimum_y <= end_y && maximum_y >= y
                }
                _ => true,
            }
        }
        _ => true,
    }
}

/// Conservative distance from a point probe to every geometry enclosed by a
/// navigation summary. It may underestimate PostgreSQL's exact operator but
/// must never overestimate it: ranked traversal prunes a subtree only after
/// this lower bound is worse than the current top-k radius.
pub(crate) fn index_distance_lower_bound(
    bounds: crate::store::SpatialBounds,
    origin: crate::store::SpatialBounds,
) -> f64 {
    use crate::store::SpatialBounds;
    let (SpatialBounds::Finite(bounds), SpatialBounds::Finite(origin)) = (bounds, origin) else {
        return if matches!(bounds, SpatialBounds::Empty) {
            f64::INFINITY
        } else {
            0.0
        };
    };
    let [minimum_x, minimum_y, maximum_x, maximum_y] = bounds.coordinates();
    let [x, y, end_x, end_y] = origin.coordinates();
    if x != end_x || y != end_y {
        return 0.0;
    }
    let dx = if x < minimum_x {
        minimum_x - x
    } else if x > maximum_x {
        x - maximum_x
    } else {
        0.0
    };
    let dy = if y < minimum_y {
        minimum_y - y
    } else if y > maximum_y {
        y - maximum_y
    } else {
        0.0
    };
    // Geometric comparisons use PostgreSQL's fuzzy epsilon. Widen the bound
    // by that tolerance and one representable step before using it to prune.
    (dx.hypot(dy) - EPSILON).max(0.0).next_down()
}

fn fp_zero(value: f64) -> bool {
    value.abs() <= EPSILON
}

fn pg_gt(left: f64, right: f64) -> bool {
    match (left.is_nan(), right.is_nan()) {
        (true, false) => true,
        (false, true) | (true, true) => false,
        (false, false) => left > right,
    }
}

fn pg_max(left: f64, right: f64) -> f64 {
    if pg_gt(left, right) { left } else { right }
}

fn pg_min(left: f64, right: f64) -> f64 {
    if pg_gt(left, right) { right } else { left }
}

fn bad(kind: GeometryKind, text: &str) -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type {}: \"{}\"",
        kind.name(),
        text
    )
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, at: 0 }
    }

    fn skip_space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        self.skip_space();
        if self.text.as_bytes().get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn number(&mut self) -> Option<f64> {
        self.skip_space();
        let bytes = self.text.as_bytes();
        let start = self.at;
        if matches!(bytes.get(self.at), Some(b'+' | b'-')) {
            self.at += 1;
        }
        let unsigned = self.at;
        for (word, magnitude) in [
            ("infinity", f64::INFINITY),
            ("inf", f64::INFINITY),
            ("nan", f64::NAN),
        ] {
            let end = unsigned + word.len();
            if self
                .text
                .get(unsigned..end)
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(word))
            {
                self.at = end;
                return Some(if bytes.get(start) == Some(&b'-') {
                    -magnitude
                } else {
                    magnitude
                });
            }
        }
        let digit_start = self.at;
        while matches!(bytes.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        let mut digits = self.at != digit_start;
        if bytes.get(self.at) == Some(&b'.') {
            self.at += 1;
            let fraction_start = self.at;
            while matches!(bytes.get(self.at), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            digits |= self.at != fraction_start;
        }
        if !digits {
            self.at = start;
            return None;
        }
        if matches!(bytes.get(self.at), Some(b'e' | b'E')) {
            let exponent = self.at;
            self.at += 1;
            if matches!(bytes.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            let exponent_digits = self.at;
            while matches!(bytes.get(self.at), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            if self.at == exponent_digits {
                self.at = exponent;
            }
        }
        let value = self.text[start..self.at].parse::<f64>().ok()?;
        value.is_finite().then_some(value)
    }

    fn point(&mut self) -> Option<(f64, f64)> {
        self.take(b'(').then_some(())?;
        let x = self.number()?;
        self.take(b',').then_some(())?;
        let y = self.number()?;
        self.take(b')').then_some(())?;
        Some((x, y))
    }

    fn done(&mut self) -> bool {
        self.skip_space();
        self.at == self.text.len()
    }
}

/// Decodes one complete PostgreSQL geometric text value.  A shape-specific
/// reader keeps punctuation meaningful: accepting the right numbers in the
/// wrong delimiters would make malformed SQL values representable.
fn visit_values(
    kind: GeometryKind,
    text: &str,
    mut visit: impl FnMut(f64) -> Result<(), SqlError>,
) -> Result<(usize, bool), SqlError> {
    let mut reader = Reader::new(text);
    let mut count = 0;
    let mut add = |point: (f64, f64)| {
        visit(point.0)?;
        visit(point.1)?;
        count += 2;
        Ok(())
    };
    let one_or_more_points = |reader: &mut Reader<'_>,
                              closing: u8,
                              min: usize,
                              add: &mut dyn FnMut((f64, f64)) -> Result<(), SqlError>|
     -> Result<(), SqlError> {
        let mut points = 0;
        loop {
            add(reader.point().ok_or_else(|| bad(kind, text))?)?;
            points += 1;
            if reader.take(closing) {
                return (points >= min && reader.done())
                    .then_some(())
                    .ok_or_else(|| bad(kind, text));
            }
            if !reader.take(b',') {
                return Err(bad(kind, text));
            }
        }
    };
    let closed = match kind {
        GeometryKind::Point => {
            let at = reader.at;
            if let Some(point) = reader.point() {
                add(point)?;
            } else {
                reader.at = at;
                let x = reader.number().ok_or_else(|| bad(kind, text))?;
                if !reader.take(b',') {
                    return Err(bad(kind, text));
                }
                let y = reader.number().ok_or_else(|| bad(kind, text))?;
                add((x, y))?;
            }
            false
        }
        GeometryKind::Lseg => {
            let closing = if reader.take(b'[') {
                b']'
            } else if reader.take(b'(') {
                b')'
            } else {
                return Err(bad(kind, text));
            };
            add(reader.point().ok_or_else(|| bad(kind, text))?)?;
            if !reader.take(b',') {
                return Err(bad(kind, text));
            }
            add(reader.point().ok_or_else(|| bad(kind, text))?)?;
            if !reader.take(closing) {
                return Err(bad(kind, text));
            }
            false
        }
        GeometryKind::Box => {
            reader.skip_space();
            let start = reader.at;
            let outer = reader.take(b'(') && {
                reader.skip_space();
                reader.text.as_bytes().get(reader.at) == Some(&b'(')
            };
            if !outer {
                reader.at = start;
            }
            add(reader.point().ok_or_else(|| bad(kind, text))?)?;
            if !reader.take(b',') {
                return Err(bad(kind, text));
            }
            add(reader.point().ok_or_else(|| bad(kind, text))?)?;
            if outer && !reader.take(b')') {
                return Err(bad(kind, text));
            }
            false
        }
        GeometryKind::Circle => {
            if reader.take(b'<') {
                add(reader.point().ok_or_else(|| bad(kind, text))?)?;
                if !reader.take(b',') {
                    return Err(bad(kind, text));
                }
                let radius = reader.number().ok_or_else(|| bad(kind, text))?;
                if !reader.take(b'>') {
                    return Err(bad(kind, text));
                }
                visit(radius)?;
                count += 1;
            } else {
                add(reader.point().ok_or_else(|| bad(kind, text))?)?;
                if !reader.take(b',') {
                    return Err(bad(kind, text));
                }
                let radius = reader.number().ok_or_else(|| bad(kind, text))?;
                visit(radius)?;
                count += 1;
            }
            false
        }
        GeometryKind::Path | GeometryKind::Polygon => {
            let opening = if reader.take(b'[') {
                b'['
            } else if reader.take(b'(') {
                b'('
            } else {
                return Err(bad(kind, text));
            };
            // PostgreSQL paths may be open or closed, while polygons are
            // always written with a closed-point-list delimiter.
            if kind == GeometryKind::Polygon && opening != b'(' {
                return Err(bad(kind, text));
            }
            let closing = if opening == b'[' { b']' } else { b')' };
            one_or_more_points(&mut reader, closing, 1, &mut add)?;
            opening == b'('
        }
        GeometryKind::Line => {
            if reader.take(b'{') {
                for index in 0..3 {
                    let value = reader.number().ok_or_else(|| bad(kind, text))?;
                    visit(value)?;
                    count += 1;
                    if index != 2 && !reader.take(b',') {
                        return Err(bad(kind, text));
                    }
                }
                if !reader.take(b'}') {
                    return Err(bad(kind, text));
                }
            } else {
                if !reader.take(b'(') {
                    return Err(bad(kind, text));
                }
                let first = reader.point().ok_or_else(|| bad(kind, text))?;
                if !reader.take(b',') {
                    return Err(bad(kind, text));
                }
                let second = reader.point().ok_or_else(|| bad(kind, text))?;
                if !reader.take(b')')
                    || ((first.0 - second.0).abs() <= EPSILON
                        && (first.1 - second.1).abs() <= EPSILON)
                {
                    return Err(bad(kind, text));
                }
                let (a, b, c) = if (first.0 - second.0).abs() <= EPSILON {
                    (-1.0, 0.0, first.0)
                } else {
                    let a = (second.1 - first.1) / (second.0 - first.0);
                    (a, -1.0, first.1 - a * first.0)
                };
                visit(a)?;
                visit(b)?;
                visit(c)?;
                count = 3;
            }
            false
        }
    };
    reader
        .done()
        .then_some((count, closed))
        .ok_or_else(|| bad(kind, text))
}

pub(crate) fn components(
    kind: GeometryKind,
    text: &str,
    values: &mut [f64],
) -> Result<(usize, bool), SqlError> {
    let mut count = 0usize;
    let result = visit_values(kind, text, |value| {
        let Some(output) = values.get_mut(count) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "{} value exceeds the statement arena",
                kind.name()
            ));
        };
        *output = value;
        count += 1;
        Ok(())
    })?;
    debug_assert_eq!(count, result.0);
    Ok(result)
}

pub(crate) fn component_count(kind: GeometryKind, text: &str) -> Result<(usize, bool), SqlError> {
    visit_values(kind, text, |_| Ok(()))
}

fn value_too_wide(kind: GeometryKind) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "{} value exceeds the statement arena",
        kind.name()
    )
}

struct GeometryText<'a> {
    bytes: &'a mut [u8],
    len: usize,
}

impl<'a> GeometryText<'a> {
    fn new(arena: &'a Arena, capacity: usize, kind: GeometryKind) -> Result<Self, SqlError> {
        let bytes = arena
            .alloc_slice_with(capacity, |_| 0)
            .map_err(|_| value_too_wide(kind))?;
        Ok(Self { bytes, len: 0 })
    }

    fn finish(self) -> &'a str {
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

impl core::fmt::Write for GeometryText<'_> {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        let end = self.len.checked_add(value.len()).ok_or(core::fmt::Error)?;
        let output = self.bytes.get_mut(self.len..end).ok_or(core::fmt::Error)?;
        output.copy_from_slice(value.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[derive(Default)]
struct GeometryLength(usize);

impl core::fmt::Write for GeometryLength {
    fn write_str(&mut self, value: &str) -> core::fmt::Result {
        self.0 = self.0.checked_add(value.len()).ok_or(core::fmt::Error)?;
        Ok(())
    }
}

fn point(out: &mut impl core::fmt::Write, x: f64, y: f64) -> core::fmt::Result {
    write!(out, "({},{})", PgFloat8(x), PgFloat8(y))
}

fn points(out: &mut impl core::fmt::Write, values: &[f64]) -> core::fmt::Result {
    for (index, pair) in values.as_chunks::<2>().0.iter().enumerate() {
        if index != 0 {
            out.write_str(",")?;
        }
        point(out, pair[0], pair[1])?;
    }
    Ok(())
}

fn render_to(
    kind: GeometryKind,
    values: &[f64],
    closed: bool,
    out: &mut impl core::fmt::Write,
) -> core::fmt::Result {
    match kind {
        GeometryKind::Point => point(out, values[0], values[1])?,
        GeometryKind::Line => write!(
            out,
            "{{{},{},{}}}",
            PgFloat8(values[0]),
            PgFloat8(values[1]),
            PgFloat8(values[2])
        )?,
        GeometryKind::Lseg => {
            out.write_str("[")?;
            point(out, values[0], values[1])?;
            out.write_str(",")?;
            point(out, values[2], values[3])?;
            out.write_str("]")?;
        }
        GeometryKind::Box => {
            let high_x = pg_max(values[0], values[2]);
            let high_y = pg_max(values[1], values[3]);
            let low_x = pg_min(values[0], values[2]);
            let low_y = pg_min(values[1], values[3]);
            point(out, high_x, high_y)?;
            out.write_str(",")?;
            point(out, low_x, low_y)?;
        }
        GeometryKind::Circle => {
            out.write_str("<")?;
            point(out, values[0], values[1])?;
            write!(out, ",{}>", PgFloat8(values[2]))?;
        }
        GeometryKind::Path => {
            out.write_str(if closed { "(" } else { "[" })?;
            points(out, values)?;
            out.write_str(if closed { ")" } else { "]" })?;
        }
        GeometryKind::Polygon => {
            out.write_str("(")?;
            points(out, values)?;
            out.write_str(")")?;
        }
    }
    Ok(())
}

/// Parses a PostgreSQL geometric literal and returns its canonical output text.
pub fn parse<'a>(kind: GeometryKind, text: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let text = text.trim();
    let (count, closed) = component_count(kind, text)?;
    let values = arena.alloc_slice_with(count, |_| 0.0).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "{} value exceeds the statement arena",
            kind.name()
        )
    })?;
    components(kind, text, values)?;
    if kind == GeometryKind::Line && fp_zero(values[0]) && fp_zero(values[1]) {
        return Err(bad(kind, text));
    }
    render(kind, values, closed, arena)
}

/// Renders already validated components without imposing another point or
/// text-width boundary. Callers use this for geometric operators that create
/// a new value from an existing canonical value.
pub(crate) fn render<'a>(
    kind: GeometryKind,
    values: &[f64],
    closed: bool,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let count = values.len();
    let valid = match kind {
        GeometryKind::Point => count == 2,
        GeometryKind::Line | GeometryKind::Circle => count == 3,
        GeometryKind::Lseg | GeometryKind::Box => count == 4,
        GeometryKind::Path | GeometryKind::Polygon => count >= 2 && count.is_multiple_of(2),
    } && (kind != GeometryKind::Circle
        || values[2].partial_cmp(&0.0) != Some(core::cmp::Ordering::Less));
    if !valid {
        return Err(bad(kind, ""));
    }
    let mut length = GeometryLength::default();
    render_to(kind, values, closed, &mut length).map_err(|_| value_too_wide(kind))?;
    let mut out = GeometryText::new(arena, length.0, kind)?;
    render_to(kind, values, closed, &mut out).map_err(|_| value_too_wide(kind))?;
    debug_assert_eq!(out.len, length.0);
    Ok(out.finish())
}

/// Length of the PostgreSQL binary send representation of a canonical value.
pub fn binary_len(kind: GeometryKind, text: &str) -> Result<usize, SqlError> {
    let (count, _) = component_count(kind, text)?;
    match kind {
        GeometryKind::Point => Ok(16),
        GeometryKind::Line | GeometryKind::Circle => Ok(24),
        GeometryKind::Lseg | GeometryKind::Box => Ok(32),
        GeometryKind::Path | GeometryKind::Polygon => count
            .checked_mul(8)
            .and_then(|bytes| bytes.checked_add(if kind == GeometryKind::Path { 5 } else { 4 }))
            .ok_or_else(|| value_too_wide(kind)),
    }
}

/// Emits the exact PostgreSQL binary send body for a canonical value.
pub fn emit_binary(
    kind: GeometryKind,
    text: &str,
    mut emit: impl FnMut(&[u8]),
) -> Result<(), SqlError> {
    let (count, closed) = component_count(kind, text)?;
    let points = i32::try_from(count / 2).map_err(|_| value_too_wide(kind))?;
    match kind {
        GeometryKind::Path => {
            emit(&[u8::from(closed)]);
            emit(&points.to_be_bytes());
        }
        GeometryKind::Polygon => emit(&points.to_be_bytes()),
        _ => {}
    }
    visit_values(kind, text, |value| {
        emit(&value.to_be_bytes());
        Ok(())
    })?;
    Ok(())
}

/// Decodes a PostgreSQL binary receive body through the same canonical text
/// boundary as SQL and text Bind input.
pub fn decode_binary<'a>(
    kind: GeometryKind,
    bytes: &[u8],
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let bad_binary = || {
        sql_err!(
            sqlstate::INVALID_BINARY_REPRESENTATION,
            "invalid binary representation for type {}",
            kind.name()
        )
    };
    let (closed, payload) = match kind {
        GeometryKind::Path => {
            let header = bytes.get(..5).ok_or_else(bad_binary)?;
            if !matches!(header[0], 0 | 1) {
                return Err(bad_binary());
            }
            let points = i32::from_be_bytes(header[1..5].try_into().unwrap());
            let expected = usize::try_from(points)
                .ok()
                .filter(|points| *points > 0)
                .and_then(|points| points.checked_mul(16))
                .and_then(|payload| payload.checked_add(5));
            if expected != Some(bytes.len()) {
                return Err(bad_binary());
            }
            (Some(header[0] != 0), &bytes[5..])
        }
        GeometryKind::Polygon => {
            let header = bytes.get(..4).ok_or_else(bad_binary)?;
            let points = i32::from_be_bytes(header.try_into().unwrap());
            let expected = usize::try_from(points)
                .ok()
                .filter(|points| *points > 0)
                .and_then(|points| points.checked_mul(16))
                .and_then(|payload| payload.checked_add(4));
            if expected != Some(bytes.len()) {
                return Err(bad_binary());
            }
            (None, &bytes[4..])
        }
        _ => {
            let expected = match kind {
                GeometryKind::Point => 16,
                GeometryKind::Line | GeometryKind::Circle => 24,
                GeometryKind::Lseg | GeometryKind::Box => 32,
                GeometryKind::Path | GeometryKind::Polygon => unreachable!(),
            };
            if bytes.len() != expected {
                return Err(bad_binary());
            }
            (None, bytes)
        }
    };
    let values = arena
        .alloc_slice_with(payload.len() / 8, |_| 0.0)
        .map_err(|_| value_too_wide(kind))?;
    for (output, bytes) in values.iter_mut().zip(payload.as_chunks::<8>().0) {
        *output = f64::from_be_bytes(*bytes);
    }
    if (kind == GeometryKind::Line && fp_zero(values[0]) && fp_zero(values[1]))
        || (kind == GeometryKind::Circle && values[2] < 0.0)
    {
        return Err(bad_binary());
    }
    render(kind, values, closed.unwrap_or(false), arena)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinaryOp;
    use crate::sql::types::Datum;

    #[test]
    fn wide_polygon_text_binary_and_bounds_use_statement_memory() {
        use core::fmt::Write;

        let mut source = String::from("(");
        for point in 0..300 {
            if point != 0 {
                source.push(',');
            }
            write!(source, "({point},{})", point % 7).unwrap();
        }
        source.push(')');

        let mut budget = crate::mem::Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "wide geometry", 1 << 19).unwrap();
        let canonical = crate::mem::guard::forbid_alloc(|| {
            parse(GeometryKind::Polygon, &source, &arena).unwrap()
        });
        assert_eq!(
            component_count(GeometryKind::Polygon, canonical).unwrap().0,
            600
        );
        assert!(matches!(
            index_bounds(Datum::Geometry {
                kind: GeometryKind::Polygon,
                text: canonical,
            })
            .unwrap(),
            crate::store::SpatialBounds::Finite(_)
        ));

        let length = binary_len(GeometryKind::Polygon, canonical).unwrap();
        assert_eq!(length, 4 + 300 * 16);
        let mut binary = Vec::with_capacity(length);
        crate::mem::guard::forbid_alloc(|| {
            emit_binary(GeometryKind::Polygon, canonical, |bytes| {
                binary.extend_from_slice(bytes);
            })
            .unwrap();
        });
        assert_eq!(binary.len(), length);

        let mut decode_budget = crate::mem::Budget::new(1 << 20);
        let decode_arena = Arena::new(&mut decode_budget, "wide geometry decode", 1 << 19).unwrap();
        let decoded = crate::mem::guard::forbid_alloc(|| {
            decode_binary(GeometryKind::Polygon, &binary, &decode_arena).unwrap()
        });
        assert_eq!(decoded, canonical);
    }

    #[test]
    fn navigation_bounds_never_exclude_exact_geometric_operator_matches() {
        use BinaryOp::*;
        use GeometryKind::*;
        let shapes = [
            (Point, "(0,0)"),
            (Point, "(-0.0000005,0)"),
            (Point, "(0.0000005,0)"),
            (Point, "(0,-0.0000005)"),
            (Point, "(0,0.0000005)"),
            (Point, "(-0,-0)"),
            (Point, "(Infinity,0)"),
            (Point, "(NaN,0)"),
            (Box, "(1,1),(0,0)"),
            (Box, "(0,0),(-1,-1)"),
            (Box, "(0.0000005,0.0000005),(-0.0000005,-0.0000005)"),
            (Polygon, "((0,0),(1,0),(0,1))"),
            (
                Polygon,
                "((-0.0000005,-0.0000005),(0.0000005,-0.0000005),(0,0.0000005))",
            ),
            (Circle, "<(0,0),1>"),
            (Circle, "<(1,0),1>"),
            (Circle, "<(0,0),0>"),
        ];
        let arena = Arena::new(
            &mut crate::mem::Budget::new(1 << 20),
            "geometry navigation",
            1 << 20,
        )
        .unwrap();
        let mut matches = 0;
        crate::mem::guard::forbid_alloc(|| {
            for &(left_kind, left_text) in &shapes {
                for &(right_kind, right_text) in &shapes {
                    let left = Datum::Geometry {
                        kind: left_kind,
                        text: left_text,
                    };
                    let right = Datum::Geometry {
                        kind: right_kind,
                        text: right_text,
                    };
                    for operator in [
                        Same,
                        Contains,
                        ContainedBy,
                        Overlaps,
                        Shl,
                        Shr,
                        NotRightOf,
                        NotLeftOf,
                        Below,
                        Above,
                        BelowPoint,
                        AbovePoint,
                        NotAbove,
                        NotBelow,
                    ] {
                        let result = crate::sql::eval::funcs::geometry::operator(
                            operator.operator_name().unwrap(),
                            &[left, right],
                            &[left_kind.oid(), right_kind.oid()],
                            &arena,
                        );
                        if result.is_some_and(|result| matches!(result.unwrap(), Datum::Bool(true)))
                        {
                            matches += 1;
                            let bounds = index_bounds(left).unwrap();
                            let search = index_bounds(right).unwrap();
                            assert!(
                                index_bounds_intersect(bounds, search, operator),
                                "{left_kind:?} {left_text} {operator:?} {right_kind:?} {right_text}"
                            );
                            assert!(index_bounds_intersect(
                                bounds.union(crate::store::SpatialBounds::new(
                                    10000.0, 10000.0, 10001.0, 10001.0
                                )),
                                search,
                                operator
                            ));
                        }
                    }
                }
            }
        });
        assert_eq!(matches, 256, "exercise the accepted geometric matrix");
        assert_eq!(
            index_bounds(Datum::Null).unwrap(),
            crate::store::SpatialBounds::Empty
        );
        assert!(
            index_bounds(Datum::Geometry {
                kind: Point,
                text: "garbage"
            })
            .is_err()
        );
    }

    #[test]
    fn ranked_navigation_bounds_never_exceed_exact_distance() {
        use GeometryKind::*;
        let shapes = [
            (Point, "(4,5)"),
            (Box, "(7,8),(4,5)"),
            (Polygon, "((4,5),(7,5),(4,8))"),
            (Circle, "<(5,6),1>"),
        ];
        let origins = ["(0,0)", "(5,6)", "(20,-3)"];
        let arena = Arena::new(
            &mut crate::mem::Budget::new(1 << 20),
            "ranked geometry navigation",
            1 << 20,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for &(kind, text) in &shapes {
                let shape = Datum::Geometry { kind, text };
                for &origin_text in &origins {
                    let origin = Datum::Geometry {
                        kind: Point,
                        text: origin_text,
                    };
                    let Datum::Float8(exact) = crate::sql::eval::funcs::geometry::operator(
                        "<->",
                        &[shape, origin],
                        &[kind.oid(), Point.oid()],
                        &arena,
                    )
                    .unwrap()
                    .unwrap() else {
                        panic!("geometric distance must be double precision");
                    };
                    let lower = index_distance_lower_bound(
                        index_bounds(shape).unwrap(),
                        index_bounds(origin).unwrap(),
                    );
                    assert!(
                        lower <= exact,
                        "{kind:?} {text} from {origin_text}: {lower} > {exact}"
                    );
                }
            }
        });
        assert_eq!(
            index_distance_lower_bound(
                crate::store::SpatialBounds::Empty,
                crate::store::SpatialBounds::new(0.0, 0.0, 0.0, 0.0),
            ),
            f64::INFINITY
        );
        assert_eq!(
            index_distance_lower_bound(
                crate::store::SpatialBounds::Unbounded,
                crate::store::SpatialBounds::new(0.0, 0.0, 0.0, 0.0),
            ),
            0.0
        );
    }
}
