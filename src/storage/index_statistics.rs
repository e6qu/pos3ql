//! Fixed counter slots are admitted with catalog changes, then pinned by readers.

use super::*;

const MAX_IMPLICIT: usize = MAX_COLUMNS + MAX_UNIQUES + MAX_EXCLUSIONS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IndexStatisticsSource {
    table: catalog_sources::CatalogObjectIdentity,
    owner: IndexStatisticsOwner,
    database: DatabaseOid,
    oid: i32,
}

#[derive(Clone, Copy)]
pub(super) struct IndexCounterSlot {
    source: Option<IndexStatisticsSource>,
    generation: u64,
    pub(super) catalog_owned: bool,
    reservations: usize,
    readers: usize,
    pub(super) statistics: IndexCumulativeStatistics,
}

impl IndexCounterSlot {
    pub(super) const EMPTY: Self = Self {
        source: None,
        generation: 0,
        catalog_owned: false,
        reservations: 0,
        readers: 0,
        statistics: IndexCumulativeStatistics::EMPTY,
    };

    pub(super) fn reclaim(&mut self) {
        if !self.catalog_owned && self.reservations == 0 && self.readers == 0 {
            self.source = None;
            self.statistics = IndexCumulativeStatistics::EMPTY;
        }
    }
}

#[derive(Clone, Copy)]
struct CounterPosition {
    slot: usize,
    generation: u64,
}

impl CounterPosition {
    const EMPTY: Self = Self {
        slot: 0,
        generation: 0,
    };
}

/// An unpublished cohort owns its reservations even if later validation fails.
pub(super) struct IndexStatisticsAdmission<'a, const N: usize> {
    statistics: &'a Mutex<CumulativeStatistics>,
    positions: [CounterPosition; N],
    count: usize,
}

impl<const N: usize> IndexStatisticsAdmission<'_, N> {
    pub(super) fn commit(mut self) {
        let mut statistics = self
            .statistics
            .lock()
            .expect("cumulative statistics lock poisoned");
        for position in &self.positions[..self.count] {
            let entry = &mut statistics.indexes[position.slot];
            assert_eq!(entry.generation, position.generation);
            entry.catalog_owned = true;
            entry.reservations -= 1;
        }
        self.count = 0;
    }

    fn reserve(&mut self, source: IndexStatisticsSource) -> Result<(), SqlError> {
        let mut statistics = self
            .statistics
            .lock()
            .expect("cumulative statistics lock poisoned");
        let position = statistics
            .indexes
            .iter()
            .position(|entry| entry.source == Some(source));
        let position = match position {
            Some(position) => position,
            None => {
                let position = statistics
                    .indexes
                    .iter()
                    .position(|entry| entry.source.is_none() && entry.generation != u64::MAX)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "index statistics slots exhausted (index_statistics_slots={})",
                            statistics.indexes.len()
                        )
                    })?;
                let entry = &mut statistics.indexes[position];
                entry.generation += 1;
                entry.source = Some(source);
                entry.statistics = IndexCumulativeStatistics {
                    owner: Some(source.owner),
                    database: source.database,
                    oid: source.oid,
                    ..IndexCumulativeStatistics::EMPTY
                };
                position
            }
        };
        let entry = &mut statistics.indexes[position];
        entry.reservations = entry.reservations.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "index statistics reservation count exhausted"
            )
        })?;
        self.positions[self.count] = CounterPosition {
            slot: position,
            generation: entry.generation,
        };
        self.count += 1;
        Ok(())
    }
}

impl<const N: usize> Drop for IndexStatisticsAdmission<'_, N> {
    fn drop(&mut self) {
        if self.count == 0 {
            return;
        }
        let mut statistics = self
            .statistics
            .lock()
            .expect("cumulative statistics lock poisoned");
        for position in &self.positions[..self.count] {
            let entry = &mut statistics.indexes[position.slot];
            assert_eq!(entry.generation, position.generation);
            entry.reservations -= 1;
            entry.reclaim();
        }
    }
}

/// A retired index cannot lend its counter to a replacement while this exists.
pub(crate) struct IndexStatisticsCapture<'a> {
    statistics: &'a Mutex<CumulativeStatistics>,
    position: CounterPosition,
    source: IndexStatisticsSource,
}

impl IndexStatisticsCapture<'_> {
    pub(super) fn table_identity(&self) -> (DatabaseOid, u64) {
        (self.source.database, self.source.table.created_at())
    }

    pub(super) fn matches_table(&self, storage: &Storage, table: usize) -> Result<bool, SqlError> {
        if !core::ptr::eq(self.statistics, &storage.cumulative_statistics) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "index statistics belongs to another storage instance"
            ));
        }
        let identity = storage.tables[table].identity();
        Ok(self.source.table
            == storage.catalog_object_identity(
                catalog_sources::CatalogObjectSlot::Table(table),
                identity.database,
                identity.created_at,
            ))
    }

    pub(super) fn record_scan(&self, entries: usize, observed_at: i64) {
        let mut statistics = self
            .statistics
            .lock()
            .expect("cumulative statistics lock poisoned");
        let slot = &mut statistics.indexes[self.position.slot];
        assert_eq!(slot.generation, self.position.generation);
        let value = &mut slot.statistics;
        value.idx_scan = value.idx_scan.saturating_add(1);
        value.last_idx_scan = Some(observed_at);
        value.idx_tup_read = value.idx_tup_read.saturating_add(entries as u64);
    }

    pub(super) fn record_tuple(&self) {
        let mut statistics = self
            .statistics
            .lock()
            .expect("cumulative statistics lock poisoned");
        let slot = &mut statistics.indexes[self.position.slot];
        assert_eq!(slot.generation, self.position.generation);
        slot.statistics.idx_tup_fetch = slot.statistics.idx_tup_fetch.saturating_add(1);
    }
}

impl Drop for IndexStatisticsCapture<'_> {
    fn drop(&mut self) {
        let mut statistics = self
            .statistics
            .lock()
            .expect("cumulative statistics lock poisoned");
        let slot = &mut statistics.indexes[self.position.slot];
        assert_eq!(slot.generation, self.position.generation);
        slot.readers -= 1;
        slot.reclaim();
    }
}

impl Storage {
    pub(crate) fn index_statistics_bytes(slots: usize) -> Option<usize> {
        core::alloc::Layout::array::<IndexCounterSlot>(slots)
            .ok()
            .map(|layout| layout.size())
    }

    pub(super) fn reserve_implicit_index_statistics<B: DefinitionBacking>(
        &self,
        table: usize,
        database: DatabaseOid,
        created_at: u64,
        definition: &TableDefinition<B>,
    ) -> Result<IndexStatisticsAdmission<'_, MAX_IMPLICIT>, SqlError> {
        let mut admission = IndexStatisticsAdmission {
            statistics: &self.cumulative_statistics,
            positions: [CounterPosition::EMPTY; MAX_IMPLICIT],
            count: 0,
        };
        let source = self.catalog_object_identity(
            catalog_sources::CatalogObjectSlot::Table(table),
            database,
            created_at,
        );
        for identity in definition.implicit_index_identities().flatten() {
            admission.reserve(IndexStatisticsSource {
                table: source,
                owner: IndexStatisticsOwner::Implicit(table as u16),
                database,
                oid: identity.oid(),
            })?;
        }
        Ok(admission)
    }

    pub(super) fn reserve_explicit_index_statistics(
        &self,
        table: usize,
        database: DatabaseOid,
        table_created_at: u64,
        index_created_at: u64,
    ) -> Result<IndexStatisticsAdmission<'_, 1>, SqlError> {
        let mut admission = IndexStatisticsAdmission {
            statistics: &self.cumulative_statistics,
            positions: [CounterPosition::EMPTY; 1],
            count: 0,
        };
        let identity = RelationIdentity::parse(index_created_at).ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "index statistics source has an invalid creation identity"
            )
        })?;
        admission.reserve(IndexStatisticsSource {
            table: self.catalog_object_identity(
                catalog_sources::CatalogObjectSlot::Table(table),
                database,
                table_created_at,
            ),
            owner: IndexStatisticsOwner::Explicit(table as u16),
            database,
            oid: identity.oid(),
        })?;
        Ok(admission)
    }

    pub(crate) fn capture_index_statistics(
        &self,
        transaction: u32,
        table: usize,
        oid: i32,
    ) -> Result<IndexStatisticsCapture<'_>, SqlError> {
        let (owner, database, created_at) =
            crate::sql::catalog::index_statistics_source(self, transaction, oid).ok_or_else(
                || {
                    sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "index statistics source changed"
                    )
                },
            )?;
        if database != current_database()
            || !matches!(owner, IndexStatisticsOwner::Implicit(slot) | IndexStatisticsOwner::Explicit(slot) if usize::from(slot) == table)
        {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "index statistics table source changed"
            ));
        }
        let source = IndexStatisticsSource {
            table: self.catalog_object_identity(
                catalog_sources::CatalogObjectSlot::Table(table),
                database,
                created_at,
            ),
            owner,
            database,
            oid,
        };
        let mut statistics = self.cumulative_statistics();
        let position = statistics
            .indexes
            .iter()
            .position(|entry| {
                entry.source == Some(source) && (entry.catalog_owned || entry.reservations != 0)
            })
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "index statistics source was not admitted"
                )
            })?;
        let entry = &mut statistics.indexes[position];
        entry.readers = entry.readers.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "index statistics reader count exhausted"
            )
        })?;
        Ok(IndexStatisticsCapture {
            statistics: &self.cumulative_statistics,
            position: CounterPosition {
                slot: position,
                generation: entry.generation,
            },
            source,
        })
    }
}
