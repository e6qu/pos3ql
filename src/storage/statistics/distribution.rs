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
//! Integers are little-endian. An array column's image appends its element
//! statistics (flag bit 1): the counts of tracked elements, of their
//! frequencies and of the distinct-count histogram (`u16` each), then those
//! `f32` numbers, then one `u32` offset per element plus the end offset and
//! the elements' one-column row encodings.

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
    /// The element statistics' start, when the image has them.
    elements: Option<ElementTail>,
}

/// Where an image's element statistics lie and how many they hold.
#[derive(Clone, Copy, Debug)]
struct ElementTail {
    at: usize,
    values: usize,
    frequencies: usize,
    counts: usize,
}

const FLAG_CORRELATION: u8 = 1;
const FLAG_ELEMENTS: u8 = 2;
const ELEMENT_HEADER: usize = 6;

impl<'a> ColumnDistribution<'a> {
    pub(crate) fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER
            || bytes[0] != FORMAT
            || bytes[1] & !(FLAG_CORRELATION | FLAG_ELEMENTS) != 0
        {
            return None;
        }
        let correlation = f32::from_le_bytes(bytes[2..6].try_into().ok()?);
        let mcv = usize::from(u16::from_le_bytes(bytes[6..8].try_into().ok()?));
        let histogram = usize::from(u16::from_le_bytes(bytes[8..10].try_into().ok()?));
        let parsed = Self {
            bytes,
            correlation: (bytes[1] & FLAG_CORRELATION != 0).then_some(correlation),
            mcv,
            histogram,
            elements: None,
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
        let end = values + previous;
        if bytes[1] & FLAG_ELEMENTS == 0 {
            return (end == bytes.len()).then_some(parsed);
        }
        let head = bytes.get(end..end + ELEMENT_HEADER)?;
        let counts = |at: usize| usize::from(u16::from_le_bytes([head[at], head[at + 1]]));
        let tail = ElementTail {
            at: end + ELEMENT_HEADER,
            values: counts(0),
            frequencies: counts(2),
            counts: counts(4),
        };
        let offsets = tail.at + (tail.frequencies + tail.counts) * 4;
        let element_values = offsets + (tail.values + 1) * 4;
        if element_values > bytes.len() {
            return None;
        }
        let mut last = 0usize;
        for index in 0..=tail.values {
            let offset = usize::try_from(u32::from_le_bytes(
                bytes[offsets + index * 4..offsets + index * 4 + 4]
                    .try_into()
                    .ok()?,
            ))
            .ok()?;
            if offset < last {
                return None;
            }
            last = offset;
        }
        (element_values + last == bytes.len()).then_some(Self {
            elements: Some(tail),
            ..parsed
        })
    }

    /// The tracked elements, their frequency numbers and the distinct-count
    /// histogram counts; zero without element statistics.
    pub(crate) fn element_len(&self) -> usize {
        self.elements.map_or(0, |tail| tail.values)
    }

    pub(crate) fn element_numbers_len(&self) -> usize {
        self.elements.map_or(0, |tail| tail.frequencies)
    }

    pub(crate) fn element_counts_len(&self) -> usize {
        self.elements.map_or(0, |tail| tail.counts)
    }

    fn element_number(&self, tail: ElementTail, index: usize) -> f32 {
        let at = tail.at + index * 4;
        f32::from_le_bytes(self.bytes[at..at + 4].try_into().expect("parsed number"))
    }

    pub(crate) fn element_frequency(&self, index: usize) -> f32 {
        self.element_number(self.elements.expect("element statistics"), index)
    }

    pub(crate) fn element_count_number(&self, index: usize) -> f32 {
        let tail = self.elements.expect("element statistics");
        self.element_number(tail, tail.frequencies + index)
    }

    pub(crate) fn element_value(
        &self,
        index: usize,
        ctype: ColType,
    ) -> Result<Datum<'a>, SqlError> {
        let tail = self.elements.expect("element statistics");
        let offsets = tail.at + (tail.frequencies + tail.counts) * 4;
        let values = offsets + (tail.values + 1) * 4;
        let offset = |index: usize| {
            u32::from_le_bytes(
                self.bytes[offsets + index * 4..offsets + index * 4 + 4]
                    .try_into()
                    .expect("parsed offset"),
            ) as usize
        };
        let encoded = &self.bytes[values + offset(index)..values + offset(index + 1)];
        let mut out = [Datum::Null];
        crate::storage::rowenc::decode(encoded, &[ctype], &mut out)?;
        Ok(out[0])
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
pub(crate) fn encoded_len(
    mcv: &[(Datum<'_>, f32)],
    histogram: &[Datum<'_>],
    elements: Option<&super::elements::ElementSet<'_>>,
) -> usize {
    let tail = elements.map_or(0, |set| {
        ELEMENT_HEADER
            + (set.frequencies.len() + set.counts.len()) * 4
            + (set.values.len() + 1) * 4
            + set
                .values
                .iter()
                .map(|value| crate::storage::rowenc::encoded_len(core::slice::from_ref(value)))
                .sum::<usize>()
    });
    let values: usize = mcv
        .iter()
        .map(|(value, _)| value)
        .chain(histogram)
        .map(|value| crate::storage::rowenc::encoded_len(core::slice::from_ref(value)))
        .sum();
    HEADER + mcv.len() * 4 + (mcv.len() + histogram.len() + 1) * 4 + values + tail
}

/// Encodes a distribution into `out`, exactly [`encoded_len`] bytes.
pub(crate) fn encode(
    correlation: Option<f32>,
    mcv: &[(Datum<'_>, f32)],
    histogram: &[Datum<'_>],
    elements: Option<&super::elements::ElementSet<'_>>,
    out: &mut [u8],
) {
    debug_assert_eq!(out.len(), encoded_len(mcv, histogram, elements));
    out[0] = FORMAT;
    out[1] = u8::from(correlation.is_some()) | if elements.is_some() { FLAG_ELEMENTS } else { 0 };
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
    let Some(set) = elements else {
        return;
    };
    let mut at = end + 4 + written;
    out[at..at + 2].copy_from_slice(&(set.values.len() as u16).to_le_bytes());
    out[at + 2..at + 4].copy_from_slice(&(set.frequencies.len() as u16).to_le_bytes());
    out[at + 4..at + 6].copy_from_slice(&(set.counts.len() as u16).to_le_bytes());
    at += ELEMENT_HEADER;
    for number in set.frequencies.iter().chain(set.counts) {
        out[at..at + 4].copy_from_slice(&number.to_le_bytes());
        at += 4;
    }
    let offsets = at;
    let values = offsets + (set.values.len() + 1) * 4;
    let mut written = 0usize;
    for (index, value) in set.values.iter().enumerate() {
        out[offsets + index * 4..offsets + index * 4 + 4]
            .copy_from_slice(&(written as u32).to_le_bytes());
        let length = crate::storage::rowenc::encoded_len(core::slice::from_ref(value));
        crate::storage::rowenc::encode(
            core::slice::from_ref(value),
            &mut out[values + written..values + written + length],
        );
        written += length;
    }
    let last = offsets + set.values.len() * 4;
    out[last..last + 4].copy_from_slice(&(written as u32).to_le_bytes());
}
