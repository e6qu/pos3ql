//! Index keys keep one table shape through decoding, evaluation and publication.

use super::*;

pub(super) struct ValueKeyPreparation {
    definitions: TableDefinitionImages,
    target: TableDefinitionTarget,
    layout: row_layout::RowLayout,
}

impl ValueKeyPreparation {
    pub(super) fn capture(storage: &Storage, table: usize) -> Result<Self, SqlError> {
        let definitions = storage.table_definition_images();
        let (target, _) = definitions.selection(storage, table, 0)?;
        let (definition, publication) = row_ownership::acquire(|| {
            Ok((
                storage.tables[table].definition.try_read_for_batch()?,
                row_ownership::try_mutex(
                    &storage.tables[table].publication,
                    "table publication lock poisoned",
                )?,
            ))
        });
        if !target.matches_definition(storage, &definition) {
            return Err(Self::changed());
        }
        Ok(Self {
            definitions,
            target,
            layout: publication.row_layout,
        })
    }

    fn changed() -> SqlError {
        sql_err!(
            sqlstate::SERIALIZATION_FAILURE,
            "table definition changed while index keys were being prepared"
        )
    }

    pub(super) fn definition(&self) -> &TableDef {
        self.definitions
            .retained_definition(self.target.table(), 0)
            .expect("key preparation owns its selected definition")
    }

    pub(super) fn database(&self) -> DatabaseOid {
        self.definitions
            .retained_identity(self.target.table(), 0)
            .expect("key preparation owns its selected identity")
            .database
    }

    pub(super) fn validate_definition(
        &self,
        storage: &Storage,
        definition: &table_definitions::DefinitionState,
    ) -> Result<(), SqlError> {
        if self.target.matches_definition(storage, definition) {
            Ok(())
        } else {
            Err(Self::changed())
        }
    }

    pub(super) fn validate_row(&self, home: &RowByteRead) -> Result<(), SqlError> {
        let snapshot = match &home.0 {
            RowByteSource::Visible(image) => image.snapshot(),
            RowByteSource::Snapshot(snapshot) => *snapshot,
            // Physical metadata is supplied by exclusive replay or maintenance.
            RowByteSource::Physical(_) => return Ok(()),
        };
        let identity = self
            .definitions
            .retained_identity(self.target.table(), 0)
            .expect("key preparation retains its table lifetime");
        if snapshot.created_at != identity.created_at || snapshot.row_layout != self.layout {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "retained row layout does not match the selected index definition"
            ));
        }
        Ok(())
    }

    pub(super) fn index(&self, storage: &Storage, created_at: u64) -> Result<IndexDef, SqlError> {
        let (definition, indexes) = row_ownership::acquire(|| {
            Ok((
                storage.tables[self.target.table()]
                    .definition
                    .try_read_for_batch()?,
                row_ownership::try_mutex(&storage.index_catalog, "index catalog lock poisoned")?,
            ))
        });
        self.validate_definition(storage, &definition)?;
        indexes
            .iter()
            .find(|index| {
                index.ddl_state != CatalogDdlState::Absent
                    && index.created_at == created_at
                    && index.database == definition.identity.database
            })
            .copied()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "index definition changed while its keys were being prepared"
                )
            })
    }

    pub(super) fn binding(&self, storage: &Storage, binding: usize) -> Result<Enforcer, SqlError> {
        let table = &storage.tables[self.target.table()];
        let (definition, publication) = row_ownership::acquire(|| {
            Ok((
                table.definition.try_read_for_batch()?,
                row_ownership::try_mutex(&table.publication, "table publication lock poisoned")?,
            ))
        });
        self.validate_definition(storage, &definition)?;
        publication
            .enforcers
            .get(binding)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "selected index binding no longer exists"
                )
            })
    }
}

pub(super) struct PreparedRowKeys {
    target: TableDefinitionTarget,
    bindings: [Option<Enforcer>; MAX_VALUE_ENFORCERS],
    pub(super) hashes: [(usize, u64); MAX_VALUE_ENFORCERS],
    pub(super) count: usize,
}

pub(super) struct PreparedCommittedRow {
    pub(super) table: usize,
    pub(super) rowid: u64,
    pub(super) transaction: u32,
    pub(super) head: u64,
    pub(super) keys: Option<PreparedRowKeys>,
}

impl PreparedRowKeys {
    pub(super) fn capture(
        storage: &Storage,
        table: usize,
        rowid: u64,
        home: Option<VisibleRowHome>,
    ) -> Result<Self, SqlError> {
        let definition = storage.table_def(table, 0);
        let target = storage.table_definition_target(table, 0, &definition);
        let (bindings, layout) = {
            let publication = storage.tables[table].publication();
            (publication.enforcers.clone(), publication.row_layout)
        };
        let plain = bindings
            .iter()
            .flatten()
            .all(|binding| binding.index_created_at.is_none());
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        let n_columns = if plain {
            definition.schema(&mut schema)
        } else {
            0
        };
        let created_at = definition.identity().created_at;
        drop(definition);
        let mut result = Self {
            target,
            bindings,
            hashes: [(0, 0); MAX_VALUE_ENFORCERS],
            count: 0,
        };
        let count = result
            .bindings
            .iter()
            .take_while(|entry| entry.is_some())
            .count();
        if let Some(home) = home.filter(|_| count != 0) {
            if plain {
                let snapshot = home.snapshot();
                if snapshot.created_at != created_at || snapshot.row_layout != layout {
                    return Err(sql_err!(
                        sqlstate::SERIALIZATION_FAILURE,
                        "retained row layout does not match the selected index definition"
                    ));
                }
                storage.with_row_bytes(table, rowid, home, |bytes| {
                    let mut values = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, &schema[..n_columns], &mut values)?;
                    for (binding, enforcer) in result.bindings[..count].iter().enumerate() {
                        let enforcer = enforcer.as_ref().expect("selected binding");
                        if enforcer
                            .columns()
                            .iter()
                            .any(|&column| values[usize::from(column)].is_null())
                        {
                            continue;
                        }
                        result.hashes[result.count] = (
                            binding,
                            hash_key_collated_catalog(
                                storage,
                                enforcer.evaluation_txid,
                                &values[..n_columns],
                                enforcer.columns(),
                                &enforcer.collations[..enforcer.n_cols],
                            )?,
                        );
                        result.count += 1;
                    }
                    Ok(())
                })?;
                return Ok(result);
            }
            let selection = ValueKeyPreparation::capture(storage, table)?;
            let definition = storage.tables[table].definition.read();
            result.validate_definition(storage, &definition)?;
            selection.validate_definition(storage, &definition)?;
            drop(definition);
            result.count = storage.row_enforcer_hashes_for(
                table,
                rowid,
                home,
                &selection,
                &result.bindings[..count],
                &mut result.hashes,
            )?;
        }
        Ok(result)
    }

    pub(super) fn validate_definition(
        &self,
        storage: &Storage,
        definition: &table_definitions::DefinitionState,
    ) -> Result<(), SqlError> {
        if self.target.matches_definition(storage, definition) {
            Ok(())
        } else {
            Err(ValueKeyPreparation::changed())
        }
    }

    pub(super) fn validate_bindings(&self, publication: &TablePublication) -> Result<(), SqlError> {
        if same_bindings(&self.bindings, &publication.enforcers, false) {
            Ok(())
        } else {
            Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "index bindings changed while row publication was being prepared"
            ))
        }
    }
}

pub(super) fn same_bindings(
    selected: &[Option<Enforcer>; MAX_VALUE_ENFORCERS],
    current: &[Option<Enforcer>; MAX_VALUE_ENFORCERS],
    include_publication_state: bool,
) -> bool {
    selected
        .iter()
        .zip(current)
        .all(|(selected, current)| match (selected, current) {
            (None, None) => true,
            (Some(selected), Some(current)) => {
                // Selected leases prevent these pool slots from being reused.
                selected.cache.slot() == current.cache.slot()
                    && (!include_publication_state || selected.metadata == current.metadata)
            }
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::{make_def, test_budget, test_config};

    fn fixture(caches: usize) -> (Storage, usize) {
        let mut config = test_config();
        config.max_value_indexes = caches;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut definition = make_def(
            "key_orders",
            &[
                ("id", ColType::Int4, true),
                ("amount", ColType::Int4, false),
            ],
        );
        definition.columns[0].primary = true;
        definition.columns[0].unique = true;
        let table = storage.create_table(definition).unwrap();
        let home = storage
            .heap
            .append_row(&[Datum::Int4(1), Datum::Int4(11)])
            .unwrap();
        let generation = storage.table(table).generation();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_in_generation(home, 7, generation))
            .unwrap();
        storage.set_lsn(7);
        storage.refresh_enforcers(table).unwrap();
        (storage, table)
    }

    fn key_rows(storage: &Storage, table: usize, id: i32) -> usize {
        let hash = hash_key_catalog(storage, 0, &[Datum::Int4(id)], &[0]).unwrap();
        let mut count = 0;
        assert!(
            storage
                .probe_value(table, &[0], hash, |_, _| {
                    count += 1;
                    Ok(())
                })
                .unwrap()
        );
        count
    }

    fn pending(storage: &Storage, table: usize, rowid: u64, txid: u32, id: i32) {
        let home = storage
            .heap
            .append_row(&[Datum::Int4(id), Datum::Int4(11)])
            .unwrap();
        storage
            .write_pending(table, rowid, txid, 1, Some(home))
            .unwrap();
    }

    #[test]
    fn key_preparation_failed_rebuild_preserves_working_cache_and_releases_candidates() {
        let (mut storage, table) = fixture(2);
        crate::mem::guard::forbid_alloc(|| {
            let before = storage.tables[table].enforcers();
            assert_eq!(key_rows(&storage, table, 1), 1);
            let mut index = IndexDef {
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("amount_index").unwrap(),
                table: SqlName::parse("key_orders").unwrap(),
                n_cols: 1,
                ..IndexDef::EMPTY
            };
            index.columns[0] = 1;
            let slot = storage.create_index(index, 0).unwrap();
            storage.commit_index_create(slot);
            let error = storage.build_enforcers_visible(table, None).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert!(same_bindings(
                &before,
                &storage.tables[table].enforcers(),
                true
            ));
            assert_eq!(key_rows(&storage, table, 1), 1);
            let spare = storage
                .value_indexes
                .acquire()
                .expect("failed candidate releases its reservation");
            assert!(storage.value_indexes.acquire().is_none());
            drop(spare);
            drop(before);
            storage.commit_index_drop(slot);
            storage.build_enforcers_visible(table, None).unwrap();
            assert_eq!(key_rows(&storage, table, 1), 1);
        });
    }

    #[test]
    fn key_preparation_expression_failure_preserves_working_bindings() {
        let (mut storage, table) = fixture(4);
        crate::mem::guard::forbid_alloc(|| {
            let before = storage.tables[table].enforcers();
            let mut index = IndexDef {
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("bad_expression").unwrap(),
                table: SqlName::parse("key_orders").unwrap(),
                n_cols: 1,
                ..IndexDef::EMPTY
            };
            index.expressions[0] = Some(index_expression_stackstr("id / (amount - 11)").unwrap());
            let slot = storage.create_index(index, 0).unwrap();
            storage.commit_index_create(slot);
            assert_eq!(
                storage
                    .build_enforcers_visible(table, None)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DIVISION_BY_ZERO
            );
            assert!(same_bindings(
                &before,
                &storage.tables[table].enforcers(),
                true
            ));
            assert_eq!(key_rows(&storage, table, 1), 1);
            storage.commit_index_drop(slot);
            storage.build_enforcers_visible(table, None).unwrap();
            assert_eq!(key_rows(&storage, table, 1), 1);
        });
    }

    #[test]
    fn key_preparation_competing_rebuilds_keep_a_complete_published_cache() {
        let (storage, table) = fixture(8);
        let start = std::sync::Barrier::new(4);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let storage = &storage;
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    crate::mem::guard::forbid_alloc(|| {
                        for _ in 0..16 {
                            if let Err(error) = storage.build_enforcers_visible(table, None) {
                                assert!(
                                    error.sqlstate == sqlstate::SERIALIZATION_FAILURE
                                        || error.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED
                                );
                            }
                            assert_eq!(key_rows(storage, table, 1), 1);
                        }
                    });
                });
            }
        });
        crate::mem::guard::forbid_alloc(|| {
            storage.build_enforcers_visible(table, None).unwrap();
            assert_eq!(key_rows(&storage, table, 1), 1);
        });
    }

    #[test]
    fn key_preparation_no_indexes_need_no_statement_image_reservation() {
        let mut config = test_config();
        config.query_workspace_slots = 0;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let table = storage
            .create_table(make_def(
                "key_orders",
                &[
                    ("id", ColType::Int4, false),
                    ("amount", ColType::Int4, false),
                ],
            ))
            .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            pending(&storage, table, 1, 8, 2);
            storage.commit_row(table, 1, 8, 7).unwrap();
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(row.committed_lsn, 7);
            assert!(row.pending_last().is_none());
        });
    }

    #[test]
    fn key_preparation_plain_indexes_do_not_copy_or_reserve_complete_definitions() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_catalog_versions_per_object = 1;
        config.query_workspace_slots = 1;
        let capacity = table_slot_capacity(&config) * (config.max_catalog_versions_per_object + 1);
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget).unwrap();
        let mut definition = make_def(
            "key_orders",
            &[
                ("id", ColType::Int4, true),
                ("amount", ColType::Int4, false),
            ],
        );
        definition.columns[0].primary = true;
        definition.columns[0].unique = true;
        let table = storage.create_table(definition).unwrap();
        storage.refresh_enforcers(table).unwrap();
        let mut readers = Vec::with_capacity(capacity);
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..capacity {
                let reader = storage.table_definition_images();
                reader.definition(&storage, table, 0).unwrap();
                readers.push(reader);
            }
            let exhausted = storage.table_definition_images();
            assert_eq!(
                exhausted
                    .definition(&storage, table, 0)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            pending(&storage, table, 1, 8, 2);
            storage.commit_row(table, 1, 8, 7).unwrap();
            assert_eq!(key_rows(&storage, table, 2), 1);
        });
    }

    #[test]
    fn key_preparation_replaced_bindings_reject_publication_without_changing_rows() {
        let (storage, table) = fixture(4);
        crate::mem::guard::forbid_alloc(|| {
            pending(&storage, table, 1, 8, 2);
            let selected = storage.resident_row_state(table, 1).unwrap().unwrap();
            let prepared = storage
                .prepare_committed_row(table, 1, 8, false, selected)
                .unwrap()
                .unwrap();
            storage.build_enforcers_visible(table, None).unwrap();
            let before = storage.tables[table].generation();
            let error = storage.publish_prepared_row(9, prepared).unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert_eq!(storage.tables[table].generation(), before);
            assert_eq!(key_rows(&storage, table, 1), 1);
            assert_eq!(key_rows(&storage, table, 2), 0);
            let row = storage.resident_row_state(table, 1).unwrap().unwrap();
            assert_eq!(row.committed_lsn, 7);
            assert_eq!(row.pending_last().unwrap().txid, 8);
            assert!(row.history.is_empty());
            drop(row);
            storage.commit_row(table, 1, 8, 9).unwrap();
            assert_eq!(key_rows(&storage, table, 1), 0);
            assert_eq!(key_rows(&storage, table, 2), 1);
        });
    }

    #[test]
    fn key_preparation_other_row_publication_keeps_prepared_keys_valid() {
        let (storage, table) = fixture(4);
        crate::mem::guard::forbid_alloc(|| {
            pending(&storage, table, 1, 8, 2);
            let selected = storage.resident_row_state(table, 1).unwrap().unwrap();
            let prepared = storage
                .prepare_committed_row(table, 1, 8, false, selected)
                .unwrap()
                .unwrap();
            pending(&storage, table, 2, 10, 3);
            storage.commit_row(table, 2, 10, 9).unwrap();
            storage.publish_prepared_row(10, prepared).unwrap();
            assert_eq!(key_rows(&storage, table, 1), 0);
            assert_eq!(key_rows(&storage, table, 2), 1);
            assert_eq!(key_rows(&storage, table, 3), 1);
        });
    }

    #[test]
    fn key_preparation_old_row_layout_is_rejected_before_key_decoding() {
        let (mut storage, table) = fixture(4);
        crate::mem::guard::forbid_alloc(|| {
            let state = storage.resident_row_state(table, 1).unwrap().unwrap();
            let selected = storage
                .visible_row_home_at(table, 1, state, 0, SNAPSHOT_ALL, 7)
                .unwrap()
                .unwrap();
            let (target, mut definition) = storage.select_table_definition(table, 8);
            definition.columns[0].ctype = ColType::Int8;
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(definition.columns[0].name);
            mapping[1] = Some(definition.columns[1].name);
            let change = storage
                .write_table_def(target, definition, &mapping, true)
                .unwrap();
            assert_eq!(storage.commit_table_def(change).unwrap(), Some(true));
            let mut output = [0; 128];
            let error = storage
                .encode_value_binding_entry(table, 0, 1, selected, &mut output)
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            assert!(error.message.as_str().contains("row layout"));
            assert_eq!(output, [0; 128]);
        });
    }

    #[test]
    fn key_preparation_retained_definition_rejects_later_index_lookup_after_rename() {
        let (mut storage, table) = fixture(4);
        crate::mem::guard::forbid_alloc(|| {
            let index = IndexDef {
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("order_index").unwrap(),
                table: SqlName::parse("key_orders").unwrap(),
                n_cols: 1,
                ..IndexDef::EMPTY
            };
            let slot = storage.create_index(index, 0).unwrap();
            storage.commit_index_create(slot);
            let created_at = storage.index(slot).created_at;
            let selection = ValueKeyPreparation::capture(&storage, table).unwrap();
            assert!(selection.index(&storage, created_at).is_ok());
            let (target, mut definition) = storage.select_table_definition(table, 8);
            definition.name = SqlName::parse("renamed_orders").unwrap();
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(definition.columns[0].name);
            mapping[1] = Some(definition.columns[1].name);
            let change = storage
                .write_table_def(target, definition, &mapping, false)
                .unwrap();
            storage.commit_table_def(change).unwrap();
            assert_eq!(selection.definition().name.as_str(), "key_orders");
            assert_eq!(
                selection
                    .index(&storage, created_at)
                    .err()
                    .unwrap()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
        });
    }
}
