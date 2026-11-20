//! PostgreSQL's `compute_scalar_stats` and `analyze_mcv_list`
//! (`src/backend/commands/analyze.c`, REL_18_6): the null fraction, width,
//! distinct-value estimate, most common values, histogram and correlation
//! of one column's ANALYZE sample. Values are sorted in place; the scratch
//! arrays come from the statement arena.

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::types::Datum;

/// PostgreSQL's `WIDTH_THRESHOLD`: wider varlena values join no MCV list or
/// histogram and count as distinct.
pub(crate) const WIDTH_THRESHOLD: usize = 1024;

/// One column of the sample: its sortable values with their position among
/// those values (PostgreSQL's `tupno`), and the counts of its other rows.
pub(crate) struct SampleColumn<'v, 'a> {
    pub(crate) values: &'v mut [(Datum<'a>, u32)],
    pub(crate) null_count: usize,
    pub(crate) nonnull_count: usize,
    pub(crate) toowide_count: usize,
    /// The summed stored width of the non-NULL values of a variable-width
    /// type, or the length of a fixed-width type.
    pub(crate) total_width: Result<f64, i32>,
}

/// `pg_statistic`'s values for one column.
pub(crate) struct ScalarStatistics<'a> {
    pub(crate) null_fraction: f32,
    pub(crate) width: i32,
    /// `stadistinct`: positive counts, negative fractions of the rows, 0
    /// for unknown.
    pub(crate) distinct: f32,
    pub(crate) mcv: &'a [(Datum<'a>, f32)],
    pub(crate) histogram: &'a [Datum<'a>],
    pub(crate) correlation: Option<f32>,
}

/// Computes the statistics of `column` from a sample of `sample_rows` of
/// the table's `total_rows`, keeping at most `target` most common values
/// and `target` histogram bins; `None` without rows.
pub(crate) fn compute<'a>(
    column: SampleColumn<'_, 'a>,
    sample_rows: usize,
    total_rows: f64,
    target: usize,
    compare: &mut dyn FnMut(&Datum<'a>, &Datum<'a>) -> Result<Ordering, SqlError>,
    arena: &'a Arena,
) -> Result<Option<ScalarStatistics<'a>>, SqlError> {
    let arena_full = crate::sql::eval::arena_full;
    let SampleColumn {
        values,
        null_count,
        nonnull_count,
        toowide_count,
        total_width,
    } = column;
    // `stawidth`: the average stored width of a variable-width type's
    // non-NULL values (0 when every value is NULL), else the type's length.
    let width = match total_width {
        Ok(total) if nonnull_count > 0 => (total / nonnull_count as f64) as i32,
        Ok(_) => 0,
        Err(length) => length,
    };
    if values.is_empty() {
        if nonnull_count > 0 {
            // Every value was too wide: assume a unique column.
            let null_fraction = (null_count as f64 / sample_rows as f64) as f32;
            return Ok(Some(ScalarStatistics {
                null_fraction,
                width,
                distinct: -(1.0 - null_fraction),
                mcv: &[],
                histogram: &[],
                correlation: None,
            }));
        }
        if null_count > 0 {
            return Ok(Some(ScalarStatistics {
                null_fraction: 1.0,
                width,
                distinct: 0.0,
                mcv: &[],
                histogram: &[],
                correlation: None,
            }));
        }
        return Ok(None);
    }
    // Sort by value, equal values by their sample position.
    let mut failure = None;
    values.sort_unstable_by(|left, right| {
        if failure.is_some() {
            return Ordering::Equal;
        }
        match compare(&left.0, &right.0) {
            Ok(Ordering::Equal) => left.1.cmp(&right.1),
            Ok(order) => order,
            Err(error) => {
                failure = Some(error);
                Ordering::Equal
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    let values_count = values.len();
    let num_mcv_target = target;
    // (count, first position) of the most common values, most common first.
    let track = arena
        .alloc_slice_with(num_mcv_target.max(1), |_| (0usize, 0usize))
        .map_err(|_| arena_full())?;
    let mut track_count = 0usize;
    let mut correlation_xy = 0.0f64;
    let mut distinct = 0usize;
    let mut multiple = 0usize;
    let mut duplicates = 0usize;
    for index in 0..values_count {
        correlation_xy += index as f64 * f64::from(values[index].1);
        duplicates += 1;
        let last_of_group =
            index + 1 == values_count || !compare(&values[index].0, &values[index + 1].0)?.is_eq();
        if !last_of_group {
            continue;
        }
        distinct += 1;
        if duplicates > 1 {
            multiple += 1;
            if track_count < num_mcv_target || duplicates > track[track_count - 1].0 {
                if track_count < num_mcv_target {
                    track_count += 1;
                }
                let mut slot = track_count - 1;
                while slot > 0 {
                    if duplicates <= track[slot - 1].0 {
                        break;
                    }
                    track[slot] = track[slot - 1];
                    slot -= 1;
                }
                track[slot] = (duplicates, index + 1 - duplicates);
            }
        }
        duplicates = 0;
    }
    let null_fraction = (null_count as f64 / sample_rows as f64) as f32;
    let mut stadistinct: f32 = if multiple == 0 {
        -(1.0 - null_fraction)
    } else if toowide_count == 0 && multiple == distinct {
        distinct as f32
    } else {
        // Haas and Stokes' Duj1 estimator over the non-NULL values.
        let f1 = (distinct - multiple + toowide_count) as f64;
        let d = f1 + multiple as f64;
        let n = (sample_rows - null_count) as f64;
        let big_n = total_rows * (1.0 - f64::from(null_fraction));
        let mut estimate = if big_n > 0.0 {
            (n * d) / ((n - f1) + f1 * n / big_n)
        } else {
            0.0
        };
        if estimate < d {
            estimate = d;
        }
        if estimate > big_n {
            estimate = big_n;
        }
        (estimate + 0.5).floor() as f32
    };
    if f64::from(stadistinct) > 0.1 * total_rows {
        stadistinct = (-(f64::from(stadistinct) / total_rows)) as f32;
    }
    let mut num_mcv = num_mcv_target;
    if track_count == distinct && toowide_count == 0 && stadistinct > 0.0 && track_count <= num_mcv
    {
        num_mcv = track_count;
    } else {
        num_mcv = num_mcv.min(track_count);
        if num_mcv > 0 {
            num_mcv = analyze_mcv_list(
                &track[..num_mcv],
                stadistinct,
                null_fraction,
                sample_rows,
                total_rows,
            );
        }
    }
    let mcv = arena
        .alloc_slice_with(num_mcv, |index| {
            (
                values[track[index].1].0,
                (track[index].0 as f64 / sample_rows as f64) as f32,
            )
        })
        .map_err(|_| arena_full())?;
    // A histogram when at least two distinct values remain outside the
    // most common ones.
    let num_bins = target;
    let mut num_hist = distinct - num_mcv;
    if num_hist > num_bins {
        num_hist = num_bins + 1;
    }
    let histogram: &[Datum<'a>] = if num_hist >= 2 {
        track[..num_mcv].sort_unstable_by_key(|&(_, first)| first);
        let kept = if num_mcv > 0 {
            let (mut source, mut destination, mut next) = (0usize, 0usize, 0usize);
            while source < values_count {
                let copy = if next < num_mcv {
                    let (count, first) = track[next];
                    if source >= first {
                        source = first + count;
                        next += 1;
                        continue;
                    }
                    first - source
                } else {
                    values_count - source
                };
                values.copy_within(source..source + copy, destination);
                source += copy;
                destination += copy;
            }
            destination
        } else {
            values_count
        };
        let delta = (kept - 1) / (num_hist - 1);
        let delta_fraction = (kept - 1) % (num_hist - 1);
        let (mut position, mut fraction) = (0usize, 0usize);
        let bounds = arena
            .alloc_slice_with(num_hist, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for bound in bounds.iter_mut() {
            *bound = values[position].0;
            position += delta;
            fraction += delta_fraction;
            if fraction >= num_hist - 1 {
                position += 1;
                fraction -= num_hist - 1;
            }
        }
        bounds
    } else {
        &[]
    };
    let correlation = (values_count > 1).then(|| {
        let count = values_count as f64;
        let sum = (count - 1.0) * count / 2.0;
        let square_sum = (count - 1.0) * count * (2.0 * count - 1.0) / 6.0;
        ((count * correlation_xy - sum * sum) / (count * square_sum - sum * sum)) as f32
    });
    Ok(Some(ScalarStatistics {
        null_fraction,
        width,
        distinct: stadistinct,
        mcv,
        histogram,
        correlation,
    }))
}

/// How many of the most common values `track` (counts, most common first)
/// are significantly more common than the values outside the list.
fn analyze_mcv_list(
    track: &[(usize, usize)],
    stadistinct: f32,
    null_fraction: f32,
    sample_rows: usize,
    total_rows: f64,
) -> usize {
    let mut num_mcv = track.len();
    if sample_rows as f64 == total_rows || total_rows <= 1.0 {
        return num_mcv;
    }
    let mut distinct_table = f64::from(stadistinct);
    if distinct_table < 0.0 {
        distinct_table = -distinct_table * total_rows;
    }
    let mut sum_count: f64 = track[..num_mcv - 1]
        .iter()
        .map(|&(count, _)| count as f64)
        .sum();
    while num_mcv > 0 {
        let mut selectivity =
            (1.0 - sum_count / sample_rows as f64 - f64::from(null_fraction)).clamp(0.0, 1.0);
        let other_distinct = distinct_table - (num_mcv - 1) as f64;
        if other_distinct > 1.0 {
            selectivity /= other_distinct;
        }
        let big_n = total_rows;
        let n = sample_rows as f64;
        let count = track[num_mcv - 1].0 as f64;
        let k = big_n * count / n;
        let variance = n * k * (big_n - k) * (big_n - n) / (big_n * big_n * (big_n - 1.0));
        let deviation = variance.sqrt();
        if count > selectivity * sample_rows as f64 + 2.0 * deviation + 0.5 {
            break;
        }
        num_mcv -= 1;
        if num_mcv == 0 {
            break;
        }
        sum_count -= track[num_mcv - 1].0 as f64;
    }
    num_mcv
}

/// The bytes of a variable-length value's data as PostgreSQL stores it,
/// without the length header. Text, `bytea`, `inet`, `cidr` and `numeric`
/// (base-10000 digits after a two-byte short or four-byte long header) are
/// exact;
/// other types count their row encoding's payload.
pub(crate) fn raw_value_length(value: &Datum<'_>) -> usize {
    match value {
        Datum::Text(text) | Datum::Bpchar(text) => text.len(),
        Datum::Bytea(bytes) => bytes.len(),
        // `inet_struct`: the family and mask length bytes, then the
        // address's own bytes (`SET_INET_VARSIZE`).
        Datum::Inet(address) | Datum::Cidr(address) => 2 + address.addr_len(),
        Datum::Numeric(number) => {
            if number.is_special() {
                2
            } else {
                let short = number.dscale <= 63 && (-64..=63).contains(&number.weight);
                (if short { 2 } else { 4 }) + 2 * number.ndigits()
            }
        }
        Datum::Array { element, raw } => array_data_length(*element, raw),
        Datum::TsVector(text) => tsvector_data_length(text.as_str()),
        // PostgreSQL's binary `jsonb`, range and multirange images, not
        // their text.
        Datum::Json { jsonb: true, .. } | Datum::Range { .. } | Datum::Multirange { .. } => {
            crate::sql::query::postgres_memory::varlena_payload(None, value)
                .expect("a jsonb, range or multirange image has a size")
        }
        other => {
            crate::storage::rowenc::encoded_len(core::slice::from_ref(other)).saturating_sub(3)
        }
    }
}

/// The canonical text of a `tsvector` as `(lexeme length, positions)` pairs,
/// each lexeme quoted with embedded quotes doubled and backslashes escaped.
pub(crate) fn tsvector_entries(text: &str, mut visit: impl FnMut(&[u8], usize)) {
    let bytes = text.as_bytes();
    let mut at = 0usize;
    let mut lexeme = [0u8; 2048];
    while at < bytes.len() {
        if bytes[at] == b' ' {
            at += 1;
            continue;
        }
        if bytes[at] != b'\'' {
            return;
        }
        at += 1;
        let mut length = 0usize;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' if at + 1 < bytes.len() => {
                    at += 1;
                }
                b'\'' if bytes.get(at + 1) == Some(&b'\'') => {
                    at += 1;
                }
                b'\'' => break,
                _ => {}
            }
            if length < lexeme.len() {
                lexeme[length] = bytes[at];
            }
            length += 1;
            at += 1;
        }
        at += 1;
        let mut positions = 0usize;
        if bytes.get(at) == Some(&b':') {
            positions = 1;
            while at < bytes.len() && bytes[at] != b' ' {
                if bytes[at] == b',' {
                    positions += 1;
                }
                at += 1;
            }
        }
        visit(&lexeme[..length.min(lexeme.len())], positions);
    }
}

/// The bytes of a stored `tsvector` after its four-byte varlena header: a
/// count, a four-byte entry per lexeme, then each lexeme and, when it has
/// positions, its count and positions at two-byte alignment.
pub(crate) fn tsvector_data_length(text: &str) -> usize {
    let mut entries = 0usize;
    let mut data = 0usize;
    tsvector_entries(text, |lexeme, positions| {
        entries += 1;
        data += lexeme.len();
        if positions > 0 {
            data = data.next_multiple_of(2) + 2 + 2 * positions;
        }
    });
    4 + 4 * entries + data
}

/// The bytes of a PostgreSQL array after its four-byte varlena header
/// (`ARR_OVERHEAD_*`: the type, flags, dimensions and lower bounds, a null
/// bitmap when an element is NULL, then each non-NULL element at its type's
/// alignment, a varlena with its four-byte header).
fn array_data_length(element: crate::sql::types::ArrElem, raw: &[u8]) -> usize {
    let ctype = element.to_coltype();
    let layout = ctype.physical_layout();
    let align = match layout.alignment {
        b'd' => 8,
        b'i' => 4,
        b's' => 2,
        _ => 1,
    };
    let count = crate::sql::array::len(raw);
    let dimensions = crate::sql::array::shape(raw).map_or(1, |shape| shape.dimension_count());
    let mut nulls = false;
    let mut data = 0usize;
    for index in 0..count {
        let value = crate::sql::array::get(raw, element, index).unwrap_or(Datum::Null);
        if value.is_null() {
            nulls = true;
            continue;
        }
        data = data.next_multiple_of(align)
            + if layout.length > 0 {
                layout.length as usize
            } else {
                raw_value_length(&value) + 4
            };
        data = data.next_multiple_of(align);
    }
    // The 16-byte header counts the varlena header, which `raw` excludes.
    let header =
        (16 + 8 * dimensions + if nulls { count.div_ceil(8) } else { 0 }).next_multiple_of(8) - 4;
    header + data
}

/// `VARSIZE_ANY` of a stored value of `raw` data bytes: a one-byte header
/// when the value fits a short varlena, else four.
pub(crate) fn stored_value_width(raw: usize) -> usize {
    if raw < 0x7f { raw + 1 } else { raw + 4 }
}
