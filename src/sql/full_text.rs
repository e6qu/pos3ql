//! PostgreSQL full-text values and built-in configurations.
//!
//! Search values cross the engine as canonical text, but only after this
//! module has parsed their complete structure. That keeps the durable and wire
//! representations compact without admitting unchecked query syntax.

use core::cmp::Ordering;
use core::fmt::Write as _;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;
use crate::util::StackStr;

/// A parsed, canonical PostgreSQL `tsvector` value. The field is private so
/// ordinary text cannot cross a text-search boundary without parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsVector<'a>(&'a str);

impl<'a> TsVector<'a> {
    pub const fn as_str(self) -> &'a str {
        self.0
    }

    pub const fn len(self) -> usize {
        self.0.len()
    }

    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.0.as_bytes()
    }
}

/// A parsed, canonical PostgreSQL `tsquery` operator tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TsQuery<'a>(&'a str);

impl<'a> TsQuery<'a> {
    pub const fn as_str(self) -> &'a str {
        self.0
    }

    pub const fn len(self) -> usize {
        self.0.len()
    }

    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    pub const fn as_bytes(self) -> &'a [u8] {
        self.0.as_bytes()
    }
}

/// Rebuilds a value emitted by this module's durable encoder. Callers use
/// this only after the enclosing row/WAL framing has been authenticated.
pub(crate) const fn restore_vector(source: &str) -> TsVector<'_> {
    TsVector(source)
}

/// See [`restore_vector`].
pub(crate) const fn restore_query(source: &str) -> TsQuery<'_> {
    TsQuery(source)
}

const MAX_QUERY_DEPTH: usize = 64;
// Text input reserves one byte for PostgreSQL's internal terminator. Binary
// input carries that terminator separately and can fill the 11-bit field.
const MAX_TEXT_LEXEME_BYTES: usize = 2_046;
const MAX_WIRE_LEXEME_BYTES: usize = 2_047;
const MAX_LEXEME_POSITIONS: usize = 256;
const MAX_TEXT_SEARCH_STORAGE_BYTES: usize = (1 << 20) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSearchConfig {
    Simple,
    English,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSearchLexeme<'a> {
    Unmapped,
    StopWord,
    Lexeme(&'a str),
}

impl TextSearchConfig {
    pub fn parse(name: &str) -> Option<Self> {
        let unqualified = name.rsplit('.').next().unwrap_or(name);
        if unqualified.eq_ignore_ascii_case("simple") {
            Some(Self::Simple)
        } else if unqualified.eq_ignore_ascii_case("english") {
            Some(Self::English)
        } else {
            None
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::English => "english",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub number: u16,
    /// PostgreSQL stores D/C/B/A as 0/1/2/3 in the high two bits.
    pub weight: u8,
}

#[derive(Clone, Copy, Debug, Default)]
struct VectorLexeme<'a> {
    text: &'a str,
    positions_start: usize,
    positions_len: usize,
}

pub struct Vector<'a> {
    lexemes: ArenaList<'a, VectorLexeme<'a>>,
    positions: ArenaList<'a, Position>,
}

impl<'a> Vector<'a> {
    fn empty(arena: &'a Arena) -> Self {
        Self {
            lexemes: ArenaList::new(arena),
            positions: ArenaList::new(arena),
        }
    }

    pub fn lexeme_count(&self) -> usize {
        self.lexemes.len()
    }

    pub fn lexeme(&self, index: usize) -> Option<(&'a str, &[Position])> {
        let entry = *self.lexemes.as_slice().get(index)?;
        let end = entry.positions_start + entry.positions_len;
        Some((
            entry.text,
            &self.positions.as_slice()[entry.positions_start..end],
        ))
    }
}

fn syntax(kind: &'static str, source: &str) -> SqlError {
    sql_err!(
        sqlstate::SYNTAX_ERROR,
        "syntax error in {}: \"{}\"",
        kind,
        source
    )
}

fn capacity(kind: &'static str) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "{} exceeds the configured statement capacity",
        kind
    )
}

fn arena_full(kind: &'static str) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "{} exceeds the statement arena",
        kind
    )
}

fn unescape_lexeme<'a>(
    source: &'a str,
    start: usize,
    end: usize,
    quoted: bool,
    max_bytes: usize,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let raw = &source.as_bytes()[start..end];
    if raw.len() > max_bytes * 2 {
        return Err(capacity("text-search lexeme"));
    }
    if !raw.contains(&b'\\') && !(quoted && raw.contains(&b'\'')) {
        let text = &source[start..end];
        if text.len() > max_bytes {
            return Err(capacity("text-search lexeme"));
        }
        return Ok(text);
    }
    let out = arena
        .alloc_slice_with(raw.len(), |_| 0u8)
        .map_err(|_| arena_full("text-search lexeme"))?;
    let mut read = 0usize;
    let mut written = 0usize;
    while read < raw.len() {
        let byte = raw[read];
        if byte == b'\\' {
            read += 1;
            if read == raw.len() {
                return Err(syntax("text search value", source));
            }
            out[written] = raw[read];
        } else if quoted && byte == b'\'' && raw.get(read + 1) == Some(&b'\'') {
            out[written] = b'\'';
            read += 1;
        } else {
            out[written] = byte;
        }
        written += 1;
        read += 1;
    }
    if written > max_bytes {
        return Err(capacity("text-search lexeme"));
    }
    core::str::from_utf8(&out[..written]).map_err(|_| syntax("text search value", source))
}

fn parse_position(source: &str, at: &mut usize) -> Result<Position, SqlError> {
    let bytes = source.as_bytes();
    let start = *at;
    let mut number = 0u32;
    while let Some(byte @ b'0'..=b'9') = bytes.get(*at).copied() {
        number = number
            .checked_mul(10)
            .and_then(|value| value.checked_add(u32::from(byte - b'0')))
            .ok_or_else(|| syntax("tsvector", source))?;
        *at += 1;
    }
    if *at == start || number == 0 {
        return Err(syntax("tsvector", source));
    }
    // WordEntryPos uses fourteen bits. PostgreSQL clamps larger input to the
    // largest representable position rather than wrapping it.
    let number = number.min(16_383) as u16;
    let weight = match bytes.get(*at).copied() {
        Some(b'A') => {
            *at += 1;
            3
        }
        Some(b'B') => {
            *at += 1;
            2
        }
        Some(b'C') => {
            *at += 1;
            1
        }
        Some(b'D') => {
            *at += 1;
            0
        }
        Some(b'a'..=b'd') => return Err(syntax("tsvector", source)),
        _ => 0,
    };
    Ok(Position { number, weight })
}

fn parse_vector_with_limit<'a>(
    source: &'a str,
    max_lexeme_bytes: usize,
    arena: &'a Arena,
) -> Result<Vector<'a>, SqlError> {
    let mut vector = Vector::empty(arena);
    let bytes = source.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        let quoted = bytes[at] == b'\'';
        let (start, end) = if quoted {
            at += 1;
            let start = at;
            let mut escaped = false;
            while at < bytes.len() {
                match bytes[at] {
                    b'\\' if !escaped => {
                        escaped = true;
                        at += 1;
                    }
                    b'\'' if !escaped => {
                        if bytes.get(at + 1) == Some(&b'\'') {
                            at += 2;
                        } else {
                            break;
                        }
                    }
                    _ => {
                        escaped = false;
                        at += 1;
                    }
                }
            }
            if at >= bytes.len() || bytes[at] != b'\'' {
                return Err(syntax("tsvector", source));
            }
            let end = at;
            at += 1;
            (start, end)
        } else {
            let start = at;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b':' {
                at += 1;
            }
            if at == start {
                return Err(syntax("tsvector", source));
            }
            (start, at)
        };
        let text = unescape_lexeme(source, start, end, quoted, max_lexeme_bytes, arena)?;
        let positions_start = vector.positions.len();
        if bytes.get(at) == Some(&b':') {
            at += 1;
            loop {
                let position = parse_position(source, &mut at)?;
                vector
                    .positions
                    .push(position)
                    .map_err(|_| arena_full("tsvector positions"))?;
                if bytes.get(at) != Some(&b',') {
                    break;
                }
                at += 1;
            }
        }
        if bytes
            .get(at)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            return Err(syntax("tsvector", source));
        }
        vector
            .lexemes
            .push(VectorLexeme {
                text,
                positions_start,
                positions_len: vector.positions.len() - positions_start,
            })
            .map_err(|_| arena_full("tsvector"))?;
    }
    vector
        .lexemes
        .as_mut_slice()
        .sort_unstable_by(|left, right| left.text.as_bytes().cmp(right.text.as_bytes()));
    Ok(vector)
}

pub fn parse_vector<'a>(source: &'a str, arena: &'a Arena) -> Result<Vector<'a>, SqlError> {
    parse_vector_with_limit(source, MAX_WIRE_LEXEME_BYTES, arena)
}

struct ArenaText<'a> {
    bytes: &'a mut [u8],
    len: usize,
    kind: &'static str,
}

impl<'a> ArenaText<'a> {
    fn new(arena: &'a Arena, capacity: usize, kind: &'static str) -> Result<Self, SqlError> {
        let bytes = arena
            .alloc_slice_with(capacity.max(1), |_| 0u8)
            .map_err(|_| arena_full(kind))?;
        Ok(Self {
            bytes,
            len: 0,
            kind,
        })
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), SqlError> {
        let Some(slot) = self.bytes.get_mut(self.len) else {
            return Err(capacity(self.kind));
        };
        *slot = byte;
        self.len += 1;
        Ok(())
    }

    fn push_str(&mut self, text: &str) -> Result<(), SqlError> {
        let end = self
            .len
            .checked_add(text.len())
            .ok_or_else(|| capacity(self.kind))?;
        let Some(target) = self.bytes.get_mut(self.len..end) else {
            return Err(capacity(self.kind));
        };
        target.copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }

    fn push_quoted(&mut self, text: &str) -> Result<(), SqlError> {
        self.push_byte(b'\'')?;
        for byte in text.bytes() {
            if matches!(byte, b'\'' | b'\\') {
                self.push_byte(b'\\')?;
            }
            self.push_byte(byte)?;
        }
        self.push_byte(b'\'')
    }

    fn finish(self) -> &'a str {
        // Every input came from UTF-8 and only ASCII punctuation was added.
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len]) }
    }
}

fn write_u16(out: &mut ArenaText<'_>, value: u16) -> Result<(), SqlError> {
    let rendered = crate::stack_format!(8, "{}", value);
    out.push_str(rendered.as_str())
}

pub fn canonical_vector<'a>(source: &'a str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    canonical_vector_with_limit(source, MAX_TEXT_LEXEME_BYTES, arena)
}

fn canonical_vector_with_limit<'a>(
    source: &'a str,
    max_lexeme_bytes: usize,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let vector = parse_vector_with_limit(source, max_lexeme_bytes, arena)?;
    let output_capacity = source
        .len()
        .checked_mul(3)
        .and_then(|n| n.checked_add(vector.positions.len() * 8 + vector.lexemes.len() * 4))
        .ok_or_else(|| capacity("tsvector"))?;
    let mut out = ArenaText::new(arena, output_capacity, "tsvector")?;
    let mut index = 0usize;
    let mut first = true;
    let mut storage_bytes = 0usize;
    let gathered = arena
        .alloc_slice_with(vector.positions.len(), |_| Position::default())
        .map_err(|_| arena_full("tsvector positions"))?;
    while index < vector.lexemes.len() {
        let text = vector.lexemes.as_slice()[index].text;
        let mut next = index;
        let mut count = 0usize;
        while next < vector.lexemes.len() && vector.lexemes.as_slice()[next].text == text {
            let entry = vector.lexemes.as_slice()[next];
            let end = entry.positions_start + entry.positions_len;
            for position in &vector.positions.as_slice()[entry.positions_start..end] {
                gathered[count] = *position;
                count += 1;
            }
            next += 1;
        }
        gathered[..count].sort_unstable_by_key(|position| (position.number, 3 - position.weight));
        let mut unique = 0usize;
        for offset in 0..count {
            let position = gathered[offset];
            if unique > 0 && gathered[unique - 1].number == position.number {
                gathered[unique - 1].weight = gathered[unique - 1].weight.max(position.weight);
            } else {
                if unique == MAX_LEXEME_POSITIONS {
                    break;
                }
                gathered[unique] = position;
                unique += 1;
            }
        }
        storage_bytes = storage_bytes
            .checked_add(text.len())
            .ok_or_else(|| capacity("tsvector"))?;
        if unique > 0 {
            storage_bytes = storage_bytes.next_multiple_of(2);
            storage_bytes = storage_bytes
                .checked_add(2 + unique * 2)
                .ok_or_else(|| capacity("tsvector"))?;
        }
        if storage_bytes > MAX_TEXT_SEARCH_STORAGE_BYTES {
            return Err(capacity("tsvector storage"));
        }
        if !first {
            out.push_byte(b' ')?;
        }
        first = false;
        out.push_quoted(text)?;
        if unique > 0 {
            out.push_byte(b':')?;
            for (position_index, position) in gathered[..unique].iter().enumerate() {
                if position_index > 0 {
                    out.push_byte(b',')?;
                }
                write_u16(&mut out, position.number)?;
                match position.weight {
                    3 => out.push_byte(b'A')?,
                    2 => out.push_byte(b'B')?,
                    1 => out.push_byte(b'C')?,
                    _ => {}
                }
            }
        }
        index = next;
    }
    Ok(out.finish())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryNode<'a> {
    Lexeme {
        text: &'a str,
        weights: u8,
        prefix: bool,
    },
    Not(u32),
    And(u32, u32),
    Or(u32, u32),
    Phrase {
        left: u32,
        right: u32,
        distance: u16,
    },
}

pub struct Query<'a> {
    nodes: ArenaList<'a, QueryNode<'a>>,
    root: Option<u32>,
}

impl<'a> Query<'a> {
    fn empty(arena: &'a Arena) -> Self {
        Self {
            nodes: ArenaList::new(arena),
            root: None,
        }
    }

    fn push(&mut self, node: QueryNode<'a>) -> Result<u32, SqlError> {
        let index = u32::try_from(self.nodes.len()).map_err(|_| capacity("tsquery"))?;
        self.nodes.push(node).map_err(|_| arena_full("tsquery"))?;
        Ok(index)
    }

    pub fn root(&self) -> Option<u32> {
        self.root
    }

    pub fn node(&self, index: u32) -> Option<QueryNode<'a>> {
        self.nodes.as_slice().get(index as usize).copied()
    }

    fn nodes(&self) -> &'a [QueryNode<'a>] {
        self.nodes.as_slice()
    }
}

struct QueryParser<'a> {
    source: &'a str,
    arena: &'a Arena,
    at: usize,
    depth: usize,
    max_lexeme_bytes: usize,
    enforce_operand_storage: bool,
    operand_bytes: usize,
    query: Query<'a>,
}

impl<'a> QueryParser<'a> {
    fn skip_ws(&mut self) {
        while self
            .source
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.at += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        self.skip_ws();
        if self.source.as_bytes().get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn parse(mut self) -> Result<Query<'a>, SqlError> {
        self.skip_ws();
        if self.at == self.source.len() {
            return Ok(self.query);
        }
        let root = self.parse_or()?;
        self.skip_ws();
        if self.at != self.source.len() {
            return Err(syntax("tsquery", self.source));
        }
        self.query.root = Some(root);
        Ok(self.query)
    }

    fn parse_or(&mut self) -> Result<u32, SqlError> {
        let mut left = self.parse_and()?;
        while self.consume(b'|') {
            let right = self.parse_and()?;
            left = self.query.push(QueryNode::Or(left, right))?;
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<u32, SqlError> {
        let mut left = self.parse_phrase()?;
        while self.consume(b'&') {
            let right = self.parse_phrase()?;
            left = self.query.push(QueryNode::And(left, right))?;
        }
        Ok(left)
    }

    fn parse_phrase(&mut self) -> Result<u32, SqlError> {
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            let rest = &self.source.as_bytes()[self.at..];
            let distance = if rest.starts_with(b"<->") {
                self.at += 3;
                Some(1)
            } else if rest.first() == Some(&b'<') {
                self.at += 1;
                let start = self.at;
                let mut distance = 0u32;
                while let Some(byte @ b'0'..=b'9') = self.source.as_bytes().get(self.at).copied() {
                    distance = distance
                        .checked_mul(10)
                        .and_then(|n| n.checked_add(u32::from(byte - b'0')))
                        .ok_or_else(|| syntax("tsquery", self.source))?;
                    self.at += 1;
                }
                if self.at == start || self.source.as_bytes().get(self.at) != Some(&b'>') {
                    return Err(syntax("tsquery", self.source));
                }
                self.at += 1;
                Some(u16::try_from(distance).map_err(|_| capacity("tsquery phrase distance"))?)
            } else {
                None
            };
            let Some(distance) = distance else { break };
            let right = self.parse_unary()?;
            left = self.query.push(QueryNode::Phrase {
                left,
                right,
                distance,
            })?;
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<u32, SqlError> {
        if self.consume(b'!') {
            let child = self.parse_unary()?;
            return self.query.push(QueryNode::Not(child));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<u32, SqlError> {
        self.skip_ws();
        if self.consume(b'(') {
            self.depth += 1;
            if self.depth > MAX_QUERY_DEPTH {
                return Err(capacity("tsquery nesting"));
            }
            let node = self.parse_or()?;
            if !self.consume(b')') {
                return Err(syntax("tsquery", self.source));
            }
            self.depth -= 1;
            return Ok(node);
        }
        let bytes = self.source.as_bytes();
        let quoted = bytes.get(self.at) == Some(&b'\'');
        let (start, end) = if quoted {
            self.at += 1;
            let start = self.at;
            let mut escaped = false;
            while self.at < bytes.len() {
                match bytes[self.at] {
                    b'\\' if !escaped => {
                        escaped = true;
                        self.at += 1;
                    }
                    b'\'' if !escaped => {
                        if bytes.get(self.at + 1) == Some(&b'\'') {
                            self.at += 2;
                        } else {
                            break;
                        }
                    }
                    _ => {
                        escaped = false;
                        self.at += 1;
                    }
                }
            }
            if bytes.get(self.at) != Some(&b'\'') {
                return Err(syntax("tsquery", self.source));
            }
            let end = self.at;
            self.at += 1;
            (start, end)
        } else {
            let start = self.at;
            while self.at < bytes.len()
                && !bytes[self.at].is_ascii_whitespace()
                && !matches!(
                    bytes[self.at],
                    b'!' | b'&' | b'|' | b'(' | b')' | b'<' | b':'
                )
            {
                self.at += 1;
            }
            if self.at == start {
                return Err(syntax("tsquery", self.source));
            }
            (start, self.at)
        };
        let text = unescape_lexeme(
            self.source,
            start,
            end,
            quoted,
            self.max_lexeme_bytes,
            self.arena,
        )?;
        if self.enforce_operand_storage && self.operand_bytes >= MAX_TEXT_SEARCH_STORAGE_BYTES {
            return Err(capacity("tsquery storage"));
        }
        self.operand_bytes = self
            .operand_bytes
            .checked_add(text.len() + 1)
            .ok_or_else(|| capacity("tsquery storage"))?;
        let mut weights = 0u8;
        let mut prefix = false;
        if bytes.get(self.at) == Some(&b':') {
            self.at += 1;
            let modifier_start = self.at;
            while let Some(byte) = bytes.get(self.at).copied() {
                match byte {
                    b'*' => prefix = true,
                    b'A' => weights |= 1 << 3,
                    b'B' => weights |= 1 << 2,
                    b'C' => weights |= 1 << 1,
                    b'D' => weights |= 1,
                    _ => break,
                }
                self.at += 1;
            }
            if self.at == modifier_start {
                return Err(syntax("tsquery", self.source));
            }
        }
        self.query.push(QueryNode::Lexeme {
            text,
            weights,
            prefix,
        })
    }
}

pub fn parse_query<'a>(source: &'a str, arena: &'a Arena) -> Result<Query<'a>, SqlError> {
    parse_query_with_limit(source, MAX_WIRE_LEXEME_BYTES, false, arena)
}

fn parse_query_with_limit<'a>(
    source: &'a str,
    max_lexeme_bytes: usize,
    enforce_operand_storage: bool,
    arena: &'a Arena,
) -> Result<Query<'a>, SqlError> {
    QueryParser {
        source,
        arena,
        at: 0,
        depth: 0,
        max_lexeme_bytes,
        enforce_operand_storage,
        operand_bytes: 0,
        query: Query::empty(arena),
    }
    .parse()
}

fn query_precedence(node: QueryNode<'_>) -> u8 {
    match node {
        QueryNode::Or(..) => 1,
        QueryNode::And(..) => 2,
        QueryNode::Phrase { .. } => 3,
        QueryNode::Not(..) => 4,
        QueryNode::Lexeme { .. } => 5,
    }
}

fn write_query_node(
    query: &Query<'_>,
    index: u32,
    parent_precedence: u8,
    out: &mut ArenaText<'_>,
) -> Result<(), SqlError> {
    let node = query.node(index).ok_or_else(|| syntax("tsquery", ""))?;
    let precedence = query_precedence(node);
    let parens = precedence < parent_precedence;
    if parens {
        out.push_str("( ")?;
    }
    match node {
        QueryNode::Lexeme {
            text,
            weights,
            prefix,
        } => {
            out.push_quoted(text)?;
            if prefix || weights != 0 {
                out.push_byte(b':')?;
                if prefix {
                    out.push_byte(b'*')?;
                }
                for (bit, letter) in [(3, b'A'), (2, b'B'), (1, b'C'), (0, b'D')] {
                    if weights & (1 << bit) != 0 {
                        out.push_byte(letter)?;
                    }
                }
            }
        }
        QueryNode::Not(child) => {
            out.push_byte(b'!')?;
            write_query_node(query, child, precedence, out)?;
        }
        QueryNode::And(left, right) | QueryNode::Or(left, right) => {
            write_query_node(query, left, precedence, out)?;
            out.push_str(if matches!(node, QueryNode::And(..)) {
                " & "
            } else {
                " | "
            })?;
            write_query_node(query, right, precedence, out)?;
        }
        QueryNode::Phrase {
            left,
            right,
            distance,
        } => {
            write_query_node(query, left, precedence, out)?;
            if distance == 1 {
                out.push_str(" <-> ")?;
            } else {
                out.push_str(" <")?;
                write_u16(out, distance)?;
                out.push_str("> ")?;
            }
            write_query_node(query, right, precedence, out)?;
        }
    }
    if parens {
        out.push_str(" )")?;
    }
    Ok(())
}

pub fn canonical_query<'a>(source: &'a str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let query = parse_query_with_limit(source, MAX_TEXT_LEXEME_BYTES, true, arena)?;
    let Some(root) = query.root else {
        return Ok("");
    };
    let output_capacity = source
        .len()
        .checked_mul(4)
        .and_then(|n| n.checked_add(query.nodes.len() * 8))
        .ok_or_else(|| capacity("tsquery"))?;
    let mut out = ArenaText::new(arena, output_capacity, "tsquery")?;
    write_query_node(&query, root, 0, &mut out)?;
    Ok(out.finish())
}

fn format_query<'a>(query: &Query<'_>, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let Some(root) = query.root else {
        return Ok("");
    };
    let output_capacity = query
        .nodes()
        .iter()
        .try_fold(1usize, |size, node| {
            let addition = match node {
                QueryNode::Lexeme { text, .. } => text.len().checked_mul(2)?.checked_add(8)?,
                _ => 16,
            };
            size.checked_add(addition)
        })
        .ok_or_else(|| capacity("tsquery"))?;
    let mut out = ArenaText::new(arena, output_capacity, "tsquery")?;
    write_query_node(query, root, 0, &mut out)?;
    Ok(out.finish())
}

fn emit_i16(emit: &mut impl FnMut(&[u8]), value: i16) {
    emit(&value.to_be_bytes());
}

fn emit_i32(emit: &mut impl FnMut(&[u8]), value: i32) {
    emit(&value.to_be_bytes());
}

fn emit_unescaped(emit: &mut impl FnMut(&[u8]), raw: &[u8]) -> usize {
    let mut at = 0usize;
    let mut begin = 0usize;
    let mut written = 0usize;
    while at < raw.len() {
        if raw[at] == b'\\' {
            if begin < at {
                emit(&raw[begin..at]);
                written += at - begin;
            }
            at += 1;
            debug_assert!(at < raw.len(), "canonical text-search escape");
            emit(&raw[at..at + 1]);
            written += 1;
            at += 1;
            begin = at;
        } else {
            at += 1;
        }
    }
    if begin < raw.len() {
        emit(&raw[begin..]);
        written += raw.len() - begin;
    }
    written
}

fn scan_quoted<'a>(source: &'a [u8], at: &mut usize) -> &'a [u8] {
    debug_assert_eq!(source.get(*at), Some(&b'\''));
    *at += 1;
    let start = *at;
    let mut escaped = false;
    while *at < source.len() {
        match source[*at] {
            b'\\' if !escaped => {
                escaped = true;
                *at += 1;
            }
            b'\'' if !escaped => break,
            _ => {
                escaped = false;
                *at += 1;
            }
        }
    }
    debug_assert!(*at < source.len(), "canonical text-search quote");
    let raw = &source[start..*at];
    *at += 1;
    raw
}

/// Emits a PostgreSQL `tsvectorsend` body. Only values produced by the typed
/// input boundary reach this function, so its canonical scanner is infallible.
pub(crate) fn emit_vector_binary(source: &str, mut emit: impl FnMut(&[u8])) -> usize {
    let bytes = source.as_bytes();
    let mut scan = 0usize;
    let mut count = 0usize;
    while scan < bytes.len() {
        if bytes[scan] == b' ' {
            scan += 1;
        }
        let _ = scan_quoted(bytes, &mut scan);
        count += 1;
        while scan < bytes.len() && bytes[scan] != b' ' {
            scan += 1;
        }
    }
    emit_i32(&mut emit, count as i32);
    let mut emitted_bytes = 4usize;
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at] == b' ' {
            at += 1;
        }
        let raw = scan_quoted(bytes, &mut at);
        let mut positions = [Position::default(); MAX_LEXEME_POSITIONS];
        let mut position_count = 0usize;
        if bytes.get(at) == Some(&b':') {
            at += 1;
            loop {
                positions[position_count] =
                    parse_position(source, &mut at).expect("canonical tsvector position");
                position_count += 1;
                if bytes.get(at) != Some(&b',') {
                    break;
                }
                at += 1;
            }
        }
        emitted_bytes += emit_unescaped(&mut emit, raw) + 3 + position_count * 2;
        emit(&[0]);
        emit_i16(&mut emit, position_count as i16);
        for position in &positions[..position_count] {
            emit(&((u16::from(position.weight) << 14) | position.number).to_be_bytes());
        }
    }
    emitted_bytes
}

#[derive(Clone, Copy)]
enum CanonicalExpression<'a> {
    Lexeme {
        raw: &'a [u8],
        weights: u8,
        prefix: bool,
    },
    Not(&'a [u8]),
    Binary {
        operator: u8,
        distance: u16,
        left: &'a [u8],
        right: &'a [u8],
    },
}

fn trim_query_space(mut source: &[u8]) -> &[u8] {
    while source.first() == Some(&b' ') {
        source = &source[1..];
    }
    while source.last() == Some(&b' ') {
        source = &source[..source.len() - 1];
    }
    source
}

fn strip_query_parentheses(mut source: &[u8]) -> &[u8] {
    source = trim_query_space(source);
    while source.starts_with(b"( ") && source.ends_with(b" )") {
        let mut depth = 0usize;
        let mut at = 0usize;
        let mut closes_at_end = false;
        while at < source.len() {
            match source[at] {
                b'\'' => {
                    at += 1;
                    while at < source.len() {
                        if source[at] == b'\\' {
                            at += 2;
                        } else if source[at] == b'\'' {
                            at += 1;
                            break;
                        } else {
                            at += 1;
                        }
                    }
                }
                b'(' => {
                    depth += 1;
                    at += 1;
                }
                b')' => {
                    depth -= 1;
                    at += 1;
                    if depth == 0 {
                        closes_at_end = at == source.len();
                        break;
                    }
                }
                _ => at += 1,
            }
        }
        if !closes_at_end {
            break;
        }
        source = trim_query_space(&source[2..source.len() - 2]);
    }
    source
}

fn canonical_expression(source: &[u8]) -> CanonicalExpression<'_> {
    let source = strip_query_parentheses(source);
    let mut depth = 0usize;
    let mut at = 0usize;
    let mut or = None;
    let mut and = None;
    let mut phrase = None;
    while at < source.len() {
        match source[at] {
            b'\'' => {
                at += 1;
                while at < source.len() {
                    if source[at] == b'\\' {
                        at += 2;
                    } else if source[at] == b'\'' {
                        at += 1;
                        break;
                    } else {
                        at += 1;
                    }
                }
            }
            b'(' => {
                depth += 1;
                at += 1;
            }
            b')' => {
                depth -= 1;
                at += 1;
            }
            b'|' if depth == 0 => {
                or = Some(at);
                at += 1;
            }
            b'&' if depth == 0 => {
                and = Some(at);
                at += 1;
            }
            b'<' if depth == 0 => {
                let begin = at;
                at += 1;
                let distance = if source.get(at..at + 2) == Some(&b"->"[..]) {
                    at += 2;
                    1
                } else {
                    let mut value = 0u16;
                    while let Some(byte @ b'0'..=b'9') = source.get(at).copied() {
                        value = value
                            .saturating_mul(10)
                            .saturating_add(u16::from(byte - b'0'));
                        at += 1;
                    }
                    value
                };
                debug_assert_eq!(source.get(at), Some(&b'>'));
                at += 1;
                phrase = Some((begin, at, distance));
            }
            _ => at += 1,
        }
    }
    let binary = if let Some(operator) = or {
        Some((operator, operator + 1, 3, 0))
    } else if let Some(operator) = and {
        Some((operator, operator + 1, 2, 0))
    } else {
        phrase.map(|(begin, end, distance)| (begin, end, 4, distance))
    };
    if let Some((begin, end, operator, distance)) = binary {
        return CanonicalExpression::Binary {
            operator,
            distance,
            left: trim_query_space(&source[..begin]),
            right: trim_query_space(&source[end..]),
        };
    }
    if source.first() == Some(&b'!') {
        return CanonicalExpression::Not(trim_query_space(&source[1..]));
    }
    let mut at = 0usize;
    let raw = scan_quoted(source, &mut at);
    let mut weights = 0u8;
    let mut prefix = false;
    if source.get(at) == Some(&b':') {
        at += 1;
        for byte in &source[at..] {
            match *byte {
                b'*' => prefix = true,
                b'A' => weights |= 1 << 3,
                b'B' => weights |= 1 << 2,
                b'C' => weights |= 1 << 1,
                b'D' => weights |= 1,
                _ => break,
            }
        }
    }
    CanonicalExpression::Lexeme {
        raw,
        weights,
        prefix,
    }
}

fn canonical_query_node_count(source: &[u8]) -> usize {
    let mut count = 0usize;
    let mut at = 0usize;
    while at < source.len() {
        match source[at] {
            b'\'' => {
                count += 1;
                at += 1;
                while at < source.len() {
                    if source[at] == b'\\' {
                        at += 2;
                    } else if source[at] == b'\'' {
                        at += 1;
                        break;
                    } else {
                        at += 1;
                    }
                }
            }
            b'!' | b'&' | b'|' | b'<' => {
                count += 1;
                at += 1;
            }
            _ => at += 1,
        }
    }
    count
}

fn emit_query_binary_node(source: &[u8], emit: &mut impl FnMut(&[u8])) -> usize {
    match canonical_expression(source) {
        CanonicalExpression::Lexeme {
            raw,
            weights,
            prefix,
        } => {
            emit(&[1, weights, u8::from(prefix)]);
            let len = emit_unescaped(emit, raw);
            emit(&[0]);
            4 + len
        }
        CanonicalExpression::Not(child) => {
            emit(&[2, 1]);
            2 + emit_query_binary_node(child, emit)
        }
        CanonicalExpression::Binary {
            operator,
            distance,
            left,
            right,
        } => {
            emit(&[2, operator]);
            let header = if operator == 4 {
                emit_i16(emit, distance as i16);
                4
            } else {
                2
            };
            header + emit_query_binary_node(right, emit) + emit_query_binary_node(left, emit)
        }
    }
}

/// Emits a PostgreSQL `tsquerysend` body in its right-first prefix order.
pub(crate) fn emit_query_binary(source: &str, mut emit: impl FnMut(&[u8])) -> usize {
    let count = canonical_query_node_count(source.as_bytes());
    emit_i32(
        &mut emit,
        i32::try_from(count).expect("canonical tsquery item count"),
    );
    4 + if source.is_empty() {
        0
    } else {
        emit_query_binary_node(source.as_bytes(), &mut emit)
    }
}

pub(crate) fn decode_vector_binary<'a>(
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut input = crate::pg::wire::MsgIn::new(bytes);
    let count = input.i32().map_err(|_| syntax("tsvector binary", ""))?;
    let count = usize::try_from(count)
        .ok()
        .ok_or_else(|| syntax("tsvector binary", ""))?;
    let mut raw = ArenaText::new(
        arena,
        bytes.len().saturating_mul(3).saturating_add(1),
        "tsvector",
    )?;
    for index in 0..count {
        let lexeme = input.cstr().map_err(|_| syntax("tsvector binary", ""))?;
        if lexeme.is_empty() || lexeme.len() > MAX_WIRE_LEXEME_BYTES {
            return Err(syntax("tsvector binary", ""));
        }
        let position_count = input.i16().map_err(|_| syntax("tsvector binary", ""))?;
        let position_count = usize::try_from(position_count)
            .ok()
            .filter(|count| *count <= MAX_LEXEME_POSITIONS)
            .ok_or_else(|| syntax("tsvector binary", ""))?;
        if index > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(lexeme)?;
        if position_count > 0 {
            raw.push_byte(b':')?;
        }
        for position_index in 0..position_count {
            let packed = u16::from_be_bytes(
                input
                    .take(2)
                    .map_err(|_| syntax("tsvector binary", ""))?
                    .try_into()
                    .unwrap(),
            );
            let number = packed & 0x3fff;
            if number == 0 {
                return Err(syntax("tsvector binary", ""));
            }
            if position_index > 0 {
                raw.push_byte(b',')?;
            }
            write_u16(&mut raw, number)?;
            match packed >> 14 {
                3 => raw.push_byte(b'A')?,
                2 => raw.push_byte(b'B')?,
                1 => raw.push_byte(b'C')?,
                _ => {}
            }
        }
    }
    if !input.done() {
        return Err(syntax("tsvector binary", ""));
    }
    canonical_vector_with_limit(raw.finish(), MAX_WIRE_LEXEME_BYTES, arena)
}

#[derive(Clone, Copy)]
struct QueryDecodeFrame {
    operator: u8,
    distance: u16,
    right: Option<u32>,
}

fn complete_decoded_query_node<'a>(
    mut node: u32,
    query: &mut Query<'a>,
    frames: &mut ArenaList<'a, QueryDecodeFrame>,
    root: &mut Option<u32>,
) -> Result<(), SqlError> {
    loop {
        let Some(frame) = frames.as_mut_slice().last_mut() else {
            if root.replace(node).is_some() {
                return Err(syntax("tsquery binary", ""));
            }
            return Ok(());
        };
        if frame.operator != 1 && frame.right.is_none() {
            frame.right = Some(node);
            return Ok(());
        }
        let frame = *frame;
        frames.truncate(frames.len() - 1);
        node = query.push(match frame.operator {
            1 => QueryNode::Not(node),
            2 => QueryNode::And(node, frame.right.expect("binary query right child")),
            3 => QueryNode::Or(node, frame.right.expect("binary query right child")),
            4 => QueryNode::Phrase {
                left: node,
                right: frame.right.expect("binary query right child"),
                distance: frame.distance,
            },
            _ => return Err(syntax("tsquery binary", "")),
        })?;
    }
}

pub(crate) fn decode_query_binary<'a>(
    bytes: &'a [u8],
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut input = crate::pg::wire::MsgIn::new(bytes);
    let count = input.i32().map_err(|_| syntax("tsquery binary", ""))?;
    let count = usize::try_from(count)
        .ok()
        .ok_or_else(|| syntax("tsquery binary", ""))?;
    let mut query = Query::empty(arena);
    let mut frames = ArenaList::new(arena);
    let mut root = None;
    let mut operand_bytes = 0usize;
    for _ in 0..count {
        match input.u8().map_err(|_| syntax("tsquery binary", ""))? {
            1 => {
                let weights = input.u8().map_err(|_| syntax("tsquery binary", ""))?;
                let prefix = input.u8().map_err(|_| syntax("tsquery binary", ""))?;
                if weights > 0x0f || prefix > 1 {
                    return Err(syntax("tsquery binary", ""));
                }
                let text = input.cstr().map_err(|_| syntax("tsquery binary", ""))?;
                if text.is_empty() || text.len() > MAX_WIRE_LEXEME_BYTES {
                    return Err(syntax("tsquery binary", ""));
                }
                if operand_bytes >= MAX_TEXT_SEARCH_STORAGE_BYTES {
                    return Err(syntax("tsquery binary", ""));
                }
                operand_bytes = operand_bytes
                    .checked_add(text.len() + 1)
                    .ok_or_else(|| syntax("tsquery binary", ""))?;
                let node = query.push(QueryNode::Lexeme {
                    text,
                    weights,
                    prefix: prefix != 0,
                })?;
                complete_decoded_query_node(node, &mut query, &mut frames, &mut root)?;
            }
            2 => {
                let operator = input.u8().map_err(|_| syntax("tsquery binary", ""))?;
                if !(1..=4).contains(&operator) {
                    return Err(syntax("tsquery binary", ""));
                }
                let distance = if operator == 4 {
                    u16::from_be_bytes(
                        input
                            .take(2)
                            .map_err(|_| syntax("tsquery binary", ""))?
                            .try_into()
                            .unwrap(),
                    )
                } else {
                    0
                };
                frames
                    .push(QueryDecodeFrame {
                        operator,
                        distance,
                        right: None,
                    })
                    .map_err(|_| arena_full("tsquery binary"))?;
            }
            _ => return Err(syntax("tsquery binary", "")),
        }
    }
    if !frames.is_empty() || root.is_none() != (count == 0) || !input.done() {
        return Err(syntax("tsquery binary", ""));
    }
    query.root = root;
    format_query(&query, arena)
}

#[derive(Clone, Copy)]
struct MatchResult {
    truth: bool,
    positions: [u16; 256],
    count: usize,
}

impl MatchResult {
    fn empty(truth: bool) -> Self {
        Self {
            truth,
            positions: [0; 256],
            count: 0,
        }
    }

    fn add(&mut self, position: u16) {
        if self.count < self.positions.len() && !self.positions[..self.count].contains(&position) {
            self.positions[self.count] = position;
            self.count += 1;
        }
    }
}

fn eval_query_node(
    query: &Query<'_>,
    index: u32,
    vector: &Vector<'_>,
) -> Result<MatchResult, SqlError> {
    Ok(
        match query.node(index).ok_or_else(|| syntax("tsquery", ""))? {
            QueryNode::Lexeme {
                text,
                weights,
                prefix,
            } => {
                let mut result = MatchResult::empty(false);
                for lexeme_index in 0..vector.lexeme_count() {
                    let (candidate, positions) =
                        vector.lexeme(lexeme_index).expect("bounded vector index");
                    if candidate == text || prefix && candidate.starts_with(text) {
                        if weights == 0 && positions.is_empty() {
                            result.truth = true;
                        }
                        for position in positions {
                            if weights == 0 || weights & (1 << position.weight) != 0 {
                                result.truth = true;
                                result.add(position.number);
                            }
                        }
                    }
                }
                result
            }
            QueryNode::Not(child) => {
                MatchResult::empty(!eval_query_node(query, child, vector)?.truth)
            }
            QueryNode::And(left, right) => {
                let left = eval_query_node(query, left, vector)?;
                let right = eval_query_node(query, right, vector)?;
                let mut result = MatchResult::empty(left.truth && right.truth);
                if result.truth {
                    for position in left.positions[..left.count]
                        .iter()
                        .chain(&right.positions[..right.count])
                    {
                        result.add(*position);
                    }
                }
                result
            }
            QueryNode::Or(left, right) => {
                let left = eval_query_node(query, left, vector)?;
                let right = eval_query_node(query, right, vector)?;
                let mut result = MatchResult::empty(left.truth || right.truth);
                for position in left.positions[..left.count]
                    .iter()
                    .chain(&right.positions[..right.count])
                {
                    result.add(*position);
                }
                result
            }
            QueryNode::Phrase {
                left,
                right,
                distance,
            } => {
                let left = eval_query_node(query, left, vector)?;
                let right = eval_query_node(query, right, vector)?;
                let mut result = MatchResult::empty(false);
                if left.truth && right.truth {
                    for &left_position in &left.positions[..left.count] {
                        for &right_position in &right.positions[..right.count] {
                            if right_position == left_position.saturating_add(distance) {
                                result.truth = true;
                                result.add(right_position);
                            }
                        }
                    }
                }
                result
            }
        },
    )
}

pub fn matches(vector_text: &str, query_text: &str, arena: &Arena) -> Result<bool, SqlError> {
    let vector = parse_vector(vector_text, arena)?;
    let query = parse_query(query_text, arena)?;
    match query.root() {
        Some(root) => Ok(eval_query_node(&query, root, &vector)?.truth),
        None => Ok(false),
    }
}

fn token_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_english_stop_word(word: &str) -> bool {
    matches!(
        word,
        "a" | "about"
            | "above"
            | "after"
            | "again"
            | "against"
            | "all"
            | "am"
            | "an"
            | "and"
            | "any"
            | "are"
            | "as"
            | "at"
            | "be"
            | "because"
            | "been"
            | "before"
            | "being"
            | "below"
            | "between"
            | "both"
            | "but"
            | "by"
            | "can"
            | "did"
            | "do"
            | "does"
            | "doing"
            | "don"
            | "down"
            | "during"
            | "each"
            | "few"
            | "for"
            | "from"
            | "further"
            | "had"
            | "has"
            | "have"
            | "having"
            | "he"
            | "her"
            | "here"
            | "hers"
            | "herself"
            | "him"
            | "himself"
            | "his"
            | "how"
            | "i"
            | "if"
            | "in"
            | "into"
            | "is"
            | "it"
            | "its"
            | "itself"
            | "me"
            | "more"
            | "just"
            | "most"
            | "my"
            | "myself"
            | "no"
            | "nor"
            | "not"
            | "now"
            | "of"
            | "off"
            | "on"
            | "once"
            | "only"
            | "or"
            | "other"
            | "our"
            | "ours"
            | "ourselves"
            | "out"
            | "over"
            | "own"
            | "same"
            | "she"
            | "should"
            | "so"
            | "some"
            | "such"
            | "than"
            | "that"
            | "the"
            | "their"
            | "theirs"
            | "them"
            | "themselves"
            | "then"
            | "there"
            | "these"
            | "they"
            | "this"
            | "those"
            | "through"
            | "to"
            | "s"
            | "t"
            | "too"
            | "under"
            | "until"
            | "up"
            | "very"
            | "was"
            | "we"
            | "were"
            | "what"
            | "when"
            | "where"
            | "which"
            | "while"
            | "who"
            | "whom"
            | "why"
            | "will"
            | "with"
            | "you"
            | "your"
            | "yours"
            | "yourself"
            | "yourselves"
    )
}

fn english_stem(word: &str) -> StackStr<MAX_TEXT_LEXEME_BYTES> {
    let mut stem = StackStr::<MAX_TEXT_LEXEME_BYTES>::from_str(word);
    if !word.is_ascii() || word.len() <= 2 {
        return stem;
    }
    if let Some(exception) = porter_exception_one(word) {
        return StackStr::from_str(exception);
    }
    let (r1, r2) = porter_regions(word);
    if stem.as_str().ends_with("'s'") {
        stem.truncate(stem.len() - 3);
    } else if stem.as_str().ends_with("'s") {
        stem.truncate(stem.len() - 2);
    } else if stem.as_str().ends_with('\'') {
        stem.truncate(stem.len() - 1);
    }
    if porter_replace(&mut stem, "sses", "ss", 0) {
    } else if stem.as_str().ends_with("ied") || stem.as_str().ends_with("ies") {
        let start = stem.len() - 3;
        stem.truncate(start);
        let _ = stem.write_str(if start > 1 { "i" } else { "ie" });
    } else if !stem.as_str().ends_with("us")
        && !stem.as_str().ends_with("ss")
        && stem.as_str().ends_with('s')
    {
        let end = stem.len() - 2;
        if porter_has_vowel(stem.as_str(), 0, end) {
            stem.truncate(stem.len() - 1);
        }
    }
    if !porter_exception_two(stem.as_str()) {
        if porter_replace(&mut stem, "eedly", "ee", r1)
            || porter_replace(&mut stem, "eed", "ee", r1)
        {
        } else {
            let removed = ["ingly", "edly", "ing", "ed"].into_iter().find(|suffix| {
                stem.as_str().ends_with(suffix)
                    && porter_has_vowel(stem.as_str(), 0, stem.len() - suffix.len())
            });
            if let Some(suffix) = removed {
                stem.truncate(stem.len() - suffix.len());
                if stem.as_str().ends_with("at")
                    || stem.as_str().ends_with("bl")
                    || stem.as_str().ends_with("iz")
                {
                    let _ = stem.write_char('e');
                } else if ["bb", "dd", "ff", "gg", "mm", "nn", "pp", "rr", "tt"]
                    .iter()
                    .any(|ending| stem.as_str().ends_with(ending))
                {
                    stem.truncate(stem.len() - 1);
                } else if r1 >= stem.len() && porter_short_syllable(stem.as_str()) {
                    let _ = stem.write_char('e');
                }
            }
        }
    }
    if stem.len() > 2 && stem.as_str().ends_with('y') {
        let before = stem.as_str().as_bytes()[stem.len() - 2];
        if !porter_vowel(stem.as_str(), stem.len() - 2) && before != b'y' {
            stem.truncate(stem.len() - 1);
            let _ = stem.write_char('i');
        }
    }
    for (suffix, replacement) in [
        ("ization", "ize"),
        ("ational", "ate"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("iveness", "ive"),
        ("tional", "tion"),
        ("biliti", "ble"),
        ("lessli", "less"),
        ("entli", "ent"),
        ("ation", "ate"),
        ("alism", "al"),
        ("aliti", "al"),
        ("ousli", "ous"),
        ("iviti", "ive"),
        ("fulli", "ful"),
        ("enci", "ence"),
        ("anci", "ance"),
        ("abli", "able"),
        ("izer", "ize"),
        ("ator", "ate"),
        ("alli", "al"),
        ("bli", "ble"),
    ] {
        if porter_replace(&mut stem, suffix, replacement, r1) {
            break;
        }
    }
    if stem.as_str().ends_with("ogi") {
        let start = stem.len() - 3;
        if start >= r1 && start > 0 && stem.as_str().as_bytes()[start - 1] == b'l' {
            stem.truncate(start);
            let _ = stem.write_str("og");
        }
    } else if stem.as_str().ends_with("li") {
        let start = stem.len() - 2;
        if start >= r1 && start > 0 && b"cdeghkmnrt".contains(&stem.as_str().as_bytes()[start - 1])
        {
            stem.truncate(start);
        }
    }
    for (suffix, replacement, region) in [
        ("ational", "ate", r1),
        ("tional", "tion", r1),
        ("alize", "al", r1),
        ("icate", "ic", r1),
        ("iciti", "ic", r1),
        ("ical", "ic", r1),
        ("ful", "", r1),
        ("ness", "", r1),
        ("ative", "", r2),
    ] {
        if porter_replace(&mut stem, suffix, replacement, region) {
            break;
        }
    }
    let mut removed_step_four = false;
    for suffix in [
        "ement", "ance", "ence", "able", "ible", "ment", "ant", "ent", "ism", "ate", "iti", "ous",
        "ive", "ize", "al", "er", "ic",
    ] {
        if porter_replace(&mut stem, suffix, "", r2) {
            removed_step_four = true;
            break;
        }
    }
    if !removed_step_four && stem.as_str().ends_with("ion") {
        let start = stem.len() - 3;
        if start >= r2 && start > 0 && matches!(stem.as_str().as_bytes()[start - 1], b's' | b't') {
            stem.truncate(start);
        }
    }
    if stem.as_str().ends_with('e') {
        let start = stem.len() - 1;
        if start >= r2 || (start >= r1 && !porter_short_syllable(&stem.as_str()[..start])) {
            stem.truncate(start);
        }
    } else if stem.as_str().ends_with("ll") && stem.len() > r2 {
        stem.truncate(stem.len() - 1);
    }
    stem
}

fn porter_vowel(word: &str, index: usize) -> bool {
    let bytes = word.as_bytes();
    match bytes[index] {
        b'a' | b'e' | b'i' | b'o' | b'u' => true,
        b'y' => {
            let mut start = index;
            while start > 0 && bytes[start] == b'y' {
                start -= 1;
            }
            if bytes[start] == b'y' {
                index % 2 == 1
            } else {
                let preceding_is_vowel = matches!(bytes[start], b'a' | b'e' | b'i' | b'o' | b'u');
                if (index - start) % 2 == 1 {
                    !preceding_is_vowel
                } else {
                    preceding_is_vowel
                }
            }
        }
        _ => false,
    }
}

fn porter_has_vowel(word: &str, start: usize, end: usize) -> bool {
    (start..end.min(word.len())).any(|index| porter_vowel(word, index))
}

fn porter_regions(word: &str) -> (usize, usize) {
    let first = if word.starts_with("gener") {
        5
    } else if word.starts_with("commun") {
        6
    } else if word.starts_with("arsen") {
        5
    } else {
        porter_region_after(word, 0)
    };
    (first, porter_region_after(word, first))
}

fn porter_region_after(word: &str, from: usize) -> usize {
    for index in from.saturating_add(1)..word.len() {
        if porter_vowel(word, index - 1) && !porter_vowel(word, index) {
            return index + 1;
        }
    }
    word.len()
}

fn porter_short_syllable(word: &str) -> bool {
    let length = word.len();
    if length >= 3 {
        let last = word.as_bytes()[length - 1];
        !porter_vowel(word, length - 3)
            && porter_vowel(word, length - 2)
            && !porter_vowel(word, length - 1)
            && !matches!(last, b'w' | b'x' | b'y')
    } else {
        length == 2 && porter_vowel(word, 0) && !porter_vowel(word, 1)
    }
}

fn porter_replace(
    word: &mut StackStr<MAX_TEXT_LEXEME_BYTES>,
    suffix: &str,
    replacement: &str,
    region: usize,
) -> bool {
    if !word.as_str().ends_with(suffix) {
        return false;
    }
    let start = word.len() - suffix.len();
    if start < region {
        return false;
    }
    word.truncate(start);
    let _ = word.write_str(replacement);
    true
}

fn porter_exception_one(word: &str) -> Option<&'static str> {
    Some(match word {
        "skis" => "ski",
        "skies" => "sky",
        "dying" => "die",
        "lying" => "lie",
        "tying" => "tie",
        "idly" => "idl",
        "gently" => "gentl",
        "ugly" => "ugli",
        "early" => "earli",
        "only" => "onli",
        "singly" => "singl",
        "sky" => "sky",
        "news" => "news",
        "howe" => "howe",
        "atlas" => "atlas",
        "cosmos" => "cosmos",
        "bias" => "bias",
        "andes" => "andes",
        _ => return None,
    })
}

fn porter_exception_two(word: &str) -> bool {
    matches!(
        word,
        "inning" | "outing" | "canning" | "herring" | "earring" | "proceed" | "exceed" | "succeed"
    )
}

pub(crate) fn normalize_token<'a>(
    token: &str,
    config: TextSearchConfig,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let mut lower = StackStr::<MAX_TEXT_LEXEME_BYTES>::new();
    for character in token.chars().flat_map(char::to_lowercase) {
        lower
            .write_char(character)
            .map_err(|_| capacity("text-search lexeme"))?;
    }
    if config == TextSearchConfig::English && is_english_stop_word(lower.as_str()) {
        return Ok(None);
    }
    let stemmed = if config == TextSearchConfig::English {
        english_stem(lower.as_str())
    } else {
        lower
    };
    Ok(Some(
        arena
            .alloc_str(stemmed.as_str())
            .map_err(|_| arena_full("text-search lexeme"))?,
    ))
}

fn token_type(token: &str) -> u8 {
    if numeric_token(token) {
        let unsigned = token.trim_start_matches(['+', '-']);
        let dots = unsigned.bytes().filter(|byte| *byte == b'.').count();
        if unsigned.bytes().any(|byte| matches!(byte, b'e' | b'E')) {
            7 // sfloat
        } else if dots > 1 {
            8 // version
        } else if dots == 1 {
            20 // float
        } else if token
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b'+' | b'-'))
        {
            21 // int
        } else {
            22 // uint
        }
    } else if token.contains('.') && token.chars().any(char::is_alphabetic) {
        6 // host
    } else if token.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        1 // asciiword
    } else if token.chars().all(char::is_alphabetic) {
        2 // word
    } else if token.bytes().all(|byte| byte.is_ascii_digit()) {
        22 // uint
    } else {
        3 // numword
    }
}

fn numeric_token(token: &str) -> bool {
    let unsigned = token.strip_prefix(['+', '-']).unwrap_or(token);
    if unsigned.is_empty() {
        return false;
    }
    if let Some((mantissa, exponent)) = unsigned.split_once(['e', 'E']) {
        if exponent.contains(['e', 'E']) {
            return false;
        }
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        return !exponent.is_empty()
            && exponent.bytes().all(|byte| byte.is_ascii_digit())
            && mantissa
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            && mantissa.bytes().filter(|byte| *byte == b'.').count() <= 1;
    }
    unsigned
        .split('.')
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn builtin_token_is_mapped(token_type: u8) -> bool {
    matches!(token_type, 1..=11 | 15..=22)
}

#[derive(Clone, Copy)]
struct DocumentLexeme<'a> {
    token: &'a str,
    position: u16,
}

type DocumentLexemes<'a> = &'a [DocumentLexeme<'a>];

fn document_tokens_with<'a>(
    document: &str,
    arena: &'a Arena,
    mut normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<DocumentLexemes<'a>, SqlError> {
    let mut tokens = ArenaList::new(arena);
    let mut position = 0u32;
    let mut emit = |kind: u8, source: &str| -> Result<(), SqlError> {
        if source.is_empty() {
            return Ok(());
        }
        match normalize(kind, source, arena)? {
            TextSearchLexeme::Unmapped => {}
            TextSearchLexeme::StopWord => position = position.saturating_add(1),
            TextSearchLexeme::Lexeme(token) => {
                position = position.saturating_add(1);
                tokens
                    .push(DocumentLexeme {
                        token,
                        position: position.min(16_383) as u16,
                    })
                    .map_err(|_| arena_full("text-search document"))?;
            }
        }
        Ok(())
    };
    let bytes = document.as_bytes();
    let mut at = 0usize;
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            let begin = at;
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            emit(12, &document[begin..at])?;
            continue;
        }
        if bytes[at] == b'<'
            && let Some(relative) = bytes[at..].iter().position(|byte| *byte == b'>')
        {
            let end = at + relative + 1;
            emit(13, &document[at..end])?;
            at = end;
            continue;
        }
        if bytes[at] == b'&'
            && let Some(relative) = bytes[at..].iter().position(|byte| *byte == b';')
        {
            let end = at + relative + 1;
            emit(23, &document[at..end])?;
            at = end;
            continue;
        }
        if !bytes[at].is_ascii_alphanumeric()
            && bytes[at] < 0x80
            && !(matches!(bytes[at], b'/' | b'-' | b'+')
                && bytes.get(at + 1).is_some_and(u8::is_ascii_alphanumeric))
        {
            let begin = at;
            at += 1;
            while at < bytes.len()
                && !bytes[at].is_ascii_alphanumeric()
                && !bytes[at].is_ascii_whitespace()
                && !matches!(bytes[at], b'<' | b'&')
            {
                at += 1;
            }
            emit(12, &document[begin..at])?;
            continue;
        }
        let begin = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !matches!(bytes[at], b'<' | b'>' | b'&')
        {
            at += 1;
        }
        let mut end = at;
        while end > begin
            && matches!(
                bytes[end - 1],
                b',' | b';' | b'!' | b'?' | b')' | b']' | b'}'
            )
        {
            end -= 1;
        }
        let source = &document[begin..end];
        if let Some(protocol_end) = source.find("://") {
            let protocol_end = protocol_end + 3;
            emit(14, &source[..protocol_end])?;
            let url = &source[protocol_end..];
            if !url.is_empty() {
                emit(5, url)?;
                let host_end = url.find('/').unwrap_or(url.len());
                emit(6, &url[..host_end])?;
                if host_end < url.len() {
                    emit(18, &url[host_end..])?;
                }
            }
        } else if source.contains('@')
            && source
                .split_once('@')
                .is_some_and(|(left, right)| !left.is_empty() && right.contains('.'))
        {
            emit(4, source)?;
        } else if source.starts_with('/') {
            emit(19, source)?;
        } else if source.contains('/') {
            emit(5, source)?;
            let host_end = source.find('/').expect("contains slash");
            emit(6, &source[..host_end])?;
            emit(18, &source[host_end..])?;
        } else if numeric_token(source) {
            emit(token_type(source), source)?;
        } else if source.contains('-')
            && source
                .split('-')
                .all(|part| !part.is_empty() && part.chars().all(char::is_alphabetic))
        {
            let unicode = source.bytes().any(|byte| !byte.is_ascii());
            emit(if unicode { 17 } else { 16 }, source)?;
            for part in source.split('-') {
                emit(if unicode { 10 } else { 11 }, part)?;
            }
        } else if source.split_once('-').is_some_and(|(left, right)| {
            left.chars().all(char::is_alphabetic) && right.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            let (left, _) = source.split_once('-').expect("checked hyphen");
            emit(token_type(left), left)?;
            emit(21, &source[left.len()..])?;
        } else if source.contains('-') {
            let mut piece_start = 0usize;
            for (separator, _) in source
                .match_indices('-')
                .chain(core::iter::once((source.len(), "")))
            {
                if separator > piece_start {
                    let part = &source[piece_start..separator];
                    emit(token_type(part), part)?;
                }
                piece_start = separator + 1;
            }
        } else {
            emit(token_type(source), source)?;
        }
        if end < at {
            emit(12, &document[end..at])?;
        }
        at = at.max(end);
    }
    Ok(tokens.as_slice())
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ParserToken<'a> {
    pub kind: u8,
    pub text: &'a str,
}

pub(crate) fn parse_document<'a>(
    document: &str,
    arena: &'a Arena,
) -> Result<&'a [ParserToken<'a>], SqlError> {
    let mut tokens = ArenaList::new(arena);
    let _ = document_tokens_with(document, arena, |kind, text, arena| {
        tokens
            .push(ParserToken {
                kind,
                text: arena
                    .alloc_str(text)
                    .map_err(|_| arena_full("text-search parser output"))?,
            })
            .map_err(|_| arena_full("text-search parser output"))?;
        Ok(TextSearchLexeme::Unmapped)
    })?;
    Ok(tokens.as_slice())
}

pub(crate) const TOKEN_TYPES: [(&str, &str); 23] = [
    ("asciiword", "Word, all ASCII"),
    ("word", "Word, all letters"),
    ("numword", "Word, letters and digits"),
    ("email", "Email address"),
    ("url", "URL"),
    ("host", "Host"),
    ("sfloat", "Scientific notation"),
    ("version", "Version number"),
    ("hword_numpart", "Hyphenated word part, letters and digits"),
    ("hword_part", "Hyphenated word part, all letters"),
    ("hword_asciipart", "Hyphenated word part, all ASCII"),
    ("blank", "Space symbols"),
    ("tag", "XML tag"),
    ("protocol", "Protocol head"),
    ("numhword", "Hyphenated word, letters and digits"),
    ("asciihword", "Hyphenated word, all ASCII"),
    ("hword", "Hyphenated word, all letters"),
    ("url_path", "URL path"),
    ("file", "File or path name"),
    ("float", "Decimal notation"),
    ("int", "Signed integer"),
    ("uint", "Unsigned integer"),
    ("entity", "XML entity"),
];

fn document_tokens<'a>(
    config: TextSearchConfig,
    document: &str,
    arena: &'a Arena,
) -> Result<DocumentLexemes<'a>, SqlError> {
    document_tokens_with(document, arena, |kind, token, arena| {
        if !builtin_token_is_mapped(kind) {
            return Ok(TextSearchLexeme::Unmapped);
        }
        Ok(match normalize_token(token, config, arena)? {
            Some(lexeme) => TextSearchLexeme::Lexeme(lexeme),
            None => TextSearchLexeme::StopWord,
        })
    })
}

pub fn to_tsvector<'a>(
    config: TextSearchConfig,
    document: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let tokens = document_tokens(config, document, arena)?;
    let mut raw = ArenaText::new(
        arena,
        document
            .len()
            .saturating_mul(3)
            .saturating_add(tokens.len() * 12 + 1),
        "tsvector",
    )?;
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(token.token)?;
        raw.push_byte(b':')?;
        write_u16(&mut raw, token.position)?;
    }
    canonical_vector(raw.finish(), arena)
}

pub(crate) fn to_tsvector_with<'a>(
    document: &str,
    arena: &'a Arena,
    normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    let tokens = document_tokens_with(document, arena, normalize)?;
    vector_from_document_tokens(document.len(), tokens, arena)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JsonTextSearchFilter(u8);

impl JsonTextSearchFilter {
    const STRING: u8 = 1;
    const NUMERIC: u8 = 2;
    const BOOLEAN: u8 = 4;
    const KEY: u8 = 8;

    pub(crate) const STRINGS: Self = Self(Self::STRING);

    pub(crate) fn parse(value: crate::sql::json::Json<'_>) -> Result<Self, SqlError> {
        let crate::sql::json::Json::Array(items) = value else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "wrong flag in flag array: JSON flag array must be an array"
            ));
        };
        let mut bits = 0u8;
        for item in items {
            let crate::sql::json::Json::Str(flag) = item else {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "wrong flag in flag array: JSON flag must be a string"
                ));
            };
            bits |= match *flag {
                "string" => Self::STRING,
                "numeric" => Self::NUMERIC,
                "boolean" => Self::BOOLEAN,
                "key" => Self::KEY,
                "all" => Self::STRING | Self::NUMERIC | Self::BOOLEAN | Self::KEY,
                other => {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "wrong flag in flag array: \"{}\"",
                        other
                    ));
                }
            };
        }
        Ok(Self(bits))
    }

    const fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

pub(crate) fn json_to_tsvector_with<'a>(
    value: crate::sql::json::Json<'a>,
    filter: JsonTextSearchFilter,
    arena: &'a Arena,
    mut normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    const MAX_SEGMENTS: usize = 4_096;
    fn collect<'a>(
        value: crate::sql::json::Json<'a>,
        filter: JsonTextSearchFilter,
        output: &mut [&'a str; MAX_SEGMENTS],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        fn push<'a>(
            text: &'a str,
            output: &mut [&'a str; MAX_SEGMENTS],
            count: &mut usize,
        ) -> Result<(), SqlError> {
            let slot = output
                .get_mut(*count)
                .ok_or_else(|| capacity("JSON text-search values"))?;
            *slot = text;
            *count += 1;
            Ok(())
        }
        match value {
            crate::sql::json::Json::Null => {}
            crate::sql::json::Json::Bool(true)
                if filter.contains(JsonTextSearchFilter::BOOLEAN) =>
            {
                push("true", output, count)?
            }
            crate::sql::json::Json::Bool(false)
                if filter.contains(JsonTextSearchFilter::BOOLEAN) =>
            {
                push("false", output, count)?
            }
            crate::sql::json::Json::Number(number)
                if filter.contains(JsonTextSearchFilter::NUMERIC) =>
            {
                push(number, output, count)?
            }
            crate::sql::json::Json::Str(text) if filter.contains(JsonTextSearchFilter::STRING) => {
                push(text, output, count)?
            }
            crate::sql::json::Json::Array(items) => {
                for item in items {
                    collect(*item, filter, output, count)?;
                }
            }
            crate::sql::json::Json::Object(members) => {
                for (key, member) in members {
                    if filter.contains(JsonTextSearchFilter::KEY) {
                        push(key, output, count)?;
                    }
                    collect(*member, filter, output, count)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut segments = [""; MAX_SEGMENTS];
    let mut segment_count = 0usize;
    collect(value, filter, &mut segments, &mut segment_count)?;
    let source_bytes = segments[..segment_count]
        .iter()
        .try_fold(0usize, |sum, value| sum.checked_add(value.len()))
        .ok_or_else(|| capacity("JSON text-search values"))?;
    let mut raw = ArenaText::new(
        arena,
        source_bytes
            .saturating_mul(16)
            .saturating_add(segment_count * 4 + 1),
        "JSON tsvector",
    )?;
    let mut position_base = 0u16;
    for segment in &segments[..segment_count] {
        let rendered = to_tsvector_with(segment, arena, &mut normalize)?;
        let parsed = parse_vector(rendered, arena)?;
        let mut maximum = 0u16;
        for index in 0..parsed.lexeme_count() {
            let (lexeme, positions) = parsed.lexeme(index).expect("vector index");
            if raw.len != 0 {
                raw.push_byte(b' ')?;
            }
            raw.push_quoted(lexeme)?;
            if !positions.is_empty() {
                raw.push_byte(b':')?;
                for (offset, position) in positions.iter().enumerate() {
                    if offset != 0 {
                        raw.push_byte(b',')?;
                    }
                    maximum = maximum.max(position.number);
                    write_u16(
                        &mut raw,
                        position.number.saturating_add(position_base).min(16_383),
                    )?;
                    match position.weight {
                        3 => raw.push_byte(b'A')?,
                        2 => raw.push_byte(b'B')?,
                        1 => raw.push_byte(b'C')?,
                        _ => {}
                    }
                }
            }
        }
        position_base = position_base
            .saturating_add(maximum)
            .saturating_add(1)
            .min(16_383);
    }
    canonical_vector(raw.finish(), arena)
}

fn vector_from_document_tokens<'a>(
    document_len: usize,
    tokens: &[DocumentLexeme<'a>],
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut raw = ArenaText::new(
        arena,
        document_len
            .saturating_mul(3)
            .saturating_add(tokens.len() * 12 + 1),
        "tsvector",
    )?;
    for (index, token) in tokens.iter().enumerate() {
        if index > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(token.token)?;
        raw.push_byte(b':')?;
        write_u16(&mut raw, token.position)?;
    }
    canonical_vector(raw.finish(), arena)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryInput {
    Plain,
    Phrase,
    Websearch,
}

pub fn text_to_query<'a>(
    config: TextSearchConfig,
    source: &str,
    mode: QueryInput,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut normalize = |kind: u8, token: &str, arena: &'a Arena| {
        if !builtin_token_is_mapped(kind) {
            return Ok(TextSearchLexeme::Unmapped);
        }
        Ok(match normalize_token(token, config, arena)? {
            Some(lexeme) => TextSearchLexeme::Lexeme(lexeme),
            None => TextSearchLexeme::StopWord,
        })
    };
    text_to_query_normalized(source, mode, arena, &mut normalize)
}

pub(crate) fn text_to_query_with<'a>(
    source: &str,
    mode: QueryInput,
    arena: &'a Arena,
    mut normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    text_to_query_normalized(source, mode, arena, &mut normalize)
}

pub(crate) fn explicit_text_to_query_with<'a>(
    source: &'a str,
    arena: &'a Arena,
    mut normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    let parsed = parse_query(source, arena)?;
    let Some(root) = parsed.root() else {
        return Ok("");
    };
    let mut normalized = Query::empty(arena);
    let result = normalize_explicit_node(&parsed, root, &mut normalized, arena, &mut normalize, 0)?;
    normalized.root = result.root;
    format_query(&normalized, arena)
}

#[derive(Clone, Copy)]
struct NormalizedQueryNode {
    root: Option<u32>,
    leading_gap: u16,
    trailing_gap: u16,
}

fn clone_normalized_query_node<'a>(
    source: &Query<'a>,
    index: u32,
    target: &mut Query<'a>,
    weights: u8,
    prefix: bool,
) -> Result<u32, SqlError> {
    Ok(
        match source.node(index).ok_or_else(|| syntax("tsquery", ""))? {
            QueryNode::Lexeme { text, .. } => target.push(QueryNode::Lexeme {
                text,
                weights,
                prefix,
            })?,
            QueryNode::Not(child) => {
                let child = clone_normalized_query_node(source, child, target, weights, prefix)?;
                target.push(QueryNode::Not(child))?
            }
            QueryNode::And(left, right) => {
                let left = clone_normalized_query_node(source, left, target, weights, prefix)?;
                let right = clone_normalized_query_node(source, right, target, weights, prefix)?;
                target.push(QueryNode::And(left, right))?
            }
            QueryNode::Or(left, right) => {
                let left = clone_normalized_query_node(source, left, target, weights, prefix)?;
                let right = clone_normalized_query_node(source, right, target, weights, prefix)?;
                target.push(QueryNode::Or(left, right))?
            }
            QueryNode::Phrase {
                left,
                right,
                distance,
            } => {
                let left = clone_normalized_query_node(source, left, target, weights, prefix)?;
                let right = clone_normalized_query_node(source, right, target, weights, prefix)?;
                target.push(QueryNode::Phrase {
                    left,
                    right,
                    distance,
                })?
            }
        },
    )
}

fn normalize_explicit_node<'a>(
    source: &Query<'a>,
    index: u32,
    target: &mut Query<'a>,
    arena: &'a Arena,
    normalize: &mut dyn FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
    depth: usize,
) -> Result<NormalizedQueryNode, SqlError> {
    if depth == MAX_QUERY_DEPTH {
        return Err(capacity("tsquery nesting"));
    }
    let result = match source.node(index).ok_or_else(|| syntax("tsquery", ""))? {
        QueryNode::Lexeme {
            text,
            weights,
            prefix,
        } => {
            let rendered = text_to_query_normalized(text, QueryInput::Phrase, arena, normalize)?;
            let parsed = parse_query(rendered, arena)?;
            let root = parsed
                .root()
                .map(|root| clone_normalized_query_node(&parsed, root, target, weights, prefix))
                .transpose()?;
            NormalizedQueryNode {
                root,
                leading_gap: u16::from(root.is_none()),
                trailing_gap: u16::from(root.is_none()),
            }
        }
        QueryNode::Not(child) => {
            let child =
                normalize_explicit_node(source, child, target, arena, normalize, depth + 1)?;
            NormalizedQueryNode {
                root: child
                    .root
                    .map(|child| target.push(QueryNode::Not(child)))
                    .transpose()?,
                leading_gap: child.leading_gap,
                trailing_gap: child.trailing_gap,
            }
        }
        QueryNode::And(left, right) | QueryNode::Or(left, right) => {
            let left = normalize_explicit_node(source, left, target, arena, normalize, depth + 1)?;
            let right =
                normalize_explicit_node(source, right, target, arena, normalize, depth + 1)?;
            let root = match (left.root, right.root) {
                (Some(left), Some(right)) => Some(target.push(
                    if matches!(source.node(index), Some(QueryNode::And(..))) {
                        QueryNode::And(left, right)
                    } else {
                        QueryNode::Or(left, right)
                    },
                )?),
                (Some(root), None) | (None, Some(root)) => Some(root),
                (None, None) => None,
            };
            NormalizedQueryNode {
                root,
                leading_gap: if left.root.is_none() {
                    left.leading_gap.saturating_add(right.leading_gap)
                } else {
                    left.leading_gap
                },
                trailing_gap: if right.root.is_none() {
                    right.trailing_gap.saturating_add(left.trailing_gap)
                } else {
                    right.trailing_gap
                },
            }
        }
        QueryNode::Phrase {
            left,
            right,
            distance,
        } => {
            let left = normalize_explicit_node(source, left, target, arena, normalize, depth + 1)?;
            let right =
                normalize_explicit_node(source, right, target, arena, normalize, depth + 1)?;
            let root = match (left.root, right.root) {
                (Some(left_root), Some(right_root)) => Some(
                    target.push(QueryNode::Phrase {
                        left: left_root,
                        right: right_root,
                        distance: distance
                            .saturating_add(left.trailing_gap)
                            .saturating_add(right.leading_gap),
                    })?,
                ),
                (Some(root), None) | (None, Some(root)) => Some(root),
                (None, None) => None,
            };
            NormalizedQueryNode {
                root,
                leading_gap: if left.root.is_none() {
                    left.leading_gap
                        .saturating_add(distance)
                        .saturating_add(right.leading_gap)
                } else {
                    left.leading_gap
                },
                trailing_gap: if right.root.is_none() {
                    right
                        .trailing_gap
                        .saturating_add(distance)
                        .saturating_add(left.trailing_gap)
                } else {
                    right.trailing_gap
                },
            }
        }
    };
    Ok(result)
}

fn text_to_query_normalized<'a>(
    source: &str,
    mode: QueryInput,
    arena: &'a Arena,
    normalize: &mut dyn FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    if mode == QueryInput::Websearch {
        return websearch_to_query(source, arena, normalize);
    }
    let tokens = document_tokens_with(source, arena, |kind, token, arena| {
        normalize(kind, token, arena)
    })?;
    let mut raw = ArenaText::new(
        arena,
        source
            .len()
            .saturating_mul(4)
            .saturating_add(tokens.len() * 16 + 1),
        "tsquery",
    )?;
    let mut prior_position = 0u16;
    for (emitted, token) in tokens.iter().enumerate() {
        if emitted > 0 {
            match mode {
                QueryInput::Plain => raw.push_str(" & ")?,
                QueryInput::Phrase => {
                    let distance = token.position.saturating_sub(prior_position);
                    if distance == 1 {
                        raw.push_str(" <-> ")?;
                    } else {
                        raw.push_str(" <")?;
                        write_u16(&mut raw, distance)?;
                        raw.push_str("> ")?;
                    }
                }
                QueryInput::Websearch => unreachable!("web search uses its parser"),
            }
        }
        raw.push_quoted(token.token)?;
        prior_position = token.position;
    }
    canonical_query(raw.finish(), arena)
}

fn websearch_to_query<'a>(
    source: &str,
    arena: &'a Arena,
    normalize: &mut dyn FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    let mut groups = ArenaList::new(arena);
    let mut current: Option<&'a str> = None;
    let mut at = 0usize;
    let bytes = source.as_bytes();
    while at < bytes.len() {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        if source[at..]
            .get(..2)
            .is_some_and(|word| word.eq_ignore_ascii_case("or"))
            && bytes
                .get(at + 2)
                .is_none_or(|byte| byte.is_ascii_whitespace())
        {
            at += 2;
            if let Some(query) = current.take() {
                groups.push(query).map_err(|_| arena_full("tsquery"))?;
            }
            continue;
        }
        let negated = bytes[at] == b'-';
        if negated {
            at += 1;
            while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
                at += 1;
            }
            if at == bytes.len() {
                break;
            }
        }
        let (begin, end, mode) = if bytes[at] == b'"' {
            at += 1;
            let begin = at;
            while at < bytes.len() && bytes[at] != b'"' {
                at += 1;
            }
            let end = at;
            if at < bytes.len() {
                at += 1;
            }
            (begin, end, QueryInput::Phrase)
        } else {
            let begin = at;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            (begin, at, QueryInput::Phrase)
        };
        let mut term = text_to_query_normalized(&source[begin..end], mode, arena, normalize)?;
        if term.is_empty() {
            continue;
        }
        if negated {
            term = not_query(term, arena)?;
        }
        current = Some(match current {
            Some(left) => and_queries(left, term, arena)?,
            None => term,
        });
    }
    if let Some(query) = current {
        groups.push(query).map_err(|_| arena_full("tsquery"))?;
    }
    let mut result: Option<&'a str> = None;
    for group in groups.as_slice() {
        result = Some(match result {
            Some(left) => combine_queries(left, group, " | ", arena)?,
            None => group,
        });
    }
    Ok(result.unwrap_or(""))
}

pub fn strip_vector<'a>(source: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let vector = parse_vector(source, arena)?;
    let mut out = ArenaText::new(
        arena,
        source.len().saturating_mul(2).saturating_add(1),
        "tsvector",
    )?;
    let mut prior: Option<&str> = None;
    for index in 0..vector.lexeme_count() {
        let (text, _) = vector.lexeme(index).expect("vector index");
        if prior == Some(text) {
            continue;
        }
        if prior.is_some() {
            out.push_byte(b' ')?;
        }
        out.push_quoted(text)?;
        prior = Some(text);
    }
    Ok(out.finish())
}

pub fn set_weight<'a>(
    source: &str,
    weight: u8,
    selected: Option<&[&str]>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let vector = parse_vector(source, arena)?;
    let mut raw = ArenaText::new(
        arena,
        source.len().saturating_mul(3).saturating_add(64),
        "tsvector",
    )?;
    for index in 0..vector.lexeme_count() {
        let (text, positions) = vector.lexeme(index).expect("vector index");
        if index > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(text)?;
        if !positions.is_empty() {
            raw.push_byte(b':')?;
            let change = selected.is_none_or(|values| values.contains(&text));
            for (offset, position) in positions.iter().enumerate() {
                if offset > 0 {
                    raw.push_byte(b',')?;
                }
                write_u16(&mut raw, position.number)?;
                match if change { weight } else { position.weight } {
                    3 => raw.push_byte(b'A')?,
                    2 => raw.push_byte(b'B')?,
                    1 => raw.push_byte(b'C')?,
                    _ => {}
                }
            }
        }
    }
    canonical_vector(raw.finish(), arena)
}

pub fn delete_lexemes<'a>(
    source: &str,
    deleted: &[&str],
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let vector = parse_vector(source, arena)?;
    let mut raw = ArenaText::new(
        arena,
        source.len().saturating_mul(2).saturating_add(1),
        "tsvector",
    )?;
    let mut emitted = 0usize;
    for index in 0..vector.lexeme_count() {
        let (text, positions) = vector.lexeme(index).expect("vector index");
        if deleted.contains(&text) {
            continue;
        }
        if emitted > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(text)?;
        if !positions.is_empty() {
            raw.push_byte(b':')?;
            for (offset, position) in positions.iter().enumerate() {
                if offset > 0 {
                    raw.push_byte(b',')?;
                }
                write_u16(&mut raw, position.number)?;
                match position.weight {
                    3 => raw.push_byte(b'A')?,
                    2 => raw.push_byte(b'B')?,
                    1 => raw.push_byte(b'C')?,
                    _ => {}
                }
            }
        }
        emitted += 1;
    }
    canonical_vector(raw.finish(), arena)
}

pub fn filter_weights<'a>(
    source: &str,
    weights: u8,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let vector = parse_vector(source, arena)?;
    let mut raw = ArenaText::new(
        arena,
        source.len().saturating_mul(2).saturating_add(1),
        "tsvector",
    )?;
    let mut emitted = 0usize;
    for index in 0..vector.lexeme_count() {
        let (text, positions) = vector.lexeme(index).expect("vector index");
        let kept = positions
            .iter()
            .filter(|position| weights & (1 << position.weight) != 0)
            .count();
        if kept == 0 {
            continue;
        }
        if emitted > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(text)?;
        raw.push_byte(b':')?;
        let mut position_index = 0usize;
        for position in positions {
            if weights & (1 << position.weight) == 0 {
                continue;
            }
            if position_index > 0 {
                raw.push_byte(b',')?;
            }
            write_u16(&mut raw, position.number)?;
            match position.weight {
                3 => raw.push_byte(b'A')?,
                2 => raw.push_byte(b'B')?,
                1 => raw.push_byte(b'C')?,
                _ => {}
            }
            position_index += 1;
        }
        emitted += 1;
    }
    canonical_vector(raw.finish(), arena)
}

pub fn query_tree<'a>(source: &'a str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let query = parse_query(source, arena)?;
    let Some(root) = query.root() else {
        return Ok("");
    };
    fn clean<'a>(
        source: &Query<'a>,
        node: u32,
        output: &mut Query<'a>,
    ) -> Result<Option<u32>, SqlError> {
        Ok(match source.node(node).expect("query node") {
            node @ QueryNode::Lexeme { .. } => Some(output.push(node)?),
            QueryNode::Not(_) => None,
            QueryNode::And(left, right) => {
                let left = clean(source, left, output)?;
                let right = clean(source, right, output)?;
                match (left, right) {
                    (Some(left), Some(right)) => Some(output.push(QueryNode::And(left, right))?),
                    (Some(node), None) | (None, Some(node)) => Some(node),
                    (None, None) => None,
                }
            }
            QueryNode::Or(left, right) => {
                let left = clean(source, left, output)?;
                let right = clean(source, right, output)?;
                match (left, right) {
                    (Some(left), Some(right)) => Some(output.push(QueryNode::Or(left, right))?),
                    _ => None,
                }
            }
            QueryNode::Phrase {
                left,
                right,
                distance,
            } => {
                let left = clean(source, left, output)?;
                let right = clean(source, right, output)?;
                match (left, right) {
                    (Some(left), Some(right)) => Some(output.push(QueryNode::Phrase {
                        left,
                        right,
                        distance,
                    })?),
                    (Some(node), None) | (None, Some(node)) => Some(node),
                    (None, None) => None,
                }
            }
        })
    }
    let mut output = Query::empty(arena);
    output.root = clean(&query, root, &mut output)?;
    if output.root.is_none() {
        return Ok("T");
    }
    format_query(&output, arena)
}

fn query_subtree_equal(
    left: &Query<'_>,
    left_index: u32,
    right: &Query<'_>,
    right_index: u32,
) -> bool {
    match (
        left.node(left_index).expect("query node"),
        right.node(right_index).expect("query node"),
    ) {
        (QueryNode::Lexeme { text: a, .. }, QueryNode::Lexeme { text: b, .. }) => a == b,
        (QueryNode::Not(a), QueryNode::Not(b)) => query_subtree_equal(left, a, right, b),
        (QueryNode::And(al, ar), QueryNode::And(bl, br))
        | (QueryNode::Or(al, ar), QueryNode::Or(bl, br)) => {
            query_subtree_equal(left, al, right, bl) && query_subtree_equal(left, ar, right, br)
        }
        (
            QueryNode::Phrase {
                left: al,
                right: ar,
                distance: ad,
            },
            QueryNode::Phrase {
                left: bl,
                right: br,
                distance: bd,
            },
        ) => {
            ad == bd
                && query_subtree_equal(left, al, right, bl)
                && query_subtree_equal(left, ar, right, br)
        }
        _ => false,
    }
}

fn clone_query_subtree<'a>(
    source: &Query<'a>,
    index: u32,
    output: &mut Query<'a>,
) -> Result<u32, SqlError> {
    let node = match source.node(index).expect("query node") {
        node @ QueryNode::Lexeme { .. } => node,
        QueryNode::Not(child) => QueryNode::Not(clone_query_subtree(source, child, output)?),
        QueryNode::And(left, right) => QueryNode::And(
            clone_query_subtree(source, left, output)?,
            clone_query_subtree(source, right, output)?,
        ),
        QueryNode::Or(left, right) => QueryNode::Or(
            clone_query_subtree(source, left, output)?,
            clone_query_subtree(source, right, output)?,
        ),
        QueryNode::Phrase {
            left,
            right,
            distance,
        } => QueryNode::Phrase {
            left: clone_query_subtree(source, left, output)?,
            right: clone_query_subtree(source, right, output)?,
            distance,
        },
    };
    output.push(node)
}

fn rewrite_query_subtree<'a>(
    source: &Query<'a>,
    index: u32,
    target: &Query<'a>,
    replacement: &Query<'a>,
    output: &mut Query<'a>,
) -> Result<Option<u32>, SqlError> {
    if target
        .root
        .is_some_and(|root| query_subtree_equal(source, index, target, root))
    {
        return replacement
            .root
            .map(|root| clone_query_subtree(replacement, root, output))
            .transpose();
    }
    let node = match source.node(index).expect("query node") {
        node @ QueryNode::Lexeme { .. } => return output.push(node).map(Some),
        QueryNode::Not(child) => {
            let Some(child) = rewrite_query_subtree(source, child, target, replacement, output)?
            else {
                return Ok(None);
            };
            QueryNode::Not(child)
        }
        QueryNode::And(left, right) | QueryNode::Or(left, right) => {
            let left = rewrite_query_subtree(source, left, target, replacement, output)?;
            let right = rewrite_query_subtree(source, right, target, replacement, output)?;
            match (left, right) {
                (Some(left), Some(right)) => {
                    if matches!(source.node(index), Some(QueryNode::And(..))) {
                        QueryNode::And(left, right)
                    } else {
                        QueryNode::Or(left, right)
                    }
                }
                (Some(node), None) | (None, Some(node)) => return Ok(Some(node)),
                (None, None) => return Ok(None),
            }
        }
        QueryNode::Phrase {
            left,
            right,
            distance,
        } => {
            let left = rewrite_query_subtree(source, left, target, replacement, output)?;
            let right = rewrite_query_subtree(source, right, target, replacement, output)?;
            match (left, right) {
                (Some(left), Some(right)) => QueryNode::Phrase {
                    left,
                    right,
                    distance,
                },
                (Some(node), None) | (None, Some(node)) => return Ok(Some(node)),
                (None, None) => return Ok(None),
            }
        }
    };
    output.push(node).map(Some)
}

pub fn rewrite_query<'a>(
    source: &str,
    target: &str,
    replacement: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let source = arena.alloc_str(source).map_err(|_| arena_full("tsquery"))?;
    let target = arena.alloc_str(target).map_err(|_| arena_full("tsquery"))?;
    let replacement = arena
        .alloc_str(replacement)
        .map_err(|_| arena_full("tsquery"))?;
    let source = parse_query(source, arena)?;
    let target = parse_query(target, arena)?;
    let replacement = parse_query(replacement, arena)?;
    let Some(root) = source.root else {
        return Ok("");
    };
    if target.root.is_none() {
        return format_query(&source, arena);
    }
    let mut output = Query::empty(arena);
    output.root = rewrite_query_subtree(&source, root, &target, &replacement, &mut output)?;
    format_query(&output, arena)
}

pub fn headline<'a>(
    document: &str,
    query_text: &str,
    start: &str,
    stop: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    headline_with(
        document,
        query_text,
        HeadlineOptions {
            start,
            stop,
            ..HeadlineOptions::DEFAULT
        },
        arena,
        |kind, token, arena| {
            if !builtin_token_is_mapped(kind) {
                return Ok(TextSearchLexeme::Unmapped);
            }
            Ok(
                match normalize_token(token, TextSearchConfig::English, arena)? {
                    Some(lexeme) => TextSearchLexeme::Lexeme(lexeme),
                    None => TextSearchLexeme::StopWord,
                },
            )
        },
    )
}

#[derive(Clone, Copy)]
pub(crate) struct HeadlineOptions<'a> {
    pub start: &'a str,
    pub stop: &'a str,
    pub fragment_delimiter: &'a str,
    pub min_words: usize,
    pub max_words: usize,
    pub short_word: usize,
    pub max_fragments: usize,
    pub highlight_all: bool,
}

impl HeadlineOptions<'static> {
    pub(crate) const DEFAULT: Self = Self {
        start: "<b>",
        stop: "</b>",
        fragment_delimiter: " ... ",
        min_words: 15,
        max_words: 35,
        short_word: 3,
        max_fragments: 0,
        highlight_all: false,
    };
}

#[derive(Clone, Copy)]
struct HeadlineWord {
    start: usize,
    end: usize,
    hit: bool,
}

pub(crate) fn headline_with<'a>(
    document: &str,
    query_text: &str,
    options: HeadlineOptions<'_>,
    arena: &'a Arena,
    mut normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    let query = parse_query(query_text, arena)?;
    let mut terms = ArenaList::new(arena);
    for node in query.nodes() {
        if let QueryNode::Lexeme { text, prefix, .. } = *node {
            terms
                .push((text, prefix))
                .map_err(|_| arena_full("ts_headline"))?;
        }
    }
    let mut words = ArenaList::new(arena);
    let mut begin = None;
    for (offset, character) in document
        .char_indices()
        .chain(core::iter::once((document.len(), ' ')))
    {
        if token_char(character) {
            begin.get_or_insert(offset);
            continue;
        }
        let Some(start) = begin.take() else { continue };
        let word = &document[start..offset];
        let hit = match normalize(token_type(word), word, arena)? {
            TextSearchLexeme::Lexeme(lexeme) => terms
                .as_slice()
                .iter()
                .any(|(term, prefix)| lexeme == *term || *prefix && lexeme.starts_with(term)),
            TextSearchLexeme::Unmapped | TextSearchLexeme::StopWord => false,
        };
        words
            .push(HeadlineWord {
                start,
                end: offset,
                hit,
            })
            .map_err(|_| arena_full("ts_headline"))?;
    }
    if words.is_empty() {
        return arena
            .alloc_str(document)
            .map_err(|_| arena_full("ts_headline"));
    }

    let mut fragments = ArenaList::new(arena);
    if options.highlight_all {
        fragments
            .push((0, words.len() - 1))
            .map_err(|_| arena_full("ts_headline"))?;
    } else if options.max_fragments == 0 {
        let first_hit = words.as_slice().iter().position(|word| word.hit);
        let start = first_hit.unwrap_or(0);
        let mut end = (start + options.min_words.saturating_sub(1)).min(words.len() - 1);
        let mut begin = start;
        if end + 1 - begin < options.min_words {
            begin = end.saturating_add(1).saturating_sub(options.min_words);
        }
        if end + 1 - begin > options.max_words {
            end = begin + options.max_words - 1;
        }
        fragments
            .push((begin, end))
            .map_err(|_| arena_full("ts_headline"))?;
    } else {
        for (hit, word) in words.as_slice().iter().enumerate() {
            if !word.hit || fragments.len() == options.max_fragments {
                continue;
            }
            let before = options.max_words.saturating_sub(1) / 2;
            let mut begin = hit.saturating_sub(before);
            let mut end = (begin + options.max_words.saturating_sub(1)).min(words.len() - 1);
            begin = end
                .saturating_add(1)
                .saturating_sub(options.max_words)
                .min(begin);
            if let Some(previous) = fragments.as_mut_slice().last_mut()
                && begin <= previous.1
            {
                previous.1 = previous.1.max(end);
                continue;
            }
            if end + 1 - begin < options.min_words {
                end = (begin + options.min_words - 1).min(words.len() - 1);
            }
            fragments
                .push((begin, end))
                .map_err(|_| arena_full("ts_headline"))?;
        }
        if fragments.is_empty() {
            fragments
                .push((0, options.min_words.saturating_sub(1).min(words.len() - 1)))
                .map_err(|_| arena_full("ts_headline"))?;
        }
    }

    let mut out = ArenaText::new(
        arena,
        document
            .len()
            .saturating_add(
                document
                    .len()
                    .saturating_mul(options.start.len().saturating_add(options.stop.len())),
            )
            .saturating_add(
                fragments
                    .len()
                    .saturating_mul(options.fragment_delimiter.len()),
            )
            .saturating_add(1),
        "ts_headline",
    )?;
    for (begin, end) in fragments.as_mut_slice() {
        while *end > *begin
            && *end + 1 - *begin > options.min_words
            && !words.as_slice()[*end].hit
            && words.as_slice()[*end].end - words.as_slice()[*end].start <= options.short_word
        {
            *end -= 1;
        }
    }
    for (fragment, (begin, end)) in fragments.as_slice().iter().copied().enumerate() {
        if fragment > 0 {
            out.push_str(options.fragment_delimiter)?;
        }
        let byte_start = words.as_slice()[begin].start;
        let mut byte_end = words.as_slice()[end].end;
        for character in document[byte_end..].chars() {
            if token_char(character) || character.is_whitespace() {
                break;
            }
            byte_end += character.len_utf8();
        }
        let mut at = byte_start;
        for word in &words.as_slice()[begin..=end] {
            out.push_str(&document[at..word.start])?;
            if word.hit {
                out.push_str(options.start)?;
            }
            out.push_str(&document[word.start..word.end])?;
            if word.hit {
                out.push_str(options.stop)?;
            }
            at = word.end;
        }
        out.push_str(&document[at..byte_end])?;
    }
    Ok(out.finish())
}

pub fn vector_length(source: &str, arena: &Arena) -> Result<i32, SqlError> {
    let vector = parse_vector(source, arena)?;
    let mut count = 0i32;
    let mut prior = None;
    for index in 0..vector.lexeme_count() {
        let (text, _) = vector.lexeme(index).expect("vector index");
        if prior != Some(text) {
            count += 1;
            prior = Some(text);
        }
    }
    Ok(count)
}

pub fn array_to_vector<'a>(lexemes: &[&str], arena: &'a Arena) -> Result<&'a str, SqlError> {
    let capacity = lexemes
        .iter()
        .try_fold(1usize, |total, lexeme| {
            total.checked_add(lexeme.len().saturating_mul(2).saturating_add(3))
        })
        .ok_or_else(|| capacity("tsvector"))?;
    let mut raw = ArenaText::new(arena, capacity, "tsvector")?;
    for (index, lexeme) in lexemes.iter().enumerate() {
        if lexeme.is_empty() {
            return Err(sql_err!(
                sqlstate::ZERO_LENGTH_CHARACTER_STRING,
                "lexeme array may not contain empty strings"
            ));
        }
        if index > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(lexeme)?;
    }
    canonical_vector(raw.finish(), arena)
}

pub fn phrase_queries_distance<'a>(
    left: &'a str,
    right: &'a str,
    distance: u16,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if left.is_empty() {
        return canonical_query(right, arena);
    }
    if right.is_empty() {
        return canonical_query(left, arena);
    }
    let mut raw = ArenaText::new(
        arena,
        left.len().saturating_add(right.len()).saturating_add(16),
        "tsquery",
    )?;
    raw.push_str(left)?;
    raw.push_str(" <")?;
    write_u16(&mut raw, distance)?;
    raw.push_str("> ")?;
    raw.push_str(right)?;
    canonical_query(raw.finish(), arena)
}

pub fn query_node_count(source: &str, arena: &Arena) -> Result<i32, SqlError> {
    i32::try_from(parse_query(source, arena)?.nodes.len()).map_err(|_| capacity("tsquery"))
}

pub fn query_operand_count(source: &str, arena: &Arena) -> Result<i32, SqlError> {
    let query = parse_query(source, arena)?;
    i32::try_from(
        query
            .nodes()
            .iter()
            .filter(|node| matches!(node, QueryNode::Lexeme { .. }))
            .count(),
    )
    .map_err(|_| capacity("tsquery"))
}

pub fn concat_vectors<'a>(left: &str, right: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let left_vector = parse_vector(left, arena)?;
    let right_vector = parse_vector(right, arena)?;
    let mut max_position = 0u16;
    for index in 0..left_vector.lexeme_count() {
        let (_, positions) = left_vector.lexeme(index).expect("vector index");
        for position in positions {
            max_position = max_position.max(position.number);
        }
    }
    let mut raw = ArenaText::new(
        arena,
        left.len()
            .saturating_add(right.len())
            .saturating_mul(3)
            .saturating_add(64),
        "tsvector",
    )?;
    raw.push_str(left)?;
    for index in 0..right_vector.lexeme_count() {
        let (text, positions) = right_vector.lexeme(index).expect("vector index");
        if raw.len > 0 {
            raw.push_byte(b' ')?;
        }
        raw.push_quoted(text)?;
        if !positions.is_empty() {
            raw.push_byte(b':')?;
            for (offset, position) in positions.iter().enumerate() {
                if offset > 0 {
                    raw.push_byte(b',')?;
                }
                write_u16(
                    &mut raw,
                    position.number.saturating_add(max_position).min(16_383),
                )?;
                match position.weight {
                    3 => raw.push_byte(b'A')?,
                    2 => raw.push_byte(b'B')?,
                    1 => raw.push_byte(b'C')?,
                    _ => {}
                }
            }
        }
    }
    canonical_vector(raw.finish(), arena)
}

pub fn concat_queries<'a>(
    left: &'a str,
    right: &'a str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    combine_queries(left, right, " | ", arena)
}

pub fn and_queries<'a>(
    left: &'a str,
    right: &'a str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    combine_queries(left, right, " & ", arena)
}

pub fn phrase_queries<'a>(
    left: &'a str,
    right: &'a str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    combine_queries(left, right, " <-> ", arena)
}

fn combine_queries<'a>(
    left: &'a str,
    right: &'a str,
    operator: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    if left.is_empty() {
        return canonical_query(right, arena);
    }
    if right.is_empty() {
        return canonical_query(left, arena);
    }
    let mut raw = ArenaText::new(
        arena,
        left.len().saturating_add(right.len()).saturating_add(8),
        "tsquery",
    )?;
    raw.push_str(left)?;
    raw.push_str(operator)?;
    raw.push_str(right)?;
    canonical_query(raw.finish(), arena)
}

pub fn not_query<'a>(source: &'a str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    if source.is_empty() {
        return Ok("");
    }
    let mut raw = ArenaText::new(arena, source.len().saturating_add(8), "tsquery")?;
    raw.push_str("!( ")?;
    raw.push_str(source)?;
    raw.push_str(" )")?;
    canonical_query(raw.finish(), arena)
}

/// PostgreSQL's tsquery containment operators compare the sets of operands;
/// boolean operators do not affect containment.
pub fn query_contains(container: &str, contained: &str, arena: &Arena) -> Result<bool, SqlError> {
    let container = parse_query(container, arena)?;
    let contained = parse_query(contained, arena)?;
    for candidate in contained.nodes() {
        let QueryNode::Lexeme { text, .. } = *candidate else {
            continue;
        };
        let found = container
            .nodes()
            .iter()
            .any(|node| matches!(node, QueryNode::Lexeme { text: other, .. } if *other == text));
        if !found {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn rank(
    vector_text: &str,
    query_text: &str,
    cover_density: bool,
    arena: &Arena,
) -> Result<f32, SqlError> {
    rank_with_options(
        vector_text,
        query_text,
        cover_density,
        [0.1, 0.2, 0.4, 1.0],
        0,
        arena,
    )
}

pub fn rank_with_options(
    vector_text: &str,
    query_text: &str,
    cover_density: bool,
    weights: [f32; 4],
    normalization: i32,
    arena: &Arena,
) -> Result<f32, SqlError> {
    let vector = parse_vector(vector_text, arena)?;
    let query = parse_query(query_text, arena)?;
    if vector.lexeme_count() == 0 || query.root().is_none() {
        return Ok(0.0);
    }
    if cover_density {
        return rank_cover_density(&vector, &query, weights, normalization, arena);
    }
    let mut operands: ArenaList<'_, RankOperand<'_>> = ArenaList::new(arena);
    for node in query.nodes() {
        let QueryNode::Lexeme {
            text,
            weights,
            prefix,
        } = *node
        else {
            continue;
        };
        if operands
            .as_slice()
            .iter()
            .any(|operand| operand.text == text)
        {
            continue;
        }
        operands
            .push(RankOperand {
                text,
                weights,
                prefix,
            })
            .map_err(|_| arena_full("ts_rank"))?;
    }
    if operands.is_empty() {
        return Ok(0.0);
    }
    operands
        .as_mut_slice()
        .sort_unstable_by(|left, right| left.text.cmp(right.text));
    let root = query.nodes()[query.root().expect("checked root") as usize];
    let conjunction = matches!(root, QueryNode::And(_, _) | QueryNode::Phrase { .. });
    let mut result = if conjunction && operands.len() >= 2 {
        rank_and(&vector, operands.as_slice(), weights, arena)?
    } else {
        rank_or(&vector, operands.as_slice(), weights)
    };
    if result < 0.0 {
        result = 1e-20;
    }
    Ok(normalize_rank(result, &vector, normalization))
}

#[derive(Clone, Copy)]
struct RankOperand<'a> {
    text: &'a str,
    weights: u8,
    prefix: bool,
}

fn matching_lexeme(operand: RankOperand<'_>, lexeme: &str) -> bool {
    lexeme == operand.text || (operand.prefix && lexeme.starts_with(operand.text))
}

fn rank_or(vector: &Vector<'_>, operands: &[RankOperand<'_>], weights: [f32; 4]) -> f32 {
    let mut score = 0.0f32;
    for operand in operands.iter().copied() {
        for index in 0..vector.lexeme_count() {
            let (candidate, positions) = vector.lexeme(index).expect("vector index");
            if !matching_lexeme(operand, candidate) {
                continue;
            }
            let mut subtotal = 0.0f32;
            let mut maximum = -1.0f32;
            let mut maximum_index = 0usize;
            if positions.is_empty() {
                subtotal = weights[0];
                maximum = weights[0];
            } else {
                for (position_index, position) in positions.iter().enumerate() {
                    if operand.weights != 0 && operand.weights & (1 << position.weight) == 0 {
                        continue;
                    }
                    let weight = weights[usize::from(position.weight)];
                    subtotal += weight / ((position_index + 1) * (position_index + 1)) as f32;
                    if weight > maximum {
                        maximum = weight;
                        maximum_index = position_index;
                    }
                }
            }
            if maximum >= 0.0 {
                score += (maximum + subtotal
                    - maximum / ((maximum_index + 1) * (maximum_index + 1)) as f32)
                    / 1.644_934;
            }
        }
    }
    score / operands.len() as f32
}

fn rank_positions(vector: &Vector<'_>, operand: RankOperand<'_>, output: &mut [Position]) -> usize {
    let mut count = 0usize;
    for index in 0..vector.lexeme_count() {
        let (lexeme, positions) = vector.lexeme(index).expect("vector index");
        if !matching_lexeme(operand, lexeme) {
            continue;
        }
        if positions.is_empty() {
            if count < output.len() {
                output[count] = Position {
                    number: 16_382,
                    weight: 0,
                };
                count += 1;
            }
        } else {
            for position in positions {
                if (operand.weights == 0 || operand.weights & (1 << position.weight) != 0)
                    && count < output.len()
                {
                    output[count] = *position;
                    count += 1;
                }
            }
        }
    }
    count
}

fn word_distance(distance: u16) -> f32 {
    if distance > 100 {
        1e-30
    } else {
        1.0 / (1.005 + 0.05 * ((f32::from(distance) / 1.5) - 2.0).exp())
    }
}

fn rank_and(
    vector: &Vector<'_>,
    operands: &[RankOperand<'_>],
    weights: [f32; 4],
    arena: &Arena,
) -> Result<f32, SqlError> {
    let mut result = -1.0f32;
    let position_capacity = rank_vector_length(vector);
    let current_positions = arena
        .alloc_slice_with(position_capacity, |_| Position::default())
        .map_err(|_| arena_full("ts_rank positions"))?;
    let earlier_positions = arena
        .alloc_slice_with(position_capacity, |_| Position::default())
        .map_err(|_| arena_full("ts_rank positions"))?;
    for (index, operand) in operands.iter().copied().enumerate() {
        let current_count = rank_positions(vector, operand, current_positions);
        for earlier in operands[..index].iter().copied() {
            let earlier_count = rank_positions(vector, earlier, earlier_positions);
            for left in &current_positions[..current_count] {
                for right in &earlier_positions[..earlier_count] {
                    let mut distance = left.number.abs_diff(right.number);
                    if distance == 0 {
                        distance = 16_383;
                    }
                    let current = (weights[usize::from(left.weight)]
                        * weights[usize::from(right.weight)]
                        * word_distance(distance))
                    .sqrt();
                    result = if result < 0.0 {
                        current
                    } else {
                        1.0 - (1.0 - result) * (1.0 - current)
                    };
                }
            }
        }
    }
    Ok(result)
}

fn rank_vector_length(vector: &Vector<'_>) -> usize {
    (0..vector.lexeme_count())
        .map(|index| {
            let (_, positions) = vector.lexeme(index).expect("vector index");
            positions.len().max(1)
        })
        .sum()
}

fn normalize_rank(mut result: f32, vector: &Vector<'_>, method: i32) -> f32 {
    if method & 1 != 0 {
        result /= ((rank_vector_length(vector) + 1) as f32).log2();
    }
    if method & 2 != 0 {
        let length = rank_vector_length(vector);
        if length > 0 {
            result /= length as f32;
        }
    }
    if method & 8 != 0 {
        result /= vector.lexeme_count() as f32;
    }
    if method & 16 != 0 {
        result /= (vector.lexeme_count() as f32 + 1.0).log2();
    }
    if method & 32 != 0 {
        result /= result + 1.0;
    }
    result
}

fn rank_cover_density(
    vector: &Vector<'_>,
    query: &Query<'_>,
    weights: [f32; 4],
    normalization: i32,
    arena: &Arena,
) -> Result<f32, SqlError> {
    #[derive(Clone, Copy)]
    struct DocumentEntry<'a> {
        position: Position,
        lexeme: &'a str,
    }

    fn cover_match(
        query: &Query<'_>,
        node: u32,
        document: &[DocumentEntry<'_>],
    ) -> Result<MatchResult, SqlError> {
        Ok(
            match query.node(node).ok_or_else(|| syntax("tsquery", ""))? {
                QueryNode::Lexeme {
                    text,
                    weights,
                    prefix,
                } => {
                    let mut result = MatchResult::empty(false);
                    for entry in document {
                        if (entry.lexeme == text || prefix && entry.lexeme.starts_with(text))
                            && (weights == 0 || weights & (1 << entry.position.weight) != 0)
                        {
                            result.truth = true;
                            result.add(entry.position.number);
                        }
                    }
                    result
                }
                QueryNode::Not(child) => {
                    MatchResult::empty(!cover_match(query, child, document)?.truth)
                }
                QueryNode::And(left, right) => {
                    let left = cover_match(query, left, document)?;
                    let right = cover_match(query, right, document)?;
                    let mut result = MatchResult::empty(left.truth && right.truth);
                    if result.truth {
                        for position in left.positions[..left.count]
                            .iter()
                            .chain(&right.positions[..right.count])
                        {
                            result.add(*position);
                        }
                    }
                    result
                }
                QueryNode::Or(left, right) => {
                    let left = cover_match(query, left, document)?;
                    let right = cover_match(query, right, document)?;
                    let mut result = MatchResult::empty(left.truth || right.truth);
                    for position in left.positions[..left.count]
                        .iter()
                        .chain(&right.positions[..right.count])
                    {
                        result.add(*position);
                    }
                    result
                }
                QueryNode::Phrase {
                    left,
                    right,
                    distance,
                } => {
                    let left = cover_match(query, left, document)?;
                    let right = cover_match(query, right, document)?;
                    let mut result = MatchResult::empty(false);
                    if left.truth && right.truth {
                        for left_position in &left.positions[..left.count] {
                            for right_position in &right.positions[..right.count] {
                                if *right_position == left_position.saturating_add(distance) {
                                    result.truth = true;
                                    result.add(*right_position);
                                }
                            }
                        }
                    }
                    result
                }
            },
        )
    }

    let Some(root) = query.root() else {
        return Ok(0.0);
    };
    let mut document = ArenaList::new(arena);
    for lexeme_index in 0..vector.lexeme_count() {
        let (lexeme, positions) = vector.lexeme(lexeme_index).expect("vector index");
        if positions.is_empty() {
            continue;
        }
        if !query.nodes().iter().any(|node| {
            matches!(node, QueryNode::Lexeme { text, prefix, .. }
                if lexeme == *text || *prefix && lexeme.starts_with(text))
        }) {
            continue;
        }
        for position in positions {
            if !query.nodes().iter().any(|node| {
                matches!(node, QueryNode::Lexeme { text, weights: required, prefix }
                    if (lexeme == *text || *prefix && lexeme.starts_with(text))
                        && (*required == 0 || *required & (1 << position.weight) != 0))
            }) {
                continue;
            }
            document
                .push(DocumentEntry {
                    position: *position,
                    lexeme,
                })
                .map_err(|_| arena_full("ts_rank_cd document"))?;
        }
    }
    document
        .as_mut_slice()
        .sort_unstable_by_key(|entry| (entry.position.number, entry.position.weight, entry.lexeme));
    if document.is_empty() {
        return Ok(0.0);
    }
    let document = document.as_slice();

    let inverse_weights = weights.map(|weight| 1.0f64 / f64::from(weight));
    let mut result = 0.0f64;
    let mut extent_count = 0usize;
    let mut distance_sum = 0.0f64;
    let mut previous_extent_position = 0.0f64;
    let mut cursor = 0usize;
    while cursor < document.len() {
        let Some(end) = (cursor..document.len()).find(|end| {
            cover_match(query, root, &document[cursor..=*end]).is_ok_and(|matched| matched.truth)
        }) else {
            break;
        };
        let begin = (cursor..=end)
            .rev()
            .find(|begin| {
                cover_match(query, root, &document[*begin..=end]).is_ok_and(|matched| matched.truth)
            })
            .expect("the complete candidate cover matched");
        let inverse_sum = document[begin..=end]
            .iter()
            .map(|entry| inverse_weights[usize::from(entry.position.weight)])
            .sum::<f64>();
        let cover_entries = end - begin + 1;
        let mut noise = i32::from(document[end].position.number)
            - i32::from(document[begin].position.number)
            - (end - begin) as i32;
        if noise < 0 {
            noise = ((end - begin) / 2) as i32;
        }
        result += cover_entries as f64 / inverse_sum / f64::from(1 + noise);
        let extent_position = f64::from(
            u32::from(document[end].position.number) + u32::from(document[begin].position.number),
        ) / 2.0;
        if extent_count > 0 && extent_position > previous_extent_position {
            distance_sum += 1.0 / (extent_position - previous_extent_position);
        }
        previous_extent_position = extent_position;
        extent_count += 1;
        cursor = begin + 1;
    }

    if normalization & 1 != 0 && vector.lexeme_count() > 0 {
        result /= (rank_vector_length(vector) as f64 + 1.0).ln();
    }
    if normalization & 2 != 0 {
        let length = rank_vector_length(vector);
        if length > 0 {
            result /= length as f64;
        }
    }
    if normalization & 4 != 0 && extent_count > 0 && distance_sum > 0.0 {
        result /= extent_count as f64 / distance_sum;
    }
    if normalization & 8 != 0 && vector.lexeme_count() > 0 {
        result /= vector.lexeme_count() as f64;
    }
    if normalization & 16 != 0 && vector.lexeme_count() > 0 {
        result /= (vector.lexeme_count() as f64 + 1.0).log2();
    }
    if normalization & 32 != 0 {
        result /= result + 1.0;
    }
    Ok(result as f32)
}

pub fn datum_text<'a>(
    datum: Datum<'a>,
    expected: &'static str,
) -> Result<Option<&'a str>, SqlError> {
    match (expected, datum) {
        (_, Datum::Null) => Ok(None),
        ("tsvector", Datum::TsVector(text)) => Ok(Some(text.as_str())),
        ("tsquery", Datum::TsQuery(text)) => Ok(Some(text.as_str())),
        (_, other) => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "{} requires {}, not type OID {}",
            expected,
            expected,
            other.type_oid()
        )),
    }
}

#[derive(Clone, Copy)]
struct CanonicalVectorEntry<'a> {
    lexeme: &'a [u8],
    positions: &'a [u8],
}

struct CanonicalVectorEntries<'a> {
    source: &'a [u8],
    at: usize,
}

impl<'a> CanonicalVectorEntries<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source: source.as_bytes(),
            at: 0,
        }
    }
}

impl<'a> Iterator for CanonicalVectorEntries<'a> {
    type Item = CanonicalVectorEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.source.get(self.at) == Some(&b' ') {
            self.at += 1;
        }
        if self.at == self.source.len() {
            return None;
        }
        debug_assert_eq!(self.source[self.at], b'\'');
        self.at += 1;
        let start = self.at;
        while self.at < self.source.len() {
            if self.source[self.at] == b'\\' {
                self.at += 2;
            } else if self.source[self.at] == b'\'' {
                break;
            } else {
                self.at += 1;
            }
        }
        let lexeme = &self.source[start..self.at];
        self.at += 1;
        let positions = if self.source.get(self.at) == Some(&b':') {
            self.at += 1;
            let start = self.at;
            while self.at < self.source.len() && self.source[self.at] != b' ' {
                self.at += 1;
            }
            &self.source[start..self.at]
        } else {
            &self.source[self.at..self.at]
        };
        Some(CanonicalVectorEntry { lexeme, positions })
    }
}

fn escaped_len(raw: &[u8]) -> usize {
    let mut at = 0usize;
    let mut len = 0usize;
    while at < raw.len() {
        at += if raw[at] == b'\\' { 2 } else { 1 };
        len += 1;
    }
    len
}

struct UnescapedBytes<'a> {
    raw: &'a [u8],
    at: usize,
}

/// Visits stable hashes of the lexemes in canonical `tsvector` text without
/// reparsing positions or allocating another vector representation.
pub(crate) fn for_each_vector_lexeme_hash(source: &str, mut visit: impl FnMut(u64)) {
    use core::hash::Hasher as _;
    for entry in CanonicalVectorEntries::new(source) {
        let mut hasher = crate::mem::fixed_map::Fnv1aHasher::default();
        for byte in (UnescapedBytes {
            raw: entry.lexeme,
            at: 0,
        }) {
            hasher.write_u8(byte);
        }
        visit(hasher.finish());
    }
}

impl Iterator for UnescapedBytes<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        let byte = *self.raw.get(self.at)?;
        self.at += 1;
        if byte == b'\\' {
            let escaped = self.raw[self.at];
            self.at += 1;
            Some(escaped)
        } else {
            Some(byte)
        }
    }
}

fn compare_escaped(left: &[u8], right: &[u8]) -> Ordering {
    UnescapedBytes { raw: left, at: 0 }.cmp(UnescapedBytes { raw: right, at: 0 })
}

fn position_count(raw: &[u8]) -> usize {
    if raw.is_empty() {
        0
    } else {
        1 + raw.iter().filter(|byte| **byte == b',').count()
    }
}

struct CanonicalPositions<'a> {
    raw: &'a [u8],
    at: usize,
}

impl Iterator for CanonicalPositions<'_> {
    type Item = Position;

    fn next(&mut self) -> Option<Position> {
        if self.at == self.raw.len() {
            return None;
        }
        let mut number = 0u16;
        while let Some(byte @ b'0'..=b'9') = self.raw.get(self.at).copied() {
            number = number * 10 + u16::from(byte - b'0');
            self.at += 1;
        }
        let weight = match self.raw.get(self.at).copied() {
            Some(b'A') => 3,
            Some(b'B') => 2,
            Some(b'C') => 1,
            _ => 0,
        };
        if weight != 0 {
            self.at += 1;
        }
        if self.raw.get(self.at) == Some(&b',') {
            self.at += 1;
        }
        Some(Position { number, weight })
    }
}

fn vector_storage_shape(source: &str) -> (usize, usize) {
    let mut count = 0usize;
    let mut data_bytes = 0usize;
    for entry in CanonicalVectorEntries::new(source) {
        count += 1;
        data_bytes += escaped_len(entry.lexeme);
        let positions = position_count(entry.positions);
        if positions != 0 {
            data_bytes = (data_bytes + 1) & !1;
            data_bytes += 2 + positions * 2;
        }
    }
    (count, 8 + count * 4 + data_bytes)
}

pub fn compare_vector(left: &str, right: &str) -> Ordering {
    let (left_count, left_size) = vector_storage_shape(left);
    let (right_count, right_size) = vector_storage_shape(right);
    left_size
        .cmp(&right_size)
        .then_with(|| left_count.cmp(&right_count))
        .then_with(|| {
            let mut left_entries = CanonicalVectorEntries::new(left);
            let mut right_entries = CanonicalVectorEntries::new(right);
            loop {
                match (left_entries.next(), right_entries.next()) {
                    (Some(a), Some(b)) => {
                        let ordering = (!a.positions.is_empty())
                            .cmp(&(!b.positions.is_empty()))
                            .reverse()
                            .then_with(|| compare_escaped(a.lexeme, b.lexeme))
                            .then_with(|| {
                                position_count(a.positions)
                                    .cmp(&position_count(b.positions))
                                    .reverse()
                            })
                            .then_with(|| {
                                let mut a_positions = CanonicalPositions {
                                    raw: a.positions,
                                    at: 0,
                                };
                                let mut b_positions = CanonicalPositions {
                                    raw: b.positions,
                                    at: 0,
                                };
                                loop {
                                    match (a_positions.next(), b_positions.next()) {
                                        (Some(a), Some(b)) => {
                                            let ordering =
                                                a.number.cmp(&b.number).reverse().then_with(|| {
                                                    a.weight.cmp(&b.weight).reverse()
                                                });
                                            if !ordering.is_eq() {
                                                break ordering;
                                            }
                                        }
                                        _ => break Ordering::Equal,
                                    }
                                }
                            });
                        if !ordering.is_eq() {
                            break ordering;
                        }
                    }
                    _ => break Ordering::Equal,
                }
            }
        })
}

fn legacy_crc32(raw: &[u8]) -> i32 {
    let mut crc = u32::MAX;
    for byte in (UnescapedBytes { raw, at: 0 }) {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            let high = crc & 0x8000_0000;
            crc <<= 1;
            if high != 0 {
                crc ^= 0x04c1_1db7;
            }
        }
    }
    (crc ^ u32::MAX) as i32
}

fn query_storage_size(source: &[u8]) -> usize {
    let mut operand_bytes = 0usize;
    let mut at = 0usize;
    while at < source.len() {
        if source[at] != b'\'' {
            at += 1;
            continue;
        }
        let raw = scan_quoted(source, &mut at);
        operand_bytes += escaped_len(raw) + 1;
    }
    8 + canonical_query_node_count(source) * 12 + operand_bytes
}

fn canonical_operator(expression: CanonicalExpression<'_>) -> u8 {
    match expression {
        CanonicalExpression::Lexeme { .. } => 0,
        CanonicalExpression::Not(_) => 1,
        CanonicalExpression::Binary { operator, .. } => operator,
    }
}

fn compare_query_nodes(left: &[u8], right: &[u8]) -> Ordering {
    let left_node = canonical_expression(left);
    let right_node = canonical_expression(right);
    let is_left_operator = !matches!(left_node, CanonicalExpression::Lexeme { .. });
    let is_right_operator = !matches!(right_node, CanonicalExpression::Lexeme { .. });
    is_left_operator
        .cmp(&is_right_operator)
        .reverse()
        .then_with(|| match (left_node, right_node) {
            (
                CanonicalExpression::Lexeme { raw: a, .. },
                CanonicalExpression::Lexeme { raw: b, .. },
            ) => legacy_crc32(a)
                .cmp(&legacy_crc32(b))
                .reverse()
                .then_with(|| compare_escaped(a, b)),
            (CanonicalExpression::Not(a), CanonicalExpression::Not(b)) => compare_query_nodes(a, b),
            (
                CanonicalExpression::Binary {
                    operator: ao,
                    distance: ad,
                    left: al,
                    right: ar,
                },
                CanonicalExpression::Binary {
                    operator: bo,
                    distance: bd,
                    left: bl,
                    right: br,
                },
            ) if ao == bo => compare_query_nodes(ar, br)
                .then_with(|| compare_query_nodes(al, bl))
                .then_with(|| {
                    if ao == 4 {
                        ad.cmp(&bd).reverse()
                    } else {
                        Ordering::Equal
                    }
                }),
            (a, b) => canonical_operator(a).cmp(&canonical_operator(b)).reverse(),
        })
}

pub fn compare_query(left: &str, right: &str) -> Ordering {
    let left = left.as_bytes();
    let right = right.as_bytes();
    canonical_query_node_count(left)
        .cmp(&canonical_query_node_count(right))
        .then_with(|| query_storage_size(left).cmp(&query_storage_size(right)))
        .then_with(|| {
            if left.is_empty() || right.is_empty() {
                Ordering::Equal
            } else {
                compare_query_nodes(left, right)
            }
        })
}

fn emit_query_hash_node(source: &[u8], emit: &mut impl FnMut(&[u8])) {
    match canonical_expression(source) {
        CanonicalExpression::Lexeme { raw, .. } => {
            emit(&[1]);
            for byte in (UnescapedBytes { raw, at: 0 }) {
                emit(&[byte]);
            }
            emit(&[0]);
        }
        CanonicalExpression::Not(child) => {
            emit(&[2, 1]);
            emit_query_hash_node(child, emit);
        }
        CanonicalExpression::Binary {
            operator,
            distance,
            left,
            right,
        } => {
            emit(&[2, operator]);
            emit_query_hash_node(right, emit);
            emit_query_hash_node(left, emit);
            if operator == 4 {
                emit(&distance.to_le_bytes());
            }
        }
    }
}

/// Folds the already validated canonical query tree without allocating. The
/// lexeme callback receives the same stable unescaped hash used by vectors.
pub(crate) fn fold_query_lexemes<T: Copy>(
    source: &str,
    mut lexeme: impl FnMut(u64, bool) -> T,
    negated: T,
    mut conjunction: impl FnMut(T, T) -> T,
    mut disjunction: impl FnMut(T, T) -> T,
) -> Option<T> {
    use core::hash::Hasher as _;

    fn fold<T: Copy>(
        source: &[u8],
        lexeme: &mut impl FnMut(u64, bool) -> T,
        negated: T,
        conjunction: &mut impl FnMut(T, T) -> T,
        disjunction: &mut impl FnMut(T, T) -> T,
    ) -> T {
        match canonical_expression(source) {
            CanonicalExpression::Lexeme { raw, prefix, .. } => {
                let mut hasher = crate::mem::fixed_map::Fnv1aHasher::default();
                for byte in (UnescapedBytes { raw, at: 0 }) {
                    hasher.write_u8(byte);
                }
                lexeme(hasher.finish(), prefix)
            }
            CanonicalExpression::Not(_) => negated,
            CanonicalExpression::Binary {
                operator: 2 | 4,
                left,
                right,
                ..
            } => {
                let left = fold(left, lexeme, negated, conjunction, disjunction);
                let right = fold(right, lexeme, negated, conjunction, disjunction);
                conjunction(left, right)
            }
            CanonicalExpression::Binary {
                operator: 3,
                left,
                right,
                ..
            } => {
                let left = fold(left, lexeme, negated, conjunction, disjunction);
                let right = fold(right, lexeme, negated, conjunction, disjunction);
                disjunction(left, right)
            }
            CanonicalExpression::Binary { .. } => unreachable!("canonical tsquery operator"),
        }
    }

    (!source.is_empty()).then(|| {
        fold(
            source.as_bytes(),
            &mut lexeme,
            negated,
            &mut conjunction,
            &mut disjunction,
        )
    })
}

pub fn emit_query_hash(source: &str, mut emit: impl FnMut(&[u8])) {
    let source = source.as_bytes();
    emit(&(canonical_query_node_count(source) as u32).to_le_bytes());
    emit(&(query_storage_size(source) as u32).to_le_bytes());
    if !source.is_empty() {
        emit_query_hash_node(source, &mut emit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::Budget;

    fn arena() -> Arena {
        let mut budget = Budget::new(8 * 1024 * 1024);
        Arena::new(&mut budget, "full-text test", 4 * 1024 * 1024).unwrap()
    }

    #[test]
    fn postgresql_binary_vector_round_trip() {
        let source = "'a':1A,2B 'fat':3";
        let mut encoded = Vec::new();
        let len = emit_vector_binary(source, |bytes| encoded.extend_from_slice(bytes));
        assert_eq!(len, encoded.len());
        assert_eq!(
            encoded,
            [
                0, 0, 0, 2, b'a', 0, 0, 2, 0xc0, 1, 0x80, 2, b'f', b'a', b't', 0, 0, 1, 0, 3,
            ]
        );
        let arena = arena();
        assert_eq!(decode_vector_binary(&encoded, &arena).unwrap(), source);
    }

    #[test]
    fn postgresql_binary_query_round_trip() {
        let source = "'fat' & ( 'rat' | !'cat':*AB )";
        let mut encoded = Vec::new();
        let len = emit_query_binary(source, |bytes| encoded.extend_from_slice(bytes));
        assert_eq!(len, encoded.len());
        assert_eq!(
            encoded,
            [
                0, 0, 0, 6, 2, 2, 2, 3, 2, 1, 1, 12, 1, b'c', b'a', b't', 0, 1, 0, 0, b'r', b'a',
                b't', 0, 1, 0, 0, b'f', b'a', b't', 0,
            ]
        );
        let arena = arena();
        assert_eq!(decode_query_binary(&encoded, &arena).unwrap(), source);
    }

    #[test]
    fn binary_lexemes_use_the_full_postgresql_field_width() {
        let lexeme = vec![b'x'; MAX_WIRE_LEXEME_BYTES];
        let mut vector = 1i32.to_be_bytes().to_vec();
        vector.extend_from_slice(&lexeme);
        vector.push(0);
        vector.extend_from_slice(&0i16.to_be_bytes());

        let mut query = 1i32.to_be_bytes().to_vec();
        query.extend_from_slice(&[1, 0, 0]);
        query.extend_from_slice(&lexeme);
        query.push(0);

        let arena = arena();
        let vector = decode_vector_binary(&vector, &arena).unwrap();
        let query = decode_query_binary(&query, &arena).unwrap();
        assert_eq!(parse_vector(vector, &arena).unwrap().lexeme_count(), 1);
        assert_eq!(parse_query(query, &arena).unwrap().nodes().len(), 1);

        let text_lexeme = "x".repeat(MAX_TEXT_LEXEME_BYTES + 1);
        assert!(canonical_vector(&text_lexeme, &arena).is_err());
        assert!(canonical_query(&text_lexeme, &arena).is_err());
    }

    #[test]
    fn statement_sized_full_text_values_cross_former_inline_limits() {
        use core::fmt::Write as _;

        let mut vector_source = String::new();
        for index in 0..600 {
            if index != 0 {
                vector_source.push(' ');
            }
            write!(vector_source, "'word{index:04}':1,2,3,4").unwrap();
        }
        let arena = arena();
        let vector = canonical_vector(&vector_source, &arena).unwrap();
        let parsed = parse_vector(vector, &arena).unwrap();
        assert_eq!(parsed.lexeme_count(), 600);
        assert_eq!(rank_vector_length(&parsed), 2_400);
        let mut vector_binary = Vec::new();
        assert_eq!(
            emit_vector_binary(vector, |bytes| vector_binary.extend_from_slice(bytes)),
            vector_binary.len()
        );

        let mut query_terms = (0..300)
            .map(|index| format!("'word{index:04}'"))
            .collect::<Vec<_>>();
        while query_terms.len() > 1 {
            let mut combined = Vec::with_capacity(query_terms.len().div_ceil(2));
            for pair in query_terms.chunks(2) {
                combined.push(if let [left, right] = pair {
                    format!("({left} & {right})")
                } else {
                    pair[0].clone()
                });
            }
            query_terms = combined;
        }
        let query_source = query_terms.pop().unwrap();
        let query = canonical_query(&query_source, &arena).unwrap();
        assert_eq!(query_node_count(query, &arena).unwrap(), 599);
        let mut query_binary = Vec::new();
        assert_eq!(
            emit_query_binary(query, |bytes| query_binary.extend_from_slice(bytes)),
            query_binary.len()
        );
        let decoded = decode_query_binary(&query_binary, &arena).unwrap();
        assert_eq!(decoded, query);
        assert_eq!(compare_query(query, decoded), Ordering::Equal);

        let grouped = canonical_query("(a | b) & (c | d)", &arena).unwrap();
        let mut grouped_binary = Vec::new();
        emit_query_binary(grouped, |bytes| grouped_binary.extend_from_slice(bytes));
        assert_eq!(
            decode_query_binary(&grouped_binary, &arena).unwrap(),
            grouped
        );
    }

    #[test]
    fn vector_canonicalization_keeps_postgresql_position_boundary_per_lexeme() {
        use core::fmt::Write as _;

        let mut source = String::from("'word':");
        for position in 1..=300 {
            if position != 1 {
                source.push(',');
            }
            write!(source, "{position}").unwrap();
        }
        let arena = arena();
        let canonical = canonical_vector(&source, &arena).unwrap();
        let parsed = parse_vector(canonical, &arena).unwrap();
        let (_, positions) = parsed.lexeme(0).unwrap();
        assert_eq!(positions.len(), MAX_LEXEME_POSITIONS);
        assert_eq!(
            positions.last().unwrap().number,
            MAX_LEXEME_POSITIONS as u16
        );
    }

    #[test]
    fn binary_codecs_preserve_escaped_lexemes_and_phrase_distance() {
        let arena = arena();
        let vector = canonical_vector("'can\\'t':12C 'back\\\\slash'", &arena).unwrap();
        let query = canonical_query("'can\\'t' <7> 'back\\\\slash':*D", &arena).unwrap();
        let mut vector_bytes = Vec::new();
        emit_vector_binary(vector, |bytes| vector_bytes.extend_from_slice(bytes));
        let mut query_bytes = Vec::new();
        emit_query_binary(query, |bytes| query_bytes.extend_from_slice(bytes));
        assert_eq!(decode_vector_binary(&vector_bytes, &arena).unwrap(), vector);
        assert_eq!(decode_query_binary(&query_bytes, &arena).unwrap(), query);
    }

    #[test]
    fn binary_decoders_reject_truncated_and_invalid_values() {
        let arena = arena();
        for bytes in [
            &b"\0\0\0\x01unterminated"[..],
            &b"\0\0\0\x01a\0\0\x01\0\0"[..],
        ] {
            assert!(decode_vector_binary(bytes, &arena).is_err());
        }
        for bytes in [
            &b"\0\0\0\x01\x01\x02\x02x\0"[..],
            &b"\0\0\0\x01\x02\x09"[..],
        ] {
            assert!(decode_query_binary(bytes, &arena).is_err());
        }
    }

    #[test]
    fn english_snowball_matches_postgresql_reference_vocabulary() {
        let arena = arena();
        let source = "skies dying lying tying idly gently ugly early only singly relational conditional rational valenci hesitanci digitizer conformabli radicalli differentli vileli analogousli vietnamization predication operator feudalism decisiveness hopefulness callousness formaliti sensitiviti sensibiliti triplicate formative formalize electriciti electrical hopeful goodness revival allowance inference airliner gyroscopic adjustable defensible irritant replacement adjustment dependent adoption homologou communism activate angulariti homologous effective bowdlerize probate rate cease controll roll";
        assert_eq!(
            to_tsvector(TextSearchConfig::English, source, &arena).unwrap(),
            "'activ':53 'adjust':44,48 'adopt':50 'airlin':42 'allow':40 'analog':21 'angular':54 'bowdler':57 'callous':28 'ceas':60 'communism':52 'condit':12 'conform':17 'control':61 'decis':26 'defens':45 'depend':49 'die':2 'differ':19 'digit':16 'earli':8 'effect':56 'electr':35,36 'feudal':25 'formal':29,34 'format':33 'gentl':6 'good':38 'gyroscop':43 'hesit':15 'homolog':55 'homologou':51 'hope':27,37 'idl':5 'infer':41 'irrit':46 'lie':3 'oper':24 'predic':23 'probat':58 'radic':18 'rate':59 'ration':13 'relat':11 'replac':47 'reviv':39 'roll':62 'sensibl':31 'sensit':30 'singl':10 'sky':1 'tie':4 'triplic':32 'ugli':7 'valenc':14 'vietnam':22 'vile':20"
        );
    }

    #[test]
    fn default_parser_categories_match_postgresql_overlapping_tokens() {
        let arena = arena();
        let source = "foo@example.com https://example.com/a-b file.txt 12.34 -4.5 1.2.3 abc-def ghi-42 42-jkl <tag> &amp; host.local /tmp/a.txt";
        assert_eq!(
            to_tsvector(TextSearchConfig::Simple, source, &arena).unwrap(),
            "'-4.5':7 '-42':13 '/a-b':4 '/tmp/a.txt':17 '1.2.3':8 '12.34':6 '42':14 'abc':10 'abc-def':9 'def':11 'example.com':3 'example.com/a-b':2 'file.txt':5 'foo@example.com':1 'ghi':12 'host.local':16 'jkl':15"
        );
    }

    #[test]
    fn rank_matches_postgresql_weight_distance_and_normalization_equations() {
        let arena = arena();
        let close = |actual: f32, expected: f32| {
            assert!(
                (actual - expected).abs() < 0.000_001,
                "{actual} != {expected}"
            );
        };
        close(
            rank("'a':1,2,3", "'a'", false, &arena).unwrap(),
            0.082_745_634,
        );
        close(
            rank("'a':1A 'b':4B", "'a' & 'b'", false, &arena).unwrap(),
            0.615_749_06,
        );
        close(
            rank_with_options(
                "'a':1A 'b':4B",
                "'a' | 'b'",
                false,
                [0.2, 0.4, 0.6, 0.8],
                32,
                &arena,
            )
            .unwrap(),
            0.298_515_86,
        );
    }

    #[test]
    fn structural_order_matches_postgresql() {
        for (left, right, expected) in [
            ("'a'", "'a':1", Ordering::Less),
            ("'a':1", "'a':2", Ordering::Greater),
            ("'a':1A", "'a':1B", Ordering::Less),
            ("'a' 'b'", "'aa'", Ordering::Greater),
            ("'a':1,2", "'a':1", Ordering::Greater),
            ("'can\\\'t'", "'can\\\\\\\\t'", Ordering::Less),
        ] {
            assert_eq!(
                compare_vector(left, right),
                expected,
                "{left} versus {right}"
            );
        }
        for (left, right, expected) in [
            ("'a'", "'b'", Ordering::Less),
            ("'a'", "!'a'", Ordering::Less),
            ("'a' & 'b'", "'a' | 'b'", Ordering::Greater),
            ("'a' <2> 'b'", "'a' <3> 'b'", Ordering::Greater),
            ("'a' & 'b'", "'b' & 'a'", Ordering::Greater),
            ("'a'", "'aa'", Ordering::Less),
        ] {
            assert_eq!(
                compare_query(left, right),
                expected,
                "{left} versus {right}"
            );
        }
    }

    #[test]
    fn cover_density_rank_matches_postgresql_extents_and_normalization() {
        let arena = arena();
        let cases = [
            (
                "'a':1A 'b':2B 'a':5C 'b':8D 'c':9",
                "'a' & 'b'",
                [
                    0.704_761_9,
                    0.393_335_1,
                    0.140_952_38,
                    0.195_767_2,
                    0.413_407_83,
                ],
            ),
            (
                "'a':1 'b':2 'c':3",
                "'a' <-> 'b'",
                [0.1, 0.072_134_756, 0.033_333_335, 0.1, 0.090_909_09],
            ),
            (
                "'a':1 'b':4 'c':5 'a':7 'b':8",
                "'a' | 'b'",
                [0.4, 0.223_244_25, 0.08, 0.166_666_67, 0.285_714_3],
            ),
            (
                "'a':1 'b':2",
                "'a' & !'c'",
                [0.1, 0.091_023_92, 0.05, 0.1, 0.090_909_09],
            ),
        ];
        for (vector, query, expected) in cases {
            for (normalization, expected) in [0, 1, 2, 4, 32].into_iter().zip(expected) {
                let actual = rank_with_options(
                    vector,
                    query,
                    true,
                    [0.1, 0.2, 0.4, 1.0],
                    normalization,
                    &arena,
                )
                .unwrap();
                assert!(
                    (actual - expected).abs() < 0.000_001,
                    "{vector}, {query}, {normalization}: {actual} != {expected}"
                );
            }
        }
    }

    #[test]
    fn query_rewrite_replaces_subtrees_and_cleans_empty_branches() {
        let arena = arena();
        assert_eq!(
            rewrite_query("'a' & ( 'b' | 'a':*A )", "'a'", "'c'", &arena).unwrap(),
            "'c' & ( 'b' | 'c' )",
        );
        assert_eq!(
            rewrite_query("'a' & 'b'", "'b'", "", &arena).unwrap(),
            "'a'"
        );
    }

    #[test]
    fn querytree_removes_non_indexable_branches_like_postgresql() {
        let arena = arena();
        for (source, expected) in [
            ("'a' & !'b'", "'a'"),
            ("'a' | !'b'", "T"),
            ("'a' <-> !'b'", "'a'"),
            ("!'a' & !'b'", "T"),
            ("( 'a' & !'b' ) | 'c'", "'a' | 'c'"),
            ("( 'a' | !'b' ) & 'c'", "'c'"),
            ("'a' <2> 'b' & !'c'", "'a' <2> 'b'"),
        ] {
            assert_eq!(query_tree(source, &arena).unwrap(), expected, "{source}");
        }
    }
}
