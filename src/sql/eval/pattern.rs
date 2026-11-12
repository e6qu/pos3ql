//! Pattern matching: `LIKE`, `SIMILAR TO`, and the regex helpers.
//!
//! The three are one family in SQL and three grammars in practice. `LIKE` is
//! matched directly — `%` and `_` over characters, with an optional ESCAPE —
//! while `SIMILAR TO` is SQL's own dialect, rewritten here into the POSIX form
//! the regex engine takes so there is one matcher underneath rather than two.

use crate::mem::arena::Arena;
use crate::sql::types::Datum;
use crate::sql_err;

use super::{SqlError, arena_full, sqlstate};

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
    let _ = buffer.write_str("^(?:");
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
    let _ = buffer.write_str(")$");
    if buffer.is_truncated() {
        return Err(sql_err!(
            sqlstate::STRING_DATA_LENGTH_MISMATCH,
            "SIMILAR TO pattern is too long"
        ));
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
) -> Result<bool, SqlError> {
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
                        None => {
                            return Err(sql_err!(
                                sqlstate::INVALID_ESCAPE_SEQUENCE,
                                "LIKE pattern must not end with escape character"
                            ));
                        }
                    };
                    if let Some((tc, t_next)) = next_char(text, t)
                        && eq(tc, want)
                    {
                        t = t_next;
                        p = after;
                        continue;
                    }
                }
                _ => {
                    if let Some((tc, t_next)) = next_char(text, t)
                        && eq(tc, pc)
                    {
                        t = t_next;
                        p = p_next;
                        continue;
                    }
                }
            }
        } else if t >= text.len() {
            return Ok(true);
        }
        // Mismatch (or pattern exhausted with text left): backtrack.
        match star {
            Some((star_p, star_t)) => match next_char(text, star_t) {
                Some((_, nt)) => {
                    star = Some((star_p, nt));
                    t = nt;
                    p = star_p;
                }
                None => return Ok(false),
            },
            None => return Ok(false),
        }
    }
}

/// Binary `LIKE`: `%`, `_`, and the optional escape byte have the same grammar
/// as text LIKE, but every match unit is one byte rather than one character.
pub(crate) fn like_match_bytes(
    text: &[u8],
    pattern: &[u8],
    escape: Option<u8>,
) -> Result<bool, SqlError> {
    let mut text_at = 0usize;
    let mut pattern_at = 0usize;
    let mut wildcard = None;
    loop {
        if let Some(&byte) = pattern.get(pattern_at) {
            match byte {
                b'%' => {
                    pattern_at += 1;
                    wildcard = Some((pattern_at, text_at));
                    continue;
                }
                b'_' if text_at < text.len() => {
                    pattern_at += 1;
                    text_at += 1;
                    continue;
                }
                byte if Some(byte) == escape => {
                    let Some(&wanted) = pattern.get(pattern_at + 1) else {
                        return Err(sql_err!(
                            sqlstate::INVALID_ESCAPE_SEQUENCE,
                            "LIKE pattern must not end with escape character"
                        ));
                    };
                    if text.get(text_at) == Some(&wanted) {
                        text_at += 1;
                        pattern_at += 2;
                        continue;
                    }
                }
                byte if text.get(text_at) == Some(&byte) => {
                    text_at += 1;
                    pattern_at += 1;
                    continue;
                }
                _ => {}
            }
        } else if text_at == text.len() {
            return Ok(true);
        }
        let Some((next_pattern, wildcard_text)) = wildcard else {
            return Ok(false);
        };
        if wildcard_text == text.len() {
            return Ok(false);
        }
        text_at = wildcard_text + 1;
        wildcard = Some((next_pattern, text_at));
        pattern_at = next_pattern;
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
                            crate::sql::eval::sqlstate::INVALID_USE_OF_ESCAPE_CHARACTER,
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
                None => {
                    return Err(sql_err!(
                        sqlstate::INVALID_ESCAPE_SEQUENCE,
                        "invalid escape string"
                    ));
                }
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
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "substring pattern too long"
        ));
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
            Ok(Datum::Text(
                arena.alloc_str(&s[from..to]).map_err(|_| arena_full())?,
            ))
        }
    }
}

pub fn regex_split_count_pub(
    src: &str,
    pattern: &str,
    options: crate::sql::regex::RegexOptions,
) -> Result<usize, SqlError> {
    for_each_regex_split(src, pattern, options, |_, _| Ok(()))
}

pub fn regex_split_piece_at_pub<'a>(
    src: &'a str,
    pattern: &str,
    options: crate::sql::regex::RegexOptions,
    wanted: usize,
) -> Result<Option<&'a str>, SqlError> {
    let mut split = RegexSplit::new(src);
    let mut index = 0usize;
    while let Some(piece) = split.next(pattern, options)? {
        if index == wanted {
            return Ok(Some(piece));
        }
        index += 1;
    }
    Ok(None)
}

pub(crate) fn for_each_regex_split<'a>(
    src: &'a str,
    pattern: &str,
    options: crate::sql::regex::RegexOptions,
    mut visit: impl FnMut(&'a str, usize) -> Result<(), SqlError>,
) -> Result<usize, SqlError> {
    let mut split = RegexSplit::new(src);
    let mut n = 0usize;
    while let Some(piece) = split.next(pattern, options)? {
        visit(piece, n)?;
        n += 1;
    }
    Ok(n)
}

/// The position of one `regexp_split_to_table` call, advanced one piece at a
/// time so a retained reader keeps only these offsets.
#[derive(Clone, Copy)]
pub(crate) struct RegexSplit<'a> {
    src: &'a str,
    /// Start of the piece being built.
    last: usize,
    /// Where the next separator search begins.
    pos: usize,
    previous_match_end: Option<usize>,
    done: bool,
}

impl<'a> RegexSplit<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src,
            last: 0,
            pos: 0,
            previous_match_end: None,
            done: false,
        }
    }

    pub(crate) fn next(
        &mut self,
        pattern: &str,
        options: crate::sql::regex::RegexOptions,
    ) -> Result<Option<&'a str>, SqlError> {
        if self.done {
            return Ok(None);
        }
        let src = self.src;
        if pattern.is_empty() {
            // An empty pattern splits into characters.
            let Some(character) = src[self.pos..].chars().next() else {
                self.done = true;
                return Ok(None);
            };
            let start = self.pos;
            self.pos += character.len_utf8();
            return Ok(Some(&src[start..self.pos]));
        }
        while self.pos <= src.len() {
            let Some((start, end)) =
                crate::sql::regex::find_with_options(pattern, src, self.pos, options)?
            else {
                break;
            };
            if end == start {
                // PostgreSQL ignores a zero-width separator at either edge or
                // immediately after the previous match, but an isolated
                // interior zero-width match still splits the string.
                let split =
                    start != 0 && start != src.len() && self.previous_match_end != Some(start);
                let piece_start = self.last;
                if split {
                    self.last = start;
                }
                self.previous_match_end = Some(end);
                match crate::sql::regex::next_match_from(src, start, end) {
                    Some(next) => self.pos = next,
                    None => self.pos = src.len() + 1,
                }
                if split {
                    return Ok(Some(&src[piece_start..start]));
                }
                continue;
            }
            let piece_start = self.last;
            self.last = end;
            self.previous_match_end = Some(end);
            self.pos = end;
            return Ok(Some(&src[piece_start..start]));
        }
        self.done = true;
        Ok(Some(&src[self.last..]))
    }
}

/// One `regexp_match`/`regexp_matches` result: the capture groups as
/// `text[]` (an unmatched group is NULL), or the whole match when the pattern
/// has no groups.
pub(crate) fn regexp_match_array<'a>(
    string: &'a str,
    whole: (usize, usize),
    groups: &[(i64, i64)],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let mut elements = [Datum::Null; crate::sql::regex::MAX_GROUPS];
    let count = if groups.is_empty() {
        elements[0] = Datum::Text(&string[whole.0..whole.1]);
        1
    } else {
        for (element, (start, end)) in elements.iter_mut().zip(groups) {
            if *start >= 0 {
                *element = Datum::Text(&string[*start as usize..*end as usize]);
            }
        }
        groups.len()
    };
    Ok(Datum::Array {
        element: crate::sql::types::ArrElem::Text,
        raw: crate::sql::array::build(&elements[..count], arena)?,
    })
}

/// Parsed PostgreSQL regex flags: global result selection is kept separate
/// from the compile behavior used by every matcher entry point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegexpFlags {
    pub global: bool,
    pub options: crate::sql::regex::RegexOptions,
}

pub fn regexp_options(flags: &str) -> Result<RegexpFlags, SqlError> {
    let mut global = false;
    let mut options = crate::sql::regex::RegexOptions::default();
    for f in flags.chars() {
        if options.apply_flag(f)? {
            global = true;
        }
    }
    Ok(RegexpFlags { global, options })
}
