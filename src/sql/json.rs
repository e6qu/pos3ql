//! A hand-written JSON parser for the `json` and `jsonb` types.
//!
//! `json` stores its input verbatim (only validated); `jsonb` is parsed into an
//! arena tree and re-serialized in PostgreSQL's canonical form: object keys
//! sorted with last-value-wins deduplication, exactly one space after `:` and
//! `,`, numbers canonicalized through the NUMERIC type, and strings minimally
//! escaped. The same tree drives the `->` / `->>` accessors.

use crate::mem::arena::{Arena, ArenaList, ArenaString};
use crate::sql::eval::sqlstate;
use crate::sql_err;

use super::eval::SqlError;
use super::numeric::Numeric;
use core::fmt::Write as _;

mod lexer;
pub(crate) use lexer::SyntaxError;
use lexer::{Expecting, Lexer, Token};

/// The nesting depths PostgreSQL 18.6's `json` and `jsonb` input functions
/// reach under its default `max_stack_depth` (2048kB) before
/// `check_stack_depth` fails, measured against the reference server: its
/// recursive descent spends a fixed stack frame per level. The parser and
/// serializers here recurse per level within the query thread's fixed stack
/// (`QUERY_STACK_BYTES`), so these bound that stack too; container width is
/// bounded only by the statement arena.
pub(crate) const JSON_MAX_DEPTH: u32 = 21_790;
pub(crate) const JSONB_MAX_DEPTH: u32 = 18_675;

pub(crate) use crate::sql::stack_depth::stack_depth_exceeded;

/// Statement-arena exhaustion while building a JSON value.
fn full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "JSON value exceeds the statement arena"
    )
}

#[derive(Clone, Copy)]
pub enum Json<'a> {
    Null,
    Bool(bool),
    /// Canonical numeric text.
    Number(&'a str),
    /// Decoded string contents.
    Str(&'a str),
    /// SQL/JSON path datetime item. It serializes as its source JSON string,
    /// but retains typed comparison semantics while a path is evaluated.
    Temporal {
        text: &'a str,
        kind: JsonTemporalKind,
        value: i64,
        offset: i32,
    },
    Array(&'a [Json<'a>]),
    /// Members, sorted by key with duplicates removed (last wins).
    Object(&'a [(&'a str, Json<'a>)]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JsonTemporalKind {
    Date,
    Time,
    TimeTz,
    Timestamp,
    TimestampTz,
}

/// Why JSON text did not parse: its syntax, as PostgreSQL reports it, or
/// another error such as statement memory running out.
pub(crate) enum Failure<'a> {
    Syntax(SyntaxError<'a>),
    Error(SqlError),
}

impl From<SqlError> for Failure<'_> {
    fn from(error: SqlError) -> Self {
        Self::Error(error)
    }
}

impl<'a> From<SyntaxError<'a>> for Failure<'a> {
    fn from(error: SyntaxError<'a>) -> Self {
        Self::Syntax(error)
    }
}

impl Failure<'_> {
    pub(crate) fn error(self) -> SqlError {
        match self {
            Self::Syntax(syntax) => syntax.error(),
            Self::Error(error) => error,
        }
    }
}

/// PostgreSQL's recursive-descent JSON parser (`pg_parse_json`). With an
/// arena it builds the value's tree; without one it only checks the text,
/// as the `json` type's input does.
struct Parser<'a> {
    lexer: Lexer<'a>,
    arena: Option<&'a Arena>,
    preserve_object_order: bool,
    /// The containers that may nest: `json`'s or `jsonb`'s depth.
    max_depth: u32,
}

/// Parses `input` into an arena tree (jsonb semantics: objects sorted/deduped).
pub fn parse<'a>(input: &'a str, arena: &'a Arena) -> Result<Json<'a>, SqlError> {
    Parser::new(input, true, Some(arena), false)
        .document()
        .map_err(Failure::error)
}

/// Parses JSON without applying jsonb object sorting or duplicate-key
/// collapse. Consumers whose PostgreSQL behavior depends on source order use
/// this boundary; ordinary jsonb operations use [`parse`].
pub fn parse_source_order<'a>(input: &'a str, arena: &'a Arena) -> Result<Json<'a>, SqlError> {
    Parser::new(input, true, Some(arena), true)
        .document()
        .map_err(Failure::error)
}

/// Checks that `input` is well-formed JSON, as the `json` type's input
/// does: string escapes are not decoded, so `"\ud800"` and `"\u0000"` are
/// accepted.
pub fn validate(input: &str) -> Result<(), SqlError> {
    Parser::new(input, false, None, false)
        .document()
        .map(|_| ())
        .map_err(Failure::error)
}

/// The `IS JSON` predicate's parse of `input`: `None` when it is not JSON.
/// Checking for unique keys decodes the keys, so their escapes are checked
/// too, as PostgreSQL's `json_validate` does. Without unique keys only the
/// value's kind is returned: containers are empty and scalars are null.
pub fn parse_predicate<'a>(
    input: &'a str,
    unique_keys: bool,
    arena: &'a Arena,
) -> Result<Option<Json<'a>>, SqlError> {
    let parsed = if unique_keys {
        Parser::new(input, true, Some(arena), true).document()
    } else {
        Parser::new(input, false, None, false).document()
    };
    match parsed {
        Ok(value) => Ok(Some(value)),
        Err(Failure::Syntax(_)) => Ok(None),
        Err(Failure::Error(error)) => Err(error),
    }
}

impl<'a> Parser<'a> {
    fn new(
        input: &'a str,
        need_escapes: bool,
        arena: Option<&'a Arena>,
        preserve_object_order: bool,
    ) -> Self {
        Self {
            lexer: Lexer::new(input, need_escapes),
            arena,
            preserve_object_order,
            // Only `jsonb`'s parse sorts and collapses object keys.
            max_depth: if arena.is_some() && !preserve_object_order {
                JSONB_MAX_DEPTH
            } else {
                JSON_MAX_DEPTH
            },
        }
    }

    fn document(&mut self) -> Result<Json<'a>, Failure<'a>> {
        self.lexer.lex()?;
        let value = self.value(0)?;
        self.lexer.expect(Expecting::End, Token::End)?;
        Ok(value)
    }

    fn value(&mut self, depth: u32) -> Result<Json<'a>, Failure<'a>> {
        crate::sql::stack_depth::check_stack_depth()?;
        match self.lexer.token() {
            Token::ObjectStart => self.object(depth),
            Token::ArrayStart => self.array(depth),
            _ => self.scalar(),
        }
    }

    fn nested(&self, depth: u32) -> Result<(), SqlError> {
        if depth >= self.max_depth {
            return Err(stack_depth_exceeded());
        }
        Ok(())
    }

    fn scalar(&mut self) -> Result<Json<'a>, Failure<'a>> {
        let token = self.lexer.token();
        if !matches!(
            token,
            Token::String | Token::Number | Token::True | Token::False | Token::Null
        ) {
            return Err(self.lexer.unexpected(Expecting::Value).into());
        }
        let text = self.lexer.text();
        self.lexer.lex()?;
        let Some(arena) = self.arena else {
            return Ok(Json::Null);
        };
        Ok(match token {
            Token::String => Json::Str(decode_string(&text[1..text.len() - 1], arena)?),
            // Canonicalize through NUMERIC (so 1e2 -> 100, 1.0 -> 1.0).
            Token::Number => Json::Number(
                arena
                    .alloc_str_display(Numeric::parse(text, arena)?)
                    .map_err(|_| full())?,
            ),
            Token::True => Json::Bool(true),
            Token::False => Json::Bool(false),
            _ => Json::Null,
        })
    }

    fn array(&mut self, depth: u32) -> Result<Json<'a>, Failure<'a>> {
        self.nested(depth)?;
        self.lexer.lex()?;
        let mut items = self.arena.map(ArenaList::new);
        if self.lexer.token() != Token::ArrayEnd {
            loop {
                let value = self.value(depth + 1)?;
                if let Some(items) = &mut items {
                    items.push(value).map_err(|_| full())?;
                }
                if self.lexer.token() != Token::Comma {
                    break;
                }
                self.lexer.lex()?;
            }
        }
        self.lexer.expect(Expecting::ArrayNext, Token::ArrayEnd)?;
        Ok(Json::Array(items.map_or(&[], |items| items.as_slice())))
    }

    fn object(&mut self, depth: u32) -> Result<Json<'a>, Failure<'a>> {
        self.nested(depth)?;
        self.lexer.lex()?;
        let mut members = self.arena.map(ArenaList::new);
        match self.lexer.token() {
            Token::String => loop {
                self.field(&mut members, depth)?;
                if self.lexer.token() != Token::Comma {
                    break;
                }
                self.lexer.lex()?;
            },
            Token::ObjectEnd => {}
            _ => return Err(self.lexer.unexpected(Expecting::ObjectStart).into()),
        }
        self.lexer.expect(Expecting::ObjectNext, Token::ObjectEnd)?;
        let (Some(arena), Some(mut members)) = (self.arena, members) else {
            return Ok(Json::Object(&[]));
        };
        if self.preserve_object_order {
            return Ok(Json::Object(members.as_slice()));
        }
        // Stable-sort by key, then drop earlier duplicates (last value wins).
        // jsonb orders object keys by length first, then bytewise — the same
        // order PostgreSQL stores and prints them in.
        crate::mem::arena::stable_sort_via(
            arena,
            members.as_mut_slice().map_err(|_| full())?,
            |a, b| {
                a.0.len()
                    .cmp(&b.0.len())
                    .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
            },
        )
        .map_err(|_| full())?;
        let ms = members.as_slice();
        let mut out = ArenaList::new(arena);
        for i in 0..ms.len() {
            // For a run of equal keys, keep only the last that appeared. Since
            // sort is stable, the last equal element is the last-inserted one.
            if i + 1 < ms.len() && ms[i].0 == ms[i + 1].0 {
                continue;
            }
            out.push(ms[i]).map_err(|_| full())?;
        }
        Ok(Json::Object(out.as_slice()))
    }

    fn field(
        &mut self,
        members: &mut Option<ArenaList<'a, (&'a str, Json<'a>)>>,
        depth: u32,
    ) -> Result<(), Failure<'a>> {
        if self.lexer.token() != Token::String {
            return Err(self.lexer.unexpected(Expecting::String).into());
        }
        let key = self.lexer.text();
        self.lexer.lex()?;
        self.lexer.expect(Expecting::ObjectLabel, Token::Colon)?;
        let value = self.value(depth + 1)?;
        if let (Some(arena), Some(members)) = (self.arena, members) {
            let key = decode_string(&key[1..key.len() - 1], arena)?;
            members.push((key, value)).map_err(|_| full())?;
        }
        Ok(())
    }
}

/// Whether every object in a source-order JSON tree has distinct decoded
/// keys. SQL/JSON `WITH UNIQUE KEYS` applies recursively, not only at the root.
pub fn has_unique_keys(value: &Json<'_>) -> bool {
    match value {
        Json::Array(items) => items.iter().all(has_unique_keys),
        Json::Object(members) => {
            for (index, (key, value)) in members.iter().enumerate() {
                if members[..index].iter().any(|(prior, _)| prior == key) || !has_unique_keys(value)
                {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
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

/// The kind of a document a JSON processing function deconstructs. A scalar
/// is lexed first, as PostgreSQL lexes it before the function's semantic
/// action rejects it, so a bad escape in it is reported instead.
pub fn container_kind(text: &str) -> Result<Kind, SqlError> {
    let kind = kind_of(text);
    if kind == Kind::Scalar {
        Lexer::new(text, true)
            .lex()
            .map_err(|error| error.error())?;
    }
    Ok(kind)
}

/// PostgreSQL's error for calling `*_object_keys` on a non-object.
pub fn object_keys_error(name: &str, kind: Kind) -> SqlError {
    let what = match kind {
        Kind::Scalar => "a scalar",
        Kind::Array => "an array",
        Kind::Object => "an object",
    };
    sql_err!(
        sqlstate::INVALID_PARAMETER_VALUE,
        "cannot call {} on {}",
        name,
        what
    )
}

/// PostgreSQL's error for calling `*_each` on a non-object: the `json`
/// variants cannot deconstruct an array or a scalar; the `jsonb` variants
/// name themselves.
pub fn each_error(name: &str, jsonb: bool, kind: Kind) -> SqlError {
    if jsonb {
        return sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot call {} on a non-object",
            name
        );
    }
    match kind {
        Kind::Array => sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot deconstruct an array as an object"
        ),
        Kind::Scalar | Kind::Object => sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot deconstruct a scalar"
        ),
    }
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
        sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot extract elements from {}",
            what
        )
    } else {
        let what = match kind {
            Kind::Scalar => "a scalar",
            Kind::Object | Kind::Array => "a non-array",
        };
        sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot call {} on {}",
            name,
            what
        )
    }
}

/// The top-level members or elements of one JSON container, stepped in
/// source order one at a time. `start` validates the whole document first,
/// rewinding its scratch after each item, so a malformed tail fails before the
/// first item is returned, as PostgreSQL parses the whole value first. Each
/// item's decoded key uses the caller's arena; values stay verbatim slices of
/// the input.
#[derive(Clone, Copy)]
pub struct SourceItems<'a> {
    /// Positioned at the first token of the next item, the separator after
    /// the last one, or the closing bracket.
    lexer: Lexer<'a>,
    object: bool,
    first: bool,
    done: bool,
}

/// One container item: a decoded object key (`None` for array elements) and
/// the item's verbatim source text.
pub type SourceItem<'a> = (Option<&'a str>, &'a str);

impl<'a> SourceItems<'a> {
    /// Positions after the opening bracket of an object (`object`) or array.
    /// `need_escapes` checks string escapes as decoding them would, as
    /// PostgreSQL does for every object and for the `_text` element
    /// functions. Validation decodes nothing, so `scratch` holds nothing.
    pub fn start(
        text: &'a str,
        object: bool,
        need_escapes: bool,
        scratch: &Arena,
    ) -> Result<Self, SqlError> {
        let open = if object { b'{' } else { b'[' };
        let first = text
            .bytes()
            .find(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        if first != Some(open) {
            return Err(if object {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "cannot call json_object_keys on a non-object"
                )
            } else {
                sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "cannot extract elements from a non-array"
                )
            });
        }
        let mut lexer = Lexer::new(text, need_escapes);
        let opened = lexer.lex().and_then(|()| lexer.lex());
        opened.map_err(|error| error.error())?;
        let items = Self {
            lexer,
            object,
            first: true,
            done: false,
        };
        let mut check = items;
        loop {
            let mark = scratch.mark();
            let item = check.step(scratch);
            // SAFETY: validation keeps no reference to the item's scratch.
            unsafe { scratch.rewind_to(mark) };
            if item?.is_none() {
                break;
            }
        }
        Ok(items)
    }

    /// The next item, or `None` after the closing bracket.
    pub fn next(&mut self, arena: &'a Arena) -> Result<Option<SourceItem<'a>>, SqlError> {
        self.step(arena)
    }

    fn step<'s>(&mut self, arena: &'s Arena) -> Result<Option<SourceItem<'s>>, SqlError>
    where
        'a: 's,
    {
        if self.done {
            return Ok(None);
        }
        let mut parser = Parser {
            lexer: self.lexer,
            arena: None,
            preserve_object_order: true,
            max_depth: JSON_MAX_DEPTH,
        };
        let item = Self::item(&mut parser, self.object, self.first, arena);
        self.lexer = parser.lexer;
        self.first = false;
        let item = item.map_err(Failure::error)?;
        if item.is_none() {
            self.done = true;
        }
        Ok(item)
    }

    /// Parses the next item as `parse_object_field` or
    /// `parse_array_element` would, or the end of the container.
    fn item<'s>(
        parser: &mut Parser<'a>,
        object: bool,
        first: bool,
        arena: &'s Arena,
    ) -> Result<Option<SourceItem<'s>>, Failure<'a>>
    where
        'a: 's,
    {
        let (close, next) = if object {
            (Token::ObjectEnd, Expecting::ObjectNext)
        } else {
            (Token::ArrayEnd, Expecting::ArrayNext)
        };
        let lexer = &mut parser.lexer;
        if first {
            if lexer.token() == close {
                return Self::close(lexer, close, next).map(|()| None);
            }
            if object && lexer.token() != Token::String {
                return Err(lexer.unexpected(Expecting::ObjectStart).into());
            }
        } else if lexer.token() == Token::Comma {
            lexer.lex()?;
        } else {
            return Self::close(lexer, close, next).map(|()| None);
        }
        let key = if object {
            if lexer.token() != Token::String {
                return Err(lexer.unexpected(Expecting::String).into());
            }
            let key = lexer.text();
            lexer.lex()?;
            lexer.expect(Expecting::ObjectLabel, Token::Colon)?;
            Some(decode_string(&key[1..key.len() - 1], arena)?)
        } else {
            None
        };
        let start = parser.lexer.start();
        parser.value(1)?;
        let text = parser.lexer.input_text();
        Ok(Some((key, &text[start..parser.lexer.previous_end()])))
    }

    fn close(lexer: &mut Lexer<'a>, close: Token, next: Expecting) -> Result<(), Failure<'a>> {
        lexer.expect(next, close)?;
        lexer.expect(Expecting::End, Token::End)?;
        Ok(())
    }
}

/// One step of a `json` accessor (`->`, `#>`): an object key or an array
/// index, negative from the end.
#[derive(Clone, Copy)]
pub enum SourceStep<'k> {
    Key(&'k str),
    Index(i64),
}

/// The source text of the member or element `step` names in the `json`
/// document `text`, as the `json` type's accessors return it: verbatim, the
/// last of duplicate keys. The whole document is checked first, its string
/// escapes as decoding them would, as PostgreSQL's `get_worker` parses it.
pub fn source_member<'a>(
    text: &'a str,
    step: SourceStep<'_>,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    match (step, kind_of(text)) {
        (SourceStep::Key(key), Kind::Object) => {
            let mut items = SourceItems::start(text, true, true, arena)?;
            let mut found = None;
            while let Some((name, value)) = items.next(arena)? {
                if name == Some(key) {
                    found = Some(value);
                }
            }
            Ok(found)
        }
        (SourceStep::Index(index), Kind::Array) => {
            let items = array_elements_source(text, true, arena)?;
            let resolved = if index < 0 {
                i64::try_from(items.len()).map_err(|_| full())? + index
            } else {
                index
            };
            Ok(usize::try_from(resolved)
                .ok()
                .and_then(|resolved| items.get(resolved).copied()))
        }
        _ => {
            Parser::new(text, true, None, false)
                .document()
                .map_err(Failure::error)?;
            Ok(None)
        }
    }
}

/// The text form of a `json` value's source, as `->>` and `#>>` return it:
/// a string's decoded value, `None` for a JSON null, anything else verbatim.
pub fn source_text<'a>(source: &'a str, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
    let trimmed = source.trim_matches([' ', '\t', '\n', '\r']);
    if trimmed == "null" {
        return Ok(None);
    }
    if let Some(quoted) = trimmed
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return decode_string(quoted, arena).map(Some);
    }
    Ok(Some(source))
}

/// Top-level members of a JSON object in source order, keys decoded and each
/// value kept as its verbatim source text. Unlike [`parse`], this preserves the
/// input's key order, duplicate keys, and interior whitespace — the behavior of
/// `json_object_keys` / `json_each` on the `json` type (not `jsonb`).
pub fn object_members_source<'a>(
    input: &'a str,
    arena: &'a Arena,
) -> Result<&'a [(&'a str, &'a str)], SqlError> {
    let mut items = SourceItems::start(input, true, true, arena)?;
    let mut members = ArenaList::new(arena);
    while let Some((key, value)) = items.next(arena)? {
        members
            .push((key.expect("object member key"), value))
            .map_err(|_| full())?;
    }
    Ok(members.as_slice())
}

/// Top-level elements of a JSON array in source order, each kept as its verbatim
/// source text. Preserves interior whitespace — the behavior of
/// `json_array_elements` on the `json` type (not `jsonb`).
pub fn array_elements_source<'a>(
    input: &'a str,
    need_escapes: bool,
    arena: &'a Arena,
) -> Result<&'a [&'a str], SqlError> {
    let mut source = SourceItems::start(input, false, need_escapes, arena)?;
    let mut items = ArenaList::new(arena);
    while let Some((_, element)) = source.next(arena)? {
        items.push(element).map_err(|_| full())?;
    }
    Ok(items.as_slice())
}

impl<'a> Json<'a> {
    /// Serializes to PostgreSQL's canonical jsonb text form.
    pub fn write(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self {
            Json::Null => out.write_str("null"),
            Json::Bool(true) => out.write_str("true"),
            Json::Bool(false) => out.write_str("false"),
            Json::Number(s) => out.write_str(s),
            Json::Str(s) | Json::Temporal { text: s, .. } => write_json_raw_string(s, out),
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
                    write_json_raw_string(k, out)?;
                    out.write_str(": ")?;
                    v.write(out)?;
                }
                out.write_str("}")
            }
        }
    }

    /// Serializes with no separator spacing — the form `json` (as opposed to
    /// `jsonb`) functions re-emit, PostgreSQL rendering `json_strip_nulls` of a
    /// spaced object compactly.
    pub fn write_compact(&self, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
        match self {
            Json::Null | Json::Bool(_) | Json::Number(_) | Json::Str(_) | Json::Temporal { .. } => {
                self.write(out)
            }
            Json::Array(items) => {
                out.write_str("[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.write_str(",")?;
                    }
                    v.write_compact(out)?;
                }
                out.write_str("]")
            }
            Json::Object(members) => {
                out.write_str("{")?;
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.write_str(",")?;
                    }
                    write_json_raw_string(k, out)?;
                    out.write_str(":")?;
                    v.write_compact(out)?;
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
                let i = if index < 0 {
                    items.len() as i64 + index
                } else {
                    index
                };
                if i >= 0 && (i as usize) < items.len() {
                    Some(items[i as usize])
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_primitive(&self) -> bool {
        matches!(
            self,
            Json::Null | Json::Bool(_) | Json::Number(_) | Json::Str(_)
        )
    }
}

/// PostgreSQL's jsonb `@>` deep containment: does `container` contain
/// `contained`? An object contains an object when every member of the
/// contained side matches a member of the container (deeply); an array contains
/// an array when every contained element is deeply contained by some container
/// element; an array contains a bare primitive that appears as one of its
/// elements; and two primitives match when equal. Every other type pairing (an
/// array vs an object, a scalar vs a container, …) is not containment.
pub fn contains(container: &Json, contained: &Json) -> bool {
    match (container, contained) {
        (Json::Object(members), Json::Object(sub)) => sub.iter().all(|(sk, sv)| {
            members
                .iter()
                .find(|(k, _)| k == sk)
                .is_some_and(|(_, v)| contains(v, sv))
        }),
        (Json::Array(items), Json::Array(sub)) => {
            sub.iter().all(|se| items.iter().any(|e| contains(e, se)))
        }
        (Json::Array(items), value) if value.is_primitive() => {
            items.iter().any(|e| contains(e, value))
        }
        (a, b) if a.is_primitive() && b.is_primitive() => scalar_eq(a, b),
        _ => false,
    }
}

/// Equality of two JSON primitives. Numbers compare by numeric value (so
/// `1.0` and `1` match, as PostgreSQL's containment does — jsonb preserves the
/// written scale, but compares by value).
fn scalar_eq(a: &Json, b: &Json) -> bool {
    match (a, b) {
        (Json::Null, Json::Null) => true,
        (Json::Bool(x), Json::Bool(y)) => x == y,
        (Json::Str(x), Json::Str(y)) => x == y,
        (Json::Number(x), Json::Number(y)) => {
            x == y || crate::sql::numeric::cmp_decimal_str(x, y) == Some(core::cmp::Ordering::Equal)
        }
        _ => false,
    }
}

/// Sorts and deduplicates object members into a fresh arena slice, in jsonb key
/// order (length, then bytewise; last duplicate wins).
fn build_object<'a>(
    members: &[(&'a str, Json<'a>)],
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    let sorted = arena.alloc_slice_copy(members).map_err(|_| full())?;
    crate::mem::arena::stable_sort_via(arena, sorted, |a, b| {
        a.0.len()
            .cmp(&b.0.len())
            .then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
    })
    .map_err(|_| full())?;
    let ms: &[(&str, Json)] = sorted;
    let mut out = ArenaList::new(arena);
    for i in 0..members.len() {
        if i + 1 < members.len() && ms[i].0 == ms[i + 1].0 {
            continue;
        }
        out.push(ms[i]).map_err(|_| full())?;
    }
    Ok(Json::Object(out.as_slice()))
}

/// Resolves a signed array index (negative counts from the end) into a bound.
/// The array index a path element names, read as PostgreSQL's
/// `setPathArray` does with `strtoint`: leading whitespace and a sign are
/// allowed, anything after the digits is not. `level` is the element's
/// zero-based place in the path.
fn path_index(element: &str, level: usize) -> Result<i64, SqlError> {
    let digits = element.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let unsigned = digits.strip_prefix(['+', '-']).unwrap_or(digits);
    match digits.parse::<i32>() {
        Ok(index) if !unsigned.is_empty() && unsigned.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(i64::from(index))
        }
        _ => Err(sql_err!(
            sqlstate::INVALID_TEXT_REPRESENTATION,
            "path element at position {} is not an integer: \"{}\"",
            level + 1,
            element
        )),
    }
}

fn array_index(index: i64, len: usize) -> Option<usize> {
    let resolved = if index < 0 { len as i64 + index } else { index };
    (resolved >= 0 && (resolved as usize) < len).then_some(resolved as usize)
}

#[derive(Clone, Copy)]
pub enum JsonSubscript<'a> {
    Key(&'a str),
    Index(i64),
}

fn empty_for<'a>(subscript: JsonSubscript<'a>) -> Json<'a> {
    match subscript {
        JsonSubscript::Key(_) => Json::Object(&[]),
        JsonSubscript::Index(_) => Json::Array(&[]),
    }
}

/// PostgreSQL jsonb assignment subscripting. Missing intermediate containers
/// are inferred from the next subscript and arrays are JSON-null padded.
pub fn set_subscript<'a>(
    root: Json<'a>,
    path: &[JsonSubscript<'a>],
    value: Json<'a>,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    let Some((head, rest)) = path.split_first() else {
        return Ok(value);
    };
    match root {
        Json::Object(members) => {
            let key = match *head {
                JsonSubscript::Key(key) => key,
                JsonSubscript::Index(index) => arena
                    .alloc_str_display(index)
                    .map_err(|_| sql_err!(sqlstate::OUT_OF_MEMORY, "statement arena exhausted"))?,
            };
            let buffer = arena
                .alloc_slice_with(members.len() + 1, |_| ("", Json::Null))
                .map_err(|_| full())?;
            buffer[..members.len()].copy_from_slice(members);
            if let Some(index) = members.iter().position(|(name, _)| *name == key) {
                let child = if rest.is_empty() {
                    value
                } else {
                    let existing = members[index].1;
                    let base = if matches!(existing, Json::Null) {
                        empty_for(rest[0])
                    } else {
                        existing
                    };
                    set_subscript(base, rest, value, arena)?
                };
                buffer[index].1 = child;
                build_object(&buffer[..members.len()], arena)
            } else {
                let child = if rest.is_empty() {
                    value
                } else {
                    set_subscript(empty_for(rest[0]), rest, value, arena)?
                };
                buffer[members.len()] = (key, child);
                build_object(&buffer[..members.len() + 1], arena)
            }
        }
        Json::Array(items) => {
            let JsonSubscript::Index(raw) = *head else {
                return Err(sql_err!(
                    sqlstate::INVALID_TEXT_REPRESENTATION,
                    "jsonb array subscript is not an integer"
                ));
            };
            let length = i64::try_from(items.len()).map_err(|_| full())?;
            let resolved = if raw < 0 { length + raw } else { raw };
            if resolved < 0 {
                return Err(sql_err!(
                    sqlstate::ARRAY_SUBSCRIPT_ERROR,
                    "jsonb array subscript is out of bounds"
                ));
            }
            let index = usize::try_from(resolved).map_err(|_| full())?;
            let new_len = items.len().max(index + 1);
            let buffer = arena
                .alloc_slice_with(new_len, |_| Json::Null)
                .map_err(|_| full())?;
            buffer[..items.len()].copy_from_slice(items);
            let existing = buffer[index];
            buffer[index] = if rest.is_empty() {
                value
            } else {
                let base = if index >= items.len() || matches!(existing, Json::Null) {
                    empty_for(rest[0])
                } else {
                    existing
                };
                set_subscript(base, rest, value, arena)?
            };
            Ok(Json::Array(&buffer[..new_len]))
        }
        Json::Null => set_subscript(empty_for(*head), path, value, arena),
        _ => Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot replace existing key"
        )),
    }
}

/// `jsonb_set(target, path, value, create_if_missing)`: replaces the value at
/// `path`, optionally creating a final missing object key.
pub fn set<'a>(
    root: Json<'a>,
    path: &[&'a str],
    value: Json<'a>,
    create: bool,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    set_at(root, path, 0, value, create, arena)
}

fn set_at<'a>(
    root: Json<'a>,
    path: &[&'a str],
    level: usize,
    value: Json<'a>,
    create: bool,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    let Some((head, rest)) = path[level..].split_first() else {
        return Ok(value);
    };
    match root {
        Json::Object(members) => {
            let n = members.len();
            let buffer = arena
                .alloc_slice_with(n + 1, |_| ("", Json::Null))
                .map_err(|_| full())?;
            buffer[..n].copy_from_slice(members);
            if let Some(i) = members.iter().position(|(k, _)| k == head) {
                buffer[i].1 = set_at(members[i].1, path, level + 1, value, create, arena)?;
            } else if rest.is_empty() && create {
                buffer[n] = (*head, value);
                return build_object(&buffer[..n + 1], arena);
            } else {
                return Ok(root); // Missing intermediate key: unchanged.
            }
            build_object(&buffer[..n], arena)
        }
        Json::Array(items) => {
            let Some(i) = array_index(path_index(head, level)?, items.len()) else {
                return Ok(root);
            };
            let buffer = arena
                .alloc_slice_with(items.len(), |_| Json::Null)
                .map_err(|_| full())?;
            buffer[..items.len()].copy_from_slice(items);
            buffer[i] = set_at(items[i], path, level + 1, value, create, arena)?;
            Ok(Json::Array(buffer))
        }
        // Cannot descend into a scalar; leave it unchanged.
        _ => Ok(root),
    }
}

/// `jsonb_insert(target, path, value, insert_after)`: inserts `value` into the
/// array at `path` before (or after) the indexed element, or a new object key.
pub fn insert<'a>(
    root: Json<'a>,
    path: &[&'a str],
    value: Json<'a>,
    after: bool,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    insert_at(root, path, 0, value, after, arena)
}

fn insert_at<'a>(
    root: Json<'a>,
    path: &[&'a str],
    level: usize,
    value: Json<'a>,
    after: bool,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    let Some((head, rest)) = path[level..].split_first() else {
        return Ok(root);
    };
    match root {
        Json::Object(members) => {
            let n = members.len();
            let buffer = arena
                .alloc_slice_with(n + 1, |_| ("", Json::Null))
                .map_err(|_| full())?;
            buffer[..n].copy_from_slice(members);
            if let Some(i) = members.iter().position(|(k, _)| k == head) {
                if rest.is_empty() {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "cannot replace existing key"
                    ));
                }
                buffer[i].1 = insert_at(members[i].1, path, level + 1, value, after, arena)?;
                build_object(&buffer[..n], arena)
            } else if rest.is_empty() {
                buffer[n] = (*head, value);
                build_object(&buffer[..n + 1], arena)
            } else {
                Ok(root)
            }
        }
        Json::Array(items) => {
            if rest.is_empty() {
                // Insert into this array at the (possibly negative) position.
                let raw = path_index(head, level)?;
                let len = items.len() as i64;
                let mut at = if raw < 0 { len + raw } else { raw };
                at = at.clamp(0, len);
                if after {
                    at = (at + 1).min(len);
                }
                let at = at as usize;
                let buffer = arena
                    .alloc_slice_with(items.len() + 1, |_| Json::Null)
                    .map_err(|_| full())?;
                buffer[..at].copy_from_slice(&items[..at]);
                buffer[at] = value;
                buffer[at + 1..items.len() + 1].copy_from_slice(&items[at..]);
                Ok(Json::Array(buffer))
            } else {
                let Some(i) = array_index(path_index(head, level)?, items.len()) else {
                    return Ok(root);
                };
                let buffer = arena
                    .alloc_slice_with(items.len(), |_| Json::Null)
                    .map_err(|_| full())?;
                buffer[..items.len()].copy_from_slice(items);
                buffer[i] = insert_at(items[i], path, level + 1, value, after, arena)?;
                Ok(Json::Array(buffer))
            }
        }
        _ => Ok(root),
    }
}

/// Remove JSON-null object members recursively, and array elements only when
/// requested. A top-level JSON null remains a JSON null.
pub fn strip_nulls<'a>(
    root: Json<'a>,
    strip_arrays: bool,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    match root {
        Json::Object(members) => {
            let mut out = ArenaList::new(arena);
            for (k, v) in members {
                if matches!(v, Json::Null) {
                    continue;
                }
                let stripped = strip_nulls(*v, strip_arrays, arena)?;
                out.push((*k, stripped)).map_err(|_| full())?;
            }
            Ok(Json::Object(out.as_slice()))
        }
        Json::Array(items) => {
            let count = items
                .iter()
                .filter(|value| !strip_arrays || !matches!(value, Json::Null))
                .count();
            let buffer = arena
                .alloc_slice_with(count, |_| Json::Null)
                .map_err(|_| full())?;
            let mut position = 0;
            for value in items {
                if strip_arrays && matches!(value, Json::Null) {
                    continue;
                }
                buffer[position] = strip_nulls(*value, strip_arrays, arena)?;
                position += 1;
            }
            Ok(Json::Array(buffer))
        }
        other => Ok(other),
    }
}

/// `jsonb - text`: removes a top-level object key (no-op for a missing key).
pub fn delete_key<'a>(root: Json<'a>, key: &str, arena: &'a Arena) -> Result<Json<'a>, SqlError> {
    match root {
        Json::Object(members) => {
            let mut out = ArenaList::new(arena);
            for (k, v) in members {
                if *k == key {
                    continue;
                }
                out.push((*k, *v)).map_err(|_| full())?;
            }
            Ok(Json::Object(out.as_slice()))
        }
        // `jsonb - text` on an array removes matching string elements.
        Json::Array(items) => {
            let mut out = ArenaList::new(arena);
            for v in items {
                if matches!(v, Json::Str(s) if *s == key) {
                    continue;
                }
                out.push(*v).map_err(|_| full())?;
            }
            Ok(Json::Array(out.as_slice()))
        }
        _ => Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot delete from scalar"
        )),
    }
}

/// `jsonb - integer`: removes the element at a signed array index.
pub fn delete_index<'a>(
    root: Json<'a>,
    index: i64,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    let Json::Array(items) = root else {
        return Err(sql_err!(
            sqlstate::INVALID_PARAMETER_VALUE,
            "cannot delete from object using integer index"
        ));
    };
    let resolved = if index < 0 {
        items.len() as i64 + index
    } else {
        index
    };
    if resolved < 0 || resolved as usize >= items.len() {
        return Ok(root);
    }
    let skip = resolved as usize;
    let mut out = ArenaList::new(arena);
    for (i, v) in items.iter().enumerate() {
        if i == skip {
            continue;
        }
        out.push(*v).map_err(|_| full())?;
    }
    Ok(Json::Array(out.as_slice()))
}

/// `jsonb #- path`: removes the value at a path.
pub fn delete_path<'a>(
    root: Json<'a>,
    path: &[&'a str],
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    delete_path_at(root, path, 0, arena)
}

fn delete_path_at<'a>(
    root: Json<'a>,
    path: &[&'a str],
    level: usize,
    arena: &'a Arena,
) -> Result<Json<'a>, SqlError> {
    let Some((head, rest)) = path[level..].split_first() else {
        return Ok(root);
    };
    if rest.is_empty() {
        return match root {
            Json::Object(_) => delete_key(root, head, arena),
            Json::Array(items) => delete_index(Json::Array(items), path_index(head, level)?, arena),
            _ => Ok(root),
        };
    }
    match root {
        Json::Object(members) => {
            let buffer = arena.alloc_slice_copy(members).map_err(|_| full())?;
            if let Some(i) = members.iter().position(|(k, _)| k == head) {
                buffer[i].1 = delete_path_at(members[i].1, path, level + 1, arena)?;
            } else {
                return Ok(root);
            }
            Ok(Json::Object(buffer))
        }
        Json::Array(items) => {
            let Some(i) = array_index(path_index(head, level)?, items.len()) else {
                return Ok(root);
            };
            let buffer = arena.alloc_slice_copy(items).map_err(|_| full())?;
            buffer[i] = delete_path_at(items[i], path, level + 1, arena)?;
            Ok(Json::Array(buffer))
        }
        _ => Ok(root),
    }
}

/// `jsonb_pretty`: renders `root` with two-space-per-level indentation (4 in
/// PostgreSQL's output — matched below).
pub fn pretty(root: &Json, indent: usize, out: &mut dyn core::fmt::Write) -> core::fmt::Result {
    let pad = |out: &mut dyn core::fmt::Write, n: usize| -> core::fmt::Result {
        for _ in 0..n * 4 {
            out.write_char(' ')?;
        }
        Ok(())
    };
    match root {
        Json::Object(members) if !members.is_empty() => {
            out.write_str("{\n")?;
            for (i, (k, v)) in members.iter().enumerate() {
                pad(out, indent + 1)?;
                write_json_raw_string(k, out)?;
                out.write_str(": ")?;
                pretty(v, indent + 1, out)?;
                if i + 1 < members.len() {
                    out.write_char(',')?;
                }
                out.write_char('\n')?;
            }
            pad(out, indent)?;
            out.write_char('}')
        }
        Json::Array(items) if !items.is_empty() => {
            out.write_str("[\n")?;
            for (i, v) in items.iter().enumerate() {
                pad(out, indent + 1)?;
                pretty(v, indent + 1, out)?;
                if i + 1 < items.len() {
                    out.write_char(',')?;
                }
                out.write_char('\n')?;
            }
            pad(out, indent)?;
            out.write_char(']')
        }
        // Empty containers and scalars render on a single line.
        scalar => scalar.write(out),
    }
}

/// Renders `root` with [`pretty`] straight into the arena (measure, then write),
/// so no large stack buffer is needed.
pub fn pretty_to_arena<'a>(root: &Json, arena: &'a Arena) -> Result<&'a str, SqlError> {
    struct Counter(usize);
    impl core::fmt::Write for Counter {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }
    let mut counter = Counter(0);
    let _ = pretty(root, 0, &mut counter);
    let bytes = arena
        .alloc_slice_with(counter.0, |_| 0u8)
        .map_err(|_| full())?;
    struct SliceWriter<'b> {
        buffer: &'b mut [u8],
        at: usize,
    }
    impl core::fmt::Write for SliceWriter<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.at + s.len();
            self.buffer[self.at..end].copy_from_slice(s.as_bytes());
            self.at = end;
            Ok(())
        }
    }
    let mut writer = SliceWriter {
        buffer: bytes,
        at: 0,
    };
    let _ = pretty(root, 0, &mut writer);
    Ok(unsafe { core::str::from_utf8_unchecked(writer.buffer) })
}

/// A `Display` adapter that renders a `Json` value in canonical jsonb form.
pub struct JsonWrite<'a, 'b>(pub &'b Json<'a>);

/// [`JsonWrite`]'s compact sibling, for `json`-typed results.
pub struct JsonWriteCompact<'a, 'b>(pub &'b Json<'a>);

impl core::fmt::Display for JsonWriteCompact<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.write_compact(f)
    }
}

impl core::fmt::Display for JsonWrite<'_, '_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.write(f)
    }
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
    let mut buffer = ArenaString::new(arena);
    let bytes = raw.as_bytes();
    let mut i = 0;
    let mut plain_start = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        buffer.write_str(&raw[plain_start..i]).map_err(|_| full())?;
        i += 1;
        let Some(&esc) = bytes.get(i) else {
            return Err(bad());
        };
        match esc {
            b'"' => buffer.write_char('"').map_err(|_| full())?,
            b'\\' => buffer.write_char('\\').map_err(|_| full())?,
            b'/' => buffer.write_char('/').map_err(|_| full())?,
            b'b' => buffer.write_char('\u{08}').map_err(|_| full())?,
            b'f' => buffer.write_char('\u{0c}').map_err(|_| full())?,
            b'n' => buffer.write_char('\n').map_err(|_| full())?,
            b'r' => buffer.write_char('\r').map_err(|_| full())?,
            b't' => buffer.write_char('\t').map_err(|_| full())?,
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
                buffer.write_char(ch).map_err(|_| full())?;
            }
            _ => return Err(bad()),
        }
        i += 1;
        plain_start = i;
    }
    buffer.write_str(&raw[plain_start..]).map_err(|_| full())?;
    Ok(buffer.as_str())
}

/// A string escape [`decode_string`] cannot decode. JSON text reaches it only
/// after the lexer checked its escapes; jsonpath's strings report their own
/// syntax error instead.
fn bad() -> SqlError {
    sql_err!(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        "invalid input syntax for type json"
    )
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

pub(crate) fn map_string_values<'a>(
    value: Json<'a>,
    arena: &'a Arena,
    transform: &mut impl FnMut(&'a str, &'a Arena) -> Result<&'a str, SqlError>,
) -> Result<Json<'a>, SqlError> {
    Ok(match value {
        Json::Str(source) => Json::Str(transform(source, arena)?),
        Json::Array(items) => {
            let mapped = arena
                .alloc_slice_with(items.len(), |_| Json::Null)
                .map_err(|_| full())?;
            for (output, item) in mapped.iter_mut().zip(items) {
                *output = map_string_values(*item, arena, transform)?;
            }
            Json::Array(mapped)
        }
        Json::Object(members) => {
            let mapped = arena
                .alloc_slice_with(members.len(), |_| ("", Json::Null))
                .map_err(|_| full())?;
            for (output, (key, item)) in mapped.iter_mut().zip(members) {
                *output = (*key, map_string_values(*item, arena, transform)?);
            }
            Json::Object(mapped)
        }
        scalar => scalar,
    })
}

/// Escapes a display value directly into a JSON string literal. Unlike a
/// fixed intermediate text buffer, the adapter cannot truncate a long datum.
pub fn write_json_display_string(
    value: &dyn core::fmt::Display,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result {
    struct Escaped<'a>(&'a mut dyn core::fmt::Write);
    impl core::fmt::Write for Escaped<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            for character in text.chars() {
                match character {
                    '"' => self.0.write_str("\\\"")?,
                    '\\' => self.0.write_str("\\\\")?,
                    '\n' => self.0.write_str("\\n")?,
                    '\r' => self.0.write_str("\\r")?,
                    '\t' => self.0.write_str("\\t")?,
                    '\x08' => self.0.write_str("\\b")?,
                    '\x0c' => self.0.write_str("\\f")?,
                    character if (character as u32) < 0x20 => {
                        write!(self.0, "\\u{:04x}", character as u32)?
                    }
                    character => self.0.write_char(character)?,
                }
            }
            Ok(())
        }
    }

    out.write_char('"')?;
    write!(Escaped(out), "{value}")?;
    out.write_char('"')
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
        Datum::Float4(value) if !value.is_finite() => write_json_raw_string(
            if value.is_nan() {
                "NaN"
            } else if value.is_sign_negative() {
                "-Infinity"
            } else {
                "Infinity"
            },
            out,
        ),
        Datum::Float8(value) if !value.is_finite() => write_json_raw_string(
            if value.is_nan() {
                "NaN"
            } else if value.is_sign_negative() {
                "-Infinity"
            } else {
                "Infinity"
            },
            out,
        ),
        Datum::Numeric(value) if value.is_special() => {
            write_json_raw_string(crate::stack_format!(32, "{value}").as_str(), out)
        }
        Datum::Int2(_)
        | Datum::Int4(_)
        | Datum::Int8(_)
        | Datum::Float4(_)
        | Datum::Float8(_)
        | Datum::Numeric(_) => write!(out, "{v}"),
        Datum::Json { text, .. } => out.write_str(text),
        // Date-time types render in ISO 8601 (a `T` separator, `+HH:MM` offset
        // for timestamptz), which is how PostgreSQL's to_json / row_to_json
        // spell them — distinct from the space-separated `::text` form.
        Datum::Timestamp(t) => write_json_raw_string(
            crate::sql::datetime::format_timestamp_json(*t, false).as_str(),
            out,
        ),
        Datum::Timestamptz(t) => write_json_raw_string(
            crate::sql::datetime::format_timestamp_json(*t, true).as_str(),
            out,
        ),
        Datum::Array {
            element: crate::sql::types::ArrElem::Record,
            raw,
        } => {
            out.write_char('[')?;
            for (i, payload) in crate::sql::array::element_payloads(raw).enumerate() {
                if i > 0 {
                    out.write_str(comma)?;
                }
                match crate::sql::array::record_element(payload) {
                    Some(value) => write_projected_json(value, colon, comma, out)?,
                    None => out.write_str("null")?,
                }
            }
            out.write_char(']')
        }
        Datum::Array { element, raw } => {
            out.write_char('[')?;
            for (i, elem) in crate::sql::array::elements(raw, *element).enumerate() {
                if i > 0 {
                    out.write_str(comma)?;
                }
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
        other => write_json_display_string(other, out),
    }
}

/// Writes an encoded value as [`write_datum_json_styled`] does, rendering a
/// record from its encoded fields (its decoded form is only text).
fn write_projected_json(
    value: crate::sql::exec::ProjectedValue<'_>,
    colon: &str,
    comma: &str,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result {
    let Some(fields) = value.record_fields() else {
        return write_datum_json_styled(&value.datum(), colon, comma, out);
    };
    out.write_char('{')?;
    for (i, (name, _, field)) in fields.enumerate() {
        if i > 0 {
            out.write_str(comma)?;
        }
        write_json_raw_string(name, out)?;
        out.write_str(colon)?;
        write_projected_json(field, colon, comma, out)?;
    }
    out.write_char('}')
}
