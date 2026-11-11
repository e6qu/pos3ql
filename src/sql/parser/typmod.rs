//! Type modifiers (`numeric(10, 2)`, `varchar(20)`): the grammar PostgreSQL
//! accepts after each type spelling, and the per-type `typmodin` checks it
//! applies during parse analysis, with their SQLSTATEs, messages and cursor.

use super::{ParseError, ParseNotice, Parser, Tok};
use crate::sql::eval::sqlstate;
use crate::sql::types::{INTERVAL_FULL_RANGE, IntervalRange, TypeMod};
use crate::stack_format;

/// Arguments converted and checked; PostgreSQL's per-type checks reject any
/// list longer than this before looking at the values.
const CHECKED_ARGUMENTS: usize = 3;

/// A modifier list as written. After a generic type name PostgreSQL's grammar
/// accepts any expressions; parse analysis then requires constants or
/// identifiers and hands their text to the type's `typmodin`.
#[derive(Clone, Copy)]
pub(super) struct WrittenTypmods<'a> {
    arguments: [&'a str; CHECKED_ARGUMENTS],
    count: usize,
    /// False when an argument is not a constant or identifier.
    simple: bool,
    /// Written under a keyword spelling that takes `(Iconst)`, such as
    /// `interval(p)`, which PostgreSQL's grammar turns into a full-range list.
    keyword: bool,
}

/// What a type's `typmodin` expects, by the name the parser resolved.
#[derive(Clone, Copy)]
enum TypmodInput {
    Numeric,
    /// `varchar` or `bpchar`, named as their input functions name them.
    Character(&'static str),
    Bit(&'static str),
    Timestamp {
        zoned: bool,
    },
    Time {
        zoned: bool,
    },
    Interval,
}

impl TypmodInput {
    fn for_type(base: &str) -> Option<Self> {
        Some(match base {
            "numeric" | "decimal" | "dec" => Self::Numeric,
            "varchar" => Self::Character("varchar"),
            "char" | "character" | "bpchar" => Self::Character("char"),
            "bit" => Self::Bit("bit"),
            "varbit" => Self::Bit("varbit"),
            "timestamp" => Self::Timestamp { zoned: false },
            "timestamptz" => Self::Timestamp { zoned: true },
            "time" => Self::Time { zoned: false },
            "timetz" => Self::Time { zoned: true },
            "interval" => Self::Interval,
            _ => return None,
        })
    }
}

/// Spellings whose grammar takes exactly `(Iconst)`; `numeric`, `bit` and
/// generic type names take an expression list instead.
fn keyword_takes_integer_constant(base: &str) -> bool {
    matches!(
        base,
        "varchar" | "char" | "character" | "timestamp" | "time" | "interval"
    )
}

/// PostgreSQL's ICONST: an integer literal that fits `int4`. Larger integers
/// and decimals are FCONST.
fn integer_constant(text: &str) -> Option<i32> {
    crate::sql::eval::parse_int_literal(text).and_then(|value| i32::try_from(value).ok())
}

impl<'a> Parser<'a> {
    /// Parses and checks the modifier list after `base`, a keyword spelling
    /// when `keyword_spelling` is set; `type_at` is where PostgreSQL places
    /// the cursor of its diagnostics, the type name's start when it has one.
    pub(super) fn type_modifier(
        &mut self,
        base: &'a str,
        keyword_spelling: bool,
        type_at: Option<usize>,
    ) -> Result<i32, ParseError> {
        let written =
            self.written_typmods(keyword_spelling && keyword_takes_integer_constant(base))?;
        self.checked_typmod(base, written, type_at, format_args!("{base}"))
    }

    /// `(Iconst)` when `integer_constant_only`, otherwise `(expr, ...)`.
    pub(super) fn written_typmods(
        &mut self,
        integer_constant_only: bool,
    ) -> Result<WrittenTypmods<'a>, ParseError> {
        self.expect_op("(")?;
        let mut written = WrittenTypmods {
            arguments: [""; CHECKED_ARGUMENTS],
            count: 0,
            simple: true,
            keyword: integer_constant_only,
        };
        if integer_constant_only {
            let Tok::Num(text) = self.peeked else {
                return Err(self.syntax_error_here());
            };
            if integer_constant(text).is_none() {
                return Err(self.syntax_error_here());
            }
            self.advance()?;
            self.expect_op(")")?;
            written.arguments[0] = text;
            written.count = 1;
            return Ok(written);
        }
        loop {
            match self.simple_typmod_argument()? {
                Some(text) => {
                    if let Some(slot) = written.arguments.get_mut(written.count) {
                        *slot = text;
                    }
                }
                None => {
                    self.expression(0)?;
                    written.simple = false;
                }
            }
            written.count += 1;
            if !self.eat_op(",")? {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(written)
    }

    /// Consumes a numeric, string or identifier constant (or a negated
    /// number, which PostgreSQL's grammar folds into a constant) that ends the
    /// argument, returning its text; leaves any other expression unconsumed.
    fn simple_typmod_argument(&mut self) -> Result<Option<&'a str>, ParseError> {
        let ends_argument = |token: Tok<'_>| matches!(token, Tok::Op(",") | Tok::Op(")"));
        match self.peeked {
            Tok::Num(text) | Tok::Str(text) | Tok::QuotedIdent(text)
                if ends_argument(self.lookahead(1)?) =>
            {
                self.advance()?;
                Ok(Some(text))
            }
            Tok::Ident(text)
                if super::keyword_category(text).is_none() && ends_argument(self.lookahead(1)?) =>
            {
                self.advance()?;
                Ok(Some(text))
            }
            Tok::Op("-") => {
                let Tok::Num(text) = self.lookahead(1)? else {
                    return Ok(None);
                };
                if !ends_argument(self.lookahead(2)?) {
                    return Ok(None);
                }
                self.advance()?;
                self.advance()?;
                self.arena
                    .alloc_str_display(format_args!("-{text}"))
                    .map(Some)
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))
            }
            _ => Ok(None),
        }
    }

    /// The token `distance` places after the current one, without consuming.
    pub(super) fn lookahead(&self, distance: usize) -> Result<Tok<'a>, ParseError> {
        let mut lexer = self.lexer.clone();
        let mut token = self.peeked;
        for _ in 0..distance {
            if token == Tok::Eof {
                break;
            }
            token = lexer
                .next_token()
                .map_err(|error| ParseError::lexical(error, self.text))?;
        }
        Ok(token)
    }

    /// Applies the checks PostgreSQL's parse analysis makes on `written`
    /// for type `base` (shown as `display`); diagnostics point at `type_at`.
    /// A failure is an analysis error; the result is then unused.
    pub(super) fn checked_typmod(
        &mut self,
        base: &'a str,
        written: WrittenTypmods<'a>,
        type_at: Option<usize>,
        display: core::fmt::Arguments<'_>,
    ) -> Result<i32, ParseError> {
        let at = type_at.unwrap_or(self.peek_at);
        let at_type = |sqlstate: &'static str, message: core::fmt::Arguments<'_>| ParseError {
            at,
            message: stack_format!(super::PARSE_MESSAGE_BYTES, "{}", message),
            sqlstate,
            cursor: type_at,
            hint: None,
            near: None,
            detail: None,
        };
        let Some(input) = TypmodInput::for_type(base) else {
            let error = at_type(
                sqlstate::SYNTAX_ERROR,
                format_args!("type modifier is not allowed for type \"{display}\""),
            );
            if self.analysis_error.is_none() {
                self.analysis_error_type = Some(base);
            }
            return self.analysis_error(error).map(|()| -1);
        };
        if !written.simple {
            let error = at_type(
                sqlstate::SYNTAX_ERROR,
                format_args!("type modifiers must be simple constants or identifiers"),
            );
            return self.analysis_error(error).map(|()| -1);
        }
        let mut values = [0_i32; CHECKED_ARGUMENTS];
        for (value, text) in values
            .iter_mut()
            .zip(&written.arguments[..written.count.min(CHECKED_ARGUMENTS)])
        {
            match crate::sql::eval::parse_int_bounded(
                text,
                i64::from(i32::MIN),
                i64::from(i32::MAX),
                "integer",
            ) {
                Ok(parsed) => *value = parsed as i32,
                Err(error) => {
                    let state = if error.sqlstate == sqlstate::NUMERIC_OUT_OF_RANGE {
                        sqlstate::NUMERIC_OUT_OF_RANGE
                    } else {
                        sqlstate::INVALID_TEXT_REPRESENTATION
                    };
                    let error = at_type(state, format_args!("{}", error.message));
                    return self.analysis_error(error).map(|()| -1);
                }
            }
        }
        let count = written.count;
        let invalid = |message: &'static str| {
            at_type(sqlstate::INVALID_PARAMETER_VALUE, format_args!("{message}"))
        };
        let checked = match input {
            TypmodInput::Numeric => {
                let (precision, scale) = match count {
                    1 => (values[0], 0),
                    2 => (values[0], values[1]),
                    _ => {
                        return self
                            .analysis_error(invalid("invalid NUMERIC type modifier"))
                            .map(|()| -1);
                    }
                };
                if !(1..=1000).contains(&precision) {
                    Err(at_type(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("NUMERIC precision {precision} must be between 1 and 1000"),
                    ))
                } else if !(-1000..=1000).contains(&scale) {
                    Err(at_type(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("NUMERIC scale {scale} must be between -1000 and 1000"),
                    ))
                } else {
                    Ok(TypeMod::NumericPS {
                        precision: precision as u16,
                        scale: scale as i16,
                    }
                    .encode())
                }
            }
            TypmodInput::Character(name) | TypmodInput::Bit(name) => {
                let (maximum, encode): (i32, fn(usize) -> TypeMod) = match input {
                    TypmodInput::Character(_) => (10_485_760, TypeMod::Length),
                    _ => (83_886_080, TypeMod::BitLength),
                };
                if count != 1 {
                    Err(invalid("invalid type modifier"))
                } else if values[0] < 1 {
                    Err(at_type(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("length for type {name} must be at least 1"),
                    ))
                } else if values[0] > maximum {
                    Err(at_type(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("length for type {name} cannot exceed {maximum}"),
                    ))
                } else {
                    Ok(encode(values[0] as usize).encode())
                }
            }
            TypmodInput::Timestamp { zoned } | TypmodInput::Time { zoned } => {
                let name = if matches!(input, TypmodInput::Timestamp { .. }) {
                    "TIMESTAMP"
                } else {
                    "TIME"
                };
                let zone = if zoned { " WITH TIME ZONE" } else { "" };
                let precision = values[0];
                if count != 1 {
                    Err(invalid("invalid type modifier"))
                } else if precision < 0 {
                    Err(at_type(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("{name}({precision}){zone} precision must not be negative"),
                    ))
                } else {
                    if precision > 6 {
                        self.typmod_warning(
                            format_args!(
                                "{name}({precision}){zone} precision reduced to maximum allowed, 6"
                            ),
                            type_at,
                        )?;
                    }
                    Ok(TypeMod::TemporalPrecision(precision.min(6) as u8).encode())
                }
            }
            TypmodInput::Interval => {
                // The grammar turns `interval(p)` into the list (full range, p).
                let (range_mask, precision, count) = if written.keyword {
                    (i32::from(INTERVAL_FULL_RANGE), values[0], 2)
                } else {
                    (values[0], values[1], count)
                };
                let range = u16::try_from(range_mask)
                    .ok()
                    .and_then(IntervalRange::from_mask);
                match (range, count) {
                    (Some(range), 1) => Ok(if range == IntervalRange::Full {
                        -1
                    } else {
                        TypeMod::IntervalMod {
                            range,
                            precision: None,
                        }
                        .encode()
                    }),
                    (Some(_), 2) if precision < 0 => Err(at_type(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("INTERVAL({precision}) precision must not be negative"),
                    )),
                    (Some(range), 2) => {
                        if precision > 6 {
                            self.typmod_warning(
                                format_args!(
                                    "INTERVAL({precision}) precision reduced to maximum allowed, 6"
                                ),
                                type_at,
                            )?;
                        }
                        Ok(TypeMod::IntervalMod {
                            range,
                            precision: Some(precision.min(6) as u8),
                        }
                        .encode())
                    }
                    _ => Err(invalid("invalid INTERVAL type modifier")),
                }
            }
        };
        match checked {
            Ok(type_mod) => Ok(type_mod),
            Err(error) => self.analysis_error(error).map(|()| -1),
        }
    }

    /// A WARNING PostgreSQL's `typmodin` raises during analysis (`22023`).
    fn typmod_warning(
        &mut self,
        message: core::fmt::Arguments<'_>,
        type_at: Option<usize>,
    ) -> Result<(), ParseError> {
        let message = self
            .arena
            .alloc_str_display(message)
            .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
        self.notice(ParseNotice {
            warning: true,
            sqlstate: sqlstate::INVALID_PARAMETER_VALUE,
            message,
            cursor: type_at,
            raw: false,
        })
    }
}
