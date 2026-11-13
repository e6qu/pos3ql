//! PostgreSQL's default text search parser, ported from PostgreSQL 18.6's
//! [`wparser_def.c`](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tsearch/wparser_def.c).
//!
//! The parser is a table-driven state machine. `tables.rs` is generated from
//! the C tables by `tools/generate-text-parser-tables.py`; this module ports
//! the interpreter (`TParserGet`), its character tests and special handlers,
//! and `headline.rs` ports the `ts_headline` support (`prsd_headline` and the
//! `ts_parse.c` headline framework).
//!
//! pos3ql databases use the C ctype with UTF-8, so the character tests follow
//! PostgreSQL's C-locale wide-character branch: every non-ASCII character is
//! a letter (`alpha` and `alnum`) and belongs to no other class.

mod headline;
mod tables;

pub(crate) use headline::{HeadlineOptions, HeadlineState, headline, parse_headline_options};
pub(crate) use tables::TOKEN_TYPES;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// `MAXSTRLEN`: document parsing skips, with a NOTICE, a token this long.
pub(crate) const MAX_PARSED_WORD_BYTES: usize = 2_047;

/// The NOTICE for a token of at least [`MAX_PARSED_WORD_BYTES`] bytes.
pub(crate) fn long_word_notice() -> Result<(), SqlError> {
    crate::sql::eval::raise_eval_message_with_detail(
        crate::sql::eval::EvalMessageLevel::Notice,
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        format_args!("word is too long to be indexed"),
        "Words longer than 2047 characters are ignored.",
    )
}

/// A `p_is*` character test from `wparser_def.c`.
#[derive(Clone, Copy)]
pub(super) enum Class {
    Eof,
    Eq,
    Ignore,
    AsciiLetter,
    Alpha,
    Alnum,
    NotAlnum,
    Digit,
    HexDigit,
    Space,
    Special,
    StopHost,
    Host,
    UrlPath,
    UrlChar,
    /// The `NULL` test that ends every action list.
    Always,
}

/// A `Special*` handler from `wparser_def.c`.
#[derive(Clone, Copy)]
pub(super) enum Handler {
    None,
    Tags,
    FullUrl,
    Hyphen,
    VerVersion,
}

/// One `TParserStateActionItem`.
#[derive(Clone, Copy)]
pub(super) struct Action {
    class: Class,
    c: u8,
    flags: u8,
    to_state: u8,
    token_type: u8,
    handler: Handler,
}

const A_BINGO: u8 = 0x01;
const A_POP: u8 = 0x02;
const A_PUSH: u8 = 0x04;
const A_RERUN: u8 = 0x08;
const A_CLEAR: u8 = 0x10;
const A_MERGE: u8 = 0x20;
const A_CLRALL: u8 = 0x40;

/// `TParserPosition`. Byte positions are absolute offsets into the document
/// because a copied parser shares its original's string end.
#[derive(Clone, Copy)]
struct Position {
    byte: usize,
    char_len: usize,
    token_bytes: usize,
    token_chars: usize,
    state: u8,
    pushed_at: Option<u16>,
}

impl Position {
    const fn start(byte: usize, state: u8) -> Self {
        Self {
            byte,
            char_len: 0,
            token_bytes: 0,
            token_chars: 0,
            state,
            pushed_at: None,
        }
    }
}

/// One `TParser`: its current position plus the part of the shared position
/// stack above `base`. Copied parsers (`TParserCopyInit`) run to completion
/// before their original continues, so they stack on the same list.
struct Frame {
    current: Position,
    base: usize,
    ignore: bool,
    want_host: bool,
    token: usize,
    token_bytes: usize,
    token_chars: usize,
    token_type: u8,
}

impl Frame {
    const fn new(current: Position, base: usize) -> Self {
        Self {
            current,
            base,
            ignore: false,
            want_host: false,
            token: 0,
            token_bytes: 0,
            token_chars: 0,
            token_type: 0,
        }
    }
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "text-search parser state exceeds the statement arena"
    )
}

/// The default parser over one document (`prsd_start`/`prsd_nexttoken`).
pub(crate) struct TextParser<'d, 'a> {
    document: &'d str,
    stack: ArenaList<'a, Position>,
    main: Frame,
}

impl<'d, 'a> TextParser<'d, 'a> {
    pub(crate) fn new(document: &'d str, arena: &'a Arena) -> Self {
        Self {
            document,
            stack: ArenaList::new(arena),
            main: Frame::new(Position::start(0, tables::BASE), 0),
        }
    }

    /// The next token and its type, or `None` at the end of the document.
    pub(crate) fn next_token(&mut self) -> Result<Option<(u8, &'d str)>, SqlError> {
        let mut frame = Frame::new(self.main.current, 0);
        frame.ignore = self.main.ignore;
        frame.want_host = self.main.want_host;
        let found = self.get(&mut frame)?;
        self.main = frame;
        if !found {
            return Ok(None);
        }
        let start = self.main.token;
        Ok(Some((
            self.main.token_type,
            &self.document[start..start + self.main.token_bytes],
        )))
    }

    fn byte(&self, at: usize) -> u8 {
        self.document.as_bytes()[at]
    }

    /// The current character as `pg_wchar`, or 0 at the end of the string,
    /// where PostgreSQL reads the wide string's terminator.
    fn wide(&self, position: &Position) -> u32 {
        self.document[position.byte..]
            .chars()
            .next()
            .map_or(0, u32::from)
    }

    /// `p_iswhat`'s C-locale wide-character branch: a non-ASCII character
    /// answers `non_ascii`, an ASCII one the C `is*` function.
    fn class_test(&self, position: &Position, non_ascii: bool, ascii: fn(&u8) -> bool) -> bool {
        let c = self.wide(position);
        if c > 0x7f {
            return non_ascii;
        }
        ascii(&(c as u8))
    }

    fn alnum(&self, position: &Position) -> bool {
        self.class_test(position, true, u8::is_ascii_alphanumeric)
    }

    fn test(&mut self, frame: &mut Frame, action: Action) -> Result<bool, SqlError> {
        let position = frame.current;
        Ok(match action.class {
            Class::Eof => position.byte == self.document.len() || position.char_len == 0,
            Class::Eq => position.char_len == 1 && self.byte(position.byte) == action.c,
            Class::Ignore => frame.ignore,
            Class::AsciiLetter => {
                position.char_len == 1 && self.byte(position.byte).is_ascii_alphabetic()
            }
            Class::Alpha => self.class_test(&position, true, u8::is_ascii_alphabetic),
            Class::Alnum => self.alnum(&position),
            Class::NotAlnum => !self.alnum(&position),
            Class::Digit => self.class_test(&position, false, u8::is_ascii_digit),
            Class::HexDigit => self.class_test(&position, false, u8::is_ascii_hexdigit),
            // C `isspace`: space, \t, \n, \v, \f and \r.
            Class::Space => self.class_test(&position, false, |c| {
                matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
            }),
            Class::Special => special(self.wide(&position)),
            Class::StopHost => core::mem::take(&mut frame.want_host),
            Class::Host => self.sub_parse(frame, false)?,
            Class::UrlPath => self.sub_parse(frame, true)?,
            Class::UrlChar => {
                position.char_len == 1
                    && matches!(self.byte(position.byte), 0x21..=0x7e)
                    && !matches!(
                        self.byte(position.byte),
                        b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}'
                    )
            }
            Class::Always => true,
        })
    }

    /// `p_ishost` (`url_path == false`) and `p_isURLPath`: run a copy of the
    /// parser from the current position and, if it finds a host or URL path
    /// token, absorb that token into the current one.
    fn sub_parse(&mut self, frame: &mut Frame, url_path: bool) -> Result<bool, SqlError> {
        let base = self.stack.len();
        let start = Position::start(frame.current.byte, tables::BASE);
        let mut copy = if url_path {
            self.stack.push(start).map_err(|_| arena_full())?;
            Frame::new(
                Position {
                    state: tables::IN_URL_PATH_FIRST,
                    ..start
                },
                base,
            )
        } else {
            Frame::new(start, base)
        };
        copy.want_host = !url_path;
        let wanted = if url_path {
            tables::URLPATH
        } else {
            tables::HOST
        };
        let found = self.get(&mut copy)? && copy.token_type == wanted;
        if found {
            let current = &mut frame.current;
            current.byte += copy.token_bytes;
            current.token_bytes += copy.token_bytes;
            current.token_chars += copy.token_chars;
            current.char_len = copy.current.char_len;
        }
        self.stack.truncate(base);
        Ok(found)
    }

    fn pop(&mut self, frame: &Frame) -> Position {
        let len = self.stack.len();
        assert!(len > frame.base, "text-search parser stack underflow");
        let top = self.stack.borrowed_slice()[len - 1];
        self.stack.truncate(len - 1);
        top
    }

    fn handle(&self, frame: &mut Frame, handler: Handler) {
        let current = &mut frame.current;
        match handler {
            Handler::None => {}
            Handler::Tags => {
                let token = &self.document.as_bytes()[frame.token..];
                let starts = |prefix: &[u8]| {
                    token.len() >= prefix.len()
                        && token[..prefix.len()].eq_ignore_ascii_case(prefix)
                };
                match current.token_chars {
                    8 if starts(b"</script") => frame.ignore = false,
                    7 if starts(b"</style") => frame.ignore = false,
                    7 if starts(b"<script") => frame.ignore = true,
                    6 if starts(b"<style") => frame.ignore = true,
                    _ => {}
                }
            }
            Handler::FullUrl | Handler::Hyphen | Handler::VerVersion => {
                if matches!(handler, Handler::FullUrl) {
                    frame.want_host = true;
                }
                current.byte -= current.token_bytes;
                if matches!(handler, Handler::VerVersion) {
                    current.token_bytes = 0;
                    current.token_chars = 0;
                }
            }
        }
    }

    /// `TParserGet`: advances `frame` to its next token.
    fn get(&mut self, frame: &mut Frame) -> Result<bool, SqlError> {
        let end = self.document.len();
        if frame.current.byte >= end {
            return Ok(false);
        }
        frame.token = frame.current.byte;
        frame.current.pushed_at = None;
        let mut flags = 0;
        while frame.current.byte <= end {
            frame.current.char_len = if frame.current.byte == end {
                0
            } else {
                utf8_len(self.byte(frame.current.byte))
            };
            let mut index = match frame.current.pushed_at.take() {
                // After a POP, pick up at the next test.
                Some(pushed) => usize::from(pushed) + 1,
                None => usize::from(tables::STATE_ACTIONS[usize::from(frame.current.state)]),
            };
            while !matches!(tables::ACTIONS[index].class, Class::Always)
                && !self.test(frame, tables::ACTIONS[index])?
            {
                index += 1;
            }
            let action = tables::ACTIONS[index];
            flags = action.flags;
            self.handle(frame, action.handler);
            if flags & A_BINGO != 0 {
                frame.token_bytes = frame.current.token_bytes;
                frame.token_chars = frame.current.token_chars;
                frame.current.token_bytes = 0;
                frame.current.token_chars = 0;
                frame.token_type = action.token_type;
            }
            if flags & A_POP != 0 {
                frame.current = self.pop(frame);
            } else if flags & A_PUSH != 0 {
                frame.current.pushed_at = Some(index as u16);
                self.stack.push(frame.current).map_err(|_| arena_full())?;
                frame.current.pushed_at = None;
            } else if flags & A_CLEAR != 0 {
                let _ = self.pop(frame);
            } else if flags & A_CLRALL != 0 {
                self.stack.truncate(frame.base);
            } else if flags & A_MERGE != 0 {
                let merged = frame.current;
                frame.current = Position {
                    byte: merged.byte,
                    char_len: merged.char_len,
                    token_bytes: merged.token_bytes,
                    token_chars: merged.token_chars,
                    ..self.pop(frame)
                };
            }
            if action.to_state != tables::NULL_STATE {
                frame.current.state = action.to_state;
            }
            if flags & A_BINGO != 0 || (frame.current.byte >= end && flags & A_RERUN == 0) {
                break;
            }
            if flags & (A_RERUN | A_POP) != 0 {
                continue;
            }
            if frame.current.char_len != 0 {
                frame.current.byte += frame.current.char_len;
                frame.current.token_bytes += frame.current.char_len;
                frame.current.token_chars += 1;
            }
        }
        Ok(flags & A_BINGO != 0)
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// `p_isspecial`: a zero-width character (`pg_dsplen` is 0) or one of the
/// spacing combining marks PostgreSQL lists as `strange_letter`.
fn special(c: u32) -> bool {
    c == 0
        || tables::NONSPACING
            .binary_search_by(|&(low, high)| {
                if high < c {
                    core::cmp::Ordering::Less
                } else if low > c {
                    core::cmp::Ordering::Greater
                } else {
                    core::cmp::Ordering::Equal
                }
            })
            .is_ok()
        || tables::STRANGE_LETTERS.binary_search(&c).is_ok()
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::mem::Budget;
    use crate::sql::full_text::TextSearchLexeme;

    fn arena() -> Arena {
        let mut budget = Budget::new(8 * 1024 * 1024);
        Arena::new(&mut budget, "text parser test", 4 * 1024 * 1024).unwrap()
    }

    #[test]
    fn tokens_match_postgresql_default_parser() {
        let arena = arena();
        let mut parser = TextParser::new(
            "http://a.b/c?d 1.2.3 x-1 foo@bar.com <b>e&amp;</b> naïve 2e5",
            &arena,
        );
        let mut tokens = String::new();
        while let Some((token_type, token)) = parser.next_token().unwrap() {
            if !tokens.is_empty() {
                tokens.push('|');
            }
            tokens.push_str(&format!("{token_type}:{token}"));
        }
        // PostgreSQL 18.6: SELECT string_agg(tokid || ':' || token, '|')
        // FROM ts_parse('default', ...).
        assert_eq!(
            tokens,
            "14:http://|19:a.b/c|12:?|1:d|12: |8:1.2.3|12: |1:x|21:-1|12: |\
             4:foo@bar.com|12: |13:<b>|1:e|23:&amp;|13:</b>|12: |2:naïve|12: |7:2e5"
        );
    }

    #[test]
    fn headline_selects_and_marks_like_postgresql() {
        let arena = arena();
        fn simple<'a>(
            _: u8,
            token: &str,
            arena: &'a Arena,
        ) -> Result<TextSearchLexeme<'a>, SqlError> {
            Ok(TextSearchLexeme::Lexeme(arena.alloc_str(token).unwrap()))
        }
        let options = headline::parse_headline_options(
            "MaxFragments=2, MaxWords=3, MinWords=1, StartSel=[, StopSel=]",
            &arena,
        )
        .unwrap();
        let text = headline::headline(
            "one two three fox four five six seven eight nine fox ten",
            "'fox'",
            HeadlineOptions::Parsed(options),
            &mut HeadlineState::default(),
            &arena,
            simple,
        )
        .unwrap();
        assert_eq!(text, "three [fox] four ... nine [fox]");
        let Err(error) = headline::parse_headline_options("MaxWords 5", &arena) else {
            panic!("a key without = is malformed");
        };
        assert_eq!(
            error.message.as_str(),
            "invalid parameter list format: \"MaxWords 5\""
        );
    }
}
