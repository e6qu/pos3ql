//! Set-returning functions that advance one row at a time from fixed state.
//!
//! A retained cursor over `generate_series` or a one-array `unnest` keeps
//! only the next value (or array position), so a long series costs no row
//! memory and its rows are produced as FETCH reaches them. Materialized
//! callers use the same arithmetic, so both paths yield identical rows.

use super::*;
use crate::sql::eval::SeriesKind;
use crate::sql::numeric::Numeric;
use crate::sql::types::Interval;

/// The builtin set-returning implementations a generator can replace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GeneratorKind {
    /// `generate_series` over `integer` (narrow) or `bigint` (wide).
    Integer {
        wide: bool,
    },
    Numeric,
    /// `generate_series` over `timestamp` or `timestamp with time zone`.
    Timestamp,
    /// `unnest(anyarray)` with a single array argument.
    Unnest,
}

impl GeneratorKind {
    /// Selects by the builtin's C implementation, which names the overload.
    pub(crate) fn for_builtin(oid: i32, arguments: usize) -> Option<Self> {
        let signature = crate::sql::catalog::builtin_callable_by_oid(oid)?;
        match signature.catalog.source {
            "generate_series_int4" | "generate_series_step_int4" => {
                Some(Self::Integer { wide: false })
            }
            "generate_series_int8" | "generate_series_step_int8" => {
                Some(Self::Integer { wide: true })
            }
            "generate_series_numeric" | "generate_series_step_numeric" => Some(Self::Numeric),
            "generate_series_timestamp" | "generate_series_timestamptz" => Some(Self::Timestamp),
            "array_unnest" if arguments == 1 => Some(Self::Unnest),
            _ => None,
        }
    }
}

/// The position of one running set-returning call.
pub(crate) enum RowGenerator<'a> {
    Integer {
        next: i64,
        step: i64,
        remaining: u64,
        wide: bool,
    },
    Numeric {
        start: Numeric<'a>,
        stop: Numeric<'a>,
        step: Numeric<'a>,
        index: usize,
    },
    Timestamp {
        next: i64,
        stop: i64,
        step: Interval,
        positive: bool,
        kind: SeriesKind,
    },
    Elements(Option<crate::sql::array::Elements<'a>>),
}

fn zero_step() -> SqlError {
    sql_err!(
        sqlstate::INVALID_PARAMETER_VALUE,
        "step size cannot equal zero"
    )
}

impl<'a> RowGenerator<'a> {
    /// Starts a generator over already evaluated arguments; NULL arguments
    /// yield no rows, as in PostgreSQL.
    pub(crate) fn start(
        kind: GeneratorKind,
        arguments: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let empty = || RowGenerator::Elements(None);
        match kind {
            GeneratorKind::Unnest => match arguments {
                [Datum::Array { element, raw }] => Ok(Self::Elements(Some(
                    crate::sql::array::elements(raw, *element),
                ))),
                [Datum::Null] => Ok(empty()),
                _ => Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "unnest requires an array or multirange argument"
                )),
            },
            GeneratorKind::Integer { wide } => {
                let value = |datum: Datum<'a>| match datum {
                    Datum::Int2(value) => Ok(Some(i64::from(value))),
                    Datum::Int4(value) => Ok(Some(i64::from(value))),
                    Datum::Int8(value) => Ok(Some(value)),
                    Datum::Null => Ok(None),
                    _ => Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "generate_series requires integer arguments"
                    )),
                };
                let (Some(start), Some(stop)) = (value(arguments[0])?, value(arguments[1])?) else {
                    return Ok(empty());
                };
                let step = match arguments.get(2) {
                    Some(step) => match value(*step)? {
                        Some(step) => step,
                        None => return Ok(empty()),
                    },
                    None => 1,
                };
                Ok(Self::integer(start, stop, step, wide)?)
            }
            GeneratorKind::Numeric => {
                if arguments.iter().any(Datum::is_null) {
                    return Ok(empty());
                }
                let numeric = |datum: Datum<'a>| match crate::sql::eval::cast_to(
                    datum,
                    ColType::Numeric,
                    arena,
                )? {
                    Datum::Numeric(value) => Ok(value),
                    _ => Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "numeric cast returned another type"
                    )),
                };
                let start = numeric(arguments[0])?;
                let stop = numeric(arguments[1])?;
                let step = match arguments.get(2) {
                    Some(step) => numeric(*step)?,
                    None => Numeric::from_i64(1, arena)?,
                };
                // Validates infinities and a zero step before any row.
                crate::sql::eval::numeric_series_at(start, stop, step, 1, arena)?;
                Ok(Self::Numeric {
                    start,
                    stop,
                    step,
                    index: 1,
                })
            }
            GeneratorKind::Timestamp => {
                let start = crate::sql::eval::text_view(arguments[0]);
                if arguments.len() != 3 {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "generate_series over timestamps requires a step"
                    ));
                }
                if arguments.iter().any(Datum::is_null) {
                    return Ok(empty());
                }
                let Some((next, kind)) = crate::sql::eval::timestamp_series_start(&start) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "generate_series over timestamps requires a timestamp start"
                    ));
                };
                let stop = crate::sql::eval::cast_to(
                    crate::sql::eval::text_view(arguments[1]),
                    kind.coltype(),
                    arena,
                )?;
                let step = crate::sql::eval::cast_to(
                    crate::sql::eval::text_view(arguments[2]),
                    ColType::Interval,
                    arena,
                )?;
                let (Some((stop, _)), Datum::Interval(step)) =
                    (crate::sql::eval::timestamp_series_start(&stop), step)
                else {
                    return Ok(empty());
                };
                if step.months == 0 && step.days == 0 && step.micros == 0 {
                    return Err(zero_step());
                }
                Ok(Self::Timestamp {
                    next,
                    stop,
                    step,
                    positive: crate::sql::eval::interval_is_positive(step),
                    kind,
                })
            }
        }
    }

    /// An integer series from `start` through `stop`. The count uses 128-bit
    /// arithmetic so series that touch the type's bounds never overflow.
    pub(crate) fn integer(start: i64, stop: i64, step: i64, wide: bool) -> Result<Self, SqlError> {
        if step == 0 {
            return Err(zero_step());
        }
        let span = i128::from(stop) - i128::from(start);
        let remaining = if span == 0 || (span > 0) == (step > 0) {
            (span / i128::from(step)) as u64 + 1
        } else {
            0
        };
        Ok(Self::Integer {
            next: start,
            step,
            remaining,
            wide,
        })
    }

    /// Rows still to come, when known without producing them.
    pub(crate) fn known_remaining(&self) -> Option<u64> {
        match self {
            Self::Integer { remaining, .. } => Some(*remaining),
            Self::Elements(elements) => Some(
                elements
                    .as_ref()
                    .map_or(0, |elements| elements.len() as u64),
            ),
            Self::Numeric { .. } | Self::Timestamp { .. } => None,
        }
    }

    /// The next value; numeric values are built in `arena`.
    pub(crate) fn next(&mut self, arena: &'a Arena) -> Result<Option<Datum<'a>>, SqlError> {
        match self {
            Self::Integer {
                next,
                step,
                remaining,
                wide,
            } => {
                if *remaining == 0 {
                    return Ok(None);
                }
                let value = *next;
                *remaining -= 1;
                if *remaining != 0 {
                    // The count guarantees the following value is in range.
                    *next = value + *step;
                }
                Ok(Some(if *wide {
                    Datum::Int8(value)
                } else {
                    Datum::Int4(value as i32)
                }))
            }
            Self::Numeric {
                start,
                stop,
                step,
                index,
            } => {
                let value =
                    crate::sql::eval::numeric_series_at(*start, *stop, *step, *index, arena)?;
                if value.is_some() {
                    *index += 1;
                }
                Ok(value.map(Datum::Numeric))
            }
            Self::Timestamp {
                next,
                stop,
                step,
                positive,
                kind,
            } => {
                let value = *next;
                if (*positive && value > *stop) || (!*positive && value < *stop) {
                    return Ok(None);
                }
                *next = crate::sql::datetime::add_interval(value, *step);
                Ok(Some(kind.datum(value)))
            }
            Self::Elements(elements) => Ok(elements.as_mut().and_then(Iterator::next)),
        }
    }
}
