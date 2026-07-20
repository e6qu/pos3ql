//! Binary text encodings for `encode`/`decode` and the `bytea` I/O forms:
//! base64, hex, and PostgreSQL's `escape` format. All allocate their result in
//! the statement arena so arbitrarily large values never truncate.

use crate::mem::arena::Arena;
use crate::sql_err;

use super::eval::{arena_full, SqlError};

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn bad_base64() -> SqlError {
    sql_err!("22023", "invalid symbol found while decoding base64 sequence")
}

fn bad_hex() -> SqlError {
    sql_err!("22023", "invalid hexadecimal digit")
}

/// Standard base64 with padding, into the arena.
pub fn base64_encode<'a>(bytes: &[u8], arena: &'a Arena) -> Result<&'a str, SqlError> {
    let out_len = bytes.len().div_ceil(3) * 4;
    let out = arena.alloc_slice_with(out_len, |_| 0u8).map_err(|_| arena_full())?;
    let mut at = 0usize;
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out[at] = BASE64[(n >> 18) as usize & 0x3f];
        out[at + 1] = BASE64[(n >> 12) as usize & 0x3f];
        out[at + 2] = if chunk.len() > 1 { BASE64[(n >> 6) as usize & 0x3f] } else { b'=' };
        out[at + 3] = if chunk.len() > 2 { BASE64[n as usize & 0x3f] } else { b'=' };
        at += 4;
    }
    Ok(unsafe { core::str::from_utf8_unchecked(out) })
}

fn base64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Decodes standard base64 (whitespace ignored, per PostgreSQL) into the arena.
pub fn base64_decode<'a>(text: &str, arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    // Collect the 6-bit symbols, skipping whitespace and stopping at padding.
    let mut acc = 0u32;
    let mut bits = 0u32;
    // A base64 string decodes to at most 3/4 of its length.
    let cap = text.len() / 4 * 3 + 3;
    let out = arena.alloc_slice_with(cap, |_| 0u8).map_err(|_| arena_full())?;
    let mut n = 0usize;
    for &c in text.as_bytes() {
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'=' => break,
            _ => {}
        }
        let v = base64_value(c).ok_or_else(bad_base64)?;
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out[n] = (acc >> bits) as u8;
            n += 1;
        }
    }
    Ok(&out[..n])
}

/// Lowercase hex, into the arena.
pub fn hex_encode<'a>(bytes: &[u8], arena: &'a Arena) -> Result<&'a str, SqlError> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let out = arena.alloc_slice_with(bytes.len() * 2, |_| 0u8).map_err(|_| arena_full())?;
    for (i, b) in bytes.iter().enumerate() {
        out[i * 2] = HEX[(b >> 4) as usize];
        out[i * 2 + 1] = HEX[(b & 0xf) as usize];
    }
    Ok(unsafe { core::str::from_utf8_unchecked(out) })
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decodes hex (whitespace between bytes ignored, per PostgreSQL) into the arena.
pub fn hex_decode<'a>(text: &str, arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let out = arena.alloc_slice_with(text.len() / 2 + 1, |_| 0u8).map_err(|_| arena_full())?;
    let mut n = 0usize;
    let mut high: Option<u8> = None;
    for &c in text.as_bytes() {
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        let v = hex_value(c).ok_or_else(bad_hex)?;
        match high {
            None => high = Some(v),
            Some(h) => {
                out[n] = (h << 4) | v;
                n += 1;
                high = None;
            }
        }
    }
    if high.is_some() {
        return Err(sql_err!("22023", "invalid hexadecimal data: odd number of digits"));
    }
    Ok(&out[..n])
}

/// PostgreSQL's `escape` output: printable ASCII (except backslash) verbatim,
/// backslash doubled, everything else as `\nnn` octal.
pub fn escape_encode<'a>(bytes: &[u8], arena: &'a Arena) -> Result<&'a str, SqlError> {
    let len: usize = bytes
        .iter()
        .map(|&b| match b {
            b'\\' => 2,
            0x20..=0x7e => 1,
            _ => 4,
        })
        .sum();
    let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| arena_full())?;
    let mut at = 0usize;
    for &b in bytes {
        match b {
            b'\\' => {
                out[at] = b'\\';
                out[at + 1] = b'\\';
                at += 2;
            }
            0x20..=0x7e => {
                out[at] = b;
                at += 1;
            }
            _ => {
                out[at] = b'\\';
                out[at + 1] = b'0' + (b >> 6);
                out[at + 2] = b'0' + ((b >> 3) & 7);
                out[at + 3] = b'0' + (b & 7);
                at += 4;
            }
        }
    }
    Ok(unsafe { core::str::from_utf8_unchecked(out) })
}

/// Parses PostgreSQL's `escape` bytea text (backslash escapes) into bytes.
pub fn escape_decode<'a>(text: &str, arena: &'a Arena) -> Result<&'a [u8], SqlError> {
    let bytes = text.as_bytes();
    let out = arena.alloc_slice_with(bytes.len(), |_| 0u8).map_err(|_| arena_full())?;
    let mut n = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out[n] = bytes[i];
            n += 1;
            i += 1;
            continue;
        }
        // A backslash escape: `\\` or `\nnn` (three octal digits).
        match bytes.get(i + 1) {
            Some(b'\\') => {
                out[n] = b'\\';
                n += 1;
                i += 2;
            }
            Some(d0 @ b'0'..=b'7') => {
                let (Some(d1 @ b'0'..=b'7'), Some(d2 @ b'0'..=b'7')) =
                    (bytes.get(i + 2), bytes.get(i + 3))
                else {
                    return Err(sql_err!("22021", "invalid input syntax for type bytea"));
                };
                out[n] = ((d0 - b'0') << 6) | ((d1 - b'0') << 3) | (d2 - b'0');
                n += 1;
                i += 4;
            }
            _ => return Err(sql_err!("22021", "invalid input syntax for type bytea")),
        }
    }
    Ok(&out[..n])
}
