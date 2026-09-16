//! Conservative scalar envelopes for immutable range and network navigation.

use crate::store::{INTERVAL_KEY_BYTES, IntervalSummary, NavigationSummary};
use crate::{sql_err, stack_format};

use super::ast::BinaryOp;
use super::eval::{SqlError, sqlstate};
use super::types::{Datum, RangeKind};

const MIN_KEY: [u8; INTERVAL_KEY_BYTES] = [0; INTERVAL_KEY_BYTES];
const MAX_KEY: [u8; INTERVAL_KEY_BYTES] = [u8::MAX; INTERVAL_KEY_BYTES];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum IntervalPredicate {
    Always,
    Never,
    EmptyOnly,
    Intersects {
        lower: [u8; INTERVAL_KEY_BYTES],
        upper: [u8; INTERVAL_KEY_BYTES],
        include_empty: bool,
    },
}

impl IntervalPredicate {
    pub(crate) fn may_match(self, summary: NavigationSummary) -> bool {
        match (self, summary) {
            (Self::Never, _) | (_, NavigationSummary::Empty) => false,
            (Self::Always, _) | (_, NavigationSummary::Unbounded) => true,
            (Self::EmptyOnly, NavigationSummary::Interval(interval)) => interval.has_empty(),
            (
                Self::Intersects {
                    lower,
                    upper,
                    include_empty,
                },
                NavigationSummary::Interval(interval),
            ) => include_empty && interval.has_empty() || interval.intersects(lower, upper),
            // A kind mismatch can only reduce pruning, never hide a candidate.
            _ => true,
        }
    }
}

fn signed_key(value: i64) -> [u8; INTERVAL_KEY_BYTES] {
    let mut key = [0; INTERVAL_KEY_BYTES];
    key[..8].copy_from_slice(&((value as u64) ^ (1u64 << 63)).to_be_bytes());
    key
}

fn numeric_text_key(source: &str) -> Result<[u8; INTERVAL_KEY_BYTES], SqlError> {
    let text = source.trim();
    let mut key = [0; INTERVAL_KEY_BYTES];
    if text.eq_ignore_ascii_case("-infinity") {
        return Ok(key);
    }
    if text.eq_ignore_ascii_case("infinity") || text.eq_ignore_ascii_case("+infinity") {
        key[0] = 4;
        return Ok(key);
    }
    if text.eq_ignore_ascii_case("nan") {
        key[0] = 5;
        return Ok(key);
    }
    let (negative, unsigned) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (integer, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if integer.is_empty() && fraction.is_empty()
        || !integer.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "invalid canonical numeric range bound"
        ));
    }
    let integer = integer.trim_start_matches('0');
    let fraction = fraction.trim_end_matches('0');
    let (exponent, significant) = if !integer.is_empty() {
        (integer.len() as i32, (integer, fraction))
    } else if let Some(first) = fraction.bytes().position(|byte| byte != b'0') {
        (-(first as i32), (&fraction[first..], ""))
    } else {
        key[0] = 2;
        return Ok(key);
    };
    key[0] = if negative { 1 } else { 3 };
    let biased = exponent + 32_768;
    if !(0..=i32::from(u16::MAX)).contains(&biased) {
        // PostgreSQL NUMERIC admits exponents wider than the compact
        // navigation key. Collapse either tail to one sentinel. Keeping every
        // value in a tail equal is conservative; allowing its significant
        // digits to participate could reverse two values with different
        // out-of-band exponents.
        let high_magnitude = biased > i32::from(u16::MAX);
        if negative == high_magnitude {
            key[1..].fill(0);
        } else {
            key[1..].fill(u8::MAX);
        }
        return Ok(key);
    }
    let biased = biased as u16;
    let exponent_bytes = if negative { !biased } else { biased }.to_be_bytes();
    key[1..3].copy_from_slice(&exponent_bytes);
    for (at, digit) in significant
        .0
        .bytes()
        .chain(significant.1.bytes())
        .chain(core::iter::repeat(b'0'))
        .take(INTERVAL_KEY_BYTES - 3)
        .enumerate()
    {
        let digit = digit - b'0';
        key[3 + at] = if negative { 9 - digit } else { digit };
    }
    Ok(key)
}

fn scalar_key(datum: Datum<'_>) -> Result<Option<[u8; INTERVAL_KEY_BYTES]>, SqlError> {
    Ok(Some(match datum {
        Datum::Int2(value) => signed_key(i64::from(value)),
        Datum::Int4(value) | Datum::Date(value) => signed_key(i64::from(value)),
        Datum::Int8(value) | Datum::Timestamp(value) | Datum::Timestamptz(value) => {
            signed_key(value)
        }
        Datum::Numeric(value) => {
            let text = stack_format!(2100, "{}", value);
            if text.is_truncated() {
                return Ok(None);
            }
            numeric_text_key(text.as_str())?
        }
        _ => return Ok(None),
    }))
}

fn range_bound_key(text: &str, kind: RangeKind) -> Result<[u8; INTERVAL_KEY_BYTES], SqlError> {
    Ok(match kind {
        RangeKind::Int4 => signed_key(super::eval::parse_int_bounded(
            text,
            i32::MIN as i64,
            i32::MAX as i64,
            "integer",
        )?),
        RangeKind::Int8 => signed_key(super::eval::parse_int_bounded(
            text,
            i64::MIN,
            i64::MAX,
            "bigint",
        )?),
        RangeKind::Date => signed_key(i64::from(super::datetime::parse_date(text.trim())?)),
        RangeKind::Ts => signed_key(super::datetime::parse_timestamp(text.trim(), false)?),
        RangeKind::Tstz => signed_key(super::datetime::parse_timestamp(text.trim(), true)?),
        RangeKind::Num => numeric_text_key(text)?,
    })
}

fn range_summary(
    text: &str,
    kind: RangeKind,
    multirange: bool,
) -> Result<NavigationSummary, SqlError> {
    let bounds = super::range::outer_bounds(text, multirange)?;
    if bounds.empty {
        return Ok(NavigationSummary::Interval(IntervalSummary::empty_value()));
    }
    let lower = bounds
        .lower
        .map_or(Ok(MIN_KEY), |value| range_bound_key(value, kind))?;
    let upper = bounds
        .upper
        .map_or(Ok(MAX_KEY), |value| range_bound_key(value, kind))?;
    Ok(NavigationSummary::Interval(IntervalSummary::bounded(
        lower, upper,
    )))
}

fn network_summary(address: super::net::NetAddr) -> NavigationSummary {
    let network = address.to_network();
    let broadcast = address.broadcast();
    let mut lower = [0; INTERVAL_KEY_BYTES];
    let mut upper = [0; INTERVAL_KEY_BYTES];
    lower.copy_from_slice(&network.addr()[..INTERVAL_KEY_BYTES]);
    upper.copy_from_slice(&broadcast.addr()[..INTERVAL_KEY_BYTES]);
    NavigationSummary::Interval(IntervalSummary::bounded(lower, upper))
}

pub(crate) fn summary(datum: Datum<'_>) -> Result<NavigationSummary, SqlError> {
    match datum {
        Datum::Null => Ok(NavigationSummary::Empty),
        Datum::Inet(address) | Datum::Cidr(address) => Ok(network_summary(address)),
        Datum::Range { text, kind } => range_summary(text, kind, false),
        Datum::Multirange { text, kind } => range_summary(text, kind, true),
        _ => Ok(NavigationSummary::Unbounded),
    }
}

fn intersects(summary: NavigationSummary, include_empty: bool) -> IntervalPredicate {
    let NavigationSummary::Interval(interval) = summary else {
        return IntervalPredicate::Always;
    };
    if !interval.has_values() {
        return if include_empty && interval.has_empty() {
            IntervalPredicate::EmptyOnly
        } else {
            IntervalPredicate::Never
        };
    }
    let (lower, upper) = interval.bounds();
    IntervalPredicate::Intersects {
        lower,
        upper,
        include_empty,
    }
}

pub(crate) fn predicate(
    search: Datum<'_>,
    operator: BinaryOp,
) -> Result<IntervalPredicate, SqlError> {
    match search {
        Datum::Inet(address) | Datum::Cidr(address) => Ok(
            if matches!(
                operator,
                BinaryOp::Eq
                    | BinaryOp::Overlaps
                    | BinaryOp::Shl
                    | BinaryOp::Shr
                    | BinaryOp::NetContainedEq
                    | BinaryOp::NetContainsEq
            ) {
                intersects(network_summary(address), false)
            } else {
                IntervalPredicate::Always
            },
        ),
        Datum::Range { text, kind } => range_predicate(text, kind, false, operator),
        Datum::Multirange { text, kind } => range_predicate(text, kind, true, operator),
        value if operator == BinaryOp::Contains => Ok(scalar_key(value)?.map_or(
            IntervalPredicate::Always,
            |point| IntervalPredicate::Intersects {
                lower: point,
                upper: point,
                include_empty: false,
            },
        )),
        _ => Ok(IntervalPredicate::Always),
    }
}

fn range_predicate(
    text: &str,
    kind: RangeKind,
    multirange: bool,
    operator: BinaryOp,
) -> Result<IntervalPredicate, SqlError> {
    let summary = range_summary(text, kind, multirange)?;
    let NavigationSummary::Interval(interval) = summary else {
        unreachable!();
    };
    if !interval.has_values() {
        return Ok(match operator {
            BinaryOp::Contains => IntervalPredicate::Always,
            BinaryOp::Eq | BinaryOp::ContainedBy => IntervalPredicate::EmptyOnly,
            BinaryOp::Overlaps
            | BinaryOp::Shl
            | BinaryOp::Shr
            | BinaryOp::NotRightOf
            | BinaryOp::NotLeftOf
            | BinaryOp::Adjacent => IntervalPredicate::Never,
            _ => IntervalPredicate::Always,
        });
    }
    Ok(match operator {
        BinaryOp::Eq | BinaryOp::Contains | BinaryOp::Overlaps => intersects(summary, false),
        BinaryOp::ContainedBy => intersects(summary, true),
        // Positional predicates match values outside the search envelope; they
        // need a richer two-sided node summary to prune safely.
        _ => IntervalPredicate::Always,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::arena::Arena;
    use crate::mem::budget::Budget;
    use crate::sql::numeric::{self, Numeric};

    #[test]
    fn numeric_prefix_keys_never_reverse_postgresql_order() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "numeric navigation test", 1 << 20).unwrap();
        let values = [
            "-Infinity",
            "-1000000000000000000000000000001",
            "-10",
            "-0.000001",
            "0",
            "0.000001",
            "1.000000000000000000000000000001",
            "10",
            "1000000000000000000000000000001",
            "Infinity",
            "NaN",
        ];
        for pair in values.windows(2) {
            let left = Numeric::parse(pair[0], &arena).unwrap();
            let right = Numeric::parse(pair[1], &arena).unwrap();
            assert_eq!(numeric::compare(&left, &right), core::cmp::Ordering::Less);
            assert!(numeric_text_key(pair[0]).unwrap() <= numeric_text_key(pair[1]).unwrap());
        }
    }

    #[test]
    fn numeric_exponent_tails_collapse_without_reversing_order() {
        let huge = format!("1{}", "0".repeat(32_768));
        let huger = format!("9{}", "0".repeat(40_000));
        let tiny = format!("0.{}1", "0".repeat(32_769));
        let tinier = format!("0.{}9", "0".repeat(40_000));
        let mut positive_high = [u8::MAX; INTERVAL_KEY_BYTES];
        positive_high[0] = 3;
        let mut positive_low = [0; INTERVAL_KEY_BYTES];
        positive_low[0] = 3;
        assert_eq!(numeric_text_key(&huge).unwrap(), positive_high);
        assert_eq!(numeric_text_key(&huger).unwrap(), positive_high);
        assert_eq!(numeric_text_key(&tiny).unwrap(), positive_low);
        assert_eq!(numeric_text_key(&tinier).unwrap(), positive_low);
        assert_eq!(
            numeric_text_key(&format!("-{huger}")).unwrap(),
            [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            numeric_text_key(&format!("-{tinier}")).unwrap(),
            [
                1, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255
            ]
        );
    }

    #[test]
    fn empty_ranges_keep_only_semantically_possible_predicates() {
        let empty = Datum::Range {
            text: "empty",
            kind: RangeKind::Int4,
        };
        let indexed_empty = summary(empty).unwrap();
        assert!(
            predicate(empty, BinaryOp::Eq)
                .unwrap()
                .may_match(indexed_empty)
        );
        assert!(
            predicate(empty, BinaryOp::Contains)
                .unwrap()
                .may_match(indexed_empty)
        );
        assert!(
            !predicate(empty, BinaryOp::Overlaps)
                .unwrap()
                .may_match(indexed_empty)
        );
    }
}
