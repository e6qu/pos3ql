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
        let ms = &mut members[..n];
        ms.sort_by(|a, b| a.0.cmp(b.0));
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
