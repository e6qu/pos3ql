//! VACUUM's and ANALYZE's option lists, read as PostgreSQL's
//! `utility_option_list` (or VACUUM's bare `FULL FREEZE VERBOSE ANALYZE`
//! words) reads them and checked as `ExecVacuum` checks them, with the same
//! SQLSTATEs, messages and cursor positions. The checks run when the
//! statement would, so a later syntax error in the statement wins.

use super::copy_options::{Argument, boolean, integer_constant, unpositioned};
use super::{ParseError, Parser, Tok};
use crate::sql::ast::{AnalyzeOptions, IndexCleanup, VacuumOptions};
use crate::sql::eval::sqlstate;

/// `BUFFER_USAGE_LIMIT`'s accepted sizes besides 0, in kilobytes, as
/// PostgreSQL's `MIN_BAS_VAC_RING_SIZE_KB` and `MAX_BAS_VAC_RING_SIZE_KB`.
const MIN_RING_KB: i32 = 128;
const MAX_RING_KB: i32 = 16 * 1024 * 1024;
/// `MAX_PARALLEL_WORKER_LIMIT`.
const MAX_PARALLEL_WORKERS: i32 = 1024;

/// The options of a VACUUM or ANALYZE as written: what they select, and the
/// first error `ExecVacuum` would report for them.
pub(super) struct WrittenMaintenanceOptions {
    pub(super) vacuum: VacuumOptions,
    pub(super) analyze: AnalyzeOptions,
    verbose: bool,
    /// `BUFFER_USAGE_LIMIT` was given.
    ring_size: bool,
    /// `PARALLEL` asked for at least one worker.
    parallel_workers: bool,
    freeze: bool,
    disable_page_skipping: bool,
    process_toast: bool,
    skip_database_stats: bool,
    error: Option<ParseError>,
}

impl WrittenMaintenanceOptions {
    /// No option written, for a VACUUM when `is_vacuum`.
    pub(super) const fn new(is_vacuum: bool) -> Self {
        Self {
            vacuum: VacuumOptions {
                analyze: !is_vacuum,
                ..VacuumOptions::DEFAULT
            },
            analyze: AnalyzeOptions { skip_locked: false },
            verbose: false,
            ring_size: false,
            parallel_workers: false,
            freeze: false,
            disable_page_skipping: false,
            process_toast: true,
            skip_database_stats: false,
            error: None,
        }
    }

    /// These options after `ExecVacuum`'s checks of a VACUUM (`is_vacuum`)
    /// of the relations listed when `has_targets`, `columns` when any of
    /// them has a column list.
    pub(super) fn checked(
        self,
        is_vacuum: bool,
        has_targets: bool,
        columns: bool,
    ) -> Result<Self, ParseError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let unsupported = |message: &str| {
            Err(unpositioned(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format_args!("{message}"),
                0,
            ))
        };
        let vacuum = self.vacuum;
        if is_vacuum {
            if vacuum.full && self.parallel_workers {
                return unsupported("VACUUM FULL cannot be performed in parallel");
            }
            if self.ring_size && vacuum.full && !vacuum.analyze {
                return unsupported("BUFFER_USAGE_LIMIT cannot be specified for VACUUM FULL");
            }
            if !vacuum.analyze && columns {
                return unsupported(
                    "ANALYZE option must be specified when a column list is provided",
                );
            }
            if vacuum.full && self.disable_page_skipping {
                return unsupported("VACUUM option DISABLE_PAGE_SKIPPING cannot be used with FULL");
            }
            if vacuum.full && !self.process_toast {
                return unsupported("PROCESS_TOAST required with VACUUM FULL");
            }
            if vacuum.only_database_stats {
                if has_targets {
                    return unsupported(
                        "ONLY_DATABASE_STATS cannot be specified with a list of tables",
                    );
                }
                if self.analyze.skip_locked
                    || vacuum.analyze
                    || self.freeze
                    || vacuum.full
                    || self.disable_page_skipping
                    || self.skip_database_stats
                {
                    return unsupported(
                        "ONLY_DATABASE_STATS cannot be specified with other VACUUM options",
                    );
                }
            }
        }
        // VERBOSE reports PostgreSQL's page scans with their timing and
        // buffer usage, which this engine does not reproduce.
        if self.verbose {
            return unsupported(if is_vacuum {
                "VACUUM option VERBOSE is not supported"
            } else {
                "ANALYZE option VERBOSE is not supported"
            });
        }
        Ok(self)
    }

    /// Notes one option, keeping the first error.
    pub(super) fn write(&mut self, name: &str, argument: Argument<'_>, at: usize, is_vacuum: bool) {
        if self.error.is_none()
            && let Err(error) = self.option(name, argument, at, is_vacuum)
        {
            self.error = Some(error);
        }
    }

    /// One option, checked as `ExecVacuum` checks it.
    fn option(
        &mut self,
        name: &str,
        argument: Argument<'_>,
        at: usize,
        is_vacuum: bool,
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
        let positioned = |message: core::fmt::Arguments<'_>| ParseError {
            cursor: Some(at),
            ..unpositioned(sqlstate::SYNTAX_ERROR, message, at)
        };
        match name {
            "verbose" => self.verbose = flag()?,
            "skip_locked" => self.analyze.skip_locked = flag()?,
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
                self.ring_size = true;
            }
            _ if !is_vacuum => {
                return Err(positioned(format_args!(
                    "unrecognized ANALYZE option \"{name}\""
                )));
            }
            "analyze" => self.vacuum.analyze = flag()?,
            "freeze" => self.freeze = flag()?,
            "full" => self.vacuum.full = flag()?,
            "disable_page_skipping" => self.disable_page_skipping = flag()?,
            "index_cleanup" => {
                self.vacuum.index_cleanup = if matches!(argument, Argument::Absent)
                    || argument
                        .text()
                        .is_some_and(|text| text.eq_ignore_ascii_case("auto"))
                {
                    IndexCleanup::Auto
                } else if flag()? {
                    IndexCleanup::On
                } else {
                    IndexCleanup::Off
                };
            }
            "process_main" => self.vacuum.process_main = flag()?,
            "process_toast" => self.process_toast = flag()?,
            "truncate" => {
                flag()?;
            }
            "parallel" => {
                if matches!(argument, Argument::Absent) {
                    return Err(positioned(format_args!(
                        "parallel option requires a value between 0 and {MAX_PARALLEL_WORKERS}"
                    )));
                }
                // `defGetInt32`: an integer constant, which an `int4` holds.
                let workers = match argument {
                    Argument::Integer(text) => crate::sql::eval::parse_int_literal(text)
                        .and_then(|value| i32::try_from(value).ok()),
                    _ => None,
                }
                .ok_or_else(|| {
                    unpositioned(
                        sqlstate::SYNTAX_ERROR,
                        format_args!("{name} requires an integer value"),
                        at,
                    )
                })?;
                if !(0..=MAX_PARALLEL_WORKERS).contains(&workers) {
                    return Err(positioned(format_args!(
                        "parallel workers for vacuum must be between 0 and {MAX_PARALLEL_WORKERS}"
                    )));
                }
                self.parallel_workers = workers > 0;
            }
            "skip_database_stats" => self.skip_database_stats = flag()?,
            "only_database_stats" => self.vacuum.only_database_stats = flag()?,
            _ => {
                return Err(positioned(format_args!(
                    "unrecognized VACUUM option \"{name}\""
                )));
            }
        }
        Ok(())
    }
}

impl<'a> Parser<'a> {
    /// The list after `VACUUM (` or `ANALYZE (`, through its closing
    /// parenthesis.
    pub(super) fn maintenance_options(
        &mut self,
        written: &mut WrittenMaintenanceOptions,
        is_vacuum: bool,
    ) -> Result<(), ParseError> {
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
            let argument = self.maintenance_option_argument()?;
            written.write(name, argument, at, is_vacuum);
            if self.eat_op(")")? {
                return Ok(());
            }
            self.expect_op(",")?;
        }
    }

    /// PostgreSQL's `utility_option_arg`: a Boolean or string word, a signed
    /// number, or nothing before `,` or `)`.
    fn maintenance_option_argument(&mut self) -> Result<Argument<'a>, ParseError> {
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
}
