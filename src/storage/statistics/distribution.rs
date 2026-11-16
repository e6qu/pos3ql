//! A column's value distribution as PostgreSQL's `compute_scalar_stats`
//! stores it in `pg_statistic`: the most common values with their
//! frequencies (`STATISTIC_KIND_MCV`), the histogram of the remaining
//! values (`STATISTIC_KIND_HISTOGRAM`) and the correlation between the
//! values' order and the rows' physical order
//! (`STATISTIC_KIND_CORRELATION`).
//!
//! Layout: format byte, a flags byte (bit 0:
//! a correlation is present), the correlation (`f32`), the most-common
//! count and the histogram count (`u16` each), each most-common frequency
//! (`f32`), then one `u32` offset per value plus the end offset, relative
//! to the value bytes that follow; each value is a one-column row encoding.
//! Integers are little-endian.

use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};

const FORMAT: u8 = 1;
const HEADER: usize = 1 + 1 + 4 + 2 + 2;

/// A parsed distribution, borrowing its image's bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ColumnDistribution<'a> {
    bytes: &'a [u8],
    correlation: Option<f32>,
    mcv: usize,
    histogram: usize,
}

impl<'a> ColumnDistribution<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER || bytes[0] != FORMAT || bytes[1] > 1 {
            return None;
        }
        let correlation = f32::from_le_bytes(bytes[2..6].try_into().ok()?);
        let mcv = usize::from(u16::from_le_bytes(bytes[6..8].try_into().ok()?));
        let histogram = usize::from(u16::from_le_bytes(bytes[8..10].try_into().ok()?));
        let parsed = Self {
            bytes,
            correlation: (bytes[1] == 1).then_some(correlation),
            mcv,
            histogram,
        };
        let values = parsed.values_at();
        if values > bytes.len() {
            return None;
        }
        let mut previous = 0usize;
        for index in 0..=mcv + histogram {
            let offset = parsed.offset(index);
            if offset < previous || values + offset > bytes.len() {
                return None;
            }
            previous = offset;
        }
        (values + previous == bytes.len()).then_some(parsed)
    }

    pub(crate) fn correlation(&self) -> Option<f32> {
        self.correlation
    }

    pub(crate) fn mcv_len(&self) -> usize {
        self.mcv
    }

    pub(crate) fn histogram_len(&self) -> usize {
        self.histogram
    }

    pub(crate) fn mcv_frequency(&self, index: usize) -> f32 {
        let at = HEADER + index * 4;
        f32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("parsed frequency"))
    }

    pub(crate) fn mcv_value(&self, index: usize, ctype: ColType) -> Result<Datum<'a>, SqlError> {
        self.value(index, ctype)
    }

    pub(crate) fn histogram_value(
        &self,
        index: usize,
        ctype: ColType,
    ) -> Result<Datum<'a>, SqlError> {
        self.value(self.mcv + index, ctype)
    }

    fn values_at(&self) -> usize {
        HEADER + self.mcv * 4 + (self.mcv + self.histogram + 1) * 4
    }

    fn offset(&self, index: usize) -> usize {
        let at = HEADER + self.mcv * 4 + index * 4;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("parsed offset")) as usize
    }

    fn value(&self, index: usize, ctype: ColType) -> Result<Datum<'a>, SqlError> {
        let values = self.values_at();
        let encoded = &self.bytes[values + self.offset(index)..values + self.offset(index + 1)];
        let mut out = [Datum::Null];
        crate::storage::rowenc::decode(encoded, &[ctype], &mut out)?;
        Ok(out[0])
    }
}

/// The encoded length of a distribution of `mcv` values and `histogram`
/// bounds.
pub(crate) fn encoded_len(mcv: &[(Datum<'_>, f32)], histogram: &[Datum<'_>]) -> usize {
    let values: usize = mcv
        .iter()
        .map(|(value, _)| value)
        .chain(histogram)
        .map(|value| crate::storage::rowenc::encoded_len(core::slice::from_ref(value)))
        .sum();
    HEADER + mcv.len() * 4 + (mcv.len() + histogram.len() + 1) * 4 + values
}

/// Encodes a distribution into `out`, exactly [`encoded_len`] bytes.
pub(crate) fn encode(
    correlation: Option<f32>,
    mcv: &[(Datum<'_>, f32)],
    histogram: &[Datum<'_>],
    out: &mut [u8],
) {
    debug_assert_eq!(out.len(), encoded_len(mcv, histogram));
    out[0] = FORMAT;
    out[1] = u8::from(correlation.is_some());
    out[2..6].copy_from_slice(&correlation.unwrap_or(0.0).to_le_bytes());
    out[6..8].copy_from_slice(&(mcv.len() as u16).to_le_bytes());
    out[8..10].copy_from_slice(&(histogram.len() as u16).to_le_bytes());
    let mut at = HEADER;
    for (_, frequency) in mcv {
        out[at..at + 4].copy_from_slice(&frequency.to_le_bytes());
        at += 4;
    }
    let offsets = at;
    let values = offsets + (mcv.len() + histogram.len() + 1) * 4;
    let mut written = 0usize;
    for (index, value) in mcv
        .iter()
        .map(|(value, _)| value)
        .chain(histogram)
        .enumerate()
    {
        out[offsets + index * 4..offsets + index * 4 + 4]
            .copy_from_slice(&(written as u32).to_le_bytes());
        let length = crate::storage::rowenc::encoded_len(core::slice::from_ref(value));
        crate::storage::rowenc::encode(
            core::slice::from_ref(value),
            &mut out[values + written..values + written + length],
        );
        written += length;
    }
    let end = offsets + (mcv.len() + histogram.len()) * 4;
    out[end..end + 4].copy_from_slice(&(written as u32).to_le_bytes());
}
