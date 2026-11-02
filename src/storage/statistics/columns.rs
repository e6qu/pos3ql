use core::num::NonZeroU16;

use super::super::{ColumnStatistics, MAX_RELATION_COLUMNS};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// Every stored record is analyzed and names a stable PostgreSQL attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AnalyzedColumn {
    attribute: NonZeroU16,
    null_fraction_ppm: u32,
    distinct_values: u64,
    distinct_fraction_ppm: u32,
    average_width: u32,
}

impl AnalyzedColumn {
    pub(crate) fn new(attribute: u16, statistics: ColumnStatistics) -> Result<Self, SqlError> {
        if !statistics.valid
            || usize::from(attribute) > MAX_RELATION_COLUMNS
            || statistics.null_fraction_ppm > 1_000_000
            || statistics.distinct_fraction_ppm > 1_000_000
        {
            return Err(invalid());
        }
        Ok(Self {
            attribute: NonZeroU16::new(attribute).ok_or_else(invalid)?,
            null_fraction_ppm: statistics.null_fraction_ppm,
            distinct_values: statistics.distinct_values,
            distinct_fraction_ppm: statistics.distinct_fraction_ppm,
            average_width: statistics.average_width,
        })
    }

    pub(crate) fn attribute(self) -> u16 {
        self.attribute.get()
    }

    pub(crate) fn statistics(self) -> ColumnStatistics {
        ColumnStatistics {
            valid: true,
            null_fraction_ppm: self.null_fraction_ppm,
            distinct_values: self.distinct_values,
            distinct_fraction_ppm: self.distinct_fraction_ppm,
            average_width: self.average_width,
        }
    }
}

fn invalid() -> SqlError {
    sql_err!(
        sqlstate::DATA_EXCEPTION,
        "invalid analyzed column statistics"
    )
}
