//! PostgreSQL's JSON lexer (`json_lex` in `src/common/jsonapi.c`) and the
//! errors its recursive-descent parser reports: `json_errdetail`'s DETAIL and
//! `report_json_context`'s `JSON data, line N:` CONTEXT
//! (`src/backend/utils/adt/jsonfuncs.c`).
//!
//! The lexer keeps the same positions as PostgreSQL's (the current token's
//! start and terminator, the line number and the line's start), because the
//! DETAIL quotes the token and the CONTEXT windows the line around it.

use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Token {
    /// No token has been lexed yet.
    Invalid,
    String,
    Number,
    ObjectStart,
    ObjectEnd,
    ArrayStart,
    ArrayEnd,
    Comma,
    Colon,
    True,
    False,
    Null,
    End,
}

/// `JsonParseErrorType`, for the errors a complete input can raise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ErrorKind {
    EscapingInvalid,
    EscapingRequired,
    ExpectedEnd,
    ExpectedArrayNext,
    ExpectedColon,
    ExpectedJson,
    ExpectedMore,
    ExpectedObjectFirst,
    ExpectedObjectNext,
    ExpectedString,
    InvalidToken,
    UnicodeCodePointZero,
    UnicodeEscapeFormat,
    UnicodeHighSurrogate,
    UnicodeLowSurrogate,
}

/// What the parser expected where it found something else
/// (`JsonParseContext`).
#[derive(Clone, Copy)]
pub(super) enum Expecting {
    End,
    Value,
    String,
    ArrayNext,
    ObjectStart,
    ObjectLabel,
    ObjectNext,
}

/// A JSON syntax error with the lexer state PostgreSQL reports it from.
#[derive(Clone, Copy)]
pub(crate) struct SyntaxError<'a> {
    kind: ErrorKind,
    at: Lexer<'a>,
}

#[derive(Clone, Copy)]
pub(super) struct Lexer<'a> {
    input: &'a [u8],
    /// Where the current token starts; `None` at the end of the input.
    token_start: Option<usize>,
    token_terminator: usize,
    /// Where the token before the current one ended.
    previous_terminator: usize,
    token: Token,
    line_number: usize,
    line_start: usize,
    /// Whether string escapes are checked as they would be decoded:
    /// surrogate pairs and `\u0000`. The `json` type's input does not.
    need_escapes: bool,
}

/// PostgreSQL's `JSON_ALPHANUMERIC_CHAR`: the bytes an unexpected word is
/// reported with.
fn alphanumeric(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

/// The length of the UTF-8 character `byte` starts.
fn character_length(byte: u8) -> usize {
    match byte {
        0xF0..=0xFF => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

impl<'a> Lexer<'a> {
    pub(super) fn new(input: &'a str, need_escapes: bool) -> Self {
        Self {
            input: input.as_bytes(),
            token_start: Some(0),
            token_terminator: 0,
            previous_terminator: 0,
            token: Token::Invalid,
            line_number: 1,
            line_start: 0,
            need_escapes,
        }
    }

    pub(super) fn token(&self) -> Token {
        self.token
    }

    /// The current token's source text.
    pub(super) fn text(&self) -> &'a str {
        let start = self.token_start.unwrap_or(self.token_terminator);
        // Tokens start and end at character boundaries of UTF-8 text.
        unsafe { core::str::from_utf8_unchecked(&self.input[start..self.token_terminator]) }
    }

    /// The whole input.
    pub(super) fn input_text(&self) -> &'a str {
        // The lexer is built from UTF-8 text.
        unsafe { core::str::from_utf8_unchecked(self.input) }
    }

    /// Where the current token starts in the input.
    pub(super) fn start(&self) -> usize {
        self.token_start.unwrap_or(self.token_terminator)
    }

    /// Where the token before the current one ended.
    pub(super) fn previous_end(&self) -> usize {
        self.previous_terminator
    }

    fn fail(&self, kind: ErrorKind) -> SyntaxError<'a> {
        SyntaxError { kind, at: *self }
    }

    /// `report_parse_error`: what was found where `expecting` was due.
    pub(super) fn unexpected(&self, expecting: Expecting) -> SyntaxError<'a> {
        if self.token_start.is_none() || self.token == Token::End {
            return self.fail(ErrorKind::ExpectedMore);
        }
        self.fail(match expecting {
            Expecting::End => ErrorKind::ExpectedEnd,
            Expecting::Value => ErrorKind::ExpectedJson,
            Expecting::String => ErrorKind::ExpectedString,
            Expecting::ArrayNext => ErrorKind::ExpectedArrayNext,
            Expecting::ObjectStart => ErrorKind::ExpectedObjectFirst,
            Expecting::ObjectLabel => ErrorKind::ExpectedColon,
            Expecting::ObjectNext => ErrorKind::ExpectedObjectNext,
        })
    }

    /// `lex_expect`: moves past the current token if it is `token`.
    pub(super) fn expect(
        &mut self,
        expecting: Expecting,
        token: Token,
    ) -> Result<(), SyntaxError<'a>> {
        if self.token == token {
            self.lex()
        } else {
            Err(self.unexpected(expecting))
        }
    }

    /// `json_lex`: lexes the token after the current one.
    pub(super) fn lex(&mut self) -> Result<(), SyntaxError<'a>> {
        let input = self.input;
        let end = input.len();
        let mut at = self.token_terminator;
        self.previous_terminator = at;
        while at < end && matches!(input[at], b' ' | b'\t' | b'\n' | b'\r') {
            at += 1;
            if input[at - 1] == b'\n' {
                self.line_number += 1;
                self.line_start = at;
            }
        }
        self.token_start = Some(at);
        if at >= end {
            self.token_start = None;
            self.token_terminator = at;
            self.token = Token::End;
            return Ok(());
        }
        let punctuation = match input[at] {
            b'{' => Some(Token::ObjectStart),
            b'}' => Some(Token::ObjectEnd),
            b'[' => Some(Token::ArrayStart),
            b']' => Some(Token::ArrayEnd),
            b',' => Some(Token::Comma),
            b':' => Some(Token::Colon),
            _ => None,
        };
        if let Some(token) = punctuation {
            self.token_terminator = at + 1;
            self.token = token;
            return Ok(());
        }
        match input[at] {
            b'"' => {
                self.string()?;
                self.token = Token::String;
            }
            b'-' => {
                self.number(at + 1)?;
                self.token = Token::Number;
            }
            b'0'..=b'9' => {
                self.number(at)?;
                self.token = Token::Number;
            }
            _ => {
                // Only true, false and null are words, but the whole word
                // is reported rather than a prefix of it.
                let mut word_end = at;
                while word_end < end && alphanumeric(input[word_end]) {
                    word_end += 1;
                }
                if word_end == at {
                    self.token_terminator = at + 1;
                    return Err(self.fail(ErrorKind::InvalidToken));
                }
                self.token_terminator = word_end;
                self.token = match &input[at..word_end] {
                    b"true" => Token::True,
                    b"false" => Token::False,
                    b"null" => Token::Null,
                    _ => return Err(self.fail(ErrorKind::InvalidToken)),
                };
            }
        }
        Ok(())
    }

    /// Fails with the token ending after the character at `at`.
    fn fail_at_character_end(&mut self, at: usize, kind: ErrorKind) -> SyntaxError<'a> {
        self.token_terminator = (at + character_length(self.input[at])).min(self.input.len());
        self.fail(kind)
    }

    /// `json_lex_string`: the current token is the string starting at
    /// `token_start`.
    fn string(&mut self) -> Result<(), SyntaxError<'a>> {
        let input = self.input;
        let end = input.len();
        let mut at = self
            .token_start
            .expect("a string token starts at its quote");
        let mut high_surrogate = None;
        loop {
            at += 1;
            if at >= end {
                self.token_terminator = at;
                return Err(self.fail(ErrorKind::InvalidToken));
            }
            match input[at] {
                b'"' => break,
                b'\\' => {
                    at += 1;
                    if at >= end {
                        self.token_terminator = at;
                        return Err(self.fail(ErrorKind::InvalidToken));
                    }
                    if input[at] == b'u' {
                        let mut code = 0u32;
                        for _ in 0..4 {
                            at += 1;
                            if at >= end {
                                self.token_terminator = at;
                                return Err(self.fail(ErrorKind::InvalidToken));
                            }
                            let Some(digit) = char::from(input[at]).to_digit(16) else {
                                return Err(
                                    self.fail_at_character_end(at, ErrorKind::UnicodeEscapeFormat)
                                );
                            };
                            code = code * 16 + digit;
                        }
                        if !self.need_escapes {
                            continue;
                        }
                        if (0xD800..=0xDBFF).contains(&code) {
                            if high_surrogate.is_some() {
                                return Err(
                                    self.fail_at_character_end(at, ErrorKind::UnicodeHighSurrogate)
                                );
                            }
                            high_surrogate = Some(code);
                            continue;
                        }
                        if (0xDC00..=0xDFFF).contains(&code) {
                            if high_surrogate.take().is_none() {
                                return Err(
                                    self.fail_at_character_end(at, ErrorKind::UnicodeLowSurrogate)
                                );
                            }
                            continue;
                        }
                        if high_surrogate.is_some() {
                            return Err(
                                self.fail_at_character_end(at, ErrorKind::UnicodeLowSurrogate)
                            );
                        }
                        if code == 0 {
                            return Err(
                                self.fail_at_character_end(at, ErrorKind::UnicodeCodePointZero)
                            );
                        }
                    } else {
                        if self.need_escapes && high_surrogate.is_some() {
                            return Err(
                                self.fail_at_character_end(at, ErrorKind::UnicodeLowSurrogate)
                            );
                        }
                        if !matches!(
                            input[at],
                            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                        ) {
                            // Only the escape sequence is reported, not the
                            // whole string.
                            self.token_start = Some(at);
                            return Err(self.fail_at_character_end(at, ErrorKind::EscapingInvalid));
                        }
                    }
                }
                _ => {
                    if self.need_escapes && high_surrogate.is_some() {
                        return Err(self.fail_at_character_end(at, ErrorKind::UnicodeLowSurrogate));
                    }
                    let mut plain = at;
                    while plain < end && !matches!(input[plain], b'\\' | b'"') {
                        if input[plain] <= 31 {
                            // The unprintable character is left out of
                            // the context.
                            self.token_terminator = plain;
                            return Err(self.fail(ErrorKind::EscapingRequired));
                        }
                        plain += 1;
                    }
                    at = plain - 1;
                }
            }
        }
        if high_surrogate.is_some() {
            self.token_terminator = at + 1;
            return Err(self.fail(ErrorKind::UnicodeLowSurrogate));
        }
        self.token_terminator = at + 1;
        Ok(())
    }

    /// `json_lex_number`: the number whose digits start at `at`, after any
    /// minus sign.
    fn number(&mut self, mut at: usize) -> Result<(), SyntaxError<'a>> {
        let input = self.input;
        let end = input.len();
        let digit = |at: usize| at < end && input[at].is_ascii_digit();
        let mut error = false;
        if at < end && input[at] == b'0' {
            at += 1;
        } else if digit(at) {
            while digit(at) {
                at += 1;
            }
        } else {
            error = true;
        }
        if at < end && input[at] == b'.' {
            at += 1;
            if !digit(at) {
                error = true;
            }
            while digit(at) {
                at += 1;
            }
        }
        if at < end && matches!(input[at], b'e' | b'E') {
            at += 1;
            if at < end && matches!(input[at], b'+' | b'-') {
                at += 1;
            }
            if !digit(at) {
                error = true;
            }
            while digit(at) {
                at += 1;
            }
        }
        // Trailing alphanumerics belong to the reported token.
        while at < end && alphanumeric(input[at]) {
            at += 1;
            error = true;
        }
        self.token_terminator = at;
        if error {
            return Err(self.fail(ErrorKind::InvalidToken));
        }
        Ok(())
    }
}

impl SyntaxError<'_> {
    /// The error PostgreSQL raises (`json_errsave_error`), its DETAIL
    /// stashed and its CONTEXT line recorded.
    pub(crate) fn error(&self) -> SqlError {
        let error = if self.kind == ErrorKind::UnicodeCodePointZero {
            sql_err!(
                sqlstate::UNTRANSLATABLE_CHARACTER,
                "unsupported Unicode escape sequence"
            )
        } else {
            sql_err!(
                sqlstate::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type json"
            )
        };
        let error = crate::sql::eval::with_detail(error, format_args!("{}", Detail(self)));
        crate::sql::error_context::with_line(error, format_args!("{}", Context(self)))
    }
}

/// `json_errdetail`.
struct Detail<'e, 'a>(&'e SyntaxError<'a>);

impl core::fmt::Display for Detail<'_, '_> {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let at = &self.0.at;
        let token = at.text();
        match self.0.kind {
            ErrorKind::EscapingInvalid => write!(out, "Escape sequence \"\\{token}\" is invalid."),
            ErrorKind::EscapingRequired => write!(
                out,
                "Character with value 0x{:02x} must be escaped.",
                at.input[at.token_terminator]
            ),
            ErrorKind::ExpectedEnd => write!(out, "Expected end of input, but found \"{token}\"."),
            ErrorKind::ExpectedArrayNext => {
                write!(out, "Expected \",\" or \"]\", but found \"{token}\".")
            }
            ErrorKind::ExpectedColon => write!(out, "Expected \":\", but found \"{token}\"."),
            ErrorKind::ExpectedJson => write!(out, "Expected JSON value, but found \"{token}\"."),
            ErrorKind::ExpectedMore => out.write_str("The input string ended unexpectedly."),
            ErrorKind::ExpectedObjectFirst => {
                write!(out, "Expected string or \"}}\", but found \"{token}\".")
            }
            ErrorKind::ExpectedObjectNext => {
                write!(out, "Expected \",\" or \"}}\", but found \"{token}\".")
            }
            ErrorKind::ExpectedString => write!(out, "Expected string, but found \"{token}\"."),
            ErrorKind::InvalidToken => write!(out, "Token \"{token}\" is invalid."),
            ErrorKind::UnicodeCodePointZero => {
                out.write_str("\\u0000 cannot be converted to text.")
            }
            ErrorKind::UnicodeEscapeFormat => {
                out.write_str("\"\\u\" must be followed by four hexadecimal digits.")
            }
            ErrorKind::UnicodeHighSurrogate => {
                out.write_str("Unicode high surrogate must not follow a high surrogate.")
            }
            ErrorKind::UnicodeLowSurrogate => {
                out.write_str("Unicode low surrogate must follow a high surrogate.")
            }
        }
    }
}

/// `report_json_context`: the error's line up to where it was detected,
/// cut to the last 50 bytes or so.
struct Context<'e, 'a>(&'e SyntaxError<'a>);

impl core::fmt::Display for Context<'_, '_> {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let at = &self.0.at;
        let input = at.input;
        let line_start = at.line_start;
        let context_end = at.token_terminator;
        let mut context_start = line_start;
        while context_end - context_start >= 50 {
            context_start +=
                character_length(input[context_start]).min(context_end - context_start);
        }
        // Within three bytes of the line's start the whole line is shown.
        if context_start - line_start <= 3 {
            context_start = line_start;
        }
        let prefix = if context_start > line_start {
            "..."
        } else {
            ""
        };
        let suffix = if at.token != Token::End
            && context_end < input.len()
            && !matches!(input[context_end], b'\n' | b'\r')
        {
            "..."
        } else {
            ""
        };
        // Both ends are character boundaries of UTF-8 text.
        let text = unsafe { core::str::from_utf8_unchecked(&input[context_start..context_end]) };
        write!(
            out,
            "JSON data, line {}: {prefix}{text}{suffix}",
            at.line_number
        )
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    fn failure(input: &str, need_escapes: bool) -> (String, String) {
        let mut parser = super::super::Parser::new(input, need_escapes, None, false);
        let Err(super::super::Failure::Syntax(error)) = parser.document() else {
            panic!("{input:?} is not a JSON syntax error");
        };
        (Detail(&error).to_string(), Context(&error).to_string())
    }

    #[test]
    fn details_quote_the_token_postgresql_quotes() {
        assert_eq!(
            failure("{\"a\":1 \"b\"}", true),
            (
                "Expected \",\" or \"}\", but found \"\"b\"\".".to_string(),
                "JSON data, line 1: {\"a\":1 \"b\"...".to_string()
            )
        );
        assert_eq!(
            failure("\"\\x\"", false).0,
            "Escape sequence \"\\x\" is invalid."
        );
        assert_eq!(
            failure("\"a\u{1}\"", false).0,
            "Character with value 0x01 must be escaped."
        );
        assert_eq!(failure("", true).1, "JSON data, line 1: ");
        // `json` input leaves escapes undecoded; `jsonb` checks them.
        assert!(
            super::super::Parser::new("\"\\ud800\"", false, None, false)
                .document()
                .is_ok()
        );
        assert_eq!(
            failure("\"\\ud800\"", true).0,
            "Unicode low surrogate must follow a high surrogate."
        );
    }

    #[test]
    fn the_context_windows_the_line_up_to_the_error() {
        let input = "{\"a\":\n 1, \"b\": \n [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20 x]}";
        assert_eq!(
            failure(input, false).1,
            "JSON data, line 3: ..., 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20 x..."
        );
        // Within three bytes of the line's start the whole line is shown.
        let near = format!("[{}x]", "1,".repeat(25));
        assert_eq!(
            failure(&near, false).1,
            format!("JSON data, line 1: {}...", &near[..near.len() - 1])
        );
    }
}
