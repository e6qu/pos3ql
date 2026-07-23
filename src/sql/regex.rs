//! A POSIX Extended Regular Expression subset for the `~` / `!~` / `~*` /
//! `~*` operators — enough for application predicates and the introspection
//! queries `psql` issues (which wrap object names as `^(name)$`).
//!
//! Supported: literals, `.`, anchors `^` and `$`, quantifiers `*` `+` `?` and
//! bounded `{m}`/`{m,}`/`{m,n}` (each optionally non-greedy with a trailing
//! `?`), bracket expressions `[...]` (ranges and a leading `^` negation),
//! shorthand classes `\d \w \s` (and negations), grouping `(...)` with capture
//! tracking, alternation `|`, and `\` escaping.
//!
//! The matcher is a bounded backtracking recursion in continuation-passing
//! style (no allocation); a step budget guards against pathological blow-up.

use crate::sql::eval::sqlstate;
use core::cell::Cell;

use super::eval::SqlError;
use crate::sql_err;

/// Whether `pattern` matches anywhere in `text` (POSIX `~` semantics).
pub fn regex_search(pattern: &str, text: &str, case_insensitive: bool) -> Result<bool, SqlError> {
    validate(pattern)?;
    let anchored = pattern.starts_with('^');
    let pat = if anchored { &pattern[1..] } else { pattern };
    let budget = Cell::new(3_000_000u32);
    // The continuation accepts any leftover text, so an unanchored pattern
    // matches a substring; a trailing `$` in the pattern enforces the end.
    let accept = |_rest: &str| Ok(true);
    if anchored {
        return m(pat, text, case_insensitive, &budget, None, &accept);
    }
    let mut rest = text;
    loop {
        if m(pat, rest, case_insensitive, &budget, None, &accept)? {
            return Ok(true);
        }
        match rest.chars().next() {
            None => return Ok(false),
            Some(c) => rest = &rest[c.len_utf8()..],
        }
    }
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
    validate(pattern)?;
    let anchored = pattern.starts_with('^');
    let pat = if anchored { &pattern[1..] } else { pattern };
    let prefer_longest = pattern_prefers_longest(pat);
    let budget = Cell::new(3_000_000u32);
    // Best match anchored at `start`: a continuation that records the preferred
    // consumed length and always returns false forces the matcher to explore
    // every match length.
    let best_at = |start: usize| -> Result<Option<usize>, SqlError> {
        let sub = &text[start..];
        let best = Cell::new(None::<usize>);
        let accept = |rest: &str| {
            let consumed = sub.len() - rest.len();
            let better = best
                .get()
                .is_none_or(|b| if prefer_longest { consumed > b } else { consumed < b });
            if better {
                best.set(Some(consumed));
            }
            Ok(false)
        };
        m(pat, sub, case_insensitive, &budget, None, &accept)?;
        Ok(best.get())
    };
    if anchored {
        // `^` matches only at the absolute start of the string.
        if from == 0 {
            return Ok(best_at(0)?.map(|len| (0, len)));
        }
        return Ok(None);
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
    /// Opening-paren byte offsets, in group order (index + 1 = group number).
    group_starts: &'a [usize],
    /// Per-group `(start, end)` spans relative to the matched suffix; `(-1, -1)`
    /// until the group participates.
    spans: &'a [Cell<(i64, i64)>],
}

impl Recorder<'_> {
    /// The 1-based group number of the group whose `(` begins `pat`.
    fn group_of(&self, pat: &str) -> Option<usize> {
        let offset = pat.as_ptr() as usize - self.pat_base;
        self.group_starts.iter().position(|&s| s == offset).map(|i| i + 1)
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
}

/// Byte offsets of each capturing `(` in `pat` (escapes and `[...]` skipped),
/// in group order. Returns the group count.
fn group_starts(pat: &str, out: &mut [usize; MAX_GROUPS]) -> usize {
    let b = pat.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'[' => {
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
            }
            b'(' => {
                if n < out.len() {
                    out[n] = i;
                }
                n += 1;
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
    validate(pattern)?;
    let anchored = pattern.starts_with('^');
    let pat = if anchored { &pattern[1..] } else { pattern };
    let mut starts = [0usize; MAX_GROUPS];
    let ng = group_starts(pat, &mut starts);
    if ng > MAX_GROUPS {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many capture groups in regular expression"));
    }
    // Locate the leftmost-longest whole match first (POSIX semantics).
    let Some((mstart, mend)) = find(pattern, text, from, case_insensitive)? else {
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
        group_starts: &starts[..ng],
        spans: &spans[..ng],
    };
    let accept = |rest: &str| Ok(sub.len() - rest.len() == target);
    m(pat, sub, case_insensitive, &budget, Some(&recorder), &accept)?;
    for (i, span) in spans[..ng].iter().enumerate() {
        let (a, b) = span.get();
        spans_out[i] = if a < 0 { (-1, -1) } else { (a + mstart as i64, b + mstart as i64) };
    }
    Ok(Some(((mstart, mend), ng)))
}

/// Rejects unsupported constructs so a pattern is never matched incorrectly.
fn validate(pattern: &str) -> Result<(), SqlError> {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1, // skip escaped char
            b'[' => {
                i += 1;
                if bytes.get(i) == Some(&b'^') {
                    i += 1;
                }
                if bytes.get(i) == Some(&b']') {
                    i += 1;
                }
                while i < bytes.len() && bytes[i] != b']' {
                    i += 1;
                }
                if i >= bytes.len() {
                    return Err(sql_err!(sqlstate::INVALID_REGULAR_EXPRESSION, "invalid regular expression: unbalanced ["));
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(sql_err!(sqlstate::INVALID_REGULAR_EXPRESSION, "invalid regular expression: unbalanced ("));
                }
            }
            b'{' if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                // Validate the bound's shape here so a malformed one errors up
                // front (parse_bound re-parses it during matching). A `{` not
                // followed by a digit is a literal character.
                let (_, _, used) = parse_bound(&pattern[i..])?;
                i += used - 1;
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return Err(sql_err!(sqlstate::INVALID_REGULAR_EXPRESSION, "invalid regular expression: unbalanced ("));
    }
    Ok(())
}

fn step(budget: &Cell<u32>) -> Result<(), SqlError> {
    let b = budget.get();
    if b == 0 {
        return Err(sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "regular expression is too complex"));
    }
    budget.set(b - 1);
    Ok(())
}

/// Matches `pat` against a prefix of `text`; on success calls `k` with the
/// remaining text.
fn m(
    pat: &str,
    text: &str,
    case_insensitive: bool,
    budget: &Cell<u32>,
    rec: Option<&Recorder>,
    k: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    // Top-level alternation: try each branch.
    if let Some(bar) = top_level_bar(pat) {
        if m(&pat[..bar], text, case_insensitive, budget, rec, k)? {
            return Ok(true);
        }
        return m(&pat[bar + 1..], text, case_insensitive, budget, rec, k);
    }
    if pat.is_empty() {
        return k(text);
    }
    // End anchor (zero-width).
    if pat.as_bytes()[0] == b'$' && pat.len() == 1 {
        return if text.is_empty() { k(text) } else { Ok(false) };
    }
    // Group.
    if pat.as_bytes()[0] == b'(' {
        let close = matching_paren(pat);
        let body = &pat[1..close];
        let after = &pat[close + 1..];
        let (quant, rest) = parse_quant(after)?;
        let group = rec.and_then(|r| r.group_of(pat));
        let entry_len = text.len();
        // Continuation after a single, non-repeated group match: record the
        // group's span, then match the remainder.
        let cont = move |t: &str| {
            if let (Some(r), Some(g)) = (rec, group) {
                r.record(g, r.consumed(entry_len), r.consumed(t.len()));
            }
            m(rest, t, case_insensitive, budget, rec, k)
        };
        return match quant {
            None => m(body, text, case_insensitive, budget, rec, &cont),
            Some(q) => {
                // The repetition records each iteration (last wins); the
                // downstream continuation does not re-record the whole span.
                let after_reps = move |t: &str| m(rest, t, case_insensitive, budget, rec, k);
                rep_group(body, text, case_insensitive, budget, q, rec, group, &after_reps)
            }
        };
    }
    // A single atom (literal / '.' / escaped / class) plus optional quantifier.
    let (atom, after) = take_atom(pat);
    let (quant, rest) = parse_quant(after)?;
    let cont = move |t: &str| m(rest, t, case_insensitive, budget, rec, k);
    match quant {
        Some(q) => rep_atom(atom, text, case_insensitive, budget, q, &cont),
        None => {
            if let Some(c) = text.chars().next()
                && atom_matches(atom, c, case_insensitive)
            {
                cont(&text[c.len_utf8()..])
            } else {
                Ok(false)
            }
        }
    }
}

/// Repetition of a single `atom` within `[q.min, q.max]` occurrences, then
/// `cont`. Greedy prefers more occurrences; non-greedy prefers fewer.
fn rep_atom(
    atom: &str,
    text: &str,
    case_insensitive: bool,
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
            && atom_matches(atom, c, case_insensitive)
        {
            rep_atom(atom, &text[c.len_utf8()..], case_insensitive, budget, q.step_down(), cont)
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
#[expect(clippy::too_many_arguments, reason = "capture recording threads context")]
fn rep_group(
    body: &str,
    text: &str,
    case_insensitive: bool,
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
            // Guard against an empty-body infinite loop.
            if t.len() == start_len {
                return Ok(false);
            }
            if let (Some(r), Some(g)) = (rec, group) {
                r.record(g, r.consumed(start_len), r.consumed(t.len()));
            }
            rep_group(body, t, case_insensitive, budget, q.step_down(), rec, group, cont)
        };
        m(body, text, case_insensitive, budget, rec, &more)
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
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
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
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
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
            max: if self.max == u32::MAX { u32::MAX } else { self.max - 1 },
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
        Some(b'*') => (Quant { min: 0, max: u32::MAX, greedy: true }, 1),
        Some(b'+') => (Quant { min: 1, max: u32::MAX, greedy: true }, 1),
        Some(b'?') => (Quant { min: 0, max: 1, greedy: true }, 1),
        // `{` opens a bound only when followed by a digit (PostgreSQL treats a
        // bare `{` as a literal character).
        Some(b'{') if b.get(1).is_some_and(u8::is_ascii_digit) => {
            let (min, max, after) = parse_bound(pat)?;
            (Quant { min, max, greedy: true }, after)
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
    let bad = || sql_err!(sqlstate::INVALID_REGULAR_EXPRESSION, "invalid regular expression: invalid repetition count(s)");
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
                i += 1;
                while i < b.len() && b[i] != b']' {
                    i += 1;
                }
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

/// Splits offset the first atom of `pat`: a single char, `.`, an escaped char, or
/// a `[...]` class.
fn take_atom(pat: &str) -> (&str, &str) {
    let b = pat.as_bytes();
    match b.first() {
        Some(b'\\') => {
            let c = pat[1..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
            (&pat[..1 + c], &pat[1 + c..])
        }
        Some(b'[') => {
            let mut j = 1;
            if b.get(j) == Some(&b'^') {
                j += 1;
            }
            if b.get(j) == Some(&b']') {
                j += 1;
            }
            while j < b.len() && b[j] != b']' {
                j += 1;
            }
            let end = (j + 1).min(b.len());
            (&pat[..end], &pat[end..])
        }
        Some(_) => {
            let c = pat.chars().next().unwrap().len_utf8();
            (&pat[..c], &pat[c..])
        }
        None => ("", ""),
    }
}

fn eq_ci(a: char, b: char, case_insensitive: bool) -> bool {
    if case_insensitive {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn atom_matches(atom: &str, ch: char, case_insensitive: bool) -> bool {
    let b = atom.as_bytes();
    match b.first() {
        Some(b'.') if atom.len() == 1 => true,
        Some(b'\\') => match atom[1..].chars().next() {
            // Perl-style shorthand classes (PostgreSQL ARE): \d \w \s and their
            // negations; any other escape is the literal character.
            Some('d') => ch.is_ascii_digit(),
            Some('D') => !ch.is_ascii_digit(),
            Some('w') => ch.is_ascii_alphanumeric() || ch == '_',
            Some('W') => !(ch.is_ascii_alphanumeric() || ch == '_'),
            Some('s') => ch.is_ascii_whitespace(),
            Some('S') => !ch.is_ascii_whitespace(),
            Some(c) => eq_ci(c, ch, case_insensitive),
            None => false,
        },
        Some(b'[') => class_matches(atom, ch, case_insensitive),
        Some(_) => atom.chars().next().map(|c| eq_ci(c, ch, case_insensitive)).unwrap_or(false),
        None => false,
    }
}

fn class_matches(class: &str, ch: char, case_insensitive: bool) -> bool {
    let inner = &class[1..class.len().saturating_sub(1)];
    let mut it = inner.chars().peekable();
    let negated = matches!(it.peek(), Some('^'));
    if negated {
        it.next();
    }
    let mut found = false;
    let mut prev: Option<char> = None;
    let mut pending_dash = false;
    while let Some(c) = it.next() {
        if c == '-' && prev.is_some() && it.peek().is_some() {
            pending_dash = true;
            continue;
        }
        if pending_dash {
            if let Some(lo) = prev
                && in_range(lo, c, ch, case_insensitive)
            {
                found = true;
            }
            pending_dash = false;
            prev = None;
            continue;
        }
        if eq_ci(c, ch, case_insensitive) {
            found = true;
        }
        prev = Some(c);
    }
    if pending_dash && eq_ci('-', ch, case_insensitive) {
        found = true;
    }
    found != negated
}

fn in_range(lo: char, hi: char, ch: char, case_insensitive: bool) -> bool {
    if (lo..=hi).contains(&ch) {
        return true;
    }
    if case_insensitive {
        (lo..=hi).contains(&ch.to_ascii_lowercase())
            || (lo..=hi).contains(&ch.to_ascii_uppercase())
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
        use super::{find_captures, MAX_GROUPS};
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
        let mut s3 = [(-1i64, -1i64); MAX_GROUPS];
        let r3 = find_captures("(ab)+", "ababab", 0, false, &mut s3).unwrap();
        assert_eq!(r3, Some(((0, 6), 1)));
        assert_eq!(s3[0], (4, 6));
        // 'g'-style second match starts after the first.
        let mut s4 = [(-1i64, -1i64); MAX_GROUPS];
        let r4 = find_captures("([0-9]+)", "a1b22", 3, false, &mut s4).unwrap();
        assert_eq!(r4, Some(((3, 5), 1)));
        assert_eq!(s4[0], (3, 5));
    }
}
