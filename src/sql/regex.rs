//! A POSIX Extended Regular Expression subset for the `~` / `!~` / `~*` /
//! `~*` operators — enough for application predicates and the introspection
//! queries `psql` issues (which wrap object names as `^(name)$`).
//!
//! Supported: literals, `.`, anchors `^` and `$`, quantifiers `*` `+` `?`,
//! bracket expressions `[...]` (ranges and a leading `^` negation), grouping
//! `(...)`, alternation `|`, and `\` escaping. Bounded repetition `{m,n}` is
//! not supported and is rejected loudly.
//!
//! The matcher is a bounded backtracking recursion in continuation-passing
//! style (no allocation); a step budget guards against pathological blow-up.

use core::cell::Cell;

use super::eval::{sqlstate, SqlError};
use crate::sql_err;

/// Whether `pattern` matches anywhere in `text` (POSIX `~` semantics).
pub fn regex_search(pattern: &str, text: &str, ci: bool) -> Result<bool, SqlError> {
    validate(pattern)?;
    let anchored = pattern.starts_with('^');
    let pat = if anchored { &pattern[1..] } else { pattern };
    let budget = Cell::new(3_000_000u32);
    // The continuation accepts any leftover text, so an unanchored pattern
    // matches a substring; a trailing `$` in the pattern enforces the end.
    let accept = |_rest: &str| Ok(true);
    if anchored {
        return m(pat, text, ci, &budget, &accept);
    }
    let mut rest = text;
    loop {
        if m(pat, rest, ci, &budget, &accept)? {
            return Ok(true);
        }
        match rest.chars().next() {
            None => return Ok(false),
            Some(c) => rest = &rest[c.len_utf8()..],
        }
    }
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
                    return Err(sql_err!("2201B", "invalid regular expression: unbalanced ["));
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(sql_err!("2201B", "invalid regular expression: unbalanced ("));
                }
            }
            b'{' => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "bounded repetition {{m,n}} is not supported"
                ))
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return Err(sql_err!("2201B", "invalid regular expression: unbalanced ("));
    }
    Ok(())
}

fn step(budget: &Cell<u32>) -> Result<(), SqlError> {
    let b = budget.get();
    if b == 0 {
        return Err(sql_err!("54000", "regular expression is too complex"));
    }
    budget.set(b - 1);
    Ok(())
}

/// Matches `pat` against a prefix of `text`; on success calls `k` with the
/// remaining text.
fn m(
    pat: &str,
    text: &str,
    ci: bool,
    budget: &Cell<u32>,
    k: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    // Top-level alternation: try each branch.
    if let Some(bar) = top_level_bar(pat) {
        if m(&pat[..bar], text, ci, budget, k)? {
            return Ok(true);
        }
        return m(&pat[bar + 1..], text, ci, budget, k);
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
        let (quant, rest) = split_quant(after);
        let cont = move |t: &str| m(rest, t, ci, budget, k);
        return match quant {
            None => m(body, text, ci, budget, &cont),
            Some(b'?') => {
                if m(body, text, ci, budget, &cont)? {
                    Ok(true)
                } else {
                    cont(text)
                }
            }
            _ => {
                let min = if quant == Some(b'+') { 1 } else { 0 };
                rep_group(body, text, ci, budget, min, &cont)
            }
        };
    }
    // A single atom (literal / '.' / escaped / class) plus optional quantifier.
    let (atom, after) = take_atom(pat);
    let (quant, rest) = split_quant(after);
    let cont = move |t: &str| m(rest, t, ci, budget, k);
    match quant {
        Some(b'*') | Some(b'+') => {
            let min = if quant == Some(b'+') { 1 } else { 0 };
            rep_atom(atom, text, ci, budget, min, &cont)
        }
        Some(b'?') => {
            if let Some(c) = text.chars().next()
                && atom_matches(atom, c, ci)
                && cont(&text[c.len_utf8()..])?
            {
                return Ok(true);
            }
            cont(text)
        }
        _ => {
            if let Some(c) = text.chars().next()
                && atom_matches(atom, c, ci)
            {
                cont(&text[c.len_utf8()..])
            } else {
                Ok(false)
            }
        }
    }
}

/// Greedy repetition of a single `atom`, at least `min` times, then `cont`.
fn rep_atom(
    atom: &str,
    text: &str,
    ci: bool,
    budget: &Cell<u32>,
    min: u32,
    cont: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    if let Some(c) = text.chars().next()
        && atom_matches(atom, c, ci)
        && rep_atom(atom, &text[c.len_utf8()..], ci, budget, min.saturating_sub(1), cont)?
    {
        return Ok(true);
    }
    if min == 0 {
        cont(text)
    } else {
        Ok(false)
    }
}

/// Greedy repetition of a group `body`, at least `min` times, then `cont`.
fn rep_group(
    body: &str,
    text: &str,
    ci: bool,
    budget: &Cell<u32>,
    min: u32,
    cont: &dyn Fn(&str) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    step(budget)?;
    let start_len = text.len();
    let more = |t: &str| {
        // Guard against an empty-body infinite loop.
        if t.len() == start_len {
            return Ok(false);
        }
        rep_group(body, t, ci, budget, min.saturating_sub(1), cont)
    };
    if m(body, text, ci, budget, &more)? {
        return Ok(true);
    }
    if min == 0 {
        cont(text)
    } else {
        Ok(false)
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

/// Splits an optional trailing quantifier byte off the front of `pat`.
fn split_quant(pat: &str) -> (Option<u8>, &str) {
    match pat.as_bytes().first() {
        Some(&q) if q == b'*' || q == b'+' || q == b'?' => (Some(q), &pat[1..]),
        _ => (None, pat),
    }
}

/// Splits off the first atom of `pat`: a single char, `.`, an escaped char, or
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

fn eq_ci(a: char, b: char, ci: bool) -> bool {
    if ci {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

fn atom_matches(atom: &str, ch: char, ci: bool) -> bool {
    let b = atom.as_bytes();
    match b.first() {
        Some(b'.') if atom.len() == 1 => true,
        Some(b'\\') => atom[1..].chars().next().map(|c| eq_ci(c, ch, ci)).unwrap_or(false),
        Some(b'[') => class_matches(atom, ch, ci),
        Some(_) => atom.chars().next().map(|c| eq_ci(c, ch, ci)).unwrap_or(false),
        None => false,
    }
}

fn class_matches(class: &str, ch: char, ci: bool) -> bool {
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
                && in_range(lo, c, ch, ci)
            {
                found = true;
            }
            pending_dash = false;
            prev = None;
            continue;
        }
        if eq_ci(c, ch, ci) {
            found = true;
        }
        prev = Some(c);
    }
    if pending_dash && eq_ci('-', ch, ci) {
        found = true;
    }
    found != negated
}

fn in_range(lo: char, hi: char, ch: char, ci: bool) -> bool {
    if (lo..=hi).contains(&ch) {
        return true;
    }
    if ci {
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
    fn mi(pat: &str, text: &str) -> bool {
        regex_search(pat, text, true).unwrap()
    }

    #[test]
    fn posix_subset_matches_postgres() {
        // All expectations verified against PostgreSQL 18.4.
        assert!(m("^pg_toast", "pg_toast"));
        assert!(!m("^pg_toast", "public"));
        assert!(m("[0-9]+", "abc123"));
        assert!(mi("^abc", "ABC"));
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
    fn unsupported_constructs_rejected() {
        assert!(regex_search("a{2}", "aa", false).is_err());
    }
}
