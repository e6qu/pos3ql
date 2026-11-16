//! `LIKE` restrictions as `like_support.c` reads them: the pattern's fixed
//! prefix (`like_fixed_prefix`), the index conditions it gives
//! (`match_pattern_prefix`, `make_greater_string`) and the selectivity of
//! the match (`patternsel_common`, `prefix_selectivity`,
//! `like_selectivity`).

use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::types::Datum;

use super::{Comparison, Planner, clamp_probability};

/// `DEFAULT_MATCH_SEL`.
const DEFAULT_MATCH_SEL: f64 = 0.005;
const FIXED_CHAR_SEL: f64 = 0.20;
const ANY_CHAR_SEL: f64 = 0.9;
const FULL_WILDCARD_SEL: f64 = 5.0;

/// A pattern's fixed prefix: the characters before its first wildcard,
/// escapes removed; `exact` when the whole pattern is fixed.
pub(super) struct FixedPrefix<'p> {
    /// `None` without a fixed character (`Pattern_Prefix_None`).
    pub(super) prefix: Option<&'p str>,
    pub(super) exact: bool,
    /// `like_selectivity` of the pattern after the prefix.
    pub(super) rest: f64,
}

/// `like_fixed_prefix` of `pattern` with the default `\` escape. The
/// prefix is the pattern's own text when no escape precedes a fixed
/// character; otherwise its characters, unescaped, are held in `held`.
pub(super) fn fixed_prefix(pattern: &str) -> FixedPrefix<'_> {
    let bytes = pattern.as_bytes();
    let mut position = 0;
    let mut escaped = false;
    let mut end = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'%' | b'_' => break,
            b'\\' => {
                escaped = true;
                position += 1;
                if position >= bytes.len() {
                    break;
                }
            }
            _ => {}
        }
        position += 1;
        end = position;
    }
    let exact = position >= bytes.len();
    // An escaped prefix is outside the model's borrowed prefix: unescaping
    // would need a copy, so it reads as no prefix only when nothing
    // precedes the escape.
    let prefix = if escaped {
        None
    } else {
        Some(&pattern[..end]).filter(|prefix| !prefix.is_empty())
    };
    FixedPrefix {
        prefix,
        exact: exact && !escaped,
        rest: rest_selectivity(&bytes[position.min(bytes.len())..]),
    }
}

/// Whether the pattern holds an escape, which [`fixed_prefix`] does not
/// unescape.
pub(super) fn escapes(pattern: &str) -> bool {
    pattern.contains('\\')
}

/// `like_selectivity` of the pattern after its fixed prefix.
fn rest_selectivity(rest: &[u8]) -> f64 {
    let mut selectivity = 1.0;
    let mut position = rest
        .iter()
        .position(|&byte| byte != b'%' && byte != b'_')
        .unwrap_or(rest.len());
    while position < rest.len() {
        match rest[position] {
            b'%' => selectivity *= FULL_WILDCARD_SEL,
            b'_' => selectivity *= ANY_CHAR_SEL,
            b'\\' => {
                position += 1;
                if position >= rest.len() {
                    break;
                }
                selectivity *= FIXED_CHAR_SEL;
            }
            _ => selectivity *= FIXED_CHAR_SEL,
        }
        position += 1;
    }
    selectivity.min(1.0)
}

/// `make_greater_string` of an ASCII `prefix` under `C`: its last byte
/// incremented, dropping trailing bytes that cannot be; `None` when none
/// can.
pub(super) fn greater_string<'a>(
    prefix: &str,
    arena: &'a Arena,
) -> Result<Option<&'a str>, SqlError> {
    let bytes = prefix.as_bytes();
    let mut length = bytes.len();
    while length > 0 {
        // `pg_utf8_increment` of a one-byte character stops below 0x7F.
        if bytes[length - 1] < 0x7f {
            let out = arena
                .alloc_slice_with(length, |at| bytes[at])
                .map_err(|_| crate::sql::eval::arena_full())?;
            out[length - 1] += 1;
            return Ok(Some(
                core::str::from_utf8(out).expect("an ASCII prefix stays ASCII"),
            ));
        }
        length -= 1;
    }
    Ok(None)
}

impl Planner<'_, '_> {
    /// `patternsel_common` of `column LIKE pattern` (`NOT LIKE` when
    /// `negated`), or the shape outside the model.
    pub(super) fn like_selectivity(
        &self,
        column: usize,
        pattern: &str,
        negated: bool,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        if escapes(pattern) {
            return Ok(Err("a LIKE pattern with an escape"));
        }
        let variable = self.variable(column);
        let null_fraction = if variable.analyzed {
            variable.null_fraction
        } else {
            0.0
        };
        let fixed = fixed_prefix(pattern);
        let matches = |value: &Datum<'_>| -> Result<bool, SqlError> {
            match value {
                Datum::Text(text) => crate::sql::eval::like_match(text, pattern, false, Some('\\')),
                _ => Ok(false),
            }
        };
        let mut result = if fixed.exact {
            let prefix = fixed.prefix.unwrap_or("");
            match self.operator_selectivity(column, Comparison::Eq, Some(Datum::Text(prefix)))? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            }
        } else {
            // `histogram_selectivity`: the pattern applied to the
            // histogram's inner entries, when it has at least ten.
            let distribution = variable.distribution.filter(|_| variable.analyzed);
            let mut hist_size = 0usize;
            let mut selectivity = -1.0;
            if let Some(distribution) = distribution {
                hist_size = distribution.histogram_len();
                if hist_size >= 10 {
                    let mut matched = 0usize;
                    for index in 1..hist_size - 1 {
                        if matches(&distribution.histogram_value(index, variable.ctype)?)? {
                            matched += 1;
                        }
                    }
                    selectivity = matched as f64 / (hist_size - 2) as f64;
                }
            }
            if hist_size < 100 {
                let prefix_selectivity = match fixed.prefix {
                    Some(prefix) => match self.prefix_selectivity(&variable, column, prefix)? {
                        Ok(selectivity) => selectivity,
                        Err(shape) => return Ok(Err(shape)),
                    },
                    None => 1.0,
                };
                let heuristic = prefix_selectivity * fixed.rest;
                selectivity = if selectivity < 0.0 {
                    heuristic
                } else {
                    let weight = hist_size as f64 / 100.0;
                    selectivity * weight + heuristic * (1.0 - weight)
                };
            }
            selectivity = selectivity.clamp(0.0001, 0.9999);
            // `mcv_selectivity`: the pattern applied to each common value.
            let mut mcv_selectivity = 0.0;
            let mut common = 0.0;
            if let Some(distribution) = distribution {
                for index in 0..distribution.mcv_len() {
                    let frequency = f64::from(distribution.mcv_frequency(index));
                    if matches(&distribution.mcv_value(index, variable.ctype)?)? {
                        mcv_selectivity += frequency;
                    }
                    common += frequency;
                }
            }
            selectivity * (1.0 - null_fraction - common) + mcv_selectivity
        };
        if negated {
            result = 1.0 - result - null_fraction;
        }
        Ok(Ok(clamp_probability(result)))
    }

    /// `prefix_selectivity`: the histogram's share at or above `prefix` and
    /// below its greater string, at least the prefix's equality.
    fn prefix_selectivity(
        &self,
        variable: &super::VariableStatistics<'_>,
        column: usize,
        prefix: &str,
    ) -> Result<Result<f64, &'static str>, SqlError> {
        let outside = "a LIKE prefix range PostgreSQL places otherwise";
        let at_least = match self.histogram_selectivity(
            variable,
            column,
            Comparison::Ge,
            Datum::Text(prefix),
        )? {
            Ok(Some(selectivity)) => selectivity,
            Ok(None) => return Ok(Ok(DEFAULT_MATCH_SEL)),
            Err(()) => return Ok(Err(outside)),
        };
        let mut selectivity = at_least;
        if let Some(greater) = greater_string(prefix, self.arena)? {
            let below = match self.histogram_selectivity(
                variable,
                column,
                Comparison::Lt,
                Datum::Text(greater),
            )? {
                Ok(Some(selectivity)) => selectivity,
                Ok(None) => return Ok(Ok(DEFAULT_MATCH_SEL)),
                Err(()) => return Ok(Err(outside)),
            };
            selectivity = below + at_least - 1.0;
        }
        let equal =
            match self.operator_selectivity(column, Comparison::Eq, Some(Datum::Text(prefix)))? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            };
        Ok(Ok(selectivity.max(equal)))
    }
}
