//! One-dimensional array (de)serialization.
//!
//! An array value is stored as its own byte blob — `u16 count` followed by,
//! per element, a `u32 length` and that element's ordinary row encoding
//! (`rowenc`). Keeping the elements as bytes means decoding an array from
//! storage needs no separate allocation: `Datum::Array` just borrows the blob,
//! and elements are decoded on demand (for output, subscript, and `ANY`).

use crate::mem::arena::Arena;
use crate::sql_err;
use crate::storage::rowenc;

use super::eval::{sqlstate, SqlError};
use super::types::{ArrElem, Datum};

fn arena_full() -> SqlError {
    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array value exceeds the statement arena")
}

/// Serializes `items` into the array blob form.
pub fn build<'a>(items: &[Datum], arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let mut total = 2usize;
    for it in items {
        total += 4 + rowenc::encoded_len(core::slice::from_ref(it));
    }
    let out = arena.alloc_slice_with(total, |_| 0u8).map_err(|_| arena_full())?;
    out[0..2].copy_from_slice(&(items.len() as u16).to_le_bytes());
    let mut at = 2;
    for it in items {
        let n = rowenc::encoded_len(core::slice::from_ref(it));
        out[at..at + 4].copy_from_slice(&(n as u32).to_le_bytes());
        at += 4;
        rowenc::encode(core::slice::from_ref(it), &mut out[at..at + n]);
        at += n;
    }
    Ok(&*out)
}

pub fn len(raw: &[u8]) -> usize {
    if raw.len() < 2 {
        0
    } else {
        u16::from_le_bytes([raw[0], raw[1]]) as usize
    }
}

/// Decodes the `index`-th element (0-based) of the blob.
pub fn get<'a>(raw: &'a [u8], element: ArrElem, index: usize) -> Option<Datum<'a>> {
    let n = len(raw);
    if index >= n {
        return None;
    }
    let schema = [element.to_coltype()];
    let mut at = 2;
    for i in 0..n {
        let l = u32::from_le_bytes(raw.get(at..at + 4)?.try_into().ok()?) as usize;
        at += 4;
        if i == index {
            let mut out = [Datum::Null; 1];
            rowenc::decode(raw.get(at..at + l)?, &schema, &mut out).ok()?;
            return Some(out[0]);
        }
        at += l;
    }
    None
}

/// Parses a `{a,b,c}` array literal, coercing each element to `element`.
pub fn parse_literal<'a>(
    text: &'a str,
    element: ArrElem,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let bad = || sql_err!(sqlstate::INVALID_TEXT_REPRESENTATION, "malformed array literal: \"{}\"", text);
    let t = text.trim();
    let inner = t
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .ok_or_else(bad)?;
    let mut items = [Datum::Null; 1024];
    let mut n = 0;
    let ct = element.to_coltype();
    if !inner.trim().is_empty() {
        let b = inner.as_bytes();
        let mut i = 0;
        loop {
            if n == items.len() {
                return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "array literal too large"));
            }
            // Skip leading whitespace.
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            let (field, is_quoted, next) = read_field(inner, i, arena)?;
            i = next;
            let d = if !is_quoted && field.eq_ignore_ascii_case("null") {
                Datum::Null
            } else {
                super::eval::cast_to(Datum::Text(field), ct, arena)?
            };
            items[n] = d;
            n += 1;
            // Skip trailing whitespace, then expect ',' or end.
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            match b.get(i) {
                None => break,
                Some(b',') => i += 1,
                _ => return Err(bad()),
            }
        }
    }
    build(&items[..n], arena)
}

/// Reads one element field starting at byte `i` of `inner`, returning
/// (unquoted-text, was_quoted, next-index).
fn read_field<'a>(
    inner: &'a str,
    i: usize,
    arena: &'a Arena,
) -> Result<(&'a str, bool, usize), SqlError> {
    let b = inner.as_bytes();
    if b.get(i) == Some(&b'"') {
        // Quoted: gather with \" and \\ unescaping into the arena.
        let mut buffer = crate::util::StackStr::<1024>::new();
        let mut j = i + 1;
        while j < b.len() {
            match b[j] {
                b'"' => {
                    let s = arena
                        .alloc_str(buffer.as_str())
                        .map_err(|_| arena_full())?;
                    return Ok((s, true, j + 1));
                }
                b'\\' if j + 1 < b.len() => {
                    let _ = core::fmt::Write::write_char(&mut buffer, b[j + 1] as char);
                    j += 2;
                }
                c => {
                    let _ = core::fmt::Write::write_char(&mut buffer, c as char);
                    j += 1;
                }
            }
        }
        Err(sql_err!(sqlstate::INVALID_TEXT_REPRESENTATION, "unterminated array element"))
    } else {
        // Unquoted: up to the next comma or closing brace.
        let start = i;
        let mut j = i;
        while j < b.len() && b[j] != b',' {
            j += 1;
        }
        Ok((inner[start..j].trim(), false, j))
    }
}

/// Renders the array in PostgreSQL's `{a,b,c}` text form.
pub fn write(f: &mut core::fmt::Formatter<'_>, element: ArrElem, raw: &[u8]) -> core::fmt::Result {
    f.write_str("{")?;
    for i in 0..len(raw) {
        if i > 0 {
            f.write_str(",")?;
        }
        match get(raw, element, i) {
            Some(d) => super::types::write_array_elem(f, &d)?,
            None => f.write_str("NULL")?,
        }
    }
    f.write_str("}")
}
