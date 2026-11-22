//! ANALYZE's parenthesized option list, read as PostgreSQL's
//! `utility_option_list` reads it and checked as `ExecVacuum` checks it, with
//! the same SQLSTATEs, messages and cursor positions. The checks run when the
//! statement would, so a later syntax error in the statement wins.

use super::copy_options::{Argument, boolean, integer_constant, unpositioned};
use super::{PARSE_MESSAGE_BYTES, ParseError, Parser, Tok};
use crate::sql::ast::AnalyzeOptions;
use crate::sql::eval::sqlstate;
use crate::stack_format;

/// `BUFFER_USAGE_LIMIT`'s accepted sizes besides 0, in kilobytes, as
/// PostgreSQL's `MIN_BAS_VAC_RING_SIZE_KB` and `MAX_BAS_VAC_RING_SIZE_KB`.
const MIN_RING_KB: i32 = 128;
const MAX_RING_KB: i32 = 16 * 1024 * 1024;

/// ANALYZE's options as written: what they select, and the first error
/// `ExecVacuum` would report for them.
pub(super) struct WrittenAnalyzeOptions {
    pub(super) options: AnalyzeOptions,
    pub(super) verbose: bool,
    pub(super) error: Option<ParseError>,
}

impl<'a> Parser<'a> {
    /// The list after `ANALYZE (`, through its closing parenthesis.
    pub(super) fn analyze_options(&mut self) -> Result<WrittenAnalyzeOptions, ParseError> {
        let mut written = WrittenAnalyzeOptions {
            options: AnalyzeOptions { skip_locked: false },
            verbose: false,
            error: None,
        };
        loop {
            let at = self.peek_at;
            // `utility_option_name`: any word but a reserved keyword, where
            // `ANALYSE` spells `analyze`.
            let name = match self.peeked {
                Tok::Ident("analyze" | "analyse") => "analyze",
                Tok::Ident(word) if !super::is_reserved_keyword(word) => word,
                Tok::QuotedIdent(word) => word,
                _ => return Err(self.syntax_error_here()),
            };
            self.advance()?;
            let argument = self.analyze_option_argument()?;
            if written.error.is_none()
                && let Err(error) = Self::analyze_option(&mut written, name, argument, at)
            {
                written.error = Some(error);
            }
            if self.eat_op(")")? {
                return Ok(written);
            }
            self.expect_op(",")?;
        }
    }

    /// PostgreSQL's `utility_option_arg`: a Boolean or string word, a signed
    /// number, or nothing before `,` or `)`.
    fn analyze_option_argument(&mut self) -> Result<Argument<'a>, ParseError> {
        Ok(match self.peeked {
            Tok::Op(",") | Tok::Op(")") => Argument::Absent,
            Tok::Op(sign @ ("+" | "-")) => {
                let Tok::Num(text) = self.lookahead(1)? else {
                    self.advance()?;
                    return Err(self.syntax_error_here());
                };
                self.advance()?;
                self.advance()?;
                let text = self
                    .arena
                    .alloc_str_display(format_args!("{sign}{text}"))
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                if integer_constant(text) {
                    Argument::Integer(text)
                } else {
                    Argument::Number(text)
                }
            }
            Tok::Num(text) => {
                self.advance()?;
                if integer_constant(text) {
                    Argument::Integer(text)
                } else {
                    Argument::Number(text)
                }
            }
            _ => Argument::Text(self.copy_option_word()?),
        })
    }

    /// One option, checked as `ExecVacuum` checks it for ANALYZE.
    fn analyze_option(
        written: &mut WrittenAnalyzeOptions,
        name: &str,
        argument: Argument<'_>,
        at: usize,
    ) -> Result<(), ParseError> {
        let flag = || {
            boolean(argument).ok_or_else(|| {
                unpositioned(
                    sqlstate::SYNTAX_ERROR,
                    format_args!("{name} requires a Boolean value"),
                    at,
                )
            })
        };
        match name {
            "verbose" => written.verbose = flag()?,
            "skip_locked" => written.options.skip_locked = flag()?,
            "buffer_usage_limit" => {
                let text = argument.text().ok_or_else(|| {
                    unpositioned(
                        sqlstate::SYNTAX_ERROR,
                        format_args!("{name} requires a parameter"),
                        at,
                    )
                })?;
                let size = crate::sql::guc::parse_kilobytes(text);
                if !size.is_ok_and(|size| size == 0 || (MIN_RING_KB..=MAX_RING_KB).contains(&size))
                {
                    return Err(ParseError {
                        hint: size.err().flatten(),
                        ..unpositioned(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            format_args!(
                                "BUFFER_USAGE_LIMIT option must be 0 or between {MIN_RING_KB} kB and {MAX_RING_KB} kB"
                            ),
                            at,
                        )
                    });
                }
            }
            _ => {
                return Err(ParseError {
                    at,
                    message: stack_format!(
                        PARSE_MESSAGE_BYTES,
                        "unrecognized ANALYZE option \"{}\"",
                        name
                    ),
                    near: None,
                    detail: None,
                    sqlstate: sqlstate::SYNTAX_ERROR,
                    cursor: Some(at),
                    hint: None,
                });
            }
        }
        Ok(())
    }
}
