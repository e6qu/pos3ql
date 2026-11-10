//! SQL lexer following PostgreSQL's lexical rules
//! (<https://www.postgresql.org/docs/18/sql-syntax-lexical.html>):
//! case-folding of unquoted identifiers to lower case, `"quoted"`
//! identifiers with `""` escapes, `'strings'` with `''` escapes, `E'...'`
//! backslash strings, `$tag$ ... $tag$` dollar quoting, `--` and nested
//! `/* */` comments, `$n` parameters, and `::` casts.
//!
//! Token text borrows from the query where possible and from the
//! per-statement arena when unescaping or case-folding had to copy.

use super::eval::sqlstate;
use crate::mem::arena::Arena;
use crate::util::StackStr;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tok<'a> {
    /// Unquoted identifier or keyword, folded to lower case.
    Ident(&'a str),
    /// Quoted identifier, exact case.
    QuotedIdent(&'a str),
    /// Numeric literal, textual form.
    Num(&'a str),
    /// String literal, unescaped.
    Str(&'a str),
    /// Bit-string literal with its `B`/`X` prefix retained for input validation.
    Bit(&'a str),
    /// `$n` parameter placeholder, 1-based.
    Param(u32),
    /// Operator or punctuation: `+ - * / % ^ = <> != < > <= >= || :: . ( ) , ;`
    Op(&'a str),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexError {
    /// Byte offset in the query.
    pub at: usize,
    pub message: StackStrMsg,
    pub kind: LexErrorKind,
    /// End of the text PostgreSQL's scanner echoes as `at or near "..."`,
    /// which starts at `at`; `None` for errors reported without it.
    pub near_end: Option<usize>,
    pub hint: Option<&'static str>,
    /// The malformed UTF-8 sequence PostgreSQL lists in an encoding error.
    pub invalid_bytes: Option<InvalidBytes>,
}

/// Up to one UTF-8 character's worth of bytes that failed validation, as
/// PostgreSQL's `report_invalid_encoding` prints them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidBytes {
    bytes: [u8; 4],
    len: u8,
}

impl InvalidBytes {
    /// The bytes at the first invalid position of `text`: as many as the lead
    /// byte announces (one for a byte no sequence starts with), clipped to
    /// the input.
    fn at_first_error(text: &[u8], error: core::str::Utf8Error) -> Self {
        let rest = &text[error.valid_up_to()..];
        let announced = match rest[0] {
            lead if lead & 0x80 == 0 => 1,
            lead if lead & 0xE0 == 0xC0 => 2,
            lead if lead & 0xF0 == 0xE0 => 3,
            lead if lead & 0xF8 == 0xF0 => 4,
            _ => 1,
        };
        let len = announced.min(rest.len());
        let mut bytes = [0; 4];
        bytes[..len].copy_from_slice(&rest[..len]);
        Self {
            bytes,
            len: len as u8,
        }
    }

    /// A NUL byte, which PostgreSQL never accepts in text.
    const NUL: Self = Self {
        bytes: [0; 4],
        len: 1,
    };
}

impl LexError {
    /// The byte offset PostgreSQL reports as the error cursor. Scanner
    /// failures point at the offending text; encoding validation and pos3ql's
    /// own memory limit report none.
    pub fn cursor(&self) -> Option<usize> {
        match self.kind {
            LexErrorKind::Syntax
            | LexErrorKind::UnicodeEscape
            | LexErrorKind::UndefinedParameter => Some(self.at),
            LexErrorKind::Encoding | LexErrorKind::Memory => None,
        }
    }

    /// The message as PostgreSQL's `scanner_yyerror` reports it for `text`,
    /// the input this error's offsets index.
    pub fn render(&self, text: &str) -> StackStr<96> {
        if let Some(invalid) = self.invalid_bytes {
            let mut message = crate::stack_format!(96, "{}", self.message);
            for (index, byte) in invalid.bytes[..usize::from(invalid.len)].iter().enumerate() {
                let separator = if index == 0 { ": " } else { " " };
                let _ = core::fmt::Write::write_fmt(
                    &mut message,
                    format_args!("{separator}0x{byte:02x}"),
                );
            }
            return message;
        }
        let Some(near) = self.near_end.and_then(|end| text.get(self.at..end)) else {
            return StackStr::from_str(self.message);
        };
        // PostgreSQL's scanner_yyerror: a location at the end of the input
        // has no text to echo.
        if self.at == text.len() {
            return crate::stack_format!(96, "{} at end of input", self.message);
        }
        crate::stack_format!(96, "{} at or near \"{}\"", self.message, near)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexErrorKind {
    Syntax,
    Encoding,
    UnicodeEscape,
    /// `$0`, which PostgreSQL reports as an undefined parameter.
    UndefinedParameter,
    Memory,
}

impl LexErrorKind {
    pub fn sqlstate(self) -> &'static str {
        match self {
            Self::Syntax => sqlstate::SYNTAX_ERROR,
            Self::Encoding => sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            Self::UnicodeEscape => sqlstate::INVALID_ESCAPE_SEQUENCE,
            Self::UndefinedParameter => sqlstate::UNDEFINED_PARAMETER,
            Self::Memory => sqlstate::PROGRAM_LIMIT_EXCEEDED,
        }
    }
}

pub type StackStrMsg = &'static str;

/// PostgreSQL's `ident_start`: an ASCII letter, `_`, or any non-ASCII
/// character (its scanner accepts every byte from 0x80 to 0xFF).
pub(crate) fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_' || !character.is_ascii()
}

/// PostgreSQL's `ident_cont`: an identifier start, digit, or `$`.
pub(crate) fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit() || character == '$'
}

/// PostgreSQL's `space` class; other Unicode spaces are identifier characters.
fn is_sql_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\x0B' | '\x0C')
}

#[derive(Clone)]
pub struct Lexer<'a> {
    text: &'a str,
    arena: &'a Arena,
    at: usize,
    token_start: usize,
}

/// An opaque lexer position, for bounded lookahead (`mark` / `reset`).
#[derive(Clone, Copy)]
pub struct LexerMark {
    at: usize,
    token_start: usize,
}

impl<'a> Lexer<'a> {
    pub fn mark(&self) -> LexerMark {
        LexerMark {
            at: self.at,
            token_start: self.token_start,
        }
    }

    pub fn reset(&mut self, mark: LexerMark) {
        self.at = mark.at;
        self.token_start = mark.token_start;
    }

    pub fn new(text: &'a str, arena: &'a Arena) -> Self {
        Self {
            text,
            arena,
            at: 0,
            token_start: 0,
        }
    }

    /// Byte offset where the most recently returned token began.
    pub fn token_start(&self) -> usize {
        self.token_start
    }

    /// Byte offset immediately after the most recently returned token.
    /// Stored-expression rewrites use these parser-owned boundaries rather
    /// than re-scanning SQL literals themselves.
    pub fn token_end(&self) -> usize {
        self.at
    }

    pub fn next_token(&mut self) -> Result<Tok<'a>, LexError> {
        if self.at == 0 && self.text.as_bytes().contains(&0) {
            return Err(self.encoding_error(0, InvalidBytes::NUL));
        }
        self.skip_ws_and_comments()?;
        self.token_start = self.at;
        let rest = self.rest();
        let Some(c) = rest.chars().next() else {
            return Ok(Tok::Eof);
        };
        match c {
            '.' if rest.as_bytes().get(1).is_some_and(u8::is_ascii_digit) => self.number(),
            // PostgreSQL's DOT_DOT, the PL/pgSQL integer range separator.
            '.' if rest.starts_with("..") => {
                let operator = &self.text[self.at..self.at + 2];
                self.at += 2;
                Ok(Tok::Op(operator))
            }
            '(' | ')' | ',' | ';' | '.' | '[' | ']' => {
                let operator = &self.text[self.at..self.at + 1];
                self.at += 1;
                Ok(Tok::Op(operator))
            }
            '+' | '-' | '*' | '/' | '%' | '^' | '<' | '>' | '=' | '!' | '|' | '~' | '&' | '#'
            | '@' | '?' | '`' => self.operator(),
            ':' => {
                if rest.starts_with("::") {
                    self.at += 2;
                    Ok(Tok::Op("::"))
                } else if rest.starts_with(":=") {
                    // PostgreSQL's COLON_EQUALS: a named argument or a
                    // PL/pgSQL assignment.
                    self.at += 2;
                    Ok(Tok::Op(":="))
                } else {
                    // A lone colon is the array-slice separator (`a[1:3]`); the
                    // parser rejects it anywhere else.
                    self.at += 1;
                    Ok(Tok::Op(":"))
                }
            }
            '\'' => self.plain_string(),
            '"' => self.quoted_ident(),
            '$' => self.dollar(),
            '0'..='9' => self.number(),
            _ if is_identifier_start(c) => self.ident(),
            _ => Err(self.syntax_error()),
        }
    }

    fn operator(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        let bytes = self.text.as_bytes();
        while self.at < bytes.len()
            && matches!(
                bytes[self.at],
                b'+' | b'-'
                    | b'*'
                    | b'/'
                    | b'<'
                    | b'>'
                    | b'='
                    | b'~'
                    | b'!'
                    | b'@'
                    | b'#'
                    | b'%'
                    | b'^'
                    | b'&'
                    | b'|'
                    | b'`'
                    | b'?'
            )
        {
            if self.text[self.at..].starts_with("/*") || self.text[self.at..].starts_with("--") {
                break;
            }
            self.at += 1;
        }
        let contains_pg_extension_character =
            self.text.as_bytes()[start..self.at].iter().any(|byte| {
                matches!(
                    byte,
                    b'~' | b'!' | b'@' | b'#' | b'%' | b'^' | b'&' | b'|' | b'`' | b'?'
                )
            });
        if !contains_pg_extension_character {
            while self.at > start + 1 && matches!(bytes[self.at - 1], b'+' | b'-') {
                self.at -= 1;
            }
        }
        Ok(Tok::Op(&self.text[start..self.at]))
    }

    fn ident(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        let rest = self.rest();

        // E'...' escape-string syntax.
        if (rest.starts_with('e') || rest.starts_with('E')) && rest[1..].starts_with('\'') {
            self.at += 1;
            return self.escape_string();
        }

        // B'1010' bit-string and X'1F' hex-bit-string literals.
        if (rest.starts_with('b') || rest.starts_with('B')) && rest[1..].starts_with('\'') {
            self.at += 1;
            return self.bit_string(false);
        }
        if (rest.starts_with('x') || rest.starts_with('X')) && rest[1..].starts_with('\'') {
            self.at += 1;
            return self.bit_string(true);
        }

        let len = rest
            .char_indices()
            .find(|(_, c)| !is_identifier_continue(*c))
            .map_or(rest.len(), |(i, _)| i);
        self.at += len;
        let word = &self.text[start..self.at];
        if word.bytes().any(|b| b.is_ascii_uppercase()) {
            // PostgreSQL's downcase_identifier folds only ASCII letters in a
            // multibyte encoding, so `Äb` becomes `Äb`, not `äb`.
            let folded = self
                .arena
                .alloc_slice_copy(word.as_bytes())
                .map_err(|_| self.arena_full(start))?;
            folded.make_ascii_lowercase();
            // ASCII case folding preserves UTF-8 validity.
            Ok(Tok::Ident(unsafe {
                core::str::from_utf8_unchecked(folded)
            }))
        } else {
            Ok(Tok::Ident(word))
        }
    }

    /// PostgreSQL's numeric scanner rules, applied as flex does: the longest
    /// match wins and ties go to the earlier rule. Underscores separate digits
    /// (`1_000`), `0x`/`0o`/`0b` prefix non-decimal integers, and an identifier
    /// directly after a number is trailing junk.
    fn number(&mut self) -> Result<Tok<'a>, LexError> {
        #[derive(Clone, Copy)]
        enum Rule {
            Number,
            NonDecimalFail(&'static str),
            NumericFail,
            RealFail,
            Junk,
        }
        let start = self.at;
        let bytes = self.text.as_bytes();
        let digits_end = |at: usize, radix: u32| -> Option<usize> {
            let is_digit = |index: usize| {
                bytes
                    .get(index)
                    .is_some_and(|byte| char::from(*byte).is_digit(radix))
            };
            if !is_digit(at) {
                return None;
            }
            let mut index = at + 1;
            loop {
                if is_digit(index) {
                    index += 1;
                } else if bytes.get(index) == Some(&b'_') && is_digit(index + 1) {
                    index += 2;
                } else {
                    return Some(index);
                }
            }
        };
        let identifier_end = |at: usize| -> Option<usize> {
            let rest = &self.text[at..];
            let first = rest.chars().next().filter(|c| is_identifier_start(*c))?;
            Some(
                at + rest[first.len_utf8()..]
                    .char_indices()
                    .find(|(_, c)| !is_identifier_continue(*c))
                    .map_or(rest.len(), |(index, _)| first.len_utf8() + index),
            )
        };
        let decinteger = digits_end(start, 10);
        let numeric = match decinteger {
            Some(end) if bytes.get(end) == Some(&b'.') => {
                Some(digits_end(end + 1, 10).unwrap_or(end + 1))
            }
            Some(_) => None,
            None => digits_end(start + 1, 10),
        };
        let exponent = |base: Option<usize>| -> (Option<usize>, Option<usize>) {
            let Some(base) = base.filter(|end| matches!(bytes.get(*end), Some(b'e' | b'E'))) else {
                return (None, None);
            };
            let signed = matches!(bytes.get(base + 1), Some(b'+' | b'-'));
            let digits = base + 1 + usize::from(signed);
            (digits_end(digits, 10), signed.then_some(digits))
        };
        let (real_from_integer, real_fail_from_integer) = exponent(decinteger);
        let (real_from_numeric, real_fail_from_numeric) = exponent(numeric);
        let real = real_from_integer.max(real_from_numeric);
        let mut candidates = [None; 12];
        candidates[0] = decinteger.map(|end| (end, Rule::Number));
        if bytes[start] == b'0'
            && let Some((radix, failure)) = match bytes.get(start + 1) {
                Some(b'x' | b'X') => Some((16, "invalid hexadecimal integer")),
                Some(b'o' | b'O') => Some((8, "invalid octal integer")),
                Some(b'b' | b'B') => Some((2, "invalid binary integer")),
                _ => None,
            }
        {
            let prefixed = start + 2;
            let separated = prefixed + usize::from(bytes.get(prefixed) == Some(&b'_'));
            candidates[1] = digits_end(separated, radix).map(|end| (end, Rule::Number));
            candidates[2] = Some((separated, Rule::NonDecimalFail(failure)));
        }
        candidates[3] = numeric.map(|end| (end, Rule::Number));
        candidates[4] = decinteger
            .filter(|end| self.text[*end..].starts_with(".."))
            .map(|end| (end + 2, Rule::NumericFail));
        candidates[5] = real.map(|end| (end, Rule::Number));
        candidates[6] = real_fail_from_integer.map(|end| (end, Rule::RealFail));
        candidates[7] = real_fail_from_numeric.map(|end| (end, Rule::RealFail));
        for (slot, base) in [decinteger, numeric, real].into_iter().enumerate() {
            candidates[8 + slot] = base.and_then(identifier_end).map(|end| (end, Rule::Junk));
        }
        // `max_by_key` keeps the last maximum; reverse so ties keep the
        // earliest rule.
        let (end, rule) = candidates
            .into_iter()
            .flatten()
            .rev()
            .max_by_key(|(end, _)| *end)
            .expect("a digit or `.digit` starts a numeric rule");
        let error = |message| LexError {
            at: start,
            message,
            kind: LexErrorKind::Syntax,
            near_end: Some(end),
            hint: None,
            invalid_bytes: None,
        };
        match rule {
            Rule::Number => {
                self.at = end;
                Ok(Tok::Num(&self.text[start..end]))
            }
            Rule::NumericFail => {
                // The integer stands alone; `..` is the next token.
                let integer_end = end - 2;
                self.at = integer_end;
                Ok(Tok::Num(&self.text[start..integer_end]))
            }
            Rule::NonDecimalFail(message) => Err(error(message)),
            Rule::RealFail | Rule::Junk => Err(error("trailing junk after numeric literal")),
        }
    }

    /// PostgreSQL rejects an identifier character directly after a number or
    /// parameter (`1a`, `$1a`) instead of reading an alias or a new token.
    fn reject_trailing_junk(
        &self,
        start: usize,
        end: usize,
        message: &'static str,
    ) -> Result<(), LexError> {
        let rest = &self.text[end..];
        if !rest.chars().next().is_some_and(is_identifier_start) {
            return Ok(());
        }
        let length = rest
            .char_indices()
            .find(|(_, c)| !is_identifier_continue(*c))
            .map_or(rest.len(), |(index, _)| index);
        Err(LexError {
            at: start,
            message,
            kind: LexErrorKind::Syntax,
            near_end: Some(end + length),
            hint: None,
            invalid_bytes: None,
        })
    }

    fn plain_string(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        let bytes = self.text.as_bytes();
        let mut copied = false;
        let mut i = start + 1;
        let close = loop {
            match bytes.get(i) {
                None => return Err(self.unterminated_string()),
                Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => {
                    copied = true;
                    i += 2;
                }
                Some(b'\'') => match self.string_continuation(i + 1) {
                    Some(next) => {
                        copied = true;
                        i = next;
                    }
                    None => break i,
                },
                Some(_) => i += 1,
            }
        };
        self.at = close + 1;
        if !copied {
            return Ok(Tok::Str(&self.text[start + 1..close]));
        }
        // Copy into the arena, undoubling quotes and joining continued segments.
        let out = self
            .arena
            .alloc_slice_with(close - start - 1, |_| 0u8)
            .map_err(|_| self.arena_full(start))?;
        let mut w = 0;
        let mut r = start + 1;
        while r < close {
            if bytes[r] == b'\'' {
                if bytes[r + 1] == b'\'' {
                    out[w] = b'\'';
                    w += 1;
                    r += 2;
                } else {
                    r = self
                        .string_continuation(r + 1)
                        .expect("the first pass found this continuation");
                }
            } else {
                out[w] = bytes[r];
                w += 1;
                r += 1;
            }
        }
        // Segments split only at ASCII quotes, so each keeps whole characters.
        Ok(Tok::Str(unsafe {
            core::str::from_utf8_unchecked(&out[..w])
        }))
    }

    /// PostgreSQL's `quotecontinue`: string segments separated only by
    /// whitespace containing a newline (and `--` comments) are one literal.
    /// Returns the offset just past the next segment's opening quote.
    fn string_continuation(&self, mut at: usize) -> Option<usize> {
        let bytes = self.text.as_bytes();
        let mut newline = false;
        loop {
            match *bytes.get(at)? {
                b' ' | b'\t' | b'\x0C' => at += 1,
                b'\n' | b'\r' => {
                    newline = true;
                    at += 1;
                }
                // Vertical tab is whitespace only after the newline.
                b'\x0B' if newline => at += 1,
                b'-' if bytes.get(at + 1) == Some(&b'-') => {
                    while bytes
                        .get(at)
                        .is_some_and(|byte| !matches!(byte, b'\n' | b'\r'))
                    {
                        at += 1;
                    }
                }
                b'\'' if newline => return Some(at + 1),
                _ => return None,
            }
        }
    }

    fn unterminated_string(&self) -> LexError {
        LexError {
            at: self.token_start,
            message: "unterminated quoted string",
            kind: LexErrorKind::Syntax,
            near_end: Some(self.text.len()),
            hint: None,
            invalid_bytes: None,
        }
    }

    fn escape_string(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        self.at += 1; // opening quote
        let bytes = self.text.as_bytes();
        // Upper bound: unescaping never grows the text.
        let scratch = self
            .arena
            .alloc_slice_with(self.text.len() - self.at, |_| 0u8)
            .map_err(|_| self.arena_full(start))?;
        let mut w = 0;
        let mut i = self.at;
        loop {
            match bytes.get(i) {
                None => {
                    return Err(LexError {
                        at: self.token_start,
                        message: "unterminated quoted string",
                        kind: LexErrorKind::Syntax,
                        near_end: Some(self.text.len()),
                        hint: None,
                        invalid_bytes: None,
                    });
                }
                Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => {
                    scratch[w] = b'\'';
                    w += 1;
                    i += 2;
                }
                Some(b'\'') => match self.string_continuation(i + 1) {
                    Some(next) => i = next,
                    None => break,
                },
                Some(b'\\') => {
                    let esc = bytes.get(i + 1).ok_or(LexError {
                        at: self.token_start,
                        message: "unterminated quoted string",
                        kind: LexErrorKind::Syntax,
                        near_end: Some(self.text.len()),
                        hint: None,
                        invalid_bytes: None,
                    })?;
                    if matches!(esc, b'0'..=b'7') {
                        let mut value = 0u16;
                        let mut digits = 0usize;
                        while digits < 3 {
                            let Some(&digit @ b'0'..=b'7') = bytes.get(i + 1 + digits) else {
                                break;
                            };
                            value = value * 8 + u16::from(digit - b'0');
                            digits += 1;
                        }
                        scratch[w] = value as u8;
                        w += 1;
                        i += 1 + digits;
                        continue;
                    }
                    if *esc == b'x' {
                        let mut value = 0u8;
                        let mut digits = 0;
                        while digits < 2 {
                            let Some(digit) = bytes.get(i + 2 + digits).and_then(|b| hex_digit(*b))
                            else {
                                break;
                            };
                            value = value * 16 + digit;
                            digits += 1;
                        }
                        if digits != 0 {
                            scratch[w] = value;
                            w += 1;
                            i += 2 + digits;
                            continue;
                        }
                    }
                    if matches!(esc, b'u' | b'U') {
                        let (mut codepoint, mut next) = self.unicode_escape(bytes, i)?;
                        if (0xD800..=0xDBFF).contains(&codepoint) {
                            let (low, end) = self
                                .unicode_escape(bytes, next)
                                .map_err(|_| self.surrogate_error(bytes, next))?;
                            if !(0xDC00..=0xDFFF).contains(&low) {
                                return Err(self.surrogate_error(bytes, next));
                            }
                            codepoint = 0x10000 + ((codepoint - 0xD800) << 10) + low - 0xDC00;
                            next = end;
                        } else if (0xDC00..=0xDFFF).contains(&codepoint) {
                            return Err(self.surrogate_error(bytes, i));
                        }
                        let character =
                            char::from_u32(codepoint)
                                .filter(|c| *c != '\0')
                                .ok_or(LexError {
                                    at: i,
                                    message: "invalid Unicode escape value",
                                    kind: LexErrorKind::Syntax,
                                    near_end: Some(next),
                                    hint: None,
                                    invalid_bytes: None,
                                })?;
                        let mut encoded = [0u8; 4];
                        let encoded = character.encode_utf8(&mut encoded).as_bytes();
                        scratch[w..w + encoded.len()].copy_from_slice(encoded);
                        w += encoded.len();
                        i = next;
                        continue;
                    }
                    let replacement = match esc {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'b' => 8,
                        b'f' => 12,
                        b'v' => 11,
                        b'\\' => b'\\',
                        b'\'' => b'\'',
                        other => *other,
                    };
                    scratch[w] = replacement;
                    w += 1;
                    i += 2;
                }
                Some(&b) => {
                    scratch[w] = b;
                    w += 1;
                    i += 1;
                }
            }
        }
        self.at = i + 1;
        let out = &scratch[..w];
        if out.contains(&0) {
            return Err(self.encoding_error(start, InvalidBytes::NUL));
        }
        core::str::from_utf8(out)
            .map(Tok::Str)
            .map_err(|error| self.encoding_error(start, InvalidBytes::at_first_error(out, error)))
    }

    fn unicode_escape(&self, bytes: &[u8], at: usize) -> Result<(u32, usize), LexError> {
        let invalid = LexError {
            at,
            message: "invalid Unicode escape",
            kind: LexErrorKind::UnicodeEscape,
            near_end: None,
            hint: Some("Unicode escapes must be \\uXXXX or \\UXXXXXXXX."),
            invalid_bytes: None,
        };
        if bytes.get(at) != Some(&b'\\') {
            return Err(invalid);
        }
        let digits = match bytes.get(at + 1) {
            Some(b'u') => 4,
            Some(b'U') => 8,
            _ => return Err(invalid),
        };
        let mut value = 0u32;
        for offset in 0..digits {
            let digit = bytes
                .get(at + 2 + offset)
                .and_then(|b| hex_digit(*b))
                .ok_or(invalid)?;
            value = (value << 4) | u32::from(digit);
        }
        Ok((value, at + 2 + digits))
    }

    fn encoding_error(&self, at: usize, invalid_bytes: InvalidBytes) -> LexError {
        LexError {
            at,
            message: "invalid byte sequence for encoding \"UTF8\"",
            kind: LexErrorKind::Encoding,
            near_end: None,
            hint: None,
            invalid_bytes: Some(invalid_bytes),
        }
    }

    /// A surrogate escape without its partner, echoing what PostgreSQL's
    /// scanner read at `at`: a whole `\u`/`\U` escape, otherwise one
    /// character, or the end of the input.
    fn surrogate_error(&self, bytes: &[u8], at: usize) -> LexError {
        let near_end = match self.unicode_escape(bytes, at) {
            Ok((_, end)) => end,
            Err(_) => at + self.text[at..].chars().next().map_or(0, char::len_utf8),
        };
        LexError {
            at,
            message: "invalid Unicode surrogate pair",
            kind: LexErrorKind::Syntax,
            near_end: Some(near_end),
            hint: None,
            invalid_bytes: None,
        }
    }

    /// Lexes a `B'…'` (binary) or `X'…'` (hexadecimal) bit-string literal.
    /// Content validation belongs to the type-input boundary, where PostgreSQL
    /// reports 22P02 rather than a syntax error. The leading letter is already
    /// consumed; `self.at` points at the opening quote.
    fn bit_string(&mut self, hex: bool) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        self.at += 1; // opening quote
        let bytes = self.text.as_bytes();
        let mut i = self.at;
        loop {
            match bytes.get(i) {
                None => {
                    return Err(LexError {
                        at: self.token_start,
                        message: if hex {
                            "unterminated hexadecimal string literal"
                        } else {
                            "unterminated bit string literal"
                        },
                        kind: LexErrorKind::Syntax,
                        near_end: Some(self.text.len()),
                        hint: None,
                        invalid_bytes: None,
                    });
                }
                Some(b'\'') => match self.string_continuation(i + 1) {
                    Some(next) => i = next,
                    None => break,
                },
                Some(_) => i += 1,
            }
        }
        let close = i;
        self.at = close + 1;
        // Retain the spelling class so the evaluator can distinguish binary
        // and hexadecimal input errors without widening the AST.
        let out = self
            .arena
            .alloc_slice_with(close - start, |_| 0u8)
            .map_err(|_| self.arena_full(start))?;
        out[0] = if hex { b'X' } else { b'B' };
        let mut w = 1;
        let mut r = start + 1;
        while r < close {
            if bytes[r] == b'\'' {
                r = self
                    .string_continuation(r + 1)
                    .expect("the first pass found this continuation");
            } else {
                out[w] = bytes[r];
                w += 1;
                r += 1;
            }
        }
        // Segments split only at ASCII quotes, so each keeps whole characters.
        Ok(Tok::Bit(unsafe {
            core::str::from_utf8_unchecked(&out[..w])
        }))
    }

    fn quoted_ident(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        self.at += 1;
        let bytes = self.text.as_bytes();
        let mut has_escape = false;
        let mut i = self.at;
        loop {
            match bytes.get(i) {
                None => {
                    return Err(LexError {
                        at: self.token_start,
                        message: "unterminated quoted identifier",
                        kind: LexErrorKind::Syntax,
                        near_end: Some(self.text.len()),
                        hint: None,
                        invalid_bytes: None,
                    });
                }
                Some(b'"') if bytes.get(i + 1) == Some(&b'"') => {
                    has_escape = true;
                    i += 2;
                }
                Some(b'"') => break,
                Some(_) => i += 1,
            }
        }
        let raw = &self.text[self.at..i];
        self.at = i + 1;
        if raw.is_empty() {
            return Err(LexError {
                at: start,
                message: "zero-length delimited identifier",
                kind: LexErrorKind::Syntax,
                near_end: Some(start + 2),
                hint: None,
                invalid_bytes: None,
            });
        }
        if !has_escape {
            return Ok(Tok::QuotedIdent(raw));
        }
        let out = self
            .arena
            .alloc_slice_copy(raw.as_bytes())
            .map_err(|_| self.arena_full(start))?;
        let mut w = 0;
        let mut r = 0;
        while r < raw.len() {
            out[w] = raw.as_bytes()[r];
            if raw.as_bytes()[r] == b'"' {
                r += 1;
            }
            r += 1;
            w += 1;
        }
        let out = &out[..w];
        Ok(Tok::QuotedIdent(unsafe {
            core::str::from_utf8_unchecked(out)
        }))
    }

    fn dollar(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        let rest = self.rest();
        // $n parameter: a decimal integer, `_` separators allowed, that must
        // fit PostgreSQL's int4.
        let bytes = rest.as_bytes();
        let mut digits = 0;
        while bytes.get(1 + digits).is_some_and(u8::is_ascii_digit)
            || (digits > 0
                && bytes.get(1 + digits) == Some(&b'_')
                && bytes.get(2 + digits).is_some_and(u8::is_ascii_digit))
        {
            digits += 1;
        }
        if digits > 0 {
            let too_large = LexError {
                at: start,
                message: "parameter number too large",
                kind: LexErrorKind::Syntax,
                near_end: Some(start + 1 + digits),
                hint: None,
                invalid_bytes: None,
            };
            let mut n = 0u32;
            for byte in bytes[1..1 + digits].iter().filter(|byte| **byte != b'_') {
                n = n
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u32::from(byte - b'0')))
                    .filter(|value| *value <= i32::MAX as u32)
                    .ok_or(too_large)?;
            }
            if n == 0 {
                return Err(LexError {
                    at: start,
                    message: "there is no parameter $0",
                    kind: LexErrorKind::UndefinedParameter,
                    near_end: None,
                    hint: None,
                    invalid_bytes: None,
                });
            }
            self.reject_trailing_junk(start, start + 1 + digits, "trailing junk after parameter")?;
            self.at += 1 + digits;
            return Ok(Tok::Param(n));
        }
        // $tag$ ... $tag$ dollar-quoted string
        let tag_len = rest[1..]
            .char_indices()
            .find(|(_, c)| !(is_identifier_start(*c) || c.is_ascii_digit()))
            .map_or(rest.len() - 1, |(i, _)| i);
        let after_tag = 1 + tag_len;
        if !rest[after_tag..].starts_with('$') {
            return Err(self.syntax_error());
        }
        let open = &rest[..after_tag + 1]; // e.g. "$tag$"
        let body_start = self.at + open.len();
        let Some(close_rel) = self.text[body_start..].find(open) else {
            return Err(LexError {
                at: start,
                message: "unterminated dollar-quoted string",
                kind: LexErrorKind::Syntax,
                near_end: Some(self.text.len()),
                hint: None,
                invalid_bytes: None,
            });
        };
        let body = &self.text[body_start..body_start + close_rel];
        self.at = body_start + close_rel + open.len();
        Ok(Tok::Str(body))
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), LexError> {
        loop {
            let rest = self.rest();
            if rest.starts_with("--") {
                let line_end = rest.find('\n').map_or(rest.len(), |i| i + 1);
                self.at += line_end;
            } else if rest.starts_with("/*") {
                let mut depth = 1usize;
                let mut i = 2;
                let bytes = rest.as_bytes();
                while depth > 0 {
                    if i + 1 >= bytes.len() {
                        return Err(LexError {
                            at: self.at,
                            message: "unterminated /* comment",
                            kind: LexErrorKind::Syntax,
                            near_end: Some(self.text.len()),
                            hint: None,
                            invalid_bytes: None,
                        });
                    }
                    if bytes[i] == b'/' && bytes[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                self.at += i;
            } else if let Some(c) = rest.chars().next() {
                if is_sql_whitespace(c) {
                    self.at += c.len_utf8();
                } else {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        }
    }

    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    /// PostgreSQL's grammar error for a character no token starts with.
    fn syntax_error(&self) -> LexError {
        let length = self.rest().chars().next().map_or(0, char::len_utf8);
        LexError {
            at: self.at,
            message: "syntax error",
            kind: LexErrorKind::Syntax,
            near_end: Some(self.at + length),
            hint: None,
            invalid_bytes: None,
        }
    }

    fn arena_full(&self, at: usize) -> LexError {
        LexError {
            at,
            message: "statement too large for SQL arena",
            kind: LexErrorKind::Memory,
            near_end: None,
            hint: None,
            invalid_bytes: None,
        }
    }
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::mem::Budget;

    fn lex_all(text: &str) -> Vec<String> {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "test", 1 << 16).unwrap();
        let mut lexer = Lexer::new(text, &arena);
        let mut out = Vec::new();
        loop {
            match lexer.next_token().unwrap() {
                Tok::Eof => break,
                t => out.push(format!("{t:?}")),
            }
        }
        out
    }

    #[test]
    fn folds_unquoted_identifiers() {
        assert_eq!(
            lex_all("SELECT Foo, \"Bar\""),
            [
                "Ident(\"select\")",
                "Ident(\"foo\")",
                "Op(\",\")",
                "QuotedIdent(\"Bar\")"
            ]
        );
    }

    #[test]
    fn strings_and_escapes() {
        assert_eq!(lex_all("'it''s'"), ["Str(\"it's\")"]);
        assert_eq!(lex_all(r"E'a\nb'"), ["Str(\"a\\nb\")"]);
        assert_eq!(lex_all("$$raw $ text$$"), ["Str(\"raw $ text\")"]);
        assert_eq!(lex_all("$q$has $$ inside$q$"), ["Str(\"has $$ inside\")"]);
    }

    #[test]
    fn cluster_metadata_escape_strings_decode_without_allocation() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "escape strings", 1 << 16).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for (sql, expected) in [
                (r"E'\101\102\103'", "ABC"),
                (r"E'\x41\x4a\x4B'", "AJK"),
                (r"E'\x7Z'", "\u{7}Z"),
                (r"E'\xq\q'", "xqq"),
                (r"E'\303\251'", "é"),
                (r"E'\xC3\xA9'", "é"),
                (r"E'\u00E9\U0001F600'", "é😀"),
                (r"E'\uD83D\uDE00'", "😀"),
                (r"E'\U0000D83D\U0000DE00'", "😀"),
                (r"E'\U0010FFFF'", "\u{10ffff}"),
                (r"E'\v\b\f\n\r\t'", "\u{b}\u{8}\u{c}\n\r\t"),
                (r"E'\\u0000'", r"\u0000"),
            ] {
                let mut lexer = Lexer::new(sql, &arena);
                assert_eq!(lexer.next_token().unwrap(), Tok::Str(expected));
                assert_eq!(lexer.next_token().unwrap(), Tok::Eof);
            }
        });
    }

    #[test]
    fn cluster_metadata_escape_strings_reject_invalid_encoding_and_unicode() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "invalid escape strings", 1 << 16).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for (sql, kind) in [
                (r"E'\000'", LexErrorKind::Encoding),
                (r"E'\400'", LexErrorKind::Encoding),
                (r"E'\x00'", LexErrorKind::Encoding),
                (r"E'\xff'", LexErrorKind::Encoding),
                (r"E'\xc0\x80'", LexErrorKind::Encoding),
                (r"E'\xed\xa0\x80'", LexErrorKind::Encoding),
                (r"E'\u0'", LexErrorKind::UnicodeEscape),
                (r"E'\U0000000'", LexErrorKind::UnicodeEscape),
                (r"E'\uZZZZ'", LexErrorKind::UnicodeEscape),
                (r"E'\u0000'", LexErrorKind::Syntax),
                (r"E'\U00110000'", LexErrorKind::Syntax),
                (r"E'\uDC00'", LexErrorKind::Syntax),
                (r"E'\uD800'", LexErrorKind::Syntax),
                (r"E'\uD800\u0041'", LexErrorKind::Syntax),
                (r"E'\uD800\x41'", LexErrorKind::Syntax),
                ("'zero\0byte'", LexErrorKind::Encoding),
                ("$$zero\0byte$$", LexErrorKind::Encoding),
                ("\"zero\0byte\"", LexErrorKind::Encoding),
            ] {
                assert_eq!(Lexer::new(sql, &arena).next_token().unwrap_err().kind, kind);
            }
            assert!(super::super::ast::PreparedTransactionId::parse("invalid\0gid").is_none());
        });
    }

    #[test]
    fn scanner_errors_render_postgresql_messages_cursors_and_hints() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "scanner diagnostics", 1 << 16).unwrap();
        // Messages, SQLSTATEs and cursor byte offsets from PostgreSQL 18.6.
        let invalid_escape_hint = Some(r"Unicode escapes must be \uXXXX or \UXXXXXXXX.");
        for (sql, message, state, cursor, hint) in [
            ("$0", "there is no parameter $0", "42P02", Some(0), None),
            (
                "$99999999999",
                "parameter number too large at or near \"$99999999999\"",
                "42601",
                Some(0),
                None,
            ),
            (
                r"E'\uD800'",
                "invalid Unicode surrogate pair at or near \"'\"",
                "42601",
                Some(8),
                None,
            ),
            (
                r"E'\uD800\uD800'",
                r#"invalid Unicode surrogate pair at or near "\uD800""#,
                "42601",
                Some(8),
                None,
            ),
            (
                r"E'\uD800\x41'",
                r#"invalid Unicode surrogate pair at or near "\""#,
                "42601",
                Some(8),
                None,
            ),
            (
                r"E'x\uDC00y'",
                r#"invalid Unicode surrogate pair at or near "\uDC00""#,
                "42601",
                Some(3),
                None,
            ),
            (
                r"E'\uD800",
                "invalid Unicode surrogate pair at end of input",
                "42601",
                Some(8),
                None,
            ),
            (
                r"E'\U00110000'",
                r#"invalid Unicode escape value at or near "\U00110000""#,
                "42601",
                Some(2),
                None,
            ),
            (
                r"E'\uZZ'",
                "invalid Unicode escape",
                "22025",
                Some(2),
                invalid_escape_hint,
            ),
            (
                r"E'\xff'",
                "invalid byte sequence for encoding \"UTF8\": 0xff",
                "22021",
                None,
                None,
            ),
            (
                r"E'ok\xc3\x28'",
                "invalid byte sequence for encoding \"UTF8\": 0xc3 0x28",
                "22021",
                None,
                None,
            ),
            (
                r"E'\xed\xa0\x80'",
                "invalid byte sequence for encoding \"UTF8\": 0xed 0xa0 0x80",
                "22021",
                None,
                None,
            ),
            (
                r"E'\xf0\x9f'",
                "invalid byte sequence for encoding \"UTF8\": 0xf0 0x9f",
                "22021",
                None,
                None,
            ),
            (
                r"E'\400'",
                "invalid byte sequence for encoding \"UTF8\": 0x00",
                "22021",
                None,
                None,
            ),
        ] {
            let error = Lexer::new(sql, &arena).next_token().unwrap_err();
            assert_eq!(error.render(sql).as_str(), message, "{sql}");
            assert_eq!(error.kind.sqlstate(), state, "{sql}");
            assert_eq!(error.cursor(), cursor, "{sql}");
            assert_eq!(error.hint, hint, "{sql}");
        }
    }

    #[test]
    fn identifiers_follow_postgresql_character_classes_and_ascii_folding() {
        assert_eq!(
            lex_all("ÄB straße ‘x’ $tæg$body$tæg$"),
            [
                "Ident(\"Äb\")",
                "Ident(\"straße\")",
                "Ident(\"‘x’\")",
                "Str(\"body\")"
            ]
        );
        // Only PostgreSQL's ASCII spaces separate tokens; NBSP is a letter.
        assert_eq!(
            lex_all("a\u{a0}b\x0Bc"),
            ["Ident(\"a\\u{a0}b\")", "Ident(\"c\")"]
        );
        let long = "A".repeat(200);
        assert_eq!(lex_all(&long), [format!("Ident({:?})", "a".repeat(200))]);
    }

    #[test]
    fn string_segments_separated_by_a_newline_concatenate() {
        // PostgreSQL 18.6 joins these into one constant.
        for (sql, expected) in [
            ("'a'\n'b'", "Str(\"ab\")"),
            ("'a' -- note\n  'b'", "Str(\"ab\")"),
            ("'it''s'\r\n\x0B'x'", "Str(\"it'sx\")"),
            ("E'\\n'\n'\\t'", "Str(\"\\n\\t\")"),
            ("B'10'\n'01'", "Bit(\"B1001\")"),
            ("'é'\n'ü'", "Str(\"éü\")"),
        ] {
            assert_eq!(lex_all(sql), [expected], "{sql:?}");
        }
        // Without a newline, or across a block comment, they stay separate.
        assert_eq!(lex_all("'a' 'b'"), ["Str(\"a\")", "Str(\"b\")"]);
        assert_eq!(lex_all("'a'/* c */\n'b'"), ["Str(\"a\")", "Str(\"b\")"]);
        assert_eq!(lex_all("'a'\x0B\n'b'"), ["Str(\"a\")", "Str(\"b\")"]);
    }

    #[test]
    fn bit_string_literals() {
        // The prefix survives lexing so invalid digits fail at type input.
        assert_eq!(lex_all("B'1010'"), ["Bit(\"B1010\")"]);
        assert_eq!(lex_all("x'1f'"), ["Bit(\"X1f\")"]);
        assert_eq!(lex_all("B''"), ["Bit(\"B\")"]);
        assert_eq!(lex_all("X'A'"), ["Bit(\"XA\")"]);
        // A bare identifier starting with b/x is unaffected.
        assert_eq!(lex_all("bit box"), ["Ident(\"bit\")", "Ident(\"box\")"]);
    }

    #[test]
    fn numbers() {
        assert_eq!(
            lex_all("1 .5 2.5 1e3 1.5e-2"),
            [
                "Num(\"1\")",
                "Num(\".5\")",
                "Num(\"2.5\")",
                "Num(\"1e3\")",
                "Num(\"1.5e-2\")"
            ]
        );
        assert_eq!(
            lex_all("1. 1.e5 1_000.000_1 .5_5 1e1_0 0x_1F 0O17 0b1_0 1..2"),
            [
                "Num(\"1.\")",
                "Num(\"1.e5\")",
                "Num(\"1_000.000_1\")",
                "Num(\".5_5\")",
                "Num(\"1e1_0\")",
                "Num(\"0x_1F\")",
                "Num(\"0O17\")",
                "Num(\"0b1_0\")",
                "Num(\"1\")",
                "Op(\"..\")",
                "Num(\"2\")"
            ]
        );
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "numeric junk", 1 << 16).unwrap();
        // Longest-match outcomes from PostgreSQL 18.6's scanner.
        for (sql, message) in [
            (
                "1.x",
                "trailing junk after numeric literal at or near \"1.x\"",
            ),
            (
                "1_",
                "trailing junk after numeric literal at or near \"1_\"",
            ),
            (
                "1__0",
                "trailing junk after numeric literal at or near \"1__0\"",
            ),
            (
                "1._5",
                "trailing junk after numeric literal at or near \"1._5\"",
            ),
            (
                "1.5_e3",
                "trailing junk after numeric literal at or near \"1.5_e3\"",
            ),
            (
                "1.5e+",
                "trailing junk after numeric literal at or near \"1.5e+\"",
            ),
            (
                "1e",
                "trailing junk after numeric literal at or near \"1e\"",
            ),
            (
                "0b2",
                "trailing junk after numeric literal at or near \"0b2\"",
            ),
            (
                "0x1Fg",
                "trailing junk after numeric literal at or near \"0x1Fg\"",
            ),
            ("0x", "invalid hexadecimal integer at or near \"0x\""),
            ("0x_", "invalid hexadecimal integer at or near \"0x_\""),
            ("0o", "invalid octal integer at or near \"0o\""),
            ("0B_", "invalid binary integer at or near \"0B_\""),
            (
                "$1_0x",
                "trailing junk after parameter at or near \"$1_0x\"",
            ),
            (
                "$2147483648",
                "parameter number too large at or near \"$2147483648\"",
            ),
        ] {
            let error = Lexer::new(sql, &arena).next_token().unwrap_err();
            assert_eq!(error.render(sql).as_str(), message, "{sql}");
            assert_eq!(error.cursor(), Some(0), "{sql}");
        }
        assert_eq!(
            lex_all("$1_0 $2147483647"),
            ["Param(10)", "Param(2147483647)"]
        );
    }

    #[test]
    fn comments_and_operators() {
        assert_eq!(
            lex_all("1 -- line\n + /* multi /* nested */ */ 2 <> 3 :: || <="),
            [
                "Num(\"1\")",
                "Op(\"+\")",
                "Num(\"2\")",
                "Op(\"<>\")",
                "Num(\"3\")",
                "Op(\"::\")",
                "Op(\"||\")",
                "Op(\"<=\")"
            ]
        );
        assert_eq!(
            lex_all("1=/*comment*/1+--line\n2"),
            [
                "Num(\"1\")",
                "Op(\"=\")",
                "Num(\"1\")",
                "Op(\"+\")",
                "Num(\"2\")"
            ]
        );
    }

    #[test]
    fn params() {
        assert_eq!(lex_all("$1 $23"), ["Param(1)", "Param(23)"]);
    }

    #[test]
    fn errors() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "test", 1 << 16).unwrap();
        assert!(Lexer::new("'unterminated", &arena).next_token().is_err());
        assert!(Lexer::new("\"\"", &arena).next_token().is_err());
        assert!(Lexer::new("/* open", &arena).next_token().is_err());
    }
}
