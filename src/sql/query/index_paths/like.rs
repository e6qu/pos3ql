//! Pattern restrictions as `like_support.c` reads them (`LIKE`, `ILIKE`,
//! with `like_escape` applied to an `ESCAPE`, and regular expressions):
//! the pattern's fixed prefix (`like_fixed_prefix`, `regex_fixed_prefix`),
//! the index conditions it gives (`match_pattern_prefix`,
//! `make_greater_string`) and the selectivity of the match
//! (`patternsel_common`, `prefix_selectivity`, `like_selectivity`,
//! `regex_selectivity`).

use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::types::Datum;

use super::{Comparison, Planner, clamp_probability};

/// `DEFAULT_MATCH_SEL`.
const DEFAULT_MATCH_SEL: f64 = 0.005;
const FIXED_CHAR_SEL: f64 = 0.20;
const ANY_CHAR_SEL: f64 = 0.9;
const FULL_WILDCARD_SEL: f64 = 5.0;

/// `CHAR_RANGE_SEL` and `PARTIAL_WILDCARD_SEL`.
const CHAR_RANGE_SEL: f64 = 0.25;
const PARTIAL_WILDCARD_SEL: f64 = 2.0;

/// The kind of a pattern match: `LIKE` (`~~`), `ILIKE` (`~~*`), or a
/// regular expression (`~`, `~*`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PatternKind {
    Like,
    ILike,
    Regex,
    IRegex,
    /// `starts_with(column, prefix)` (`Pattern_Type_Prefix`), through its
    /// support function.
    Prefix,
}

impl PatternKind {
    fn case_insensitive(self) -> bool {
        matches!(self, Self::ILike | Self::IRegex)
    }
}

/// A pattern's fixed prefix (`pattern_fixed_prefix`): the characters every
/// match starts with; `exact` when it is the whole match.
pub(super) struct FixedPrefix<'p> {
    /// `None` without a fixed character (`Pattern_Prefix_None`).
    pub(super) prefix: Option<&'p str>,
    pub(super) exact: bool,
    /// The selectivity of the pattern after the prefix.
    pub(super) rest: f64,
}

/// The fixed prefix of a `kind` pattern under `C`, or the shape outside
/// the model.
pub(super) fn fixed_prefix<'a>(
    kind: PatternKind,
    pattern: &str,
    arena: &'a Arena,
) -> Result<Result<FixedPrefix<'a>, &'static str>, SqlError> {
    match kind {
        PatternKind::Like | PatternKind::ILike => Ok(Ok(like_fixed_prefix(
            pattern,
            kind.case_insensitive(),
            arena,
        )?)),
        PatternKind::Regex | PatternKind::IRegex => {
            regex_fixed_prefix(pattern, kind.case_insensitive(), arena)
        }
        PatternKind::Prefix => Ok(Ok(FixedPrefix {
            prefix: Some(
                arena
                    .alloc_str(pattern)
                    .map_err(|_| crate::sql::eval::arena_full())?,
            )
            .filter(|prefix| !prefix.is_empty()),
            exact: false,
            rest: 1.0,
        })),
    }
}

/// `like_fixed_prefix` of `pattern`, whose escape is `\`: the bytes
/// before its first wildcard, escapes removed, stopping at a letter when
/// `case_insensitive` (the `C` locale's `isalpha`).
fn like_fixed_prefix<'a>(
    pattern: &str,
    case_insensitive: bool,
    arena: &'a Arena,
) -> Result<FixedPrefix<'a>, SqlError> {
    let bytes = pattern.as_bytes();
    let out = arena
        .alloc_slice_with(bytes.len(), |_| 0u8)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut length = 0;
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'%' || bytes[position] == b'_' {
            break;
        }
        if bytes[position] == b'\\' {
            position += 1;
            if position >= bytes.len() {
                break;
            }
        }
        if case_insensitive && bytes[position].is_ascii_alphabetic() {
            break;
        }
        out[length] = bytes[position];
        length += 1;
        position += 1;
    }
    let exact = position >= bytes.len();
    let prefix = core::str::from_utf8(&out[..length]).map_err(|_| {
        crate::sql_err!(
            crate::sql::eval::sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            "invalid byte sequence in a LIKE pattern's prefix"
        )
    })?;
    Ok(FixedPrefix {
        prefix: (length > 0).then_some(prefix),
        exact,
        rest: if exact {
            1.0
        } else {
            like_rest_selectivity(&bytes[position.min(bytes.len())..])
        },
    })
}

/// `like_selectivity` of the pattern after its fixed prefix.
fn like_rest_selectivity(rest: &[u8]) -> f64 {
    let mut selectivity = 1.0;
    let mut position = rest
        .iter()
        .position(|&byte| byte != b'%' && byte != b'_')
        .unwrap_or(rest.len());
    while position < rest.len() {
        match rest[position] {
            b'%' => selectivity *= FULL_WILDCARD_SEL,
            b'_' => selectivity *= ANY_CHAR_SEL,
            b'\\' => {
                position += 1;
                if position >= rest.len() {
                    break;
                }
                selectivity *= FIXED_CHAR_SEL;
            }
            _ => selectivity *= FIXED_CHAR_SEL,
        }
        position += 1;
    }
    selectivity.min(1.0)
}

/// The pattern `like_escape` (`do_like_escape`) makes of `pattern` with
/// the escape `escape`: each escape becomes `\\` and the character after it
/// is kept, and every other `\\` is doubled (every one, when the escape is
/// empty). An escape of several characters is PostgreSQL's error.
pub(crate) fn like_escape<'a>(
    pattern: &str,
    escape: &str,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let escape = crate::sql::eval::escape_char(Datum::Text(escape))?;
    if escape == Some('\\') {
        return arena
            .alloc_str(pattern)
            .map_err(|_| crate::sql::eval::arena_full());
    }
    let out = arena
        .alloc_slice_with(pattern.len() * 2, |_| 0u8)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut length = 0;
    let mut push = |character: char, length: &mut usize| {
        *length += character.encode_utf8(&mut out[*length..]).len();
    };
    let mut after_escape = false;
    for character in pattern.chars() {
        if Some(character) == escape && !after_escape {
            push('\\', &mut length);
            after_escape = true;
        } else if character == '\\' {
            if !after_escape {
                push('\\', &mut length);
            }
            push('\\', &mut length);
            after_escape = false;
        } else {
            push(character, &mut length);
            after_escape = false;
        }
    }
    Ok(core::str::from_utf8(&out[..length]).expect("escaped characters stay UTF-8"))
}

/// `regexp_fixed_prefix` of `pattern` (`pg_regprefix` over its compiled
/// NFA), read from the pattern's structure: after a leading `^`, each
/// character the match must start with, one occurrence of a repeated one,
/// stopping at a character class, wildcard, optional part or (when
/// `case_insensitive`) a cased letter; `exact` when a final `$` follows.
fn regex_fixed_prefix<'a>(
    pattern: &str,
    case_insensitive: bool,
    arena: &'a Arena,
) -> Result<Result<FixedPrefix<'a>, &'static str>, SqlError> {
    let outside = "a regular expression whose fixed prefix the model does not read";
    let none = FixedPrefix {
        prefix: None,
        exact: false,
        rest: regex_selectivity(pattern.as_bytes(), case_insensitive, 0),
    };
    if pattern.starts_with("***") || pattern.starts_with("(?") {
        return Ok(Err(outside));
    }
    // An alternation at the top level leaves an unanchored arm.
    if top_level_bar(pattern.as_bytes()) {
        return Ok(Ok(none));
    }
    let Some(body) = pattern.strip_prefix('^') else {
        return Ok(Ok(none));
    };
    let out = arena
        .alloc_slice_with(pattern.len() * 4, |_| 0u8)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut length = 0;
    let exact = match regex_sequence(body, case_insensitive, out, &mut length) {
        Ok(Sequence::End) => false,
        Ok(Sequence::Exact) => true,
        Ok(Sequence::Stopped) => false,
        Err(()) => return Ok(Err(outside)),
    };
    let prefix = core::str::from_utf8(&out[..length]).expect("characters encode as UTF-8");
    if length == 0 && !exact {
        return Ok(Ok(none));
    }
    Ok(Ok(FixedPrefix {
        prefix: Some(prefix),
        exact,
        rest: if exact {
            1.0
        } else {
            regex_selectivity(pattern.as_bytes(), case_insensitive, length)
        },
    }))
}

/// How the prefix scan of a regular expression ended.
enum Sequence {
    /// At the pattern's end: more may follow the match.
    End,
    /// At a final `$`: the prefix is the whole match.
    Exact,
    /// At a part that is no single fixed character.
    Stopped,
}

/// Whether `pattern` has a `|` outside parentheses and brackets.
fn top_level_bar(pattern: &[u8]) -> bool {
    let mut depth = 0usize;
    let mut position = 0;
    while position < pattern.len() {
        match pattern[position] {
            b'\\' => position += 1,
            b'[' => {
                position = class_end(pattern, position);
            }
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b'|' if depth == 0 => return true,
            _ => {}
        }
        position += 1;
    }
    false
}

/// The position of the `]` closing the bracket expression opened at
/// `open`.
fn class_end(pattern: &[u8], open: usize) -> usize {
    let mut position = open + 1;
    if pattern.get(position) == Some(&b'^') {
        position += 1;
    }
    if pattern.get(position) == Some(&b']') {
        position += 1;
    }
    while position < pattern.len() && pattern[position] != b']' {
        position += 1;
    }
    position
}

/// Appends the fixed characters `pattern` starts with to `out`.
fn regex_sequence(
    pattern: &str,
    case_insensitive: bool,
    out: &mut [u8],
    length: &mut usize,
) -> Result<Sequence, ()> {
    let bytes = pattern.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        // One atom: its text, and whether it is fixed text.
        let (atom_end, fixed): (usize, Option<&str>) = match bytes[position] {
            b'$' => {
                return Ok(if position + 1 == bytes.len() {
                    Sequence::Exact
                } else {
                    Sequence::Stopped
                });
            }
            b'\\' => {
                let Some(next) = pattern[position + 1..].chars().next() else {
                    return Err(());
                };
                if next.is_ascii_alphanumeric() {
                    // A class, constraint, back reference or code escape.
                    return Ok(Sequence::Stopped);
                }
                let end = position + 1 + next.len_utf8();
                (end, Some(&pattern[position + 1..end]))
            }
            b'(' => {
                let close = group_end(bytes, position).ok_or(())?;
                let inner = &pattern[position + 1..close];
                if inner.starts_with('?') {
                    return Ok(Sequence::Stopped);
                }
                // A choice adds the one character every alternative starts
                // with, if there is one, and ends the prefix.
                if top_level_bar(inner.as_bytes()) {
                    if let Some(shared) = shared_first_character(inner, case_insensitive) {
                        append_fixed(shared, case_insensitive, out, length);
                    }
                    return Ok(Sequence::Stopped);
                }
                (close + 1, Some(inner))
            }
            b'[' | b'.' => return Ok(Sequence::Stopped),
            b'*' | b'+' | b'?' | b'{' | b')' | b'|' => return Err(()),
            _ => {
                let character = pattern[position..].chars().next().ok_or(())?;
                (
                    position + character.len_utf8(),
                    Some(&pattern[position..position + character.len_utf8()]),
                )
            }
        };
        let Some(text) = fixed else {
            return Ok(Sequence::Stopped);
        };
        let group = bytes[position] == b'(';
        // Its quantifier: how often the match must hold it, and whether
        // more may follow.
        let (minimum, repeats, quantifier_end) = match bytes.get(atom_end) {
            Some(b'*') | Some(b'?') => (0, true, atom_end + 1),
            Some(b'+') => (1, true, atom_end + 1),
            Some(b'{') => {
                let close = pattern[atom_end..].find('}').ok_or(())? + atom_end;
                let bound = &pattern[atom_end + 1..close];
                let (low, high) = match bound.split_once(',') {
                    Some((low, high)) => (low, Some(high)),
                    None => (bound, None),
                };
                let low: usize = low.parse().map_err(|_| ())?;
                let more = match high {
                    None => false,
                    Some("") => true,
                    Some(high) => high.parse::<usize>().map_err(|_| ())? > low,
                };
                (low, more, close + 1)
            }
            _ => (1, false, atom_end),
        };
        let mut next = quantifier_end;
        // A non-greedy quantifier's `?`.
        if next != atom_end && bytes.get(next) == Some(&b'?') {
            next += 1;
        }
        for _ in 0..minimum {
            let ended = if group {
                regex_sequence(text, case_insensitive, out, length)?
            } else {
                append_fixed(text, case_insensitive, out, length)
            };
            match ended {
                Sequence::End => {}
                Sequence::Exact | Sequence::Stopped => return Ok(Sequence::Stopped),
            }
        }
        if minimum == 0 || repeats {
            return Ok(Sequence::Stopped);
        }
        position = next;
    }
    Ok(Sequence::End)
}

/// The fixed character each top-level alternative of `pattern` must start
/// with, when they share one.
fn shared_first_character(pattern: &str, case_insensitive: bool) -> Option<&str> {
    let bytes = pattern.as_bytes();
    let mut shared: Option<&str> = None;
    let mut start = 0;
    let mut depth = 0usize;
    let mut position = 0;
    loop {
        let end = position >= bytes.len() || (bytes[position] == b'|' && depth == 0);
        if end {
            let first =
                first_character(&pattern[start..position.min(bytes.len())], case_insensitive)?;
            match shared {
                Some(held) if held != first => return None,
                _ => shared = Some(first),
            }
            if position >= bytes.len() {
                return shared;
            }
            start = position + 1;
        } else {
            match bytes[position] {
                b'\\' => position += 1,
                b'[' => position = class_end(bytes, position),
                b'(' => depth += 1,
                b')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        position += 1;
    }
}

/// The fixed character `pattern` must start with, if any.
fn first_character(pattern: &str, case_insensitive: bool) -> Option<&str> {
    let bytes = pattern.as_bytes();
    let (atom_end, text) = match *bytes.first()? {
        b'\\' => {
            let next = pattern[1..].chars().next()?;
            if next.is_ascii_alphanumeric() {
                return None;
            }
            (1 + next.len_utf8(), &pattern[1..1 + next.len_utf8()])
        }
        b'(' => {
            let close = group_end(bytes, 0)?;
            let inner = &pattern[1..close];
            if inner.starts_with('?') {
                return None;
            }
            let first = if top_level_bar(inner.as_bytes()) {
                shared_first_character(inner, case_insensitive)?
            } else {
                first_character(inner, case_insensitive)?
            };
            (close + 1, first)
        }
        b'[' | b'.' | b'$' | b'^' | b'*' | b'+' | b'?' | b'{' | b')' | b'|' => return None,
        _ => {
            let length = pattern.chars().next()?.len_utf8();
            (length, &pattern[..length])
        }
    };
    if matches!(bytes.get(atom_end), Some(b'*' | b'?')) || pattern[atom_end..].starts_with("{0") {
        return None;
    }
    let character = text.chars().next()?;
    if case_insensitive
        && (character.to_lowercase().ne(core::iter::once(character))
            || character.to_uppercase().ne(core::iter::once(character)))
    {
        return None;
    }
    Some(&text[..character.len_utf8()])
}

/// Appends the fixed character `text`; a cased letter under a
/// case-insensitive match stops the prefix.
fn append_fixed(
    text: &str,
    case_insensitive: bool,
    out: &mut [u8],
    length: &mut usize,
) -> Sequence {
    let character = text.chars().next().expect("an atom has a character");
    if case_insensitive
        && (character.to_lowercase().ne(core::iter::once(character))
            || character.to_uppercase().ne(core::iter::once(character)))
    {
        return Sequence::Stopped;
    }
    out[*length..*length + text.len()].copy_from_slice(text.as_bytes());
    *length += text.len();
    Sequence::End
}

/// The position of the `)` closing the group opened at `open`.
fn group_end(pattern: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut position = open;
    while position < pattern.len() {
        match pattern[position] {
            b'\\' => position += 1,
            b'[' => position = class_end(pattern, position),
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(position);
                }
            }
            _ => {}
        }
        position += 1;
    }
    None
}

/// `regex_selectivity` of `pattern` with a fixed prefix of `prefix` bytes.
fn regex_selectivity(pattern: &[u8], case_insensitive: bool, prefix: usize) -> f64 {
    let length = pattern.len();
    let mut selectivity = if length > 0
        && pattern[length - 1] == b'$'
        && (length == 1 || pattern[length - 2] != b'\\')
    {
        regex_selectivity_sub(&pattern[..length - 1], case_insensitive)
    } else {
        regex_selectivity_sub(pattern, case_insensitive) * FULL_WILDCARD_SEL
    };
    if prefix > 0 {
        let prefix_selectivity = FIXED_CHAR_SEL.powi(prefix as i32);
        if prefix_selectivity > 0.0 {
            selectivity /= prefix_selectivity;
        }
    }
    clamp_probability(selectivity)
}

/// `regex_selectivity_sub`.
fn regex_selectivity_sub(pattern: &[u8], case_insensitive: bool) -> f64 {
    let mut selectivity = 1.0;
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut position = 0usize;
    while position < pattern.len() {
        match pattern[position] {
            b'(' => {
                if depth == 0 {
                    start = position;
                }
                depth += 1;
            }
            b')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    selectivity *=
                        regex_selectivity_sub(&pattern[start + 1..position], case_insensitive);
                }
            }
            b'|' if depth == 0 => {
                selectivity += regex_selectivity_sub(&pattern[position + 1..], case_insensitive);
                break;
            }
            b'[' => {
                let mut negated = false;
                position += 1;
                if pattern.get(position) == Some(&b'^') {
                    negated = true;
                    position += 1;
                }
                if pattern.get(position) == Some(&b']') {
                    position += 1;
                }
                while position < pattern.len() && pattern[position] != b']' {
                    position += 1;
                }
                if depth == 0 {
                    selectivity *= if negated {
                        1.0 - CHAR_RANGE_SEL
                    } else {
                        CHAR_RANGE_SEL
                    };
                }
            }
            b'.' => {
                if depth == 0 {
                    selectivity *= ANY_CHAR_SEL;
                }
            }
            b'*' | b'?' | b'+' => {
                if depth == 0 {
                    selectivity *= PARTIAL_WILDCARD_SEL;
                }
            }
            b'{' => {
                while position < pattern.len() && pattern[position] != b'}' {
                    position += 1;
                }
                if depth == 0 {
                    selectivity *= PARTIAL_WILDCARD_SEL;
                }
            }
            b'\\' => {
                position += 1;
                if position >= pattern.len() {
                    break;
                }
                if depth == 0 {
                    selectivity *= FIXED_CHAR_SEL;
                }
            }
            _ => {
                if depth == 0 {
                    selectivity *= FIXED_CHAR_SEL;
                }
            }
        }
        position += 1;
    }
    let _ = case_insensitive;
    selectivity.min(1.0)
}

/// `make_greater_string` of `prefix` under `C`: its last character
/// incremented (`pg_utf8_increment`), dropping trailing characters that
/// cannot be; `None` when none can.
pub(super) fn greater_string<'a>(
    prefix: &str,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let out = arena
        .alloc_slice_with(prefix.len(), |at| prefix.as_bytes()[at])
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut length = prefix.len();
    while length > 0 {
        let last = prefix[..length]
            .chars()
            .next_back()
            .expect("a nonempty prefix has a last character")
            .len_utf8();
        if utf8_increment(&mut out[length - last..length]) {
            return Ok(Some(core::str::from_utf8(&out[..length]).map_err(
                |_| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
                        "invalid byte sequence in a pattern's greater string"
                    )
                },
            )?));
        }
        length -= last;
    }
    Ok(None)
}

/// `pg_utf8_increment`.
fn utf8_increment(character: &mut [u8]) -> bool {
    let length = character.len();
    if length >= 4 && character[3] < 0xbf {
        character[3] += 1;
        return true;
    }
    if length >= 3 && character[2] < 0xbf {
        character[2] += 1;
        return true;
    }
    if length >= 2 {
        let limit = match character[0] {
            0xed => 0x9f,
            0xf4 => 0x8f,
            _ => 0xbf,
        };
        if character[1] < limit {
            character[1] += 1;
            return true;
        }
    }
    if length == 0 || length > 4 {
        return false;
    }
    let first = character[0];
    if first == 0x7f || first == 0xdf || first == 0xef || first == 0xf4 {
        return false;
    }
    character[0] += 1;
    true
}

impl Planner<'_, '_> {
    /// `patternsel_common` of `column LIKE pattern` (`NOT LIKE` when
    /// `negated`), or the shape outside the model.
    pub(super) fn like_selectivity(
        &self,
        column: usize,
        kind: PatternKind,
        pattern: &str,
        negated: bool,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        let variable = self.variable(column);
        let null_fraction = if variable.analyzed {
            variable.null_fraction
        } else {
            0.0
        };
        let fixed = match fixed_prefix(kind, pattern, self.arena)? {
            Ok(fixed) => fixed,
            Err(shape) => return Ok(Err(shape)),
        };
        let matches = |value: &Datum<'_>| -> Result<bool, SqlError> {
            match (value, kind) {
                (Datum::Text(text), PatternKind::Like | PatternKind::ILike) => {
                    crate::sql::eval::like_match(text, pattern, kind.case_insensitive(), Some('\\'))
                }
                (Datum::Text(text), PatternKind::Regex | PatternKind::IRegex) => {
                    crate::sql::regex::regex_search(pattern, text, kind.case_insensitive())
                }
                (Datum::Text(text), PatternKind::Prefix) => Ok(text.starts_with(pattern)),
                _ => Ok(false),
            }
        };
        let mut result = if fixed.exact {
            let prefix = fixed.prefix.unwrap_or("");
            match self.operator_selectivity(column, Comparison::Eq, Some(Datum::Text(prefix)))? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            }
        } else {
            // `histogram_selectivity`: the pattern applied to the
            // histogram's inner entries, when it has at least ten.
            let distribution = variable.distribution.filter(|_| variable.analyzed);
            let mut hist_size = 0usize;
            let mut selectivity = -1.0;
            if let Some(distribution) = distribution {
                hist_size = distribution.histogram_len();
                if hist_size >= 10 {
                    let mut matched = 0usize;
                    for index in 1..hist_size - 1 {
                        if matches(&distribution.histogram_value(index, variable.ctype)?)? {
                            matched += 1;
                        }
                    }
                    selectivity = matched as f64 / (hist_size - 2) as f64;
                }
            }
            if hist_size < 100 {
                let prefix_selectivity = match fixed.prefix {
                    Some(prefix) => match self.prefix_selectivity(&variable, column, prefix)? {
                        Ok(selectivity) => selectivity,
                        Err(shape) => return Ok(Err(shape)),
                    },
                    None => 1.0,
                };
                let heuristic = prefix_selectivity * fixed.rest;
                selectivity = if selectivity < 0.0 {
                    heuristic
                } else {
                    let weight = hist_size as f64 / 100.0;
                    selectivity * weight + heuristic * (1.0 - weight)
                };
            }
            selectivity = selectivity.clamp(0.0001, 0.9999);
            // `mcv_selectivity`: the pattern applied to each common value.
            let mut mcv_selectivity = 0.0;
            let mut common = 0.0;
            if let Some(distribution) = distribution {
                for index in 0..distribution.mcv_len() {
                    let frequency = f64::from(distribution.mcv_frequency(index));
                    if matches(&distribution.mcv_value(index, variable.ctype)?)? {
                        mcv_selectivity += frequency;
                    }
                    common += frequency;
                }
            }
            selectivity * (1.0 - null_fraction - common) + mcv_selectivity
        };
        if negated {
            result = 1.0 - result - null_fraction;
        }
        Ok(Ok(clamp_probability(result)))
    }

    /// `prefix_selectivity`: the histogram's share at or above `prefix` and
    /// below its greater string, at least the prefix's equality.
    fn prefix_selectivity(
        &self,
        variable: &super::VariableStatistics<'_>,
        column: usize,
        prefix: &str,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        let outside = "a LIKE prefix range PostgreSQL places otherwise";
        let at_least = match self.histogram_selectivity(
            variable,
            column,
            Comparison::Ge,
            Datum::Text(prefix),
        )? {
            Ok(Some(selectivity)) => selectivity,
            Ok(None) => return Ok(Ok(DEFAULT_MATCH_SEL)),
            Err(()) => return Ok(Err(outside)),
        };
        let mut selectivity = at_least;
        if let Some(greater) = greater_string(prefix, self.arena)? {
            let below = match self.histogram_selectivity(
                variable,
                column,
                Comparison::Lt,
                Datum::Text(greater),
            )? {
                Ok(Some(selectivity)) => selectivity,
                Ok(None) => return Ok(Ok(DEFAULT_MATCH_SEL)),
                Err(()) => return Ok(Err(outside)),
            };
            selectivity = below + at_least - 1.0;
        }
        let equal =
            match self.operator_selectivity(column, Comparison::Eq, Some(Datum::Text(prefix)))? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            };
        Ok(Ok(selectivity.max(equal)))
    }
}
