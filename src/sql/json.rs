//! A hand-written JSON parser for the `json` and `jsonb` types.
//!
//! `json` stores its input verbatim (only validated); `jsonb` is parsed into an
//! arena tree and re-serialized in PostgreSQL's canonical form: object keys
//! sorted with last-value-wins deduplication, exactly one space after `:` and
//! `,`, numbers canonicalized through the NUMERIC type, and strings minimally
//! escaped. The same tree drives the `->` / `->>` accessors.

use crate::mem::arena::Arena;
use crate::sql_err;

use super::eval::SqlError;
use super::numeric::Numeric;
use core::fmt::Write as _;

/// Maximum elements in one array / members in one object while parsing.
const MAX_ELEMS: usize = 1024;
/// Maximum nesting depth.
const MAX_DEPTH: u32 = 64;

#[derive(Clone, Copy)]
pub enum Json<'a> {
    Null,
    Bool(bool),
    /// Canonical numeric text.
    Number(&'a str),
    /// The raw (unescaped-in-source) string contents, without the quotes.
    Str(&'a str),
    Array(&'a [Json<'a>]),
    /// Members, sorted by key with duplicates removed (last wins).
    Object(&'a [(&'a str, Json<'a>)]),
}

struct P<'a> {
    b: &'a [u8],
    at: usize,
    arena: &'a Arena,
}

fn bad() -> SqlError {
    sql_err!("22P02", "invalid input syntax for type json")
}

/// Parses `input` into an arena tree (jsonb semantics: objects sorted/deduped).
pub fn parse<'a>(input: &'a str, arena: &'a Arena) -> Result<Json<'a>, SqlError> {
    let mut p = P { b: input.as_bytes(), at: 0, arena };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.at != p.b.len() {
        return Err(bad());
    }
    Ok(v)
}

/// Validates that `input` is well-formed JSON (for the `json` type, which is
/// stored verbatim).
pub fn validate(input: &str, arena: &Arena) -> Result<(), SqlError> {
    parse(input, arena).map(|_| ())
}

/// The shape of a JSON value at the top level, for the diagnostic messages the
/// `*_object_keys` / `*_array_elements` functions raise on the wrong input.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Scalar,
    Array,
    Object,
}

/// Classifies a JSON value by its first non-whitespace byte.
pub fn kind_of(text: &str) -> Kind {
    for &b in text.as_bytes() {
        match b {
            b' ' | b'\t' | b'\n' | b'\r' => continue,
            b'{' => return Kind::Object,
            b'[' => return Kind::Array,
            _ => return Kind::Scalar,
        }
    }
    Kind::Scalar
}

/// PostgreSQL's error for calling `*_object_keys` on a non-object.
pub fn object_keys_error(name: &str, kind: Kind) -> SqlError {
    let what = match kind {
        Kind::Scalar => "a scalar",
        Kind::Array => "an array",
        Kind::Object => "an object",
    };
    sql_err!("22023", "cannot call {} on {}", name, what)
}

/// PostgreSQL's error for calling `*_array_elements` on a non-array. The `json`
/// variants phrase it as `cannot call <fn> on a scalar / a non-array`; the
/// `jsonb` variants as `cannot extract elements from a scalar / an object`.
pub fn array_elements_error(name: &str, jsonb: bool, kind: Kind) -> SqlError {
    if jsonb {
        let what = match kind {
            Kind::Scalar => "a scalar",
            Kind::Object => "an object",
            Kind::Array => "an array",
        };
        sql_err!("22023", "cannot extract elements from {}", what)
    } else {
        let what = match kind {
            Kind::Scalar => "a scalar",
            Kind::Object | Kind::Array => "a non-array",
        };
        sql_err!("22023", "cannot call {} on {}", name, what)
    }
}

/// Top-level members of a JSON object in source order, keys decoded and each
/// value kept as its verbatim source text. Unlike [`parse`], this preserves the
/// input's key order, duplicate keys, and interior whitespace — the behavior of
/// `json_object_keys` / `json_each` on the `json` type (not `jsonb`).
pub fn object_members_source<'a>(
    input: &'a str,
    arena: &'a Arena,
) -> Result<&'a [(&'a str, &'a str)], SqlError> {
    let mut p = P { b: input.as_bytes(), at: 0, arena };
    p.ws();
    if p.b.get(p.at) != Some(&b'{') {
        return Err(sql_err!("22023", "cannot call json_object_keys on a non-object"));
    }
    p.at += 1;
    let mut members: [(&str, &str); MAX_ELEMS] = [("", ""); MAX_ELEMS];
    let mut n = 0;
    p.ws();
    if p.b.get(p.at) == Some(&b'}') {
        return Ok(&[]);
    }
    loop {
        if n == MAX_ELEMS {
            return Err(sql_err!("54000", "JSON object too large"));
        }
        p.ws();
        if p.b.get(p.at) != Some(&b'"') {
            return Err(bad());
        }
        let key = decode_string(p.string()?, arena)?;
        p.ws();
        if p.b.get(p.at) != Some(&b':') {
            return Err(bad());
        }
        p.at += 1;
        p.ws();
        let start = p.at;
        p.value(0)?;
        let value = core::str::from_utf8(&p.b[start..p.at]).map_err(|_| bad())?;
        members[n] = (key, value);
        n += 1;
        p.ws();
        match p.b.get(p.at) {
            Some(b',') => p.at += 1,
            Some(b'}') => {
                p.at += 1;
                break;
            }
            _ => return Err(bad()),
        }
    }
    p.ws();
    if p.at != p.b.len() {
        return Err(bad());
    }
    Ok(arena.alloc_slice_copy(&members[..n]).map_err(|_| bad())?)
}

/// Top-level elements of a JSON array in source order, each kept as its verbatim
/// source text. Preserves interior whitespace — the behavior of
/// `json_array_elements` on the `json` type (not `jsonb`).
pub fn array_elements_source<'a>(
    input: &'a str,
    arena: &'a Arena,
) -> Result<&'a [&'a str], SqlError> {
    let mut p = P { b: input.as_bytes(), at: 0, arena };
    p.ws();
    if p.b.get(p.at) != Some(&b'[') {
        return Err(sql_err!("22023", "cannot extract elements from a non-array"));
    }
    p.at += 1;
    let mut items: [&str; MAX_ELEMS] = [""; MAX_ELEMS];
    let mut n = 0;
    p.ws();
    if p.b.get(p.at) == Some(&b']') {
        return Ok(&[]);
    }
    loop {
        if n == MAX_ELEMS {
            return Err(sql_err!("54000", "JSON array too large"));
        }
        p.ws();
        let start = p.at;
        p.value(0)?;
        items[n] = core::str::from_utf8(&p.b[start..p.at]).map_err(|_| bad())?;
        n += 1;
        p.ws();
        match p.b.get(p.at) {
            Some(b',') => p.at += 1,
            Some(b']') => {
                p.at += 1;
                break;
            }
            _ => return Err(bad()),
        }
    }
    p.ws();
    if p.at != p.b.len() {
        return Err(bad());
    }
    Ok(arena.alloc_slice_copy(&items[..n]).map_err(|_| bad())?)
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while self.at < self.b.len() && matches!(self.b[self.at], b' ' | b'\t' | b'\n' | b'\r') {
            self.at += 1;
        }
    }

    fn value(&mut self, depth: u32) -> Result<Json<'a>, SqlError> {
        if depth > MAX_DEPTH {
            return Err(sql_err!("54001", "JSON nested too deeply"));
        }
        self.ws();
        match self.b.get(self.at) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => {
                self.lit("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.lit("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'n') => {
                self.lit("null")?;
                Ok(Json::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => self.number(),
            _ => Err(bad()),
        }
    }

    fn lit(&mut self, s: &str) -> Result<(), SqlError> {
        if self.b[self.at..].starts_with(s.as_bytes()) {
            self.at += s.len();
            Ok(())
        } else {
            Err(bad())
        }
    }

    fn number(&mut self) -> Result<Json<'a>, SqlError> {
        let start = self.at;
        if self.b.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        while self.at < self.b.len()
            && (self.b[self.at].is_ascii_digit()
                || matches!(self.b[self.at], b'.' | b'e' | b'E' | b'+' | b'-'))
        {
            self.at += 1;
        }
        let raw = core::str::from_utf8(&self.b[start..self.at]).map_err(|_| bad())?;
        // Canonicalize through NUMERIC (so 1e2 -> 100, 1.0 -> 1.0).
        let n = Numeric::parse(raw, self.arena).map_err(|_| bad())?;
        let canon = crate::stack_format!(80, "{}", n);
        Ok(Json::Number(self.arena.alloc_str(canon.as_str()).map_err(|_| bad())?))
    }

    /// Parses a JSON string literal, returning the raw source contents between
    /// the quotes (escapes are validated but kept as written).
    fn string(&mut self) -> Result<&'a str, SqlError> {
        debug_assert_eq!(self.b[self.at], b'"');
        self.at += 1;
        let start = self.at;
        loop {
            match self.b.get(self.at) {
                None => return Err(bad()),
                Some(b'"') => {
                    let s = core::str::from_utf8(&self.b[start..self.at]).map_err(|_| bad())?;
                    self.at += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    self.at += 1;
                    match self.b.get(self.at) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1;
                        }
                        Some(b'u') => {
                            self.at += 1;
                            for _ in 0..4 {
                                if !self.b.get(self.at).is_some_and(u8::is_ascii_hexdigit) {
                                    return Err(bad());
                                }
                                self.at += 1;
                            }
                        }
                        _ => return Err(bad()),
                    }
                }
                Some(_) => self.at += 1,
            }
        }
    }

    fn array(&mut self, depth: u32) -> Result<Json<'a>, SqlError> {
        self.at += 1; // [
        let mut items = [Json::Null; MAX_ELEMS];
        let mut n = 0;
        self.ws();
        if self.b.get(self.at) == Some(&b']') {
            self.at += 1;
            return Ok(Json::Array(&[]));
        }
        loop {
            if n == MAX_ELEMS {
                return Err(sql_err!("54000", "JSON array too large"));
            }
            items[n] = self.value(depth + 1)?;
            n += 1;
            self.ws();
            match self.b.get(self.at) {
                Some(b',') => {
                    self.at += 1;
                }
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(bad()),
            }
        }
        Ok(Json::Array(self.arena.alloc_slice_copy(&items[..n]).map_err(|_| bad())?))
    }

    fn object(&mut self, depth: u32) -> Result<Json<'a>, SqlError> {
        self.at += 1; // {
        let mut members: [(&str, Json); MAX_ELEMS] = [("", Json::Null); MAX_ELEMS];
        let mut n = 0;
        self.ws();
        if self.b.get(self.at) == Some(&b'}') {
            self.at += 1;
            return Ok(Json::Object(&[]));
        }
        loop {
            if n == MAX_ELEMS {
                return Err(sql_err!("54000", "JSON object too large"));
            }
            self.ws();
            if self.b.get(self.at) != Some(&b'"') {
                return Err(bad());
            }
            let key = self.string()?;
            self.ws();
            if self.b.get(self.at) != Some(&b':') {
                return Err(bad());
            }
            self.at += 1;
            let value = self.value(depth + 1)?;
            members[n] = (key, value);
            n += 1;
            self.ws();
            match self.b.get(self.at) {
                Some(b',') => {
                    self.at += 1;
                }
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(bad()),
            }
        }
        // Stable-sort by key, then drop earlier duplicates (last value wins).
        // jsonb orders object keys by length first, then bytewise — the same
        // order PostgreSQL stores and prints them in.
        let ms = &mut members[..n];
        ms.sort_by(|a, b| {
            a.0.len().cmp(&b.0.len()).then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
        });
        let mut out: [(&str, Json); MAX_ELEMS] = [("", Json::Null); MAX_ELEMS];
        let mut m = 0;
        for i in 0..n {
            // For a run of equal keys, keep only the last that appeared. Since
            // sort is stable, the last equal element is the last-inserted one.
            if i + 1 < n && ms[i].0 == ms[i + 1].0 {
                continue;
            }
            out[m] = ms[i];
            m += 1;
        }
        Ok(Json::Object(self.arena.alloc_slice_copy(&out[..m]).map_err(|_| bad())?))
    }
}

impl<'a> Json<'a> {
    /// Serializes to PostgreSQL's canonical jsonb text form.
    pub fn write(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self {
            Json::Null => out.write_str("null"),
            Json::Bool(true) => out.write_str("true"),
            Json::Bool(false) => out.write_str("false"),
            Json::Number(s) => out.write_str(s),
            Json::Str(s) => write_json_string(s, out),
            Json::Array(items) => {
                out.write_str("[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    v.write(out)?;
                }
                out.write_str("]")
            }
            Json::Object(members) => {
                out.write_str("{")?;
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.write_str(", ")?;
                    }
                    write_json_string(k, out)?;
                    out.write_str(": ")?;
                    v.write(out)?;
                }
                out.write_str("}")
            }
        }
    }

    /// `->` accessor: object field by key, or array element by (0-based) index.
    pub fn get_field(&self, key: &str) -> Option<Json<'a>> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| *k == key).map(|(_, v)| *v),
            _ => None,
        }
    }

    pub fn get_index(&self, index: i64) -> Option<Json<'a>> {
        match self {
            Json::Array(items) => {
                let i = if index < 0 { items.len() as i64 + index } else { index };
                if i >= 0 && (i as usize) < items.len() {
                    Some(items[i as usize])
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// A `Display` adapter that renders a `Json` value in canonical jsonb form.
pub struct JsonWrite<'a, 'b>(pub &'b Json<'a>);

impl core::fmt::Display for JsonWrite<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.write(f)
    }
}

/// Writes a JSON string literal with the canonical minimal escaping.
fn write_json_string(s: &str, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    out.write_str("\"")?;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => {
                // The source already contains an escape; copy it through so we
                // do not double-escape a `\uXXXX` or `\n` written by the user.
                let bytes = s.as_bytes();
                if let Some(&nx) = bytes.get(i + 1) {
                    out.write_char('\\')?;
                    out.write_char(nx as char)?;
                    // consume the escaped char from the iterator
                    if nx == b'u' {
                        for _ in 0..5 {
                            chars.next();
                        }
                    } else {
                        chars.next();
                    }
                } else {
                    out.write_str("\\\\")?;
                }
            }
            c => out.write_char(c)?,
        }
    }
    out.write_str("\"")
}

/// Decodes a JSON string body (the bytes between the quotes, with escapes still
/// present) into its raw text. This is what the `->>`, `#>>` and
/// `*_text` accessors return: a JSON string's *value*, with `\n`, `\t`,
/// `\uXXXX`, surrogate pairs, etc. resolved to the characters they denote.
/// Strings without a backslash are returned by reference with no allocation.
pub fn decode_string<'a>(raw: &'a str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    if !raw.as_bytes().contains(&b'\\') {
        return Ok(raw);
    }
    let mut buffer = crate::util::StackStr::<65536>::new();
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'\\' {
            buffer.write_char(b as char).map_err(|_| bad())?;
            i += 1;
            continue;
        }
        i += 1;
        let Some(&esc) = bytes.get(i) else {
            return Err(bad());
        };
        match esc {
            b'"' => buffer.write_char('"').map_err(|_| bad())?,
            b'\\' => buffer.write_char('\\').map_err(|_| bad())?,
            b'/' => buffer.write_char('/').map_err(|_| bad())?,
            b'b' => buffer.write_char('\u{08}').map_err(|_| bad())?,
            b'f' => buffer.write_char('\u{0c}').map_err(|_| bad())?,
            b'n' => buffer.write_char('\n').map_err(|_| bad())?,
            b'r' => buffer.write_char('\r').map_err(|_| bad())?,
            b't' => buffer.write_char('\t').map_err(|_| bad())?,
            b'u' => {
                let code = hex4(bytes, i + 1)?;
                i += 4;
                let scalar = if (0xD800..=0xDBFF).contains(&code) {
                    // High surrogate: must be followed by `\uXXXX` low surrogate.
                    if bytes.get(i + 1) == Some(&b'\\') && bytes.get(i + 2) == Some(&b'u') {
                        let low = hex4(bytes, i + 3)?;
                        if !(0xDC00..=0xDFFF).contains(&low) {
                            return Err(bad());
                        }
                        i += 6;
                        0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00)
                    } else {
                        return Err(bad());
                    }
                } else if (0xDC00..=0xDFFF).contains(&code) {
                    return Err(bad());
                } else {
                    code
                };
                let ch = char::from_u32(scalar).ok_or_else(bad)?;
                buffer.write_char(ch).map_err(|_| bad())?;
            }
            _ => return Err(bad()),
        }
        i += 1;
    }
    arena.alloc_str(buffer.as_str()).map_err(|_| bad())
}

/// Parses four hex digits at `bytes[at..at+4]` into a code point.
fn hex4(bytes: &[u8], at: usize) -> Result<u32, SqlError> {
    let mut value = 0u32;
    for offset in 0..4 {
        let digit = bytes.get(at + offset).ok_or_else(bad)?;
        let nibble = match digit {
            b'0'..=b'9' => digit - b'0',
            b'a'..=b'f' => digit - b'a' + 10,
            b'A'..=b'F' => digit - b'A' + 10,
            _ => return Err(bad()),
        };
        value = (value << 4) | nibble as u32;
    }
    Ok(value)
}

/// Escapes a raw string into a JSON string literal (used by `row_to_json` and
/// friends, whose inputs are raw text, not pre-escaped JSON).
pub fn write_json_raw_string(s: &str, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    out.write_str("\"")?;
    for c in s.chars() {
        match c {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            '\x08' => out.write_str("\\b")?,
            '\x0c' => out.write_str("\\f")?,
            c if (c as u32) < 0x20 => write!(out, "\\u{:04x}", c as u32)?,
            c => out.write_char(c)?,
        }
    }
    out.write_str("\"")
}

/// Renders a datum as JSON (`row_to_json`/`to_json` → compact spacing;
/// `to_jsonb` → jsonb spacing with `": "` / `", "`), following PostgreSQL's
/// `datum_to_json`: numbers and booleans bare, everything else a quoted
/// string of its text form, records as objects, arrays as JSON arrays, and an
/// existing json/jsonb value embedded verbatim.
pub fn write_datum_json(
    v: &crate::sql::types::Datum,
    jsonb: bool,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result {
    // `row_to_json`/`to_json` use compact spacing; `to_jsonb` the jsonb form.
    let (colon, comma) = if jsonb { (": ", ", ") } else { (":", ",") };
    write_datum_json_styled(v, colon, comma, out)
}

/// Like [`write_datum_json`] but with explicit object-`:` and element-`,`
/// spacing, so the `json_build_*` family (which uses `" : "` / `, `) and the
/// `jsonb_build_*` family (which uses `": "` / `, `) can share the renderer.
pub fn write_datum_json_styled(
    v: &crate::sql::types::Datum,
    colon: &str,
    comma: &str,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result {
    use crate::sql::types::Datum;
    match v {
        Datum::Null => out.write_str("null"),
        Datum::Bool(b) => out.write_str(if *b { "true" } else { "false" }),
        Datum::Int4(_) | Datum::Int8(_) | Datum::Float8(_) | Datum::Numeric(_) => {
            write!(out, "{v}")
        }
        Datum::Json { text, .. } => out.write_str(text),
        Datum::Array { element, raw } => {
            out.write_char('[')?;
            let count = crate::sql::array::len(raw);
            for i in 0..count {
                if i > 0 {
                    out.write_str(comma)?;
                }
                let elem = crate::sql::array::get(raw, *element, i).unwrap_or(Datum::Null);
                write_datum_json_styled(&elem, colon, comma, out)?;
            }
            out.write_char(']')
        }
        Datum::Record(fields) => {
            out.write_char('{')?;
            for (i, field) in fields.iter().enumerate() {
                if i > 0 {
                    out.write_str(comma)?;
                }
                write_json_raw_string(field.name, out)?;
                out.write_str(colon)?;
                write_datum_json_styled(&field.value, colon, comma, out)?;
            }
            out.write_char('}')
        }
        // Everything else renders as a quoted string of its text form.
        other => {
            let mut buf = crate::util::StackStr::<8192>::default();
            let _ = write!(buf, "{other}");
            write_json_raw_string(buf.as_str(), out)
        }
    }
}
