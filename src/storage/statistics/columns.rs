use core::num::NonZeroU16;

use super::super::{ColumnStatistics, MAX_RELATION_COLUMNS, SharedSlice};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// The key of a table column's statistics over the table and its
/// descendants, after every index's: no index's creation stamp reaches it.
pub(crate) const INHERITED: u64 = u64::MAX;

/// Every stored record is analyzed and names a stable PostgreSQL attribute:
/// a column of its table, or a key column of one of the table's indexes
/// (`index`, the index's creation stamp, nonzero), as `pg_statistic` keys an
/// index expression's statistics to the index. Its value distribution (`pg_statistic`'s most common values, histogram
/// and correlation slots), when ANALYZE computed one, is the byte range
/// `distribution_at..distribution_at + distribution_len` of its image's
/// value bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnalyzedColumn {
    /// The creation stamp of the index whose key column this is; 0 for a
    /// column of the table.
    index: u64,
    attribute: NonZeroU16,
    null_fraction_bits: u32,
    distinct_values: u64,
    distinct_fraction_bits: u32,
    average_width: u32,
    distribution_at: u32,
    distribution_len: u32,
}

impl AnalyzedColumn {
    pub(crate) fn new(attribute: u16, statistics: ColumnStatistics) -> Result<Self, SqlError> {
        if !statistics.valid
            || usize::from(attribute) > MAX_RELATION_COLUMNS
            || !statistics.fractions_valid()
        {
            return Err(invalid());
        }
        Ok(Self {
            index: 0,
            attribute: NonZeroU16::new(attribute).ok_or_else(invalid)?,
            null_fraction_bits: statistics.null_fraction.to_bits(),
            distinct_values: statistics.distinct_values,
            distinct_fraction_bits: statistics.distinct_fraction.to_bits(),
            average_width: statistics.average_width,
            distribution_at: 0,
            distribution_len: 0,
        })
    }

    /// The statistics of column `attribute` over the table and all its
    /// descendants (`stainherit`), as ANALYZE of a partitioned table or an
    /// inheritance parent gathers them.
    pub(crate) fn inherited(
        attribute: u16,
        statistics: ColumnStatistics,
    ) -> Result<Self, SqlError> {
        Ok(Self {
            index: INHERITED,
            ..Self::new(attribute, statistics)?
        })
    }

    /// The record of `attribute` keyed `index`: a table column (0), its
    /// inherited statistics ([`INHERITED`]) or an index's key column.
    pub(crate) fn keyed(
        index: u64,
        attribute: u16,
        statistics: ColumnStatistics,
    ) -> Result<Self, SqlError> {
        match index {
            0 => Self::new(attribute, statistics),
            INHERITED => Self::inherited(attribute, statistics),
            _ => Self::of_index(index, attribute, statistics),
        }
    }

    /// The statistics of key column `attribute` (from 1) of the index
    /// created at `index`.
    pub(crate) fn of_index(
        index: u64,
        attribute: u16,
        statistics: ColumnStatistics,
    ) -> Result<Self, SqlError> {
        if index == 0 || index == INHERITED || usize::from(attribute) > super::super::MAX_INDEX_COLS
        {
            return Err(invalid());
        }
        Ok(Self {
            index,
            ..Self::new(attribute, statistics)?
        })
    }

    /// This record with its distribution at `at..at + len` of its image's
    /// value bytes; an empty range records none.
    pub(crate) fn with_distribution(self, at: usize, len: usize) -> Result<Self, SqlError> {
        Ok(Self {
            distribution_at: u32::try_from(at).map_err(|_| invalid())?,
            distribution_len: u32::try_from(len).map_err(|_| invalid())?,
            ..self
        })
    }

    pub(crate) fn attribute(self) -> u16 {
        self.attribute.get()
    }

    /// The creation stamp of the index the record describes, 0 for a column
    /// of the table.
    pub(crate) fn index(self) -> u64 {
        self.index
    }

    /// The order of records in an image: the table's columns, then each
    /// index's key columns.
    pub(crate) fn key(self) -> (u64, u16) {
        (self.index, self.attribute.get())
    }

    pub(crate) fn statistics(self) -> ColumnStatistics {
        ColumnStatistics {
            valid: true,
            null_fraction: f32::from_bits(self.null_fraction_bits),
            distinct_values: self.distinct_values,
            distinct_fraction: f32::from_bits(self.distinct_fraction_bits),
            average_width: self.average_width,
        }
    }

    pub(crate) fn distribution_range(self) -> core::ops::Range<usize> {
        let at = self.distribution_at as usize;
        at..at + self.distribution_len as usize
    }

    /// The record without its place in an image's bytes.
    fn unplaced(self) -> Self {
        Self {
            distribution_at: 0,
            ..self
        }
    }
}

/// One table's analyzed column records, ordered by [`AnalyzedColumn::key`], and the value
/// bytes their distributions occupy. The records and their bytes form one
/// transactional image: ANALYZE, definition changes and recovery replace
/// them together.
#[derive(Clone, Debug)]
pub(crate) struct ColumnImages {
    records: SharedSlice<AnalyzedColumn>,
    values: SharedSlice<u8>,
}

impl PartialEq for ColumnImages {
    fn eq(&self, other: &Self) -> bool {
        self.records.len() == other.records.len()
            && self
                .records
                .iter()
                .zip(other.records.iter())
                .all(|(left, right)| {
                    left.unplaced() == right.unplaced()
                        && self.distribution_bytes(*left) == other.distribution_bytes(*right)
                })
    }
}

impl Eq for ColumnImages {}

impl core::ops::Deref for ColumnImages {
    type Target = [AnalyzedColumn];
    fn deref(&self) -> &Self::Target {
        self.records.as_slice()
    }
}

impl ColumnImages {
    pub(crate) const EMPTY: Self = Self {
        records: SharedSlice::EMPTY,
        values: SharedSlice::EMPTY,
    };

    /// Joins `records` with the value bytes `values` their distributions
    /// name; a record whose range lies outside them, or does not hold a
    /// distribution, is corrupt.
    pub(crate) fn new(
        records: SharedSlice<AnalyzedColumn>,
        values: SharedSlice<u8>,
    ) -> Result<Self, SqlError> {
        for record in records.iter() {
            let range = record.distribution_range();
            if range.end > values.len()
                || (!range.is_empty()
                    && super::distribution::ColumnDistribution::parse(&values[range]).is_none())
            {
                return Err(invalid());
            }
        }
        Ok(Self { records, values })
    }

    /// The distribution of `record`, one of this image's records.
    pub(crate) fn distribution(
        &self,
        record: AnalyzedColumn,
    ) -> Option<super::distribution::ColumnDistribution<'_>> {
        let range = record.distribution_range();
        if range.is_empty() {
            return None;
        }
        super::distribution::ColumnDistribution::parse(&self.values[range])
    }

    /// The distribution bytes of `record`, empty without one.
    pub(crate) fn distribution_bytes(&self, record: AnalyzedColumn) -> &[u8] {
        &self.values[record.distribution_range()]
    }
}

fn invalid() -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "invalid analyzed column statistics"
    )
}
