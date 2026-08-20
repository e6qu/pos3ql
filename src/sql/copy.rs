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
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// Splits one row into fields, decoding escapes into the arena. Returns the
/// field count; `fields[i] = None` is NULL. The line excludes its newline.
pub fn decode_row<'a>(
    line: &[u8],
    arena: &'a Arena,
    fields: &mut [Option<&'a str>],
) -> Result<usize, SqlError> {
    decode_row_text(line, arena, fields, b'\t', "\\N")
}

/// Decodes one text COPY row using its resolved delimiter and NULL sentinel.
/// The sentinel is compared before escape processing, as PostgreSQL requires.
pub fn decode_row_text<'a>(
    line: &[u8],
    arena: &'a Arena,
    fields: &mut [Option<&'a str>],
    delimiter: u8,
    null: &str,
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
        while end < line.len() && line[end] != delimiter {
            if line[end] == b'\\' && end + 1 < line.len() {
                end += 2;
            } else {
                end += 1;
            }
        }
        let raw = &line[start..end];
        if raw == null.as_bytes() {
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
                    let copy = arena
                        .alloc_slice_copy(raw)
                        .map_err(|_| copy_row_too_large())?;
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
        at = end + 1;
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

/// The binary COPY file signature.
pub const BINARY_SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";

/// Result of trying to consume the binary COPY header from a buffer.
pub enum BinaryHeader {
    /// Not enough bytes yet.
    Incomplete,
    /// The signature is wrong — a loud error.
    Bad,
    /// Header consumed; this many bytes to skip (signature + flags + extension).
    Done(usize),
}

/// Validates and measures the binary COPY header (11-byte signature, int32 flags,
/// int32 header-extension length, then that many extension bytes).
pub fn binary_header(buf: &[u8]) -> BinaryHeader {
    let fixed = BINARY_SIGNATURE.len() + 8;
    if buf.len() < fixed {
        return BinaryHeader::Incomplete;
    }
    if &buf[..BINARY_SIGNATURE.len()] != BINARY_SIGNATURE {
        return BinaryHeader::Bad;
    }
    let ext_at = BINARY_SIGNATURE.len() + 4;
    let ext_len = i32::from_be_bytes([
        buf[ext_at],
        buf[ext_at + 1],
        buf[ext_at + 2],
        buf[ext_at + 3],
    ]);
    if ext_len < 0 {
        return BinaryHeader::Bad;
    }
    let total = fixed + ext_len as usize;
    if buf.len() < total {
        BinaryHeader::Incomplete
    } else {
        BinaryHeader::Done(total)
    }
}

/// One binary COPY frame at the front of `buf`.
pub enum BinaryFrame {
    /// A full row: this many bytes (the int16 field count and all fields).
    Row(usize),
    /// The end-of-data trailer (an int16 field count of -1): 2 bytes.
    Trailer,
    /// Not enough bytes buffered yet.
    Incomplete,
    /// A malformed field length.
    Bad,
}

/// Measures the first complete binary COPY frame in `buf` without consuming it,
/// so a row that spans several CopyData chunks is assembled before it is
/// decoded.
pub fn binary_frame(buf: &[u8]) -> BinaryFrame {
    if buf.len() < 2 {
        return BinaryFrame::Incomplete;
    }
    let count = i16::from_be_bytes([buf[0], buf[1]]);
    if count == -1 {
        return BinaryFrame::Trailer;
    }
    if count < 0 {
        return BinaryFrame::Bad;
    }
    let mut at = 2usize;
    for _ in 0..count {
        if buf.len() < at + 4 {
            return BinaryFrame::Incomplete;
        }
        let flen = i32::from_be_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]]);
        at += 4;
        if flen == -1 {
            continue; // NULL field, no bytes
        }
        if flen < 0 {
            return BinaryFrame::Bad;
        }
        let end = at + flen as usize;
        if buf.len() < end {
            return BinaryFrame::Incomplete;
        }
        at = end;
    }
    BinaryFrame::Row(at)
}

/// Appends one CSV field. NULL is the (unquoted) null string; a value is quoted
/// when forced, when it equals the null string, or when it contains the
/// delimiter, the quote, or a newline/CR. Inside a quoted field the quote and
/// escape characters are prefixed with the escape character (with the default
/// escape = quote, that doubles the quote).
#[allow(clippy::too_many_arguments)]
pub fn encode_field_csv(
    out: &mut dyn FnMut(&[u8]),
    value: Option<&str>,
    null_str: &str,
    delimiter: u8,
    quote: u8,
    escape: u8,
    force_quote: bool,
) {
    let Some(text) = value else {
        out(null_str.as_bytes());
        return;
    };
    let bytes = text.as_bytes();
    let needs_quote = force_quote
        || text == null_str
        || bytes
            .iter()
            .any(|&b| b == delimiter || b == quote || b == b'\n' || b == b'\r');
    if !needs_quote {
        out(bytes);
        return;
    }
    out(&[quote]);
    let mut from = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == quote || b == escape {
            if from < i {
                out(&bytes[from..i]);
            }
            out(&[escape]);
            from = i;
        }
    }
    if from < bytes.len() {
        out(&bytes[from..]);
    }
    out(&[quote]);
}

/// Parses one CSV row into `fields`. Unquoted fields split on the delimiter; a
/// quoted field may contain the delimiter, a newline, or (by doubling or the
/// escape character) the quote. An unquoted field equal to the null string is
/// NULL unless FORCE_NOT_NULL names it; a quoted field is the literal string,
/// NULL only under FORCE_NULL. A trailing CR (from CRLF) is ignored. Returns the
/// field count.
#[allow(clippy::too_many_arguments)]
pub fn decode_row_csv<'a>(
    line: &[u8],
    arena: &'a Arena,
    fields: &mut [Option<&'a str>],
    delimiter: u8,
    quote: u8,
    escape: u8,
    null_str: &str,
    force_not_null: &dyn Fn(usize) -> bool,
    force_null: &dyn Fn(usize) -> bool,
) -> Result<usize, SqlError> {
    let line = match line.split_last() {
        Some((b'\r', head)) => head,
        _ => line,
    };
    let mut n = 0usize;
    let mut at = 0usize;
    loop {
        if n == fields.len() {
            return Err(sql_err!(
                sqlstate::BAD_COPY_FILE_FORMAT,
                "extra data after last expected column"
            ));
        }
        let quoted = line.get(at) == Some(&quote);
        let (text, next) = if quoted {
            parse_quoted_csv(line, at, arena, quote, escape, delimiter)?
        } else {
            parse_unquoted_csv(line, at, arena, delimiter)?
        };
        let is_null = if quoted {
            force_null(n) && text == null_str
        } else {
            text == null_str && !force_not_null(n)
        };
        fields[n] = if is_null { None } else { Some(text) };
        n += 1;
        match next {
            Some(after) => at = after,
            None => return Ok(n),
        }
    }
}

fn parse_unquoted_csv<'a>(
    line: &[u8],
    at: usize,
    arena: &'a Arena,
    delimiter: u8,
) -> Result<(&'a str, Option<usize>), SqlError> {
    let mut end = at;
    while end < line.len() && line[end] != delimiter {
        if line[end] == b'\r' {
            return Err(literal_carriage_return());
        }
        end += 1;
    }
    let copy = arena
        .alloc_slice_copy(&line[at..end])
        .map_err(|_| copy_row_too_large())?;
    let text = core::str::from_utf8(&*copy).map_err(|_| invalid_utf8())?;
    let next = if end < line.len() {
        Some(end + 1)
    } else {
        None
    };
    Ok((text, next))
}

fn parse_quoted_csv<'a>(
    line: &[u8],
    at: usize,
    arena: &'a Arena,
    quote: u8,
    escape: u8,
    delimiter: u8,
) -> Result<(&'a str, Option<usize>), SqlError> {
    let unterminated = || {
        sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "unterminated CSV quoted field"
        )
    };
    let buf = arena
        .alloc_slice_with(line.len().saturating_sub(at), |_| 0u8)
        .map_err(|_| copy_row_too_large())?;
    let mut w = 0usize;
    let mut i = at + 1; // past the opening quote
    loop {
        let &c = line.get(i).ok_or_else(unterminated)?;
        if escape != quote && c == escape {
            i += 1;
            let &e = line.get(i).ok_or_else(unterminated)?;
            buf[w] = e;
            w += 1;
            i += 1;
        } else if c == quote {
            if escape == quote && line.get(i + 1) == Some(&quote) {
                buf[w] = quote;
                w += 1;
                i += 2;
            } else {
                i += 1; // closing quote
                break;
            }
        } else {
            buf[w] = c;
            w += 1;
            i += 1;
        }
    }
    let text = core::str::from_utf8(&buf[..w]).map_err(|_| invalid_utf8())?;
    let next = if i >= line.len() {
        None
    } else if line[i] == delimiter {
        Some(i + 1)
    } else {
        return Err(sql_err!(
            sqlstate::BAD_COPY_FILE_FORMAT,
            "extra data after last expected column"
        ));
    };
    Ok((text, next))
}

/// The length of the first complete CSV row in `buf` (up to but excluding its
/// terminating newline), or `None` if the buffer holds no complete row yet — a
/// newline inside a quoted field is not a row terminator, so a row can span
/// several CopyData chunks.
pub fn csv_row_len(buf: &[u8], quote: u8, escape: u8) -> Option<usize> {
    let mut i = 0usize;
    let mut in_quote = false;
    while i < buf.len() {
        let c = buf[i];
        if in_quote {
            if escape != quote && c == escape {
                i += 2; // the escaped byte cannot end the row
                continue;
            }
            if c == quote {
                in_quote = false;
            }
            i += 1;
        } else if c == quote {
            in_quote = true;
            i += 1;
        } else if c == b'\n' {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

fn invalid_utf8() -> SqlError {
    sql_err!(
        sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
        "invalid byte sequence for encoding \"UTF8\""
    )
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
        fields[..n]
            .iter()
            .map(|f| f.map(|s| s.to_string()))
            .collect()
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
        for text in [
            "plain",
            "tab\there",
            "line\nbreak",
            "back\\slash",
            "cr\rlf",
            "\u{8}\u{b}\u{c}",
            "héllo",
        ] {
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

    fn no_force(_: usize) -> bool {
        false
    }

    fn decode_csv(line: &[u8], null: &str) -> Vec<Option<String>> {
        let a = arena();
        let mut fields = [None; 16];
        let n = decode_row_csv(
            line,
            &a,
            &mut fields,
            b',',
            b'"',
            b'"',
            null,
            &no_force,
            &no_force,
        )
        .expect("decodes");
        fields[..n]
            .iter()
            .map(|f| f.map(|s| s.to_string()))
            .collect()
    }

    fn encode_csv(value: Option<&str>, null: &str, force: bool) -> String {
        let mut out = Vec::new();
        encode_field_csv(
            &mut |b| out.extend_from_slice(b),
            value,
            null,
            b',',
            b'"',
            b'"',
            force,
        );
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn csv_decodes_quoting_and_nulls() {
        // Unquoted empty is NULL; quoted empty is the empty string.
        assert_eq!(
            decode_csv(b"a,,\"\"", ""),
            vec![Some("a".into()), None, Some("".into())]
        );
        // A quoted field carries the delimiter and doubled quotes.
        assert_eq!(
            decode_csv(b"\"a,b\",\"say \"\"hi\"\"\"", ""),
            vec![Some("a,b".into()), Some("say \"hi\"".into())]
        );
        // The NULL string only nulls an unquoted field.
        assert_eq!(
            decode_csv(b"NA,\"NA\"", "NA"),
            vec![None, Some("NA".into())]
        );
        // A trailing CR (CRLF line ending) is ignored.
        assert_eq!(
            decode_csv(b"x,y\r", ""),
            vec![Some("x".into()), Some("y".into())]
        );
    }

    #[test]
    fn csv_encodes_only_when_needed() {
        assert_eq!(encode_csv(Some("plain"), "", false), "plain");
        assert_eq!(encode_csv(Some("a,b"), "", false), "\"a,b\"");
        assert_eq!(
            encode_csv(Some("say \"hi\""), "", false),
            "\"say \"\"hi\"\"\""
        );
        assert_eq!(encode_csv(Some("a\nb"), "", false), "\"a\nb\"");
        assert_eq!(encode_csv(None, "", false), "");
        assert_eq!(encode_csv(None, "NA", false), "NA");
        // A value equal to the NULL string is quoted to stay distinct from NULL.
        assert_eq!(encode_csv(Some("NA"), "NA", false), "\"NA\"");
        // FORCE_QUOTE quotes an otherwise-plain value.
        assert_eq!(encode_csv(Some("plain"), "", true), "\"plain\"");
    }

    #[test]
    fn binary_header_validates_signature() {
        assert!(matches!(binary_header(b"PGCOPY"), BinaryHeader::Incomplete));
        // Signature + int32 flags + int32 extension-length (0) = 19 bytes.
        let mut good = BINARY_SIGNATURE.to_vec();
        good.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(matches!(binary_header(&good), BinaryHeader::Done(19)));
        // A wrong signature is a loud error.
        let mut bad = good.clone();
        bad[0] = b'X';
        assert!(matches!(binary_header(&bad), BinaryHeader::Bad));
        // A header extension extends the consumed length.
        let mut ext = BINARY_SIGNATURE.to_vec();
        ext.extend_from_slice(&[0, 0, 0, 0]); // flags
        ext.extend_from_slice(&3i32.to_be_bytes()); // extension length
        ext.extend_from_slice(&[1, 2, 3]); // extension
        assert!(matches!(binary_header(&ext), BinaryHeader::Done(22)));
    }

    #[test]
    fn binary_frame_measures_rows_and_trailer() {
        // Two fields: a 4-byte int and a NULL (-1).
        let mut row = 2i16.to_be_bytes().to_vec();
        row.extend_from_slice(&4i32.to_be_bytes());
        row.extend_from_slice(&42i32.to_be_bytes());
        row.extend_from_slice(&(-1i32).to_be_bytes());
        assert!(matches!(binary_frame(&row), BinaryFrame::Row(14)));
        // Truncated by one byte is incomplete.
        assert!(matches!(
            binary_frame(&row[..row.len() - 1]),
            BinaryFrame::Incomplete
        ));
        // The -1 field count is the trailer.
        assert!(matches!(
            binary_frame(&(-1i16).to_be_bytes()),
            BinaryFrame::Trailer
        ));
        // A negative field length is malformed.
        let mut bad = 1i16.to_be_bytes().to_vec();
        bad.extend_from_slice(&(-2i32).to_be_bytes());
        assert!(matches!(binary_frame(&bad), BinaryFrame::Bad));
    }

    #[test]
    fn csv_row_len_respects_quotes() {
        // A newline inside a quoted field is not the row end.
        assert_eq!(csv_row_len(b"a,\"x\ny\",b\nrest", b'"', b'"'), Some(9));
        // No complete row yet (quote still open).
        assert_eq!(csv_row_len(b"a,\"x\ny", b'"', b'"'), None);
        // Plain row.
        assert_eq!(csv_row_len(b"a,b,c\n", b'"', b'"'), Some(5));
    }
}
