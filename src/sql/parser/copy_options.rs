//! COPY option lists, read as PostgreSQL's grammar reads them and checked as
//! its `ProcessCopyOptions` checks them, with the same SQLSTATEs, messages and
//! cursor positions. The checks run when the statement would, so they are
//! analysis errors here: a later syntax error in the query text wins.

use super::{PARSE_MESSAGE_BYTES, ParseError, Parser, Tok};
use crate::sql::ast::{CopyErrorAction, CopyFormat, CopyHeader, CopyLogVerbosity, CopyOptions};
use crate::sql::eval::sqlstate;
use crate::stack_format;

/// An option argument as PostgreSQL's `copy_generic_opt_arg` reads it.
#[derive(Clone, Copy)]
enum Argument<'a> {
    Absent,
    /// A word, string or boolean keyword.
    Text(&'a str),
    /// An integer constant (PostgreSQL's Integer node).
    Integer(&'a str),
    /// Any other numeric constant (PostgreSQL's Float node).
    Number(&'a str),
    Star,
    List(&'a [&'a str]),
}

impl<'a> Argument<'a> {
    /// PostgreSQL's `defGetString`, or `None` for a missing argument.
    fn text(self) -> Option<&'a str> {
        match self {
            Self::Text(text) | Self::Integer(text) | Self::Number(text) => Some(text),
            Self::Star => Some("*"),
            Self::Absent | Self::List(_) => None,
        }
    }
}

/// Columns named by a FORCE option, or every column (`*`).
#[derive(Clone, Copy)]
enum ForcedColumns<'a> {
    All,
    Columns(&'a [&'a str]),
}

/// One option as written, before the cross-option checks.
#[derive(Clone, Copy, Default)]
struct WrittenOptions<'a> {
    format: Option<CopyFormat>,
    freeze: Option<bool>,
    delimiter: Option<&'a str>,
    null: Option<&'a str>,
    default: Option<&'a str>,
    header: Option<CopyHeader>,
    quote: Option<&'a str>,
    escape: Option<&'a str>,
    force_quote: Option<ForcedColumns<'a>>,
    force_not_null: Option<ForcedColumns<'a>>,
    force_null: Option<ForcedColumns<'a>>,
    encoding: Option<crate::storage::PgEncoding>,
    on_error: Option<CopyErrorAction>,
    log_verbosity: Option<CopyLogVerbosity>,
    reject_limit: Option<i64>,
}

impl<'a> Parser<'a> {
    /// The option list after a COPY target, both the parenthesized generic
    /// form and the legacy bare keywords, checked for a COPY FROM when
    /// `from`. A failing check is deferred to statement execution.
    pub(super) fn copy_options(
        &mut self,
        from: bool,
        delimiters: Option<(&'a str, usize)>,
    ) -> Result<CopyOptions<'a>, ParseError> {
        let mut written = WrittenOptions::default();
        let mut failure = None;
        if let Some((delimiter, at)) = delimiters {
            failure = self
                .copy_option(
                    &mut written,
                    "delimiter",
                    Argument::Text(delimiter),
                    at,
                    from,
                )
                .err();
        }
        let _ = self.eat_ident("with")?;
        if self.eat_op("(")? {
            loop {
                let at = self.peek_at;
                let name = self.copy_option_name()?;
                let argument = self.copy_option_argument()?;
                if failure.is_none() {
                    failure = self
                        .copy_option(&mut written, name, argument, at, from)
                        .err();
                }
                if !self.eat_op(",")? {
                    break;
                }
            }
            self.expect_op(")")?;
        } else {
            loop {
                let at = self.peek_at;
                let Some((name, argument)) = self.copy_legacy_option()? else {
                    break;
                };
                if failure.is_none() {
                    failure = self
                        .copy_option(&mut written, name, argument, at, from)
                        .err();
                }
            }
        }
        let checked = match failure {
            Some(error) => Err(error),
            None => self.checked_copy_options(written, from),
        };
        match checked {
            Ok(options) => Ok(options),
            Err(error) => {
                self.analysis_error(error)?;
                Ok(CopyOptions::TEXT)
            }
        }
    }

    /// PostgreSQL's `ColLabel`: any identifier or keyword.
    fn copy_option_name(&mut self) -> Result<&'a str, ParseError> {
        match self.peeked {
            Tok::Ident(name) | Tok::QuotedIdent(name) => {
                self.advance()?;
                Ok(name)
            }
            _ => Err(self.syntax_error_here()),
        }
    }

    /// PostgreSQL's `copy_generic_opt_arg`, or nothing before `,` or `)`.
    fn copy_option_argument(&mut self) -> Result<Argument<'a>, ParseError> {
        Ok(match self.peeked {
            Tok::Op(",") | Tok::Op(")") => Argument::Absent,
            Tok::Op("*") => {
                self.advance()?;
                Argument::Star
            }
            Tok::Op("(") => {
                self.advance()?;
                let mut words = crate::mem::arena::ArenaList::new(self.arena);
                loop {
                    let word = self.copy_option_word()?;
                    words
                        .push(word)
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                self.expect_op(")")?;
                Argument::List(words.as_slice())
            }
            Tok::Op(sign @ ("+" | "-")) => {
                let Tok::Num(text) = self.lookahead(1)? else {
                    return Err(self.syntax_error_here());
                };
                let integer = integer_constant(text);
                // PostgreSQL's grammar takes a sign only on a FCONST or ICONST,
                // and `+` only on a FCONST.
                if sign == "+" && integer {
                    self.advance()?;
                    return Err(self.syntax_error_here());
                }
                self.advance()?;
                self.advance()?;
                let text = self
                    .arena
                    .alloc_str_display(format_args!("{sign}{text}"))
                    .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                if integer {
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
            Tok::Ident("default") => {
                self.advance()?;
                Argument::Text("default")
            }
            _ => Argument::Text(self.copy_option_word()?),
        })
    }

    /// PostgreSQL's `opt_boolean_or_string`: `TRUE`, `FALSE`, `ON`, a string
    /// or any word that is not a reserved keyword.
    fn copy_option_word(&mut self) -> Result<&'a str, ParseError> {
        let word = match self.peeked {
            Tok::Str(text) | Tok::QuotedIdent(text) => text,
            Tok::Ident(word @ ("true" | "false" | "on")) => word,
            Tok::Ident(word) if !super::is_reserved_keyword(word) => word,
            _ => return Err(self.syntax_error_here()),
        };
        self.advance()?;
        Ok(word)
    }

    /// One legacy bare option (`CSV`, `DELIMITER [AS] ','`, `FORCE QUOTE a`,
    /// ...) as the generic option it stands for, or `None` at the list's end.
    fn copy_legacy_option(&mut self) -> Result<Option<(&'a str, Argument<'a>)>, ParseError> {
        let Tok::Ident(word) = self.peeked else {
            return Ok(None);
        };
        let option = match word {
            "binary" => ("format", Argument::Text("binary")),
            "csv" => ("format", Argument::Text("csv")),
            "freeze" => ("freeze", Argument::Absent),
            "header" => ("header", Argument::Absent),
            "delimiter" | "null" | "quote" | "escape" | "encoding" => {
                self.advance()?;
                if word != "encoding" {
                    let _ = self.eat_ident("as")?;
                }
                let Tok::Str(text) = self.peeked else {
                    return Err(self.syntax_error_here());
                };
                self.advance()?;
                return Ok(Some((word, Argument::Text(text))));
            }
            "force" => {
                self.advance()?;
                let name = if self.eat_ident("quote")? {
                    "force_quote"
                } else if self.eat_ident("not")? {
                    self.expect_ident("null")?;
                    "force_not_null"
                } else {
                    self.expect_ident("null")?;
                    "force_null"
                };
                if self.eat_op("*")? {
                    return Ok(Some((name, Argument::Star)));
                }
                let mut columns = crate::mem::arena::ArenaList::new(self.arena);
                loop {
                    let column = self.col_ident("column name")?;
                    columns
                        .push(column)
                        .map_err(|_| self.arena_full("statement too large for SQL arena"))?;
                    if !self.eat_op(",")? {
                        break;
                    }
                }
                return Ok(Some((name, Argument::List(columns.as_slice()))));
            }
            _ => return Ok(None),
        };
        self.advance()?;
        Ok(Some(option))
    }

    /// Records one option as PostgreSQL's `ProcessCopyOptions` loop does;
    /// `at` is the option name's position.
    fn copy_option(
        &self,
        written: &mut WrittenOptions<'a>,
        name: &'a str,
        argument: Argument<'a>,
        at: usize,
        from: bool,
    ) -> Result<(), ParseError> {
        let positioned = |sqlstate, message: core::fmt::Arguments<'_>| ParseError {
            at,
            message: stack_format!(PARSE_MESSAGE_BYTES, "{}", message),
            near: None,
            detail: None,
            sqlstate,
            cursor: Some(at),
            hint: None,
        };
        let conflict = || {
            positioned(
                sqlstate::SYNTAX_ERROR,
                format_args!("conflicting or redundant options"),
            )
        };
        let string = || {
            argument.text().ok_or_else(|| {
                unpositioned(
                    sqlstate::SYNTAX_ERROR,
                    format_args!("{name} requires a parameter"),
                    at,
                )
            })
        };
        // Options taking a string value.
        let slot = match name {
            "delimiter" => Some(&mut written.delimiter),
            "null" => Some(&mut written.null),
            "default" => Some(&mut written.default),
            "quote" => Some(&mut written.quote),
            "escape" => Some(&mut written.escape),
            _ => None,
        };
        if let Some(slot) = slot {
            if slot.is_some() {
                return Err(conflict());
            }
            *slot = Some(string()?);
            return Ok(());
        }
        match name {
            "format" => {
                if written.format.is_some() {
                    return Err(conflict());
                }
                let format = string()?;
                written.format = Some(match format {
                    "text" => CopyFormat::Text,
                    "csv" => CopyFormat::Csv,
                    "binary" => CopyFormat::Binary,
                    _ => {
                        return Err(positioned(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            format_args!("COPY format \"{format}\" not recognized"),
                        ));
                    }
                });
            }
            "freeze" => {
                if written.freeze.is_some() {
                    return Err(conflict());
                }
                written.freeze = Some(boolean(argument).ok_or_else(|| {
                    unpositioned(
                        sqlstate::SYNTAX_ERROR,
                        format_args!("{name} requires a Boolean value"),
                        at,
                    )
                })?);
            }
            "header" => {
                if written.header.is_some() {
                    return Err(conflict());
                }
                let header = match argument {
                    Argument::Integer("0") => Some(CopyHeader::None),
                    Argument::Integer("1") => Some(CopyHeader::Skip),
                    Argument::Integer(_) => None,
                    _ => match argument.text() {
                        None => Some(CopyHeader::Skip),
                        Some(text) if text.eq_ignore_ascii_case("match") => {
                            if !from {
                                return Err(unpositioned(
                                    sqlstate::FEATURE_NOT_SUPPORTED,
                                    format_args!("cannot use \"{text}\" with HEADER in COPY TO"),
                                    at,
                                ));
                            }
                            Some(CopyHeader::Match)
                        }
                        Some(text) => boolean_word(text).map(|value| {
                            if value {
                                CopyHeader::Skip
                            } else {
                                CopyHeader::None
                            }
                        }),
                    },
                };
                written.header = Some(header.ok_or_else(|| {
                    unpositioned(
                        sqlstate::SYNTAX_ERROR,
                        format_args!("{name} requires a Boolean value or \"match\""),
                        at,
                    )
                })?);
            }
            "force_quote" | "force_not_null" | "force_null" => {
                let slot = match name {
                    "force_quote" => &mut written.force_quote,
                    "force_not_null" => &mut written.force_not_null,
                    _ => &mut written.force_null,
                };
                if slot.is_some() {
                    return Err(conflict());
                }
                *slot = Some(match argument {
                    Argument::Star => ForcedColumns::All,
                    Argument::List(columns) => ForcedColumns::Columns(columns),
                    _ => {
                        return Err(positioned(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            format_args!(
                                "argument to option \"{name}\" must be a list of column names"
                            ),
                        ));
                    }
                });
            }
            "encoding" => {
                if written.encoding.is_some() {
                    return Err(conflict());
                }
                let encoding = string()?;
                written.encoding =
                    Some(crate::storage::PgEncoding::parse(encoding).ok_or_else(|| {
                        positioned(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            format_args!(
                                "argument to option \"{name}\" must be a valid encoding name"
                            ),
                        )
                    })?);
            }
            "on_error" => {
                if written.on_error.is_some() {
                    return Err(conflict());
                }
                if !from {
                    return Err(positioned(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("COPY ON_ERROR cannot be used with COPY TO"),
                    ));
                }
                let text = string()?;
                written.on_error = Some(match text {
                    text if text.eq_ignore_ascii_case("stop") => CopyErrorAction::Stop,
                    text if text.eq_ignore_ascii_case("ignore") => CopyErrorAction::Ignore,
                    text => {
                        return Err(positioned(
                            sqlstate::INVALID_PARAMETER_VALUE,
                            format_args!("COPY ON_ERROR \"{text}\" not recognized"),
                        ));
                    }
                });
            }
            "log_verbosity" => {
                if written.log_verbosity.is_some() {
                    return Err(conflict());
                }
                let text = string()?;
                written.log_verbosity = Some(if text.eq_ignore_ascii_case("default") {
                    CopyLogVerbosity::Default
                } else if text.eq_ignore_ascii_case("verbose") {
                    CopyLogVerbosity::Verbose
                } else if text.eq_ignore_ascii_case("silent") {
                    CopyLogVerbosity::Silent
                } else {
                    return Err(positioned(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("COPY LOG_VERBOSITY \"{text}\" not recognized"),
                    ));
                });
            }
            "reject_limit" => {
                if written.reject_limit.is_some() {
                    return Err(conflict());
                }
                let Some(text) = argument.text() else {
                    return Err(unpositioned(
                        sqlstate::SYNTAX_ERROR,
                        format_args!("{name} requires a numeric value"),
                        at,
                    ));
                };
                let limit = crate::sql::eval::parse_int_bounded(text, i64::MIN, i64::MAX, "bigint")
                    .map_err(|error| {
                        let state = if error.sqlstate == sqlstate::NUMERIC_OUT_OF_RANGE {
                            sqlstate::NUMERIC_OUT_OF_RANGE
                        } else {
                            sqlstate::INVALID_TEXT_REPRESENTATION
                        };
                        unpositioned(state, format_args!("{}", error.message), at)
                    })?;
                if limit <= 0 {
                    return Err(unpositioned(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        format_args!("REJECT_LIMIT ({limit}) must be greater than zero"),
                        at,
                    ));
                }
                written.reject_limit = Some(limit);
            }
            _ => {
                return Err(positioned(
                    sqlstate::SYNTAX_ERROR,
                    format_args!("option \"{name}\" not recognized"),
                ));
            }
        }
        Ok(())
    }

    /// The checks PostgreSQL's `ProcessCopyOptions` makes after reading every
    /// option, in its order, and the typed options they leave.
    fn checked_copy_options(
        &self,
        written: WrittenOptions<'a>,
        from: bool,
    ) -> Result<CopyOptions<'a>, ParseError> {
        let at = self.peek_at;
        let fail =
            |sqlstate, message: core::fmt::Arguments<'_>| Err(unpositioned(sqlstate, message, at));
        let format = written.format.unwrap_or(CopyFormat::Text);
        let binary = matches!(format, CopyFormat::Binary);
        let csv = matches!(format, CopyFormat::Csv);
        for (given, name) in [
            (written.delimiter.is_some(), "DELIMITER"),
            (written.null.is_some(), "NULL"),
            (written.default.is_some(), "DEFAULT"),
        ] {
            if binary && given {
                return fail(
                    sqlstate::SYNTAX_ERROR,
                    format_args!("cannot specify {name} in BINARY mode"),
                );
            }
        }
        let delimiter = written.delimiter.unwrap_or(if csv { "," } else { "\t" });
        let null = written.null.unwrap_or(if csv { "" } else { "\\N" });
        let quote = written.quote.or(csv.then_some("\""));
        let escape = written.escape.or(if csv { quote } else { None });
        if delimiter.len() != 1 {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("COPY delimiter must be a single one-byte character"),
            );
        }
        let line_break = |text: &str| text.contains(['\r', '\n']);
        if line_break(delimiter) {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY delimiter cannot be newline or carriage return"),
            );
        }
        if line_break(null) {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY null representation cannot use newline or carriage return"),
            );
        }
        if written.default.is_some_and(line_break) {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY default representation cannot use newline or carriage return"),
            );
        }
        if !csv && "\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(delimiter) {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY delimiter cannot be \"{delimiter}\""),
            );
        }
        let header = written.header.unwrap_or(CopyHeader::None);
        if binary && !matches!(header, CopyHeader::None) {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("cannot specify HEADER in BINARY mode"),
            );
        }
        if !csv && written.quote.is_some() {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("COPY QUOTE requires CSV mode"),
            );
        }
        let quote_byte = quote.map(str::as_bytes);
        if csv && quote_byte.is_some_and(|quote| quote.len() != 1) {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("COPY quote must be a single one-byte character"),
            );
        }
        if csv && quote_byte.is_some_and(|quote| quote[0] == delimiter.as_bytes()[0]) {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY delimiter and quote must be different"),
            );
        }
        if !csv && written.escape.is_some() {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("COPY ESCAPE requires CSV mode"),
            );
        }
        if csv && escape.is_some_and(|escape| escape.len() != 1) {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("COPY escape must be a single one-byte character"),
            );
        }
        for (forced, name, allowed_direction_from, wrong_direction) in [
            (
                written.force_quote,
                "FORCE_QUOTE",
                false,
                sqlstate::FEATURE_NOT_SUPPORTED,
            ),
            (
                written.force_not_null,
                "FORCE_NOT_NULL",
                true,
                sqlstate::INVALID_PARAMETER_VALUE,
            ),
            (
                written.force_null,
                "FORCE_NULL",
                true,
                sqlstate::INVALID_PARAMETER_VALUE,
            ),
        ] {
            if forced.is_none() {
                continue;
            }
            if !csv {
                return fail(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format_args!("COPY {name} requires CSV mode"),
                );
            }
            if from != allowed_direction_from {
                return fail(
                    wrong_direction,
                    format_args!(
                        "COPY {name} cannot be used with {}",
                        if from { "COPY FROM" } else { "COPY TO" }
                    ),
                );
            }
        }
        if null.contains(delimiter) {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY delimiter character must not appear in the NULL specification"),
            );
        }
        if let Some(quote) = quote.filter(|_| csv)
            && null.contains(quote)
        {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("CSV quote character must not appear in the NULL specification"),
            );
        }
        if written.freeze == Some(true) && !from {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY FREEZE cannot be used with COPY TO"),
            );
        }
        if let Some(default) = written.default {
            if !from {
                return fail(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format_args!("COPY DEFAULT cannot be used with COPY TO"),
                );
            }
            if default.contains(delimiter) {
                return fail(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format_args!(
                        "COPY delimiter character must not appear in the DEFAULT specification"
                    ),
                );
            }
            if let Some(quote) = quote.filter(|_| csv)
                && default.contains(quote)
            {
                return fail(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format_args!(
                        "CSV quote character must not appear in the DEFAULT specification"
                    ),
                );
            }
            if default == null {
                return fail(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    format_args!("NULL specification and DEFAULT specification cannot be the same"),
                );
            }
        }
        let on_error = written.on_error.unwrap_or(CopyErrorAction::Stop);
        if binary && on_error != CopyErrorAction::Stop {
            return fail(
                sqlstate::SYNTAX_ERROR,
                format_args!("only ON_ERROR STOP is allowed in BINARY mode"),
            );
        }
        if written.reject_limit.is_some() && on_error != CopyErrorAction::Ignore {
            return fail(
                sqlstate::INVALID_PARAMETER_VALUE,
                format_args!("COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE"),
            );
        }
        // Accepted by PostgreSQL but not provided here.
        if written.freeze == Some(true) {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("COPY FREEZE is not supported"),
            );
        }
        if written
            .encoding
            .is_some_and(|encoding| encoding != crate::storage::PgEncoding::UTF8)
        {
            return fail(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("a COPY ENCODING other than UTF8 is not supported"),
            );
        }
        let columns = |forced: Option<ForcedColumns<'a>>| match forced {
            Some(ForcedColumns::Columns(columns)) => (false, columns),
            Some(ForcedColumns::All) => (true, &[][..]),
            None => (false, &[][..]),
        };
        let (force_quote_all, force_quote) = columns(written.force_quote);
        let (force_not_null_all, force_not_null) = columns(written.force_not_null);
        let (force_null_all, force_null) = columns(written.force_null);
        Ok(CopyOptions {
            format,
            delimiter: written.delimiter.map(|text| text.as_bytes()[0]),
            null_string: written.null,
            header,
            on_error,
            reject_limit: written.reject_limit.map(|limit| limit as u64),
            log_verbosity: written.log_verbosity.unwrap_or(CopyLogVerbosity::Default),
            default_string: written.default,
            quote: written.quote.map(|text| text.as_bytes()[0]),
            escape: written.escape.map(|text| text.as_bytes()[0]),
            force_quote_all,
            force_quote,
            force_not_null_all,
            force_not_null,
            force_null_all,
            force_null,
        })
    }
}

fn unpositioned(
    sqlstate: &'static str,
    message: core::fmt::Arguments<'_>,
    at: usize,
) -> ParseError {
    ParseError {
        at,
        message: stack_format!(PARSE_MESSAGE_BYTES, "{}", message),
        near: None,
        detail: None,
        sqlstate,
        cursor: None,
        hint: None,
    }
}

/// PostgreSQL's ICONST: an integer literal that fits `int4`.
fn integer_constant(text: &str) -> bool {
    crate::sql::eval::parse_int_literal(text).is_some_and(|value| i32::try_from(value).is_ok())
}

/// PostgreSQL's `defGetBoolean`.
fn boolean(argument: Argument<'_>) -> Option<bool> {
    match argument {
        Argument::Absent => Some(true),
        Argument::Integer("0") => Some(false),
        Argument::Integer("1") => Some(true),
        Argument::Integer(_) => None,
        other => other.text().and_then(boolean_word),
    }
}

fn boolean_word(text: &str) -> Option<bool> {
    if text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("on") {
        Some(true)
    } else if text.eq_ignore_ascii_case("false") || text.eq_ignore_ascii_case("off") {
        Some(false)
    } else {
        None
    }
}
