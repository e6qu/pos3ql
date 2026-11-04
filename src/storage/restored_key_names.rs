//! Recovery checks committed and prepared name claims without catalog relocking.

use super::*;

#[derive(Clone, Copy)]
struct NameClaim {
    database: DatabaseOid,
    schema: SqlName,
    name: SqlName,
    relation: i32,
    owner: Option<CatalogPosition>,
    state: CatalogDdlState,
    backing: bool,
}

fn changed_states(
    state: CatalogDdlState,
    transaction: u32,
) -> Result<(CatalogDdlState, CatalogDdlState), SqlError> {
    use CatalogDdlState::*;
    match state {
        Present => Ok((
            PendingDrop { txid: transaction },
            PendingCreate { txid: transaction },
        )),
        PendingCreate { txid } if txid == transaction => {
            Ok((PendingCreateDrop { txid }, PendingCreate { txid }))
        }
        PendingDrop { txid } if txid == transaction => {
            Ok((PendingDrop { txid }, PendingCreateDrop { txid }))
        }
        PendingCreateDrop { txid } if txid == transaction => Ok((state, state)),
        Absent => Ok((Absent, Absent)),
        _ => Err(sql_err!(
            sqlstate::DATA_EXCEPTION,
            "restored relation names have conflicting transaction owners"
        )),
    }
}

fn conflicts(left: NameClaim, right: NameClaim) -> bool {
    use CatalogDdlState::*;
    if left.database != right.database
        || left.schema != right.schema
        || left.name != right.name
        || matches!(left.state, Absent | PendingCreateDrop { .. })
        || matches!(right.state, Absent | PendingCreateDrop { .. })
        || (left.relation == right.relation && left.owner == right.owner)
    {
        return false;
    }
    !matches!((left.state, right.state),
        (PendingDrop { txid: old }, PendingCreate { txid: new })
        | (PendingCreate { txid: new }, PendingDrop { txid: old }) if old == new)
}

fn relation_oid(created_at: u64) -> Result<i32, SqlError> {
    RelationIdentity::parse(created_at)
        .map(RelationIdentity::oid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::DATA_EXCEPTION,
                "restored relation name has an invalid creation identity"
            )
        })
}

fn visit_table_claims(
    slot: usize,
    current: &TableDefinitionRead<'_>,
    mut visit: impl FnMut(NameClaim) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let identity = current.identity();
    if identity.existence == CatalogDdlState::Absent {
        return Ok(());
    }
    let previous = current.committed_definition();
    Storage::validate_inline_key_names(previous)?;
    Storage::validate_inline_key_names(&**current)?;
    let owner = CatalogPosition::new(slot, identity.created_at)?;
    let base = NameClaim {
        database: identity.database,
        schema: previous.schema,
        name: previous.name,
        relation: relation_oid(identity.created_at)?,
        owner: None,
        state: identity.existence,
        backing: false,
    };
    let changes = current
        .state
        .pending
        .map(|pending| changed_states(identity.existence, pending.transaction))
        .transpose()?;
    let renamed = previous.schema != current.schema || previous.name != current.name;
    visit(NameClaim {
        state: if renamed {
            changes
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "restored relation rename has no owner"
                    )
                })?
                .0
        } else {
            base.state
        },
        ..base
    })?;
    if renamed {
        visit(NameClaim {
            schema: current.schema,
            name: current.name,
            state: changes.expect("checked rename owner").1,
            ..base
        })?;
    }
    for (name, index) in previous.backing_index_names() {
        let matched = current.schema == previous.schema
            && current
                .backing_index_names()
                .any(|candidate| candidate == (name, index));
        let state = if matched {
            base.state
        } else {
            changes
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "restored index removal has no owner"
                    )
                })?
                .0
        };
        visit(NameClaim {
            name,
            relation: index
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "restored backing index has no identity"
                    )
                })?
                .oid(),
            owner: Some(owner),
            state,
            backing: true,
            ..base
        })?;
    }
    for (name, index) in current.backing_index_names() {
        if current.schema == previous.schema
            && previous
                .backing_index_names()
                .any(|candidate| candidate == (name, index))
        {
            continue;
        }
        visit(NameClaim {
            schema: current.schema,
            name,
            relation: index
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "restored backing index has no identity"
                    )
                })?
                .oid(),
            owner: Some(owner),
            state: changes
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "restored index creation has no owner"
                    )
                })?
                .1,
            backing: true,
            ..base
        })?;
    }
    Ok(())
}

fn visit_name_pair(
    base: NameClaim,
    pending: Option<(u32, SqlName, SqlName)>,
    mut visit: impl FnMut(NameClaim) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if matches!(
        base.state,
        CatalogDdlState::Absent | CatalogDdlState::PendingCreateDrop { .. }
    ) {
        return Ok(());
    }
    if let Some((transaction, schema, name)) = pending
        && (schema != base.schema || name != base.name)
    {
        let (old, new) = changed_states(base.state, transaction)?;
        visit(NameClaim { state: old, ..base })?;
        visit(NameClaim {
            schema,
            name,
            state: new,
            ..base
        })?;
    } else {
        visit(base)?;
    }
    Ok(())
}

impl Storage {
    fn visit_registry_name_claims(
        &self,
        sources: &RelationNameSources<'_>,
        selected: (usize, table_definitions::TableIdentity),
        mut visit: impl FnMut(NameClaim) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        for index in sources.indexes {
            if matches!(
                index.ddl_state,
                CatalogDdlState::Absent | CatalogDdlState::PendingCreateDrop { .. }
            ) {
                continue;
            }
            let binding = index.table_binding.ok_or_else(|| {
                sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "restored index name has no selected owner"
                )
            })?;
            let catalog_sources::CatalogObjectSlot::Table(table) = binding.source.slot() else {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "restored index name owner is not a table"
                ));
            };
            let identity = if table == selected.0 {
                selected.1
            } else {
                self.tables
                    .get(table)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::DATA_EXCEPTION,
                            "restored index name owner is outside the catalog"
                        )
                    })?
                    .identity()
            };
            if !binding.source.matches_lifetime(
                self,
                identity.database,
                identity.created_at,
                identity.existence,
            ) || identity.database != index.database
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "restored index name belongs to another table lifetime"
                ));
            }
            let transaction = index
                .pending_name
                .map(|pending| pending.txid)
                .or(index.pending_table.map(|pending| pending.transaction));
            if index
                .pending_name
                .zip(index.pending_table)
                .is_some_and(|(name, table)| name.txid != table.transaction)
            {
                return Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "restored index rename has conflicting owners"
                ));
            }
            let relation = RelationIdentity::parse(index.created_at)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::DATA_EXCEPTION,
                        "restored index name has an invalid identity"
                    )
                })?
                .companion_oid(match index.relation_kind() {
                    IndexRelationKind::Table => FIRST_EXPLICIT_INDEX_OID,
                    IndexRelationKind::Toast => FIRST_TOAST_INDEX_OID,
                });
            visit_name_pair(
                NameClaim {
                    database: index.database,
                    schema: index.schema,
                    name: index.name,
                    relation,
                    owner: Some(CatalogPosition::new(table, identity.created_at)?),
                    state: index.ddl_state,
                    backing: false,
                },
                transaction.map(|transaction| {
                    (
                        transaction,
                        index.schema_for(transaction),
                        index.name_for(transaction),
                    )
                }),
                &mut visit,
            )?;
        }
        for view in sources.views {
            if view.ddl_state == CatalogDdlState::Absent {
                continue;
            }
            visit_name_pair(
                NameClaim {
                    database: view.database,
                    schema: view.schema,
                    name: view.name,
                    relation: relation_oid(view.created_at)?,
                    owner: None,
                    state: view.ddl_state,
                    backing: false,
                },
                view.pending_definition
                    .as_ref()
                    .map(|pending| (pending.txid, pending.schema, pending.name)),
                &mut visit,
            )?;
        }
        for sequence in sources.sequences {
            if sequence.ddl_state == CatalogDdlState::Absent {
                continue;
            }
            visit_name_pair(
                NameClaim {
                    database: sequence.database,
                    schema: sequence.schema,
                    name: sequence.name,
                    relation: relation_oid(sequence.created_at)?,
                    owner: None,
                    state: sequence.ddl_state,
                    backing: false,
                },
                sequence.pending_definition.map(|pending| {
                    let current = sequence.definition_for(pending.txid);
                    (pending.txid, current.schema, current.name)
                }),
                &mut visit,
            )?;
        }
        for composite in sources.composites {
            if composite.ddl_state == CatalogDdlState::Absent {
                continue;
            }
            visit_name_pair(
                NameClaim {
                    database: composite.database,
                    schema: composite.schema,
                    name: composite.name,
                    relation: relation_oid(composite.created_at)?,
                    owner: None,
                    state: composite.ddl_state,
                    backing: false,
                },
                composite
                    .pending_definition
                    .map(|pending| (pending.txid, pending.schema, pending.name)),
                &mut visit,
            )?;
        }
        Ok(())
    }

    pub(super) fn validate_restored_key_names(
        &self,
        sources: RelationNameSources<'_>,
    ) -> Result<(), SqlError> {
        let check = |candidate: NameClaim, other: NameClaim| {
            if conflicts(candidate, other) {
                Err(sql_err!(
                    sqlstate::DATA_EXCEPTION,
                    "restored backing index name duplicates another relation in its schema"
                ))
            } else {
                Ok(())
            }
        };
        for (slot, table) in self.tables.iter().enumerate() {
            if slot == self.large_object_page_table() {
                continue;
            }
            let current = TableDefinitionRead::current(&table.definition, &self.pending_table_defs);
            visit_table_claims(slot, &current, |candidate| {
                if !candidate.backing {
                    return Ok(());
                }
                // Reuse the outer table's guard instead of reacquiring its read lock.
                visit_table_claims(slot, &current, |other| check(candidate, other))?;
                for (other_slot, other_table) in self.tables.iter().enumerate() {
                    if other_slot == slot || other_slot == self.large_object_page_table() {
                        continue;
                    }
                    let other = TableDefinitionRead::current(
                        &other_table.definition,
                        &self.pending_table_defs,
                    );
                    visit_table_claims(other_slot, &other, |other| check(candidate, other))?;
                }
                self.visit_registry_name_claims(&sources, (slot, current.identity()), |other| {
                    check(candidate, other)
                })
            })?;
        }
        Ok(())
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::sql::types::ColType;
    use crate::storage::tests::{make_def, test_budget, test_config};

    fn keyed(name: &str) -> TableDefBuilder {
        let mut definition = make_def(name, &[("id", ColType::Int4, false)]);
        definition.columns[0].unique = true;
        definition.n_checks = 1;
        definition.checks[0].name = SqlName::parse("shared_check").unwrap();
        definition.checks[0].expression = StackStr::from_str("id > 0");
        definition
    }

    fn replace(storage: &Storage, table: usize, definition: &TableDefBuilder) {
        let revision = storage.reserve_table_definition_revision().unwrap();
        let saved = definition.freeze(&storage.metadata_pool).unwrap();
        *storage.tables[table].definition_mut(revision) = saved;
    }

    #[test]
    fn restored_inline_names_reject_cross_table_collisions_and_keep_named_index_aliases() {
        let mut config = test_config();
        config.max_tables = 2;
        config.max_indexes = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let first = storage
                .create_table(keyed("first_namespace_owner"))
                .unwrap();
            let second = storage
                .create_table(keyed("second_namespace_owner"))
                .unwrap();
            storage
                .validate_restored_relation_identities(storage.catalog_identity_sequence())
                .unwrap();
            let mut definition = TableDefBuilder::from_definition(&storage.table_def(first, 0));
            let name = definition.take_column_key_name(0).unwrap();
            let mut key = UniqueKey::EMPTY;
            key.name = name;
            key.index_identity = definition.take_column_index_identity(0);
            key.identity = definition.take_column_constraint_identity(0);
            key.n_cols = 1;
            definition.columns[0].unique = false;
            definition.uniques[0] = key;
            definition.n_uniques = 1;
            replace(&storage, first, &definition);
            let alias = IndexDef {
                created_at: key.index_identity.unwrap().created_at(),
                schema: definition.schema,
                table: definition.name,
                name,
                n_cols: 1,
                unique: true,
                ..IndexDef::EMPTY
            };
            let index = storage.create_index(alias, 0).unwrap();
            storage.commit_index_create(index).unwrap();
            // The key and its registry entry represent one relation; CHECK names may repeat.
            storage
                .validate_restored_relation_identities(storage.catalog_identity_sequence())
                .unwrap();
            let mut malformed = TableDefBuilder::from_definition(&storage.table_def(second, 0));
            let original = malformed.column_key_name(0).unwrap();
            for collision in [name, definition.name] {
                malformed.set_column_key_name(0, collision).unwrap();
                replace(&storage, second, &malformed);
                let sequence = storage.catalog_identity_sequence();
                assert_eq!(
                    storage
                        .validate_restored_relation_identities(sequence)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::DATA_EXCEPTION
                );
                assert_eq!(storage.catalog_identity_sequence(), sequence);
            }
            malformed.set_column_key_name(0, original).unwrap();
            replace(&storage, second, &malformed);
            storage
                .validate_restored_relation_identities(storage.catalog_identity_sequence())
                .unwrap();
        });
    }

    #[test]
    fn restored_inline_names_allow_one_transaction_replacement_and_reject_other_owners() {
        let mut config = test_config();
        config.max_tables = 2;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let old = storage.create_table(keyed("old_namespace_owner")).unwrap();
            let name = storage.table_def(old, 0).column_key_name(0).unwrap();
            storage.tables[old].definition.write().identity.existence =
                CatalogDdlState::PendingDrop { txid: 7 };
            let new = storage
                .create_table_in(keyed("new_namespace_owner"), 7)
                .unwrap();
            let mut restored = TableDefBuilder::from_definition(&storage.table_def(new, 7));
            restored.set_column_key_name(0, name).unwrap();
            replace(&storage, new, &restored);
            storage
                .validate_restored_relation_identities(storage.catalog_identity_sequence())
                .unwrap();
            // A different prepared owner would see both names and could not commit this state.
            storage.tables[new].definition.write().identity.existence =
                CatalogDdlState::PendingCreate { txid: 8 };
            assert_eq!(
                storage
                    .validate_restored_relation_identities(storage.catalog_identity_sequence())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            storage.tables[old].definition.write().identity.existence =
                CatalogDdlState::PendingCreate { txid: 7 };
            assert_eq!(
                storage
                    .validate_restored_relation_identities(storage.catalog_identity_sequence())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
        });
    }
    #[test]
    fn prepared_inline_key_removal_and_same_name_index_creation_share_one_namespace() {
        let mut config = test_config();
        config.max_tables = 1;
        config.max_indexes = 1;
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut definition = make_def("prepared_key_owner", &[("id", ColType::Int4, false)]);
            definition.columns[0].unique = true;
            let table = storage.create_table(definition).unwrap();
            let (target, current) = storage.select_table_definition(table, 7);
            let mut changed = TableDefBuilder::from_definition(&current);
            let name = changed.take_column_key_name(0).unwrap();
            changed.columns[0].unique = false;
            let mut mapping = [None; MAX_COLUMNS];
            mapping[0] = Some(changed.columns[0].name);
            drop(current);
            storage
                .write_table_def(target, changed, &mapping, false, ColumnSet::EMPTY)
                .unwrap();
            let index = storage
                .create_index(
                    IndexDef {
                        schema: SqlName::parse("public").unwrap(),
                        table: SqlName::parse("prepared_key_owner").unwrap(),
                        name,
                        n_cols: 1,
                        ..IndexDef::EMPTY
                    },
                    7,
                )
                .unwrap();
            storage
                .validate_restored_relation_identities(storage.catalog_identity_sequence())
                .unwrap();
            storage.index_catalog()[index].ddl_state = CatalogDdlState::PendingCreate { txid: 8 };
            assert_eq!(
                storage
                    .validate_restored_relation_identities(storage.catalog_identity_sequence())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
        });
    }
}
