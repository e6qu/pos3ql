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
    /// `pg_class.relpages` and `reltuples`, which no statistics reset
    /// touches, and whether a commit still has to journal them.
    relation: Option<IndexRelationCounts>,
    relation_revision: u64,
    /// The catalog sequence when the counts were last written, which
    /// orders them against a TRUNCATE.
    relation_written: u64,
    relation_dirty: bool,
    /// The transaction whose commit journaled the counts, and their
    /// revision then.
    relation_staged: Option<(u32, u64)>,
    /// A measurement found the index's layout outside the model.
    relation_unmodelled: bool,
}

impl IndexCounterSlot {
    pub(super) const EMPTY: Self = Self {
        source: None,
        generation: 0,
        catalog_owned: false,
        reservations: 0,
        readers: 0,
        statistics: IndexCumulativeStatistics::EMPTY,
        relation: None,
        relation_revision: 0,
        relation_written: 0,
        relation_dirty: false,
        relation_staged: None,
        relation_unmodelled: false,
    };

    pub(super) fn reclaim(&mut self) {
        if !self.catalog_owned && self.reservations == 0 && self.readers == 0 {
            self.source = None;
            self.statistics = IndexCumulativeStatistics::EMPTY;
            self.relation = None;
            self.relation_dirty = false;
            self.relation_staged = None;
            self.relation_unmodelled = false;
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
                entry.relation = None;
                entry.relation_dirty = false;
                entry.relation_staged = None;
                entry.relation_unmodelled = false;
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

/// Where a captured index's counter is, without a reference to its
/// storage: a scan keeps it between reads and captures the counter again
/// for each read, which fails if the counter went to another index.
#[derive(Clone, Copy)]
pub(crate) struct IndexStatisticsTicket {
    position: CounterPosition,
    source: IndexStatisticsSource,
}

impl IndexStatisticsCapture<'_> {
    pub(crate) fn ticket(&self) -> IndexStatisticsTicket {
        IndexStatisticsTicket {
            position: self.position,
            source: self.source,
        }
    }

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
        relation_kind: super::IndexRelationKind,
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
            oid: identity.companion_oid(match relation_kind {
                super::IndexRelationKind::Table => super::FIRST_EXPLICIT_INDEX_OID,
                super::IndexRelationKind::Toast => super::FIRST_TOAST_INDEX_OID,
            }),
        })?;
        Ok(admission)
    }

    /// The counter `ticket` names, captured again.
    pub(crate) fn recapture_index_statistics(
        &self,
        ticket: IndexStatisticsTicket,
    ) -> Result<IndexStatisticsCapture<'_>, SqlError> {
        let mut statistics = self.cumulative_statistics();
        let entry = &mut statistics.indexes[ticket.position.slot];
        if entry.generation != ticket.position.generation || entry.source != Some(ticket.source) {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "index statistics source changed"
            ));
        }
        entry.readers = entry.readers.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "index statistics reader count exhausted"
            )
        })?;
        Ok(IndexStatisticsCapture {
            statistics: &self.cumulative_statistics,
            position: ticket.position,
            source: ticket.source,
        })
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

/// `pg_class.relpages` and `reltuples` of one index, which CREATE INDEX,
/// REINDEX, VACUUM and ANALYZE overwrite in place (PostgreSQL's
/// `vac_update_relstats`) and a later commit journals, and the layout of its
/// pages the planner reads.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct IndexRelationCounts {
    pub(crate) pages: u32,
    /// -1 until something counts the index's tuples.
    pub(crate) tuples: f32,
    pub(crate) layout: IndexLayout,
}

/// The pages an index's file holds, as last measured: its build's layout,
/// never fewer pages than an earlier measurement found until a rebuild
/// (PostgreSQL keeps deleted btree pages in the file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexLayout {
    pub(crate) pages: u64,
    /// The root's level (`btm_level`).
    pub(crate) height: u32,
    pub(crate) leaves: u64,
    /// [`Storage::relation_row_changes`] of the table when measured: the
    /// layout describes the index while no row has changed since.
    pub(crate) changes: u64,
}

/// One index's counts a commit journals.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IndexCountsImage {
    pub(crate) database: DatabaseOid,
    pub(crate) oid: i32,
    pub(crate) counts: IndexRelationCounts,
}

impl Storage {
    fn index_counter_position(
        statistics: &CumulativeStatistics,
        database: DatabaseOid,
        oid: i32,
    ) -> Option<usize> {
        statistics.indexes.iter().position(|entry| {
            entry.source.is_some()
                && (entry.catalog_owned || entry.reservations != 0)
                && entry.statistics.database == database
                && entry.statistics.oid == oid
        })
    }

    /// The relation counts of index `oid` of `database`, once something
    /// counted them.
    pub(crate) fn index_relation_counts(
        &self,
        database: DatabaseOid,
        oid: i32,
    ) -> Option<IndexRelationCounts> {
        let statistics = self.cumulative_statistics();
        Self::index_counter_position(&statistics, database, oid)
            .and_then(|position| statistics.indexes[position].relation)
    }

    /// Overwrites the relation counts of index `oid` of the current
    /// database in place; the next commit journals them.
    pub(crate) fn set_index_relation_counts(
        &self,
        oid: i32,
        counts: IndexRelationCounts,
    ) -> Result<(), SqlError> {
        let written = self.next_relation_counts_stamp()?;
        let mut statistics = self.cumulative_statistics();
        let position = Self::index_counter_position(&statistics, current_database(), oid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "index {} has no admitted statistics slot",
                    oid
                )
            })?;
        let entry = &mut statistics.indexes[position];
        entry.relation_written = written;
        entry.relation_revision = entry.relation_revision.checked_add(1).ok_or_else(|| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "index relation count revision exhausted"
            )
        })?;
        entry.relation = Some(counts);
        entry.relation_dirty = true;
        Ok(())
    }

    /// The catalog sequence when index `oid` of the current database last
    /// had its counts written (0 when never).
    pub(crate) fn index_counts_written(&self, oid: i32) -> u64 {
        let statistics = self.cumulative_statistics();
        Self::index_counter_position(&statistics, current_database(), oid)
            .map_or(0, |position| statistics.indexes[position].relation_written)
    }

    /// The first counter slot at or after `from` of an index of the current
    /// database nothing has measured, its table's slot and the index OID.
    pub(crate) fn next_unmeasured_index(&self, from: usize) -> Option<(usize, usize, i32)> {
        let database = current_database();
        let statistics = self.cumulative_statistics();
        statistics
            .indexes
            .iter()
            .enumerate()
            .skip(from)
            .find_map(|(slot, entry)| {
                let owner = entry.statistics.owner?;
                (entry.source.is_some()
                    && entry.relation.is_none()
                    && !entry.relation_unmodelled
                    && entry.statistics.database == database)
                    .then(|| {
                        let (IndexStatisticsOwner::Implicit(table)
                        | IndexStatisticsOwner::Explicit(table)) = owner;
                        (slot, usize::from(table), entry.statistics.oid)
                    })
            })
    }

    /// Records that index `oid` of the current database has a layout the
    /// model does not measure, so later sweeps pass it by.
    pub(crate) fn mark_index_counts_unmodelled(&self, oid: i32) {
        let mut statistics = self.cumulative_statistics();
        if let Some(position) = Self::index_counter_position(&statistics, current_database(), oid) {
            statistics.indexes[position].relation_unmodelled = true;
        }
    }

    /// Restores journaled or checkpointed counts, which need no journal.
    pub(crate) fn restore_index_relation_counts(
        &self,
        database: DatabaseOid,
        oid: i32,
        counts: IndexRelationCounts,
    ) -> Result<(), SqlError> {
        if counts.tuples.is_nan() || counts.tuples < -1.0 || counts.layout.pages == 0 {
            return Err(sql_err!(
                sqlstate::DATA_EXCEPTION,
                "invalid index relation counts"
            ));
        }
        let mut statistics = self.cumulative_statistics();
        let position =
            Self::index_counter_position(&statistics, database, oid).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "relation counts name unknown index {}",
                    oid
                )
            })?;
        let entry = &mut statistics.indexes[position];
        entry.relation = Some(counts);
        entry.relation_dirty = false;
        Ok(())
    }

    /// The counter slot at or after `from` holding counts no commit has
    /// journaled since they changed, their revision and the counts.
    pub(crate) fn next_unjournaled_index_counts(
        &self,
        from: usize,
    ) -> Option<(usize, u64, IndexCountsImage)> {
        let statistics = self.cumulative_statistics();
        let (slot, entry) = statistics
            .indexes
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, entry)| entry.relation_dirty && entry.relation.is_some())?;
        Some((
            slot,
            entry.relation_revision,
            IndexCountsImage {
                database: entry.statistics.database,
                oid: entry.statistics.oid,
                counts: entry.relation?,
            },
        ))
    }

    /// Records that `transaction`'s commit carries revision `revision` of
    /// the counts in counter slot `slot`.
    pub(crate) fn stage_index_counts(&self, slot: usize, revision: u64, transaction: u32) {
        let mut statistics = self.cumulative_statistics();
        let entry = &mut statistics.indexes[slot];
        if entry.relation_revision == revision {
            entry.relation_staged = Some((transaction, revision));
        }
    }

    /// Marks the counts `transaction` staged journaled once its commit is
    /// durable, unless they changed since.
    pub(crate) fn acknowledge_index_counts(&self, transaction: u32) {
        let mut statistics = self.cumulative_statistics();
        for entry in statistics.indexes.iter_mut() {
            if let Some((staged_by, revision)) = entry.relation_staged
                && staged_by == transaction
            {
                entry.relation_staged = None;
                if revision == entry.relation_revision {
                    entry.relation_dirty = false;
                }
            }
        }
    }

    /// The rows `txid` and committed transactions have inserted, updated or
    /// deleted in the table in slot `table`: an index layout measured at one
    /// count describes the index until it moves.
    pub(crate) fn relation_row_changes(&self, table: usize, txid: u32) -> u64 {
        let committed = self.relation_cumulative_statistics(table);
        let pending = self.relation_transaction_statistics(txid, table);
        committed
            .n_tup_ins
            .saturating_add(committed.n_tup_upd)
            .saturating_add(committed.n_tup_del)
            .saturating_add(pending.n_tup_ins)
            .saturating_add(pending.n_tup_upd)
            .saturating_add(pending.n_tup_del)
    }

    /// Visits every admitted index's counts, for a checkpoint.
    pub(crate) fn for_each_index_relation_counts(
        &self,
        each: &mut dyn FnMut(DatabaseOid, i32, IndexRelationCounts) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        let statistics = self.cumulative_statistics();
        for entry in statistics.indexes.iter() {
            if entry.catalog_owned
                && let Some(counts) = entry.relation
                && let Some(owner) = entry.statistics.owner
                && self.index_counts_durable(owner, entry.statistics.database, entry.statistics.oid)
            {
                each(entry.statistics.database, entry.statistics.oid, counts)?;
            }
        }
        Ok(())
    }

    /// Whether a checkpoint saves index `oid`'s definition: a committed
    /// index of a committed, non-temporary table.
    fn index_counts_durable(
        &self,
        owner: IndexStatisticsOwner,
        database: DatabaseOid,
        oid: i32,
    ) -> bool {
        let (IndexStatisticsOwner::Implicit(table) | IndexStatisticsOwner::Explicit(table)) = owner;
        let table = usize::from(table);
        let identity = self.tables[table].identity();
        if identity.database != database || !identity.existence.committed_exists() {
            return false;
        }
        let definition = self.table_def(table, 0);
        if definition.persistence == super::RelationPersistence::Temporary {
            return false;
        }
        match owner {
            IndexStatisticsOwner::Explicit(_) => self
                .checkpoint_indexes()
                .any(|(_, index)| index.relation_oid() == oid),
            IndexStatisticsOwner::Implicit(_) => {
                (0..definition.columns().len()).any(|column| {
                    definition
                        .column_index_identity(column)
                        .is_some_and(|identity| identity.oid() == oid)
                }) || definition.uniques().iter().any(|key| {
                    key.index_identity
                        .is_some_and(|identity| identity.oid() == oid)
                }) || definition.exclusions().iter().any(|exclusion| {
                    exclusion
                        .index_identity
                        .is_some_and(|identity| identity.oid() == oid)
                })
            }
        }
    }
}
