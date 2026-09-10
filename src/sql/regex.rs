//! A bounded PostgreSQL advanced-regular-expression matcher for the `~` /
//! `!~` / `~*` / `!~*` operators and the `regexp_*` function family.
//!
//! Implements PostgreSQL 18's BRE, ERE, literal, and ARE modes: newline and
//! expanded flags, bracket and shorthand classes, character-entry escapes,
//! greedy and non-greedy quantifiers, captures and backreferences, word and
//! absolute constraints, and lookaround.
//!
//! The matcher is a bounded backtracking recursion in continuation-passing
//! style (no allocation); a step budget guards against pathological blow-up.
//! Semantics are pinned to PostgreSQL's upstream regression source:
//! <https://github.com/postgres/postgres/blob/REL_18_STABLE/src/test/regress/sql/regex.sql>.

use crate::sql::eval::sqlstate;
use core::cell::Cell;
use core::fmt::Write as _;

use super::eval::SqlError;
use crate::sql_err;

/// PostgreSQL's selectable regular-expression grammar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegexFlavor {
    Advanced,
    Extended,
    Basic,
    Literal,
}

/// Compile-time matching behavior selected by PostgreSQL regex flags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegexOptions {
    pub case_insensitive: bool,
    pub flavor: RegexFlavor,
    pub expanded: bool,
    pub newline_stop: bool,
    pub newline_anchors: bool,
}

impl Default for RegexOptions {
    fn default() -> Self {
        Self {
            case_insensitive: false,
            flavor: RegexFlavor::Advanced,
            expanded: false,
            newline_stop: false,
            newline_anchors: false,
        }
    }
}

impl RegexOptions {
    pub fn case_insensitive(value: bool) -> Self {
        Self {
            case_insensitive: value,
            ..Self::default()
        }
    }

    pub fn apply_flag(&mut self, flag: char) -> Result<bool, SqlError> {
        match flag {
            'g' => return Ok(true),
            'b' => self.flavor = RegexFlavor::Basic,
            'c' => self.case_insensitive = false,
            'e' => self.flavor = RegexFlavor::Extended,
            'i' => self.case_insensitive = true,
            'm' | 'n' => {
                self.newline_stop = true;
                self.newline_anchors = true;
            }
            'p' => {
                self.newline_stop = true;
                self.newline_anchors = false;
            }
            'q' => self.flavor = RegexFlavor::Literal,
            's' => {
                self.newline_stop = false;
                self.newline_anchors = false;
            }
            't' => self.expanded = false,
            'w' => {
                self.newline_stop = false;
                self.newline_anchors = true;
            }
            'x' => self.expanded = true,
            _ => {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "invalid regular expression option: \"{}\"",
                    flag
                ));
            }
        }
        Ok(false)
    }
}

/// Whether `pattern` matches anywhere in `text` (POSIX `~` semantics).
pub fn regex_search(pattern: &str, text: &str, case_insensitive: bool) -> Result<bool, SqlError> {
    regex_search_with_options(
        pattern,
        text,
        RegexOptions::case_insensitive(case_insensitive),
    )
}

pub fn regex_search_with_options(
    pattern: &str,
    text: &str,
    options: RegexOptions,
) -> Result<bool, SqlError> {
    Ok(find_with_options(pattern, text, 0, options)?.is_some())
}

/// Advances a global search after a match without ever landing inside UTF-8.
/// A non-empty match resumes at its end; an empty match resumes after exactly
/// one character and can therefore still match once at end-of-string.
pub fn next_match_from(text: &str, start: usize, end: usize) -> Option<usize> {
    if end > start {
        return Some(end);
    }
    text.get(end..)?
        .chars()
        .next()
        .map(|ch| end + ch.len_utf8())
}

/// Finds the leftmost match at or after byte offset `from`, returning its
/// `(start, end)` byte range. At the leftmost matching position the match
/// length follows the RE's overall length preference: longest normally,
/// shortest when the pattern's first quantified atom is non-greedy (`*?` etc.),
/// per PostgreSQL's ARE rules. A `^`-anchored pattern matches only at the very
/// start of `text`. Used by `regexp_replace` and `regexp_matches`.
pub fn find(
    pattern: &str,
    text: &str,
    from: usize,
    case_insensitive: bool,
) -> Result<Option<(usize, usize)>, SqlError> {
    find_with_options(
        pattern,
        text,
        from,
        RegexOptions::case_insensitive(case_insensitive),
    )
}

pub fn find_with_options(
    pattern: &str,
    text: &str,
    from: usize,
    mut options: RegexOptions,
) -> Result<Option<(usize, usize)>, SqlError> {
    let mut prepared = crate::util::StackStr::<8192>::new();
    prepare_pattern(pattern, &mut options, &mut prepared)?;
    find_prepared(prepared.as_str(), text, from, options)
}

fn find_prepared(
    pattern: &str,
    text: &str,
    from: usize,
    options: RegexOptions,
) -> Result<Option<(usize, usize)>, SqlError> {
    if from > text.len() || !text.is_char_boundary(from) {
        return Ok(None);
    }
    validate(pattern)?;
    let leading_anchor = pattern.starts_with('^');
    // With backreferences, PostgreSQL chooses the earliest DFA candidate
    // before verifying captures. An anchored backreference alternative can
    // therefore prevent retrying an unanchored alternative at a later byte.
    let anchored = leading_anchor || has_anchored_backreference_alternative(pattern);
    let pat = if leading_anchor {
        &pattern[1..]
    } else {
        pattern
    };
    let prefer_longest = pattern_prefers_longest(pat);
    let budget = Cell::new(3_000_000u32);
    let mut starts = [0usize; MAX_GROUPS];
    let group_count = group_starts(pat, &mut starts);
    if group_count > MAX_GROUPS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many capture groups in regular expression"
        ));
    }
    // Best match anchored at `start`: a continuation that records the preferred
    // consumed length and always returns false forces the matcher to explore
    // every match length.
    let best_at = |start: usize| -> Result<Option<usize>, SqlError> {
        let sub = &text[start..];
        let best = Cell::new(None::<usize>);
        let spans: [Cell<(i64, i64)>; MAX_GROUPS] = core::array::from_fn(|_| Cell::new((-1, -1)));
        let recorder = Recorder {
            pat_base: pat.as_ptr() as usize,
            text_total: sub.len(),
            whole_text: text,
            candidate_start: start,
            group_starts: &starts[..group_count],
            spans: &spans[..group_count],
        };
        let accept = |rest: &str| {
            let consumed = sub.len() - rest.len();
            let better = best.get().is_none_or(|b| {
                if prefer_longest {
                    consumed > b
                } else {
                    consumed < b
                }
            });
            if better {
                best.set(Some(consumed));
            }
            Ok(false)
        };
        m(pat, sub, options, &budget, Some(&recorder), &accept)?;
        Ok(best.get())
    };
    if anchored {
        let mut start = 0usize;
        loop {
            if start >= from
                && (start == 0
                    || (options.newline_anchors
                        && text.as_bytes().get(start.wrapping_sub(1)) == Some(&b'\n')))
                && let Some(len) = best_at(start)?
            {
                return Ok(Some((start, start + len)));
            }
            if !options.newline_anchors {
                return Ok(None);
            }
            let Some(next_line) = text[start..].find('\n') else {
                return Ok(None);
            };
            start += next_line + 1;
            if start > text.len() {
                return Ok(None);
            }
        }
    }
    let mut start = from;
    loop {
        if let Some(len) = best_at(start)? {
            return Ok(Some((start, start + len)));
        }
        match text[start..].chars().next() {
            None => return Ok(None),
            Some(c) => start += c.len_utf8(),
        }
    }
}

fn has_anchored_backreference_alternative(pattern: &str) -> bool {
    let mut remaining = pattern;
    while let Some(bar) = top_level_bar(remaining) {
        let branch = &remaining[..bar];
        if branch.starts_with('^') && contains_backreference(branch) {
            return true;
        }
        remaining = &remaining[bar + 1..];
    }
    remaining.starts_with('^') && contains_backreference(remaining)
}

fn contains_backreference(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] == b'\\'
            && matches!(bytes[index + 1], b'1'..=b'9')
            && !bytes.get(index + 2).is_some_and(u8::is_ascii_digit)
        {
            return true;
        }
        if bytes[index] == b'[' {
            index = class_end(pattern, index)
                .map(|close| close + 1)
                .unwrap_or(bytes.len());
        } else if bytes[index] == b'\\' {
            index += take_atom(&pattern[index..]).0.len();
        } else {
            index += 1;
        }
    }
    false
}

fn prepare_pattern(
    pattern: &str,
    options: &mut RegexOptions,
    out: &mut crate::util::StackStr<8192>,
) -> Result<(), SqlError> {
    if options.flavor == RegexFlavor::Literal && options.expanded {
        return Err(sql_err!(
            sqlstate::INVALID_REGULAR_EXPRESSION,
            "invalid regular expression: invalid argument to regex function"
        ));
    }
    let mut source = pattern;
    if let Some(literal) = source.strip_prefix("***=") {
        options.flavor = RegexFlavor::Literal;
        options.expanded = false;
        source = literal;
    } else if let Some(advanced) = source.strip_prefix("***:") {
        options.flavor = RegexFlavor::Advanced;
        source = advanced;
    }
    // ARE embedded options are permitted only at the beginning. A leading
    // lookaround or noncapturing group is not an option declaration.
    if options.flavor == RegexFlavor::Advanced
        && let Some(rest) = source.strip_prefix("(?")
        && !matches!(rest.as_bytes().first(), Some(b':' | b'=' | b'!' | b'<'))
        && let Some(close) = rest.find(')')
    {
        for flag in rest[..close].chars() {
            let global = options.apply_flag(flag).map_err(|_| {
                sql_err!(
                    sqlstate::INVALID_REGULAR_EXPRESSION,
                    "invalid regular expression: invalid embedded option"
                )
            })?;
            if global {
                return Err(sql_err!(
                    sqlstate::INVALID_REGULAR_EXPRESSION,
                    "invalid regular expression: global option is not an embedded option"
                ));
            }
        }
        source = &rest[close + 1..];
    }

    let mut in_class = false;
    let mut escaped = false;
    let mut comment = false;
    for ch in source.chars() {
        if comment {
            if ch == '\n' {
                comment = false;
            }
            continue;
        }
        if options.expanded && !in_class && !escaped {
            if ch == '#' {
                comment = true;
                continue;
            }
            if ch.is_whitespace() {
                continue;
            }
        }
        if options.flavor == RegexFlavor::Literal {
            if "\\.^$|()[]*+?{}".contains(ch) {
                let _ = out.write_char('\\');
            }
            let _ = out.write_char(ch);
            continue;
        }
        if matches!(options.flavor, RegexFlavor::Basic | RegexFlavor::Extended) {
            if escaped {
                if ch.is_ascii_alphabetic()
                    || ch == '0'
                    || in_class && ch.is_ascii_digit()
                    || options.flavor == RegexFlavor::Basic
                        && !in_class
                        && matches!(ch, '(' | ')' | '{' | '}')
                {
                    let _ = out.write_char(ch);
                } else {
                    let _ = out.write_char('\\');
                    let _ = out.write_char(ch);
                }
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if !in_class && matches!(ch, '(' | ')' | '|' | '+' | '?' | '{' | '}') {
                let _ = out.write_char('\\');
            }
            let _ = out.write_char(ch);
            if ch == '[' {
                in_class = true;
            } else if ch == ']' {
                in_class = false;
            }
            continue;
        }
        let _ = out.write_char(ch);
        if ch == '\\' && !escaped {
            escaped = true;
        } else {
            if ch == '[' && !escaped {
                in_class = true;
            } else if ch == ']' && !escaped {
                in_class = false;
            }
            escaped = false;
        }
    }
    if escaped {
        let _ = out.write_char('\\');
    }
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "regular expression is too large"
        ));
    }
    Ok(())
}

/// Maximum capture groups tracked for `regexp_matches`.
pub const MAX_GROUPS: usize = 16;

/// A whole-match byte span `(start, end)` plus the number of capturing groups.
pub type MatchSpan = ((usize, usize), usize);

/// Records the byte span each capturing group matched, keyed by the byte offset
/// of its opening `(` in the (leading-`^`-stripped) pattern.
struct Recorder<'a> {
    /// `pat.as_ptr() as usize` for the stripped pattern, so a sub-slice's group
    /// index can be recovered from its address.
    pat_base: usize,
    /// Length of the suffix being matched, for computing consumed byte offsets.
    text_total: usize,
    whole_text: &'a str,
    candidate_start: usize,
    /// Opening-paren byte offsets, in group order (index + 1 = group number).
    group_starts: &'a [usize],
    /// Per-group `(start, end)` spans relative to the matched suffix; `(-1, -1)`
    /// until the group participates.
    spans: &'a [Cell<(i64, i64)>],
}

impl Recorder<'_> {
    fn snapshot(&self) -> [(i64, i64); MAX_GROUPS] {
        core::array::from_fn(|index| {
            self.spans
                .get(index)
                .map_or((-1, -1), core::cell::Cell::get)
        })
    }

    fn restore(&self, snapshot: &[(i64, i64); MAX_GROUPS]) {
        for (span, saved) in self.spans.iter().zip(snapshot) {
            span.set(*saved);
        }
    }

    /// The 1-based group number of the group whose `(` begins `pat`.
    fn group_of(&self, pat: &str) -> Option<usize> {
        let offset = (pat.as_ptr() as usize).checked_sub(self.pat_base)?;
        self.group_starts
            .iter()
            .position(|&s| s == offset)
            .map(|i| i + 1)
    }
    /// Byte offset consumed so far, given the remaining suffix length.
    fn consumed(&self, remaining_len: usize) -> i64 {
        (self.text_total - remaining_len) as i64
    }
    fn record(&self, group: usize, start: i64, end: i64) {
        if group >= 1 && group <= self.spans.len() {
            self.spans[group - 1].set((start, end));
        }
    }

    fn group_span(&self, group: usize) -> Option<(usize, usize)> {
        let (start, end) = self.spans.get(group.checked_sub(1)?)?.get();
        (start >= 0).then(|| {
            (
                self.candidate_start + start as usize,
                self.candidate_start + end as usize,
            )
        })
    }

    fn absolute_offset(&self, remaining_len: usize) -> usize {
        self.candidate_start + self.consumed(remaining_len) as usize
    }
}

/// Byte offsets of each capturing `(` in `pat` (escapes and `[...]` skipped),
/// in group order. Returns the group count.
fn group_starts(pat: &str, out: &mut [usize; MAX_GROUPS]) -> usize {
    let b = pat.as_bytes();
    let mut i = 0;
    let mut n = 0;
    let mut depth = 0usize;
    let mut constraint_depth = 0usize;
    let mut constraint_groups = [false; 256];
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                i = class_end(pat, i).unwrap_or(b.len().saturating_sub(1));
            }
            b'(' => {
                let kind = group_kind(&pat[i..])
                    .expect("validated regular-expression group")
                    .0;
                if kind == GroupKind::Capture && constraint_depth == 0 {
                    if n < out.len() {
                        out[n] = i;
                    }
                    n += 1;
                }
                let constraint = kind.is_constraint();
                constraint_groups[depth] = constraint;
                depth += 1;
                constraint_depth += usize::from(constraint);
            }
            b')' => {
                depth -= 1;
                constraint_depth -= usize::from(constraint_groups[depth]);
            }
            _ => {}
        }
        i += 1;
    }
    n
}

/// Finds the leftmost-longest match at or after `from` and records each
/// capturing group's byte span into `spans_out` (absolute offsets into `text`;
/// `(-1, -1)` for a group that did not participate). Returns the whole match's
/// `(start, end)` and the number of capturing groups, or `None` for no match.
pub fn find_captures(
    pattern: &str,
    text: &str,
    from: usize,
    case_insensitive: bool,
    spans_out: &mut [(i64, i64); MAX_GROUPS],
) -> Result<Option<MatchSpan>, SqlError> {
    find_captures_with_options(
        pattern,
        text,
        from,
        RegexOptions::case_insensitive(case_insensitive),
        spans_out,
    )
}

pub fn find_captures_with_options(
    pattern: &str,
    text: &str,
    from: usize,
    mut options: RegexOptions,
    spans_out: &mut [(i64, i64); MAX_GROUPS],
) -> Result<Option<MatchSpan>, SqlError> {
    let mut prepared = crate::util::StackStr::<8192>::new();
    prepare_pattern(pattern, &mut options, &mut prepared)?;
    let pattern = prepared.as_str();
    validate(pattern)?;
    let anchored = pattern.starts_with('^');
    let pat = if anchored { &pattern[1..] } else { pattern };
    let mut starts = [0usize; MAX_GROUPS];
    let ng = group_starts(pat, &mut starts);
    if ng > MAX_GROUPS {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "too many capture groups in regular expression"
        ));
    }
    // Locate the leftmost-longest whole match first (POSIX semantics).
    let Some((mstart, mend)) = find_prepared(pattern, text, from, options)? else {
        return Ok(None);
    };
    // Re-match anchored at the match start, recording group spans on the first
    // greedy path that consumes exactly the whole match.
    let sub = &text[mstart..];
    let target = mend - mstart;
    let spans: [Cell<(i64, i64)>; MAX_GROUPS] = core::array::from_fn(|_| Cell::new((-1, -1)));
    let budget = Cell::new(3_000_000u32);
    let recorder = Recorder {
        pat_base: pat.as_ptr() as usize,
        text_total: sub.len(),
        whole_text: text,
        candidate_start: mstart,
        group_starts: &starts[..ng],
        spans: &spans[..ng],
    };
    let accept = |rest: &str| Ok(sub.len() - rest.len() == target);
    m(pat, sub, options, &budget, Some(&recorder), &accept)?;
    for (i, span) in spans[..ng].iter().enumerate() {
        let (a, b) = span.get();
        spans_out[i] = if a < 0 {
            (-1, -1)
        } else {
            (a + mstart as i64, b + mstart as i64)
        };
    }
    Ok(Some(((mstart, mend), ng)))
}

/// Rejects unsupported constructs so a pattern is never matched incorrectly.
fn validate(pattern: &str) -> Result<(), SqlError> {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    let mut depth = 0usize;
    let mut constraint_depth = 0usize;
    let mut constraint_groups = [false; 256];
    let mut captures = 0usize;
    let mut can_quantify = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let Some(&escaped) = bytes.get(i + 1) else {
                    return Err(invalid_escape());
                };
                if escaped.is_ascii_alphabetic()
                    && !matches!(
                        escaped,
                        b'A' | b'Z'
                            | b'm'
                            | b'M'
                            | b'y'
                            | b'Y'
                            | b'd'
                            | b'D'
                            | b'w'
                            | b'W'
                            | b's'
                            | b'S'
                            | b'a'
                            | b'b'
                            | b'B'
                            | b'c'
                            | b'e'
                            | b'f'
                            | b'n'
                            | b'r'
                            | b't'
                            | b'v'
                            | b'u'
                            | b'U'
                            | b'x'
                    )
                {
                    return Err(invalid_escape());
                }
                let atom = take_atom(&pattern[i..]).0;
                if escaped == b'c' && atom.chars().count() != 3 {
                    return Err(invalid_escape());
                }
                if escaped.is_ascii_digit() {
                    let numeric = &atom[1..];
                    if numeric.len() == 1 && escaped != b'0' {
                        if constraint_depth != 0 {
                            return Err(sql_err!(
                                sqlstate::INVALID_REGULAR_EXPRESSION,
                                "invalid regular expression: backreference in lookaround constraint"
                            ));
                        }
                        let group = usize::from(escaped - b'0');
                        if group > captures {
                            return Err(sql_err!(
                                sqlstate::INVALID_REGULAR_EXPRESSION,
                                "invalid regular expression: invalid backreference number"
                            ));
                        }
                    } else if decode_octal_escape(atom).is_none() {
                        return Err(invalid_escape());
                    }
                }
                if matches!(escaped, b'u' | b'U')
                    && ((escaped == b'u' && atom.len() != 6)
                        || (escaped == b'U' && atom.len() != 10)
                        || decode_character_escape(atom).is_none())
                {
                    return Err(invalid_escape());
                }
                if escaped == b'x' && (atom.len() == 2 || decode_character_escape(atom).is_none()) {
                    return Err(invalid_escape());
                }
                can_quantify = !matches!(escaped, b'A' | b'Z' | b'm' | b'M' | b'y' | b'Y')
                    && !matches!(escaped, b'<' | b'>');
                i += atom.len() - 1;
            }
            b'[' => {
                let Some(close) = class_end(pattern, i) else {
                    return Err(sql_err!(
                        sqlstate::INVALID_REGULAR_EXPRESSION,
                        "invalid regular expression: unbalanced ["
                    ));
                };
                validate_class(&pattern[i..=close])?;
                can_quantify = true;
                i = close;
            }
            b'(' => {
                if depth == constraint_groups.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "regular expression nesting is too deep"
                    ));
                }
                let (kind, body_start) = group_kind(&pattern[i..])?;
                if kind == GroupKind::Capture && constraint_depth == 0 {
                    captures += 1;
                }
                let constraint = kind.is_constraint();
                constraint_groups[depth] = constraint;
                depth += 1;
                constraint_depth += usize::from(constraint);
                can_quantify = false;
                i += body_start - 1;
            }
            b')' => {
                if depth == 0 {
                    return Err(sql_err!(
                        sqlstate::INVALID_REGULAR_EXPRESSION,
                        "invalid regular expression: unbalanced ("
                    ));
                }
                depth -= 1;
                let constraint = constraint_groups[depth];
                constraint_depth -= usize::from(constraint);
                can_quantify = !constraint;
            }
            b'*' | b'+' | b'?' => {
                if !can_quantify {
                    return Err(invalid_quantifier_operand());
                }
                can_quantify = false;
                if bytes.get(i + 1) == Some(&b'?') {
                    i += 1;
                }
            }
            b'{' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                // Validate the bound's shape here so a malformed one errors up
                // front (parse_bound re-parses it during matching). A `{` not
                // followed by a digit is a literal character.
                if !can_quantify {
                    return Err(invalid_quantifier_operand());
                }
                let (_, _, used) = parse_bound(&pattern[i..])?;
                i += used - 1;
                can_quantify = false;
                if bytes.get(i + 1) == Some(&b'?') {
                    i += 1;
                }
            }
            b'|' | b'^' | b'$' => can_quantify = false,
            _ => can_quantify = true,
        }
        i += 1;
    }
    if depth != 0 {
        return Err(sql_err!(
            sqlstate::INVALID_REGULAR_EXPRESSION,
            "invalid regular expression: unbalanced ("
        ));
    }
    Ok(())
}

fn invalid_quantifier_operand() -> SqlError {
    sql_err!(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        "invalid regular expression: quantifier operand invalid"
    )
}

fn invalid_escape() -> SqlError {
    sql_err!(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        "invalid regular expression: invalid escape \\ sequence"
    )
}

fn validate_class(class: &str) -> Result<(), SqlError> {
    let inner = &class[1..class.len() - 1];
    let mut offset = usize::from(inner.starts_with('^'));
    while offset < inner.len() {
        let rest = &inner[offset..];
        if let Some(named) = rest.strip_prefix("[:") {
            let Some(end) = named.find(":]") else {
                return Err(invalid_character_class());
            };
            if !is_named_class(&named[..end]) {
                return Err(invalid_character_class());
            }
        }
        if rest.starts_with("[.") || rest.starts_with("[=") {
            let marker = rest.as_bytes()[1] as char;
            let closing = if marker == '.' { ".]" } else { "=]" };
            let Some(end) = rest[2..].find(closing) else {
                return Err(invalid_character_class());
            };
            if rest[2..2 + end].chars().count() != 1 {
                return Err(invalid_character_class());
            }
        }
        if rest.starts_with('\\') {
            let (atom, _) = take_atom(rest);
            let escaped = atom.as_bytes().get(1).copied().ok_or_else(invalid_escape)?;
            if escaped.is_ascii_alphabetic()
                && !matches!(
                    escaped,
                    b'a' | b'b'
                        | b'B'
                        | b'c'
                        | b'd'
                        | b'D'
                        | b'e'
                        | b'f'
                        | b'n'
                        | b'r'
                        | b's'
                        | b'S'
                        | b't'
                        | b'u'
                        | b'U'
                        | b'v'
                        | b'w'
                        | b'W'
                        | b'x'
                )
            {
                return Err(invalid_escape());
            }
            if escaped == b'c' && decode_control_escape(atom).is_none()
                || matches!(escaped, b'u' | b'U' | b'x') && decode_character_escape(atom).is_none()
                || escaped.is_ascii_digit() && decode_octal_escape(atom).is_none()
            {
                return Err(invalid_escape());
            }
        }
        let Some((first, next)) = class_token(inner, offset) else {
            return Err(invalid_character_class());
        };
        if inner.as_bytes().get(next) == Some(&b'-') && next + 1 < inner.len() {
            let Some((second, after)) = class_token(inner, next + 1) else {
                return Err(invalid_character_range());
            };
            match (first, second) {
                (ClassToken::Character(low), ClassToken::Character(high)) if low <= high => {
                    offset = after;
                }
                _ => return Err(invalid_character_range()),
            }
        } else {
            offset = next;
        }
    }
    Ok(())
}

fn invalid_character_range() -> SqlError {
    sql_err!(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        "invalid regular expression: invalid character range"
    )
}

fn invalid_character_class() -> SqlError {
    sql_err!(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        "invalid regular expression: invalid character class"
    )
}

fn step(budget: &Cell<u32>) -> Result<(), SqlError> {
    let b = budget.get();
    if b == 0 {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "regular expression is too complex"
        ));
    }
    budget.set(b - 1);
    Ok(())
}

/// Matches `pat` against a prefix of `text`; on success calls `k` with the
/// remaining text.
fn m(
    pat: &str,
    text: &str,
    options: RegexOptions,
    budget: &Cell<u32>,
    rec: Option<&Recorder>,
    k: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    let snapshot = rec.map(Recorder::snapshot);
    let result = m_inner(pat, text, options, budget, rec, k);
    if !matches!(result, Ok(true))
        && let (Some(recorder), Some(saved)) = (rec, snapshot.as_ref())
    {
        recorder.restore(saved);
    }
    result
}

fn m_inner(
    pat: &str,
    text: &str,
    options: RegexOptions,
    budget: &Cell<u32>,
    rec: Option<&Recorder>,
    k: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    // Top-level alternation: try each branch.
    if let Some(bar) = top_level_bar(pat) {
        if m(&pat[..bar], text, options, budget, rec, k)? {
            return Ok(true);
        }
        return m(&pat[bar + 1..], text, options, budget, rec, k);
    }
    if pat.is_empty() {
        return k(text);
    }
    if let Some(rest) = pat.strip_prefix('^') {
        let Some(recorder) = rec else {
            return Ok(false);
        };
        let offset = recorder.absolute_offset(text.len());
        let at_anchor = offset == 0
            || (options.newline_anchors
                && recorder.whole_text.as_bytes().get(offset.wrapping_sub(1)) == Some(&b'\n'));
        return if at_anchor {
            m(rest, text, options, budget, rec, k)
        } else {
            Ok(false)
        };
    }
    // End anchor (zero-width).
    if pat.as_bytes()[0] == b'$' {
        return if text.is_empty() || (options.newline_anchors && text.starts_with('\n')) {
            m(&pat[1..], text, options, budget, rec, k)
        } else {
            Ok(false)
        };
    }
    if pat.starts_with('\\')
        && let Some(escaped) = pat[1..].chars().next()
    {
        if let Some(group) = escaped.to_digit(10).filter(|group| *group != 0)
            && !pat.as_bytes().get(2).is_some_and(u8::is_ascii_digit)
        {
            let Some(recorder) = rec else {
                return Ok(false);
            };
            let Some((start, end)) = recorder.group_span(group as usize) else {
                return Ok(false);
            };
            let captured = &recorder.whole_text[start..end];
            let after_escape = &pat[1 + escaped.len_utf8()..];
            let (quantifier, rest) = parse_quant(after_escape)?;
            let continuation = |remaining: &str| m(rest, remaining, options, budget, rec, k);
            return match quantifier {
                Some(quantifier) => {
                    rep_text(captured, text, options, budget, quantifier, &continuation)
                }
                None => match_prefix(captured, text, options).map_or(Ok(false), continuation),
            };
        }
        if matches!(escaped, 'A' | 'Z' | 'm' | 'M' | 'y' | 'Y' | '<' | '>') {
            let Some(recorder) = rec else {
                return Ok(false);
            };
            let offset = recorder.absolute_offset(text.len());
            let previous = recorder.whole_text[..offset].chars().next_back();
            let next = recorder.whole_text[offset..].chars().next();
            let previous_word = previous.is_some_and(is_word_character);
            let next_word = next.is_some_and(is_word_character);
            let matches = match escaped {
                'A' => offset == 0,
                'Z' => offset == recorder.whole_text.len(),
                'm' | '<' => !previous_word && next_word,
                'M' | '>' => previous_word && !next_word,
                'y' => previous_word != next_word,
                'Y' => previous_word == next_word,
                _ => unreachable!(),
            };
            return if matches {
                m(
                    &pat[1 + escaped.len_utf8()..],
                    text,
                    options,
                    budget,
                    rec,
                    k,
                )
            } else {
                Ok(false)
            };
        }
    }
    // Group.
    if pat.as_bytes()[0] == b'(' {
        let close = matching_paren(pat);
        let (kind, body_start) = group_kind(pat)?;
        let body = &pat[body_start..close];
        let after = &pat[close + 1..];
        let (quant, rest) = parse_quant(after)?;
        if kind.is_constraint() {
            if quant.is_some() {
                return Err(sql_err!(
                    sqlstate::INVALID_REGULAR_EXPRESSION,
                    "invalid regular expression: quantifier operand invalid"
                ));
            }
            let matched = match kind {
                GroupKind::Ahead | GroupKind::NotAhead => {
                    let accept = |_remaining: &str| Ok(true);
                    m(body, text, options, budget, rec, &accept)?
                }
                GroupKind::Behind | GroupKind::NotBehind => {
                    lookbehind_matches(body, text, options, budget, rec)?
                }
                _ => unreachable!(),
            };
            let wanted = matches!(kind, GroupKind::Ahead | GroupKind::Behind);
            return if matched == wanted {
                m(rest, text, options, budget, rec, k)
            } else {
                Ok(false)
            };
        }
        let group = rec.and_then(|r| r.group_of(pat));
        let entry_len = text.len();
        // Continuation after a single, non-repeated group match: record the
        // group's span, then match the remainder.
        let cont = move |t: &str| {
            if let (Some(r), Some(g)) = (rec, group) {
                r.record(g, r.consumed(entry_len), r.consumed(t.len()));
            }
            m(rest, t, options, budget, rec, k)
        };
        return match quant {
            None => m(body, text, options, budget, rec, &cont),
            Some(q) => {
                // The repetition records each iteration (last wins); the
                // downstream continuation does not re-record the whole span.
                // POSIX subexpression preference comes from the outer
                // quantifier, even when an inner quantifier spells the
                // opposite preference.
                let mut preferred = crate::util::StackStr::<8192>::new();
                let repeated_body = if contains_group(body) {
                    body
                } else {
                    force_quantifier_preference(body, q.greedy, &mut preferred)?;
                    preferred.as_str()
                };
                let after_reps = move |t: &str| m(rest, t, options, budget, rec, k);
                rep_group(
                    repeated_body,
                    text,
                    options,
                    budget,
                    q,
                    rec,
                    group,
                    &after_reps,
                )
            }
        };
    }
    // A single atom (literal / '.' / escaped / class) plus optional quantifier.
    let (atom, after) = take_atom(pat);
    let (quant, rest) = parse_quant(after)?;
    let cont = move |t: &str| m(rest, t, options, budget, rec, k);
    match quant {
        Some(q) => rep_atom(atom, text, options, budget, q, &cont),
        None => {
            if let Some(c) = text.chars().next()
                && atom_matches(atom, c, options)
            {
                cont(&text[c.len_utf8()..])
            } else {
                Ok(false)
            }
        }
    }
}

fn contains_group(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += take_atom(&pattern[index..]).0.len(),
            b'[' => {
                index = class_end(pattern, index)
                    .map(|close| close + 1)
                    .unwrap_or(bytes.len());
            }
            b'(' => return true,
            _ => {
                index += pattern[index..]
                    .chars()
                    .next()
                    .expect("nonempty suffix")
                    .len_utf8();
            }
        }
    }
    false
}

fn force_quantifier_preference(
    pattern: &str,
    greedy: bool,
    out: &mut crate::util::StackStr<8192>,
) -> Result<(), SqlError> {
    use core::fmt::Write as _;

    let bytes = pattern.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let (atom, _) = take_atom(&pattern[index..]);
            let _ = out.write_str(atom);
            index += atom.len();
            continue;
        }
        if bytes[index] == b'[' {
            let close = class_end(pattern, index).unwrap_or(bytes.len() - 1);
            let _ = out.write_str(&pattern[index..=close]);
            index = close + 1;
            continue;
        }
        let simple = matches!(bytes[index], b'*' | b'+')
            || bytes[index] == b'?' && index != 0 && bytes[index - 1] != b'(';
        let bounded = bytes[index] == b'{' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit);
        if simple || bounded {
            let end = if bounded {
                pattern[index..]
                    .find('}')
                    .map(|offset| index + offset + 1)
                    .unwrap_or(index + 1)
            } else {
                index + 1
            };
            let _ = out.write_str(&pattern[index..end]);
            index = end;
            if bytes.get(index) == Some(&b'?') {
                index += 1;
            }
            if !greedy {
                let _ = out.write_char('?');
            }
            continue;
        }
        let character = pattern[index..].chars().next().expect("nonempty suffix");
        let _ = out.write_char(character);
        index += character.len_utf8();
    }
    if out.is_truncated() {
        Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "regular expression is too large"
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum GroupKind {
    Capture,
    NonCapture,
    Ahead,
    NotAhead,
    Behind,
    NotBehind,
}

impl GroupKind {
    fn is_constraint(self) -> bool {
        !matches!(self, Self::Capture | Self::NonCapture)
    }
}

fn group_kind(pattern: &str) -> Result<(GroupKind, usize), SqlError> {
    let bytes = pattern.as_bytes();
    let result = match bytes.get(1..4) {
        Some([b'?', b'<', b'=']) => (GroupKind::Behind, 4),
        Some([b'?', b'<', b'!']) => (GroupKind::NotBehind, 4),
        _ => match bytes.get(1..3) {
            Some([b'?', b':']) => (GroupKind::NonCapture, 3),
            Some([b'?', b'=']) => (GroupKind::Ahead, 3),
            Some([b'?', b'!']) => (GroupKind::NotAhead, 3),
            Some([b'?', _]) => {
                return Err(sql_err!(
                    sqlstate::INVALID_REGULAR_EXPRESSION,
                    "invalid regular expression: invalid embedded option"
                ));
            }
            _ => (GroupKind::Capture, 1),
        },
    };
    Ok(result)
}

fn is_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn match_prefix<'a>(wanted: &str, text: &'a str, options: RegexOptions) -> Option<&'a str> {
    if !options.case_insensitive {
        return text.strip_prefix(wanted);
    }
    let mut consumed = 0usize;
    let mut actual = text.chars();
    for expected in wanted.chars() {
        let found = actual.next()?;
        if !eq_ci(expected, found, options) {
            return None;
        }
        consumed += found.len_utf8();
    }
    Some(&text[consumed..])
}

fn lookbehind_matches(
    body: &str,
    remaining: &str,
    options: RegexOptions,
    budget: &Cell<u32>,
    recorder: Option<&Recorder>,
) -> Result<bool, SqlError> {
    let Some(recorder) = recorder else {
        return Ok(false);
    };
    let end = recorder.absolute_offset(remaining.len());
    let prefix = &recorder.whole_text[..end];
    for (start, _) in prefix
        .char_indices()
        .chain(core::iter::once((prefix.len(), '\0')))
    {
        let candidate = &prefix[start..];
        let group_starts = [];
        let spans = [];
        let constraint_recorder = Recorder {
            pat_base: body.as_ptr() as usize,
            text_total: candidate.len(),
            whole_text: recorder.whole_text,
            candidate_start: start,
            group_starts: &group_starts,
            spans: &spans,
        };
        let accept = |rest: &str| Ok(rest.is_empty());
        if m(
            body,
            candidate,
            options,
            budget,
            Some(&constraint_recorder),
            &accept,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn rep_text(
    wanted: &str,
    text: &str,
    options: RegexOptions,
    budget: &Cell<u32>,
    quantifier: Quant,
    continuation: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    let consume = || -> Result<bool, SqlError> {
        if quantifier.max == 0 || wanted.is_empty() {
            return Ok(false);
        }
        let Some(remaining) = match_prefix(wanted, text, options) else {
            return Ok(false);
        };
        rep_text(
            wanted,
            remaining,
            options,
            budget,
            quantifier.step_down(),
            continuation,
        )
    };
    if quantifier.greedy {
        if consume()? {
            return Ok(true);
        }
        if quantifier.min == 0 {
            continuation(text)
        } else {
            Ok(false)
        }
    } else {
        if quantifier.min == 0 && continuation(text)? {
            return Ok(true);
        }
        consume()
    }
}

/// Repetition of a single `atom` within `[q.min, q.max]` occurrences, then
/// `cont`. Greedy prefers more occurrences; non-greedy prefers fewer.
fn rep_atom(
    atom: &str,
    text: &str,
    options: RegexOptions,
    budget: &Cell<u32>,
    q: Quant,
    cont: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    let consume_one = || -> Result<bool, SqlError> {
        if q.max == 0 {
            return Ok(false);
        }
        if let Some(c) = text.chars().next()
            && atom_matches(atom, c, options)
        {
            rep_atom(
                atom,
                &text[c.len_utf8()..],
                options,
                budget,
                q.step_down(),
                cont,
            )
        } else {
            Ok(false)
        }
    };
    if q.greedy {
        if consume_one()? {
            return Ok(true);
        }
        if q.min == 0 { cont(text) } else { Ok(false) }
    } else {
        if q.min == 0 && cont(text)? {
            return Ok(true);
        }
        consume_one()
    }
}

/// Repetition of a group `body` within `[q.min, q.max]` occurrences, then
/// `cont`. Each iteration records the group's span (the last iteration wins,
/// matching PostgreSQL/POSIX capture semantics for a repeated group).
#[expect(
    clippy::too_many_arguments,
    reason = "capture recording threads context"
)]
fn rep_group(
    body: &str,
    text: &str,
    options: RegexOptions,
    budget: &Cell<u32>,
    q: Quant,
    rec: Option<&Recorder>,
    group: Option<usize>,
    cont: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    let start_len = text.len();
    let one_iteration = || -> Result<bool, SqlError> {
        if q.max == 0 {
            return Ok(false);
        }
        let more = |t: &str| {
            if t.len() == start_len {
                // One empty iteration satisfies every remaining lower bound;
                // continuing instead of recursing prevents an unbounded empty
                // group from looping while preserving its capture.
                if let (Some(r), Some(g)) = (rec, group) {
                    r.record(g, r.consumed(start_len), r.consumed(t.len()));
                }
                return cont(t);
            }
            if let (Some(r), Some(g)) = (rec, group) {
                r.record(g, r.consumed(start_len), r.consumed(t.len()));
            }
            rep_group(body, t, options, budget, q.step_down(), rec, group, cont)
        };
        m(body, text, options, budget, rec, &more)
    };
    if q.greedy {
        if one_iteration()? {
            return Ok(true);
        }
        if q.min == 0 { cont(text) } else { Ok(false) }
    } else {
        if q.min == 0 && cont(text)? {
            return Ok(true);
        }
        one_iteration()
    }
}

/// Index of the top-level `|`, or None. Respects group and class nesting.
fn top_level_bar(pat: &str) -> Option<usize> {
    let b = pat.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                i = class_end(pat, i).unwrap_or(b.len().saturating_sub(1));
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            b'|' if depth == 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Index of the `)` matching the `(` at position 0.
fn matching_paren(pat: &str) -> usize {
    let b = pat.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                i = class_end(pat, i).unwrap_or(b.len().saturating_sub(1));
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    b.len() // validate() guarantees balance, so this is unreachable
}

/// A parsed quantifier: occurrence bounds plus the length preference.
/// `max == u32::MAX` means unbounded.
#[derive(Clone, Copy)]
struct Quant {
    min: u32,
    max: u32,
    greedy: bool,
}

impl Quant {
    /// The bounds after consuming one occurrence.
    fn step_down(self) -> Quant {
        Quant {
            min: self.min.saturating_sub(1),
            max: if self.max == u32::MAX {
                u32::MAX
            } else {
                self.max - 1
            },
            greedy: self.greedy,
        }
    }
}

/// PostgreSQL caps `{m,n}` bounds at 255 (DUPMAX).
const MAX_REPEAT: u32 = 255;

/// Parses an optional quantifier at the front of `pat`: `*` `+` `?` or a bound
/// `{m}` / `{m,}` / `{m,n}`, each optionally followed by `?` for non-greedy
/// (shortest-preference), per PostgreSQL's ARE syntax.
fn parse_quant(pat: &str) -> Result<(Option<Quant>, &str), SqlError> {
    let b = pat.as_bytes();
    let (mut q, mut used) = match b.first() {
        Some(b'*') => (
            Quant {
                min: 0,
                max: u32::MAX,
                greedy: true,
            },
            1,
        ),
        Some(b'+') => (
            Quant {
                min: 1,
                max: u32::MAX,
                greedy: true,
            },
            1,
        ),
        Some(b'?') => (
            Quant {
                min: 0,
                max: 1,
                greedy: true,
            },
            1,
        ),
        // `{` opens a bound only when followed by a digit (PostgreSQL treats a
        // bare `{` as a literal character).
        Some(b'{') if b.get(1).is_some_and(u8::is_ascii_digit) => {
            let (min, max, after) = parse_bound(pat)?;
            (
                Quant {
                    min,
                    max,
                    greedy: true,
                },
                after,
            )
        }
        _ => return Ok((None, pat)),
    };
    if b.get(used) == Some(&b'?') {
        q.greedy = false;
        used += 1;
    }
    Ok((Some(q), &pat[used..]))
}

/// Parses `{m}` / `{m,}` / `{m,n}` starting at `pat[0] == '{'`; returns
/// `(min, max, bytes_used)`. Bounds are validated exactly as PostgreSQL does:
/// integers, `m <= n`, both at most 255.
fn parse_bound(pat: &str) -> Result<(u32, u32, usize), SqlError> {
    let bad = || {
        sql_err!(
            sqlstate::INVALID_REGULAR_EXPRESSION,
            "invalid regular expression: invalid repetition count(s)"
        )
    };
    let b = pat.as_bytes();
    let mut i = 1;
    let read_int = |i: &mut usize| -> Option<u32> {
        let start = *i;
        while *i < b.len() && b[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start {
            return None;
        }
        pat[start..*i].parse::<u32>().ok()
    };
    let min = read_int(&mut i).ok_or_else(bad)?;
    let max = match b.get(i) {
        Some(b'}') => min,
        Some(b',') => {
            i += 1;
            if b.get(i) == Some(&b'}') {
                u32::MAX
            } else {
                read_int(&mut i).ok_or_else(bad)?
            }
        }
        _ => return Err(bad()),
    };
    if b.get(i) != Some(&b'}') {
        return Err(bad());
    }
    i += 1;
    if min > MAX_REPEAT || (max != u32::MAX && (max > MAX_REPEAT || min > max)) {
        return Err(bad());
    }
    Ok((min, max, i))
}

/// The length preference of the RE as a whole: per PostgreSQL's ARE rules, the
/// whole match prefers the length preference of the first quantified atom in
/// the pattern (greedy when there is none).
fn pattern_prefers_longest(pat: &str) -> bool {
    let b = pat.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                i = class_end(pat, i).unwrap_or(b.len().saturating_sub(1));
            }
            b'*' | b'+' | b'?' => {
                return b.get(i + 1) != Some(&b'?');
            }
            b'{' if b.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                while i < b.len() && b[i] != b'}' {
                    i += 1;
                }
                return b.get(i + 1) != Some(&b'?');
            }
            _ => {}
        }
        i += 1;
    }
    true
}

/// Splits off the first atom of `pat`: a single char, `.`, an escaped char, or
/// a `[...]` class.
fn take_atom(pat: &str) -> (&str, &str) {
    let b = pat.as_bytes();
    match b.first() {
        Some(b'\\') => {
            let escaped = pat[1..].chars().next();
            let mut end = 1 + escaped.map_or(0, char::len_utf8);
            match escaped {
                Some('c') => {
                    if let Some(control) = pat[end..].chars().next() {
                        end += control.len_utf8();
                    }
                }
                Some('u') => end = (end + 4).min(pat.len()),
                Some('U') => end = (end + 8).min(pat.len()),
                Some('x') => {
                    while end < pat.len() && pat.as_bytes()[end].is_ascii_hexdigit() {
                        end += 1;
                    }
                }
                Some('0'..='7') => {
                    while end < pat.len() && end < 4 && matches!(pat.as_bytes()[end], b'0'..=b'7') {
                        end += 1;
                    }
                }
                _ => {}
            }
            (&pat[..end], &pat[end..])
        }
        Some(b'[') => {
            let end = class_end(pat, 0).map_or(b.len(), |close| close + 1);
            (&pat[..end], &pat[end..])
        }
        Some(_) => {
            let c = pat.chars().next().unwrap().len_utf8();
            (&pat[..c], &pat[c..])
        }
        None => ("", ""),
    }
}

fn class_end(pattern: &str, open: usize) -> Option<usize> {
    let bytes = pattern.as_bytes();
    let mut index = open + 1;
    if bytes.get(index) == Some(&b'^') {
        index += 1;
    }
    if bytes.get(index) == Some(&b']') {
        index += 1;
    }
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index += 2;
            continue;
        }
        if bytes[index] == b'[' && matches!(bytes.get(index + 1), Some(b':' | b'.' | b'=')) {
            let marker = bytes[index + 1];
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == marker && bytes[index + 1] == b']') {
                index += 1;
            }
            if index + 1 >= bytes.len() {
                return None;
            }
            index += 2;
            continue;
        }
        if bytes[index] == b']' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn eq_ci(a: char, b: char, options: RegexOptions) -> bool {
    if options.case_insensitive {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn atom_matches(atom: &str, ch: char, options: RegexOptions) -> bool {
    let b = atom.as_bytes();
    match b.first() {
        Some(b'.') if atom.len() == 1 => !options.newline_stop || ch != '\n',
        Some(b'\\') => match atom[1..].chars().next() {
            // Perl-style shorthand classes (PostgreSQL ARE): \d \w \s and their
            // negations; any other escape is the literal character.
            Some('d') => ch.is_ascii_digit(),
            Some('D') => !ch.is_ascii_digit(),
            Some('w') => is_word_character(ch),
            Some('W') => !is_word_character(ch),
            Some('s') => ch.is_ascii_whitespace(),
            Some('S') => !ch.is_ascii_whitespace(),
            Some('a') => ch == '\u{0007}',
            Some('b') => ch == '\u{0008}',
            Some('B') => ch == '\\',
            Some('c') => decode_control_escape(atom).is_some_and(|wanted| wanted == ch),
            Some('e') => ch == '\u{001b}',
            Some('f') => ch == '\u{000c}',
            Some('n') => ch == '\n',
            Some('r') => ch == '\r',
            Some('t') => ch == '\t',
            Some('v') => ch == '\u{000b}',
            Some('u' | 'U' | 'x') => {
                decode_character_escape(atom).is_some_and(|wanted| eq_ci(wanted, ch, options))
            }
            Some('0'..='9') => {
                decode_octal_escape(atom).is_some_and(|wanted| eq_ci(wanted, ch, options))
            }
            Some(c) => eq_ci(c, ch, options),
            None => false,
        },
        Some(b'[') => class_matches(atom, ch, options),
        Some(_) => atom
            .chars()
            .next()
            .map(|c| eq_ci(c, ch, options))
            .unwrap_or(false),
        None => false,
    }
}

fn decode_character_escape(atom: &str) -> Option<char> {
    let digits = match atom.as_bytes().get(1) {
        Some(b'u' | b'U' | b'x') => &atom[2..],
        _ => return None,
    };
    u32::from_str_radix(digits, 16)
        .ok()
        .and_then(char::from_u32)
}

fn decode_control_escape(atom: &str) -> Option<char> {
    let mut chars = atom.chars();
    (chars.next() == Some('\\') && chars.next() == Some('c'))
        .then(|| chars.next())
        .flatten()
        .filter(|_| chars.next().is_none())
        .and_then(|character| char::from_u32(u32::from(character) & 0x1f))
}

fn decode_octal_escape(atom: &str) -> Option<char> {
    let digits = atom.strip_prefix('\\')?;
    if digits.is_empty()
        || digits.len() > 3
        || !digits.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
    {
        return None;
    }
    u32::from_str_radix(digits, 8)
        .ok()
        .filter(|value| *value <= 0xff)
        .and_then(char::from_u32)
}

fn class_matches(class: &str, ch: char, options: RegexOptions) -> bool {
    let inner = &class[1..class.len().saturating_sub(1)];
    let (negated, mut offset) = if inner.starts_with('^') {
        (true, 1)
    } else {
        (false, 0)
    };
    let mut found = false;
    while offset < inner.len() {
        let Some((first, after_first)) = class_token(inner, offset) else {
            break;
        };
        if let ClassToken::Character(low) = first
            && inner.as_bytes().get(after_first) == Some(&b'-')
            && after_first + 1 < inner.len()
            && let Some((ClassToken::Character(high), after_high)) =
                class_token(inner, after_first + 1)
        {
            found |= in_range(low, high, ch, options);
            offset = after_high;
            continue;
        }
        found |= match first {
            ClassToken::Character(wanted) => eq_ci(wanted, ch, options),
            ClassToken::Named(name) => named_class_matches(name, ch),
            ClassToken::Shorthand(class) => shorthand_matches(class, ch),
        };
        offset = after_first;
    }
    (found != negated) && (!options.newline_stop || !negated || ch != '\n')
}

#[derive(Clone, Copy)]
enum ClassToken<'a> {
    Character(char),
    Named(&'a str),
    Shorthand(char),
}

fn class_token(class: &str, offset: usize) -> Option<(ClassToken<'_>, usize)> {
    let rest = class.get(offset..)?;
    if let Some(named) = rest.strip_prefix("[:")
        && let Some(end) = named.find(":]")
    {
        return Some((ClassToken::Named(&named[..end]), offset + 2 + end + 2));
    }
    if rest.starts_with("[.") || rest.starts_with("[=") {
        let marker = rest.as_bytes()[1] as char;
        let closing = if marker == '.' { ".]" } else { "=]" };
        let end = rest[2..].find(closing)?;
        let character = rest[2..2 + end].chars().next()?;
        return Some((ClassToken::Character(character), offset + 2 + end + 2));
    }
    if rest.starts_with('\\') {
        let (atom, _) = take_atom(rest);
        let character = match atom.as_bytes().get(1) {
            Some(b'a') => '\u{0007}',
            Some(b'b') => '\u{0008}',
            Some(b'B') => '\\',
            Some(b'c') => decode_control_escape(atom)?,
            Some(class @ (b'd' | b'D' | b'w' | b'W' | b's' | b'S')) => {
                return Some((ClassToken::Shorthand(*class as char), offset + atom.len()));
            }
            Some(b'e') => '\u{001b}',
            Some(b'f') => '\u{000c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'v') => '\u{000b}',
            Some(b'u' | b'U' | b'x') => decode_character_escape(atom)?,
            Some(b'0'..=b'9') => decode_octal_escape(atom)?,
            _ => atom[1..].chars().next()?,
        };
        return Some((ClassToken::Character(character), offset + atom.len()));
    }
    let character = rest.chars().next()?;
    Some((
        ClassToken::Character(character),
        offset + character.len_utf8(),
    ))
}

fn shorthand_matches(class: char, ch: char) -> bool {
    match class {
        'd' => ch.is_ascii_digit(),
        'D' => !ch.is_ascii_digit(),
        'w' => is_word_character(ch),
        'W' => !is_word_character(ch),
        's' => ch.is_ascii_whitespace(),
        'S' => !ch.is_ascii_whitespace(),
        _ => false,
    }
}

fn named_class_matches(name: &str, ch: char) -> bool {
    match name {
        "alnum" => ch.is_ascii_alphanumeric(),
        "alpha" => ch.is_ascii_alphabetic(),
        "ascii" => ch.is_ascii(),
        "blank" => matches!(ch, ' ' | '\t'),
        "cntrl" => ch.is_ascii_control(),
        "digit" => ch.is_ascii_digit(),
        "graph" => ch.is_ascii_graphic(),
        "lower" => ch.is_ascii_lowercase(),
        "print" => ch.is_ascii() && !ch.is_ascii_control(),
        "punct" => ch.is_ascii_punctuation(),
        "space" => ch.is_ascii_whitespace(),
        "upper" => ch.is_ascii_uppercase(),
        "word" => is_word_character(ch),
        "xdigit" => ch.is_ascii_hexdigit(),
        _ => false,
    }
}

fn is_named_class(name: &str) -> bool {
    matches!(
        name,
        "alnum"
            | "alpha"
            | "ascii"
            | "blank"
            | "cntrl"
            | "digit"
            | "graph"
            | "lower"
            | "print"
            | "punct"
            | "space"
            | "upper"
            | "word"
            | "xdigit"
    )
}

fn in_range(lo: char, hi: char, ch: char, options: RegexOptions) -> bool {
    if (lo..=hi).contains(&ch) {
        return true;
    }
    if options.case_insensitive {
        (lo..=hi).contains(&ch.to_ascii_lowercase()) || (lo..=hi).contains(&ch.to_ascii_uppercase())
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::regex_search;

    fn m(pat: &str, text: &str) -> bool {
        regex_search(pat, text, false).unwrap()
    }
    fn minute(pat: &str, text: &str) -> bool {
        regex_search(pat, text, true).unwrap()
    }

    #[test]
    fn posix_subset_matches_postgres() {
        // All expectations verified against PostgreSQL 18.4.
        assert!(m("^pg_toast", "pg_toast"));
        assert!(!m("^pg_toast", "public"));
        assert!(m("[0-9]+", "abc123"));
        assert!(minute("^abc", "ABC"));
        assert!(!m("^abc", "ABC"));
        assert!(!m("x", "hello"));
        assert!(m("a.b", "axb"));
        assert!(m("o+b", "foobar"));
        assert!(m("^x.*z$", "xyz"));
        assert!(m("c[aeiou]t", "cat"));
        assert!(!m("c[aeiou]t", "cxt"));
        assert!(m("[^0-9]", "a"));
        assert!(!m("^[^0-9]+$", "12a3"));
        assert!(m("colou?r", "color"));
        assert!(m("colou?r", "colour"));
        // Groups and alternation (as psql's \d wraps names).
        assert!(m("^(foo)$", "foo"));
        assert!(!m("^(foo)$", "foobar"));
        assert!(m("^(foo|bar)$", "bar"));
        assert!(!m("^(foo|bar)$", "baz"));
        assert!(m("^(ab)+$", "ababab"));
        assert!(!m("^(ab)+$", "aba"));
    }

    #[test]
    fn bounded_repetition() {
        // All expectations verified against PostgreSQL 18.4.
        assert!(m("^a{2}$", "aa"));
        assert!(!m("^a{2}$", "a"));
        assert!(!m("^a{2}$", "aaa"));
        assert!(m("^a{2,}$", "aaaa"));
        assert!(!m("^a{2,}$", "a"));
        assert!(m("^a{1,3}$", "aa"));
        assert!(!m("^a{1,3}$", "aaaa"));
        assert!(m("^(ab){2}$", "abab"));
        assert!(!m("^(ab){2}$", "ababab"));
        assert!(m("^[0-9]{4}-[0-9]{2}$", "2024-06"));
        // Zero-minimum bound.
        assert!(m("^a{0,2}$", ""));
        assert!(m("^a{0,2}$", "aa"));
        // A `{` not followed by a digit is a literal (PostgreSQL behavior).
        assert!(m("a{", "xa{y"));
        assert!(m("^a\\{$", "a{"));
        // Malformed / out-of-range bounds are loud errors.
        assert!(regex_search("a{2,1}", "a", false).is_err());
        assert!(regex_search("a{999}", "a", false).is_err());
        assert!(regex_search("a{2x}", "a", false).is_err());
    }

    #[test]
    fn non_greedy_quantifiers() {
        use super::find;
        // Boolean search is unaffected by greediness.
        assert!(m("a+?", "aaa"));
        assert!(m("a*?b", "aab"));
        // find(): a leading non-greedy quantifier makes the whole match
        // shortest-preference (PostgreSQL ARE rule).
        assert_eq!(find("a+?", "aaa", 0, false).unwrap(), Some((0, 1)));
        assert_eq!(find("a+", "aaa", 0, false).unwrap(), Some((0, 3)));
        assert_eq!(find("a??", "aaa", 0, false).unwrap(), Some((0, 0)));
        assert_eq!(find("a{2,3}?", "aaaa", 0, false).unwrap(), Some((0, 2)));
        assert_eq!(find("a{2,3}", "aaaa", 0, false).unwrap(), Some((0, 3)));
    }

    #[test]
    fn captures_record_group_spans() {
        use super::{MAX_GROUPS, find_captures};
        let mut spans = [(-1i64, -1i64); MAX_GROUPS];
        // Two groups: substrings "abc" (0..3) and "123" (4..7).
        let r = find_captures("([a-z]+)-([0-9]+)", "abc-123", 0, false, &mut spans).unwrap();
        assert_eq!(r, Some(((0, 7), 2)));
        assert_eq!(spans[0], (0, 3));
        assert_eq!(spans[1], (4, 7));
        // No groups: the whole match is reported, group count 0.
        let mut s2 = [(-1i64, -1i64); MAX_GROUPS];
        let r2 = find_captures("[0-9]+", "abc123", 0, false, &mut s2).unwrap();
        assert_eq!(r2, Some(((3, 6), 0)));
        // A repeated group keeps its last iteration.
        let mut third_spans = [(-1i64, -1i64); MAX_GROUPS];
        let r3 = find_captures("(ab)+", "ababab", 0, false, &mut third_spans).unwrap();
        assert_eq!(r3, Some(((0, 6), 1)));
        assert_eq!(third_spans[0], (4, 6));
        // 'g'-style second match starts after the first.
        let mut s4 = [(-1i64, -1i64); MAX_GROUPS];
        let r4 = find_captures("([0-9]+)", "a1b22", 3, false, &mut s4).unwrap();
        assert_eq!(r4, Some(((3, 5), 1)));
        assert_eq!(s4[0], (3, 5));
    }

    #[test]
    fn postgres_advanced_constructs_are_bounded_and_typed() {
        use super::{RegexOptions, find_with_options};

        assert!(m("^([bc])\\1*$", "bbbbb"));
        assert!(!m("^([bc])\\1*$", "bbc"));
        assert!(m("^(\\w+)( \\1)+$", "abc abc abc"));
        assert!(!m("^(\\w+)( \\1)+$", "abc abd abc"));
        assert!(m("a(?=b)b*", "ab"));
        assert!(!m("a(?=b)b*", "a"));
        assert!(m("a(?!b)b*", "a"));
        assert!(m("(?<=foo)b+", "foobar"));
        assert!(!m("(?<=f)b+", "foobar"));
        assert!(m("(?<=^a)b", "ab"));
        assert!(m("(?<=\\Aa)b", "ab"));
        assert!(m("(?<=\\ma )b", "a b"));
        assert!(m("(?:foo|bar)", "bar"));
        assert!(m("[[:alpha:]]+\\s+[[:digit:]]+", "abc 123"));
        assert!(m("\\mbar\\M", "foo bar"));
        assert!(!m("\\mbar\\M", "foobar"));
        assert!(m("\\x61\\u0062\\U00000063", "abc"));
        assert!(m("\\B", "\\"));
        assert!(m("\\cA", "\u{0001}"));
        assert!(m("\\101", "A"));
        assert!(m("[\\d]+", "123"));
        assert!(m("[\\B]", "\\"));
        assert!(m("[\\cA]", "\u{0001}"));
        assert!(m("[\\101]", "A"));
        assert!(!m("[[:alpha:]]", "é"));
        assert!(!m("\\w", "é"));
        assert!(!m("\\s", "\u{00a0}"));
        assert!(!super::regex_search("É", "é", true).unwrap());
        assert!(m("(^)+^", "a"));
        assert!(m("$($$)+", "a"));
        assert!(m("()*\\1", "a"));
        assert!(m("()+\\1", "a"));
        assert!(!m("$()|^\\1", "a"));
        assert!(!m("^(.)\\1|\\1.", "abcdef"));
        assert!(!m("^((.)\\2|..)\\2", "abadef"));
        assert!(regex_search("\\1", "a", false).is_err());
        assert!(regex_search("x(\\w)(?=\\1)", "xyz", false).is_err());
        assert!(regex_search("\\q", "q", false).is_err());
        assert!(regex_search("[\\q]", "q", false).is_err());
        assert!(regex_search("[[:bogus:]]", "a", false).is_err());
        assert!(regex_search("[z-a]", "a", false).is_err());
        assert!(regex_search("[a-[:digit:]]", "a", false).is_err());
        assert!(regex_search("[[:digit:]-a]", "a", false).is_err());
        assert!(regex_search("(?z)a", "a", false).is_err());
        assert!(regex_search("*a", "a", false).is_err());
        assert!(regex_search("a**", "a", false).is_err());
        assert!(regex_search("{1}a", "a", false).is_err());
        assert!(regex_search("^*a", "a", false).is_err());
        let mut nested = [(-1i64, -1i64); super::MAX_GROUPS];
        assert_eq!(
            super::find_captures("((a))+", "a", 0, false, &mut nested).unwrap(),
            Some(((0, 1), 2))
        );
        let mut constrained = [(-1i64, -1i64); super::MAX_GROUPS];
        assert_eq!(
            super::find_captures("(?=(ab))a", "ab", 0, false, &mut constrained).unwrap(),
            Some(((0, 1), 0))
        );
        assert!(regex_search("(?=(a))\\1", "a", false).is_err());

        let literal = RegexOptions {
            flavor: super::RegexFlavor::Literal,
            ..RegexOptions::default()
        };
        assert_eq!(
            find_with_options("a.b", "a.b", 0, literal).unwrap(),
            Some((0, 3))
        );
        assert_eq!(find_with_options("a.b", "axb", 0, literal).unwrap(), None);
        assert!(m("***=a+b", "a+b"));
        assert!(m("***:a+b", "aaab"));
        assert!(m("(?i)abc", "ABC"));
        let quoted_expanded = RegexOptions {
            flavor: super::RegexFlavor::Literal,
            expanded: true,
            ..RegexOptions::default()
        };
        assert!(find_with_options("a b", "a b", 0, quoted_expanded).is_err());
        let expanded_director = RegexOptions {
            expanded: true,
            ..RegexOptions::default()
        };
        assert_eq!(
            find_with_options("***=a b", "a b", 0, expanded_director).unwrap(),
            Some((0, 3))
        );
        assert_eq!(
            find_with_options("***=a b", "ab", 0, expanded_director).unwrap(),
            None
        );
        let basic = RegexOptions {
            flavor: super::RegexFlavor::Basic,
            ..RegexOptions::default()
        };
        assert_eq!(find_with_options("(?i)a", "A", 0, basic).unwrap(), None);
        assert_eq!(
            find_with_options("(?i)a", "(?i)a", 0, basic).unwrap(),
            Some((0, 5))
        );
        assert_eq!(
            find_with_options("\\d", "d", 0, basic).unwrap(),
            Some((0, 1))
        );
        assert_eq!(find_with_options("[\\d]", "1", 0, basic).unwrap(), None);
        let extended = RegexOptions {
            flavor: super::RegexFlavor::Extended,
            ..RegexOptions::default()
        };
        assert_eq!(find_with_options("(?i)a", "A", 0, extended).unwrap(), None);
        assert_eq!(
            find_with_options("\\d", "d", 0, extended).unwrap(),
            Some((0, 1))
        );
        assert_eq!(find_with_options("[\\d]", "1", 0, extended).unwrap(), None);
        assert!(find_with_options("\\1", "1", 0, extended).is_err());

        let expanded = RegexOptions {
            expanded: true,
            ..RegexOptions::default()
        };
        assert_eq!(
            find_with_options("a # ignored\n b", "ab", 0, expanded).unwrap(),
            Some((0, 2))
        );

        let newline = RegexOptions {
            newline_stop: true,
            newline_anchors: true,
            ..RegexOptions::default()
        };
        assert_eq!(
            find_with_options("^b$", "a\nb", 0, newline).unwrap(),
            Some((2, 3))
        );
        assert_eq!(find_with_options("a.b", "a\nb", 0, newline).unwrap(), None);
        assert_eq!(
            find_with_options("(?<=^a )b", "x\na b", 0, newline).unwrap(),
            Some((4, 5))
        );
    }
}
