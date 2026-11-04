//! Generation capacity is admitted before WAL and consumed without allocation.

use super::{Storage, TablePublication};
use crate::mem::lease_pool::LeasePool;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(crate) enum IndexGenerationChange {
    Created(usize),
    Dropped(usize),
    Renamed(usize),
    State(usize),
}

impl IndexGenerationChange {
    fn slot(self) -> usize {
        match self {
            Self::Created(slot) | Self::Dropped(slot) | Self::Renamed(slot) | Self::State(slot) => {
                slot
            }
        }
    }
}

pub(super) struct GenerationReservation {
    transaction: u32,
    remaining: u64,
}

#[derive(Clone, Copy)]
pub(super) enum RetirementGeneration {
    Preserve,
    Advance,
}

impl TablePublication {
    pub(super) fn reserve_generations(
        &mut self,
        pool: &Arc<LeasePool>,
        transaction: u32,
        count: u64,
    ) -> Result<(), SqlError> {
        let selected = self.generation_reservations.as_ref().and_then(|records| {
            records
                .iter()
                .position(|record| record.transaction == transaction)
        });
        let prior = selected.map_or(0, |index| {
            self.generation_reservations.as_ref().unwrap()[index].remaining
        });
        let reserved = self
            .reserved_generations
            .checked_sub(prior)
            .and_then(|others| others.checked_add(count))
            .filter(|reserved| self.generation.checked_add(*reserved).is_some())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table publication generations exhausted"
                )
            })?;
        if count == 0 {
            if let Some(index) = selected {
                self.remove_generation_reservation(index);
            }
        } else if let Some(index) = selected {
            self.generation_reservations.as_mut().unwrap()[index].remaining = count;
        } else if let Some(records) = &mut self.generation_reservations {
            records
                .reserve_exact(records.len().checked_add(1).ok_or_else(|| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "table generation reservation count exhausted"
                    )
                })?)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
            records
                .push_reserved(GenerationReservation {
                    transaction,
                    remaining: count,
                })
                .expect("reserved generation owner capacity covers insertion");
        } else {
            let mut records = pool
                .lease(1)
                .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
            records
                .push_reserved(GenerationReservation {
                    transaction,
                    remaining: count,
                })
                .expect("one admitted generation owner");
            self.generation_reservations = Some(records);
        }
        self.reserved_generations = reserved;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn generation_reservation(&self, transaction: u32) -> u64 {
        self.generation_reservations
            .as_ref()
            .and_then(|records| {
                records
                    .iter()
                    .find(|record| record.transaction == transaction)
            })
            .map_or(0, |record| record.remaining)
    }

    fn remove_generation_reservation(&mut self, index: usize) {
        let records = self
            .generation_reservations
            .as_mut()
            .expect("selected generation owner exists");
        let last = records.len() - 1;
        records.as_mut_slice().swap(index, last);
        let _ = records.pop();
        if records.is_empty() {
            self.generation_reservations = None;
        }
    }

    pub(super) fn release_generations(&mut self, transaction: u32) -> u64 {
        let Some(index) = self.generation_reservations.as_ref().and_then(|records| {
            records
                .iter()
                .position(|record| record.transaction == transaction)
        }) else {
            return 0;
        };
        let count = self.generation_reservations.as_ref().unwrap()[index].remaining;
        self.reserved_generations -= count;
        self.remove_generation_reservation(index);
        count
    }

    pub(super) fn preview_reserved_generation(&self, transaction: u32) -> Result<u64, SqlError> {
        if !self
            .generation_reservations
            .as_ref()
            .is_some_and(|records| {
                records
                    .iter()
                    .any(|record| record.transaction == transaction && record.remaining != 0)
            })
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "table publication generation was not admitted before WAL durability"
            ));
        }
        Ok(self
            .generation
            .checked_add(1)
            .expect("reserved generation capacity cannot overflow"))
    }

    pub(super) fn publish_reserved_generation(&mut self, transaction: u32) -> u64 {
        let generation = self
            .preview_reserved_generation(transaction)
            .expect("durable publication owns its generation reservation");
        let records = self
            .generation_reservations
            .as_mut()
            .expect("admitted generation owner exists");
        let index = records
            .iter()
            .position(|record| record.transaction == transaction)
            .expect("admitted generation belongs to its transaction");
        records[index].remaining -= 1;
        self.reserved_generations -= 1;
        if records[index].remaining == 0 {
            self.remove_generation_reservation(index);
        }
        self.generation = generation;
        self.dirty = true;
        generation
    }

    pub(super) fn preview_unreserved_generation(&self) -> Result<u64, SqlError> {
        self.generation
            .checked_add(self.reserved_generations)
            .and_then(|used| used.checked_add(1))
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "table publication generations exhausted"
                )
            })?;
        Ok(self.generation + 1)
    }
}

impl Storage {
    fn generation_index_table(&self, change: IndexGenerationChange) -> Option<usize> {
        let index = self.index(change.slot());
        if matches!(index.relation, super::IndexRelationSource::Toast(_)) {
            return None;
        }
        if matches!(change, IndexGenerationChange::State(_))
            && index
                .pending_definition
                .is_none_or(|pending| pending.definition.kind.ready() == index.mutable.kind.ready())
        {
            return None;
        }
        match index.table_binding?.source.slot() {
            super::catalog_sources::CatalogObjectSlot::Table(table) => Some(table),
            _ => None,
        }
    }

    pub(super) fn preview_index_generation(
        &self,
        table: usize,
        admitted: Option<u32>,
    ) -> Result<(), SqlError> {
        let publication = self.tables[table].publication();
        match admitted {
            Some(transaction) => {
                publication.preview_reserved_generation(transaction)?;
            }
            None => {
                publication.preview_unreserved_generation()?;
            }
        }
        Ok(())
    }

    pub(super) fn publish_index_generation(
        &self,
        table: usize,
        admitted: Option<u32>,
    ) -> Result<(), SqlError> {
        match admitted {
            Some(transaction) => {
                self.tables[table]
                    .publication()
                    .publish_reserved_generation(transaction);
            }
            None => {
                self.tables[table].mark_dirty()?;
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_commit_generations(
        &self,
        transaction: u32,
        touched: impl Iterator<Item = (usize, u64)> + Clone,
        indexes: impl Iterator<Item = IndexGenerationChange> + Clone,
    ) -> Result<(), SqlError> {
        let metadata = self
            .definition_publication
            .metadata_generations(transaction);
        let candidates = metadata
            .iter()
            .map(|(table, _)| *table)
            .chain(touched.clone().map(|(table, _)| table))
            .chain(
                indexes
                    .clone()
                    .filter_map(|change| self.generation_index_table(change)),
            );
        let unique = candidates
            .clone()
            .enumerate()
            .filter(|(position, table)| {
                !candidates
                    .clone()
                    .take(*position)
                    .any(|prior| prior == *table)
            })
            .map(|(_, table)| table);
        let result = (|| {
            let existing = self.definition_publication.generation_tables(transaction);
            let tables = if existing.is_empty() {
                let mut tables = self
                    .metadata_pool
                    .lease(unique.clone().count())
                    .map_err(|error| sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "{}", error))?;
                for table in unique.clone() {
                    tables
                        .push_reserved(table)
                        .expect("unique table count covers targets");
                }
                tables.freeze()
            } else {
                if !existing.iter().copied().eq(unique.clone()) {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "transaction generation targets changed after admission"
                    ));
                }
                existing
            };
            self.definition_publication.retain_generation_tables(
                self,
                transaction,
                tables.clone(),
            )?;
            for &table in tables.iter() {
                let rows = touched
                    .clone()
                    .enumerate()
                    .filter(|(position, (slot, row))| {
                        *slot == table
                            && !touched
                                .clone()
                                .take(*position)
                                .any(|prior| prior == (*slot, *row))
                            && self
                                .selected_prepared_row_keys(table, *row, transaction)
                                .is_some()
                    })
                    .count();
                let mut count = u64::try_from(rows).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "table publication generation count exhausted"
                    )
                })?;
                count = count
                    .checked_add(
                        metadata
                            .iter()
                            .find(|(slot, _)| *slot == table)
                            .map_or(0, |(_, count)| *count),
                    )
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "table publication generation count exhausted"
                        )
                    })?;
                for (position, change) in indexes.clone().enumerate() {
                    if self.generation_index_table(change) != Some(table) {
                        continue;
                    }
                    if matches!(change, IndexGenerationChange::State(_))
                        && indexes.clone().take(position).any(|prior|
                            matches!(prior, IndexGenerationChange::State(slot) if slot == change.slot()))
                    {
                        continue;
                    }
                    if matches!(change, IndexGenerationChange::Renamed(_))
                        && indexes.clone().take(position).any(|prior| matches!(prior, IndexGenerationChange::Renamed(slot) if slot == change.slot()))
                    { continue }
                    let index = self.index(change.slot());
                    if matches!(change, IndexGenerationChange::Renamed(_))
                        && !index
                            .pending_name
                            .is_some_and(|name| name.txid == transaction)
                    {
                        continue;
                    }
                    let Some(binding) = index.table_binding else {
                        continue;
                    };
                    if binding.source.slot()
                        != super::catalog_sources::CatalogObjectSlot::Table(table)
                    {
                        continue;
                    }
                    count = count.checked_add(1).ok_or_else(|| {
                        sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "table publication generation count exhausted"
                        )
                    })?;
                }
                if count != 0 || touched.clone().any(|(slot, _)| slot == table) {
                    self.tables[table].publication().reserve_generations(
                        &self.metadata_pool,
                        transaction,
                        count,
                    )?;
                }
            }
            Ok(())
        })();
        if result.is_err() {
            self.release_commit_generations(transaction);
        }
        result
    }

    pub(crate) fn release_commit_generations(&self, transaction: u32) {
        let tables = self
            .definition_publication
            .release_generation_tables(transaction);
        for &table in tables.iter() {
            self.tables[table]
                .publication()
                .release_generations(transaction);
        }
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::sql::types::Datum;
    use crate::storage::tests::{make_def, test_budget, test_config};
    use crate::storage::{ColType, ForeignKey, MAX_COLUMNS, SqlName, TableDefBuilder};

    fn fixture() -> (Storage, usize, usize) {
        let mut config = test_config();
        config.max_tables = 2;
        config.row_overlay_slots = 16;
        config.table_metadata_bytes = 4 * 1024 * 1024;
        config.table_metadata_leases = 512;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let parent = storage
            .create_table(make_def(
                "generation_parent",
                &[("id", ColType::Int4, false)],
            ))
            .unwrap();
        let mut child = make_def("generation_child", &[("id", ColType::Int4, false)]);
        child.n_fkeys = 1;
        child.fkeys[0] = ForeignKey {
            parent_schema: SqlName::parse("public").unwrap(),
            parent: SqlName::parse("generation_parent").unwrap(),
            n_cols: 1,
            n_parent_cols: 1,
            ..ForeignKey::EMPTY
        };
        let child = storage.create_table(child).unwrap();
        (storage, parent, child)
    }

    #[test]
    fn complete_generation_admission_counts_source_and_dependent_rows_before_wal() {
        for selected in [0, 1] {
            let (storage, parent, child) = fixture();
            let table = [parent, child][selected];
            let home = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
            storage.write_pending(table, 1, 7, 1, Some(home)).unwrap();
            let (target, prior) = storage.select_table_definition(parent, 7);
            let mut definition = TableDefBuilder::from_definition(&prior);
            definition.name = SqlName::parse("generation_renamed").unwrap();
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(SqlName::parse("id").unwrap());
            let change = storage
                .write_table_def(
                    target,
                    definition,
                    &mapping,
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            crate::mem::guard::forbid_alloc(|| {
                storage
                    .prepare_commit_row_keys(7, [(table, 1)].into_iter())
                    .unwrap();
                storage
                    .prepare_definition_publication(7, [change].into_iter(), core::iter::empty())
                    .unwrap();
                storage.tables[table].publication().generation = u64::MAX - 1;
                let error = storage
                    .prepare_commit_generations(7, [(table, 1)].into_iter(), core::iter::empty())
                    .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert!(error.message.as_str().contains("generations exhausted"));
                assert_eq!(
                    storage.table_def(parent, 0).name.as_str(),
                    "generation_parent"
                );
                assert_eq!(
                    storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                    "generation_parent"
                );
                for slot in [parent, child] {
                    assert_eq!(storage.tables[slot].publication().reserved_generations, 0);
                }
                storage.tables[table].publication().generation = u64::MAX - 2;
                storage
                    .prepare_commit_generations(7, [(table, 1)].into_iter(), core::iter::empty())
                    .unwrap();
                storage
                    .prepare_commit_generations(7, [(table, 1)].into_iter(), core::iter::empty())
                    .unwrap();
                assert_eq!(storage.tables[table].publication().reserved_generations, 2);
                storage.commit_table_def(change).unwrap();
                storage.publish_row(table, 1, 7, 1, false).unwrap();
                storage.finish_definition_publication(7).unwrap();
                assert_eq!(storage.tables[table].publication().generation, u64::MAX);
                assert_eq!(storage.tables[table].publication().reserved_generations, 0);
                assert_eq!(
                    storage.table_def(child, 0).fkeys()[0].parent.as_str(),
                    "generation_renamed"
                );
                storage.release_commit_generations(7);
                storage.release_object_locks(7);
            });
        }
    }

    #[test]
    fn generation_quotas_share_capacity_without_serializing_independent_writers() {
        let (storage, parent, _) = fixture();
        storage.tables[parent].publication().generation = u64::MAX - 2;
        crate::mem::guard::forbid_alloc(|| {
            storage.tables[parent]
                .publication()
                .reserve_generations(&storage.metadata_pool, 7, 1)
                .unwrap();
            storage.tables[parent]
                .publication()
                .reserve_generations(&storage.metadata_pool, 9, 1)
                .unwrap();
            assert_eq!(
                storage.tables[parent]
                    .publication()
                    .reserve_generations(&storage.metadata_pool, 11, 1)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                storage.tables[parent]
                    .publication()
                    .publish_reserved_generation(9),
                u64::MAX - 1
            );
            assert_eq!(
                storage.tables[parent].mark_dirty().unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(
                storage.tables[parent]
                    .publication()
                    .publish_reserved_generation(7),
                u64::MAX
            );
            let publication = storage.tables[parent].publication();
            assert_eq!(publication.reserved_generations, 0);
            assert!(publication.generation_reservations.is_none());
        });
    }

    #[test]
    fn repeated_row_writes_use_one_ticket_and_cancellation_returns_reserved_capacity() {
        let (storage, parent, _) = fixture();
        let first = storage.heap.append_row(&[Datum::Int4(1)]).unwrap();
        let second = storage.heap.append_row(&[Datum::Int4(2)]).unwrap();
        storage.write_pending(parent, 1, 7, 1, Some(first)).unwrap();
        storage
            .write_pending(parent, 1, 7, 2, Some(second))
            .unwrap();
        storage.tables[parent].publication().generation = u64::MAX - 1;
        crate::mem::guard::forbid_alloc(|| {
            storage
                .prepare_commit_row_keys(7, [(parent, 1), (parent, 1)].into_iter())
                .unwrap();
            storage
                .prepare_commit_generations(
                    7,
                    [(parent, 1), (parent, 1)].into_iter(),
                    core::iter::empty(),
                )
                .unwrap();
            assert_eq!(storage.tables[parent].publication().reserved_generations, 1);
            storage.release_commit_generations(7);
            assert_eq!(storage.tables[parent].publication().reserved_generations, 0);
            storage
                .prepare_commit_generations(
                    7,
                    [(parent, 1), (parent, 1)].into_iter(),
                    core::iter::empty(),
                )
                .unwrap();
            storage.publish_row(parent, 1, 7, 1, false).unwrap();
            storage.publish_row(parent, 1, 7, 1, false).unwrap();
            assert_eq!(storage.tables[parent].publication().generation, u64::MAX);
            assert!(
                storage.tables[parent]
                    .publication()
                    .generation_reservations
                    .is_none()
            );
        });
    }
}
