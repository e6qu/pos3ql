//! `ts_headline` support, ported from PostgreSQL 18.6: the headline
//! framework in
//! [`ts_parse.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tsearch/ts_parse.c)
//! (`hlparsetext`, `hlfinditem`, `generateHeadline`), the default parser's
//! selector in
//! [`wparser_def.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tsearch/wparser_def.c)
//! (`prsd_headline`, `hlCover`, `mark_hl_words`, `mark_hl_fragments`), the
//! position-reporting query executor it uses from
//! [`tsvector_op.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/tsvector_op.c)
//! (`TS_execute`, `TS_phrase_execute`, `TS_execute_locations`) and the option
//! list parser from
//! [`tsearchcmds.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/tsearchcmds.c)
//! (`deserialize_deflist`).
//!
//! The headline callback never answers "maybe", so the executor's ternary
//! results reduce to booleans here.

use super::TextParser;
use super::tables::{
    ASCIIHWORD, DECIMAL_T, HWORD, NUMHWORD, PROTOCOL, SCIENTIFIC, SIGNEDINT, SPACE, TAG_T,
    UNSIGNEDINT, URL_T, VERSIONNUMBER, XMLENTITY,
};
use crate::mem::arena::{Arena, ArenaList};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::full_text::{self, Query, QueryNode, TextSearchLexeme};
use crate::sql_err;

/// `MAXENTRYPOS`: positions saturate below it (`LIMITPOS`).
const MAX_ENTRY_POSITION: i32 = 1 << 14;

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "ts_headline exceeds the statement arena"
    )
}

/// `HeadlineWordEntry`.
#[derive(Clone, Copy)]
struct Word {
    /// The token's byte range in the document.
    start: usize,
    len: usize,
    token_type: u8,
    position: u16,
    /// The matching query operand: its node index in the parsed query.
    item: Option<u32>,
    selected: bool,
    included: bool,
    replace: bool,
    repeated: bool,
    skip: bool,
}

/// State a headline carries between the strings of one JSON document:
/// PostgreSQL keeps counting lexeme positions across them.
#[derive(Default)]
pub(crate) struct HeadlineState {
    vector_position: i32,
}

/// One headline option (`DefElem`), with its value as `defGetString` gives it.
#[derive(Clone, Copy)]
pub(crate) struct HeadlineOption<'a> {
    name: &'a str,
    value: &'a str,
}

/// Headline options: raw text that a plain-text headline parses after its
/// document, as `ts_headline_byid_opt` does, or a list parsed up front, as
/// the JSON variants do before visiting any string.
#[derive(Clone, Copy)]
pub(crate) enum HeadlineOptions<'o, 'a> {
    Text(Option<&'o str>),
    Parsed(&'a [HeadlineOption<'a>]),
}

/// `deserialize_deflist`: parses `name = value, ...` into options.
pub(crate) fn parse_headline_options<'a>(
    text: &str,
    arena: &'a Arena,
) -> Result<&'a [HeadlineOption<'a>], SqlError> {
    #[derive(PartialEq)]
    enum State {
        WaitKey,
        InKey,
        InQuotedKey,
        WaitEq,
        WaitValue,
        InSingleQuotedValue,
        InDoubleQuotedValue,
        InWordValue,
    }
    let invalid = || {
        sql_err!(
            sqlstate::SYNTAX_ERROR,
            "invalid parameter list format: \"{}\"",
            text
        )
    };
    let bytes = text.as_bytes();
    let workspace = arena
        .alloc_slice_with(bytes.len(), |_| 0u8)
        .map_err(|_| arena_full())?;
    let mut options = ArenaList::new(arena);
    let mut key = 0..0;
    let mut written = 0usize;
    let mut value_start = 0usize;
    let mut state = State::WaitKey;
    let mut finish = |key: &core::ops::Range<usize>,
                      value: core::ops::Range<usize>,
                      quoted: bool,
                      workspace: &[u8]|
     -> Result<(), SqlError> {
        let name = arena
            .alloc_str(utf8(&workspace[key.clone()]))
            .map_err(|_| arena_full())?;
        let value = option_value(utf8(&workspace[value]), quoted, arena)?;
        options
            .push(HeadlineOption { name, value })
            .map_err(|_| arena_full())
    };
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = bytes[at];
        let next = bytes.get(at + 1).copied();
        match state {
            State::WaitKey => {
                if !(c_space(byte) || byte == b',') {
                    written = 0;
                    if byte == b'"' {
                        state = State::InQuotedKey;
                    } else {
                        workspace[written] = byte;
                        written += 1;
                        state = State::InKey;
                    }
                    key = 0..0;
                }
            }
            State::InKey => {
                if c_space(byte) || byte == b'=' {
                    key = 0..written;
                    state = if byte == b'=' {
                        State::WaitValue
                    } else {
                        State::WaitEq
                    };
                } else {
                    workspace[written] = byte;
                    written += 1;
                }
            }
            State::InQuotedKey => {
                if byte == b'"' && next == Some(b'"') {
                    workspace[written] = byte;
                    written += 1;
                    at += 1;
                } else if byte == b'"' {
                    key = 0..written;
                    state = State::WaitEq;
                } else {
                    workspace[written] = byte;
                    written += 1;
                }
            }
            State::WaitEq => {
                if byte == b'=' {
                    state = State::WaitValue;
                } else if !c_space(byte) {
                    return Err(invalid());
                }
            }
            State::WaitValue => {
                value_start = written;
                if byte == b'\'' {
                    state = State::InSingleQuotedValue;
                } else if byte == b'E' && next == Some(b'\'') {
                    at += 1;
                    state = State::InSingleQuotedValue;
                } else if byte == b'"' {
                    state = State::InDoubleQuotedValue;
                } else if !c_space(byte) {
                    workspace[written] = byte;
                    written += 1;
                    state = State::InWordValue;
                }
            }
            State::InSingleQuotedValue | State::InDoubleQuotedValue => {
                let quote = if state == State::InSingleQuotedValue {
                    b'\''
                } else {
                    b'"'
                };
                if byte == quote && next == Some(quote) {
                    workspace[written] = byte;
                    written += 1;
                    at += 1;
                } else if byte == quote {
                    finish(&key, value_start..written, true, workspace)?;
                    state = State::WaitKey;
                } else if state == State::InSingleQuotedValue
                    && byte == b'\\'
                    && next == Some(b'\\')
                {
                    workspace[written] = byte;
                    written += 1;
                    at += 1;
                } else {
                    workspace[written] = byte;
                    written += 1;
                }
            }
            State::InWordValue => {
                if byte == b',' || c_space(byte) {
                    finish(&key, value_start..written, false, workspace)?;
                    state = State::WaitKey;
                } else {
                    workspace[written] = byte;
                    written += 1;
                }
            }
        }
        at += 1;
    }
    if state == State::InWordValue {
        finish(&key, value_start..written, false, workspace)?;
    } else if state != State::WaitKey {
        return Err(invalid());
    }
    drop(finish);
    Ok(options.as_slice())
}

/// The input is UTF-8 and the parser splits only at ASCII bytes.
fn utf8(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).expect("option text splits at ASCII bytes")
}

fn c_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `buildDefItem` followed by `defGetString`: an unquoted value that
/// `strtoint` reads completely becomes an Integer and prints back in decimal;
/// a float, `true` or `false` keeps its spelling.
fn option_value<'a>(value: &str, quoted: bool, arena: &'a Arena) -> Result<&'a str, SqlError> {
    if !quoted
        && !value.is_empty()
        && let Some(integer) = c_strtoint(value)
    {
        let mut buffer = crate::util::StackStr::<16>::new();
        core::fmt::Write::write_fmt(&mut buffer, format_args!("{integer}"))
            .expect("an i32 fits 16 bytes");
        return arena.alloc_str(buffer.as_str()).map_err(|_| arena_full());
    }
    arena.alloc_str(value).map_err(|_| arena_full())
}

/// C `strtol` in base 10 that must consume the whole string and fit an
/// `int` (`strtoint`).
fn c_strtoint(text: &str) -> Option<i32> {
    let trimmed = text.trim_start_matches(|c: char| c.is_ascii() && c_space(c as u8));
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut value = 0i64;
    for byte in digits.bytes() {
        value = value * 10 + i64::from(byte - b'0');
        if value > i64::from(i32::MAX) + 1 {
            return None;
        }
    }
    i32::try_from(if negative { -value } else { value }).ok()
}

/// Options as `prsd_headline` reads them, validated.
struct Selection<'a> {
    min_words: i32,
    max_words: i32,
    short_word: i32,
    max_fragments: i32,
    highlight_all: bool,
    start: &'a str,
    stop: &'a str,
    fragment_delimiter: &'a str,
}

fn read_options<'a>(options: &[HeadlineOption<'a>]) -> Result<Selection<'a>, SqlError> {
    let mut selection = Selection {
        min_words: 15,
        max_words: 35,
        short_word: 3,
        max_fragments: 0,
        highlight_all: false,
        start: "<b>",
        stop: "</b>",
        fragment_delimiter: " ... ",
    };
    let integer = |value: &str| {
        crate::sql::eval::parse_int_bounded(
            value,
            i64::from(i32::MIN),
            i64::from(i32::MAX),
            "integer",
        )
        .map(|value| value as i32)
    };
    for option in options {
        let name = option.name;
        let value = option.value;
        if name.eq_ignore_ascii_case("MaxWords") {
            selection.max_words = integer(value)?;
        } else if name.eq_ignore_ascii_case("MinWords") {
            selection.min_words = integer(value)?;
        } else if name.eq_ignore_ascii_case("ShortWord") {
            selection.short_word = integer(value)?;
        } else if name.eq_ignore_ascii_case("MaxFragments") {
            selection.max_fragments = integer(value)?;
        } else if name.eq_ignore_ascii_case("StartSel") {
            selection.start = value;
        } else if name.eq_ignore_ascii_case("StopSel") {
            selection.stop = value;
        } else if name.eq_ignore_ascii_case("FragmentDelimiter") {
            selection.fragment_delimiter = value;
        } else if name.eq_ignore_ascii_case("HighlightAll") {
            selection.highlight_all = ["1", "on", "true", "t", "y", "yes"]
                .iter()
                .any(|accepted| value.eq_ignore_ascii_case(accepted));
        } else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "unrecognized headline parameter: \"{}\"",
                name
            ));
        }
    }
    if !selection.highlight_all {
        let invalid = |message: core::fmt::Arguments<'_>| {
            sql_err!(sqlstate::INVALID_PARAMETER_VALUE, "{}", message)
        };
        if selection.min_words >= selection.max_words {
            return Err(invalid(format_args!(
                "{} must be less than {}",
                "MinWords", "MaxWords"
            )));
        }
        if selection.min_words <= 0 {
            return Err(invalid(format_args!("{} must be positive", "MinWords")));
        }
        if selection.short_word < 0 {
            return Err(invalid(format_args!("{} must be >= 0", "ShortWord")));
        }
        if selection.max_fragments < 0 {
            return Err(invalid(format_args!("{} must be >= 0", "MaxFragments")));
        }
    }
    for (name, value) in [
        ("StartSel", selection.start),
        ("StopSel", selection.stop),
        ("FragmentDelimiter", selection.fragment_delimiter),
    ] {
        if value.len() > i16::MAX as usize {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "value for \"{}\" is too long",
                name
            ));
        }
    }
    Ok(selection)
}

/// `ts_headline` over one document (or one JSON string value): parses and
/// normalizes it (`hlparsetext`), selects words (`prsd_headline`) and renders
/// the result (`generateHeadline`).
pub(crate) fn headline<'a>(
    document: &str,
    query_text: &str,
    options: HeadlineOptions<'_, 'a>,
    state: &mut HeadlineState,
    arena: &'a Arena,
    mut normalize: impl FnMut(u8, &str, &'a Arena) -> Result<TextSearchLexeme<'a>, SqlError>,
) -> Result<&'a str, SqlError> {
    let query = full_text::parse_query(query_text, arena)?;
    let mut operands = ArenaList::new(arena);
    if let Some(root) = query.root() {
        polish_operands(&query, root, &mut operands)?;
    }
    let operands = operands.as_slice();

    let mut words = ArenaList::new(arena);
    let mut parser = TextParser::new(document, arena);
    while let Some((token_type, token)) = parser.next_token()? {
        if token.len() >= super::MAX_PARSED_WORD_BYTES {
            super::long_word_notice()?;
            continue;
        }
        let lexeme = normalize(token_type, token, arena)?;
        if !matches!(lexeme, TextSearchLexeme::Unmapped) {
            state.vector_position += 1;
        }
        let original = words.len();
        words
            .push(Word {
                start: token.as_ptr() as usize - document.as_ptr() as usize,
                len: token.len(),
                token_type,
                position: 0,
                item: None,
                selected: false,
                included: false,
                replace: false,
                repeated: false,
                skip: false,
            })
            .map_err(|_| arena_full())?;
        if let TextSearchLexeme::Lexeme(lexeme) = lexeme {
            find_items(
                &mut words,
                original,
                &query,
                operands,
                state.vector_position,
                lexeme,
            )?;
        }
    }
    let words = words.into_mut_slice().map_err(|_| arena_full())?;

    let options = match options {
        HeadlineOptions::Text(None) => &[][..],
        HeadlineOptions::Text(Some(text)) => parse_headline_options(text, arena)?,
        HeadlineOptions::Parsed(options) => options,
    };
    let selection = read_options(options)?;
    // A failed match leaves no locations, as does an empty query.
    let mut locations = ArenaList::new(arena);
    if let Some(root) = query.root() {
        execute_locations(&query, root, words, arena, &mut locations)?;
    }
    let locations = locations.as_slice();
    if selection.max_fragments == 0 {
        mark_words(words, &query, locations, &selection, arena)?;
    } else {
        mark_fragments(words, &query, locations, &selection, arena)?;
    }
    generate(document, words, &selection, arena)
}

/// The query's operands in PostgreSQL's stored (Polish) order: an operator,
/// then its right operand, then its left.
fn polish_operands(
    query: &Query<'_>,
    node: u32,
    out: &mut ArenaList<'_, u32>,
) -> Result<(), SqlError> {
    match query.node(node).expect("parsed query node") {
        QueryNode::Lexeme { .. } => out.push(node).map_err(|_| arena_full()),
        QueryNode::Not(child) => polish_operands(query, child, out),
        QueryNode::And(left, right)
        | QueryNode::Or(left, right)
        | QueryNode::Phrase { left, right, .. } => {
            polish_operands(query, right, out)?;
            polish_operands(query, left, out)
        }
    }
}

/// `hlfinditem`: records the lexeme's position on the word just added and
/// links it to every matching operand, repeating the word for each match
/// after the first.
fn find_items<'a>(
    words: &mut ArenaList<'a, Word>,
    original: usize,
    query: &Query<'_>,
    operands: &[u32],
    position: i32,
    lexeme: &str,
) -> Result<(), SqlError> {
    let mut word = words.borrowed_slice()[original];
    word.position = position.min(MAX_ENTRY_POSITION - 1) as u16;
    for &operand in operands {
        let Some(QueryNode::Lexeme { text, prefix, .. }) = query.node(operand) else {
            continue;
        };
        let matched = if prefix {
            lexeme.as_bytes().starts_with(text.as_bytes())
        } else {
            lexeme == text
        };
        if !matched {
            continue;
        }
        if word.item.is_some() {
            words
                .push(Word {
                    item: Some(operand),
                    repeated: true,
                    ..word
                })
                .map_err(|_| arena_full())?;
        } else {
            word.item = Some(operand);
        }
    }
    words.as_mut_slice().map_err(|_| arena_full())?[original] = word;
    Ok(())
}

/// `ExecPhraseData`. Positions are raw `WordEntryPos` values.
#[derive(Clone, Copy, Default)]
struct PhraseData<'a> {
    positions: &'a [u16],
    width: i32,
    negate: bool,
}

/// `checkcondition_HL`: whether `operand` matches a word, and with `data`,
/// the increasing positions of its matches.
fn check_operand<'a>(
    words: &[Word],
    operand: u32,
    data: Option<&mut PhraseData<'a>>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let matches = || {
        let mut last = None;
        words.iter().filter_map(move |word| {
            if word.item != Some(operand) || last.is_some_and(|last| word.position <= last) {
                return None;
            }
            last = Some(word.position);
            last
        })
    };
    let count = matches().count();
    let Some(data) = data else {
        return Ok(count > 0);
    };
    if count > 0 {
        let mut positions = matches();
        data.positions = arena
            .alloc_slice_with(count, |_| positions.next().expect("counted positions"))
            .map_err(|_| arena_full())?;
    }
    Ok(count > 0)
}

/// `TS_execute` with `TS_EXEC_EMPTY` over the headline words.
fn execute(query: &Query<'_>, node: u32, words: &[Word], arena: &Arena) -> Result<bool, SqlError> {
    Ok(match query.node(node).expect("parsed query node") {
        QueryNode::Lexeme { .. } => check_operand(words, node, None, arena)?,
        QueryNode::Not(child) => !execute(query, child, words, arena)?,
        QueryNode::And(left, right) => {
            execute(query, left, words, arena)? && execute(query, right, words, arena)?
        }
        QueryNode::Or(left, right) => {
            execute(query, left, words, arena)? || execute(query, right, words, arena)?
        }
        QueryNode::Phrase { .. } => phrase_execute(query, node, words, None, arena)?,
    })
}

const EMIT_LEFT_ONLY: u8 = 0x01;
const EMIT_RIGHT_ONLY: u8 = 0x02;
const EMIT_BOTH: u8 = 0x04;

/// `TS_phrase_output`: merges two sorted position lists.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors TS_phrase_output's offsets and output bound"
)]
fn phrase_output<'a>(
    data: Option<&mut PhraseData<'a>>,
    left: &PhraseData<'_>,
    right: &PhraseData<'_>,
    emit: u8,
    left_offset: i32,
    right_offset: i32,
    max_positions: usize,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let mut output = match data {
        Some(_) if max_positions > 0 => Some(
            arena
                .alloc_slice_with(max_positions, |_| 0u16)
                .map_err(|_| arena_full())?,
        ),
        _ => None,
    };
    let mut count = 0usize;
    let (mut left_index, mut right_index) = (0usize, 0usize);
    while left_index < left.positions.len() || right_index < right.positions.len() {
        let left_position = match left.positions.get(left_index) {
            Some(&position) => i32::from(position & 0x3fff) + left_offset,
            None if emit & EMIT_RIGHT_ONLY == 0 => break,
            None => i32::MAX,
        };
        let right_position = match right.positions.get(right_index) {
            Some(&position) => i32::from(position & 0x3fff) + right_offset,
            None if emit & EMIT_LEFT_ONLY == 0 => break,
            None => i32::MAX,
        };
        let mut output_position = 0;
        if left_position < right_position {
            if emit & EMIT_LEFT_ONLY != 0 {
                output_position = left_position;
            }
            left_index += 1;
        } else if left_position == right_position {
            if emit & EMIT_BOTH != 0 {
                output_position = right_position;
            }
            left_index += 1;
            right_index += 1;
        } else {
            if emit & EMIT_RIGHT_ONLY != 0 {
                output_position = right_position;
            }
            right_index += 1;
        }
        if output_position > 0 {
            let Some(output) = output.as_deref_mut() else {
                return Ok(true);
            };
            // PostgreSQL stores the position in a 16-bit `WordEntryPos`.
            output[count] = output_position as u16;
            count += 1;
        }
    }
    if let (Some(data), Some(output)) = (data, output) {
        data.positions = &output[..count];
    }
    Ok(count > 0)
}

/// `TS_phrase_execute` with `TS_EXEC_EMPTY`.
fn phrase_execute<'a>(
    query: &Query<'_>,
    node: u32,
    words: &[Word],
    mut data: Option<&mut PhraseData<'a>>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let (left, right, distance, is_or) = match query.node(node).expect("parsed query node") {
        QueryNode::Lexeme { .. } => return check_operand(words, node, data, arena),
        QueryNode::Not(child) => {
            let data = data.expect("phrase NOT reports positions");
            return Ok(
                if !phrase_execute(query, child, words, Some(&mut *data), arena)? {
                    data.negate = true;
                    true
                } else if !data.positions.is_empty() {
                    data.negate = !data.negate;
                    true
                } else {
                    assert!(data.negate, "a phrase match without positions is negated");
                    data.negate = false;
                    false
                },
            );
        }
        QueryNode::And(left, right) => (left, right, None, false),
        QueryNode::Phrase {
            left,
            right,
            distance,
        } => (left, right, Some(i32::from(distance)), false),
        QueryNode::Or(left, right) => (left, right, None, true),
    };
    let mut left_data = PhraseData::default();
    let mut right_data = PhraseData::default();
    let left_match = phrase_execute(query, left, words, Some(&mut left_data), arena)?;
    if !is_or && !left_match {
        return Ok(false);
    }
    let right_match = phrase_execute(query, right, words, Some(&mut right_data), arena)?;
    if !is_or && !right_match {
        return Ok(false);
    }
    let (left_npos, right_npos) = (left_data.positions.len(), right_data.positions.len());
    if is_or {
        if !left_match && !right_match {
            return Ok(false);
        }
        if !left_match {
            left_data.width = 0;
        }
        if !right_match {
            right_data.width = 0;
        }
    }
    let (left_offset, right_offset, width) = match distance {
        Some(distance) => (
            distance + right_data.width,
            0,
            distance + left_data.width + right_data.width,
        ),
        None => {
            let width = left_data.width.max(right_data.width);
            (width - left_data.width, width - right_data.width, width)
        }
    };
    if let Some(data) = data.as_deref_mut() {
        data.width = width;
    }
    let all = EMIT_BOTH | EMIT_LEFT_ONLY | EMIT_RIGHT_ONLY;
    let output = |data: Option<&mut PhraseData<'a>>, emit, max| {
        phrase_output(
            data,
            &left_data,
            &right_data,
            emit,
            left_offset,
            right_offset,
            max,
            arena,
        )
    };
    match (left_data.negate, right_data.negate, is_or) {
        (true, true, false) => {
            output(data.as_deref_mut(), all, left_npos + right_npos)?;
            if let Some(data) = data {
                data.negate = true;
            }
            Ok(true)
        }
        (true, false, false) => output(data, EMIT_RIGHT_ONLY, right_npos),
        (false, true, false) => output(data, EMIT_LEFT_ONLY, left_npos),
        (false, false, false) => output(data, EMIT_BOTH, left_npos.min(right_npos)),
        (left_negate, right_negate, true) => {
            let data = data.expect("phrase OR reports positions");
            let (emit, max) = match (left_negate, right_negate) {
                (true, true) => (EMIT_BOTH, left_npos.min(right_npos)),
                (true, false) => (EMIT_LEFT_ONLY, left_npos),
                (false, true) => (EMIT_RIGHT_ONLY, right_npos),
                (false, false) => return output(Some(data), all, left_npos + right_npos),
            };
            output(Some(&mut *data), emit, max)?;
            data.negate = true;
            Ok(true)
        }
    }
}

/// `TS_execute_locations_recurse`: on a match, appends one entry per AND'ed
/// term or phrase to `locations`.
fn execute_locations<'a>(
    query: &Query<'_>,
    node: u32,
    words: &[Word],
    arena: &'a Arena,
    locations: &mut ArenaList<'a, PhraseData<'a>>,
) -> Result<bool, SqlError> {
    let push = |locations: &mut ArenaList<'a, PhraseData<'a>>, data| {
        locations.push(data).map_err(|_| arena_full())
    };
    match query.node(node).expect("parsed query node") {
        QueryNode::Lexeme { .. } => {
            let mut data = PhraseData::default();
            if check_operand(words, node, Some(&mut data), arena)? {
                push(locations, data)?;
                return Ok(true);
            }
            Ok(false)
        }
        QueryNode::Not(child) => {
            let mut ignored = ArenaList::new(arena);
            Ok(!execute_locations(
                query,
                child,
                words,
                arena,
                &mut ignored,
            )?)
        }
        QueryNode::And(left, right) => {
            let mut left_locations = ArenaList::new(arena);
            if !execute_locations(query, left, words, arena, &mut left_locations)? {
                return Ok(false);
            }
            let mut right_locations = ArenaList::new(arena);
            if !execute_locations(query, right, words, arena, &mut right_locations)? {
                return Ok(false);
            }
            for &data in left_locations
                .borrowed_slice()
                .iter()
                .chain(right_locations.borrowed_slice())
            {
                push(locations, data)?;
            }
            Ok(true)
        }
        QueryNode::Or(left, right) => {
            let mut left_locations = ArenaList::new(arena);
            let mut right_locations = ArenaList::new(arena);
            let left_match = execute_locations(query, left, words, arena, &mut left_locations)?;
            let right_match = execute_locations(query, right, words, arena, &mut right_locations)?;
            if !left_match && !right_match {
                return Ok(false);
            }
            let (left_list, right_list) = (
                left_locations.borrowed_slice(),
                right_locations.borrowed_slice(),
            );
            if left_list.is_empty() || right_list.is_empty() {
                for &data in left_list.iter().chain(right_list) {
                    push(locations, data)?;
                }
                return Ok(true);
            }
            for left_data in left_list {
                for right_data in right_list {
                    let mut data = PhraseData::default();
                    phrase_output(
                        Some(&mut data),
                        left_data,
                        right_data,
                        EMIT_BOTH | EMIT_LEFT_ONLY | EMIT_RIGHT_ONLY,
                        0,
                        0,
                        left_data.positions.len() + right_data.positions.len(),
                        arena,
                    )?;
                    data.width = left_data.width.max(right_data.width);
                    push(locations, data)?;
                }
            }
            Ok(true)
        }
        QueryNode::Phrase { .. } => {
            let mut data = PhraseData::default();
            if phrase_execute(query, node, words, Some(&mut data), arena)? {
                if !data.negate {
                    push(locations, data)?;
                }
                return Ok(true);
            }
            Ok(false)
        }
    }
}

fn non_word(token_type: u8) -> bool {
    matches!(
        token_type,
        SPACE | TAG_T | URL_T | NUMHWORD | ASCIIHWORD | HWORD
    )
}

fn no_end(token_type: u8) -> bool {
    non_word(token_type)
        || matches!(
            token_type,
            SCIENTIFIC
                | VERSIONNUMBER
                | DECIMAL_T
                | SIGNEDINT
                | UNSIGNEDINT
                | TAG_T
                | PROTOCOL
                | SPACE
                | XMLENTITY
        )
}

fn interesting(word: &Word) -> bool {
    word.item.is_some() && !word.repeated
}

fn bad_endpoint(word: &Word, short_word: i32) -> bool {
    (no_end(word.token_type) || word.len as i64 <= i64::from(short_word)) && !interesting(word)
}

/// `hlCover`: the next minimal word range satisfying the query, starting at
/// lexeme position `*next`.
fn cover(
    words: &[Word],
    query: &Query<'_>,
    locations: &[PhraseData<'_>],
    next: &mut i32,
    arena: &Arena,
) -> Result<Option<(usize, usize)>, SqlError> {
    let root = query.root().expect("a query with locations has a root");
    let mut position = *next;
    loop {
        let mut end = -1;
        for data in locations {
            let Some(first) = data
                .positions
                .iter()
                .map(|&position| i32::from(position))
                .find(|&candidate| candidate >= position)
            else {
                return Ok(None);
            };
            end = end.max(first);
        }
        if end < 0 {
            return Ok(None);
        }
        let mut begin = i32::MAX - 1;
        for data in locations {
            let last = data
                .positions
                .iter()
                .rev()
                .map(|&position| i32::from(position) - data.width)
                .find(|&start| start <= end)
                .unwrap_or(-1);
            begin = begin.min(last);
        }
        begin = begin.max(position);
        if begin <= end {
            let mut first = None;
            let mut last = None;
            for (index, word) in words.iter().enumerate() {
                if word.item.is_none() {
                    continue;
                }
                if first.is_none() && i32::from(word.position) >= begin {
                    first = Some(index);
                }
                if i32::from(word.position) <= end {
                    last = Some(index);
                } else {
                    break;
                }
            }
            if let (Some(first), Some(last)) = (first, last)
                && last >= first
                && execute(query, root, &words[first..=last], arena)?
            {
                *next = begin + 1;
                return Ok(Some((first, last)));
            }
        }
        position = begin + 1;
    }
}

/// `mark_fragment`.
fn mark_fragment(words: &mut [Word], highlight_all: bool, start: usize, end: isize) {
    for index in start as isize..=end {
        let word = &mut words[index as usize];
        if word.item.is_some() {
            word.selected = true;
        }
        if !highlight_all {
            if word.token_type == TAG_T {
                word.replace = true;
            } else if matches!(word.token_type, URL_T | NUMHWORD | ASCIIHWORD | HWORD) {
                word.skip = true;
            }
        } else if matches!(word.token_type, URL_T | NUMHWORD | ASCIIHWORD | HWORD) {
            word.skip = true;
        }
        word.included = !word.repeated;
    }
}

/// The first `min_words` words, when nothing else was chosen.
fn leading_words(words: &[Word], min_words: i32) -> isize {
    let mut length = 0;
    let mut end = -1isize;
    let mut index = 0usize;
    while index < words.len() && length < min_words {
        if !non_word(words[index].token_type) {
            length += 1;
        }
        end = index as isize;
        index += 1;
    }
    end
}

/// `mark_hl_words`: the headline selector when `MaxFragments` is 0.
fn mark_words(
    words: &mut [Word],
    query: &Query<'_>,
    locations: &[PhraseData<'_>],
    selection: &Selection<'_>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let (min_words, max_words, short_word) = (
        selection.min_words,
        selection.max_words,
        selection.short_word,
    );
    let count = words.len() as isize;
    let (best_begin, best_end);
    if !selection.highlight_all {
        let mut next = 0;
        let mut begin_best = -1isize;
        let mut end_best = -1isize;
        let mut best_length = -1i32;
        let mut best_cover = false;
        let bad = |index: isize| bad_endpoint(&words[index as usize], short_word);
        while let Some((p, q)) = if locations.is_empty() {
            None
        } else {
            cover(words, query, locations, &mut next, arena)?
        } {
            let (p, q) = (p as isize, q as isize);
            let mut length = 0;
            let mut interesting_count = 0;
            let mut begin = p;
            let mut end = p;
            let mut i = p;
            while i <= q && length < max_words {
                if !non_word(words[i as usize].token_type) {
                    length += 1;
                }
                if interesting(&words[i as usize]) {
                    interesting_count += 1;
                }
                end = i;
                i += 1;
            }
            if length < max_words {
                i -= 1;
                while i < count && length < max_words {
                    if i > q {
                        if !non_word(words[i as usize].token_type) {
                            length += 1;
                        }
                        if interesting(&words[i as usize]) {
                            interesting_count += 1;
                        }
                    }
                    end = i;
                    if bad(i) {
                        i += 1;
                        continue;
                    }
                    if length >= min_words {
                        break;
                    }
                    i += 1;
                }
                if length < min_words {
                    i = p - 1;
                    while i >= 0 {
                        if !non_word(words[i as usize].token_type) {
                            length += 1;
                        }
                        if interesting(&words[i as usize]) {
                            interesting_count += 1;
                        }
                        if length >= max_words {
                            break;
                        }
                        if bad(i) {
                            i -= 1;
                            continue;
                        }
                        if length >= min_words {
                            break;
                        }
                        i -= 1;
                    }
                    begin = i.max(0);
                }
            } else {
                if i > q {
                    i = q;
                }
                while length > min_words {
                    if !bad(i) {
                        break;
                    }
                    if !non_word(words[i as usize].token_type) {
                        length -= 1;
                    }
                    if interesting(&words[i as usize]) {
                        interesting_count -= 1;
                    }
                    end = i - 1;
                    i -= 1;
                }
            }
            let covers = begin <= p && end >= q;
            if covers && !best_cover
                || covers == best_cover && interesting_count > best_length
                || covers == best_cover
                    && interesting_count == best_length
                    && !bad(end)
                    && bad(end_best)
            {
                begin_best = begin;
                end_best = end;
                best_length = interesting_count;
                best_cover = covers;
            }
        }
        if best_length < 0 {
            begin_best = 0;
            end_best = leading_words(words, min_words);
        }
        (best_begin, best_end) = (begin_best, end_best);
    } else {
        (best_begin, best_end) = (0, count - 1);
    }
    mark_fragment(
        words,
        selection.highlight_all,
        best_begin as usize,
        best_end,
    );
    Ok(())
}

/// `CoverPos`.
#[derive(Clone, Copy)]
struct Fragment {
    start: isize,
    end: isize,
    interesting: i32,
    length: i32,
    chosen: bool,
    excluded: bool,
}

/// `get_next_fragment`: the next piece of a cover with at most `max_words`
/// words and interesting words at both ends.
fn next_fragment(words: &[Word], start: &mut isize, end: &mut isize, max_words: i32) -> (i32, i32) {
    let mut i = *start;
    while i <= *end {
        *start = i;
        if interesting(&words[i as usize]) {
            break;
        }
        i += 1;
    }
    let mut length = 0;
    let mut interesting_count = 0;
    i = *start;
    while i <= *end && length < max_words {
        if !non_word(words[i as usize].token_type) {
            length += 1;
        }
        if interesting(&words[i as usize]) {
            interesting_count += 1;
        }
        i += 1;
    }
    if *end > i {
        *end = i;
        i = *end;
        while i >= *start {
            *end = i;
            if interesting(&words[i as usize]) {
                break;
            }
            if !non_word(words[i as usize].token_type) {
                length -= 1;
            }
            i -= 1;
        }
    }
    (length, interesting_count)
}

/// `mark_hl_fragments`: the headline selector when `MaxFragments` > 0.
fn mark_fragments(
    words: &mut [Word],
    query: &Query<'_>,
    locations: &[PhraseData<'_>],
    selection: &Selection<'_>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let (min_words, max_words, short_word) = (
        selection.min_words,
        selection.max_words,
        selection.short_word,
    );
    let count = words.len() as isize;
    let mut covers = ArenaList::new(arena);
    let mut next = 0;
    while let Some((p, q)) = if locations.is_empty() {
        None
    } else {
        cover(words, query, locations, &mut next, arena)?
    } {
        let mut start = p as isize;
        let mut end = q as isize;
        while start <= end {
            let (length, interesting_count) = next_fragment(words, &mut start, &mut end, max_words);
            covers
                .push(Fragment {
                    start,
                    end,
                    interesting: interesting_count,
                    length,
                    chosen: false,
                    excluded: false,
                })
                .map_err(|_| arena_full())?;
            start = end + 1;
            end = q as isize;
        }
    }
    let covers = covers.into_mut_slice().map_err(|_| arena_full())?;
    let mut marked = 0;
    for _ in 0..selection.max_fragments {
        let mut max_items = 0;
        let mut min_length = i32::MAX;
        let mut chosen = None;
        for (index, fragment) in covers.iter().enumerate() {
            if !fragment.chosen
                && !fragment.excluded
                && (max_items < fragment.interesting
                    || max_items == fragment.interesting && min_length > fragment.length)
            {
                max_items = fragment.interesting;
                min_length = fragment.length;
                chosen = Some(index);
            }
        }
        let Some(chosen) = chosen else { break };
        covers[chosen].chosen = true;
        let mut start = covers[chosen].start;
        let mut end = covers[chosen].end;
        let mut length = covers[chosen].length;
        if length < max_words {
            let max_stretch = (max_words - length) / 2;
            let mut stretch = 0;
            let mut marker = start;
            let mut i = start - 1;
            while i >= 0 && stretch < max_stretch && !words[i as usize].included {
                if !non_word(words[i as usize].token_type) {
                    length += 1;
                    stretch += 1;
                }
                marker = i;
                i -= 1;
            }
            i = marker;
            while i < start && bad_endpoint(&words[i as usize], short_word) {
                if !non_word(words[i as usize].token_type) {
                    length -= 1;
                }
                i += 1;
            }
            start = i;
            marker = end;
            i = end + 1;
            while i < count && length < max_words && !words[i as usize].included {
                if !non_word(words[i as usize].token_type) {
                    length += 1;
                }
                marker = i;
                i += 1;
            }
            i = marker;
            while i > end && bad_endpoint(&words[i as usize], short_word) {
                if !non_word(words[i as usize].token_type) {
                    length -= 1;
                }
                i -= 1;
            }
            end = i;
        }
        covers[chosen].start = start;
        covers[chosen].end = end;
        covers[chosen].length = length;
        mark_fragment(words, selection.highlight_all, start as usize, end);
        marked += 1;
        for (index, fragment) in covers.iter_mut().enumerate() {
            if index != chosen
                && (fragment.start >= start && fragment.start <= end
                    || fragment.end >= start && fragment.end <= end
                    || fragment.start < start && fragment.end > end)
            {
                fragment.excluded = true;
            }
        }
    }
    if marked == 0 {
        let end = leading_words(words, min_words);
        mark_fragment(words, selection.highlight_all, 0, end);
    }
    Ok(())
}

/// `generateHeadline`.
fn generate<'a>(
    document: &str,
    words: &[Word],
    selection: &Selection<'_>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let render = |mut out: Option<&mut [u8]>| {
        let mut length = 0usize;
        let mut push = |text: &str, out: &mut Option<&mut [u8]>| {
            if let Some(out) = out.as_deref_mut() {
                out[length..length + text.len()].copy_from_slice(text.as_bytes());
            }
            length += text.len();
        };
        let mut in_fragment = false;
        let mut fragments = 0;
        for word in words {
            if word.included && !word.repeated {
                if !in_fragment {
                    in_fragment = true;
                    fragments += 1;
                    if fragments > 1 {
                        push(selection.fragment_delimiter, &mut out);
                    }
                }
                if word.replace {
                    push(" ", &mut out);
                } else if !word.skip {
                    if word.selected {
                        push(selection.start, &mut out);
                    }
                    push(&document[word.start..word.start + word.len], &mut out);
                    if word.selected {
                        push(selection.stop, &mut out);
                    }
                }
            } else if !word.repeated {
                in_fragment = false;
            }
        }
        length
    };
    let length = render(None);
    let bytes = arena
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| arena_full())?;
    render(Some(bytes));
    Ok(core::str::from_utf8(bytes).expect("a headline joins UTF-8 pieces"))
}
