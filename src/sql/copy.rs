//! COPY's text format: PostgreSQL's tab-delimited, backslash-escaped rows.
//!
//! Reference: PostgreSQL 18, `COPY` — *Text Format*. One row per newline;
//! fields separated by tabs; `\N` alone is NULL; the escapes `\b \f \n \r
//! \t \v \\`, octal `\digits` (up to three) and hex `\xdigits` (up to two)
//! are decoded on input, and any other backslashed character represents
//! itself. On output only `\\` and the control characters are escaped, so a
//! dump is exactly what `psql` and `pg_dump` produce and accept. A line of
//! `\.` ends the data (kept for `pg_dump` scripts, whose data is inline).

use crate::mem::arena::Arena;
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql_err;

/// Splits one row into fields, decoding escapes into the arena. Returns the
/// field count; `fields[i] = None` is NULL. The line excludes its newline.
pub fn decode_row<'a>(
    line: &[u8],
    arena: &'a Arena,
    fields: &mut [Option<&'a str>],
) -> Result<usize, SqlError> {
    let mut n = 0usize;
    let mut at = 0usize;
    loop {
        if n == fields.len() {
            return Err(sql_err!(
                sqlstate::BAD_COPY_FILE_FORMAT,
                "extra data after last expected column"
            ));
        }
        // Decode one field into the arena byte-by-byte. Fields are usually
        // escape-free; the arena copy is what lets the decoded text outlive
        // the wire buffer either way.
        let start = at;
        let mut is_null = false;
        let mut decoded: Option<&mut [u8]> = None;
        let mut out_len = 0usize;
        // First pass over the raw span to find the field's end.
        let mut end = start;
        while end < line.len() && line[end] != b'\t' {
            if line[end] == b'\\' && end + 1 < line.len() {
                end += 2;
            } else {
                end += 1;
            }
        }
        let raw = &line[start..end];
        if raw == b"\\N" {
            is_null = true;
        } else if raw.contains(&b'\\') {
            let buf = arena
                .alloc_slice_with(raw.len(), |_| 0u8)
                .map_err(|_| copy_row_too_large())?;
            let mut i = 0usize;
            while i < raw.len() {
                let b = raw[i];
                if b != b'\\' {
                    if b == b'\r' {
                        return Err(literal_carriage_return());
                    }
                    buf[out_len] = b;
                    out_len += 1;
                    i += 1;
                    continue;
                }
                i += 1;
                let Some(&e) = raw.get(i) else {
                    return Err(sql_err!(
                        sqlstate::BAD_COPY_FILE_FORMAT,
                        "end-of-copy marker corrupt"
                    ));
                };
                i += 1;
                let decoded_byte = match e {
                    b'b' => 0x08,
                    b'f' => 0x0C,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'v' => 0x0B,
                    b'0'..=b'7' => {
                        let mut value = (e - b'0') as u32;
                        for _ in 0..2 {
                            match raw.get(i) {
                                Some(&d @ b'0'..=b'7') => {
                                    value = value * 8 + (d - b'0') as u32;
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        value as u8
                    }
                    b'x' => {
                        let mut value = 0u32;
                        let mut digits = 0;
                        for _ in 0..2 {
                            match raw.get(i) {
                                Some(&d) if (d as char).is_ascii_hexdigit() => {
                                    value = value * 16 + (d as char).to_digit(16).unwrap();
                                    i += 1;
                                    digits += 1;
                                }
                                _ => break,
                            }
                        }
                        if digits == 0 {
                            // `\x` with no digits: the x stands for itself.
                            b'x'
                        } else {
                            value as u8
                        }
                    }
                    // Any other backslashed character represents itself.
                    other => other,
                };
                buf[out_len] = decoded_byte;
                out_len += 1;
            }
            decoded = Some(buf);
        } else if raw.contains(&b'\r') {
            return Err(literal_carriage_return());
        }
        fields[n] = if is_null {
            None
        } else {
            let bytes: &[u8] = match decoded {
                Some(buf) => &buf[..out_len],
                None => {
                    let copy = arena.alloc_slice_copy(raw).map_err(|_| copy_row_too_large())?;
                    &*copy
                }
            };
            Some(core::str::from_utf8(bytes).map_err(|_| {
                sql_err!(
                    sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\""
                )
            })?)
        };
        n += 1;
        if end == line.len() {
            return Ok(n);
        }
        at = end + 1; // past the tab
    }
}

/// Appends one value's COPY-escaped text to `out`; `None` writes `\N`.
pub fn encode_field(out: &mut dyn FnMut(&[u8]), value: Option<&str>) {
    let Some(text) = value else {
        out(b"\\N");
        return;
    };
    let bytes = text.as_bytes();
    let mut plain_from = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        let escape: Option<&[u8]> = match b {
            b'\\' => Some(b"\\\\"),
            b'\t' => Some(b"\\t"),
            b'\n' => Some(b"\\n"),
            b'\r' => Some(b"\\r"),
            0x08 => Some(b"\\b"),
            0x0C => Some(b"\\f"),
            0x0B => Some(b"\\v"),
            _ => None,
        };
        if let Some(seq) = escape {
            if plain_from < i {
                out(&bytes[plain_from..i]);
            }
            out(seq);
            plain_from = i + 1;
        }
    }
    if plain_from < bytes.len() {
        out(&bytes[plain_from..]);
    }
}

/// Whether a data line is the classic end-of-data marker.
pub fn is_end_marker(line: &[u8]) -> bool {
    line == b"\\."
}

fn literal_carriage_return() -> SqlError {
    sql_err!(
        sqlstate::BAD_COPY_FILE_FORMAT,
        "literal carriage return found in data"
    )
}

fn copy_row_too_large() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "COPY row exceeds the statement arena"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    fn arena() -> Arena {
        Arena::new(&mut Budget::new(1 << 22), "copy test", 1 << 20).unwrap()
    }

    fn decode(line: &[u8]) -> Vec<Option<String>> {
        let a = arena();
        let mut fields = [None; 16];
        let n = decode_row(line, &a, &mut fields).expect("decodes");
        fields[..n].iter().map(|f| f.map(|s| s.to_string())).collect()
    }

    fn encode(value: Option<&str>) -> String {
        let mut out = Vec::new();
        encode_field(&mut |b| out.extend_from_slice(b), value);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn plain_fields_split_on_tabs() {
        assert_eq!(
            decode(b"a\tbb\t\tc"),
            vec![
                Some("a".into()),
                Some("bb".into()),
                Some("".into()),
                Some("c".into())
            ]
        );
    }

    #[test]
    fn null_and_escapes_decode() {
        assert_eq!(
            decode(b"\\N\ta\\tb\\nc\\\\d\\015\\x41\\z"),
            vec![None, Some("a\tb\nc\\d\rAz".into())]
        );
        // `\N` must stand alone to be NULL; `\NN` is text.
        assert_eq!(decode(b"\\NN"), vec![Some("NN".into())]);
    }

    #[test]
    fn carriage_returns_are_refused() {
        let a = arena();
        let mut fields = [None; 4];
        let err = decode_row(b"a\rb", &a, &mut fields).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);
        let err = decode_row(b"a\\tb\rc", &a, &mut fields).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::BAD_COPY_FILE_FORMAT);
    }

    #[test]
    fn encoding_round_trips_the_awkward_bytes() {
        for text in ["plain", "tab\there", "line\nbreak", "back\\slash", "cr\rlf", "\u{8}\u{b}\u{c}", "héllo"] {
            let encoded = encode(Some(text));
            let decoded = decode(encoded.as_bytes());
            assert_eq!(decoded, vec![Some(text.to_string())], "via {encoded:?}");
        }
        assert_eq!(encode(None), "\\N");
    }

    #[test]
    fn end_marker_is_exact() {
        assert!(is_end_marker(b"\\."));
        assert!(!is_end_marker(b"\\.."));
        assert!(!is_end_marker(b" \\."));
    }
}
