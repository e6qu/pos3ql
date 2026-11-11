//! Set-returning functions that advance one row at a time from fixed state.
//!
//! A retained cursor or result stream over `generate_series`,
//! `generate_subscripts`, one-array `unnest`,
//! `regexp_matches`, `regexp_split_to_table`, or the `json`/`jsonb`
//! `*_each`, `*_array_elements` and `*_object_keys` families keeps only its
//! constant arguments and the next position, so a long result costs no row
//! memory and its rows are produced as FETCH or the client reaches them.
//! `ROWS FROM (...)`, which `FROM unnest(a, b)` expands to, advances several
//! such calls in lockstep, padding finished ones with NULL, and `WITH
//! ORDINALITY` numbers the rows.
//! Materialized callers share the stepping code (series arithmetic, regex
//! splitting and match arrays, JSON item scanning), so both paths yield
//! identical rows.

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
    /// `generate_subscripts(anyarray, integer [, boolean])`.
    Subscripts,
    RegexpMatches,
    RegexpSplit,
    /// One of the `json`/`jsonb` container functions.
    Json(JsonItems),
}

/// What a `json`/`jsonb` container function returns for each item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JsonItems {
    pub(crate) jsonb: bool,
    pub(crate) output: JsonOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JsonOutput {
    /// `*_array_elements`; `text` for the `_text` variants.
    Elements { text: bool },
    /// `*_each`: a key and its value.
    Each { text: bool },
    /// `*_object_keys`.
    Keys,
}

impl JsonItems {
    fn name(self) -> &'static str {
        match (self.jsonb, self.output) {
            (false, JsonOutput::Elements { text: false }) => "json_array_elements",
            (false, JsonOutput::Elements { text: true }) => "json_array_elements_text",
            (true, JsonOutput::Elements { text: false }) => "jsonb_array_elements",
            (true, JsonOutput::Elements { text: true }) => "jsonb_array_elements_text",
            (false, JsonOutput::Each { text: false }) => "json_each",
            (false, JsonOutput::Each { text: true }) => "json_each_text",
            (true, JsonOutput::Each { text: false }) => "jsonb_each",
            (true, JsonOutput::Each { text: true }) => "jsonb_each_text",
            (false, JsonOutput::Keys) => "json_object_keys",
            (true, JsonOutput::Keys) => "jsonb_object_keys",
        }
    }
}

impl GeneratorKind {
    /// Selects by the builtin's C implementation, which names the overload.
    pub(crate) fn for_builtin(oid: i32, arguments: usize) -> Option<Self> {
        let signature = crate::sql::catalog::builtin_callable_by_oid(oid)?;
        let json = |jsonb, output| Some(Self::Json(JsonItems { jsonb, output }));
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
            "generate_subscripts" | "generate_subscripts_nodir" => Some(Self::Subscripts),
            "regexp_matches" | "regexp_matches_no_flags" => Some(Self::RegexpMatches),
            "regexp_split_to_table" | "regexp_split_to_table_no_flags" => Some(Self::RegexpSplit),
            "json_array_elements" => json(false, JsonOutput::Elements { text: false }),
            "json_array_elements_text" => json(false, JsonOutput::Elements { text: true }),
            "jsonb_array_elements" => json(true, JsonOutput::Elements { text: false }),
            "jsonb_array_elements_text" => json(true, JsonOutput::Elements { text: true }),
            "json_each" => json(false, JsonOutput::Each { text: false }),
            "json_each_text" => json(false, JsonOutput::Each { text: true }),
            "jsonb_each" => json(true, JsonOutput::Each { text: false }),
            "jsonb_each_text" => json(true, JsonOutput::Each { text: true }),
            "json_object_keys" => json(false, JsonOutput::Keys),
            "jsonb_object_keys" => json(true, JsonOutput::Keys),
            _ => None,
        }
    }
}

/// The position of one running set-returning call.
#[derive(Clone, Copy)]
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
    RegexpMatches {
        string: &'a str,
        pattern: &'a str,
        options: crate::sql::regex::RegexOptions,
        global: bool,
        /// Where the next match search starts; `None` once finished.
        from: Option<usize>,
    },
    RegexpSplit {
        split: crate::sql::eval::RegexSplit<'a>,
        pattern: &'a str,
        options: crate::sql::regex::RegexOptions,
    },
    Json {
        items: crate::sql::json::SourceItems<'a>,
        kind: JsonItems,
    },
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
            GeneratorKind::RegexpMatches | GeneratorKind::RegexpSplit => {
                let name = if kind == GeneratorKind::RegexpMatches {
                    "regexp_matches"
                } else {
                    "regexp_split_to_table"
                };
                let signature = || {
                    sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "function {}(...) does not exist",
                        name
                    )
                };
                let (string, pattern) = match (
                    crate::sql::eval::text_view(arguments[0]),
                    crate::sql::eval::text_view(arguments[1]),
                ) {
                    (Datum::Text(string), Datum::Text(pattern)) => (string, pattern),
                    (Datum::Null, _) | (_, Datum::Null) => return Ok(empty()),
                    _ => return Err(signature()),
                };
                let flags = match arguments
                    .get(2)
                    .map(|flags| crate::sql::eval::text_view(*flags))
                {
                    None => "",
                    Some(Datum::Text(flags)) => flags,
                    Some(Datum::Null) => return Ok(empty()),
                    Some(_) => return Err(signature()),
                };
                let parsed = crate::sql::eval::regexp_options(flags)?;
                if kind == GeneratorKind::RegexpMatches {
                    return Ok(Self::RegexpMatches {
                        string,
                        pattern,
                        options: parsed.options,
                        global: parsed.global,
                        from: Some(0),
                    });
                }
                if parsed.global {
                    return Err(sql_err!(
                        sqlstate::INVALID_PARAMETER_VALUE,
                        "regexp_split_to_table() does not support the \"global\" option"
                    ));
                }
                Ok(Self::RegexpSplit {
                    split: crate::sql::eval::RegexSplit::new(string),
                    pattern,
                    options: parsed.options,
                })
            }
            GeneratorKind::Json(items) => {
                let name = items.name();
                let document = arguments[0];
                crate::sql::eval::funcs::json::check_json_family(name, &document)?;
                let container_error = |container| match items.output {
                    JsonOutput::Keys => crate::sql::json::object_keys_error(name, container),
                    JsonOutput::Elements { .. } => {
                        crate::sql::json::array_elements_error(name, items.jsonb, container)
                    }
                    JsonOutput::Each { .. } => {
                        crate::sql::json::each_error(name, items.jsonb, container)
                    }
                };
                let text = match crate::sql::eval::text_view(document) {
                    Datum::Json { text, .. } | Datum::Text(text) => text,
                    Datum::Null => return Ok(empty()),
                    _ => return Err(container_error(crate::sql::json::Kind::Scalar)),
                };
                let object = !matches!(items.output, JsonOutput::Elements { .. });
                let container = crate::sql::json::kind_of(text);
                let expected = if object {
                    crate::sql::json::Kind::Object
                } else {
                    crate::sql::json::Kind::Array
                };
                if container != expected {
                    return Err(container_error(container));
                }
                let text = if items.jsonb {
                    canonical_jsonb(text, arena)?
                } else {
                    text
                };
                Ok(Self::Json {
                    items: crate::sql::json::SourceItems::start(text, object, arena)?,
                    kind: items,
                })
            }
            GeneratorKind::Subscripts => {
                // Declared subscripts along one dimension, as PostgreSQL's
                // generate_subscripts; a NULL argument or a missing
                // dimension yields no rows.
                let input = match crate::sql::eval::text_view(arguments[0]) {
                    Datum::Null => return Ok(empty()),
                    value => crate::sql::array::Input::from_datum(value).ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "generate_subscripts requires an array argument"
                        )
                    })?,
                };
                let dimension = match crate::sql::eval::text_view(arguments[1]) {
                    Datum::Int4(value) => i64::from(value),
                    Datum::Int8(value) => value,
                    Datum::Null => return Ok(empty()),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "generate_subscripts requires an integer dimension"
                        ));
                    }
                };
                let reverse = match arguments.get(2) {
                    None => false,
                    Some(Datum::Bool(reverse)) => *reverse,
                    Some(Datum::Null) => return Ok(empty()),
                    Some(_) => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "generate_subscripts requires a boolean direction"
                        ));
                    }
                };
                let bounds = match usize::try_from(dimension)
                    .ok()
                    .and_then(|dimension| dimension.checked_sub(1))
                {
                    Some(dimension) => input.bounds(dimension)?,
                    None => None,
                };
                let Some((_, lower, upper)) = bounds else {
                    return Ok(empty());
                };
                let (start, stop, step) = if reverse {
                    (upper, lower, -1)
                } else {
                    (lower, upper, 1)
                };
                Ok(Self::integer(
                    i64::from(start),
                    i64::from(stop),
                    step,
                    false,
                )?)
            }
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
            Self::Numeric { .. }
            | Self::Timestamp { .. }
            | Self::RegexpMatches { .. }
            | Self::RegexpSplit { .. }
            | Self::Json { .. } => None,
        }
    }

    /// Output columns per row.
    pub(crate) fn width(&self) -> usize {
        match self {
            Self::Json {
                kind:
                    JsonItems {
                        output: JsonOutput::Each { .. },
                        ..
                    },
                ..
            } => 2,
            _ => 1,
        }
    }

    /// Writes the next row's [`Self::width`] values into `out`; values are
    /// built in `arena`. Returns false when the call has no more rows.
    pub(crate) fn next_row(
        &mut self,
        arena: &'a Arena,
        out: &mut [Datum<'a>],
    ) -> Result<bool, SqlError> {
        match self {
            Self::RegexpMatches {
                string,
                pattern,
                options,
                global,
                from,
            } => {
                let Some(start) = *from else {
                    return Ok(false);
                };
                let mut spans = [(-1i64, -1i64); crate::sql::regex::MAX_GROUPS];
                let Some(((match_start, match_end), groups)) =
                    crate::sql::regex::find_captures_with_options(
                        pattern, string, start, *options, &mut spans,
                    )?
                else {
                    *from = None;
                    return Ok(false);
                };
                out[0] = crate::sql::eval::regexp_match_array(
                    string,
                    (match_start, match_end),
                    &spans[..groups],
                    arena,
                )?;
                *from = if *global {
                    crate::sql::regex::next_match_from(string, match_start, match_end)
                } else {
                    None
                };
                return Ok(true);
            }
            Self::RegexpSplit {
                split,
                pattern,
                options,
            } => {
                let Some(piece) = split.next(pattern, *options)? else {
                    return Ok(false);
                };
                out[0] = Datum::Text(piece);
                return Ok(true);
            }
            Self::Json { items, kind } => {
                let Some((key, value)) = items.next(arena)? else {
                    return Ok(false);
                };
                let json_value = |text: bool| -> Result<Datum<'a>, SqlError> {
                    if !text {
                        return Ok(Datum::Json {
                            text: value,
                            jsonb: kind.jsonb,
                        });
                    }
                    Ok(match crate::sql::json::parse(value, arena)? {
                        crate::sql::json::Json::Str(decoded) => Datum::Text(decoded),
                        crate::sql::json::Json::Null => Datum::Null,
                        _ => Datum::Text(value),
                    })
                };
                match kind.output {
                    JsonOutput::Elements { text } => out[0] = json_value(text)?,
                    JsonOutput::Each { text } => {
                        out[0] = Datum::Text(key.expect("object member key"));
                        out[1] = json_value(text)?;
                    }
                    JsonOutput::Keys => out[0] = Datum::Text(key.expect("object member key")),
                }
                return Ok(true);
            }
            _ => {}
        }
        let Some(value) = self.next_value(arena)? else {
            return Ok(false);
        };
        out[0] = value;
        Ok(true)
    }

    fn next_value(&mut self, arena: &'a Arena) -> Result<Option<Datum<'a>>, SqlError> {
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
            Self::RegexpMatches { .. } | Self::RegexpSplit { .. } | Self::Json { .. } => {
                unreachable!("multi-value generators step in next_row")
            }
        }
    }
}

/// A `jsonb` document in its canonical text: keys sorted and deduplicated and
/// values rendered as `jsonb` output does, so stepping its members in source
/// order yields `jsonb` order and values. The parse tree is scratch.
fn canonical_jsonb<'a>(text: &str, arena: &'a Arena) -> Result<&'a str, SqlError> {
    let mut buffer = crate::util::StackStr::<8192>::new();
    let mark = arena.mark();
    let parsed = crate::sql::json::parse(text, arena).and_then(|tree| {
        let _ = core::fmt::Write::write_fmt(
            &mut buffer,
            format_args!("{}", crate::sql::json::JsonWrite(&tree)),
        );
        if buffer.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "jsonb value exceeds the supported size"
            ));
        }
        Ok(())
    });
    // SAFETY: the rendered text lives in `buffer`; nothing references the tree.
    unsafe { arena.rewind_to(mark) };
    parsed?;
    arena.alloc_str(buffer.as_str()).map_err(|_| arena_full())
}

/// The calls of one function source: a single call, or `ROWS FROM (...)`
/// advancing each call in lockstep until all have finished, with an optional
/// `WITH ORDINALITY` row number after their columns.
pub(crate) struct FunctionRows<'a> {
    calls: &'a mut [RowGenerator<'a>],
    /// Each call's first position, for a rescan.
    initial: &'a [RowGenerator<'a>],
    live: &'a mut [bool],
    /// The next row number when the source has `WITH ORDINALITY`.
    ordinality: Option<i64>,
}

impl<'a> FunctionRows<'a> {
    pub(crate) fn new(
        calls: &'a mut [RowGenerator<'a>],
        arena: &'a Arena,
        ordinality: bool,
    ) -> Result<Self, SqlError> {
        let live = arena
            .alloc_slice_with(calls.len(), |_| true)
            .map_err(|_| arena_full())?;
        let initial = arena
            .alloc_slice_with(calls.len(), |index| calls[index])
            .map_err(|_| arena_full())?;
        Ok(Self {
            calls,
            initial,
            live,
            ordinality: ordinality.then_some(1),
        })
    }

    /// Starts the calls again from their first rows, as PostgreSQL rescans
    /// a function scan's stored result.
    pub(crate) fn rewind(&mut self) {
        self.calls.copy_from_slice(self.initial);
        self.live.fill(true);
        if let Some(ordinal) = self.ordinality.as_mut() {
            *ordinal = 1;
        }
    }

    /// A source that yields no rows, for an eliminated or closed reader.
    pub(crate) fn finished() -> Self {
        Self {
            calls: &mut [],
            initial: &[],
            live: &mut [],
            ordinality: None,
        }
    }

    pub(crate) fn close(&mut self) {
        self.live.fill(false);
    }

    /// The next row encoded as projected values in `arena`.
    pub(crate) fn next_encoded(&mut self, arena: &'a Arena) -> Result<Option<&'a [u8]>, SqlError> {
        let width = self.calls.iter().map(RowGenerator::width).sum::<usize>()
            + usize::from(self.ordinality.is_some());
        if width > crate::storage::MAX_COLUMNS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "function source output exceeds configured column capacity"
            ));
        }
        let mut values = [Datum::Null; crate::storage::MAX_COLUMNS];
        let mut any = false;
        let mut at = 0usize;
        for (call, live) in self.calls.iter_mut().zip(self.live.iter_mut()) {
            let columns = call.width();
            if *live {
                *live = call.next_row(arena, &mut values[at..at + columns])?;
                any |= *live;
            }
            at += columns;
        }
        if !any {
            self.close();
            return Ok(None);
        }
        if let Some(ordinal) = self.ordinality.as_mut() {
            values[at] = Datum::Int8(*ordinal);
            *ordinal += 1;
        }
        crate::sql::exec::encode_projected_pub(&values[..width], arena).map(Some)
    }
}
