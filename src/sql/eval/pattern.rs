//! Pattern matching: `LIKE`, `SIMILAR TO`, and the regex helpers.
//!
//! The three are one family in SQL and three grammars in practice. `LIKE` is
//! matched directly — `%` and `_` over characters, with an optional ESCAPE —
//! while `SIMILAR TO` is SQL's own dialect, rewritten here into the POSIX form
//! the regex engine takes so there is one matcher underneath rather than two.

use crate::mem::arena::Arena;
use crate::sql::types::Datum;
use crate::sql_err;

use super::{arena_full, SqlError};

/// Translates a SQL `SIMILAR TO` pattern into a POSIX regular expression
/// anchored to the whole string. `%`/`_` become `.*`/`.`; the SIMILAR TO
/// metacharacters (`| * + ? ( ) [ ] { }`) pass through; characters that are
/// literal in SIMILAR TO but special in POSIX (`. ^ $`) are escaped; `\` escapes
/// the next character. Inside a `[...]` bracket expression everything is literal.
pub(crate) fn similar_to_posix(
    pattern: &str,
    buffer: &mut crate::util::StackStr<256>,
    escape: Option<char>,
) -> Result<(), SqlError> {
    use core::fmt::Write as _;
    let _ = buffer.write_char('^');
    let mut chars = pattern.chars();
    let mut in_bracket = false;
    while let Some(c) = chars.next() {
        if in_bracket {
            let _ = buffer.write_char(c);
            if c == ']' {
                in_bracket = false;
            }
            continue;
        }
        if Some(c) == escape {
            // The escaped character stands for itself, so it reaches POSIX
            // backslash-quoted whatever it is.
            if let Some(next) = chars.next() {
                let _ = buffer.write_char('\\');
                let _ = buffer.write_char(next);
            }
            continue;
        }
        match c {
            '%' => {
                let _ = buffer.write_str(".*");
            }
            '_' => {
                let _ = buffer.write_char('.');
            }
            '.' | '^' | '$' => {
                let _ = buffer.write_char('\\');
                let _ = buffer.write_char(c);
            }
            '[' => {
                let _ = buffer.write_char('[');
                in_bracket = true;
            }
            _ => {
                let _ = buffer.write_char(c);
            }
        }
    }
    let _ = buffer.write_char('$');
    if buffer.is_truncated() {
        return Err(sql_err!("22026", "SIMILAR TO pattern is too long"));
    }
    Ok(())
}

/// SQL LIKE: `%` matches any run (including empty), `_` exactly one
/// character, `\` escapes the next pattern character. Iterative
/// two-pointer match with backtracking to the last `%`; allocation-free.
pub fn like_match(
    text: &str,
    pattern: &str,
    case_insensitive: bool,
    escape: Option<char>,
) -> bool {
    fn next_char(s: &str, at: usize) -> Option<(char, usize)> {
        s[at..].chars().next().map(|c| (c, at + c.len_utf8()))
    }
    let eq = |a: char, b: char| {
        if case_insensitive {
            a.to_lowercase().eq(b.to_lowercase())
        } else {
            a == b
        }
    };

    let mut t = 0usize;
    let mut p = 0usize;
    let mut star: Option<(usize, usize)> = None; // (pattern pos after %, text pos)

    loop {
        if let Some((pc, p_next)) = next_char(pattern, p) {
            match pc {
                '%' => {
                    star = Some((p_next, t));
                    p = p_next;
                    continue;
                }
                '_' => {
                    if let Some((_, t_next)) = next_char(text, t) {
                        t = t_next;
                        p = p_next;
                        continue;
                    }
                }
                c if Some(c) == escape => {
                    let (want, after) = match next_char(pattern, p_next) {
                        Some((c, n)) => (c, n),
                        None => (c, p_next), // a trailing escape stands for itself
                    };
                    if let Some((tc, t_next)) = next_char(text, t)
                        && eq(tc, want) {
                            t = t_next;
                            p = after;
                            continue;
                        }
                }
                _ => {
                    if let Some((tc, t_next)) = next_char(text, t)
                        && eq(tc, pc) {
                            t = t_next;
                            p = p_next;
                            continue;
                        }
                }
            }
        } else if t >= text.len() {
            return true;
        }
        // Mismatch (or pattern exhausted with text left): backtrack.
        match star {
            Some((star_p, star_t)) => match next_char(text, star_t) {
                Some((_, nt)) => {
                    star = Some((star_p, nt));
                    t = nt;
                    p = star_p;
                }
                None => return false,
            },
            None => return false,
        }
    }
}

/// `substring(str FROM posix_pattern)`: the first match, or its first capture
/// group when the pattern has one (PostgreSQL semantics). NULL if no match.
pub(crate) fn regex_substring<'a>(s: &'a str, pattern: &str) -> Result<Datum<'a>, SqlError> {
    let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
    match crate::sql::regex::find_captures(pattern, s, 0, false, &mut spans)? {
        None => Ok(Datum::Null),
        Some(((mstart, mend), ng)) => {
            if ng >= 1 {
                let (gs, ge) = spans[0];
                if gs < 0 {
                    Ok(Datum::Null)
                } else {
                    Ok(Datum::Text(&s[gs as usize..ge as usize]))
                }
            } else {
                Ok(Datum::Text(&s[mstart..mend]))
            }
        }
    }
}

/// `substring(str FROM sql_pattern FOR escape)`: the SQL-regular-expression
/// form. The pattern uses `SIMILAR TO` syntax; an `<escape>"` pair delimits the
/// portion to return (else the whole match is returned). NULL if no match.
pub(crate) fn sql_regex_substring<'a>(
    s: &'a str,
    pattern: &str,
    escape: &str,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let escape_char = escape.chars().next();
    // Translate to a POSIX regex, turning the `<escape>"` markers into a single
    // capture group and every SIMILAR TO metacharacter per `similar_to_posix`.
    let mut posix = crate::util::StackStr::<512>::new();
    use core::fmt::Write as _;
    let _ = posix.write_char('^');
    let mut chars = pattern.chars().peekable();
    let mut captures = 0u8;
    let mut in_bracket = false;
    while let Some(c) = chars.next() {
        if Some(c) == escape_char {
            match chars.next() {
                Some('"') => {
                    // Group boundary: first opens the capture, second closes it.
                    // PostgreSQL allows exactly zero or two, and says so rather
                    // than letting a third reach the regex engine as an
                    // unbalanced parenthesis.
                    if captures == 2 {
                        return Err(sql_err!(
                            "2200C",
                            "SQL regular expression may not contain more than two escape-double-quote separators"
                        ));
                    }
                    let _ = posix.write_char(if captures == 0 { '(' } else { ')' });
                    captures += 1;
                }
                Some(other) => {
                    // An escaped metacharacter is a literal.
                    if "\\^$.[]|()*+?{}".contains(other) {
                        let _ = posix.write_char('\\');
                    }
                    let _ = posix.write_char(other);
                }
                None => return Err(sql_err!("22025", "invalid escape string")),
            }
            continue;
        }
        if in_bracket {
            let _ = posix.write_char(c);
            if c == ']' {
                in_bracket = false;
            }
            continue;
        }
        match c {
            '%' => {
                // Outside the `#"..."#` capture, `%` is lazy so the captured run
                // is maximal; inside it, greedy so the capture itself is maximal
                // — matching PostgreSQL's SQL-regex substring extraction.
                let _ = posix.write_str(if captures == 1 { ".*" } else { ".*?" });
            }
            '_' => {
                let _ = posix.write_char('.');
            }
            '[' => {
                in_bracket = true;
                let _ = posix.write_char('[');
            }
            '.' | '^' | '$' | '\\' => {
                let _ = posix.write_char('\\');
                let _ = posix.write_char(c);
            }
            _ => {
                let _ = posix.write_char(c);
            }
        }
    }
    let _ = posix.write_char('$');
    if posix.is_truncated() {
        return Err(sql_err!("54000", "substring pattern too long"));
    }
    let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
    match crate::sql::regex::find_captures(posix.as_str(), s, 0, false, &mut spans)? {
        None => Ok(Datum::Null),
        Some(((mstart, mend), ng)) => {
            let (from, to) = if ng >= 1 && spans[0].0 >= 0 {
                (spans[0].0 as usize, spans[0].1 as usize)
            } else {
                (mstart, mend)
            };
            Ok(Datum::Text(arena.alloc_str(&s[from..to]).map_err(|_| arena_full())?))
        }
    }
}

/// Splits `src` on every match of `pattern` into an arena slice of text pieces,
/// for callers outside this module (`regexp_split_to_table` in the FROM clause).
pub fn regex_split_pub<'a>(
    src: &'a str,
    pattern: &str,
    case_insensitive: bool,
    arena: &'a Arena,
) -> Result<&'a [Datum<'a>], SqlError> {
    let mut pieces = [Datum::Null; 1024];
    let n = regex_split(src, pattern, case_insensitive, &mut pieces)?;
    Ok(&*arena.alloc_slice_copy(&pieces[..n]).map_err(|_| arena_full())?)
}

/// Splits `src` on every match of `pattern`, writing the pieces into `out` and
/// returning the count. An empty pattern splits into individual characters.
pub(crate) fn regex_split<'a>(
    src: &'a str,
    pattern: &str,
    case_insensitive: bool,
    out: &mut [Datum<'a>],
) -> Result<usize, SqlError> {
    let mut n = 0usize;
    let mut push = |piece: &'a str, n: &mut usize| -> Result<(), SqlError> {
        if *n == out.len() {
            return Err(sql_err!("54000", "too many split pieces"));
        }
        out[*n] = Datum::Text(piece);
        *n += 1;
        Ok(())
    };
    if pattern.is_empty() {
        for (i, ch) in src.char_indices() {
            push(&src[i..i + ch.len_utf8()], &mut n)?;
        }
        return Ok(n);
    }
    let mut last = 0usize;
    let mut pos = 0usize;
    while pos <= src.len() {
        let Some((start, end)) = crate::sql::regex::find(pattern, src, pos, case_insensitive)? else {
            break;
        };
        if end == start {
            // A zero-width match: advance one character so the scan progresses.
            let step = src[pos..].chars().next().map_or(1, char::len_utf8);
            if pos + step > src.len() {
                break;
            }
            pos += step;
            continue;
        }
        push(&src[last..start], &mut n)?;
        last = end;
        pos = end;
    }
    push(&src[last..], &mut n)?;
    Ok(n)
}

/// Parses PostgreSQL regex flags into `(global, case_insensitive)`; an unknown
/// flag is a loud error.
pub fn regexp_flags(flags: &str) -> Result<(bool, bool), SqlError> {
    let mut global = false;
    let mut case_insensitive = false;
    for f in flags.chars() {
        match f {
            'g' => global = true,
            'i' => case_insensitive = true,
            'c' => case_insensitive = false,
            _ => return Err(sql_err!("22023", "invalid regular expression option: \"{}\"", f)),
        }
    }
    Ok((global, case_insensitive))
}
