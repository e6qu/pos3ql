//! SQL lexer following PostgreSQL's lexical rules
//! (<https://www.postgresql.org/docs/18/sql-syntax-lexical.html>):
//! case-folding of unquoted identifiers to lower case, `"quoted"`
//! identifiers with `""` escapes, `'strings'` with `''` escapes, `E'...'`
//! backslash strings, `$tag$ ... $tag$` dollar quoting, `--` and nested
//! `/* */` comments, `$n` parameters, and `::` casts.
//!
//! Token text borrows from the query where possible and from the
//! per-statement arena when unescaping or case-folding had to copy.

use crate::mem::arena::{Arena, ArenaFull};
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
    /// `$n` parameter placeholder, 1-based.
    Param(u32),
    /// Operator or punctuation: `+ - * / % ^ = <> != < > <= >= || :: . ( ) , ;`
    Op(&'a str),
    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    /// Byte offset in the query.
    pub at: usize,
    pub message: StackStrMsg,
}

pub type StackStrMsg = &'static str;

#[derive(Clone)]
pub struct Lexer<'a> {
    text: &'a str,
    arena: &'a Arena,
    at: usize,
    token_start: usize,
}

impl<'a> Lexer<'a> {
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

    pub fn next_token(&mut self) -> Result<Tok<'a>, LexError> {
        self.skip_ws_and_comments()?;
        self.token_start = self.at;
        let rest = self.rest();
        let Some(c) = rest.chars().next() else {
            return Ok(Tok::Eof);
        };
        match c {
            '(' | ')' | ',' | ';' | '+' | '*' | '%' | '^' | '.' | '[' | ']' => {
                let operator = &self.text[self.at..self.at + 1];
                self.at += 1;
                Ok(Tok::Op(operator))
            }
            '-' => {
                // JSON accessors `->`/`->>`, range adjacency `-|-`, else minus.
                let rest = self.rest();
                let operator = if rest.starts_with("->>") {
                    "->>"
                } else if rest.starts_with("->") {
                    "->"
                } else if rest.starts_with("-|-") {
                    "-|-"
                } else {
                    "-"
                };
                self.at += operator.len();
                Ok(Tok::Op(&self.text[self.at - operator.len()..self.at]))
            }
            '/' => {
                // Comment starters were consumed above, so this is an operator.
                let operator = &self.text[self.at..self.at + 1];
                self.at += 1;
                Ok(Tok::Op(operator))
            }
            ':' => {
                if rest.starts_with("::") {
                    self.at += 2;
                    Ok(Tok::Op("::"))
                } else {
                    Err(self.error("unexpected ':'"))
                }
            }
            '<' | '>' | '=' | '!' | '|' | '~' | '&' | '#' | '@' => self.operator(),
            '\'' => self.plain_string(),
            '"' => self.quoted_ident(),
            '$' => self.dollar(),
            '0'..='9' => self.number(),
            _ if c == '_' || c.is_alphabetic() => self.ident(),
            _ => Err(self.error("unexpected character")),
        }
    }

    fn operator(&mut self) -> Result<Tok<'a>, LexError> {
        let rest = self.rest();
        // Longer operators first: the POSIX regex match family before `~`.
        for operator in [
            "!~*", "!~", "~*", "<=", ">=", "<>", "!=", "=>", "||", "<<", ">>", "@>", "<@", "&<",
            "&>", "&&", "<", ">", "=", "~", "|", "&", "#", "^",
        ] {
            if rest.starts_with(operator) {
                self.at += operator.len();
                return Ok(Tok::Op(&self.text[self.at - operator.len()..self.at]));
            }
        }
        Err(self.error("unknown operator"))
    }

    fn ident(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        let rest = self.rest();

        // E'...' escape-string syntax.
        if (rest.starts_with('e') || rest.starts_with('E'))
            && rest[1..].starts_with('\'')
        {
            self.at += 1;
            return self.escape_string();
        }

        let len = rest
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '$'))
            .map_or(rest.len(), |(i, _)| i);
        self.at += len;
        let word = &self.text[start..self.at];
        if word.bytes().any(|b| b.is_ascii_uppercase()) {
            let mut folded = StackStr::<128>::new();
            for c in word.chars() {
                let _ = core::fmt::Write::write_fmt(
                    &mut folded,
                    format_args!("{}", c.to_lowercase()),
                );
            }
            if folded.is_truncated() {
                return Err(LexError {
                    at: start,
                    message: "identifier longer than 128 bytes",
                });
            }
            let stored = self.arena_str(folded.as_str(), start)?;
            Ok(Tok::Ident(stored))
        } else {
            Ok(Tok::Ident(word))
        }
    }

    fn number(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        let bytes = self.text.as_bytes();
        let mut i = self.at;
        // Non-decimal integer literals: `0x1F` / `0o17` / `0b1010` (with `_`
        // digit separators). The parser validates the digits.
        if bytes[i] == b'0'
            && i + 1 < bytes.len()
            && matches!(bytes[i + 1], b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
        {
            i += 2;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            self.at = i;
            return Ok(Tok::Num(&self.text[start..i]));
        }
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
        {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                j += 1;
            }
            if j < bytes.len() && bytes[j].is_ascii_digit() {
                i = j;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
        }
        self.at = i;
        Ok(Tok::Num(&self.text[start..i]))
    }

    fn plain_string(&mut self) -> Result<Tok<'a>, LexError> {
        let start = self.at;
        self.at += 1; // opening quote
        let bytes = self.text.as_bytes();
        let mut has_escape = false;
        let mut i = self.at;
        loop {
            match bytes.get(i) {
                None => return Err(LexError { at: start, message: "unterminated string" }),
                Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => {
                    has_escape = true;
                    i += 2;
                }
                Some(b'\'') => break,
                Some(_) => i += 1,
            }
        }
        let raw = &self.text[self.at..i];
        self.at = i + 1;
        if !has_escape {
            return Ok(Tok::Str(raw));
        }
        // Copy into the arena replacing '' with '.
        let out = self
            .arena
            .alloc_slice_copy(raw.as_bytes())
            .map_err(|_| self.arena_full(start))?;
        let mut w = 0;
        let mut r = 0;
        let src = raw.as_bytes();
        while r < src.len() {
            out[w] = src[r];
            if src[r] == b'\'' {
                r += 1; // skip the doubled quote
            }
            r += 1;
            w += 1;
        }
        let out = &mut out[..w];
        Ok(Tok::Str(unsafe { core::str::from_utf8_unchecked(out) }))
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
                None => return Err(LexError { at: start, message: "unterminated string" }),
                Some(b'\'') if bytes.get(i + 1) == Some(&b'\'') => {
                    scratch[w] = b'\'';
                    w += 1;
                    i += 2;
                }
                Some(b'\'') => break,
                Some(b'\\') => {
                    let esc = bytes.get(i + 1).ok_or(LexError {
                        at: start,
                        message: "unterminated string",
                    })?;
                    let replacement = match esc {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        b'b' => 8,
                        b'f' => 12,
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
        core::str::from_utf8(out)
            .map(Tok::Str)
            .map_err(|_| LexError { at: start, message: "invalid UTF-8 after unescaping" })
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
                    return Err(LexError { at: start, message: "unterminated quoted identifier" })
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
            return Err(LexError { at: start, message: "zero-length quoted identifier" });
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
        // $n parameter
        let digits = rest[1..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digits > 0 {
            let n: u32 = rest[1..1 + digits]
                .parse()
                .map_err(|_| LexError { at: start, message: "parameter number too large" })?;
            if n == 0 {
                return Err(LexError { at: start, message: "there is no parameter $0" });
            }
            self.at += 1 + digits;
            return Ok(Tok::Param(n));
        }
        // $tag$ ... $tag$ dollar-quoted string
        let tag_len = rest[1..]
            .char_indices()
            .find(|(_, c)| !(c.is_alphanumeric() || *c == '_'))
            .map_or(rest.len() - 1, |(i, _)| i);
        let after_tag = 1 + tag_len;
        if !rest[after_tag..].starts_with('$') {
            return Err(self.error("unexpected '$'"));
        }
        let open = &rest[..after_tag + 1]; // e.g. "$tag$"
        let body_start = self.at + open.len();
        let Some(close_rel) = self.text[body_start..].find(open) else {
            return Err(LexError { at: start, message: "unterminated dollar-quoted string" });
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
                        return Err(self.error("unterminated block comment"));
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
                if c.is_whitespace() {
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

    fn error(&self, message: &'static str) -> LexError {
        LexError { at: self.at, message }
    }

    fn arena_full(&self, at: usize) -> LexError {
        LexError { at, message: "statement too large for SQL arena" }
    }

    fn arena_str(&self, s: &str, at: usize) -> Result<&'a str, LexError> {
        self.arena.alloc_str(s).map_err(|_: ArenaFull| self.arena_full(at))
    }
}

#[cfg(test)]
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
            ["Ident(\"select\")", "Ident(\"foo\")", "Op(\",\")", "QuotedIdent(\"Bar\")"]
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
    fn numbers() {
        assert_eq!(lex_all("1 2.5 1e3 1.5e-2"), ["Num(\"1\")", "Num(\"2.5\")", "Num(\"1e3\")", "Num(\"1.5e-2\")"]);
        // A trailing dot is not part of the number (it's projection syntax).
        assert_eq!(lex_all("1.x"), ["Num(\"1\")", "Op(\".\")", "Ident(\"x\")"]);
    }

    #[test]
    fn comments_and_operators() {
        assert_eq!(
            lex_all("1 -- line\n + /* multi /* nested */ */ 2 <> 3 :: || <="),
            ["Num(\"1\")", "Op(\"+\")", "Num(\"2\")", "Op(\"<>\")", "Num(\"3\")", "Op(\"::\")", "Op(\"||\")", "Op(\"<=\")"]
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
