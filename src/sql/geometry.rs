//! PostgreSQL geometric text and binary values.

use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::{GeometryKind, PgFloat8};
use crate::sql_err;
use crate::util::StackStr;

const MAX_POINTS: usize = 128;

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

fn push_point(
    values: &mut [f64; MAX_POINTS * 2],
    count: &mut usize,
    point: (f64, f64),
) -> Option<()> {
    if *count + 2 > values.len() {
        return None;
    }
    values[*count] = point.0;
    values[*count + 1] = point.1;
    *count += 2;
    Some(())
}

/// Decodes one complete PostgreSQL geometric text value.  A shape-specific
/// reader keeps punctuation meaningful: accepting the right numbers in the
/// wrong delimiters would make malformed SQL values representable.
fn read_values(
    kind: GeometryKind,
    text: &str,
    values: &mut [f64; MAX_POINTS * 2],
) -> Result<(usize, bool), SqlError> {
    let mut reader = Reader::new(text);
    let mut count = 0;
    let mut add = |point| push_point(values, &mut count, point).ok_or_else(|| bad(kind, text));
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
                let radius = reader
                    .number()
                    .filter(|radius| *radius >= 0.0)
                    .ok_or_else(|| bad(kind, text))?;
                if !reader.take(b'>') {
                    return Err(bad(kind, text));
                }
                values[count] = radius;
                count += 1;
            } else {
                add(reader.point().ok_or_else(|| bad(kind, text))?)?;
                if !reader.take(b',') {
                    return Err(bad(kind, text));
                }
                let radius = reader
                    .number()
                    .filter(|radius| *radius >= 0.0)
                    .ok_or_else(|| bad(kind, text))?;
                values[count] = radius;
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
            one_or_more_points(
                &mut reader,
                closing,
                if kind == GeometryKind::Polygon { 3 } else { 1 },
                &mut add,
            )?;
            opening == b'('
        }
        GeometryKind::Line => {
            if reader.take(b'{') {
                for index in 0..3 {
                    let value = reader.number().ok_or_else(|| bad(kind, text))?;
                    values[count] = value;
                    count += 1;
                    if index != 2 && !reader.take(b',') {
                        return Err(bad(kind, text));
                    }
                }
                if !reader.take(b'}') {
                    return Err(bad(kind, text));
                }
                if values[0] == 0.0 && values[1] == 0.0 {
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
                if !reader.take(b')') || first == second {
                    return Err(bad(kind, text));
                }
                let (a, b, c) = if first.0 == second.0 {
                    (-1.0, 0.0, first.0)
                } else {
                    let a = (second.1 - first.1) / (second.0 - first.0);
                    (a, -1.0, first.1 - a * first.0)
                };
                values[0] = a;
                values[1] = b;
                values[2] = c;
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
    values: &mut [f64; MAX_POINTS * 2],
) -> Result<(usize, bool), SqlError> {
    read_values(kind, text, values)
}

fn point(out: &mut StackStr<2048>, x: f64, y: f64) {
    let _ = write!(out, "({},{})", PgFloat8(x), PgFloat8(y));
}

fn points(out: &mut StackStr<2048>, values: &[f64]) {
    for (index, pair) in values.as_chunks::<2>().0.iter().enumerate() {
        if index != 0 {
            let _ = out.write_str(",");
        }
        point(out, pair[0], pair[1]);
    }
}

/// Parses a PostgreSQL geometric literal and returns its canonical output text.
pub fn parse<'a>(kind: GeometryKind, text: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut values = [0.0; MAX_POINTS * 2];
    let (count, closed) = read_values(kind, text.trim(), &mut values)?;
    let text = text.trim();
    let mut out = StackStr::<2048>::new();
    match kind {
        GeometryKind::Point if count == 2 => point(&mut out, values[0], values[1]),
        GeometryKind::Line if count == 3 => {
            let _ = write!(
                out,
                "{{{},{},{}}}",
                PgFloat8(values[0]),
                PgFloat8(values[1]),
                PgFloat8(values[2])
            );
        }
        GeometryKind::Lseg if count == 4 => {
            let _ = out.write_str("[");
            point(&mut out, values[0], values[1]);
            let _ = out.write_str(",");
            point(&mut out, values[2], values[3]);
            let _ = out.write_str("]");
        }
        GeometryKind::Box if count == 4 => {
            let high_x = values[0].max(values[2]);
            let high_y = values[1].max(values[3]);
            let low_x = values[0].min(values[2]);
            let low_y = values[1].min(values[3]);
            point(&mut out, high_x, high_y);
            let _ = out.write_str(",");
            point(&mut out, low_x, low_y);
        }
        GeometryKind::Circle if count == 3 && values[2] >= 0.0 => {
            let _ = out.write_str("<");
            point(&mut out, values[0], values[1]);
            let _ = write!(out, ",{}>", PgFloat8(values[2]));
        }
        GeometryKind::Path if count >= 2 && count % 2 == 0 => {
            let _ = out.write_str(if closed { "(" } else { "[" });
            points(&mut out, &values[..count]);
            let _ = out.write_str(if closed { ")" } else { "]" });
        }
        GeometryKind::Polygon if count >= 6 && count % 2 == 0 => {
            let _ = out.write_str("(");
            points(&mut out, &values[..count]);
            let _ = out.write_str(")");
        }
        _ => return Err(bad(kind, text)),
    }
    arena.alloc_str(out.as_str()).map_err(|_| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "{} value exceeds the statement arena",
            kind.name()
        )
    })
}

/// Length of the PostgreSQL binary send representation of a canonical value.
pub fn binary_len(kind: GeometryKind, text: &str) -> Result<usize, SqlError> {
    let mut values = [0.0; MAX_POINTS * 2];
    let (count, _) = read_values(kind, text, &mut values)?;
    Ok(match kind {
        GeometryKind::Point => 16,
        GeometryKind::Line | GeometryKind::Circle => 24,
        GeometryKind::Lseg | GeometryKind::Box => 32,
        GeometryKind::Path => 8 + count * 8,
        GeometryKind::Polygon => 4 + count * 8,
    })
}

/// Emits the exact PostgreSQL binary send body for a canonical value.
pub fn emit_binary(
    kind: GeometryKind,
    text: &str,
    mut emit: impl FnMut(&[u8]),
) -> Result<(), SqlError> {
    let mut values = [0.0; MAX_POINTS * 2];
    let (count, closed) = read_values(kind, text, &mut values)?;
    match kind {
        GeometryKind::Path => {
            emit(&[u8::from(closed), 0, 0, 0]);
            emit(&((count / 2) as i32).to_be_bytes());
        }
        GeometryKind::Polygon => emit(&((count / 2) as i32).to_be_bytes()),
        _ => {}
    }
    for value in &values[..count] {
        emit(&value.to_be_bytes());
    }
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
            let header = bytes.get(..8).ok_or_else(bad_binary)?;
            if !matches!(header[0], 0 | 1) || header[1..4] != [0; 3] {
                return Err(bad_binary());
            }
            let points = i32::from_be_bytes(header[4..8].try_into().unwrap());
            if !(1..=MAX_POINTS as i32).contains(&points) || bytes.len() != 8 + points as usize * 16
            {
                return Err(bad_binary());
            }
            (Some(header[0] != 0), &bytes[8..])
        }
        GeometryKind::Polygon => {
            let header = bytes.get(..4).ok_or_else(bad_binary)?;
            let points = i32::from_be_bytes(header.try_into().unwrap());
            if !(3..=MAX_POINTS as i32).contains(&points) || bytes.len() != 4 + points as usize * 16
            {
                return Err(bad_binary());
            }
            (None, &bytes[4..])
        }
        _ => {
            let expected = binary_len(
                kind,
                match kind {
                    GeometryKind::Point => "(0,0)",
                    GeometryKind::Line => "{0,0,0}",
                    GeometryKind::Lseg | GeometryKind::Box => "(0,0),(0,0)",
                    GeometryKind::Circle => "<(0,0),0>",
                    GeometryKind::Path | GeometryKind::Polygon => unreachable!(),
                },
            )?;
            if bytes.len() != expected {
                return Err(bad_binary());
            }
            (None, bytes)
        }
    };
    let mut out = StackStr::<2048>::new();
    let read = |at: usize| f64::from_be_bytes(payload[at..at + 8].try_into().unwrap());
    let finite = |value: f64| value.is_finite();
    if (0..payload.len()).step_by(8).any(|at| !finite(read(at))) {
        return Err(bad_binary());
    }
    match kind {
        GeometryKind::Point => point(&mut out, read(0), read(8)),
        GeometryKind::Line => {
            let _ = write!(
                out,
                "{{{},{},{}}}",
                PgFloat8(read(0)),
                PgFloat8(read(8)),
                PgFloat8(read(16))
            );
        }
        GeometryKind::Lseg => {
            let _ = out.write_str("[");
            point(&mut out, read(0), read(8));
            let _ = out.write_str(",");
            point(&mut out, read(16), read(24));
            let _ = out.write_str("]");
        }
        GeometryKind::Box => {
            point(&mut out, read(0), read(8));
            let _ = out.write_str(",");
            point(&mut out, read(16), read(24));
        }
        GeometryKind::Circle => {
            if read(16) < 0.0 {
                return Err(bad_binary());
            }
            let _ = out.write_str("<");
            point(&mut out, read(0), read(8));
            let _ = write!(out, ",{}>", PgFloat8(read(16)));
        }
        GeometryKind::Path => {
            let _ = out.write_str(if closed == Some(true) { "(" } else { "[" });
            for at in (0..payload.len()).step_by(16) {
                if at != 0 {
                    let _ = out.write_str(",");
                }
                point(&mut out, read(at), read(at + 8));
            }
            let _ = out.write_str(if closed == Some(true) { ")" } else { "]" });
        }
        GeometryKind::Polygon => {
            let _ = out.write_str("(");
            for at in (0..payload.len()).step_by(16) {
                if at != 0 {
                    let _ = out.write_str(",");
                }
                point(&mut out, read(at), read(at + 8));
            }
            let _ = out.write_str(")");
        }
    }
    parse(kind, out.as_str(), arena)
}
