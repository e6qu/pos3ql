// Native storage regressions; shared interpreter fixtures remain beside Storage.

#[test]
fn visible_row_heap_ownership_statistics_exhaustion_reverts_pending_append() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def(
            "write_statistics",
            &[("id", ColType::Int4, false)],
        ))
        .unwrap();
    let values = [Datum::Int4(1)];
    let (location, bytes) = storage.heap.append(rowenc::encoded_len(&values)).unwrap();
    rowenc::encode(&values, bytes);
    {
        let mut statistics = storage.cumulative_statistics();
        while statistics.relation_transactions.len() < statistics.relation_transactions.capacity() {
            statistics
                .relation_transactions
                .push(RelationTransactionStatistics::new(
                    8,
                    table,
                    1,
                    storage.tables[table].database(),
                    storage.tables[table].identity().created_at,
                ))
                .unwrap();
        }
    }
    crate::mem::guard::forbid_alloc(|| {
        let error = storage
            .write_pending(table, 1, 7, 1, Some(location))
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(storage.resident_row_state(table, 1).unwrap().is_none());
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        let error = storage.write_pending(table, 1, 7, 1, None).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        assert!(row.pending.is_none());
        assert_eq!(row.committed, Some(RowHome::Heap(location)));
        drop(row);
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.heap.test_exclusive_write().is_ok());
    });
}

#[test]
fn visible_row_heap_ownership_pins_before_metadata_owner_releases() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("pinned", &[("id", ColType::Int4, false)]))
        .unwrap();
    storage.heap.append(4).unwrap();
    let (location, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"row!");
    storage.tables[table]
        .rows
        .insert(1, RowState::committed_only_at(location, 7))
        .unwrap();
    let arena = Arena::new(&mut budget, "visible row image", 4).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, 8, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.tables[table].rows.test_write().is_ok());
        assert!(
            storage.heap.test_exclusive_write().is_err(),
            "visible heap locations retain byte ownership"
        );
        assert_eq!(storage.row_bytes(table, 1, image, &arena).unwrap(), b"row!");
        assert!(storage.heap.test_exclusive_write().is_ok());
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, 8, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        assert_eq!(
            storage
                .row_bytes(table, 1, image, &arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert!(storage.heap.test_exclusive_write().is_ok());
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, 8, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        let error = storage
            .with_row_bytes(table, 1, image, |bytes| -> Result<(), SqlError> {
                assert_eq!(bytes, b"row!");
                assert!(storage.heap.test_exclusive_write().is_err());
                Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "callback exhausted"
                ))
            })
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(storage.heap.test_exclusive_write().is_ok());
    });
}

#[test]
fn visible_row_heap_ownership_committed_snapshot_freezes_selected_lsn() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def(
            "committed_snapshot",
            &[("id", ColType::Int4, false)],
        ))
        .unwrap();
    storage.heap.append(4).unwrap();
    let (original, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"old!");
    let mut state = RowState::committed_only_at(original, 7);
    storage.tables[table].rows.insert(1, state).unwrap();
    let image = storage
        .visible_row_home_at(
            table,
            1,
            storage.resident_row_state(table, 1).unwrap().unwrap(),
            7,
            SNAPSHOT_ALL,
            u64::MAX,
        )
        .unwrap()
        .unwrap();
    let retained = image.snapshot();
    drop(image);
    let arena = Arena::new(&mut budget, "committed snapshot images", 16).unwrap();
    let mut scratch = FixedVec::new(&mut budget, "committed snapshot compaction", 8).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let (later, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"new!");
        push_committed_version(
            &mut storage.row_versions.exclusive().committed_row_versions,
            &mut state.history,
            storage.max_row_versions_per_row,
            CommittedVersion {
                home: state.committed,
                lsn: 7,
            },
        )
        .unwrap();
        state.committed = Some(RowHome::Heap(later));
        state.committed_lsn = 8;
        storage.tables[table].rows.insert(1, state).unwrap();
        storage.compact_heap(&mut scratch).unwrap();
        assert_eq!(
            storage.row_bytes(table, 1, retained, &arena).unwrap(),
            b"old!"
        );
        let image = storage
            .visible_row_home_at(
                table,
                1,
                storage.resident_row_state(table, 1).unwrap().unwrap(),
                7,
                SNAPSHOT_ALL,
                u64::MAX,
            )
            .unwrap()
            .unwrap();
        assert_eq!(storage.row_bytes(table, 1, image, &arena).unwrap(), b"new!");
        let versions = storage.row_versions.exclusive();
        let state = storage.tables[table].rows.get_mut(&1).unwrap();
        prune_committed_history(&mut versions.committed_row_versions, state, None);
        assert_eq!(
            storage
                .row_bytes(table, 1, retained, &arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
    });
}

#[test]
fn visible_row_heap_ownership_same_command_versions_and_undo_reject_reuse() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("same_command", &[("id", ColType::Int4, false)]))
        .unwrap();
    storage.heap.append(4).unwrap();
    let (original, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"old!");
    let first = storage
        .write_pending_untracked(table, 1, 7, 2, Some(original))
        .unwrap();
    let image = storage
        .visible_row_home_at(
            table,
            1,
            storage.resident_row_state(table, 1).unwrap().unwrap(),
            7,
            SNAPSHOT_ALL,
            7,
        )
        .unwrap()
        .unwrap();
    let retained = image.snapshot();
    drop(image);
    let mut scratch = FixedVec::new(&mut budget, "same command compaction", 8).unwrap();
    let arena = Arena::new(&mut budget, "same command images", 32).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let (later, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"new!");
        let second = storage
            .write_pending_untracked(table, 1, 7, 2, Some(later))
            .unwrap();
        let image = storage
            .visible_row_home_at(
                table,
                1,
                storage.resident_row_state(table, 1).unwrap().unwrap(),
                7,
                SNAPSHOT_ALL,
                7,
            )
            .unwrap()
            .unwrap();
        let removed = image.snapshot();
        drop(image);
        storage.compact_heap(&mut scratch).unwrap();
        assert_eq!(
            storage.row_bytes(table, 1, retained, &arena).unwrap(),
            b"old!"
        );
        assert_eq!(
            storage.row_bytes(table, 1, removed, &arena).unwrap(),
            b"new!"
        );
        storage.restore_pending(table, 1, 7, second);
        assert_eq!(
            storage
                .row_bytes(table, 1, removed, &arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        let (replacement, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"next");
        let third = storage
            .write_pending_untracked(table, 1, 7, 2, Some(replacement))
            .unwrap();
        assert_eq!(
            storage
                .row_bytes(table, 1, removed, &arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        storage.restore_pending(table, 1, 7, second);
        assert_eq!(
            storage
                .resident_row_state(table, 1)
                .unwrap()
                .unwrap()
                .pending
                .len(),
            2
        );
        assert_eq!(
            storage.row_bytes(table, 1, retained, &arena).unwrap(),
            b"old!"
        );
        storage.restore_pending(table, 1, 7, third);
        assert_eq!(
            storage.row_bytes(table, 1, retained, &arena).unwrap(),
            b"old!"
        );
        storage.restore_pending(table, 1, 7, first);
        assert!(storage.resident_row_state(table, 1).unwrap().is_none());
        assert!(storage.heap.test_exclusive_write().is_ok());
        assert!(storage.row_versions.test_write().is_ok());
    });
}

#[test]
fn visible_row_heap_ownership_deferred_snapshot_survives_relocation_and_rejects_reuse() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("snapshot", &[("id", ColType::Int4, false)]))
        .unwrap();
    storage.heap.append(4).unwrap();
    let (committed, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"base");
    let (original, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"old!");
    let mut state = RowState::committed_only_at(committed, 7);
    push_pending_version(
        &mut storage.row_versions.exclusive().pending_row_versions,
        &mut state.pending,
        storage.max_row_versions_per_row,
        PendingChange {
            txid: 7,
            cid: 2,
            loc: Some(original),
            changed_columns: ColumnSet::EMPTY,
            changes_existence: false,
        },
    )
    .unwrap();
    storage.tables[table].rows.insert(1, state).unwrap();
    let mut scratch = FixedVec::new(&mut budget, "snapshot compaction", 8).unwrap();
    let arena = Arena::new(&mut budget, "deferred row images", 32).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let image = storage
            .visible_row_home_at(table, 1, row, 7, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        let snapshot = image.snapshot();
        drop(image);
        assert!(storage.heap.test_exclusive_write().is_ok());
        let (later, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"new!");
        push_pending_version(
            &mut storage.row_versions.exclusive().pending_row_versions,
            &mut state.pending,
            storage.max_row_versions_per_row,
            PendingChange {
                txid: 7,
                cid: 4,
                loc: Some(later),
                changed_columns: ColumnSet::EMPTY,
                changes_existence: false,
            },
        )
        .unwrap();
        storage.tables[table].rows.insert(1, state).unwrap();
        storage.set_commit_snapshot(u64::MAX);
        storage.compact_heap(&mut scratch).unwrap();
        assert_eq!(
            storage.heap.get(original).err().unwrap().sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        assert_eq!(
            storage.row_bytes(table, 1, snapshot, &arena).unwrap(),
            b"old!"
        );
        assert!(storage.heap.test_exclusive_write().is_ok());
        let row = storage.resident_row_state(table, 1).unwrap().unwrap();
        let current = storage
            .visible_row_home_at(table, 1, row, 7, SNAPSHOT_ALL, 7)
            .unwrap()
            .unwrap();
        assert_eq!(
            storage.row_bytes(table, 1, current, &arena).unwrap(),
            b"new!"
        );
        storage.commit_drop(table);
        let replacement = storage
            .create_table(make_def("replacement", &[("id", ColType::Int4, false)]))
            .unwrap();
        assert_eq!(replacement, table);
        assert_eq!(
            storage
                .row_bytes(table, 1, snapshot, &arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.heap.test_exclusive_write().is_ok());
    });
}

#[test]
fn execution_path_is_thread_private() {
    let prior = replace_execution_path(PathContext::public_schema(7));
    let child = std::thread::spawn(|| {
        assert_eq!(
            execution_path().creation_target(),
            Some(PathEntry::Schema(0))
        );
        let prior = replace_execution_path(PathContext::public_schema(11));
        assert_eq!(prior.creation_target(), Some(PathEntry::Schema(0)));
        assert_eq!(
            execution_path().creation_target(),
            Some(PathEntry::Schema(11))
        );
    });
    child.join().expect("path worker completes");
    assert_eq!(
        execution_path().creation_target(),
        Some(PathEntry::Schema(7))
    );
    replace_execution_path(prior);
}

#[test]
fn visibility_snapshots_are_thread_private() {
    assert_eq!(core::mem::size_of::<VisibilityContext>(), 16);
    let prior = replace_execution_visibility(VisibilityContext {
        read_snapshot: 7,
        commit_snapshot: 13,
        analyzing: false,
    });
    let child = std::thread::spawn(|| {
        let initial = execution_visibility();
        assert_eq!(initial.read_snapshot, SNAPSHOT_ALL);
        assert_eq!(initial.commit_snapshot, u64::MAX);
        replace_execution_visibility(VisibilityContext {
            read_snapshot: 11,
            commit_snapshot: 17,
            analyzing: false,
        });
        let changed = execution_visibility();
        assert_eq!(changed.read_snapshot, 11);
        assert_eq!(changed.commit_snapshot, 17);
    });
    child.join().expect("visibility worker completes");
    let unchanged = execution_visibility();
    assert_eq!(unchanged.read_snapshot, 7);
    assert_eq!(unchanged.commit_snapshot, 13);
    replace_execution_visibility(prior);
}

#[test]
fn catalog_lifetime_routine_dependencies_use_their_complete_creation_range() {
    crate::mem::guard::forbid_alloc(|| {
        for birth in [1, 40_000, 40_001, MAX_CATALOG_CREATION] {
            let identity = StoredDependencyIdentity::RoutineOid(ROUTINE_OID_BASE + birth as i32);
            assert!(identity.valid_for(DependencyClass::Routine));
            assert!(!identity.valid_for(DependencyClass::Operator));
            assert_eq!(
                StoredDependencyIdentity::from_storage(
                    DependencyClass::Routine,
                    identity.storage_key().as_str()
                ),
                Some(identity)
            );
        }
        for birth in [0, MAX_CATALOG_CREATION + 1] {
            let identity = StoredDependencyIdentity::RoutineOid(ROUTINE_OID_BASE + birth as i32);
            assert!(!identity.valid_for(DependencyClass::Routine));
            assert!(
                StoredDependencyIdentity::from_storage(
                    DependencyClass::Routine,
                    identity.storage_key().as_str()
                )
                .is_none()
            );
        }
    });
}

#[test]
fn catalog_generations_reject_exhaustion_without_saturation() {
    for (maximum, object) in [
        (MAX_TRIGGER_OID_GENERATION, "trigger"),
        (MAX_INDEX_OID_GENERATION, "index"),
    ] {
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                bounded_catalog_generation(maximum, maximum, object).unwrap(),
                maximum
            );
            for invalid in [0, maximum + 1, u64::MAX] {
                assert_eq!(
                    bounded_catalog_generation(invalid, maximum, object)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
            }
        });
    }
}

#[test]
fn pending_value_index_object_read_is_an_internal_wait() {
    let error = value_index_read_error(crate::store::ValueIndexError::Store(
        crate::store::StoreError::NotReady,
    ));
    assert_eq!(error.sqlstate, sqlstate::INTERNAL_IO_WAIT);
}

#[test]
fn column_metadata_stays_within_recovery_stack_budget() {
    assert!(
        core::mem::size_of::<ColumnMeta>() <= 512,
        "column metadata: {} bytes; type reference: {}; default: {}; value: {}",
        core::mem::size_of::<ColumnMeta>(),
        core::mem::size_of::<Option<UserTypeName>>(),
        core::mem::size_of::<ColumnDefault>(),
        core::mem::size_of::<OwnedDatum>()
    );
}

#[test]
fn schema_sql_rewrite_leaves_literals_and_comments_opaque() {
    let mut budget = Budget::new(1024);
    let arena = crate::mem::arena::Arena::new(&mut budget, "schema rewrite", 1024).unwrap();
    let old = SqlName::parse("old_schema").unwrap();
    let new = SqlName::parse("new_schema").unwrap();
    let mut source = StackStr::<256>::from_str(
        "SELECT old_schema.item, 'old_schema.item', $$old_schema.item$$ /* old_schema.item /* nested */ */ FROM old_schema.items",
    );
    rewrite_schema_sql_tokens(
        &mut source,
        &mut [],
        old,
        new,
        NamespaceRewriteScope::Lexical { arena: &arena },
    )
    .unwrap();
    assert_eq!(
        source.as_str(),
        "SELECT new_schema.item, 'old_schema.item', $$old_schema.item$$ /* old_schema.item /* nested */ */ FROM new_schema.items"
    );
}

#[test]
fn schema_sql_rewrite_preserves_escape_and_dollar_strings_and_identifier_spelling() {
    let mut budget = Budget::new(1024);
    let arena = crate::mem::arena::Arena::new(&mut budget, "schema rewrite", 1024).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let old = SqlName::parse("old_schema").unwrap();
        let new = SqlName::parse("new_schema").unwrap();
        let mut source = StackStr::<256>::from_str(
            r"SELECT E'it\'s old_schema.item', $$ old_schema.item $$, $tag$ old_schema.item $tag$ FROM old_schema.items",
        );
        rewrite_schema_sql_tokens(
            &mut source,
            &mut [],
            old,
            new,
            NamespaceRewriteScope::Lexical { arena: &arena },
        )
        .unwrap();
        assert_eq!(
            source.as_str(),
            r"SELECT E'it\'s old_schema.item', $$ old_schema.item $$, $tag$ old_schema.item $tag$ FROM new_schema.items"
        );
        let mut quoted =
            StackStr::<128>::from_str(r#"SELECT "old""schema".item FROM "old""schema".items"#);
        rewrite_schema_sql_tokens(
            &mut quoted,
            &mut [],
            SqlName::parse("old\"schema").unwrap(),
            SqlName::parse("new space").unwrap(),
            NamespaceRewriteScope::Lexical { arena: &arena },
        )
        .unwrap();
        assert_eq!(
            quoted.as_str(),
            r#"SELECT "new space".item FROM "new space".items"#
        );
        let mut unicode = StackStr::<128>::from_str("SELECT café.item FROM café.items");
        rewrite_schema_sql_tokens(
            &mut unicode,
            &mut [],
            SqlName::parse("café").unwrap(),
            new,
            NamespaceRewriteScope::Lexical { arena: &arena },
        )
        .unwrap();
        assert_eq!(
            unicode.as_str(),
            "SELECT new_schema.item FROM new_schema.items"
        );
    });
}

#[test]
fn schema_sql_rewrite_remaps_view_and_rule_spans_for_shorter_and_longer_names() {
    let mut budget = crate::mem::budget::Budget::new(1 << 20);
    let arena = crate::mem::arena::Arena::new(&mut budget, "schema SQL rewrite", 1 << 20).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let old = SqlName::parse("old_schema").unwrap();
        for name in [
            "x",
            "a_longer_schema_name",
            "new space",
            "schéma",
            "select",
            "a\"quoted",
        ] {
            let mut rule = RuleDefinition::EMPTY;
            rule.source = StackStr::from_str("SELECT id FROM old_schema.orders");
            rule.action_count = 1;
            rule.actions[0] = RuleTextSpan {
                start: 0,
                len: rule.source.len() as u16,
            };
            rule.rename_schema(old, SqlName::parse(name).unwrap(), &arena)
                .unwrap();
            assert_eq!(rule.action_sql().next(), Some(rule.source.as_str()));
            assert_eq!(usize::from(rule.actions[0].len), rule.source.len());
        }
        let mut rule = RuleDefinition::EMPTY;
        rule.source = StackStr::from_str(
            "CREATE RULE r AS ON INSERT TO old_schema.orders WHERE old_schema.ok() DO ALSO (SELECT old_schema.first(); SELECT old_schema.second())",
        );
        let span = |source: &str| RuleTextSpan {
            start: rule.source.as_str().find(source).unwrap() as u16,
            len: source.len() as u16,
        };
        rule.condition = Some(span("old_schema.ok()"));
        rule.actions[0] = span("SELECT old_schema.first()");
        rule.actions[1] = span("SELECT old_schema.second()");
        rule.action_count = 2;
        rule.rename_schema(old, SqlName::parse("x").unwrap(), &arena)
            .unwrap();
        assert_eq!(rule.condition_sql(), Some("x.ok()"));
        let mut actions = rule.action_sql();
        assert_eq!(actions.next(), Some("SELECT x.first()"));
        assert_eq!(actions.next(), Some("SELECT x.second()"));
        assert_eq!(actions.next(), None);
    });
}

#[test]
fn schema_sql_rewrite_rejects_invalid_spans_and_exhaustion_without_partial_edits() {
    let mut budget = Budget::new(1024);
    let arena = crate::mem::arena::Arena::new(&mut budget, "schema rewrite", 1024).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let old = SqlName::parse("old_schema").unwrap();
        let new = SqlName::parse("new_schema").unwrap();
        let initial = StackStr::<64>::from_str("SELECT old_schema.orders");
        for span in [
            RuleTextSpan { start: 8, len: 3 },
            RuleTextSpan { start: 63, len: 2 },
        ] {
            let mut source = initial;
            let mut spans = [span];
            let error = rewrite_schema_sql_tokens(
                &mut source,
                &mut spans,
                old,
                new,
                NamespaceRewriteScope::Lexical { arena: &arena },
            )
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
            assert_eq!(source, initial);
            assert_eq!(spans, [span]);
        }
        let mut source = StackStr::<24>::from_str("SELECT old_schema.orders");
        let initial = source;
        let span = RuleTextSpan {
            start: 0,
            len: source.len() as u16,
        };
        let mut spans = [span];
        let error = rewrite_schema_sql_tokens(
            &mut source,
            &mut spans,
            old,
            SqlName::parse("a_schema_name_that_cannot_fit").unwrap(),
            NamespaceRewriteScope::Lexical { arena: &arena },
        )
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(source, initial);
        assert_eq!(spans, [span]);
    });
}

#[test]
fn committed_image_obeys_an_lsn_snapshot() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage.heap.append(24).unwrap();
    let location = RowLoc::test(12, 4);
    let state = RowState::committed_only_at(location, 42);
    storage.tables[0].rows.insert(1, state).unwrap();
    assert_eq!(
        storage
            .visible_row_home_at(
                0,
                1,
                storage.resident_row_state(0, 1).unwrap().unwrap(),
                7,
                SNAPSHOT_ALL,
                41
            )
            .unwrap()
            .map(|image| image.metadata()),
        None
    );
    assert_eq!(
        storage
            .visible_row_home_at(
                0,
                1,
                storage.resident_row_state(0, 1).unwrap().unwrap(),
                7,
                SNAPSHOT_ALL,
                42
            )
            .unwrap()
            .map(|image| image.metadata()),
        Some(RowHome::Heap(location))
    );
    assert_eq!(
        storage
            .visible_row_home_at(
                0,
                1,
                storage.resident_row_state(0, 1).unwrap().unwrap(),
                7,
                SNAPSHOT_ALL,
                99
            )
            .unwrap()
            .map(|image| image.metadata()),
        Some(RowHome::Heap(location))
    );

    // A transaction's own pending image remains governed by command ID:
    // committed-snapshot filtering applies only when no visible own image
    // overlays it.
    let mut pending = state;
    let row_versions = storage.row_versions.exclusive();
    push_pending_version(
        &mut row_versions.pending_row_versions,
        &mut pending.pending,
        storage.max_row_versions_per_row,
        PendingChange {
            txid: 7,
            cid: 3,
            loc: Some(RowLoc::test(20, 4)),
            changed_columns: ColumnSet::all(MAX_COLUMNS),
            changes_existence: false,
        },
    )
    .unwrap();
    storage.tables[0].rows.insert(1, pending).unwrap();
    assert_eq!(
        storage
            .visible_row_home_at(
                0,
                1,
                storage.resident_row_state(0, 1).unwrap().unwrap(),
                7,
                4,
                41
            )
            .unwrap()
            .map(|image| image.metadata()),
        Some(RowHome::Heap(RowLoc::test(20, 4)))
    );
    assert_eq!(
        storage
            .visible_row_home_at(
                0,
                1,
                storage.resident_row_state(0, 1).unwrap().unwrap(),
                8,
                4,
                41
            )
            .unwrap()
            .map(|image| image.metadata()),
        None
    );
}

#[test]
fn unpublished_spilled_row_state_is_not_redundant() {
    let mut state = RowState {
        committed: Some(RowHome::Spilled {
            len: 4,
            sst: crate::storage::SpillGenerationId::fixture(0),
            commit_lsn: 42,
        }),
        committed_lsn: 42,
        committed_generation: Some(0),
        checkpoint_change_lsn: 42,
        history: CommittedHistory::empty(),
        pending: PendingVersions::empty(),
    };
    assert!(!Storage::redundant_spilled_row_state(&state));
    state.checkpoint_change_lsn = 0;
    assert!(Storage::redundant_spilled_row_state(&state));
}

#[test]
fn comment_class_codec_rejects_unknown_values() {
    for class in [
        CommentClass::Relation,
        CommentClass::Schema,
        CommentClass::Type,
        CommentClass::Tablespace,
        CommentClass::Extension,
        CommentClass::Trigger,
        CommentClass::Database,
        CommentClass::Collation,
        CommentClass::Conversion,
        CommentClass::EventTrigger,
        CommentClass::Rule,
        CommentClass::TextSearchParser,
        CommentClass::TextSearchTemplate,
        CommentClass::TextSearchDictionary,
        CommentClass::TextSearchConfiguration,
        CommentClass::LargeObject,
        CommentClass::Routine,
        CommentClass::Policy,
        CommentClass::Statistics,
        CommentClass::Publication,
        CommentClass::Subscription,
        CommentClass::Role,
        CommentClass::ForeignServer,
        CommentClass::Cast,
        CommentClass::Operator,
        CommentClass::OperatorFamily,
        CommentClass::OperatorClass,
        CommentClass::Constraint,
        CommentClass::ForeignDataWrapper,
        CommentClass::AccessMethod,
        CommentClass::ProceduralLanguage,
    ] {
        assert_eq!(CommentClass::from_u8(class.to_u8()), Some(class));
    }
    assert_eq!(CommentClass::from_u8(31), None);
    assert_eq!(CommentClass::from_u8(u8::MAX), None);
}

#[test]
fn access_class_codec_covers_every_durable_catalog_class() {
    for class in [
        AccessClass::Table,
        AccessClass::View,
        AccessClass::MaterializedView,
        AccessClass::Sequence,
        AccessClass::Schema,
        AccessClass::Domain,
        AccessClass::Enum,
        AccessClass::Index,
        AccessClass::Routine,
        AccessClass::Composite,
        AccessClass::Statistics,
        AccessClass::Tablespace,
        AccessClass::Extension,
        AccessClass::Trigger,
        AccessClass::EventTrigger,
        AccessClass::Database,
        AccessClass::LargeObject,
        AccessClass::ForeignDataWrapper,
        AccessClass::ForeignServer,
        AccessClass::Language,
    ] {
        assert_eq!(AccessClass::from_u8(class as u8), Some(class));
    }
    assert_eq!(AccessClass::from_u8(u8::MAX), None);
}

#[test]
fn row_read_ownership_walk_releases_guards_on_break_and_error() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage.heap.append(4).unwrap();
    storage.tables[0]
        .rows
        .insert(1, RowState::committed_only_at(RowLoc::test(0, 4), 7))
        .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .for_each_row_state(0, &mut |_, row| {
                assert!(storage.row_versions.test_write().is_ok());
                assert!(storage.tables[0].rows.test_write().is_ok());
                assert_eq!(row.committed_lsn, 7);
                Ok(core::ops::ControlFlow::Break(()))
            })
            .unwrap();
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.tables[0].rows.test_write().is_ok());
        let error = storage
            .for_each_row_state(0, &mut |_, _| {
                Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "reader callback exhausted"
                ))
            })
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.tables[0].rows.test_write().is_ok());
        let row = storage.row_state(0, 1).unwrap().unwrap();
        assert!(storage.row_versions.test_write().is_ok());
        assert_eq!(
            storage
                .visible_row_home_at(0, 1, row, 7, SNAPSHOT_ALL, 7)
                .unwrap()
                .map(|image| image.metadata()),
            Some(RowHome::Heap(RowLoc::test(0, 4)))
        );
        assert!(storage.row_versions.test_write().is_ok());
    });
}

#[test]
fn table_state_row_map_controls_are_charged_at_startup() {
    for (rows, pages) in [(8, 16), (16, 8)] {
        let mut config = test_config();
        config.query_workspace_slots = 2;
        config.row_overlay_slots = rows;
        config.large_object_pages = pages;
        let bytes = config.memtable_bytes + Storage::extra_budget_bytes(&config);
        let mut budget = Budget::new(bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        storage.configure_collation(&config, &mut budget).unwrap();
        assert_eq!(budget.remaining(), 0);
        assert!(size_of::<RowMap>() > size_of::<RowEntries>());
        assert_eq!(storage.tables.len(), table_slot_capacity(&config));
        for (slot, table) in storage.tables.iter().enumerate() {
            assert_eq!(
                table.rows.capacity(),
                if slot == config.max_tables {
                    pages
                } else {
                    rows
                }
            );
        }
    }
}

fn test_prepared_catalog_entry(
    transaction_id: u32,
    gid: &str,
    generation: u64,
) -> PreparedTransactionCatalogEntry {
    PreparedTransactionCatalogEntry {
        transaction_id,
        gid: StackStr::from_str(gid),
        prepared_at: generation as i64,
        owner: RoleOid::BOOTSTRAP,
        database: DatabaseOid::POSTGRES,
        first_lsn: generation,
        prepared_lsn: generation,
    }
}

#[test]
fn cluster_metadata_system_settings_publish_owned_bounded_images() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<SystemSetting>>>();
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let names = ["setting_a", "setting_b", "setting_c", "setting_d"];
    let barrier = crate::mem::mutex::Barrier::new(names.len() + 1);
    std::thread::scope(|scope| {
        for (worker, name) in names.iter().enumerate() {
            let storage = &storage;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    let name = SqlName::parse(name).unwrap();
                    let value = Some(StackStr::from_str(name.as_str()));
                    let txid = worker as u32 + 20;
                    for iteration in 0..16 {
                        let (slot, prior) = storage
                            .change_system_setting(name, value, txid)
                            .unwrap()
                            .unwrap();
                        if iteration % 2 == 0 {
                            storage.rollback_system_setting(slot, prior);
                        } else {
                            storage.commit_system_setting(slot);
                            if iteration != 15 {
                                let (slot, _) = storage
                                    .change_system_setting(name, None, txid)
                                    .unwrap()
                                    .unwrap();
                                storage.commit_system_setting(slot);
                            }
                        }
                    }
                });
            });
        }
        barrier.wait();
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..256 {
                for (_, image) in storage.system_settings() {
                    if image.live {
                        assert_eq!(image.name.as_str(), image.value.as_str());
                    } else if image.pending.is_none() {
                        assert_eq!(image.name, SqlName::EMPTY);
                        assert!(image.value.as_str().is_empty());
                    }
                    let _ = storage.system_settings().count();
                }
                std::thread::yield_now();
            }
        });
    });
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.system_settings_catalog().len(), MAX_SYSTEM_SETTINGS);
        assert_eq!(
            storage.system_settings_catalog().capacity(),
            MAX_SYSTEM_SETTINGS
        );
        assert_eq!(
            storage
                .system_settings()
                .filter(|(_, image)| image.live)
                .count(),
            names.len()
        );
        let retained = storage.system_settings();
        for name in names {
            storage
                .install_system_setting(SqlName::parse(name).unwrap(), None)
                .unwrap();
        }
        assert_eq!(
            retained.filter(|(_, image)| image.live).count(),
            names.len()
        );
        assert_eq!(
            storage
                .system_settings()
                .filter(|(_, image)| image.live)
                .count(),
            0
        );
        for index in 0..MAX_SYSTEM_SETTINGS {
            use core::fmt::Write;
            let mut name = StackStr::<64>::new();
            write!(name, "setting_{index}").unwrap();
            storage
                .install_system_setting(
                    SqlName::parse(name.as_str()).unwrap(),
                    Some(StackStr::from_str("value")),
                )
                .unwrap();
        }
        assert_eq!(
            storage
                .change_system_setting(
                    SqlName::parse("overflow").unwrap(),
                    Some(StackStr::from_str("value")),
                    30
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
    });
}

#[test]
fn table_catalog_lifecycle_capacity_rollback_and_reuse_preserve_retained_identity() {
    let mut config = test_config();
    config.max_tables = 1;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let definition = make_def("lifecycle_original", &[("id", ColType::Int4, true)]);
        let slot = storage.create_table_in(definition, 7).unwrap();
        let images = storage.table_definition_images();
        images.definition(&storage, slot, 7).unwrap();
        let original = images.retained_identity(slot, 7).unwrap();
        assert_eq!(
            original.existence,
            CatalogDdlState::PendingCreate { txid: 7 }
        );
        assert!(storage.table(slot).visible_to(7));
        assert!(!storage.table(slot).visible_to(8));
        let dropped = storage.drop_table_in(slot, 7).unwrap();
        assert!(!storage.table(slot).visible_to(7));
        assert!(!storage.table(slot).visible_to(8));
        assert!(!storage.table(slot).is_free());
        let replacement = make_def("lifecycle_replacement", &[("id", ColType::Int4, true)]);
        let error = storage.create_table_in(replacement.clone(), 8).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(error.message.as_str(), "too many tables (limit 1)");
        storage.finish_table_drop(dropped, crate::storage::CatalogDdlDecision::Rollback);
        assert_eq!(storage.table(slot).identity().existence, original.existence);
        assert!(storage.table(slot).visible_to(7));
        assert!(!storage.table(slot).visible_to(8));
        storage.commit_create(slot);
        assert!(storage.table(slot).visible_to(8));
        let dropped = storage.drop_table_in(slot, 9).unwrap();
        assert!(!storage.table(slot).visible_to(9));
        assert!(storage.table(slot).visible_to(8));
        storage.finish_table_drop(dropped, crate::storage::CatalogDdlDecision::Rollback);
        assert!(storage.table(slot).visible_to(9));
        storage.drop_table_in(slot, 9).unwrap();
        storage.commit_drop(slot);
        let reused = storage.create_table_in(replacement, 8).unwrap();
        assert_eq!(reused, slot);
        assert_ne!(storage.table(slot).created_at(), original.created_at);
        assert_eq!(
            images.definition(&storage, slot, 7).unwrap_err().sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        assert_eq!(
            images.retained_definition(slot, 7).unwrap().name.as_str(),
            "lifecycle_original"
        );
        assert_eq!(
            images.retained_identity(slot, 7).unwrap().created_at,
            original.created_at
        );
        storage.rollback_create(slot);
        assert!(storage.table(slot).is_free());
    });
}

#[test]
fn value_cache_owners_survive_replacement_and_storage_removal() {
    let mut config = test_config();
    config.max_value_indexes = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut definition = make_def("owned_cache", &[("id", ColType::Int4, true)]);
    definition.columns[0].unique = true;
    let table = storage.create_table(definition).unwrap();
    let selected = storage.tables[table].enforcer(0).unwrap();
    selected.cache.write().insert(41, 99).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage.refresh_enforcers(table).unwrap();
        let replacement = storage.tables[table].enforcer(0).unwrap();
        assert_ne!(replacement.cache.slot(), selected.cache.slot());
        assert!(replacement.cache.read().is_empty());
        let mut original = 0;
        selected.cache.read().probe(41, |row| original = row);
        assert_eq!(original, 99);
        assert_eq!(selected.columns(), &[0]);
    });
    drop(storage);
    crate::mem::guard::forbid_alloc(|| {
        let mut original = 0;
        selected.cache.read().probe(41, |row| original = row);
        assert_eq!(original, 99);
    });
}

#[test]
fn value_cache_owners_failed_candidate_releases_capacity_and_publishes_no_prefix() {
    let mut config = test_config();
    config.max_value_indexes = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut definition = make_def(
        "cache_candidate",
        &[("id", ColType::Int4, true), ("other", ColType::Int4, true)],
    );
    definition.columns[0].unique = true;
    let table = storage.create_table(definition).unwrap();
    let selected = storage.tables[table].enforcer(0).unwrap();
    selected.cache.write().insert(41, 99).unwrap();
    let revision = storage.reserve_table_definition_revision().unwrap();
    let mut updated = TableDefBuilder::from_definition(&storage.table_def(table, 0));
    updated.columns[1].unique = true;
    *storage.tables[table].definition_mut(revision) =
        updated.freeze(&storage.metadata_pool).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let error = storage.refresh_enforcers(table).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("max_value_indexes"));
        {
            let publication = storage.tables[table].publication();
            assert_eq!(publication.enforcers.len(), 1);
            assert_eq!(
                publication.enforcers[0].as_ref().unwrap().cache.slot(),
                selected.cache.slot()
            );
            assert!(publication.enforcers[1..].iter().all(Option::is_none));
        }
        let free = storage.value_indexes.acquire().unwrap();
        assert!(storage.value_indexes.acquire().is_none());
        drop(free);
        let mut original = 0;
        selected.cache.read().probe(41, |row| original = row);
        assert_eq!(original, 99);
        drop(selected);
        storage
            .build_enforcers_visible(
                table,
                None,
                key_preparation::EnforcerRows::ReusePrepared,
                None,
            )
            .unwrap();
        let publication = storage.tables[table].publication();
        assert_eq!(publication.enforcers.len(), 2);
        assert!(publication.enforcers[..2].iter().all(Option::is_some));
    });
}

#[test]
fn cluster_metadata_failed_table_creation_releases_identity_and_value_bindings() {
    let mut config = test_config();
    config.max_value_indexes = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut retained = make_def("retained", &[("id", ColType::Int4, true)]);
    retained.columns[0].unique = true;
    let retained_slot = storage.create_table(retained.clone()).unwrap();
    let mut rejected = make_def(
        "rejected",
        &[("a", ColType::Int4, true), ("b", ColType::Int4, true)],
    );
    rejected.columns[0].unique = true;
    rejected.columns[1].unique = true;
    crate::mem::guard::forbid_alloc(|| {
        let slot = storage.tables.iter().position(Table::is_free).unwrap();
        for owner in [None, Some(7)] {
            let result = match owner {
                None => storage.create_table(rejected.clone()),
                Some(txid) => storage.create_table_in(rejected.clone(), txid),
            };
            assert_eq!(
                result.unwrap_err().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(
                storage
                    .find_visible("public", "rejected", owner.unwrap_or(0))
                    .is_none()
            );
            let rejected = &storage.tables[slot];
            assert!(rejected.is_free());
            assert_eq!(rejected.definition().name, SqlName::EMPTY);
            assert_eq!(rejected.definition().n_columns, 0);
            assert_eq!(rejected.created_at(), 0);
            assert_eq!(rejected.ownership().owner, Ownership::BOOTSTRAP.owner);
            assert!(rejected.ownership().pending.is_none());
            assert_eq!(rejected.existence(), CatalogDdlState::Absent);
            assert_eq!(rejected.enforcer_count(), 0);
            assert!(rejected.enforcers().iter().all(Option::is_none));
            assert_eq!(storage.tables[retained_slot].enforcer_count(), 1);
            let mut replacement = retained.clone();
            replacement.name = SqlName::parse("replacement").unwrap();
            let reused = storage.create_table_in(replacement, 8).unwrap();
            assert_eq!(reused, slot);
            assert_eq!(storage.tables[reused].enforcer_count(), 1);
            storage.rollback_create(reused);
        }
    });
}

#[test]
fn catalog_creation_role_dependencies_wait_for_pending_owners() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let (role, _) = storage
        .create_role(
            SqlName::parse("pending_owner").unwrap(),
            RoleAttributes {
                superuser: true,
                ..RoleAttributes::ORDINARY
            },
            7,
        )
        .unwrap();
    storage.commit_role_change(role);
    let actor = SessionIdentity::bound(storage.capture_role_slot(role, 0).unwrap());
    let _actor = crate::sql::eval::funcs::system::enter_current_user(actor);
    crate::mem::guard::forbid_alloc(|| {
        let table = storage
            .create_table_in(
                make_def("pending_owned_table", &[("id", ColType::Int4, false)]),
                7,
            )
            .unwrap();
        assert_eq!(
            storage
                .role_object_dependency(role, 8)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        let renamed = storage
            .rename_role(role, SqlName::parse("pending_renamed_owner").unwrap(), 8)
            .unwrap();
        let during_rename = storage
            .create_table_in(
                make_def(
                    "create_during_owner_rename",
                    &[("id", ColType::Int4, false)],
                ),
                9,
            )
            .unwrap();
        storage.rollback_create(during_rename);
        storage.release_object_locks(9);
        storage.rollback_role_change(role, renamed);
        storage.rollback_create(table);
        storage.release_object_locks(7);
        assert_eq!(storage.role_object_dependency(role, 8).unwrap(), None);
        let prior = storage.drop_role_in(role, 8).unwrap();
        let sequence = storage.catalog_identity_sequence();
        let error = storage
            .create_table_in(
                make_def("wait_for_owner_drop", &[("id", ColType::Int4, false)]),
                9,
            )
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
        assert_eq!(storage.catalog_identity_sequence(), sequence);
        assert!(
            storage
                .find_table("public", "wait_for_owner_drop")
                .is_none()
        );
        storage.rollback_role_change(role, prior);
    });
}

#[test]
fn catalog_creation_role_install_rejects_identity_conflicts_and_pending_sources() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let name = SqlName::parse("installed_actor").unwrap();
    let (slot, _) = storage
        .create_role(name, RoleAttributes::ORDINARY, 7)
        .unwrap();
    let oid = storage.role(slot).oid();
    let sequence = storage.role_identity_sequence();
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            storage
                .install_role(name, RoleAttributes::ORDINARY, oid, slot)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        assert!(storage.role(slot).visible_to(7));
        assert!(!storage.role(slot).visible_to(0));
        storage.commit_role_change(slot);
        assert_eq!(
            storage
                .install_role(name, RoleAttributes::ORDINARY, oid, slot)
                .unwrap(),
            slot
        );
        let actor = SessionIdentity::bound(storage.capture_role_slot(slot, 0).unwrap());
        let other_oid = RoleOid::parse(16_400).unwrap();
        assert_eq!(
            storage
                .install_role(name, RoleAttributes::ORDINARY, other_oid, slot)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(storage.role_identity_sequence(), sequence);
        assert_eq!(storage.role_oid(slot), oid.get());
        let renamed = SqlName::parse("installed_renamed_actor").unwrap();
        assert_eq!(
            storage
                .install_role(renamed, RoleAttributes::ORDINARY, oid, slot)
                .unwrap(),
            slot
        );
        assert_eq!(actor.refresh(&storage, 0).name().as_str(), renamed.as_str());
    });
}

#[test]
fn catalog_creation_role_oid_exhaustion_preserves_the_catalog() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage
        .restore_role_identity_sequence(i32::MAX as u64)
        .unwrap();
    let sequence = storage.catalog_identity_sequence();
    crate::mem::guard::forbid_alloc(|| {
        for _ in 0..2 {
            let error = storage
                .create_role(
                    SqlName::parse("exhausted_actor").unwrap(),
                    RoleAttributes::ORDINARY,
                    7,
                )
                .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(error.message.as_str(), "role identity space exhausted");
            assert!(storage.find_role_visible("exhausted_actor", 7).is_none());
            assert_eq!(storage.role_identity_sequence(), i32::MAX as u64);
            assert_eq!(storage.catalog_identity_sequence(), sequence);
            assert_eq!(storage.role_oid(0), 10);
        }
    });
}

#[test]
fn catalog_creation_known_name_requires_an_authenticated_identity() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let actor = SessionIdentity::unauthenticated("postgres");
    let _actor = crate::sql::eval::funcs::system::enter_current_user(actor);
    crate::mem::guard::forbid_alloc(|| {
        assert!(storage.current_role_slot(7).is_none());
        assert_missing_creation_actor(storage.initial_ownership(7));
        assert_eq!(
            actor.require_name().unwrap_err().sqlstate,
            sqlstate::INSUFFICIENT_PRIVILEGE
        );
    });
}

#[test]
fn catalog_creation_role_identity_survives_rename_but_rejects_slot_reuse() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let name = SqlName::parse("source_actor").unwrap();
    let renamed = SqlName::parse("renamed_actor").unwrap();
    let slot = storage
        .install_role(
            name,
            RoleAttributes::ORDINARY,
            storage.catalog_sources.next_role().unwrap(),
            1,
        )
        .unwrap();
    let actor = SessionIdentity::bound(storage.capture_role_slot(slot, 0).unwrap());
    crate::mem::guard::forbid_alloc(|| {
        let prior = storage.rename_role(slot, renamed, 7).unwrap();
        assert_eq!(actor.refresh(&storage, 0).name().as_str(), name.as_str());
        assert_eq!(actor.refresh(&storage, 7).name().as_str(), renamed.as_str());
        storage.rollback_role_change(slot, prior);
        assert_eq!(actor.refresh(&storage, 7).name().as_str(), name.as_str());
        storage.rename_role(slot, renamed, 7).unwrap();
        storage.commit_role_change(slot);
        assert_eq!(actor.refresh(&storage, 0).name().as_str(), renamed.as_str());
        storage.drop_role_in(slot, 8).unwrap();
        storage.commit_role_change(slot);
        assert!(storage.session_role_slot(actor.source(), 0).is_none());
        assert_eq!(
            storage
                .install_role(
                    name,
                    RoleAttributes::ORDINARY,
                    storage.catalog_sources.next_role().unwrap(),
                    slot
                )
                .unwrap(),
            slot
        );
        assert_eq!(
            actor
                .refresh(&storage, 0)
                .require_name()
                .unwrap_err()
                .sqlstate,
            sqlstate::UNDEFINED_OBJECT
        );
        let _actor = crate::sql::eval::funcs::system::enter_current_user(actor);
        assert_missing_creation_actor(storage.initial_ownership(9));
        let replacement = SessionIdentity::bound(storage.capture_role_slot(slot, 0).unwrap());
        assert_ne!(actor.source(), replacement.source());
        assert_eq!(
            storage.session_role_slot(replacement.source(), 0),
            Some(slot)
        );
    });
}

#[test]
fn catalog_creation_role_identity_rejects_rollback_and_foreign_storage() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut other_budget = test_budget(&config);
    let other = Storage::new(&config, &mut other_budget, 0).unwrap();
    let name = SqlName::parse("aborted_actor").unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let (slot, prior) = storage
            .create_role(name, RoleAttributes::ORDINARY, 7)
            .unwrap();
        let actor = SessionIdentity::bound(storage.capture_role_slot(slot, 7).unwrap());
        assert_eq!(storage.session_role_slot(actor.source(), 7), Some(slot));
        assert!(storage.session_role_slot(actor.source(), 8).is_none());
        storage.rollback_role_change(slot, prior);
        assert_eq!(
            storage
                .install_role(
                    name,
                    RoleAttributes::ORDINARY,
                    storage.catalog_sources.next_role().unwrap(),
                    slot
                )
                .unwrap(),
            slot
        );
        assert!(storage.session_role_slot(actor.source(), 7).is_none());
        let local = SessionIdentity::bound(storage.capture_role_slot(slot, 0).unwrap());
        assert!(other.session_role_slot(local.source(), 0).is_none());
        let bootstrap = SessionIdentity::bound(storage.capture_role_slot(0, 0).unwrap());
        assert!(other.session_role_slot(bootstrap.source(), 0).is_none());
    });
}

fn assert_missing_creation_actor<T>(result: Result<T, SqlError>) {
    let error = match result {
        Ok(_) => panic!("a missing actor was accepted as an object owner"),
        Err(error) => error,
    };
    assert_eq!(error.sqlstate, sqlstate::INSUFFICIENT_PRIVILEGE);
    assert_eq!(
        error.message.as_str(),
        "current role is not present in the role catalog"
    );
}

#[test]
fn catalog_creation_missing_actor_preserves_catalogs_and_identifiers() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let wrapper = storage
        .create_foreign_wrapper(
            foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("base_wrapper").unwrap(),
                handler: foreign::ForeignDataHandler::None,
                validator: foreign::ForeignDataValidator::None,
                options: foreign::ForeignOptions::EMPTY,
            },
            0,
        )
        .unwrap();
    let server = storage
        .create_foreign_server(
            foreign::ForeignServerDefinition {
                name: SqlName::parse("base_server").unwrap(),
                wrapper: wrapper as u16,
                server_type: None,
                version: None,
                options: foreign::ForeignOptions::EMPTY,
            },
            0,
        )
        .unwrap();
    let table = storage
        .create_table(make_def("base_table", &[("id", ColType::Int4, false)]))
        .unwrap();
    let sequence = storage.catalog_sequence.current();
    let next_oid = storage.large_objects.lock().unwrap().next_oid;
    let actor = crate::sql::eval::funcs::system::enter_current_user(
        SessionIdentity::unauthenticated("missing_creator"),
    );
    crate::mem::guard::forbid_alloc(|| {
        assert_missing_creation_actor(storage.initial_ownership(7));
        assert_missing_creation_actor(storage.initial_catalog_owner(7));
        assert_missing_creation_actor(storage.create_foreign_wrapper(
            foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("unowned_wrapper").unwrap(),
                handler: foreign::ForeignDataHandler::None,
                validator: foreign::ForeignDataValidator::None,
                options: foreign::ForeignOptions::EMPTY,
            },
            7,
        ));
        assert_missing_creation_actor(storage.create_foreign_server(
            foreign::ForeignServerDefinition {
                name: SqlName::parse("unowned_server").unwrap(),
                wrapper: wrapper as u16,
                server_type: None,
                version: None,
                options: foreign::ForeignOptions::EMPTY,
            },
            7,
        ));
        assert_missing_creation_actor(storage.create_foreign_table_binding(
            foreign::ForeignTableDefinition {
                table: table as u16,
                server: server as u16,
                options: foreign::ForeignOptions::EMPTY,
                column_options: foreign::ForeignColumnOptions::EMPTY,
            },
            7,
        ));
        assert_missing_creation_actor(storage.create_large_object(CatalogCreation::New, None, 7));
        assert_missing_creation_actor(table_creation::allocate(
            &storage,
            make_def("unowned_table", &[("id", ColType::Int4, false)]),
            CatalogAllocation::New,
            TableCreation::Pending(7),
        ));
        assert_eq!(storage.catalog_sequence.current(), sequence);
        assert_eq!(storage.large_objects.lock().unwrap().next_oid, next_oid);
        assert!(storage.find_table("public", "unowned_table").is_none());
    });
    drop(actor);
    let (_, oid) = storage
        .create_large_object(CatalogCreation::New, None, 7)
        .unwrap();
    assert_eq!(Some(oid), next_oid);
    assert_eq!(storage.catalog_sequence.current(), sequence + 1);
}

#[test]
fn catalog_creation_replay_uses_its_explicit_owner_without_a_session_actor() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let _actor = crate::sql::eval::funcs::system::enter_current_user(
        SessionIdentity::unauthenticated("missing_creator"),
    );
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.initial_ownership(0).unwrap().owner, BOOTSTRAP_ROLE);
        let slot = storage
            .alloc_schema(
                SqlName::parse("replayed_schema").unwrap(),
                CatalogDdlState::Present,
                Ownership::BOOTSTRAP,
                schema_names::SchemaAllocation::New,
            )
            .unwrap();
        let schemas = storage.schemas.lock().unwrap();
        assert_eq!(schemas[slot].ownership.owner, BOOTSTRAP_ROLE);
        assert!(schemas[slot].ownership.pending.is_none());
    });
}

#[test]
fn cluster_metadata_table_names_reserve_only_within_their_database() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let prior_database = current_database();
        crate::sql::set_execution_database_oid(DatabaseOid::TEMPLATE1);
        let foreign =
            storage.create_table_in(make_def("reserved", &[("id", ColType::Int4, true)]), 7);
        crate::sql::set_execution_database_oid(prior_database);
        let foreign = foreign.unwrap();
        let local = storage
            .create_table_in(make_def("reserved", &[("id", ColType::Int4, true)]), 8)
            .unwrap();
        assert_eq!(
            storage.ddl_name_locked_by_other("public", "reserved", 9),
            Some(8)
        );
        storage.rollback_create(local);
        assert_eq!(
            storage.ddl_name_locked_by_other("public", "reserved", 9),
            None
        );
        let (target, selected_definition) = storage.select_table_definition(foreign, 7);
        let mut renamed = TableDefBuilder::from_definition(&selected_definition);
        renamed.name = SqlName::parse("pending_name").unwrap();
        assert_eq!(
            storage
                .write_table_def(
                    target,
                    renamed.clone(),
                    &[None; MAX_COLUMNS],
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE,
        );
        crate::sql::set_execution_database_oid(DatabaseOid::TEMPLATE1);
        storage.commit_create(foreign);
        let change = storage.write_table_def(
            target,
            renamed,
            &[None; MAX_COLUMNS],
            false,
            crate::storage::ColumnSet::EMPTY,
        );
        crate::sql::set_execution_database_oid(prior_database);
        let _change = change.unwrap();
        assert_eq!(
            storage.ddl_name_locked_by_other("public", "pending_name", 9),
            None
        );
        let local = storage
            .create_table_in(make_def("pending_name", &[("id", ColType::Int4, true)]), 9)
            .unwrap();
        assert_eq!(
            storage.ddl_name_locked_by_other("public", "pending_name", 10),
            Some(9)
        );
        storage.rollback_create(local);
    });
}

#[test]
fn cluster_metadata_system_settings_reserve_reset_names_and_reject_truncation() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let name = SqlName::parse("pending_setting").unwrap();
        let (slot, prior) = storage
            .change_system_setting(name, Some(StackStr::from_str("first")), 11)
            .unwrap()
            .unwrap();
        assert_eq!(
            storage
                .change_system_setting(name, None, 12)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        storage.release_row_locks(12);
        assert_eq!(
            storage
                .install_system_setting(name, Some(StackStr::from_str("replayed")))
                .unwrap_err()
                .sqlstate,
            sqlstate::OBJECT_IN_USE
        );
        storage.rollback_system_setting(slot, prior);
        assert!(
            storage
                .change_system_setting(name, None, 12)
                .unwrap()
                .is_none()
        );
        storage
            .install_system_setting(name, Some(StackStr::from_str("retained")))
            .unwrap();
        let (slot, prior) = storage
            .change_system_setting(name, None, 11)
            .unwrap()
            .unwrap();
        assert!(
            storage
                .system_settings()
                .find(|(index, _)| *index == slot)
                .unwrap()
                .1
                .live
        );
        storage.rollback_system_setting(slot, prior);
        assert_eq!(
            storage
                .system_settings()
                .find(|(index, _)| *index == slot)
                .unwrap()
                .1
                .value
                .as_str(),
            "retained"
        );
        let oversized = [b'x'; ROLE_SETTING_VALUE_MAX + 1];
        let oversized = StackStr::from_str(core::str::from_utf8(&oversized).unwrap());
        assert_eq!(
            storage
                .change_system_setting(name, Some(oversized), 11)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .install_system_setting(name, Some(oversized))
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .system_settings()
                .find(|(index, _)| *index == slot)
                .unwrap()
                .1
                .value
                .as_str(),
            "retained"
        );
        let (slot, _) = storage
            .change_system_setting(name, None, 11)
            .unwrap()
            .unwrap();
        storage.commit_system_setting(slot);
        let image = storage
            .system_settings()
            .find(|(index, _)| *index == slot)
            .unwrap()
            .1;
        assert_eq!(image.name, SqlName::EMPTY);
        assert!(image.value.as_str().is_empty());
        assert!(!image.live);
        assert!(image.pending.is_none());
    });
}

#[test]
fn cluster_metadata_prepared_catalog_snapshots_are_atomic_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PreparedTransactionCatalog>();
    let mut config = test_config();
    config.max_prepared_transactions = 2;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut arena = Arena::new(&mut budget, "prepared reader", 4096).unwrap();
    let initial = [
        test_prepared_catalog_entry(10, "first", 1),
        test_prepared_catalog_entry(11, "second", 1),
    ];
    storage
        .replace_prepared_transaction_catalog(initial)
        .unwrap();
    let barrier = crate::mem::mutex::Barrier::new(5);
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let storage = &storage;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for iteration in 0..16 {
                        let generation = worker * 100 + iteration + 1;
                        storage
                            .replace_prepared_transaction_catalog([
                                test_prepared_catalog_entry(
                                    generation as u32 * 2,
                                    "first",
                                    generation,
                                ),
                                test_prepared_catalog_entry(
                                    generation as u32 * 2 + 1,
                                    "second",
                                    generation,
                                ),
                            ])
                            .unwrap();
                    }
                });
            });
        }
        barrier.wait();
        crate::mem::guard::forbid_alloc(|| {
            for _ in 0..256 {
                arena.reset();
                let image = storage
                    .prepared_transaction_catalog_snapshot(&arena)
                    .unwrap();
                assert_eq!(image.len(), 2);
                assert_eq!(image[0].prepared_lsn, image[1].prepared_lsn);
                assert_eq!(image[0].transaction_id + 1, image[1].transaction_id);
                assert_eq!(image[0].gid.as_str(), "first");
                assert_eq!(image[1].gid.as_str(), "second");
                assert_eq!(
                    storage.with_prepared_transaction_catalog(|entries| entries.len()),
                    2
                );
                std::thread::yield_now();
            }
        });
    });
    crate::mem::guard::forbid_alloc(|| {
        let catalog = storage.prepared_transactions.lock().unwrap();
        assert_eq!(catalog.entries.capacity(), 2);
        assert_eq!(catalog.candidate.capacity(), 2);
        assert_eq!(catalog.entries.len(), 2);
        assert!(catalog.candidate.is_empty());
    });
}

#[test]
fn cluster_metadata_prepared_catalog_rejection_preserves_published_images() {
    let mut config = test_config();
    config.max_prepared_transactions = 2;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let tiny_arena = Arena::new(&mut budget, "small prepared snapshot", 8).unwrap();
    let mut tiny_snapshot = FixedVec::new(&mut budget, "small prepared roster", 1).unwrap();
    let initial = [
        test_prepared_catalog_entry(10, "first", 1),
        test_prepared_catalog_entry(11, "second", 1),
    ];
    tiny_snapshot.push(initial[0]).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .replace_prepared_transaction_catalog(initial)
            .unwrap();
        for duplicate in [
            test_prepared_catalog_entry(10, "third", 1),
            test_prepared_catalog_entry(12, "first", 1),
        ] {
            assert_eq!(
                storage
                    .replace_prepared_transaction_catalog([initial[0], duplicate])
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_OBJECT
            );
            assert_eq!(
                storage
                    .install_prepared_transaction_catalog_entry(duplicate)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DUPLICATE_OBJECT
            );
            assert_eq!(
                storage.with_prepared_transaction_catalog(|entries| [entries[0], entries[1]]),
                initial
            );
        }
        assert_eq!(
            storage
                .replace_prepared_transaction_catalog([
                    initial[0],
                    initial[1],
                    test_prepared_catalog_entry(12, "third", 1)
                ])
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .prepared_transaction_catalog_snapshot(&tiny_arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .copy_prepared_transaction_catalog(&mut tiny_snapshot)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(tiny_snapshot.len(), 1);
        assert_eq!(tiny_snapshot[0], initial[0]);
        let oversized_gid = [b'x'; 200];
        let oversized_gid = core::str::from_utf8(&oversized_gid).unwrap();
        for invalid in [
            test_prepared_catalog_entry(0, "invalid", 1),
            test_prepared_catalog_entry(12, "invalid", 0),
            test_prepared_catalog_entry(12, "invalid\0gid", 1),
            test_prepared_catalog_entry(12, oversized_gid, 1),
        ] {
            assert_eq!(
                storage
                    .replace_prepared_transaction_catalog([invalid])
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INVALID_PARAMETER_VALUE
            );
            assert_eq!(
                storage
                    .install_prepared_transaction_catalog_entry(invalid)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::INVALID_PARAMETER_VALUE
            );
            assert_eq!(
                storage.with_prepared_transaction_catalog(|entries| [entries[0], entries[1]]),
                initial
            );
        }
        storage.replace_prepared_transaction_catalog([]).unwrap();
        assert_eq!(
            storage.with_prepared_transaction_catalog(|entries| entries.len()),
            0
        );
        storage
            .install_prepared_transaction_catalog_entry(initial[0])
            .unwrap();
        assert_eq!(
            storage.with_prepared_transaction_catalog(|entries| entries[0]),
            initial[0]
        );
    });
}

#[test]
fn cluster_metadata_disabled_prepared_catalog_rejects_publication() {
    let mut config = test_config();
    config.max_prepared_transactions = 0;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let entry = test_prepared_catalog_entry(10, "disabled", 1);
        assert_eq!(
            storage
                .replace_prepared_transaction_catalog([entry])
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .install_prepared_transaction_catalog_entry(entry)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage.with_prepared_transaction_catalog(|entries| entries.len()),
            0
        );
        let catalog = storage.prepared_transactions.lock().unwrap();
        assert_eq!(catalog.entries.capacity(), 0);
        assert_eq!(catalog.candidate.capacity(), 0);
        assert!(catalog.candidate.is_empty());
    });
}

#[test]
fn database_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DatabaseDef>();
    assert_send_sync::<Mutex<DatabaseCatalog>>();
    assert_send_sync::<DatabaseIter<'_>>();
    const WORKERS: usize = 4;
    let names = ["database_a", "database_b", "database_c", "database_d"];
    let staged = ["staged_a", "staged_b", "staged_c", "staged_d"];
    let mut config = test_config();
    config.max_databases = 3 + WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let barrier = crate::mem::mutex::Barrier::new(WORKERS + 1);
    let finished = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            let barrier = &barrier;
            let finished = &finished;
            scope.spawn(move || {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    let txid = worker as u32 + 20;
                    for iteration in 0..16 {
                        let slot = storage
                            .reserve_database(
                                None,
                                test_database_definition(names[worker]),
                                0,
                                txid,
                            )
                            .unwrap();
                        let image = storage.database_named(names[worker], txid).unwrap().1;
                        assert_eq!(image.ddl_state, CatalogDdlState::PendingCreate { txid });
                        storage
                            .alter_database_definition(
                                slot,
                                test_database_definition(staged[worker]),
                                txid,
                            )
                            .unwrap();
                        if iteration % 2 == 0 {
                            storage.retire_database(slot, true);
                        } else {
                            storage.publish_database_create(slot);
                            storage.commit_database_alter(slot, txid);
                            assert_eq!(
                                storage
                                    .database_named(staged[worker], 0)
                                    .unwrap()
                                    .1
                                    .definition
                                    .name
                                    .as_str(),
                                staged[worker]
                            );
                            if iteration != 15 {
                                storage.drop_database(slot, txid);
                                storage.retire_database(slot, false);
                            }
                        }
                    }
                });
                finished.fetch_add(1, std::sync::atomic::Ordering::Release);
            });
        }
        barrier.wait();
        crate::mem::guard::forbid_alloc(|| {
            while finished.load(std::sync::atomic::Ordering::Acquire) != WORKERS {
                let statistics = storage.cumulative_statistics();
                let catalog = storage.database_catalog();
                for (slot, database) in catalog.definitions.iter().enumerate() {
                    assert_eq!(statistics.databases[slot].oid, database.oid);
                    if database.ddl_state == CatalogDdlState::Absent {
                        assert_eq!(database.definition.name, SqlName::EMPTY);
                        assert!(database.pending.is_none());
                        assert!(database.ownership.pending.is_none());
                    } else {
                        assert!(
                            catalog.definitions[..slot]
                                .iter()
                                .all(|other| other.ddl_state == CatalogDdlState::Absent
                                    || other.oid != database.oid)
                        );
                    }
                }
                drop(catalog);
                drop(statistics);
                for (slot, database) in storage.databases_visible_to(0) {
                    let _ = storage.database_named(database.definition.name.as_str(), 0);
                    assert_eq!(
                        storage
                            .database_cumulative_statistics(slot, database.oid)
                            .oid,
                        database.oid
                    );
                }
                storage.reset_current_database_statistics();
                std::thread::yield_now();
            }
        });
    });
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.database_count(), 3 + WORKERS);
        assert_eq!(
            storage.database_catalog().definitions.capacity(),
            3 + WORKERS
        );
        assert_eq!(storage.database_catalog().definitions.len(), 3 + WORKERS);
        assert_eq!(storage.databases_visible_to(0).count(), 3 + WORKERS);
        assert_eq!(
            storage
                .reserve_database(None, test_database_definition("overflow"), 0, 30)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
    });
}

#[test]
fn database_reservation_rejects_pending_and_recovery_collisions() {
    let mut config = test_config();
    config.max_databases = 7;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let first = storage
            .reserve_database(None, test_database_definition("pending_database"), 0, 11)
            .unwrap();
        let first_oid = storage.database(first).oid;
        assert_eq!(
            storage
                .reserve_database(None, test_database_definition("pending_database"), 0, 12)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        storage.release_row_locks(12);
        let second = storage
            .reserve_database(None, test_database_definition("other_database"), 0, 12)
            .unwrap();
        assert_ne!(first_oid, storage.database(second).oid);
        assert_eq!(
            storage
                .reserve_database(Some(first_oid), test_database_definition("same_oid"), 0, 12)
                .unwrap_err()
                .sqlstate,
            sqlstate::DUPLICATE_OBJECT
        );
        storage
            .alter_database_definition(first, test_database_definition("pending_rename"), 11)
            .unwrap();
        assert_eq!(
            storage
                .reserve_database(None, test_database_definition("pending_rename"), 0, 12)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        storage.release_row_locks(12);
        assert_eq!(
            storage
                .alter_database_definition(second, test_database_definition("pending_rename"), 12)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        storage.release_row_locks(12);
        assert_eq!(
            storage
                .restore_database(first_oid, test_database_definition("recovery_oid"), 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::DUPLICATE_OBJECT
        );
        assert_eq!(
            storage
                .restore_database(
                    DatabaseOid(USER_DATABASE_OID_BASE + 100),
                    test_database_definition("pending_rename"),
                    0
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::DUPLICATE_OBJECT
        );
        assert_eq!(storage.databases_visible_to(11).count(), 4);
        assert_eq!(storage.databases_visible_to(12).count(), 4);
        storage.retire_database(first, true);
        storage.retire_database(second, true);
        let maximum = storage
            .reserve_database(
                Some(DatabaseOid(i32::MAX)),
                test_database_definition("maximum_oid"),
                0,
                13,
            )
            .unwrap();
        assert_eq!(
            storage
                .reserve_database(None, test_database_definition("oid_overflow"), 0, 14)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.databases_visible_to(13).count(), 4);
        storage.retire_database(maximum, true);
        assert_eq!(storage.databases_visible_to(0).count(), 3);
    });
}

#[test]
fn guarded_spill_generations_database_clone_preserves_eviction_boundaries() {
    fn handle(tag: u8) -> crate::store::SstHandle {
        crate::store::SstHandle {
            index: crate::store::BlockId([tag; 32]),
            filter: crate::store::BlockId([tag + 1; 32]),
            roster: crate::store::BlockId([tag + 2; 32]),
            format: crate::store::RowSstFormat::PackedV4,
        }
    }
    let mut config = test_config();
    config.max_databases = 4;
    config.max_schemas = 5;
    config.max_spill_generations_per_table = 3;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let source = storage
        .create_table(make_def("saved_orders", &[("id", ColType::Int4, false)]))
        .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .append_spill_through(
                &storage
                    .reserve_spill_generation(
                        source,
                        storage.table(source).created_at(),
                        storage.table(source).rewrite_generation(),
                    )
                    .unwrap(),
                handle(1),
                7,
            )
            .unwrap();
        storage
            .append_spill_through(
                &storage
                    .reserve_spill_generation(
                        source,
                        storage.table(source).created_at(),
                        storage.table(source).rewrite_generation(),
                    )
                    .unwrap(),
                handle(5),
                9,
            )
            .unwrap();
        let database = storage
            .create_database(
                None,
                DatabaseOid::POSTGRES,
                DatabaseCloneAllocation::New,
                test_database_definition("cloned_orders"),
                0,
                17,
            )
            .unwrap();
        let oid = storage.database(database).oid;
        storage.commit_database_create(database);
        storage.select_database(oid).unwrap();
        let target = storage.find_table("public", "saved_orders").unwrap();
        assert_ne!(source, target);
        storage
            .append_spill_through(
                &storage
                    .reserve_spill_generation(
                        target,
                        storage.table(target).created_at(),
                        storage.table(target).rewrite_generation(),
                    )
                    .unwrap(),
                handle(9),
                11,
            )
            .unwrap();
        for (rowid, commit_lsn) in [(1, 7), (2, 9), (3, 11)] {
            let values = [Datum::Int4(rowid as i32)];
            let (location, bytes) = storage.heap.append(rowenc::encoded_len(&values)).unwrap();
            rowenc::encode(&values, bytes);
            storage.tables[target]
                .rows
                .insert(rowid, RowState::committed_only_at(location, commit_lsn))
                .unwrap();
        }
        storage.finish_row_cleanup(
            RowCleanupTask::HeapRows(HeapCleanup::AllSaved),
            RowCleanupTarget::Table(target),
        );
        for (rowid, sst, commit_lsn) in [(1, 0, 7), (2, 1, 9), (3, 2, 11)] {
            let state = storage.resident_row_state(target, rowid).unwrap().unwrap();
            assert!(
                matches!(state.committed, Some(RowHome::Spilled { sst: member, commit_lsn: version, .. })
                if member == SpillGenerationId::fixture(sst) && version == commit_lsn)
            );
        }
        assert_eq!(storage.table(source).spill_generations().len(), 2);
        assert_eq!(storage.table(target).spill_generations().len(), 3);
        storage.select_database(DatabaseOid::POSTGRES).unwrap();
    });
}

#[test]
fn database_retirement_and_failed_clone_clear_complete_images() {
    let mut config = test_config();
    config.max_databases = 4;
    config.max_schemas = 5;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for committed in [false, true] {
            let slot = storage
                .create_database(
                    None,
                    DatabaseOid::TEMPLATE1,
                    DatabaseCloneAllocation::New,
                    test_database_definition("retired_database"),
                    0,
                    17,
                )
                .unwrap();
            storage
                .alter_database_definition(slot, test_database_definition("pending_name"), 17)
                .unwrap();
            let oid = storage.database(slot).oid;
            storage.cumulative_statistics().databases[slot].sessions = 123;
            if committed {
                storage.commit_database_create(slot);
                storage.drop_database(slot, 17);
                storage.commit_database_drop(slot);
            } else {
                storage.rollback_database_create(slot);
            }
            let image = storage.database(slot);
            assert_eq!(image.ddl_state, CatalogDdlState::Absent);
            assert_eq!(image.definition.name, SqlName::EMPTY);
            assert!(image.pending.is_none());
            assert!(image.ownership.pending.is_none());
            assert_eq!(
                storage.database_cumulative_statistics(slot, oid).sessions,
                0
            );
            assert!(
                !storage
                    .schemas
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|schema| schema.database == oid && schema.visible_to(0))
            );
        }
        let old = storage
            .reserve_database(None, test_database_definition("old_identity"), 0, 18)
            .unwrap();
        let old_oid = storage.database(old).oid;
        storage.retire_database(old, true);
        let reused = storage
            .reserve_database(None, test_database_definition("new_identity"), 0, 18)
            .unwrap();
        let new_oid = storage.database(reused).oid;
        assert_eq!(old, reused);
        assert!(new_oid > old_oid);
        storage.cumulative_statistics().databases[reused].sessions = 321;
        assert_eq!(
            storage
                .database_cumulative_statistics(old, old_oid)
                .sessions,
            0
        );
        assert_eq!(
            storage
                .database_cumulative_statistics(reused, new_oid)
                .sessions,
            321
        );
        storage.retire_database(reused, true);
        storage
            .alloc_schema(
                SqlName::parse("clone_extra").unwrap(),
                CatalogDdlState::Present,
                Ownership::BOOTSTRAP,
                schema_names::SchemaAllocation::New,
            )
            .unwrap();
        assert_eq!(
            storage
                .create_database(
                    None,
                    DatabaseOid::POSTGRES,
                    DatabaseCloneAllocation::New,
                    test_database_definition("failed_clone"),
                    0,
                    19
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.databases_visible_to(19).count(), 3);
        assert_eq!(storage.database(3).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.database(3).definition.name, SqlName::EMPTY);
        assert_eq!(storage.cumulative_statistics().databases[3].sessions, 0);
        assert_eq!(
            storage
                .schemas
                .lock()
                .unwrap()
                .iter()
                .filter(|schema| schema.visible_to(19))
                .count(),
            4
        );
        assert!(storage.find_schema("clone_extra").is_some());
    });
}

fn test_index_definition(name: &str) -> IndexDef {
    IndexDef {
        schema: SqlName::parse("public").unwrap(),
        name: SqlName::parse(name).unwrap(),
        table: SqlName::parse("index_target").unwrap(),
        n_cols: 1,
        ..IndexDef::EMPTY
    }
}

#[test]
fn index_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<IndexDef>();
    assert_send_sync::<Mutex<FixedVec<IndexDef>>>();
    assert_send_sync::<IndexIter<'_>>();
    const WORKERS: usize = 4;
    let names = ["index_a", "index_b", "index_c", "index_d"];
    let mut config = test_config();
    config.max_indexes = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage
        .create_table(make_def("index_target", &[("id", ColType::Int4, false)]))
        .unwrap();
    let barrier = crate::mem::mutex::Barrier::new(WORKERS + 1);
    let finished = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for name in names {
            let storage = &storage;
            let barrier = &barrier;
            let finished = &finished;
            scope.spawn(move || {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for iteration in 0..16 {
                        let slot = storage
                            .create_index(test_index_definition(name), 0)
                            .unwrap();
                        assert_eq!(
                            storage
                                .index_definition("public", name, 0)
                                .unwrap()
                                .created_at,
                            storage.index(slot).created_at
                        );
                        if iteration != 15 {
                            storage.rollback_index_create(slot);
                        }
                    }
                });
                finished.fetch_add(1, std::sync::atomic::Ordering::Release);
            });
        }
        barrier.wait();
        crate::mem::guard::forbid_alloc(|| {
            while finished.load(std::sync::atomic::Ordering::Acquire) != WORKERS {
                let indexes = storage.index_catalog();
                let maintenance = storage.brin_maintenance();
                for (slot, index) in indexes.iter().enumerate() {
                    assert_eq!(maintenance.states[slot].index_created_at, index.created_at);
                    if index.ddl_state == CatalogDdlState::Absent {
                        assert_eq!(index.name, SqlName::EMPTY);
                        assert_eq!(index.n_cols, 0);
                    }
                }
                drop(maintenance);
                drop(indexes);
                for index in storage.indexes_for("public", "index_target", 0) {
                    let _ = storage.index_definition("public", index.name.as_str(), 0);
                }
                std::thread::yield_now();
            }
        });
    });
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.index_count(), WORKERS);
        assert_eq!(storage.index_catalog().capacity(), WORKERS);
        assert_eq!(storage.index_catalog().len(), WORKERS);
        assert_eq!(
            storage.indexes_for("public", "index_target", 0).count(),
            WORKERS
        );
        for slot in 0..WORKERS {
            storage.commit_index_create(slot).unwrap();
        }
        assert_eq!(storage.checkpoint_indexes().count(), WORKERS);
        assert_eq!(storage.live_indexes().count(), WORKERS);
        assert_eq!(
            storage
                .create_index(test_index_definition("overflow"), 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .create_index(test_index_definition("index_a"), 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::DUPLICATE_TABLE
        );
    });
}

#[test]
fn index_slot_retirement_clears_complete_images() {
    let mut config = test_config();
    config.max_indexes = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage
        .create_table(make_def("index_target", &[("id", ColType::Int4, false)]))
        .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut invalid = test_index_definition("invalid_generation");
        invalid.created_at = MAX_INDEX_OID_GENERATION + 1;
        assert_eq!(
            storage.create_index(invalid, 17).unwrap_err().sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.index(0).created_at, 0);
        assert_eq!(storage.brin_maintenance().states[0].index_created_at, 0);
        for committed in [false, true] {
            let slot = storage
                .create_index(test_index_definition("retired"), 17)
                .unwrap();
            storage
                .rename_index(slot, SqlName::parse("pending_name").unwrap(), 17)
                .unwrap();
            storage
                .alter_index_definition(
                    slot,
                    IndexMutableDefinition {
                        clustered: true,
                        ..IndexMutableDefinition::DEFAULT
                    },
                    17,
                )
                .unwrap();
            let mut maintenance = storage.brin_maintenance();
            maintenance.states[slot].summarized_until_page = 128;
            maintenance.unsummarized_ranges[0] = 1;
            drop(maintenance);
            if committed {
                storage.commit_index_create(slot).unwrap();
                storage.drop_index("public", "pending_name", 17).unwrap();
                storage.commit_index_drop(slot).unwrap();
            } else {
                storage.rollback_index_create(slot);
            }
            let image = storage.index(slot);
            assert_eq!(image.ddl_state, CatalogDdlState::Absent);
            assert_eq!(image.created_at, 0);
            assert_eq!(image.name, SqlName::EMPTY);
            assert!(image.pending_name.is_none());
            assert!(image.pending_definition.is_none());
            assert_eq!(image.n_cols, 0);
            assert!(image.ownership.pending.is_none());
            assert_eq!(image.ownership.owner, Ownership::BOOTSTRAP.owner);
            assert_eq!(storage.brin_maintenance().states[slot].index_created_at, 0);
            assert!(
                storage
                    .brin_maintenance()
                    .unsummarized_ranges
                    .iter()
                    .all(|range| *range == 0)
            );
            assert!(storage.index_table_slot(slot).is_none());
        }
    });
}

#[test]
fn index_rename_checks_only_its_database() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage
        .create_table(make_def("index_target", &[("id", ColType::Int4, false)]))
        .unwrap();
    let slot = storage
        .create_index(test_index_definition("local_index"), 17)
        .unwrap();
    let foreign = storage
        .create_index(test_index_definition("remote_index"), 18)
        .unwrap();
    storage.commit_index_create(foreign).unwrap();
    storage.index_catalog()[foreign].database = DatabaseOid::TEMPLATE1;
    storage
        .apply_comment(
            CommentClass::Relation,
            SqlName::parse("public").unwrap(),
            SqlName::parse("local_index").unwrap(),
            0,
            Some(StackStr::from_str("local")),
        )
        .unwrap();
    {
        let mut comments = storage.comment_catalog();
        let local = comments
            .entries
            .iter()
            .position(|entry| entry.used)
            .unwrap();
        let foreign = comments
            .entries
            .iter()
            .position(|entry| !entry.used)
            .unwrap();
        comments.entries[foreign] = comments.entries[local];
        comments.entries[foreign].database = Some(DatabaseOid::TEMPLATE1);
    }
    crate::mem::guard::forbid_alloc(|| {
        storage
            .rename_index(slot, SqlName::parse("remote_index").unwrap(), 17)
            .unwrap();
        storage.rollback_index_rename(slot, None);
        storage.index_catalog()[foreign].pending_name = Some(PendingIndexName {
            txid: 18,
            name: SqlName::parse("remote_pending").unwrap(),
        });
        storage
            .rename_index(slot, SqlName::parse("remote_pending").unwrap(), 17)
            .unwrap();
        assert_eq!(
            storage
                .create_index(test_index_definition("remote_pending"), 19)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        storage.release_row_locks(19);
        storage.commit_index_rename(slot, 17).unwrap();
        assert!(
            storage
                .comment_text(CommentClass::Relation, "public", "remote_pending", 0, 0)
                .is_some()
        );
        assert!(
            storage
                .comment_catalog()
                .entries
                .iter()
                .any(|entry| entry.used
                    && entry.database == Some(DatabaseOid::TEMPLATE1)
                    && entry.name.as_str() == "local_index")
        );
        storage.index_catalog()[foreign].database = DatabaseOid::POSTGRES;
        storage.rollback_index_rename(slot, None);
        assert_eq!(
            storage
                .rename_index(slot, SqlName::parse("remote_index").unwrap(), 17)
                .unwrap_err()
                .sqlstate,
            sqlstate::DUPLICATE_TABLE
        );
    });
}

#[test]
fn view_rule_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ViewRuleCatalog>();
    assert_send_sync::<Mutex<ViewRuleCatalog>>();
    assert_send_sync::<ViewRuleIter<'_, ViewDef>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_views = WORKERS;
    config.max_rules = WORKERS;
    config.max_stored_query_dependencies_per_object = 1;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut arena = Arena::new(&mut budget, "view snapshot test", 1 << 16).unwrap();
    let barrier = crate::mem::mutex::Barrier::new(WORKERS + 1);
    let finished = std::sync::atomic::AtomicUsize::new(0);
    let names = ["worker_a", "worker_b", "worker_c", "worker_d"];

    std::thread::scope(|scope| {
        for (worker, worker_name) in names.iter().copied().enumerate() {
            let storage = &storage;
            let barrier = &barrier;
            let finished = &finished;
            scope.spawn(move || {
                let schema = SqlName::parse("public").unwrap();
                let name = SqlName::parse(worker_name).unwrap();
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for iteration in 0..16 {
                        let publication_slot = storage
                            .create_view(
                                schema,
                                name,
                                test_view_definition(StoredQueryDependencies::EMPTY),
                                false,
                                worker as u32 + 20,
                            )
                            .unwrap();
                        assert!(matches!(publication_slot, ViewPublication::Created(_)));
                        if iteration == 15 {
                            storage.finish_view_publication(
                                publication_slot,
                                crate::storage::CatalogDdlDecision::Commit,
                            );
                        } else {
                            storage.finish_view_publication(
                                publication_slot,
                                crate::storage::CatalogDdlDecision::Rollback,
                            );
                        }
                    }
                });
                finished.fetch_add(1, std::sync::atomic::Ordering::Release);
            });
        }
        barrier.wait();
        crate::mem::guard::forbid_alloc(|| {
            let mut polls = 0;
            while finished.load(std::sync::atomic::Ordering::Acquire) != WORKERS || polls < 1024 {
                arena.reset();
                let catalog = storage.view_rule_catalog();
                for (slot, view) in catalog.views.iter().enumerate() {
                    if view.ddl_state == CatalogDdlState::Absent {
                        assert_eq!(view.return_rule, u16::MAX);
                        continue;
                    }
                    let rule = &catalog.rules[usize::from(view.return_rule)];
                    assert_eq!(rule.definition.target, RuleTarget::View(slot as u16));
                    assert_eq!(rule.ddl_state, view.ddl_state);
                }
                for (slot, rule) in catalog.rules.iter().enumerate() {
                    if rule.ddl_state != CatalogDdlState::Absent {
                        let RuleTarget::View(view) = rule.definition.target else {
                            panic!("unexpected table rule");
                        };
                        assert_eq!(catalog.views[usize::from(view)].return_rule, slot as u16);
                    }
                }
                drop(catalog);
                for (slot, view) in storage.checkpoint_views() {
                    let query = storage.snapshot_view_query(slot, 0, &arena).unwrap();
                    assert_eq!(query.sql.as_str(), "SELECT 1");
                    assert_eq!(query.creation_path.as_str(), "public");
                    assert_eq!(
                        storage.rule(usize::from(view.return_rule)).ddl_state,
                        CatalogDdlState::Present
                    );
                }
                polls += 1;
                std::thread::yield_now();
            }
        });
    });

    arena.reset();
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(storage.view_count(), WORKERS);
        assert_eq!(storage.rule_count(), WORKERS);
        assert_eq!(storage.checkpoint_views().count(), WORKERS);
        for (slot, view) in storage.views_visible_to(0) {
            let rule = storage.rule(usize::from(view.return_rule));
            assert_eq!(rule.definition.target, RuleTarget::View(slot as u16));
            let query = storage.snapshot_view_query(slot, 0, &arena).unwrap();
            assert_eq!(query.sql.as_str(), "SELECT 1");
            assert_eq!(query.creation_path.as_str(), "public");
            assert!(query.dependencies.entries().is_empty());
            assert_eq!(storage.view_sql(slot), query.sql);
        }
        assert_eq!(
            storage
                .create_view(
                    SqlName::parse("public").unwrap(),
                    SqlName::parse("overflow").unwrap(),
                    test_view_definition(StoredQueryDependencies::EMPTY),
                    false,
                    30,
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
    });
}

fn test_table_dependency(storage: &Storage, table: usize) -> StoredQueryDependency {
    let identity = storage
        .capture_stored_query_relation(ResolvedRelation::Table(table), 0)
        .unwrap();
    let definition = storage.table_def(table, 0);
    StoredQueryDependency {
        class: DependencyClass::Table,
        slot: table as u16,
        identity,
        referenced_columns: ColumnSet::EMPTY,
        schema: definition.schema,
        name: definition.name,
        referenced_schema: SqlName::EMPTY,
        referenced_name: definition.name,
    }
}

fn oversized_test_dependencies(storage: &Storage, table: usize) -> StoredQueryDependencies {
    let dependency = test_table_dependency(storage, table);
    let mut dependencies = StoredQueryDependencies::EMPTY;
    dependencies.push(dependency).unwrap();
    dependencies
        .push(StoredQueryDependency {
            referenced_schema: dependency.schema,
            ..dependency
        })
        .unwrap();
    dependencies
}

#[test]
fn failed_rule_creation_preserves_capacity_and_dependency_images() {
    let mut config = test_config();
    config.max_rules = 1;
    config.max_stored_query_dependencies_per_object = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage.create_table(make_def("rule_target", &[])).unwrap();
    let definition = RuleDefinition {
        name: SqlName::parse("test_rule").unwrap(),
        target: RuleTarget::Table(table as u16),
        ..RuleDefinition::EMPTY
    };
    let oversized = oversized_test_dependencies(&storage, table);
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            storage
                .create_rule(definition, oversized.view(), false, 7)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.rule(0).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.rule(0).created_at, 0);
        assert!(storage.rule_dependencies(0, 7).entries().is_empty());
        let publication_slot = storage
            .create_rule(definition, StoredQueryDependencies::EMPTY.view(), false, 7)
            .unwrap();
        let slot = publication_slot.slot();
        let (prior, _) = storage
            .alter_rule(
                storage.prepare_rule_edit(slot, 7).unwrap(),
                RuleMetadataChange::SetEnabled(definition.enabled),
                7,
            )
            .unwrap();
        assert!(prior.prior.is_none());
        storage.finish_rule_publication(publication_slot, None, CatalogDdlDecision::Rollback);
        assert_eq!(storage.rule(slot).created_at, 0);
        assert!(storage.rule(slot).pending.is_none());
        assert!(
            storage
                .stored_query_dependencies
                .lock()
                .unwrap()
                .pending
                .iter()
                .all(|pending| !pending.used)
        );
        let publication_reused = storage
            .create_rule(definition, StoredQueryDependencies::EMPTY.view(), false, 8)
            .unwrap();
        let reused = publication_reused.slot();
        assert_eq!(reused, slot);
        storage.finish_rule_publication(publication_reused, None, CatalogDdlDecision::Commit);
        let dropped = storage
            .drop_rule(storage.prepare_rule_edit(reused, 8).unwrap(), 8)
            .unwrap();
        storage.commit_rule_drop(dropped);
        assert_eq!(storage.rule(reused).definition.name, SqlName::EMPTY);
        assert_eq!(storage.rule(reused).created_at, 0);
    });
}

#[test]
fn failed_policy_edits_preserve_roles_definitions_and_pending_capacity() {
    let mut config = test_config();
    config.max_catalog_versions_per_object = 1;
    config.max_stored_query_dependencies_per_object = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("policy_target", &[]))
        .unwrap();
    let definition = PolicyDefinition {
        using: Some(StackStr::from_str("true")),
        with_check: None,
    };
    let spec = PolicySpec {
        name: SqlName::parse("bounded_policy").unwrap(),
        table,
        command: PolicyCommandKind::All,
        permissive: true,
        definition,
        roles: &[PUBLIC_ROLE],
    };
    let empty = StoredQueryDependencies::EMPTY;
    let oversized = oversized_test_dependencies(&storage, table);
    let changed = PolicyDefinition {
        using: Some(StackStr::from_str("false")),
        with_check: None,
    };
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            storage
                .create_policy(spec, &oversized, 7)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert!(
            storage
                .policy_catalog
                .lock()
                .unwrap()
                .definitions
                .iter()
                .all(|policy| policy.ddl_state == CatalogDdlState::Absent)
        );
        let slot = storage.create_policy(spec, &empty, 7).unwrap();
        storage.commit_policy_create(slot);
        for (roles, dependencies) in [(&[][..], &empty), (&[PUBLIC_ROLE][..], &oversized)] {
            assert_eq!(
                storage
                    .alter_policy(slot, changed, roles, dependencies, 8)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert!(storage.policy(slot).pending_definition.is_none());
            assert_eq!(storage.policy(slot).definition.using, definition.using);
            assert_eq!(&*storage.policy_roles(slot, 8), &[PUBLIC_ROLE]);
            assert!(
                storage
                    .stored_query_dependencies
                    .lock()
                    .unwrap()
                    .pending
                    .iter()
                    .all(|image| !image.used)
            );
        }
        let prior = storage
            .alter_policy(slot, changed, &[PUBLIC_ROLE, 0], &empty, 8)
            .unwrap();
        assert!(prior.is_none());
        let pending_slot = storage
            .policy(slot)
            .pending_definition
            .unwrap()
            .dependency_slot;
        assert_eq!(
            storage
                .alter_policy(slot, definition, &[PUBLIC_ROLE], &empty, 8)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .policy(slot)
                .pending_definition
                .unwrap()
                .dependency_slot,
            pending_slot
        );
        assert_eq!(&*storage.policy_roles(slot, 8), &[PUBLIC_ROLE, 0]);
        assert_eq!(&*storage.policy_roles(slot, 0), &[PUBLIC_ROLE]);
        assert_eq!(
            storage
                .stored_query_dependencies
                .lock()
                .unwrap()
                .pending
                .iter()
                .filter(|image| image.used)
                .count(),
            1
        );
        storage.rollback_policy_alter(slot, prior);
        assert!(storage.policy(slot).pending_definition.is_none());
        assert!(
            storage
                .stored_query_dependencies
                .lock()
                .unwrap()
                .pending
                .iter()
                .all(|image| !image.used)
        );
        storage
            .alter_policy(slot, changed, &[PUBLIC_ROLE], &empty, 9)
            .unwrap();
        storage.commit_policy_alter(slot, 9);
        assert_eq!(storage.policy(slot).definition.using, changed.using);
        assert!(
            storage
                .stored_query_dependencies
                .lock()
                .unwrap()
                .pending
                .iter()
                .all(|image| !image.used)
        );
    });
}

#[test]
fn failed_view_creation_and_replacement_leave_both_catalogs_unchanged() {
    let mut config = test_config();
    config.max_views = 3;
    config.max_rules = 2;
    config.max_stored_query_dependencies_per_object = 1;
    config.max_catalog_versions_per_object = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage.create_table(make_def("view_source", &[])).unwrap();
    let schema = SqlName::parse("public").unwrap();
    let name = SqlName::parse("test_view").unwrap();
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            storage
                .create_view(
                    schema,
                    name,
                    test_view_definition(oversized_test_dependencies(&storage, table)),
                    false,
                    7,
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert!(
            storage
                .view_entries()
                .map(|(_, view)| view)
                .all(|view| view.created_at == 0
                    && view.ddl_state == CatalogDdlState::Absent
                    && view.return_rule == u16::MAX)
        );
        assert_eq!(storage.rule(0).ddl_state, CatalogDdlState::Absent);
        let publication_slot = storage
            .create_view(
                schema,
                name,
                test_view_definition(StoredQueryDependencies::EMPTY),
                false,
                7,
            )
            .unwrap();
        let slot = publication_slot.slot();
        storage
            .finish_view_publication(publication_slot, crate::storage::CatalogDdlDecision::Commit);
        assert_eq!(
            storage
                .create_view(
                    schema,
                    name,
                    test_view_definition(oversized_test_dependencies(&storage, table)),
                    true,
                    8,
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.view(slot).ddl_state, CatalogDdlState::Present);
        assert_eq!(storage.rule(0).ddl_state, CatalogDdlState::Present);
        assert_eq!(storage.rule(1).ddl_state, CatalogDdlState::Absent);
        let publication_filler = storage
            .create_view(
                schema,
                SqlName::parse("filler").unwrap(),
                test_view_definition(StoredQueryDependencies::EMPTY),
                false,
                8,
            )
            .unwrap();
        let filler = publication_filler.slot();
        assert_ne!(filler, slot);
        storage.finish_view_publication(
            publication_filler,
            crate::storage::CatalogDdlDecision::Commit,
        );
        let mut replacement = test_view_definition(StoredQueryDependencies::EMPTY);
        replacement.query.sql = StackStr::from_str("SELECT 2");
        let publication = storage
            .create_view(schema, name, replacement, true, 8)
            .unwrap();
        assert_eq!(publication.slot(), slot);
        assert_eq!(storage.view_sql(slot).as_str(), "SELECT 1");
        assert_eq!(
            storage.rule(0).definition_for(8).source.as_str(),
            "SELECT 2"
        );
        assert_eq!(
            storage
                .create_view(
                    schema,
                    name,
                    test_view_definition(StoredQueryDependencies::EMPTY),
                    true,
                    8,
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.view(slot).ddl_state, CatalogDdlState::Present);
        assert_eq!(storage.rule(0).ddl_state, CatalogDdlState::Present);
        assert_eq!(storage.view_sql(slot).as_str(), "SELECT 1");
        assert_eq!(
            storage.rule(0).definition_for(8).source.as_str(),
            "SELECT 2"
        );
        storage.finish_view_publication(publication, CatalogDdlDecision::Rollback);
        assert!(storage.view(slot).pending_definition.is_none());
        assert!(storage.rule(0).pending.is_none());
        assert_eq!(storage.view(2).created_at, 0);
        let dropped = storage
            .drop_view("public", "test_view", 8)
            .unwrap()
            .unwrap();
        storage.commit_view_drop(dropped);
        assert_eq!(storage.view(slot).created_at, 0);
        assert_eq!(storage.view(slot).name, SqlName::EMPTY);
        assert_eq!(storage.view(slot).return_rule, u16::MAX);
        assert_eq!(storage.rule(0).created_at, 0);
        let publication_reused = storage
            .create_view(
                schema,
                name,
                test_view_definition(StoredQueryDependencies::EMPTY),
                false,
                9,
            )
            .unwrap();
        let reused = publication_reused.slot();
        assert_eq!(reused, slot);
        storage.finish_view_publication(
            publication_reused,
            crate::storage::CatalogDdlDecision::Rollback,
        );
        assert_eq!(storage.view(slot).created_at, 0);
        assert_eq!(storage.view(slot).return_rule, u16::MAX);
        assert_eq!(storage.rule(0).created_at, 0);
    });
}

#[test]
fn checkpoint_duplicate_view_rejection_preserves_existing_view_and_rule() {
    let mut config = test_config();
    config.max_views = 3;
    config.max_rules = 3;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let schema = SqlName::parse("public").unwrap();
    let name = SqlName::parse("test_view").unwrap();
    storage
        .replay_view(
            ViewCatalogIdentity::new(2, 7, 2, 8).unwrap(),
            schema,
            name,
            test_view_definition(StoredQueryDependencies::EMPTY),
        )
        .unwrap();
    let return_rule = storage.view_return_rule(2);
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            storage
                .replay_view(
                    ViewCatalogIdentity::new(1, 9, 1, 10).unwrap(),
                    schema,
                    name,
                    test_view_definition(StoredQueryDependencies::EMPTY)
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(storage.view(2).ddl_state, CatalogDdlState::Present);
        assert_eq!(
            storage.rule(return_rule).ddl_state,
            CatalogDdlState::Present
        );
        assert_eq!(
            storage.rule(return_rule).definition.target,
            RuleTarget::View(2)
        );
        assert_eq!(storage.live_views().count(), 1);
        assert_eq!(storage.rules_visible_to(0).count(), 1);
        for slot in [0, 1] {
            assert_eq!(storage.view(slot).created_at, 0);
            assert_eq!(storage.view(slot).return_rule, u16::MAX);
        }
    });
}

#[test]
fn failed_rule_replay_preserves_view_link_and_original_dependencies() {
    let mut config = test_config();
    config.max_views = 2;
    config.max_rules = 2;
    config.max_stored_query_dependencies_per_object = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage.create_table(make_def("rule_target", &[])).unwrap();
    let schema = SqlName::parse("public").unwrap();
    let name = SqlName::parse("test_view").unwrap();
    let dependency = test_table_dependency(&storage, table);
    let mut dependencies = StoredQueryDependencies::EMPTY;
    dependencies.push(dependency).unwrap();
    storage
        .replay_view(
            ViewCatalogIdentity::new(0, 7, 0, 8).unwrap(),
            schema,
            name,
            test_view_definition(dependencies),
        )
        .unwrap();
    let definition = RuleDefinition {
        name: SqlName::parse("replayed_rule").unwrap(),
        target: RuleTarget::Table(table as u16),
        ..RuleDefinition::EMPTY
    };
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            storage
                .replay_rule(
                    0,
                    42,
                    definition,
                    oversized_test_dependencies(&storage, table)
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.view_return_rule(0), 0);
        assert_eq!(storage.view_sql(0).as_str(), "SELECT 1");
        assert_eq!(storage.rule(1).ddl_state, CatalogDdlState::Absent);
        assert_eq!(storage.rule_dependencies(0, 0).entries(), &[dependency]);
        assert!(!storage.table_has_rules(table, 0));
        assert_eq!(
            storage
                .replay_rule(0, 42, definition, StoredQueryDependencies::EMPTY)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        storage
            .replay_rule(1, 42, definition, StoredQueryDependencies::EMPTY)
            .unwrap();
        assert_eq!(storage.view_return_rule(0), 0);
        assert_eq!(storage.view_sql(0).as_str(), "SELECT 1");
        assert_eq!(storage.rule_dependencies(0, 0).entries(), &[dependency]);
        assert!(storage.rule_dependencies(1, 0).entries().is_empty());
        assert!(storage.table_has_rules(table, 0));
        let renamed = RuleDefinition {
            name: SqlName::parse("renamed_rule").unwrap(),
            ..definition
        };
        storage
            .replay_rule(1, 42, renamed, StoredQueryDependencies::EMPTY)
            .unwrap();
        assert_eq!(storage.rule(1).definition.name.as_str(), "renamed_rule");
        let invalid_return = RuleDefinition {
            name: SqlName::parse("renamed_return").unwrap(),
            target: RuleTarget::View(0),
            ..definition
        };
        assert_eq!(
            storage
                .replay_rule(0, 8, invalid_return, StoredQueryDependencies::EMPTY)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(storage.view_return_rule(0), 0);
        assert_eq!(storage.view_sql(0).as_str(), "SELECT 1");
    });
}

#[test]
fn view_creation_and_drop_use_transactional_names() {
    let mut config = test_config();
    config.max_views = 2;
    config.max_rules = 2;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let schema = SqlName::parse("public").unwrap();
    let old = SqlName::parse("old_view").unwrap();
    let new = SqlName::parse("new_view").unwrap();
    let publication_slot = storage
        .create_view(
            schema,
            old,
            test_view_definition(StoredQueryDependencies::EMPTY),
            false,
            7,
        )
        .unwrap();
    let slot = publication_slot.slot();
    storage.finish_view_publication(publication_slot, crate::storage::CatalogDdlDecision::Commit);
    crate::mem::guard::forbid_alloc(|| {
        let prior = storage
            .stage_view_rename(storage.prepare_view_edit(slot, 8).unwrap(), new, 8)
            .unwrap();
        assert_eq!(
            storage
                .create_view(
                    schema,
                    new,
                    test_view_definition(StoredQueryDependencies::EMPTY),
                    false,
                    8
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::DUPLICATE_TABLE
        );
        let publication_reused_name = storage
            .create_view(
                schema,
                old,
                test_view_definition(StoredQueryDependencies::EMPTY),
                false,
                8,
            )
            .unwrap();
        assert_ne!(publication_reused_name.slot(), slot);
        storage.finish_view_publication(
            publication_reused_name,
            crate::storage::CatalogDdlDecision::Rollback,
        );
        let dropped = storage.drop_view("public", "new_view", 8).unwrap().unwrap();
        assert_eq!(dropped.slot(), slot);
        storage.rollback_view_drop(dropped);
        storage.rollback_view_alter(prior);
        assert!(storage.find_view("public", "old_view", 8).is_some());
        assert!(storage.find_view("public", "new_view", 8).is_none());
    });
}

#[test]
fn database_removal_clears_view_rules_and_pending_dependency_chains() {
    let mut config = test_config();
    config.max_views = 2;
    config.max_rules = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let schema = SqlName::parse("public").unwrap();
    let name = SqlName::parse("test_view").unwrap();
    let publication_slot = storage
        .create_view(
            schema,
            name,
            test_view_definition(StoredQueryDependencies::EMPTY),
            false,
            7,
        )
        .unwrap();
    let slot = publication_slot.slot();
    storage.finish_view_publication(publication_slot, crate::storage::CatalogDdlDecision::Commit);
    let rule = storage.view_return_rule(slot);
    let definition = storage.rule(rule).definition;
    storage
        .alter_rule(
            storage.prepare_rule_edit(rule, 8).unwrap(),
            RuleMetadataChange::SetEnabled(definition.enabled),
            8,
        )
        .unwrap();
    storage
        .alter_rule(
            storage.prepare_rule_edit(rule, 8).unwrap(),
            RuleMetadataChange::SetEnabled(definition.enabled),
            8,
        )
        .unwrap();
    let other_database = DatabaseOid::parse(USER_DATABASE_OID_BASE + 1).unwrap();
    storage.view_rule_catalog().views[slot].database = other_database;
    storage.view_rule_catalog().rules[rule].database = other_database;
    crate::mem::guard::forbid_alloc(|| {
        storage.clear_database_catalog(other_database);
        assert_eq!(storage.view(slot).created_at, 0);
        assert_eq!(storage.view(slot).return_rule, u16::MAX);
        assert_eq!(storage.rule(rule).created_at, 0);
        assert!(storage.rule(rule).pending.is_none());
        assert!(storage.rule_dependencies(rule, 8).entries().is_empty());
        assert!(
            storage
                .stored_query_dependencies
                .lock()
                .unwrap()
                .pending
                .iter()
                .all(|pending| !pending.used)
        );
    });
}

fn create_native_sequence(storage: &mut Storage, name: &str, spec: SeqSpec) -> usize {
    let change = storage
        .create_sequence(
            SequenceCreateSpec {
                allocation: CatalogAllocation::New,
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse(name).unwrap(),
                spec,
                owner: None,
                generator_for: None,
                persistence: RelationPersistence::Permanent,
            },
            0,
        )
        .unwrap();
    storage.finish_sequence_change(change, CatalogDdlDecision::Commit);
    change.slot()
}

fn native_sequence_spec() -> SeqSpec {
    SeqSpec {
        data_type: SeqType::Bigint,
        increment: 1,
        min_value: 1,
        max_value: i64::MAX,
        start_value: 1,
        cache: 1,
        cycle: false,
    }
}

#[test]
fn sequence_values_concurrent_advances_are_unique_and_allocation_free() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let sequence = create_native_sequence(&mut storage, "serial_counter", native_sequence_spec());
    let barrier = crate::mem::mutex::Barrier::new(4);
    let seen: [std::sync::atomic::AtomicBool; 256] =
        core::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let storage = &storage;
            let barrier = &barrier;
            let seen = &seen;
            scope.spawn(move || {
                barrier.wait();
                crate::mem::guard::forbid_alloc(|| {
                    for _ in 0..64 {
                        let (value, reserved, _) =
                            storage.reserve_sequence_values(sequence, 0, 1).unwrap();
                        assert_eq!(reserved, 1);
                        assert!(
                            !seen[value as usize - 1]
                                .swap(true, std::sync::atomic::Ordering::Relaxed)
                        );
                    }
                });
            });
        }
    });
    assert!(
        seen.iter()
            .all(|value| value.load(std::sync::atomic::Ordering::Relaxed))
    );
    assert_eq!(storage.sequence_value_for(sequence, 0), (256, true));
}

#[test]
fn sequence_values_staging_preserves_other_publishers_and_later_advances() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let sequence = create_native_sequence(&mut storage, "serial_staging", native_sequence_spec());
    crate::mem::guard::forbid_alloc(|| {
        assert!(!storage.sequence_value_image_for(sequence, 0).3);
        storage.set_sequence_value(sequence, 0, 7, true).unwrap();
        let captured = storage.sequence_value_image_for(sequence, 0);
        storage.mark_sequence_value_staged(sequence, 10, captured.4);
        storage.reserve_sequence_values(sequence, 0, 1).unwrap();
        let advanced = storage.sequence_value_image_for(sequence, 0);
        storage.mark_sequence_value_staged(sequence, 11, advanced.4);
        storage.clear_sequence_value_dirty(sequence, 10);
        assert!(storage.sequence_value_image_for(sequence, 0).3);
        assert_eq!(captured.0, 7);
        assert_eq!(advanced.0, 8);
        storage.clear_sequence_value_dirty(sequence, 11);
        assert!(!storage.sequence_value_image_for(sequence, 0).3);
        storage.reserve_sequence_values(sequence, 0, 1).unwrap();
        storage.mark_sequence_value_staged(sequence, 10, advanced.4);
        storage.clear_sequence_value_dirty(sequence, 10);
        storage.clear_sequence_value_dirty(sequence, 11);
        assert!(storage.sequence_value_image_for(sequence, 0).3);
        let retried = storage.sequence_value_image_for(sequence, 0);
        storage.mark_sequence_value_staged(sequence, 12, retried.4);
        storage.clear_sequence_value_dirty(sequence, 10);
        assert!(storage.sequence_value_image_for(sequence, 0).3);
        storage.clear_sequence_value_dirty(sequence, 12);
        assert!(!storage.sequence_value_image_for(sequence, 0).3);
    });
}

#[test]
fn sequence_values_slot_reuse_and_replay_reject_stale_creation_and_receipts() {
    let mut config = test_config();
    config.max_sequences = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let sequence = create_native_sequence(&mut storage, "old_serial", native_sequence_spec());
    crate::mem::guard::forbid_alloc(|| {
        storage.set_sequence_value(sequence, 0, 91, true).unwrap();
        let old_position = storage.sequence_position(sequence);
        let old_image = storage.sequence_value_image_for(sequence, 0);
        storage.mark_sequence_value_staged(sequence, 10, old_image.4);
        let dropped = storage
            .drop_sequence("public", "old_serial", 0)
            .unwrap()
            .unwrap();
        storage.finish_sequence_change(dropped, CatalogDdlDecision::Commit);
        let replacement =
            create_native_sequence(&mut storage, "new_serial", native_sequence_spec());
        assert_eq!(replacement, sequence);
        assert_eq!(storage.sequence_value_for(replacement, 0), (1, false));
        assert!(!storage.sequence_value_image_for(replacement, 0).3);
        storage.set_sequence_value(replacement, 0, 3, true).unwrap();
        let generation = storage.sequence_value_image_for(replacement, 0).4;
        storage.mark_sequence_value_staged(replacement, 11, generation);
        storage.clear_sequence_value_dirty(replacement, 10);
        assert!(storage.sequence_value_image_for(replacement, 0).3);
        assert_eq!(
            storage
                .apply_sequence_advance(old_position, "public", "new_serial", 99, true, 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(storage.sequence_value_for(replacement, 0), (3, true));
        storage.clear_sequence_value_dirty(replacement, 11);
        assert!(!storage.sequence_value_image_for(replacement, 0).3);
        storage.restore_sequence_value_image(replacement, 14, true, 0);
        assert_eq!(storage.sequence_value_for(replacement, 0), (14, true));
        assert!(!storage.sequence_value_image_for(replacement, 0).3);
    });
}

#[test]
fn sequence_values_range_and_generation_exhaustion_do_not_change_values_or_receipts() {
    let mut config = test_config();
    config.max_sequences = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for (data_type, minimum, maximum) in [
            (SeqType::Smallint, i64::from(i16::MIN), i64::from(i16::MAX)),
            (SeqType::Integer, i64::from(i32::MIN), i64::from(i32::MAX)),
            (SeqType::Bigint, i64::MIN, i64::MAX),
        ] {
            for (value, increment) in [(minimum, -1), (maximum, 1)] {
                let sequence = create_native_sequence(
                    &mut storage,
                    "serial_limits",
                    SeqSpec {
                        data_type,
                        increment,
                        min_value: minimum,
                        max_value: maximum,
                        start_value: value,
                        cache: 1,
                        cycle: false,
                    },
                );
                storage
                    .set_sequence_value(sequence, 0, value, true)
                    .unwrap();
                let before = storage.sequence_value_image_for(sequence, 0);
                storage.mark_sequence_value_staged(sequence, 10, before.4);
                assert_eq!(
                    storage
                        .reserve_sequence_values(sequence, 0, 1)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::SEQUENCE_GENERATOR_LIMIT_EXCEEDED
                );
                assert_eq!(storage.sequence_value_image_for(sequence, 0), before);
                storage.clear_sequence_value_dirty(sequence, 10);
                assert!(!storage.sequence_value_image_for(sequence, 0).3);
                let dropped = storage
                    .drop_sequence("public", "serial_limits", 0)
                    .unwrap()
                    .unwrap();
                storage.finish_sequence_change(dropped, CatalogDdlDecision::Commit);
            }
        }
        let sequence =
            create_native_sequence(&mut storage, "serial_generation", native_sequence_spec());
        storage.sequence_catalog().values[sequence]
            .committed
            .generation = u64::MAX;
        let before = storage.sequence_value_image_for(sequence, 0);
        assert_eq!(
            storage
                .reserve_sequence_values(sequence, 0, 1)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(
            storage
                .set_sequence_value(sequence, 0, 7, true)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.sequence_value_image_for(sequence, 0), before);
    });
}

#[test]
fn table_definition_images_survive_publication_rollback_and_slot_reuse() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("original", &[("id", ColType::Int8, true)]))
        .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let committed = storage.table_definition_images();
        let original = committed.definition(&storage, table, 0).unwrap();
        assert!(core::ptr::eq(
            original,
            committed.definition(&storage, table, 0).unwrap()
        ));
        let first_change = storage
            .write_table_def(
                storage.select_table_definition(table, 7).0,
                make_def("pending", &[("id", ColType::Int8, true)]),
                &[None; MAX_COLUMNS],
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let pending = storage.table_definition_images();
        let pending_image = pending.definition(&storage, table, 7).unwrap();
        let other_transaction = storage.table_definition_images();
        assert_eq!(
            other_transaction
                .definition(&storage, table, 8)
                .unwrap()
                .name
                .as_str(),
            "original"
        );
        storage.rollback_table_def(first_change);
        assert_eq!(pending_image.name.as_str(), "pending");
        assert!(core::ptr::eq(
            pending_image,
            pending.definition(&storage, table, 7).unwrap()
        ));
        let rolled_back = storage.table_definition_images();
        assert_eq!(
            rolled_back
                .definition(&storage, table, 7)
                .unwrap()
                .name
                .as_str(),
            "original"
        );
        let second_change = storage
            .write_table_def(
                storage.select_table_definition(table, 7).0,
                make_def("published", &[("id", ColType::Int8, true)]),
                &[None; MAX_COLUMNS],
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        assert_eq!(
            crate::storage::tests::publish_table_definition(&storage, second_change).unwrap(),
            Some(false)
        );
        assert_eq!(original.name.as_str(), "original");
        let published = storage.table_definition_images();
        assert_eq!(
            published
                .definition(&storage, table, 0)
                .unwrap()
                .name
                .as_str(),
            "published"
        );
        storage.commit_drop(table);
        let replacement = storage
            .create_table(make_def("replacement", &[("id", ColType::Int8, true)]))
            .unwrap();
        assert_eq!(replacement, table);
        assert_eq!(original.name.as_str(), "original");
        assert_eq!(pending_image.name.as_str(), "pending");
        assert_eq!(
            committed
                .definition(&storage, table, 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        let reused = storage.table_definition_images();
        assert_eq!(
            reused.definition(&storage, table, 0).unwrap().name.as_str(),
            "replacement"
        );
    });
}

#[test]
fn table_definition_images_reject_a_different_storage_owner() {
    let config = test_config();
    let mut first_budget = test_budget(&config);
    let mut second_budget = test_budget(&config);
    let mut first = Storage::new(&config, &mut first_budget, 0).unwrap();
    let mut second = Storage::new(&config, &mut second_budget, 0).unwrap();
    let table = first
        .create_table(make_def("first", &[("id", ColType::Int8, true)]))
        .unwrap();
    let other = second
        .create_table(make_def("second", &[("id", ColType::Int8, true)]))
        .unwrap();
    let definitions = first.table_definition_images();
    crate::mem::guard::forbid_alloc(|| {
        let image = definitions.definition(&first, table, 0).unwrap();
        assert_eq!(
            definitions
                .definition(&second, other, 0)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert_eq!(image.name.as_str(), "first");
    });
}

#[test]
fn table_definition_images_keep_their_pool_alive_after_storage_drops() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("retained", &[("id", ColType::Int8, true)]))
        .unwrap();
    let definitions = storage.table_definition_images();
    crate::mem::guard::forbid_alloc(|| {
        let image = definitions.definition(&storage, table, 0).unwrap();
        drop(storage);
        assert_eq!(image.name.as_str(), "retained");
        assert_eq!(image.columns()[0].name.as_str(), "id");
    });
}

#[test]
fn table_definition_images_exhaustion_preserves_readers_and_releases_capacity() {
    let mut config = test_config();
    config.definition_image_slots = 4;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("bounded", &[("id", ColType::Int8, true)]))
        .unwrap();
    let capacity = config.definition_image_slots;
    let mut owners = Vec::with_capacity(capacity);
    crate::mem::guard::forbid_alloc(|| {
        let retained_owner = storage.table_definition_images();
        let retained = retained_owner.definition(&storage, table, 0).unwrap();
        for _ in 1..capacity {
            let owner = storage.table_definition_images();
            owner.definition(&storage, table, 0).unwrap();
            owners.push(owner);
        }
        let exhausted = storage.table_definition_images();
        let Err(error) = exhausted.definition(&storage, table, 0) else {
            panic!("all definition images are held by readers");
        };
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(retained.name.as_str(), "bounded");
        drop(owners.pop().unwrap());
        exhausted.definition(&storage, table, 0).unwrap();
        assert_eq!(retained.name.as_str(), "bounded");
        drop(exhausted);
        owners.clear();
        let recovered = storage.table_definition_images();
        recovered.definition(&storage, table, 0).unwrap();
    });
}

#[test]
fn table_definition_images_concurrent_owners_reuse_cells_without_allocation() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("shared", &[("id", ColType::Int8, true)]))
        .unwrap();
    let retained = storage.table_definition_images();
    let image = retained.definition(&storage, table, 0).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let storage = &storage;
            scope.spawn(move || {
                crate::mem::guard::forbid_alloc(|| {
                    for _ in 0..256 {
                        let definitions = storage.table_definition_images();
                        let current = definitions.definition(storage, table, 0).unwrap();
                        assert_eq!(current.name.as_str(), "shared");
                        assert!(core::ptr::eq(
                            current,
                            definitions.definition(storage, table, 0).unwrap()
                        ));
                    }
                });
            });
        }
    });
    assert_eq!(image.name.as_str(), "shared");
}

#[test]
fn stored_query_dependency_arrays_live_outside_catalog_definitions() {
    // Exhaustive destructuring is the compile-time guard. Catalog entries
    // retain only the rule/backing-table indirection; the dependency array
    // lives in that separately sized object rather than every entry.
    let view_shape = |view: &ViewDef| {
        let ViewDef {
            database: _,
            created_at: _,
            schema: _,
            name: _,
            persistence: _,
            return_rule: _,
            options: _,
            columns: _,
            default_inputs: _,
            ownership: _,
            version: _,
            ddl_version: _,
            pending_definition: _,
            ddl_state: _,
        } = view;
    };
    let matview_shape = |view: &MatviewDef| {
        let MatviewDef {
            database: _,
            created_at: _,
            backing_table: _,
            backing_created_at: _,
            ddl_version: _,
            sql: _,
            creation_path: _,
            ownership: _,
            populated: _,
            population_version: _,
            pending_population: _,
            query_version: _,
            pending_query: _,
            ddl_state: _,
        } = view;
    };
    let _ = (view_shape, matview_shape);
    assert!(
        size_of::<StoredQueryDependencies>()
            < MAX_STORED_QUERY_DEPENDENCIES * size_of::<StoredQueryDependency>(),
        "the durable maximum must not inflate every executor frame"
    );
}

#[test]
fn catalog_object_pools_follow_their_independent_startup_capacities() {
    let mut config = test_config();
    config.query_workspace_slots = 2;
    config.max_tables = 2;
    config.max_databases = 6;
    config.max_schemas = 17;
    config.max_sequences = 18;
    config.max_domains = 26;
    config.max_enums = 28;
    config.max_enum_labels_per_type = 96;
    config.max_composites = 29;
    config.max_roles = 19;
    config.max_role_memberships = 20;
    config.max_role_settings = 21;
    config.max_acl_entries = 22;
    config.max_column_acl_entries = 23;
    config.max_default_acl_entries = 24;
    config.max_parameter_acl_entries = 25;
    config.max_indexes = 6;
    config.max_extended_statistics = 31;
    config.max_brin_unsummarized_ranges_per_index = 70;
    config.max_views = 3;
    config.max_materialized_views = 4;
    config.max_routines = 5;
    config.max_rules = 7;
    config.max_policies = 8;
    config.max_stored_query_dependencies_per_object = 73;
    config.max_catalog_versions_per_object = 3;
    config.max_row_versions_per_row = 11;
    config.max_spill_generations_per_table = 12;
    config.max_casts = 6;
    config.max_operators = 7;
    config.max_operator_families = 8;
    config.max_operator_classes = 9;
    config.max_triggers = 10;
    config.max_publications = 11;
    config.max_collations = 12;
    config.max_conversions = 13;
    config.max_text_search_objects = 27;
    config.max_event_triggers = 14;
    config.max_tablespaces = 15;
    config.max_comments = 16;
    config.max_foreign_sessions = 3;
    let expected_budget = config.memtable_bytes + Storage::extra_budget_bytes(&config);
    let mut budget = Budget::new(expected_budget);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage.configure_collation(&config, &mut budget).unwrap();

    assert_eq!(storage.tables.len(), 3); // two relations plus large-object pages
    assert_eq!(
        storage.row_versions.read().pending_row_versions.capacity(),
        pending_row_version_capacity(&config)
    );
    assert_eq!(
        storage
            .row_versions
            .read()
            .committed_row_versions
            .capacity(),
        committed_row_version_capacity(&config)
    );
    assert!(
        storage
            .tables
            .iter()
            .all(|table| table.spill_generations().capacity() == 12)
    );
    assert_eq!(
        storage.pending_table_defs.capacity(),
        pending_table_definition_capacity(&config)
    );
    assert_eq!(
        storage.pending_table_statistics.capacity(),
        pending_table_statistics_capacity(&config)
    );
    assert_eq!(storage.database_count(), 6);
    assert_eq!(storage.cumulative_statistics().databases.len(), 6);
    assert_eq!(storage.schema_count(), 17);
    let sequence_catalog = storage.sequence_catalog();
    assert_eq!(sequence_catalog.definitions.len(), 18);
    assert_eq!(sequence_catalog.values.capacity(), 18);
    drop(sequence_catalog);
    let type_catalog = storage
        .type_catalog
        .lock()
        .expect("type catalog lock poisoned");
    assert_eq!(type_catalog.domains.len(), 26);
    assert_eq!(
        storage
            .domain_graph_scratch
            .lock()
            .expect("domain graph scratch lock poisoned")
            .len(),
        26
    );
    assert_eq!(type_catalog.enums.len(), 28);
    assert_eq!(storage.enum_members_per_image, 96);
    assert_eq!(
        type_catalog.pending_enum_members.len(),
        pending_enum_definition_capacity(&config)
    );
    assert_eq!(
        storage
            .enum_member_images
            .lock()
            .expect("enum member-image lock poisoned")
            .len(),
        enum_member_image_capacity(&config) * 96
    );
    assert_eq!(type_catalog.composites.len(), 29);
    drop(type_catalog);
    let role_catalog = storage
        .role_catalog
        .lock()
        .expect("role catalog lock poisoned");
    assert_eq!(role_catalog.roles.len(), 19);
    assert_eq!(role_catalog.memberships.len(), 20);
    assert_eq!(role_catalog.settings.len(), 21);
    drop(role_catalog);
    assert_eq!(storage.role_graph_scratch.len(), 2);
    assert!(storage.role_graph_scratch.iter().all(|scratch| {
        scratch
            .lock()
            .expect("role graph scratch lock poisoned")
            .len()
            == 19
    }));
    storage.role_graph_scratch[0]
        .lock()
        .expect("role graph scratch lock poisoned")[3] = true;
    assert!(
        !storage.role_graph_scratch[1]
            .lock()
            .expect("role graph scratch lock poisoned")[3]
    );
    assert_eq!(storage.foreign_statement_contexts.len(), 2);
    assert!(storage.foreign_statement_contexts.iter().all(|context| {
        context
            .lock()
            .expect("foreign statement context lock poisoned")
            .savepoints
            .capacity()
            == config.max_savepoints_per_transaction
    }));
    {
        let mut context = storage.foreign_statement_contexts[0]
            .lock()
            .expect("foreign statement context lock poisoned");
        context.transaction_id = 41;
        context.serializable = true;
        context
            .savepoints
            .push(crate::util::StackStr::from_str("one"))
            .unwrap();
    }
    let other_context = storage.foreign_statement_contexts[1]
        .lock()
        .expect("foreign statement context lock poisoned");
    assert_eq!(other_context.transaction_id, 0);
    assert!(!other_context.serializable);
    assert!(other_context.savepoints.is_empty());
    assert_eq!(
        storage
            .temporary_transactions
            .lock()
            .expect("temporary transaction lock poisoned")
            .transactions
            .capacity(),
        config.max_connections as usize + config.max_prepared_transactions
    );
    let catalog = storage
        .acl_catalog
        .lock()
        .expect("ACL catalog lock poisoned");
    assert_eq!(catalog.objects.capacity(), 22);
    assert_eq!(catalog.columns.capacity(), 23);
    assert_eq!(catalog.defaults.capacity(), 24);
    assert_eq!(catalog.parameters.capacity(), 25);
    assert_eq!(storage.index_count(), 6);
    assert_eq!(storage.extended_statistics_count(), 31);
    let brin_maintenance = storage.brin_maintenance();
    assert_eq!(brin_maintenance.states.len(), 6);
    assert_eq!(brin_maintenance.unsummarized_ranges.len(), 6 * 70);
    drop(brin_maintenance);
    assert_eq!(storage.view_count(), 3);
    assert_eq!(storage.matview_count(), 4);
    assert_eq!(storage.routine_count(), 5);
    let committed_dependency_images = 7 + 8 + 5 + 4;
    let dependency_catalog = storage
        .stored_query_dependencies
        .lock()
        .expect("stored-query dependency catalog lock poisoned");
    assert_eq!(
        dependency_catalog.pending.len(),
        pending_stored_query_dependency_capacity(&config)
    );
    assert_eq!(dependency_catalog.counts.len(), committed_dependency_images);
    assert_eq!(
        dependency_catalog.entries.len(),
        (committed_dependency_images + pending_stored_query_dependency_capacity(&config)) * 73
    );
    drop(dependency_catalog);
    assert_eq!(storage.policy_roles_per_image, 20);
    let policy_catalog = storage
        .policy_catalog
        .lock()
        .expect("policy catalog lock poisoned");
    assert_eq!(
        policy_catalog.roles.len(),
        policy_role_image_capacity(&config) * 20
    );
    drop(policy_catalog);
    assert_eq!(
        storage.policy_role_replay_image,
        policy_role_image_capacity(&config) - 1
    );
    assert_eq!(
        storage
            .casts
            .lock()
            .expect("cast catalog lock poisoned")
            .len(),
        6
    );
    let operator_catalog = storage
        .operator_catalog
        .lock()
        .expect("operator catalog lock poisoned");
    assert_eq!(operator_catalog.access_methods.len(), MAX_ACCESS_METHODS);
    assert_eq!(operator_catalog.operators.len(), 7);
    assert_eq!(operator_catalog.families.len(), 8);
    assert_eq!(operator_catalog.classes.len(), 9);
    drop(operator_catalog);
    let trigger_catalog = storage
        .trigger_catalog
        .lock()
        .expect("trigger catalog lock poisoned");
    assert_eq!(trigger_catalog.definitions.len(), 10);
    assert_eq!(trigger_catalog.partition_states.len(), 20);
    drop(trigger_catalog);
    assert_eq!(storage.publication_count(), 11);
    let text_catalog = storage
        .text_catalog
        .lock()
        .expect("text catalog lock poisoned");
    assert_eq!(text_catalog.collations.len(), 12);
    assert_eq!(text_catalog.conversions.len(), 13);
    assert_eq!(text_catalog.text_search_objects.len(), 27);
    drop(text_catalog);
    assert_eq!(storage.event_trigger_capacity(), 14);
    assert_eq!(
        storage
            .extended_statistics_catalog
            .lock()
            .expect("extended-statistics catalog lock poisoned")
            .pending_data
            .capacity(),
        pending_extended_statistics_capacity(&config)
    );
    assert_eq!(storage.tablespace_capacity(), 15);
    assert_eq!(storage.comment_capacity(), 16);
    assert_eq!(storage.foreign_sessions.slots.capacity(), 3);
    assert!(storage.foreign_sessions.slots.is_empty());
    assert_eq!(storage.cumulative_statistics().functions.capacity(), 5);
    assert_eq!(
        storage
            .cumulative_statistics()
            .function_transactions
            .capacity(),
        (config.max_connections as usize + config.max_prepared_transactions) * 5
    );
    assert_eq!(budget.used(), expected_budget);
}

#[test]
fn transaction_identity_state_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TransactionIdentityState>();

    let config = test_config();
    let transaction_capacity = config.max_connections as usize + config.max_prepared_transactions;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let identities = &storage.transaction_identities;
    std::thread::scope(|scope| {
        for worker in 0..4u64 {
            scope.spawn(move || {
                for offset in 0..8u64 {
                    let transaction_id = 100 + worker * 8 + offset;
                    let mut identities = identities
                        .lock()
                        .expect("transaction identity lock poisoned");
                    identities.latest = identities.latest.max(transaction_id);
                    Storage::record_transaction_status(
                        &mut identities,
                        transaction_id,
                        worker % 2 == 0,
                    );
                }
            });
        }
    });
    let identities = storage.transaction_identity_state();
    assert_eq!(identities.latest, 131);
    assert_eq!(identities.active.capacity(), transaction_capacity);
    assert_eq!(
        identities.recent.capacity(),
        transaction_capacity.saturating_mul(8).max(64)
    );
    assert_eq!(identities.recent.len(), 32);
    assert!(
        identities
            .recent
            .iter()
            .any(|status| status.transaction_id == 100 && status.committed)
    );
    assert!(
        identities
            .recent
            .iter()
            .any(|status| status.transaction_id == 131 && !status.committed)
    );
}

#[test]
fn shared_transaction_identity_capacity_failure_is_atomic() {
    let mut config = test_config();
    config.max_connections = 1;
    config.max_prepared_transactions = 1;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let first = storage.start_transaction_identity().unwrap();
        storage.assign_transaction_identity(first);
        storage.prepare_transaction_locks(first);
        let second = storage.start_transaction_identity().unwrap();
        let latest = storage.latest_transaction_identity();
        let error = storage.start_transaction_identity().unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            error.message.as_str(),
            "transaction identity registry exhausted"
        );
        assert_eq!(storage.latest_transaction_identity(), latest);
        assert_eq!(
            storage.assigned_transaction_identity(first),
            Some(u64::from(first))
        );
        storage.begin_transaction_identity(first).unwrap();
        assert!(storage.begin_transaction_identity(100).is_err());
        assert!(storage.begin_transaction_identity(0).is_err());
        assert_eq!(storage.latest_transaction_identity(), latest);
        let maintenance = storage.reserve_transaction_identity().unwrap();
        assert_eq!(maintenance, second + 1);
        storage.finish_transaction_identity(second, false);
        let replacement = storage.start_transaction_identity().unwrap();
        assert_eq!(replacement, maintenance + 1);
        storage.finish_transaction_identity(replacement, false);
        storage.finish_transaction_identity(first, true);
        assert_eq!(
            storage.transaction_status(u64::from(first)).unwrap(),
            Some("committed")
        );
        assert!(storage.transaction_identity_state().active.is_empty());
        storage.observe_transaction_identity(u64::from(u32::MAX - 1));
        assert_eq!(storage.reserve_transaction_identity().unwrap(), u32::MAX);
        assert!(storage.start_transaction_identity().is_err());
        assert!(storage.reserve_transaction_identity().is_err());
        assert!(storage.transaction_identity_state().active.is_empty());
        assert_eq!(storage.latest_transaction_identity(), u64::from(u32::MAX));
    });
}

#[test]
fn shared_transaction_identity_sql_and_maintenance_reservations_are_unique() {
    let mut config = test_config();
    config.max_connections = 2;
    config.max_prepared_transactions = 0;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(3);
        for worker in 0..3 {
            let storage = &storage;
            workers.push(scope.spawn(move || {
                crate::sql::set_execution_connection_id(worker + 1);
                crate::mem::guard::forbid_alloc(|| {
                    std::array::from_fn::<_, 64, _>(|_| {
                        if worker == 2 {
                            return storage.reserve_transaction_identity().unwrap();
                        }
                        let transaction = storage.start_transaction_identity().unwrap();
                        storage.assign_transaction_identity(transaction);
                        {
                            let identities = storage.transaction_identity_state();
                            let active = identities
                                .active
                                .iter()
                                .find(|entry| entry.transaction_id == transaction)
                                .unwrap();
                            assert_eq!(active.connection_id, Some(worker + 1));
                            assert!(active.assigned);
                        }
                        storage.finish_transaction_identity(transaction, worker == 0);
                        transaction
                    })
                })
            }));
        }
        let mut ids = [0; 192];
        for (worker, handle) in workers.into_iter().enumerate() {
            ids[worker * 64..(worker + 1) * 64].copy_from_slice(&handle.join().unwrap());
        }
        crate::mem::guard::forbid_alloc(|| {
            ids.sort_unstable();
            assert_eq!(ids, std::array::from_fn(|index| index as u32 + 1));
            assert!(storage.transaction_identity_state().active.is_empty());
            assert_eq!(storage.latest_transaction_identity(), 192);
        });
    });
}

#[test]
fn relation_publication_recovered_identity_never_belongs_to_a_connection() {
    let mut config = test_config();
    config.max_connections = 1;
    config.max_prepared_transactions = 1;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let previous = crate::sql::execution_connection_id();
    crate::sql::set_execution_connection_id(42);
    crate::mem::guard::forbid_alloc(|| {
        storage.begin_prepared_transaction_identity(7).unwrap();
        storage
            .lock_object(
                7,
                ObjectLockTarget::View(0),
                crate::sql::ast::TableLockMode::AccessShare,
                false,
            )
            .unwrap();
        storage.visit_object_locks(|_, owner, _, _| {
            assert_eq!(crate::sql::lock::wait_owner_pid(owner), None)
        });
        let ordinary = storage.start_transaction_identity().unwrap();
        assert_eq!(ordinary, 8);
        assert_eq!(
            storage
                .begin_prepared_transaction_identity(ordinary)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        assert_eq!(
            storage.transaction_wait_owner(ordinary),
            crate::sql::lock::connection_wait_owner(42)
        );
        assert_eq!(
            storage
                .begin_prepared_transaction_identity(9)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.latest_transaction_identity(), 8);
        assert_eq!(
            storage
                .begin_prepared_transaction_identity(0)
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        storage.release_object_locks(7);
        storage.finish_transaction_identity(7, false);
        storage.finish_transaction_identity(ordinary, false);
    });
    crate::sql::set_execution_connection_id(previous);
}

#[test]
fn relation_publication_rule_history_rejects_foreign_owners_and_restores_savepoints() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage.create_table(make_def("rule_history", &[])).unwrap();
    let target = RuleTarget::Table(table as u16);
    let first = RuleDefinition {
        name: SqlName::parse("first").unwrap(),
        target,
        ..RuleDefinition::EMPTY
    };
    let second = RuleDefinition {
        name: SqlName::parse("second").unwrap(),
        target,
        ..RuleDefinition::EMPTY
    };
    crate::mem::guard::forbid_alloc(|| {
        let publication_a = storage
            .create_rule(first, StoredQueryDependencies::EMPTY.view(), false, 7)
            .unwrap();
        let a = publication_a.slot();
        let prior_a = storage.begin_rule_history(target, 7).unwrap();
        assert_eq!(prior_a, None);
        assert!(storage.table_has_rules(table, 7));
        assert!(!storage.table_has_rules(table, 8));
        let publication_rejected = storage
            .create_rule(second, StoredQueryDependencies::EMPTY.view(), false, 8)
            .unwrap();
        let rejected = publication_rejected.slot();
        assert_eq!(
            storage.begin_rule_history(target, 8).unwrap_err().sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        storage.discard_rule_create(publication_rejected);
        assert_eq!(storage.rule(rejected).ddl_state, CatalogDdlState::Absent);
        assert!(storage.table_has_rules(table, 7));
        assert!(!storage.table_has_rules(table, 8));
        assert_eq!(storage.tables[table].ddl_locked_by_other(8), Some(7));
        storage.release_row_locks(8);
        let publication_b = storage
            .create_rule(second, StoredQueryDependencies::EMPTY.view(), false, 7)
            .unwrap();
        let b = publication_b.slot();
        assert_ne!(a, b);
        let prior_b = storage.begin_rule_history(target, 7).unwrap();
        assert_eq!(prior_b, Some(7));
        storage.finish_rule_publication(publication_b, prior_b, CatalogDdlDecision::Rollback);
        assert!(storage.table_has_rules(table, 7));
        storage.finish_rule_publication(publication_a, prior_a, CatalogDdlDecision::Rollback);
        assert!(!storage.table_has_rules(table, 7));
        assert_eq!(storage.tables[table].ddl_locked_by_other(8), None);
        let publication_reused = storage
            .create_rule(second, StoredQueryDependencies::EMPTY.view(), false, 8)
            .unwrap();
        let reused = publication_reused.slot();
        assert_eq!(reused, a);
        assert_eq!(storage.begin_rule_history(target, 8).unwrap(), None);
        storage.finish_rule_publication(publication_reused, None, CatalogDdlDecision::Commit);
        assert!(storage.table_has_rules(table, 0));
        assert_eq!(
            storage.tables[table]
                .definition
                .read()
                .pending_has_rules_txid,
            None
        );
    });
}

#[test]
fn relation_publication_retained_definition_does_not_block_another_table() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let first = storage
        .create_table_in(make_def("selected", &[]), 7)
        .unwrap();
    let second = storage
        .create_table_in(make_def("published", &[]), 8)
        .unwrap();
    let selected = storage.table(first).definition();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::scope(|scope| {
        let shared = &storage;
        scope.spawn(move || {
            crate::mem::guard::forbid_alloc(|| {
                assert_eq!(
                    shared
                        .begin_rule_history(RuleTarget::Table(second as u16), 8)
                        .unwrap(),
                    None
                );
                shared.commit_create(second);
                assert!(shared.table_has_rules(second, 8));
                assert!(!shared.table_has_rules(second, 7));
            });
            sender.send(()).unwrap();
        });
        let progress = receiver.recv_timeout(crate::test_wait::HANG);
        assert_eq!(
            selected.identity().existence,
            CatalogDdlState::PendingCreate { txid: 7 }
        );
        drop(selected);
        progress.expect("unrelated table publication must finish while a definition is selected");
    });
    assert_eq!(storage.table(second).existence(), CatalogDdlState::Present);
}

#[test]
fn relation_publication_table_and_view_slots_are_distinct_and_bounded() {
    use crate::sql::ast::TableLockMode;
    let mut config = test_config();
    config.max_connections = 2;
    config.max_prepared_transactions = 0;
    config.max_views = 2;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .lock_table(7, 0, TableLockMode::AccessExclusive, false)
            .unwrap();
        storage
            .lock_object(
                8,
                ObjectLockTarget::View(0),
                TableLockMode::AccessShare,
                false,
            )
            .unwrap();
        assert_eq!(
            storage
                .lock_table(9, 0, TableLockMode::AccessShare, true)
                .unwrap_err()
                .sqlstate,
            sqlstate::LOCK_NOT_AVAILABLE
        );
        assert_eq!(
            storage
                .lock_object(
                    9,
                    ObjectLockTarget::View(0),
                    TableLockMode::AccessExclusive,
                    true
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::LOCK_NOT_AVAILABLE
        );
        let mut visited = 0;
        storage.visit_object_locks(|_, _, _, _| {
            assert!(storage.locks.try_lock().is_ok());
            visited += 1;
        });
        assert_eq!(visited, 2);
        storage.release_object_locks(7);
        storage.release_object_locks(8);
        let capacity = storage.lock_state().table.capacity();
        for owner in 1..=capacity as u32 {
            storage
                .lock_object(
                    owner,
                    ObjectLockTarget::View(0),
                    TableLockMode::AccessShare,
                    false,
                )
                .unwrap();
        }
        let error = storage
            .lock_table(capacity as u32 + 1, 0, TableLockMode::AccessShare, false)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("object-lock registry"));
        assert_eq!(storage.lock_state().table.len(), capacity);
        storage.release_object_locks(1);
        storage
            .lock_table(capacity as u32 + 1, 0, TableLockMode::AccessShare, false)
            .unwrap();
        assert_eq!(storage.lock_state().table.len(), capacity);
    });
}

#[test]
fn relation_publication_prepared_view_locks_round_trip_and_reject_corruption() {
    use crate::sql::ast::TableLockMode;
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage.create_table(make_def("lock_table", &[])).unwrap();
    let publication_view = storage
        .create_view(
            SqlName::parse("public").unwrap(),
            SqlName::parse("lock_view").unwrap(),
            test_view_definition(StoredQueryDependencies::EMPTY),
            false,
            7,
        )
        .unwrap();
    let view = publication_view.slot();
    storage.finish_view_publication(publication_view, crate::storage::CatalogDdlDecision::Commit);
    let mut encoded = FixedBuf::new(&mut budget, "prepared relation locks", 512).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .lock_table(7, table, TableLockMode::AccessShare, false)
            .unwrap();
        let before_view = storage.lock_mark();
        storage
            .lock_object(
                7,
                ObjectLockTarget::View(view),
                TableLockMode::AccessExclusive,
                false,
            )
            .unwrap();
        storage.encode_transaction_locks(7, &mut encoded).unwrap();
        let table_record_bytes = 12 + "public".len() + "lock_table".len();
        assert_eq!(encoded.readable()[0], 0);
        assert_eq!(encoded.readable()[table_record_bytes], 3);
        storage.rollback_locks_to(7, before_view);
        storage
            .lock_object(
                8,
                ObjectLockTarget::View(view),
                TableLockMode::AccessShare,
                false,
            )
            .unwrap();
        storage.release_object_locks(7);
        storage.release_object_locks(8);
        storage
            .restore_transaction_locks(9, encoded.readable())
            .unwrap();
        assert_eq!(
            storage
                .lock_table(10, table, TableLockMode::AccessExclusive, true)
                .unwrap_err()
                .sqlstate,
            sqlstate::LOCK_NOT_AVAILABLE
        );
        assert_eq!(
            storage
                .lock_object(
                    10,
                    ObjectLockTarget::View(view),
                    TableLockMode::AccessShare,
                    true
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::LOCK_NOT_AVAILABLE
        );
        storage.release_object_locks(9);
        for invalid in [254, 255] {
            encoded.filled_mut()[table_record_bytes] = invalid;
            assert_eq!(
                storage
                    .restore_transaction_locks(9, encoded.readable())
                    .unwrap_err()
                    .sqlstate,
                sqlstate::DATA_EXCEPTION
            );
            storage.release_object_locks(9);
        }
        encoded.filled_mut()[table_record_bytes] = 3;
        encoded.filled_mut()[table_record_bytes + 1] = 255;
        assert_eq!(
            storage
                .restore_transaction_locks(9, encoded.readable())
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        storage.release_object_locks(9);
        encoded.filled_mut()[table_record_bytes + 1] = TableLockMode::AccessExclusive as u8;
        let bytes = encoded.readable();
        assert_eq!(
            storage
                .restore_transaction_locks(9, &bytes[..bytes.len() - 1])
                .unwrap_err()
                .sqlstate,
            sqlstate::DATA_EXCEPTION
        );
        storage.release_object_locks(9);
        storage
            .restore_transaction_locks(9, encoded.readable())
            .unwrap();
        storage.release_object_locks(9);
    });
}

#[test]
fn lock_state_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<LockState>();

    let config = test_config();
    let transaction_capacity = config.max_connections as usize + config.max_prepared_transactions;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let locks = &storage.locks;
    std::thread::scope(|scope| {
        for worker in 0..4u32 {
            scope.spawn(move || {
                for offset in 0..1_000u32 {
                    let mut locks = locks.lock().expect("lock state lock poisoned");
                    let sequence = locks
                        .next_sequence()
                        .expect("the bounded fixture has acquisition identities");
                    if offset == 0 {
                        let owner = 100 + worker;
                        let mut modes = [0; 8];
                        modes[0] = sequence;
                        locks
                            .table
                            .push(ObjectLock {
                                owner: ObjectLockOwner::Transaction(owner),
                                wait_owner: crate::sql::lock::prepared_wait_owner(owner),
                                relation: ObjectLockTarget::Table(0),
                                modes,
                            })
                            .unwrap();
                    }
                }
            });
        }
    });
    let locks = storage.lock_state();
    assert_eq!(locks.sequence, 4_000);
    assert_eq!(locks.table.len(), 4);
    assert_eq!(
        locks.table.capacity(),
        transaction_capacity * (table_slot_capacity(&config) + config.max_views + config.max_roles)
            + config.max_connections as usize
    );
    assert!(locks.table.iter().all(|lock| lock.modes[0] != 0));
}

#[test]
fn backend_state_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BackendState>();

    let mut config = test_config();
    config.max_connections = 4;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let backends = &storage.backend_state;
    std::thread::scope(|scope| {
        for worker in 0..4i32 {
            scope.spawn(move || {
                let pid = 100 + worker;
                for offset in 0..1_000 {
                    let mut backends = backends.lock().expect("backend state lock poisoned");
                    if offset == 0 {
                        backends.signals.push(BackendSignal::Cancel(pid)).unwrap();
                    } else {
                        let signal = backends
                            .signals
                            .iter()
                            .position(|signal| {
                                matches!(
                                    signal,
                                    BackendSignal::Cancel(candidate)
                                        | BackendSignal::Terminate(candidate)
                                        if *candidate == pid
                                )
                            })
                            .unwrap();
                        backends.signals[signal] = if offset % 2 == 0 {
                            BackendSignal::Cancel(pid)
                        } else {
                            BackendSignal::Terminate(pid)
                        };
                    }
                }
            });
        }
    });
    let backends = storage.backend_state();
    assert_eq!(backends.activities.capacity(), 4);
    assert_eq!(backends.signals.capacity(), 4);
    assert_eq!(backends.signals.len(), 4);
    assert!(
        backends
            .signals
            .iter()
            .all(|signal| matches!(signal, BackendSignal::Terminate(_)))
    );
}

#[test]
fn transaction_workspaces_are_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ForeignStatementContext>();
    assert_send_sync::<TemporaryTransactionState>();

    let mut config = test_config();
    config.max_connections = 4;
    config.query_workspace_slots = 4;
    let transaction_capacity = config.max_connections as usize + config.max_prepared_transactions;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let temporary = &storage.temporary_transactions;
    let foreign = &storage.foreign_statement_contexts[0];
    std::thread::scope(|scope| {
        for worker in 0..4u32 {
            scope.spawn(move || {
                let transaction_id = 100 + worker;
                for iteration in 0..1_000 {
                    temporary
                        .lock()
                        .expect("temporary transaction lock poisoned")
                        .mark(transaction_id);
                    let mut context = foreign
                        .lock()
                        .expect("foreign statement context lock poisoned");
                    context.transaction_id = 73;
                    context.serializable = true;
                    if iteration == 0 {
                        let name = match worker {
                            0 => "one",
                            1 => "two",
                            2 => "three",
                            _ => "four",
                        };
                        context
                            .savepoints
                            .push(crate::util::StackStr::from_str(name))
                            .unwrap();
                    }
                }
            });
        }
    });
    let temporary = storage
        .temporary_transactions
        .lock()
        .expect("temporary transaction lock poisoned");
    assert_eq!(temporary.transactions.capacity(), transaction_capacity);
    assert_eq!(temporary.transactions.len(), 4);
    assert!((100..104).all(|transaction_id| temporary.contains(transaction_id)));
    drop(temporary);
    let context = storage.foreign_statement_contexts[0]
        .lock()
        .expect("foreign statement context lock poisoned");
    assert_eq!(context.transaction_id, 73);
    assert!(context.serializable);
    assert_eq!(
        context.savepoints.capacity(),
        config.max_savepoints_per_transaction
    );
    assert_eq!(context.savepoints.len(), 4);
    drop(context);
    let context = storage.foreign_statement_context_for(73);
    assert!(context.is_serializable());
    assert_eq!(context.savepoints().len(), 4);
    drop(context);
    let stale = storage.foreign_statement_context_for(74);
    assert!(!stale.is_serializable());
    assert!(stale.savepoints().is_empty());
    drop(stale);
    let temporary = &storage.temporary_transactions;
    std::thread::scope(|scope| {
        for transaction_id in 100..104 {
            scope.spawn(move || {
                temporary
                    .lock()
                    .expect("temporary transaction lock poisoned")
                    .clear(transaction_id);
            });
        }
    });
    assert!(
        storage
            .temporary_transactions
            .lock()
            .expect("temporary transaction lock poisoned")
            .transactions
            .is_empty()
    );
}

#[test]
fn cursor_read_statistics_survive_nested_savepoints_without_double_counting() {
    for release_inner in [false, true] {
        for release_outer in [false, true] {
            for commit in [false, true] {
                let config = test_config();
                let mut budget = test_budget(&config);
                let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
                let mut definition = make_def("cursor_statistics", &[("id", ColType::Int4, true)]);
                definition.columns[0].primary = true;
                let table = storage.create_table(definition).unwrap();
                let index = storage
                    .table_def(table, 0)
                    .column_index_identity(0)
                    .unwrap()
                    .oid();
                let database = storage.database_slot_by_oid(current_database(), 0).unwrap();
                crate::mem::guard::forbid_alloc(|| {
                    for level in [2, 3] {
                        storage.set_cumulative_transaction_nest_level(7, level);
                        storage
                            .record_relation_scan(7, table, None, 0, level as i64)
                            .unwrap();
                        storage.record_relation_tuple_read(7, table, None).unwrap();
                        storage
                            .record_relation_scan(
                                7,
                                table,
                                Some(&storage.capture_index_statistics(7, table, index).unwrap()),
                                3,
                                level as i64,
                            )
                            .unwrap();
                        storage
                            .record_relation_tuple_read(
                                7,
                                table,
                                Some(&storage.capture_index_statistics(7, table, index).unwrap()),
                            )
                            .unwrap();
                    }
                    storage.finish_cumulative_subtransactions(7, 3, release_inner);
                    storage.finish_cumulative_subtransactions(7, 2, release_outer);
                    // Finishing an already-finished savepoint must not add its reads again.
                    storage.finish_cumulative_subtransactions(7, 2, release_outer);
                    let transaction = storage.relation_transaction_statistics(7, table);
                    assert_eq!(transaction.seq_scan, 2);
                    assert_eq!(transaction.seq_tup_read, 2);
                    assert_eq!(transaction.idx_scan, 2);
                    assert_eq!(transaction.idx_tup_read, 6);
                    assert_eq!(transaction.idx_tup_fetch, 2);
                    assert_eq!(transaction.last_seq_scan, Some(3));
                    assert_eq!(transaction.last_idx_scan, Some(3));
                    assert_eq!(storage.relation_cumulative_statistics(table).seq_scan, 0);
                    storage.finish_cumulative_transaction(7, commit);
                    assert_eq!(
                        storage.relation_transaction_statistics(7, table).seq_scan,
                        0
                    );
                    let total = storage.relation_cumulative_statistics(table);
                    assert_eq!(total.seq_scan, 2);
                    assert_eq!(total.seq_tup_read, 2);
                    assert_eq!(total.idx_scan, 2);
                    assert_eq!(total.idx_tup_fetch, 2);
                    assert_eq!(total.last_seq_scan, Some(3));
                    assert_eq!(total.last_idx_scan, Some(3));
                    let cumulative = storage.cumulative_statistics();
                    assert_eq!(cumulative.databases[database].tup_returned, 8);
                    assert_eq!(cumulative.databases[database].tup_fetched, 2);
                });
            }
        }
    }
}

#[test]
fn cursor_read_statistics_never_follow_a_reused_table_slot() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let old = storage
        .create_table(make_def("old_statistics", &[("id", ColType::Int4, true)]))
        .unwrap();
    storage.set_cumulative_transaction_nest_level(7, 3);
    storage.record_relation_scan(7, old, None, 0, 1).unwrap();
    storage.record_relation_tuple_read(7, old, None).unwrap();
    storage.record_relation_write(7, old, false, true).unwrap();
    storage.rollback_create(old);
    let new = storage
        .create_table(make_def("new_statistics", &[("id", ColType::Int4, true)]))
        .unwrap();
    assert_eq!(old, new, "the regression must exercise slot reuse");
    assert_eq!(storage.relation_transaction_statistics(7, new).seq_scan, 0);
    storage.record_relation_scan(7, new, None, 0, 2).unwrap();
    storage.record_relation_tuple_read(7, new, None).unwrap();
    storage.finish_cumulative_subtransactions(7, 3, false);
    storage.finish_cumulative_subtransactions(7, 2, true);
    let transaction = storage.relation_transaction_statistics(7, new);
    assert_eq!(transaction.seq_scan, 1);
    assert_eq!(transaction.seq_tup_read, 1);
    storage.finish_cumulative_transaction(7, true);
    let total = storage.relation_cumulative_statistics(new);
    assert_eq!(total.seq_scan, 1);
    assert_eq!(total.seq_tup_read, 1);
    assert_eq!(total.n_tup_ins, 0);
    assert_eq!(total.n_dead_tup, 0);
    let database = storage.database_slot_by_oid(current_database(), 0).unwrap();
    let cumulative = storage.cumulative_statistics();
    assert_eq!(cumulative.databases[database].tup_returned, 2);
    assert_eq!(cumulative.databases[database].tup_inserted, 1);
}

#[test]
fn relation_identity_statistics_follow_savepoint_and_index_lifetimes() {
    let mut config = test_config();
    config.index_statistics_slots = 3;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut definition = make_def("identity_statistics", &[("id", ColType::Int4, true)]);
    definition.columns[0].primary = true;
    let table = storage.create_table(definition).unwrap();
    let database = storage.table(table).database();
    let original = storage
        .table_def(table, 0)
        .column_index_identity(0)
        .unwrap()
        .oid();
    let mut mapping = [None; MAX_COLUMNS];
    mapping[0] = Some(SqlName::parse("id").unwrap());
    crate::mem::guard::forbid_alloc(|| {
        let capture = storage
            .capture_index_statistics(1, table, original)
            .unwrap();
        storage
            .record_relation_scan(1, table, Some(&capture), 3, 1)
            .unwrap();
        storage
            .record_relation_tuple_read(1, table, Some(&capture))
            .unwrap();
        storage.reset_index_statistics(database, original);
        storage
            .record_relation_scan(1, table, Some(&capture), 3, 2)
            .unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, original)
                .idx_scan,
            1
        );
        let (target, selected) = storage.select_table_definition(table, 1);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].primary = false;
        let change = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        storage.rollback_table_def(change);
        assert_eq!(
            storage
                .index_cumulative_statistics(database, original)
                .idx_scan,
            1
        );
        let (target, selected) = storage.select_table_definition(table, 2);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].primary = false;
        let change = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        crate::storage::tests::publish_table_definition(&storage, change).unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, original)
                .idx_scan,
            0
        );
        // A reader which selected the retired source keeps its own counter.
        storage
            .record_relation_scan(1, table, Some(&capture), 3, 3)
            .unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, original)
                .idx_scan,
            0
        );
        drop(capture);
        // Committing a later version without the private key retires its hidden predecessor.
        let (target, selected) = storage.select_table_definition(table, 7);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].unique = true;
        storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let transient = storage
            .table_def(table, 7)
            .column_index_identity(0)
            .unwrap()
            .oid();
        let transient_reader = storage
            .capture_index_statistics(7, table, transient)
            .unwrap();
        storage
            .record_relation_scan(7, table, Some(&transient_reader), 1, 3)
            .unwrap();
        let (target, selected) = storage.select_table_definition(table, 7);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].unique = false;
        let change = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        crate::storage::tests::publish_table_definition(&storage, change).unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, transient)
                .idx_scan,
            0
        );
        drop(transient_reader);
        let (target, selected) = storage.select_table_definition(table, 3);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].unique = true;
        let first = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let saved = storage
            .table_def(table, 3)
            .column_index_identity(0)
            .unwrap()
            .oid();
        assert_ne!(saved, original);
        let capture = storage.capture_index_statistics(3, table, saved).unwrap();
        storage
            .record_relation_scan(3, table, Some(&capture), 1, 4)
            .unwrap();
        let (target, selected) = storage.select_table_definition(table, 3);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].unique = false;
        let second = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let (target, selected) = storage.select_table_definition(table, 3);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.columns[0].unique = true;
        let third = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let discarded = storage
            .table_def(table, 3)
            .column_index_identity(0)
            .unwrap()
            .oid();
        let discarded_capture = storage
            .capture_index_statistics(3, table, discarded)
            .unwrap();
        storage
            .record_relation_scan(3, table, Some(&discarded_capture), 1, 5)
            .unwrap();
        storage.rollback_table_def(third);
        assert_eq!(
            storage
                .index_cumulative_statistics(database, discarded)
                .idx_scan,
            0
        );
        assert_eq!(
            storage
                .index_cumulative_statistics(database, saved)
                .idx_scan,
            1
        );
        storage.rollback_table_def(second);
        assert_eq!(
            storage
                .index_cumulative_statistics(database, saved)
                .idx_scan,
            1
        );
        storage.rollback_table_def(first);
        assert_eq!(
            storage
                .index_cumulative_statistics(database, saved)
                .idx_scan,
            0
        );
        drop(discarded_capture);
        drop(capture);
        let mut explicit = test_index_definition("statistics_explicit");
        explicit.table = SqlName::parse("identity_statistics").unwrap();
        let slot = storage.create_index(explicit, 6).unwrap();
        let oid = crate::sql::catalog::explicit_index_oid(&storage.index(slot));
        let capture = storage.capture_index_statistics(6, table, oid).unwrap();
        storage
            .record_relation_scan(6, table, Some(&capture), 1, 6)
            .unwrap();
        assert_eq!(
            storage.index_cumulative_statistics(database, oid).idx_scan,
            1
        );
        storage.rollback_index_create(slot);
        assert_eq!(
            storage.index_cumulative_statistics(database, oid).idx_scan,
            0
        );
    });
}

#[test]
fn index_statistics_admission_exhaustion_preserves_catalogs_and_active_readers() {
    let mut config = test_config();
    config.index_statistics_slots = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut definition = make_def(
        "counter_slots",
        &[("id", ColType::Int4, true), ("extra", ColType::Int4, false)],
    );
    definition.columns[0].primary = true;
    let table = storage.create_table(definition).unwrap();
    let database = storage.table(table).database();
    let original = storage
        .table_def(table, 0)
        .column_index_identity(0)
        .unwrap()
        .oid();
    let mut mapping = [None; MAX_COLUMNS];
    mapping[0] = Some(SqlName::parse("id").unwrap());
    mapping[1] = Some(SqlName::parse("extra").unwrap());
    crate::mem::guard::forbid_alloc(|| {
        // One new key fits; the complete two-key cohort does not. Neither is installed.
        let (target, selected) = storage.select_table_definition(table, 1);
        let mut next = TableDefBuilder::from_definition(&selected);
        next.take_column_index_identity(0);
        next.columns[0].primary = false;
        next.columns[0].unique = true;
        next.columns[1].unique = true;
        let error = storage
            .write_table_def(
                target,
                next,
                &mapping,
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("index_statistics_slots=2"));
        assert!(storage.table_def(table, 1).columns()[0].primary);
        assert!(!storage.table_def(table, 1).columns()[1].unique);
        let mut index = test_index_definition("counter_extra");
        index.table = SqlName::parse("counter_slots").unwrap();
        let explicit = storage.create_index(index, 2).unwrap();
        let explicit_oid = crate::sql::catalog::explicit_index_oid(&storage.index(explicit));
        let reader = storage
            .capture_index_statistics(2, table, explicit_oid)
            .unwrap();
        let mut overflow = index;
        overflow.name = SqlName::parse("counter_overflow").unwrap();
        let error = storage.create_index(overflow, 2).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            !storage
                .indexes_for("public", "counter_slots", 2)
                .any(|definition| definition.name == overflow.name)
        );
        let mut table_definition =
            make_def("counter_table_overflow", &[("id", ColType::Int4, true)]);
        table_definition.columns[0].primary = true;
        let error = table_creation::allocate(
            &storage,
            table_definition,
            CatalogAllocation::New,
            table_creation::TableCreation::Committed,
        )
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            storage
                .find_table("public", "counter_table_overflow")
                .is_none()
        );
        // Reset does not discard the admission needed by an already selected reader.
        storage.reset_index_statistics(database, explicit_oid);
        storage
            .record_relation_scan(2, table, Some(&reader), 4, 1)
            .unwrap();
        storage
            .record_relation_tuple_read(2, table, Some(&reader))
            .unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, explicit_oid)
                .idx_scan,
            1
        );
        storage.reset_current_database_statistics();
        storage
            .record_relation_scan(2, table, Some(&reader), 4, 2)
            .unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, explicit_oid)
                .idx_scan,
            1
        );
        storage.rollback_index_create(explicit);
        assert_eq!(
            storage
                .index_cumulative_statistics(database, explicit_oid)
                .idx_scan,
            0
        );
        // Catalog removal alone cannot recycle a counter retained by a late reader.
        assert_eq!(
            storage.create_index(overflow, 2).unwrap_err().sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        storage
            .record_relation_scan(2, table, Some(&reader), 4, 2)
            .unwrap();
        drop(reader);
        let replacement = storage.create_index(overflow, 2).unwrap();
        let replacement_oid = crate::sql::catalog::explicit_index_oid(&storage.index(replacement));
        assert_ne!(replacement_oid, explicit_oid);
        assert_eq!(
            storage
                .index_cumulative_statistics(database, replacement_oid)
                .idx_scan,
            0
        );
        storage.rollback_index_create(replacement);
        let reader = storage
            .capture_index_statistics(3, table, original)
            .unwrap();
        storage
            .record_relation_scan(3, table, Some(&reader), 1, 3)
            .unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(database, original)
                .idx_scan,
            1
        );
    });
}

#[test]
fn index_statistics_source_pins_survive_table_slot_reuse_without_cross_database_reset() {
    let mut config = test_config();
    config.index_statistics_slots = 3;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut definition = make_def("old_counter_source", &[("id", ColType::Int4, true)]);
    definition.columns[0].primary = true;
    let old = storage.create_table(definition).unwrap();
    let old_identity = storage.table(old).identity();
    let old_index_identity = storage.table_def(old, 0).column_index_identity(0).unwrap();
    let old_oid = old_index_identity.oid();
    let other_database = DatabaseOid::parse(USER_DATABASE_OID_BASE + 1).unwrap();
    // Database clones preserve births and OIDs but own independent counter slots.
    storage
        .reserve_explicit_index_statistics(
            old,
            other_database,
            old_identity.created_at,
            old_index_identity.created_at(),
            IndexRelationKind::Table,
        )
        .unwrap()
        .commit();
    {
        let mut counters = storage.cumulative_statistics();
        let other = counters
            .indexes
            .iter_mut()
            .find(|entry| entry.statistics.database == other_database)
            .unwrap();
        other.statistics.idx_scan = 11;
    }
    crate::mem::guard::forbid_alloc(|| {
        let reader = storage.capture_index_statistics(1, old, old_oid).unwrap();
        storage.reset_index_statistics(old_identity.database, old_oid);
        assert_eq!(
            storage
                .index_cumulative_statistics(other_database, old_oid)
                .idx_scan,
            11
        );
        storage.rollback_create(old);
        let mut definition = make_def("new_counter_source", &[("id", ColType::Int4, true)]);
        definition.columns[0].primary = true;
        let new = table_creation::allocate(
            &storage,
            definition,
            CatalogAllocation::New,
            table_creation::TableCreation::Committed,
        )
        .unwrap();
        assert_eq!(new, old);
        let new_oid = storage
            .table_def(new, 0)
            .column_index_identity(0)
            .unwrap()
            .oid();
        assert_ne!(new_oid, old_oid);
        storage
            .record_relation_scan(1, old, Some(&reader), 5, 1)
            .unwrap();
        storage
            .record_relation_tuple_read(1, old, Some(&reader))
            .unwrap();
        assert_eq!(storage.relation_transaction_statistics(1, new).idx_scan, 0);
        assert_eq!(
            storage
                .index_cumulative_statistics(old_identity.database, new_oid)
                .idx_scan,
            0
        );
        assert_eq!(
            storage
                .index_cumulative_statistics(other_database, old_oid)
                .idx_scan,
            11
        );
        let current = storage.capture_index_statistics(2, new, new_oid).unwrap();
        storage
            .record_relation_scan(2, new, Some(&current), 2, 2)
            .unwrap();
        assert_eq!(
            storage
                .index_cumulative_statistics(old_identity.database, new_oid)
                .idx_scan,
            1
        );
    });
}

#[test]
fn relation_identity_recovery_accepts_only_matching_inline_backing_relations() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut definition = make_def(
        "inline_recovery_rows",
        &[
            ("id", ColType::Int4, true),
            (
                "value",
                ColType::Range(crate::sql::types::RangeKind::Int4),
                false,
            ),
        ],
    );
    definition.columns[0].primary = true;
    definition.n_exclusions = 1;
    definition.exclusions[0] = ExclusionConstraint {
        name: SqlName::parse("inline_recovery_value_exclusion").unwrap(),
        columns: [1; MAX_INDEX_COLS],
        n_cols: 1,
        ..ExclusionConstraint::EMPTY
    };
    let table = storage.create_table(definition).unwrap();
    let definition = storage.table_def(table, 0);
    let backing = IndexDef {
        created_at: definition.column_index_identity(0).unwrap().created_at(),
        schema: definition.schema,
        table: definition.name,
        name: definition.column_key_name(0).unwrap(),
        n_cols: 1,
        unique: true,
        ..IndexDef::EMPTY
    };
    let mut exclusion = IndexDef {
        created_at: definition.exclusions()[0]
            .index_identity
            .unwrap()
            .created_at(),
        name: definition.exclusions()[0].name,
        columns: [1; MAX_INDEX_COLS],
        method: crate::sql::ast::IndexAccessMethod::Gist,
        unique: false,
        ..backing
    };
    exclusion.resolved_operator_classes[0] =
        crate::sql::types::GistOperatorClass::for_type(definition.columns()[1].ctype)
            .map(IndexOperatorClass::Gist);
    let target = storage.table_definition_target(table, 0, &definition);
    drop(definition);
    crate::mem::guard::forbid_alloc(|| {
        let index = storage
            .create_reindex_index(backing, target, IndexRelationKind::Table, 0)
            .unwrap();
        storage.commit_index_create(index).unwrap();
        let excluded = storage
            .create_reindex_index(exclusion, target, IndexRelationKind::Table, 0)
            .unwrap();
        storage.commit_index_create(excluded).unwrap();
        let sequence = storage.catalog_identity_sequence();
        storage
            .validate_restored_relation_identities(sequence)
            .unwrap();
        storage.index_catalog()[index].columns[0] = 1;
        let error = storage
            .validate_restored_relation_identities(sequence)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert!(error.message.as_str().contains("duplicated"));
        storage.index_catalog()[index].columns[0] = 0;
        storage
            .validate_restored_relation_identities(sequence)
            .unwrap();
        storage.index_catalog()[excluded].columns[0] = 0;
        let error = storage
            .validate_restored_relation_identities(sequence)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert!(error.message.as_str().contains("duplicated"));
        storage.index_catalog()[excluded].columns[0] = 1;
        storage
            .validate_restored_relation_identities(sequence)
            .unwrap();
        assert_eq!(storage.catalog_identity_sequence(), sequence);
    });
}

#[test]
fn relation_identity_recovery_rejects_cross_catalog_aliases_without_mutation() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage.validate_restored_relation_identities(0).unwrap();
    let mut definition = make_def("identity_orders", &[("id", ColType::Int4, true)]);
    definition.columns[0].primary = true;
    let slot = storage.create_table(definition).unwrap();
    let sequence = storage.catalog_identity_sequence();
    storage
        .validate_restored_relation_identities(sequence)
        .unwrap();
    for saved_sequence in [0, storage.table(slot).created_at()] {
        let error = crate::mem::guard::forbid_alloc(|| {
            storage.validate_restored_relation_identities(saved_sequence)
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
        assert!(error.message.as_str().contains("invalid"));
        assert_eq!(storage.catalog_identity_sequence(), sequence);
    }
    let table_identity = storage.table(slot).created_at();
    let revision = storage.reserve_table_definition_revision().unwrap();
    let mut invalid = TableDefBuilder::from_definition(&storage.table_def(slot, 0));
    invalid.inline_index_identities[0] = RelationIdentity::parse(table_identity);
    *storage.tables[slot].definition_mut(revision) =
        invalid.freeze(&storage.metadata_pool).unwrap();
    let before = storage.catalog_identity_sequence();
    let error =
        crate::mem::guard::forbid_alloc(|| storage.validate_restored_relation_identities(before))
            .unwrap_err();
    assert_eq!(error.sqlstate, sqlstate::DATA_EXCEPTION);
    assert!(error.message.as_str().contains("duplicated"));
    assert_eq!(storage.catalog_identity_sequence(), before);
}

#[test]
fn catalog_sequence_is_concurrent_monotonic_and_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CatalogSequence>();

    const WORKERS: usize = 4;
    const ADVANCES_PER_WORKER: usize = 1_000;
    const ADVANCES: usize = WORKERS * ADVANCES_PER_WORKER;
    let sequence = CatalogSequence::new(0);
    let claimed: [std::sync::atomic::AtomicBool; ADVANCES + 1] =
        std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            let sequence = &sequence;
            let claimed = &claimed;
            scope.spawn(move || {
                for _ in 0..ADVANCES_PER_WORKER {
                    let value = sequence.next().unwrap() as usize;
                    assert!(value <= ADVANCES);
                    assert!(!claimed[value].swap(true, std::sync::atomic::Ordering::Relaxed));
                }
            });
        }
    });
    assert_eq!(sequence.current(), ADVANCES as u64);
    assert!(
        claimed[1..]
            .iter()
            .all(|value| value.load(std::sync::atomic::Ordering::Relaxed))
    );

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let sequence = &sequence;
            scope.spawn(move || sequence.observe(5_000 + worker as u64));
        }
    });
    assert_eq!(sequence.current(), 5_003);
    assert_eq!(sequence.next().unwrap(), 5_004);

    let bounded = CatalogSequence::new(1);
    assert_eq!(bounded.next_bounded(2, "test").unwrap(), 2);
    assert!(bounded.next_bounded(2, "test").is_err());
}

#[test]
fn role_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<RoleCatalog>>();
    assert_send_sync::<RoleIter<'_>>();
    assert_send_sync::<RoleMembershipIter<'_>>();
    assert_send_sync::<RoleSettingIter<'_>>();

    const WORKERS: usize = 4;
    let names = ["role_a", "role_b", "role_c", "role_d"];
    let mut config = test_config();
    config.max_connections = WORKERS as u32;
    config.max_roles = 1 + WORKERS;
    config.max_role_memberships = WORKERS;
    config.max_role_settings = WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for (worker, worker_name) in names.iter().copied().enumerate() {
            let storage = &storage;
            scope.spawn(move || {
                let txid = worker as u32 + 1;
                let name = SqlName::parse(worker_name).unwrap();
                let (role, _) = storage
                    .create_role(name, RoleAttributes::ORDINARY, txid)
                    .unwrap();
                storage.commit_role_change(role);

                let options = RoleMembershipOptions {
                    admin: true,
                    ..RoleMembershipOptions::DEFAULT
                };
                let deadline = crate::test_wait::deadline();
                let (membership, _) = loop {
                    match storage.change_role_membership(
                        BOOTSTRAP_ROLE as usize,
                        role,
                        0,
                        options,
                        true,
                        txid,
                    ) {
                        Ok(change) => break change,
                        Err(error) if error.sqlstate == sqlstate::INTERNAL_LOCK_WAIT => {
                            assert!(std::time::Instant::now() < deadline, "{error:?}");
                            std::thread::yield_now();
                        }
                        Err(error) => panic!("{error:?}"),
                    }
                };
                storage.commit_role_membership_change(membership);

                let (setting, _) = storage
                    .change_role_setting(
                        RoleSettingScope::RoleAllDatabases(role as u16),
                        SqlName::parse("application_name").unwrap(),
                        Some(StackStr::from_str(worker_name)),
                        txid,
                    )
                    .unwrap();
                storage.commit_role_setting(setting);
                storage.release_object_locks(txid);
            });
        }
    });

    let catalog = storage
        .role_catalog
        .lock()
        .expect("role catalog lock poisoned");
    assert_eq!(catalog.roles.capacity(), 1 + WORKERS);
    assert_eq!(catalog.memberships.capacity(), WORKERS);
    assert_eq!(catalog.settings.capacity(), WORKERS);
    drop(catalog);

    assert_eq!(storage.live_roles().count(), 1 + WORKERS);
    assert_eq!(storage.live_role_memberships().count(), WORKERS);
    assert_eq!(
        storage
            .role_settings()
            .filter(|(_, setting)| setting.live)
            .count(),
        WORKERS
    );
    for (slot, role) in storage.live_roles() {
        assert_eq!(storage.role_name(slot, 0), role.name);
    }
    for worker_name in names.iter().copied() {
        let role = storage.find_role(worker_name).unwrap();
        assert!(storage.role_can_admin(role, BOOTSTRAP_ROLE as usize, 0));
    }
    assert!(
        storage
            .create_role(
                SqlName::parse("role_e").unwrap(),
                RoleAttributes::ORDINARY,
                9,
            )
            .is_err()
    );
    let first = storage.find_role(names[0]).unwrap();
    assert!(
        storage
            .change_role_membership(
                first,
                BOOTSTRAP_ROLE as usize,
                0,
                RoleMembershipOptions::DEFAULT,
                true,
                9,
            )
            .is_err()
    );
    assert!(
        storage
            .change_role_setting(
                RoleSettingScope::AllRolesInDatabase(DatabaseOid::POSTGRES),
                SqlName::parse("application_name").unwrap(),
                Some(StackStr::from_str("full")),
                9,
            )
            .is_err()
    );

    storage.remove_role(names[0]);
    assert!(storage.find_role(names[0]).is_none());
    assert_eq!(storage.live_role_memberships().count(), WORKERS - 1);
    assert_eq!(
        storage
            .role_settings()
            .filter(|(_, setting)| setting.live)
            .count(),
        WORKERS - 1
    );
}

#[test]
fn operator_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<OperatorCatalog>>();
    assert_send_sync::<AccessMethodIter<'_>>();
    assert_send_sync::<OperatorIter<'_>>();
    assert_send_sync::<OperatorFamilyIter<'_>>();
    assert_send_sync::<OperatorClassIter<'_>>();

    const WORKERS: usize = 4;
    let names = ["am_a", "am_b", "am_c", "am_d"];
    let mut config = test_config();
    config.max_operators = WORKERS;
    config.max_operator_families = WORKERS;
    config.max_operator_classes = WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for (worker, worker_name) in names.iter().copied().enumerate() {
            let storage = &storage;
            scope.spawn(move || {
                let slot = storage
                    .create_access_method(
                        AccessMethodDefinition {
                            name: SqlName::parse(worker_name).unwrap(),
                            handler: TableAccessMethodHandler::Heap,
                        },
                        worker as u32 + 1,
                    )
                    .unwrap();
                storage.commit_access_method_create(slot);

                let mut catalog = storage
                    .operator_catalog
                    .lock()
                    .expect("operator catalog lock poisoned");
                let mut operator = OperatorDef::EMPTY;
                operator.database = DatabaseOid::POSTGRES;
                operator.created_at = 100 + worker as u64;
                operator.definition.name = SqlName::parse(worker_name).unwrap();
                operator.ddl_state = CatalogDdlState::Present;
                catalog.operators[worker] = operator;

                let mut family = OperatorFamilyDef::EMPTY;
                family.database = DatabaseOid::POSTGRES;
                family.created_at = 200 + worker as u64;
                family.definition.name = SqlName::parse(worker_name).unwrap();
                family.ddl_state = CatalogDdlState::Present;
                catalog.families[worker] = family;

                let mut class = OperatorClassDef::EMPTY;
                class.database = DatabaseOid::POSTGRES;
                class.created_at = 300 + worker as u64;
                class.definition.name = SqlName::parse(worker_name).unwrap();
                class.ddl_state = CatalogDdlState::Present;
                catalog.classes[worker] = class;
            });
        }
    });

    let catalog = storage
        .operator_catalog
        .lock()
        .expect("operator catalog lock poisoned");
    assert_eq!(catalog.access_methods.capacity(), MAX_ACCESS_METHODS);
    assert_eq!(catalog.operators.capacity(), WORKERS);
    assert_eq!(catalog.families.capacity(), WORKERS);
    assert_eq!(catalog.classes.capacity(), WORKERS);
    drop(catalog);

    assert_eq!(storage.access_methods_visible_to(0).count(), WORKERS);
    assert_eq!(storage.operators_visible_to(0).count(), WORKERS);
    assert_eq!(storage.operator_families_visible_to(0).count(), WORKERS);
    assert_eq!(storage.operator_classes_visible_to(0).count(), WORKERS);
    for (slot, definition) in storage.operators_visible_to(0) {
        assert_eq!(storage.operator(slot).definition.name, definition.name);
    }
    for slot in WORKERS..MAX_ACCESS_METHODS {
        let name = stack_format!(63, "am_{slot}");
        let created = storage
            .create_access_method(
                AccessMethodDefinition {
                    name: SqlName::parse(name.as_str()).unwrap(),
                    handler: TableAccessMethodHandler::Heap,
                },
                9,
            )
            .unwrap();
        storage.commit_access_method_create(created);
    }
    assert!(
        storage
            .create_access_method(
                AccessMethodDefinition {
                    name: SqlName::parse("am_full").unwrap(),
                    handler: TableAccessMethodHandler::Heap,
                },
                9,
            )
            .is_err()
    );

    let slot = storage.access_method_slot(names[0], 0).unwrap();
    let oid = storage.access_method_oid_at(slot).unwrap() as u32;
    let (comment, _) = storage
        .set_comment(
            CommentClass::AccessMethod,
            SqlName::EMPTY,
            SqlName::EMPTY,
            oid,
            Some(StackStr::from_str("comment")),
            10,
        )
        .unwrap()
        .unwrap();
    storage.commit_comment(comment, 10);
    assert!(
        storage
            .comment_text(CommentClass::AccessMethod, "", "", oid, 0,)
            .is_some()
    );
    storage.drop_access_method(slot, 10);
    storage.commit_access_method_drop(slot);
    assert!(
        storage
            .comment_text(CommentClass::AccessMethod, "", "", oid, 0,)
            .is_none()
    );
}

#[test]
fn type_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<TypeCatalog>>();
    assert_send_sync::<DomainIter<'_>>();
    assert_send_sync::<EnumIter<'_>>();
    assert_send_sync::<EnumMemberIter>();
    assert_send_sync::<CompositeIter<'_, fn(&CompositeDef) -> bool>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_domains = WORKERS;
    config.max_enums = WORKERS;
    config.max_enum_labels_per_type = WORKERS;
    config.max_composites = WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let domain_name = stack_format!(63, "domain_{worker}");
                let enum_name = stack_format!(63, "enum_{worker}");
                let composite_name = stack_format!(63, "composite_{worker}");
                let label = stack_format!(63, "label_{worker}");
                let mut catalog = storage
                    .type_catalog
                    .lock()
                    .expect("type catalog lock poisoned");

                let mut domain = DomainDef::EMPTY;
                domain.created_at = worker as u64 + 1;
                domain.name = SqlName::parse(domain_name.as_str()).unwrap();
                domain.ddl_state = CatalogDdlState::Present;
                catalog.domains[worker] = domain;

                let mut enumeration = EnumDef::EMPTY;
                enumeration.created_at = worker as u64 + 101;
                enumeration.name = SqlName::parse(enum_name.as_str()).unwrap();
                enumeration.member_count = 1;
                enumeration.ddl_state = CatalogDdlState::Present;
                let member = EnumMember {
                    identity: None,
                    label: SqlName::parse(label.as_str()).unwrap(),
                    legacy_label: None,
                    sort: worker as f64 + 1.0,
                };
                let image = EnumImageReservation::write(
                    &storage.enum_member_images,
                    &[member],
                    &storage.catalog_sequence,
                    None,
                )
                .unwrap();
                enumeration.member_image = Some(image.publish());
                catalog.enums[worker] = enumeration;

                let mut composite = CompositeDef::EMPTY;
                composite.created_at = worker as u64 + 201;
                composite.name = SqlName::parse(composite_name.as_str()).unwrap();
                composite.ddl_state = CatalogDdlState::Present;
                catalog.composites[worker] = composite;
            });
        }
    });

    let catalog = storage
        .type_catalog
        .lock()
        .expect("type catalog lock poisoned");
    assert_eq!(catalog.domains.capacity(), WORKERS);
    assert_eq!(catalog.enums.capacity(), WORKERS);
    assert_eq!(catalog.composites.capacity(), WORKERS);
    assert_eq!(
        storage
            .enum_member_images
            .lock()
            .expect("enum member-image lock poisoned")
            .len(),
        enum_member_image_capacity(&config) * WORKERS
    );
    assert_eq!(
        catalog.pending_enum_members.capacity(),
        pending_enum_definition_capacity(&config)
    );
    drop(catalog);

    assert_eq!(storage.live_domains().count(), WORKERS);
    assert_eq!(storage.live_enums().count(), WORKERS);
    assert_eq!(storage.live_composites().count(), WORKERS);
    for (slot, domain) in storage.live_domains() {
        assert_eq!(storage.domain(slot).name, domain.name);
        assert_eq!(storage.enum_for(slot, 0).members().len(), 1);
        assert!(storage.composite(slot).visible_to(0));
    }

    assert!(
        storage
            .create_domain(
                SqlName::parse("public").unwrap(),
                SqlName::parse("domain_full").unwrap(),
                DomainSpec {
                    base_domain: None,
                    base_user_type: None,
                    base: ColType::Int4,
                    base_type_mod: -1,
                    not_null: false,
                    not_null_name: None,
                    not_null_identity: None,
                    default_expr: None,
                    default_inputs: StoredTypeInputs::EMPTY,
                    checks: [CheckConstraint::EMPTY; MAX_DOMAIN_CHECKS],
                    n_checks: 0,
                },
                9,
            )
            .is_err()
    );
}

#[test]
fn routine_candidate_lookup_preserves_visibility_identity_and_guard_release() {
    let mut config = test_config();
    config.max_routines = 4;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    {
        let mut catalog = storage.routines.lock().unwrap();
        for slot in 1..4 {
            catalog[slot].schema = SqlName::parse("public").unwrap();
            catalog[slot].name = SqlName::parse("probe").unwrap();
            catalog[slot].ddl_state = CatalogDdlState::Present;
        }
        catalog[1].pending_identity = Some(PendingRoutineIdentity {
            txid: 7,
            version: 1,
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("renamed").unwrap(),
        });
        catalog[2].database = DatabaseOid::parse(USER_DATABASE_OID_BASE + 1).unwrap();
        catalog[3].ddl_state = CatalogDdlState::PendingCreate { txid: 7 };
    }
    crate::mem::guard::forbid_alloc(|| {
        let selected = |name, transaction, kind| {
            let qualified = stack_format!(130, "public.{}", name);
            let selected =
                storage.select_routine_call(qualified.as_str(), &[], &[], false, transaction, kind);
            assert!(storage.routines.try_lock().is_ok());
            if let RoutineCallSelection::Stored(slot) = selected {
                assert!(storage.routine_slot_visible_to(slot, transaction));
            }
            selected
        };
        assert_eq!(
            storage.routine_identity_for(1, 0).unwrap().name.as_str(),
            "probe"
        );
        let private = storage.routine_identity_for(1, 7).unwrap();
        assert_eq!(private.name.as_str(), "renamed");
        assert_eq!(storage.routine_slot_by_oid(private.oid, 7), Some(1));
        assert!(storage.routine_signature_matches(1, 7, &[]));
        assert!(!storage.routine_signature_matches(1, 7, &[RoutineArgumentDef::EMPTY]));
        assert!(!storage.routine_signature_matches(2, 7, &[]));
        assert!(storage.routines.try_lock().is_ok());
        assert!(storage.routine_identity_for(0, 7).is_none());
        assert!(storage.routine_identity_for(2, 7).is_none());
        assert!(storage.routine_identity_for(3, 0).is_none());
        assert!(storage.routine_identity_for(3, 7).is_some());
        assert_eq!(
            selected("probe", 0, RoutineCallKind::Scalar),
            RoutineCallSelection::Stored(1)
        );
        assert_eq!(
            selected("probe", 7, RoutineCallKind::Scalar),
            RoutineCallSelection::Stored(3)
        );
        assert_eq!(
            selected("renamed", 7, RoutineCallKind::Scalar),
            RoutineCallSelection::Stored(1)
        );
        assert_eq!(
            selected("renamed", 0, RoutineCallKind::Scalar),
            RoutineCallSelection::Absent
        );
        assert_eq!(
            selected("probe", 0, RoutineCallKind::Procedure),
            RoutineCallSelection::WrongKind {
                is_procedure: false
            }
        );
        assert_eq!(
            selected("absent", 0, RoutineCallKind::Scalar),
            RoutineCallSelection::Absent
        );
        {
            let mut catalog = storage.routines.lock().unwrap();
            catalog[1].arguments[0].ctype = ColType::Int4;
            catalog[1].argument_count = 1;
            catalog[1].parameters[0].ctype = ColType::Int4;
            catalog[1].parameter_count = 1;
        }
        let RoutineCallSelection::Stored(retained_slot) = storage.select_routine_call(
            "public.renamed",
            &[],
            &[crate::sql::types::oid::INT4],
            false,
            7,
            RoutineCallKind::Scalar,
        ) else {
            panic!("the visible typed routine must be selected");
        };
        let retained = {
            let catalog = storage.routines.lock().unwrap();
            RoutineInputSignature::from_definition(&catalog[retained_slot], 7)
        };
        assert!(storage.routines.try_lock().is_ok());
        {
            let mut catalog = storage.routines.lock().unwrap();
            catalog[1].ddl_state = CatalogDdlState::Absent;
            catalog[1].arguments[0].ctype = ColType::Text;
            catalog[1].parameters[0].ctype = ColType::Text;
        }
        assert_eq!(retained_slot, 1);
        assert_eq!(retained.arguments[0].ctype, ColType::Int4);
        assert_eq!(
            retained.parameter_for_input(0).unwrap().ctype,
            ColType::Int4
        );
        assert_eq!(
            selected("renamed", 7, RoutineCallKind::Scalar),
            RoutineCallSelection::Absent
        );
        assert_eq!(storage.routine_entries().count(), 4);
    });
}

#[test]
fn routine_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<RoutineDef>>>();
    assert_send_sync::<RoutineIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_routines = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "routine_{worker}");
                let mut routine = RoutineDef::EMPTY;
                routine.created_at = worker as u64 + 1;
                routine.schema = SqlName::parse("public").unwrap();
                routine.name = SqlName::parse(name.as_str()).unwrap();
                routine.ddl_state = CatalogDdlState::Present;
                storage
                    .routines
                    .lock()
                    .expect("routine catalog lock poisoned")[worker] = routine;
            });
        }
    });

    assert_eq!(
        storage
            .routines
            .lock()
            .expect("routine catalog lock poisoned")
            .capacity(),
        WORKERS
    );
    assert_eq!(
        storage
            .routine_entries()
            .filter(|(_, routine)| routine.visible_to(0))
            .count(),
        WORKERS
    );
    for (slot, routine) in storage.routine_entries() {
        assert_eq!(storage.routine(slot).name, routine.name);
        assert_eq!(
            storage.routine_slot_by_oid(routine_oid(&routine), 0),
            Some(slot)
        );
    }

    assert!(
        storage
            .create_routine(
                RoutineSpec {
                    identity: RoutineIdentity::Allocate,
                    schema: SqlName::parse("public").unwrap(),
                    name: SqlName::parse("routine_full").unwrap(),
                    arguments: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
                    argument_count: 0,
                    parameters: [RoutineParameterDef::EMPTY; MAX_ROUTINE_ARGUMENTS],
                    parameter_count: 0,
                    kind: RoutineKind::Function {
                        result: RoutineResult::TEXT,
                    },
                    result_columns: [RoutineArgumentDef::EMPTY; MAX_ROUTINE_OUTPUT_COLUMNS],
                    result_column_count: 0,
                    language: RoutineLanguage::Sql,
                    attributes: RoutineAttributes::DEFAULT,
                    configs: [RoutineConfig::EMPTY; MAX_ROUTINE_CONFIGS],
                    config_count: 0,
                    body_kind: RoutineBodyKind::String,
                    body: StackStr::new(),
                    creation_path: StackStr::new(),
                    dependencies: StoredQueryDependencies::EMPTY,
                },
                9,
            )
            .is_err()
    );
}

#[test]
fn cast_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<CastDef>>>();
    assert_send_sync::<CastIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_casts = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                storage.casts.lock().expect("cast catalog lock poisoned")[worker] = CastDef {
                    database: DatabaseOid::POSTGRES,
                    created_at: worker as u64 + 1,
                    source: RoutineResult::builtin(ColType::Int2),
                    target: RoutineResult::builtin(ColType::Date),
                    method: CastMethod::InOut,
                    context: CastContext::Explicit,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });

    assert_eq!(
        storage
            .casts
            .lock()
            .expect("cast catalog lock poisoned")
            .capacity(),
        WORKERS
    );
    assert_eq!(storage.casts_visible_to(0).count(), WORKERS);
    for (slot, definition) in storage.casts_visible_to(0) {
        assert_eq!(storage.cast(slot).created_at, definition.created_at);
    }

    assert!(
        storage
            .create_cast(
                RoutineResult::builtin(ColType::Bool),
                RoutineResult::builtin(ColType::Date),
                CastMethod::InOut,
                CastContext::Explicit,
                9,
            )
            .is_err()
    );

    let mut pending = storage.cast(0);
    pending.ddl_state = CatalogDdlState::PendingCreate { txid: 9 };
    storage.casts.lock().expect("cast catalog lock poisoned")[0] = pending;
    storage.rollback_cast_create(0);
    assert_eq!(storage.cast(0).created_at, 0);
    assert_eq!(storage.cast(0).ddl_state, CatalogDdlState::Absent);
    storage.casts.lock().expect("cast catalog lock poisoned")[0] = CastDef {
        created_at: 99,
        ddl_state: CatalogDdlState::PendingDrop { txid: 9 },
        ..CastDef::EMPTY
    };
    storage.commit_cast_drop(0);
    assert_eq!(storage.cast(0).created_at, 0);
    assert_eq!(storage.cast(0).ddl_state, CatalogDdlState::Absent);
}

#[test]
fn event_trigger_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<EventTriggerDef>>>();
    assert_send_sync::<EventTriggerIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_event_triggers = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    {
        let mut routines = storage
            .routines
            .lock()
            .expect("routine catalog lock poisoned");
        let routine = &mut routines[0];
        routine.database = DatabaseOid::POSTGRES;
        routine.kind = RoutineKind::EventTrigger;
        routine.ddl_state = CatalogDdlState::Present;
    }

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "event_trigger_{worker}");
                storage
                    .event_triggers
                    .lock()
                    .expect("event trigger catalog lock poisoned")[worker] = EventTriggerDef {
                    database: DatabaseOid::POSTGRES,
                    created_at: worker as u64 + 1,
                    definition: EventTriggerDefinition {
                        name: SqlName::parse(name.as_str()).unwrap(),
                        event: crate::sql::ast::EventTriggerEvent::DdlCommandStart,
                        function: 0,
                        tags: EventTriggerTags::EMPTY,
                        enabled: TriggerEnabled::Origin,
                        ownership: Ownership::BOOTSTRAP,
                    },
                    pending: None,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });

    assert_eq!(storage.event_trigger_capacity(), WORKERS);
    assert_eq!(storage.event_triggers_visible_to(0).count(), WORKERS);
    for (slot, definition) in storage.event_triggers_visible_to(0) {
        assert_eq!(storage.event_trigger(slot).definition.name, definition.name);
    }

    let full = EventTriggerDefinition {
        name: SqlName::parse("event_trigger_full").unwrap(),
        event: crate::sql::ast::EventTriggerEvent::DdlCommandStart,
        function: 0,
        tags: EventTriggerTags::EMPTY,
        enabled: TriggerEnabled::Origin,
        ownership: Ownership::BOOTSTRAP,
    };
    assert!(storage.create_event_trigger(full, 9).is_err());

    {
        let mut event_triggers = storage
            .event_triggers
            .lock()
            .expect("event trigger catalog lock poisoned");
        event_triggers[0].ddl_state = CatalogDdlState::PendingCreate { txid: 9 };
    }
    storage.rollback_event_trigger_create(0);
    assert_eq!(storage.event_trigger(0).created_at, 0);
    assert_eq!(storage.event_trigger(0).ddl_state, CatalogDdlState::Absent);

    let pending = EventTriggerDefinition {
        name: SqlName::parse("event_trigger_pending").unwrap(),
        ..full
    };
    storage
        .event_triggers
        .lock()
        .expect("event trigger catalog lock poisoned")[0] = EventTriggerDef {
        database: DatabaseOid::POSTGRES,
        created_at: 99,
        definition: pending,
        pending: None,
        ddl_state: CatalogDdlState::PendingCreate { txid: 9 },
    };
    assert!(storage.create_event_trigger(pending, 10).is_err());
    storage.rollback_event_trigger_create(0);

    {
        let mut event_triggers = storage
            .event_triggers
            .lock()
            .expect("event trigger catalog lock poisoned");
        event_triggers[1].ddl_state = CatalogDdlState::PendingDrop { txid: 9 };
    }
    storage.commit_event_trigger_drop(1);
    assert_eq!(storage.event_trigger(1).created_at, 0);
    assert_eq!(storage.event_trigger(1).ddl_state, CatalogDdlState::Absent);
}

#[test]
fn trigger_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<TriggerCatalog>>();
    assert_send_sync::<TriggerIter<'_>>();
    assert_send_sync::<PartitionTriggerStateIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_triggers = WORKERS;
    let max_tables = config.max_tables;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "trigger_{worker}");
                let mut catalog = storage
                    .trigger_catalog
                    .lock()
                    .expect("trigger catalog lock poisoned");
                catalog.definitions[worker] = TriggerDef {
                    created_at: worker as u64 + 1,
                    name: SqlName::parse(name.as_str()).unwrap(),
                    target: TriggerTarget::Table(0),
                    enabled: TriggerEnabled::Origin,
                    ddl_state: CatalogDdlState::Present,
                    ..TriggerDef::EMPTY
                };
                catalog.partition_states[storage.partition_trigger_state_slot(worker, worker)] =
                    PartitionTriggerState {
                        database: current_database(),
                        committed: Some(PartitionTriggerImage {
                            creation: CatalogIdentity::require(worker as u64 + 10).unwrap(),
                            trigger: CatalogPosition::new(worker, worker as u64 + 1).unwrap(),
                            table: CatalogPosition::new(worker, worker as u64 + 1).unwrap(),
                            enabled: TriggerEnabled::Replica,
                        }),
                        pending: None,
                    };
            });
        }
    });

    {
        let catalog = storage
            .trigger_catalog
            .lock()
            .expect("trigger catalog lock poisoned");
        assert_eq!(catalog.definitions.capacity(), WORKERS);
        assert_eq!(catalog.partition_states.capacity(), WORKERS * max_tables);
    }
    assert_eq!(storage.triggers_with_slots_visible_to(0).count(), WORKERS);
    for (slot, trigger) in storage.trigger_metadata_for_target(TriggerTarget::Table(0), 0) {
        assert_eq!(storage.trigger(slot).name, trigger.name);
    }
    for worker in 0..WORKERS {
        assert_eq!(
            storage.partition_trigger_enabled_to(worker, worker, 0),
            TriggerEnabled::Replica
        );
    }

    let full = TriggerSpec {
        name: SqlName::parse("trigger_full").unwrap(),
        target: TriggerTarget::Table(0),
        kind: TriggerKind::Ordinary,
        function: 0,
        timing: crate::sql::ast::TriggerTiming::Before,
        level: crate::sql::ast::TriggerLevel::Row,
        events: crate::sql::ast::TriggerEvents::from_bits(1).unwrap(),
        update_columns: ColumnSet::EMPTY,
        transition_tables: TriggerTransitionTables::None,
        when: None,
        arguments: TriggerArguments::EMPTY,
    };
    assert!(storage.create_trigger(full, 9).is_err());

    let renamed = SqlName::parse("trigger_pending").unwrap();
    let prior = match storage
        .alter_trigger(0, renamed, TriggerEnabled::Always, 9)
        .unwrap()
    {
        TriggerAlter::Changed { prior } => prior,
        TriggerAlter::Unchanged => panic!("trigger alter must stage a new definition"),
    };
    assert!(storage.drop_trigger(0, 10).is_err());
    assert!(
        storage
            .create_trigger(
                TriggerSpec {
                    name: renamed,
                    ..full
                },
                10
            )
            .is_err()
    );
    storage.rollback_trigger_alter(0, prior);

    storage.drop_trigger(0, 9).unwrap();
    storage.commit_trigger_drop(0);
    assert_eq!(storage.trigger(0).created_at, 0);
    assert_eq!(storage.trigger(0).ddl_state, CatalogDdlState::Absent);
    let image = storage.partition_trigger_image_to(0, 0, 0).unwrap();
    storage
        .install_partition_trigger_image(Some(image), None, 0)
        .unwrap();
    assert_eq!(storage.partition_trigger_states().count(), WORKERS - 1);
}

#[test]
fn extended_statistics_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<ExtendedStatisticsCatalog>>();
    assert_send_sync::<ExtendedStatisticsIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_connections = WORKERS as u32;
    config.max_extended_statistics = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def(
            "statistics_table",
            &[("value", ColType::Int4, false)],
        ))
        .unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "statistics_{worker}");
                let mut catalog = storage
                    .extended_statistics_catalog
                    .lock()
                    .expect("extended-statistics catalog lock poisoned");
                let pending = catalog.pending_data.len();
                catalog
                    .pending_data
                    .push(PendingExtendedStatisticsDataSlot {
                        used: true,
                        previous: None,
                        depth: 1,
                        data: ExtendedStatisticsData {
                            valid: true,
                            rows: worker as u64 + 1,
                            ..ExtendedStatisticsData::EMPTY
                        },
                    })
                    .unwrap();
                catalog.definitions[worker] = ExtendedStatisticsDef {
                    database: DatabaseOid::POSTGRES,
                    created_at: worker as u64 + 1,
                    table: table as u16,
                    mutable: ExtendedStatisticsMutableDefinition {
                        schema: SqlName::parse("public").unwrap(),
                        name: SqlName::parse(name.as_str()).unwrap(),
                        target: None,
                    },
                    ownership: Ownership::BOOTSTRAP,
                    keys: [ExtendedStatisticsKey::Column(SqlName::parse("value").unwrap());
                        MAX_EXTENDED_STATISTICS_KEYS],
                    n_keys: 1,
                    kinds: crate::sql::ast::StatisticsKinds::ALL,
                    pending_data_tail: Some(pending as u32),
                    pending_data_txid: Some(100 + worker as u32),
                    ddl_state: CatalogDdlState::Present,
                    ..ExtendedStatisticsDef::EMPTY
                };
            });
        }
    });

    {
        let catalog = storage
            .extended_statistics_catalog
            .lock()
            .expect("extended-statistics catalog lock poisoned");
        assert_eq!(catalog.definitions.capacity(), WORKERS);
        assert_eq!(
            catalog.pending_data.capacity(),
            pending_extended_statistics_capacity(&config)
        );
    }
    assert_eq!(storage.extended_statistics_visible(0).count(), WORKERS);
    for (slot, definition) in storage.extended_statistics_visible(0) {
        assert_eq!(
            storage.extended_statistics(slot).created_at,
            definition.created_at
        );
        assert_eq!(
            storage
                .extended_statistics_data(slot, 100 + slot as u32)
                .rows,
            slot as u64 + 1
        );
    }

    let full = ExtendedStatisticsSpec {
        creation: CatalogCreation::New,
        schema: SqlName::parse("public").unwrap(),
        name: SqlName::parse("statistics_full").unwrap(),
        table: table as u16,
        target: None,
        keys: [ExtendedStatisticsKey::Column(SqlName::parse("value").unwrap());
            MAX_EXTENDED_STATISTICS_KEYS],
        n_keys: 1,
        kinds: crate::sql::ast::StatisticsKinds::ALL,
        expression_only: false,
    };
    assert!(storage.create_extended_statistics(full, 0).is_err());

    for slot in 0..WORKERS {
        storage.rollback_extended_statistics_data(slot, 100 + slot as u32);
    }
    let renamed = ExtendedStatisticsMutableDefinition {
        schema: SqlName::parse("public").unwrap(),
        name: SqlName::parse("statistics_pending").unwrap(),
        target: None,
    };
    let prior = storage.alter_extended_statistics(0, renamed, 9).unwrap();
    assert!(storage.alter_extended_statistics(1, renamed, 10).is_err());
    assert!(
        storage
            .write_extended_statistics_data(0, 10, ExtendedStatisticsData::EMPTY)
            .is_err()
    );
    storage.rollback_extended_statistics_alter(0, prior);

    {
        let mut catalog = storage
            .extended_statistics_catalog
            .lock()
            .expect("extended-statistics catalog lock poisoned");
        catalog.definitions[0].ddl_state = CatalogDdlState::PendingCreate { txid: 9 };
        let keys = catalog.definitions[0].keys;
        catalog.definitions[0].pending_keys = Some(PendingExtendedStatisticsKeys { txid: 9, keys });
    }
    storage
        .write_extended_statistics_data(0, 9, ExtendedStatisticsData::EMPTY)
        .unwrap();
    storage.rollback_extended_statistics_create(0);
    assert_eq!(storage.extended_statistics(0).created_at, 0);
    assert_eq!(
        storage.extended_statistics(0).ddl_state,
        CatalogDdlState::Absent
    );
    assert!(
        storage
            .extended_statistics_catalog
            .lock()
            .expect("extended-statistics catalog lock poisoned")
            .pending_data
            .iter()
            .all(|pending| !pending.used)
    );

    storage.drop_extended_statistics(1, 9).unwrap();
    storage.commit_extended_statistics_drop(1);
    assert_eq!(storage.extended_statistics(1).created_at, 0);
    assert_eq!(
        storage.extended_statistics(1).ddl_state,
        CatalogDdlState::Absent
    );
}

#[test]
fn publication_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<PublicationDef>>>();
    assert_send_sync::<PublicationIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_connections = WORKERS as u32;
    config.max_publications = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "publication_{worker}");
                storage
                    .publications
                    .lock()
                    .expect("publication catalog lock poisoned")[worker] = PublicationDef {
                    created_at: worker as u64 + 1,
                    name: SqlName::parse(name.as_str()).unwrap(),
                    all_tables: true,
                    ddl_state: CatalogDdlState::Present,
                    ..PublicationDef::EMPTY
                };
            });
        }
    });

    assert_eq!(
        storage
            .publications
            .lock()
            .expect("publication catalog lock poisoned")
            .capacity(),
        WORKERS
    );
    assert_eq!(storage.publications_with_slots().count(), WORKERS);
    for (slot, definition) in storage.publications_with_slots() {
        assert_eq!(
            storage.publication_at(slot).created_at,
            definition.created_at
        );
    }

    let empty_masks: [ColumnSet; 0] = [];
    let empty_descendants: [bool; 0] = [];
    let empty_filters: [StackStr<PUBLICATION_FILTER_SQL_MAX>; 0] = [];
    let full = PublicationSpec {
        creation: CatalogCreation::New,
        name: SqlName::parse("publication_full").unwrap(),
        all_tables: true,
        tables: &[],
        table_column_masks: &empty_masks,
        table_include_descendants: &empty_descendants,
        table_filter_sql: &empty_filters,
        schemas: &[],
        publish_insert: true,
        publish_update: true,
        publish_delete: true,
        publish_truncate: true,
        publish_via_partition_root: false,
        publish_generated_columns: PublishGeneratedColumns::None,
    };
    assert!(storage.create_publication(full, 0).is_err());

    let pending_name = SqlName::parse("publication_pending").unwrap();
    let prior = storage.rename_publication(0, pending_name, 9).unwrap();
    assert!(storage.rename_publication(1, pending_name, 10).is_err());
    assert!(
        storage
            .create_publication(
                PublicationSpec {
                    creation: CatalogCreation::New,
                    name: pending_name,
                    ..full
                },
                10,
            )
            .is_err()
    );
    assert!(storage.drop_publication("publication_0", 10).is_err());
    storage.rollback_publication_rename(0, prior);

    {
        let mut publications = storage
            .publications
            .lock()
            .expect("publication catalog lock poisoned");
        publications[0].ddl_state = CatalogDdlState::PendingCreate { txid: 9 };
        publications[0].pending_name = Some(PendingPublicationName {
            txid: 9,
            name: pending_name,
        });
    }
    storage.rollback_publication_create(0);
    assert_eq!(storage.publication_at(0).created_at, 0);
    assert_eq!(storage.publication_at(0).ddl_state, CatalogDdlState::Absent);

    storage.drop_publication("publication_1", 9).unwrap();
    storage.commit_publication_drop(1);
    assert_eq!(storage.publication_at(1).created_at, 0);
    assert_eq!(storage.publication_at(1).ddl_state, CatalogDdlState::Absent);
}

#[test]
fn replication_slot_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<ReplicationSlotDef>>>();
    assert_send_sync::<ReplicationSlotIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_connections = WORKERS as u32;
    config.max_replication_slots = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "replication_slot_{worker}");
                storage
                    .replication_slots
                    .lock()
                    .expect("replication slot catalog lock poisoned")[worker] =
                    ReplicationSlotDef {
                        name: SqlName::parse(name.as_str()).unwrap(),
                        restart_lsn: worker as u64 + 1,
                        confirmed_flush_lsn: worker as u64 + 1,
                        sent_lsn: worker as u64 + 1,
                        live: true,
                        ..ReplicationSlotDef::EMPTY
                    };
            });
        }
    });

    assert_eq!(storage.replication_slot_capacity(), WORKERS);
    assert_eq!(storage.replication_slots_with_slots().count(), WORKERS);
    for (_, slot) in storage.replication_slots_with_slots() {
        assert_eq!(
            storage
                .replication_slot(slot.name.as_str())
                .unwrap()
                .restart_lsn,
            slot.restart_lsn
        );
    }
    assert!(
        storage
            .create_replication_slot(
                ReplicationSlotName::parse("replication_slot_full").unwrap(),
                10,
                ReplicationSlotBehavior::DEFAULT,
            )
            .is_err()
    );

    storage
        .activate_replication_slot("replication_slot_0", 41)
        .unwrap();
    storage.record_replication_slot_transaction("replication_slot_0", 20, 100);
    assert!(storage.drop_replication_slot("replication_slot_0").is_err());
    storage.deactivate_replication_slot("replication_slot_0");
    storage.drop_replication_slot("replication_slot_0").unwrap();
    let cleared = storage
        .replication_slots
        .lock()
        .expect("replication slot catalog lock poisoned")[0];
    assert!(!cleared.live);
    assert_eq!(cleared.name, SqlName::EMPTY);
    assert_eq!((cleared.total_txns, cleared.total_bytes), (0, 0));

    let other_database = DatabaseOid::parse(USER_DATABASE_OID_BASE + 1).unwrap();
    {
        let mut slots = storage
            .replication_slots
            .lock()
            .expect("replication slot catalog lock poisoned");
        let name = slots[1].name;
        slots[0] = ReplicationSlotDef {
            database: other_database,
            name,
            restart_lsn: 99,
            confirmed_flush_lsn: 99,
            sent_lsn: 99,
            live: true,
            ..ReplicationSlotDef::EMPTY
        };
    }
    storage
        .activate_replication_slot("replication_slot_1", 42)
        .unwrap();
    storage.record_replication_slot_transaction("replication_slot_1", 21, 200);
    storage.deactivate_replication_slot("replication_slot_1");
    let slots = storage
        .replication_slots
        .lock()
        .expect("replication slot catalog lock poisoned");
    assert_eq!(slots[0].sent_lsn, 99);
    assert_eq!((slots[0].total_txns, slots[0].total_bytes), (0, 0));
    assert_eq!(slots[1].sent_lsn, 21);
    assert_eq!((slots[1].total_txns, slots[1].total_bytes), (1, 200));
}

#[test]
fn subscription_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<SubscriptionCatalog>>();
    assert_send_sync::<SubscriptionIter<'_>>();
    assert_send_sync::<SubscriptionRelationIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_connections = WORKERS as u32;
    config.max_subscriptions = WORKERS;
    config.subscription_relation_capacity = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let connection =
        SubscriptionConnInfo::parse("host=127.0.0.1 port=1 user=test dbname=test sslmode=disable")
            .unwrap();
    let publication = SqlName::parse("published_changes").unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "subscription_{worker}");
                let mut catalog = storage
                    .subscription_catalog
                    .lock()
                    .expect("subscription catalog lock poisoned");
                catalog.definitions[worker] = SubscriptionDef {
                    created_at: worker as u64 + 1,
                    definition_generation: 1,
                    name: SqlName::parse(name.as_str()).unwrap(),
                    connection,
                    publications: [publication; MAX_SUBSCRIPTION_PUBLICATIONS],
                    publication_count: 1,
                    ddl_state: CatalogDdlState::Present,
                    ..SubscriptionDef::EMPTY
                };
                catalog.relations[worker] = SubscriptionRelation {
                    subscription_created_at: worker as u64 + 1,
                    definition_generation: 1,
                    table_slot: worker as u16,
                    state: SubscriptionRelationState::DataCopy,
                    synchronization_lsn: 0,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });

    {
        let catalog = storage
            .subscription_catalog
            .lock()
            .expect("subscription catalog lock poisoned");
        assert_eq!(catalog.definitions.capacity(), WORKERS);
        assert_eq!(catalog.relations.capacity(), WORKERS);
    }
    assert_eq!(
        storage
            .subscriptions_with_slots_visible_to(Some(storage.current_database_oid()), 0)
            .count(),
        WORKERS
    );
    for (slot, definition) in
        storage.subscriptions_with_slots_visible_to(Some(storage.current_database_oid()), 0)
    {
        let nested = storage
            .subscription(definition.name.as_str(), 0)
            .expect("copied iterator releases the catalog lock");
        assert_eq!(nested.0, slot);
        assert_eq!(
            storage
                .subscription_relations_visible_to(&definition, 0)
                .count(),
            1
        );
    }

    assert!(
        storage
            .create_subscription(
                SubscriptionSpec {
                    name: SqlName::parse("subscription_full").unwrap(),
                    connection,
                    publications: &[publication],
                    enabled: false,
                    slot: SubscriptionSlot::Absent,
                    behavior: SubscriptionBehavior::POSTGRESQL_18_DEFAULT,
                    bootstrap: SubscriptionBootstrap::Deferred,
                },
                9,
            )
            .is_err()
    );

    let mut definition = storage.subscription_definition_to(0, 9);
    definition.connection =
        SubscriptionConnInfo::parse("host=127.0.0.1 port=2 user=test dbname=test sslmode=disable")
            .unwrap();
    assert!(
        storage
            .set_subscription_definition(0, definition, 9)
            .unwrap()
            .changed
    );
    storage.commit_subscription_definition(0, 9);
    let subscription = storage.subscription("subscription_0", 0).unwrap().1;
    assert_eq!(subscription.definition_generation, 2);
    let relation = storage
        .subscription_relations_visible_to(&subscription, 0)
        .next()
        .unwrap();
    assert_eq!(relation.definition_generation, 2);

    let stream = storage.subscription_stream(0, 0).unwrap();
    let advance = storage
        .subscription_advance(stream, 42, 0)
        .unwrap()
        .unwrap();
    storage.apply_subscription_advance(advance, 7);
    let subscription = storage.subscription("subscription_0", 0).unwrap().1;
    assert_eq!(
        (subscription.confirmed_lsn, subscription.origin_lsn),
        (42, 7)
    );
    let relation = storage
        .subscription_relations_visible_to(&subscription, 0)
        .next()
        .unwrap();
    assert_eq!(relation.state(), SubscriptionRelationState::Ready);
    assert_eq!(relation.synchronization_lsn(), 42);

    storage
        .begin_subscription_relation_refresh(stream, 11)
        .unwrap();
    assert!(
        storage
            .rename_subscription(0, SqlName::parse("blocked_rename").unwrap(), 12)
            .is_err()
    );
    storage.rollback_subscription_relation_refresh(11);

    storage.drop_subscription("subscription_0", 9).unwrap();
    storage.commit_subscription_drop(0);
    let catalog = storage
        .subscription_catalog
        .lock()
        .expect("subscription catalog lock poisoned");
    assert_eq!(catalog.definitions[0].created_at, 0);
    assert_eq!(catalog.definitions[0].name, SqlName::EMPTY);
    assert_eq!(catalog.relations[0].subscription_created_at, 0);
    assert_eq!(catalog.relations[0].ddl_state, CatalogDdlState::Absent);
    drop(catalog);

    let other_database = DatabaseOid::parse(USER_DATABASE_OID_BASE + 1).unwrap();
    storage
        .subscription_catalog
        .lock()
        .expect("subscription catalog lock poisoned")
        .definitions[1]
        .database = other_database;
    storage.clear_database_catalog(other_database);
    let catalog = storage
        .subscription_catalog
        .lock()
        .expect("subscription catalog lock poisoned");
    assert_eq!(catalog.definitions[1].created_at, 0);
    assert_eq!(catalog.relations[1].subscription_created_at, 0);
}

#[test]
fn text_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<TextCatalog>>();
    assert_send_sync::<CollationIter<'_>>();
    assert_send_sync::<ConversionIter<'_>>();
    assert_send_sync::<TextSearchIter<'_>>();

    const WORKERS: usize = 4;
    const BUILTINS: usize = 21;
    let mut config = test_config();
    config.max_collations = WORKERS;
    config.max_conversions = WORKERS;
    config.max_text_search_objects = BUILTINS + WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let name = stack_format!(63, "text_{worker}");
                let name = SqlName::parse(name.as_str()).unwrap();
                let schema = SqlName::parse("public").unwrap();
                let mut catalog = storage
                    .text_catalog
                    .lock()
                    .expect("text catalog lock poisoned");

                let mut collation = CollationDef::EMPTY;
                collation.created_at = worker as u64 + 1;
                collation.definition.schema = schema;
                collation.definition.name = name;
                collation.ddl_state = CatalogDdlState::Present;
                catalog.collations[worker] = collation;

                let mut conversion = ConversionDef::EMPTY;
                conversion.created_at = worker as u64 + 101;
                conversion.definition.schema = schema;
                conversion.definition.name = name;
                conversion.ddl_state = CatalogDdlState::Present;
                catalog.conversions[worker] = conversion;

                catalog.text_search_objects[BUILTINS + worker] = TextSearchDef {
                    database: DatabaseOid::POSTGRES,
                    created_at: worker as u64 + 201,
                    definition: TextSearchDefinition::Parser {
                        schema,
                        name,
                        oid: 400_000 + worker as i32,
                        start: 3717,
                        gettoken: 3718,
                        end: 3719,
                        headline: 3720,
                        lextypes: 3721,
                    },
                    pending: None,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });

    let catalog = storage
        .text_catalog
        .lock()
        .expect("text catalog lock poisoned");
    assert_eq!(catalog.collations.capacity(), WORKERS);
    assert_eq!(catalog.conversions.capacity(), WORKERS);
    assert_eq!(catalog.text_search_objects.capacity(), BUILTINS + WORKERS);
    drop(catalog);

    assert_eq!(storage.collations_visible_to(0).count(), WORKERS);
    assert_eq!(storage.conversions_visible_to(0).count(), WORKERS);
    assert_eq!(
        storage.text_search_objects_visible_to(0).count(),
        7 + WORKERS
    );
    for (slot, definition) in storage.collations_visible_to(0) {
        assert_eq!(storage.collation(slot).definition.name, definition.name);
        assert_eq!(storage.conversion(slot).definition.name, definition.name);
    }

    assert!(
        storage
            .create_collation(
                CollationDefinition {
                    schema: SqlName::parse("public").unwrap(),
                    name: SqlName::parse("collation_full").unwrap(),
                    behavior: CollationBehavior::Database,
                    ..CollationDefinition::EMPTY
                },
                9,
            )
            .is_err()
    );
}

#[test]
fn large_object_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<LargeObjectCatalog>>();
    assert_send_sync::<LargeObjectIter<'_>>();

    const WORKERS: usize = 4;
    const ADVANCES_PER_WORKER: usize = 1_000;
    const ADVANCES: usize = WORKERS * ADVANCES_PER_WORKER;
    const FIRST_OID: u32 = 16_384;
    let mut config = test_config();
    config.max_large_objects = WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let (owner, _) = storage
        .create_role(
            SqlName::parse("large_object_owner").unwrap(),
            RoleAttributes::ORDINARY,
            7,
        )
        .unwrap();
    storage.commit_role_change(owner);
    let catalog = &storage.large_objects;
    let claimed: [std::sync::atomic::AtomicBool; ADVANCES] =
        std::array::from_fn(|_| std::sync::atomic::AtomicBool::new(false));
    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let claimed = &claimed;
            scope.spawn(move || {
                let mut last = LargeObjectOid::parse(FIRST_OID).unwrap();
                for _ in 0..ADVANCES_PER_WORKER {
                    let oid = catalog
                        .lock()
                        .expect("large object catalog lock poisoned")
                        .allocate_oid(DatabaseOid::POSTGRES)
                        .unwrap();
                    let offset = (oid.get() - FIRST_OID) as usize;
                    assert!(offset < ADVANCES);
                    assert!(!claimed[offset].swap(true, std::sync::atomic::Ordering::Relaxed));
                    last = oid;
                }
                catalog
                    .lock()
                    .expect("large object catalog lock poisoned")
                    .definitions[worker] = LargeObjectDef {
                    database: DatabaseOid::POSTGRES,
                    oid: last,
                    ownership: Ownership::BOOTSTRAP,
                    created_at: worker as u64 + 1,
                    allocated: true,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });
    assert!(
        claimed
            .iter()
            .all(|value| value.load(std::sync::atomic::Ordering::Relaxed))
    );
    let catalog = catalog.lock().expect("large object catalog lock poisoned");
    assert_eq!(catalog.definitions.len(), WORKERS);
    assert_eq!(catalog.definitions.capacity(), WORKERS);
    assert_eq!(catalog.next_oid.unwrap().get(), FIRST_OID + ADVANCES as u32);
    drop(catalog);

    let mut seen = 0;
    for (slot, object) in storage.checkpoint_large_objects() {
        assert_eq!(storage.large_object(slot).oid, object.oid);
        seen += 1;
    }
    assert_eq!(seen, WORKERS);
    storage
        .large_objects
        .lock()
        .expect("large object catalog lock poisoned")
        .definitions[0]
        .ownership
        .owner = owner as u16;
    assert_eq!(
        storage.role_object_dependency(owner, 0).unwrap(),
        Some(RoleObjectDependency::OwnedObject)
    );
}

#[test]
fn acl_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<AclCatalog>>();
    assert_send_sync::<ObjectAclIter<'_>>();
    assert_send_sync::<ColumnAclIter<'_>>();
    assert_send_sync::<DefaultAclIter<'_>>();
    assert_send_sync::<parameter_acls::ParameterAclRowIter<'_>>();

    const WORKERS: usize = 4;
    let parameters = ["acl.a", "acl.b", "acl.c", "acl.d"];
    let mut config = test_config();
    config.max_roles = WORKERS + 2;
    config.max_acl_entries = 3 + WORKERS;
    config.max_column_acl_entries = WORKERS;
    config.max_default_acl_entries = WORKERS;
    config.max_parameter_acl_entries = 2 * WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let grantees = ["acl_a", "acl_b", "acl_c", "acl_d", "acl_e"].map(|name| {
        let (slot, _) = storage
            .create_role(SqlName::parse(name).unwrap(), RoleAttributes::ORDINARY, 1)
            .unwrap();
        storage.commit_role_change(slot);
        slot as u16
    });

    std::thread::scope(|scope| {
        for (worker, parameter) in parameters.iter().copied().enumerate() {
            let storage = &storage;
            let grantee = grantees[worker];
            scope.spawn(move || {
                let txid = worker as u32 + 1;
                let object = AccessObject {
                    class: AccessClass::Database,
                    slot: 0,
                };
                let (object_slot, _) = storage
                    .change_acl(
                        object,
                        grantee,
                        BOOTSTRAP_ROLE,
                        PrivilegeSet::CONNECT,
                        PrivilegeSet::NONE,
                        ObjectAclWrite::Change { transaction: 0 },
                    )
                    .unwrap();
                storage.commit_acl(object_slot, txid);

                let target = ColumnPrivilegeTarget::new(
                    AccessObject {
                        class: AccessClass::Table,
                        slot: worker as u16,
                    },
                    1,
                )
                .unwrap();
                let (column_slot, _) = storage
                    .change_column_acl(
                        target,
                        grantee,
                        BOOTSTRAP_ROLE,
                        PrivilegeSet::SELECT,
                        PrivilegeSet::NONE,
                        txid,
                    )
                    .unwrap();
                storage.commit_column_acl(column_slot, txid);

                let (default_slot, _) = storage
                    .change_default_acl(
                        DefaultAclKey {
                            owner: grantee,
                            schema: DEFAULT_ACL_ALL_SCHEMAS,
                            class: DefaultPrivilegeClass::Table,
                            grantee,
                        },
                        true,
                        PrivilegeSet::SELECT,
                        PrivilegeSet::NONE,
                        txid,
                    )
                    .unwrap()
                    .expect("the fresh default privilege declaration changes its baseline");
                storage.commit_default_acl(default_slot, txid);

                let parameter =
                    crate::sql::ast::ParameterName::parse(parameter).expect("parameter name fits");
                let _ = storage
                    .change_parameter_acl(
                        parameter,
                        grantee,
                        BOOTSTRAP_ROLE,
                        crate::sql::ast::ParameterPrivileges::SET,
                        crate::sql::ast::ParameterPrivileges::NONE,
                        txid,
                    )
                    .unwrap();
                storage.commit_parameter_acls(txid);
            });
        }
    });

    let catalog = storage
        .acl_catalog
        .lock()
        .expect("ACL catalog lock poisoned");
    assert_eq!(catalog.objects.capacity(), 3 + WORKERS);
    assert_eq!(catalog.columns.capacity(), WORKERS);
    assert_eq!(catalog.defaults.capacity(), WORKERS);
    assert_eq!(catalog.parameters.capacity(), 2 * WORKERS);
    drop(catalog);

    let mut seen = 0;
    for (slot, entry) in storage.checkpoint_acls() {
        assert_eq!(storage.acl_entry(slot).object, entry.object);
        seen += usize::from(entry.grantee != PUBLIC_ROLE);
    }
    assert_eq!(seen, WORKERS);
    assert_eq!(storage.checkpoint_column_acls().count(), WORKERS);
    assert_eq!(storage.checkpoint_default_acls().count(), WORKERS);
    assert_eq!(
        storage
            .acl_catalog
            .lock()
            .unwrap()
            .parameters
            .iter()
            .filter(|entry| entry.live)
            .count(),
        2 * WORKERS
    );
    assert_eq!(storage.parameter_acl_rows_visible(0).count(), WORKERS);
    assert_eq!(
        storage
            .change_parameter_acl(
                crate::sql::ast::ParameterName::parse("acl.e").unwrap(),
                grantees[WORKERS],
                BOOTSTRAP_ROLE,
                crate::sql::ast::ParameterPrivileges::SET,
                crate::sql::ast::ParameterPrivileges::NONE,
                (WORKERS + 1) as u32,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );
}

#[test]
fn object_acl_ownership_order_and_undo_are_bounded() {
    let mut config = test_config();
    config.max_acl_entries = 5;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut other_budget = test_budget(&config);
    let other = Storage::new(&config, &mut other_budget, 0).unwrap();
    let object = AccessObject {
        class: AccessClass::Database,
        slot: 0,
    };
    let unrelated = AccessObject {
        class: AccessClass::Database,
        slot: 1,
    };
    crate::mem::guard::forbid_alloc(|| {
        let (slot, undo) = storage
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::CONNECT,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 7 },
            )
            .unwrap();
        let order = storage.object_acl_order(slot, 7).unwrap();
        assert!(storage.object_acl_defined(object, 7));
        assert!(!storage.object_acl_defined(object, 8));
        assert_eq!(
            storage
                .change_acl(
                    object,
                    BOOTSTRAP_ROLE,
                    BOOTSTRAP_ROLE,
                    PrivilegeSet::CONNECT,
                    PrivilegeSet::NONE,
                    ObjectAclWrite::Change { transaction: 8 }
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        let (other_slot, other_undo) = other
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::CONNECT,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 7 },
            )
            .unwrap();
        assert_eq!(other_slot, slot);
        assert!(other.restore_acl_pending(other_slot, undo).is_err());
        other.restore_acl_pending(other_slot, other_undo).unwrap();
        let (independent, independent_undo) = storage
            .change_acl(
                unrelated,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::CONNECT,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 8 },
            )
            .unwrap();
        storage
            .restore_acl_pending(independent, independent_undo)
            .unwrap();
        storage.restore_acl_pending(slot, undo).unwrap();
        assert!(!storage.object_acl_defined(object, 0));
        let (reused, replacement) = storage
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::CONNECT,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 8 },
            )
            .unwrap();
        assert_eq!(reused, slot);
        assert!(storage.restore_acl_pending(slot, undo).is_err());
        let new_order = storage.object_acl_order(slot, 8).unwrap();
        assert!(new_order > order);
        storage.commit_acl(slot, 8);
        assert!(storage.restore_acl_pending(slot, replacement).is_err());
        let (_, update) = storage
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::CONNECT.union(PrivilegeSet::TEMPORARY),
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 9 },
            )
            .unwrap();
        assert_eq!(storage.object_acl_order(slot, 9), Some(new_order));
        let (_, revoke) = storage
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::NONE,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 9 },
            )
            .unwrap();
        assert!(storage.restore_acl_pending(slot, update).is_err());
        storage.restore_acl_pending(slot, revoke).unwrap();
        storage.restore_acl_pending(slot, update).unwrap();
        assert_eq!(storage.acl_state(slot, 0).0, PrivilegeSet::CONNECT);
        storage
            .change_acl(
                object,
                BOOTSTRAP_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::DATABASE_ALL,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 0 },
            )
            .unwrap();
        storage
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::NONE,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 9 },
            )
            .unwrap();
        storage.commit_acl(slot, 9);
        assert!(storage.object_acl_defined(object, 0));
        assert!(!storage.acl_entry_visible(slot, 0));
        storage
            .change_acl(
                object,
                PUBLIC_ROLE,
                BOOTSTRAP_ROLE,
                PrivilegeSet::CONNECT,
                PrivilegeSet::NONE,
                ObjectAclWrite::Change { transaction: 10 },
            )
            .unwrap();
        assert!(storage.object_acl_order(slot, 10).unwrap() > new_order);
        storage.commit_acl(slot, 10);
        for transaction in 11..43 {
            let (removed, _) = storage
                .change_acl(
                    object,
                    PUBLIC_ROLE,
                    BOOTSTRAP_ROLE,
                    PrivilegeSet::NONE,
                    PrivilegeSet::NONE,
                    ObjectAclWrite::Change { transaction },
                )
                .unwrap();
            storage.commit_acl(removed, transaction);
            let (added, _) = storage
                .change_acl(
                    object,
                    PUBLIC_ROLE,
                    BOOTSTRAP_ROLE,
                    PrivilegeSet::CONNECT,
                    PrivilegeSet::NONE,
                    ObjectAclWrite::Change { transaction },
                )
                .unwrap();
            storage.commit_acl(added, transaction);
            assert_eq!(added, slot);
        }
        let before = storage.acl_entry_count();
        storage.acl_catalog.lock().unwrap().next_object_order = u64::MAX;
        assert_eq!(
            storage
                .change_acl(
                    unrelated,
                    PUBLIC_ROLE,
                    BOOTSTRAP_ROLE,
                    PrivilegeSet::CONNECT,
                    PrivilegeSet::NONE,
                    ObjectAclWrite::Change { transaction: 11 }
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.acl_entry_count(), before);
        assert!(!storage.object_acl_defined(unrelated, 0));
    });
}

#[test]
fn comment_catalog_private_writes_wait_and_absent_removals_use_no_capacity() {
    let mut config = test_config();
    config.max_comments = 1;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let schema = SqlName::parse("public").unwrap();
    let name = SqlName::parse("commented").unwrap();
    let absent = SqlName::parse("absent").unwrap();
    let text = Some(comment_stackstr("same text").unwrap());
    crate::mem::guard::forbid_alloc(|| {
        let (slot, _) = storage
            .set_comment(CommentClass::Relation, schema, name, 0, text, 7)
            .unwrap()
            .unwrap();
        let error = storage
            .set_comment(CommentClass::Relation, schema, name, 0, text, 8)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
        assert!(
            storage
                .set_comment(CommentClass::Relation, schema, absent, 0, None, 8)
                .unwrap()
                .is_none()
        );
        assert!(storage.commit_comment(slot, 7).is_some());
        let original_root = storage.catalog_comments(0).next().unwrap().root.unwrap();
        let (same_slot, prior) = storage
            .set_comment(CommentClass::Relation, schema, name, 0, text, 8)
            .unwrap()
            .unwrap();
        assert_eq!(same_slot, slot);
        assert!(storage.catalog_comments(8).next().unwrap().private);
        assert_eq!(
            storage.catalog_comments(8).next().unwrap().root,
            Some(original_root)
        );
        let (removed_slot, removed_prior) = storage
            .set_comment(CommentClass::Relation, schema, name, 0, None, 8)
            .unwrap()
            .unwrap();
        let (inserted_slot, inserted_prior) = storage
            .set_comment(CommentClass::Relation, schema, name, 0, text, 8)
            .unwrap()
            .unwrap();
        let inserted_root = storage.catalog_comments(8).next().unwrap().root.unwrap();
        assert_ne!(inserted_root, original_root);
        assert_eq!(
            storage.catalog_comments(0).next().unwrap().root,
            Some(original_root)
        );
        storage.restore_comment_pending(inserted_slot, inserted_prior);
        assert!(storage.catalog_comments(8).next().unwrap().text.is_none());
        storage.restore_comment_pending(removed_slot, removed_prior);
        assert_eq!(
            storage.catalog_comments(8).next().unwrap().root,
            Some(original_root)
        );
        storage.restore_comment_pending(same_slot, prior);
        assert!(!storage.catalog_comments(8).next().unwrap().private);
        let (slot, _) = storage
            .set_comment(CommentClass::Relation, schema, name, 0, None, 9)
            .unwrap()
            .unwrap();
        assert!(storage.catalog_comments(9).next().unwrap().private);
        storage.commit_comment(slot, 9).unwrap();
        assert!(
            storage
                .set_comment(CommentClass::Relation, schema, name, 0, None, 10)
                .unwrap()
                .is_none()
        );
        assert_eq!(storage.catalog_comments(10).count(), 0);
        storage.comment_catalog().next_root = core::num::NonZeroU64::new(u64::MAX).unwrap();
        let exhausted = storage
            .set_comment(CommentClass::Relation, schema, name, 0, text, 10)
            .unwrap_err();
        assert_eq!(exhausted.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            exhausted.message.as_str(),
            "comment tuple identity exhausted"
        );
        assert_eq!(storage.catalog_comments(10).count(), 0);
    });
}

#[test]
fn comment_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<CommentCatalog>>();
    assert_send_sync::<LiveCommentIter<'_>>();
    assert_send_sync::<CheckpointCommentIter<'_>>();
    assert_send_sync::<CatalogCommentIter<'_>>();

    const WORKERS: usize = 4;
    let names = ["comment_a", "comment_b", "comment_c", "comment_d"];
    let texts = ["text a", "text b", "text c", "text d"];
    let mut config = test_config();
    config.max_comments = WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let txid = worker as u32 + 1;
                let (slot, prior) = storage
                    .set_comment(
                        CommentClass::Schema,
                        SqlName::EMPTY,
                        SqlName::parse(names[worker]).unwrap(),
                        0,
                        Some(comment_stackstr(texts[worker]).unwrap()),
                        txid,
                    )
                    .unwrap()
                    .unwrap();
                assert!(prior.is_none());
                assert!(storage.commit_comment(slot, txid).is_some());
            });
        }
    });

    assert_eq!(storage.comment_capacity(), WORKERS);
    assert_eq!(storage.live_comments().count(), WORKERS);
    assert!(
        storage
            .set_comment(
                CommentClass::Schema,
                SqlName::EMPTY,
                SqlName::parse("comment_e").unwrap(),
                0,
                Some(comment_stackstr("text e").unwrap()),
                5,
            )
            .is_err()
    );

    let mut seen = 0;
    for (class, schema, name, subid, text) in storage.comments_visible(0) {
        assert_eq!(class, CommentClass::Schema);
        assert_eq!(schema, SqlName::EMPTY);
        assert_eq!(subid, 0);
        assert_eq!(
            storage
                .comment_text(class, schema.as_str(), name.as_str(), subid, 0)
                .unwrap()
                .as_str(),
            text.as_str()
        );
        seen += 1;
    }
    assert_eq!(seen, WORKERS);
}

#[test]
fn catalog_graph_scratch_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<FixedVec<bool>>>();
    assert_send_sync::<Mutex<FixedVec<u8>>>();

    let mut config = test_config();
    config.max_domains = 4;
    config.max_roles = 4;
    config.query_workspace_slots = 2;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let roles = &storage.role_graph_scratch[0];
    let domains = &storage.domain_graph_scratch;
    std::thread::scope(|scope| {
        for worker in 0..4 {
            scope.spawn(move || {
                for _ in 0..1_000 {
                    roles.lock().expect("role graph scratch lock poisoned")[worker] = true;
                    let mut domains = domains.lock().expect("domain graph scratch lock poisoned");
                    domains[worker] = domains[worker].wrapping_add(1);
                }
            });
        }
    });
    let roles = storage.role_graph_scratch[0]
        .lock()
        .expect("role graph scratch lock poisoned");
    assert_eq!(roles.capacity(), config.max_roles);
    assert!(roles.iter().all(|visited| *visited));
    drop(roles);
    let domains = storage
        .domain_graph_scratch
        .lock()
        .expect("domain graph scratch lock poisoned");
    assert_eq!(domains.capacity(), config.max_domains);
    assert!(domains.iter().all(|visits| *visits == 232));
}

#[test]
fn reader_scratch_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CollationRuntime>();
    assert_send_sync::<SpillReader>();

    let config = test_config();
    let mut collation_budget = Budget::new(2 * config.collation_scratch_bytes);
    let collation = CollationRuntime::new(&config, &mut collation_budget).unwrap();
    assert_eq!(collation_budget.used(), 2 * config.collation_scratch_bytes);
    assert_eq!(
        collation
            .scratch
            .lock()
            .expect("collation scratch lock poisoned")
            .left
            .capacity(),
        config.collation_scratch_bytes
    );
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..1_000 {
                    assert_eq!(
                        collation.compare("alpha", "beta").unwrap(),
                        core::cmp::Ordering::Less
                    );
                }
            });
        }
    });

    let max_spill_generations = 1;
    let expected_budget = SpillReader::budget_bytes(false, max_spill_generations);
    let mut spill_budget = Budget::new(expected_budget);
    let spill = SpillReader::new(&mut spill_budget, max_spill_generations, None, None).unwrap();
    assert_eq!(spill_budget.used(), expected_budget);
    assert_eq!(spill.scratch.len(), 2);
    assert_eq!(spill.scan_contexts.len(), SCAN_CONTEXTS);
    assert_eq!(spill.cursor_contexts.len(), MAX_ROW_WALK_NESTING);
    std::thread::scope(|scope| {
        for worker in 0..4 {
            let spill = &spill;
            scope.spawn(move || {
                for _ in 0..1_000 {
                    let mut scratch = spill.scratch[worker % 2]
                        .lock()
                        .expect("spill row scratch lock poisoned");
                    scratch.index_buf[worker] = scratch.index_buf[worker].wrapping_add(1);
                    drop(scratch);
                    let mut context = spill.scan_contexts[0]
                        .lock()
                        .expect("spill scan context lock poisoned");
                    let count = u64::from_le_bytes(context.index_buf[..8].try_into().unwrap());
                    context.index_buf[..8].copy_from_slice(&(count + 1).to_le_bytes());
                    drop(context);
                    spill.cursor_contexts[worker]
                        .lock()
                        .expect("spill cursor lock poisoned")[0]
                        .ordinal += 1;
                    spill.next_walk_id().unwrap();
                }
            });
        }
    });
    assert_eq!(
        *spill
            .next_walk_id
            .lock()
            .expect("spill walk identifier lock poisoned"),
        4_001
    );
    assert_eq!(
        u64::from_le_bytes(
            spill.scan_contexts[0]
                .lock()
                .expect("spill scan context lock poisoned")
                .index_buf[..8]
                .try_into()
                .unwrap()
        ),
        4_000
    );
    for worker in 0..4 {
        assert_eq!(
            spill.scratch[worker % 2]
                .lock()
                .expect("spill row scratch lock poisoned")
                .index_buf[worker],
            232
        );
        assert_eq!(
            spill.cursor_contexts[worker]
                .lock()
                .expect("spill cursor lock poisoned")[0]
                .ordinal,
            1_000
        );
    }
}

#[test]
fn atomic_block_reads_scan_images_resume_without_any_backing_store() {
    let mut budget = Budget::new(SpillReader::budget_bytes(false, 2));
    let reader = SpillReader::new(&mut budget, 2, None, None).unwrap();
    let handle = |byte| crate::store::SstHandle {
        index: crate::store::BlockId([byte; 32]),
        filter: crate::store::BlockId([byte + 1; 32]),
        roster: crate::store::BlockId([byte + 2; 32]),
        format: crate::store::RowSstFormat::DirectV2,
    };
    let first = handle(1);
    let second = handle(4);
    let first_handles = [first];
    let second_handles = [second];
    fn relation(handles: &[crate::store::SstHandle]) -> SpillRelation<'_> {
        const RANGES: [spill_generations::SpillGenerationRange; 1] =
            [spill_generations::SpillGenerationRange::single(
                SpillGenerationId::fixture(0),
            )];
        SpillRelation {
            persistence: RelationPersistence::Permanent,
            schema: [ColType::Bool; MAX_COLUMNS],
            n_columns: 0,
            handles,
            ranges: &RANGES,
        }
    }
    let first_relation = relation(&first_handles);
    let second_relation = relation(&second_handles);
    let mut context = reader.scan_contexts[0].lock().unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for (handle, rowid, lsn, value) in [(first, 1u64, 7u64, b"one"), (second, 2, 9, b"two")] {
            let slot = context.reserve_image(ScanBlockImage {
                handle,
                ordinal: 0,
                reference: crate::store::DataBlockRef::Direct(handle.index),
                successor: ScanSuccessor::End,
                bytes: Some(ScanBlockBytes {
                    raw_len: 23,
                    decoded_len: 23,
                    kind: crate::store::BlockType::SstDataV2,
                }),
            });
            let bytes = &mut context.member_blocks[slot][..23];
            bytes[..8].copy_from_slice(&rowid.to_le_bytes());
            bytes[8..16].copy_from_slice(&lsn.to_le_bytes());
            bytes[16..20].copy_from_slice(&3u32.to_le_bytes());
            bytes[20..].copy_from_slice(value);
        }
        let mut first_cursor = MemberCursor::EMPTY;
        Storage::cursor_advance(&reader, &first_relation, 0, &mut first_cursor, &mut context)
            .unwrap();
        assert_eq!(
            first_cursor.head,
            Some((crate::store::SstKey::at(1, 7), false, 3))
        );
        let mut nested = MemberCursor::EMPTY;

        Storage::cursor_advance(&reader, &second_relation, 0, &mut nested, &mut context).unwrap();
        assert_eq!(
            nested.head,
            Some((crate::store::SstKey::at(2, 9), false, 3))
        );
        // Nested sources keep this cursor's next byte offset.

        Storage::cursor_advance(&reader, &first_relation, 0, &mut first_cursor, &mut context)
            .unwrap();
        assert!(first_cursor.done);
        assert!(first_cursor.head.is_none());
        let mut rebuilt = MemberCursor::EMPTY;
        Storage::cursor_advance(&reader, &first_relation, 0, &mut rebuilt, &mut context).unwrap();
        assert_eq!(
            rebuilt.head,
            Some((crate::store::SstKey::at(1, 7), false, 3))
        );
        // A wait on the next block must not reuse the previous block's
        // end offset when its frame has since changed.
        let mut next = context.member_images[context.find_image(first, 0).unwrap()].unwrap();
        next.ordinal = 1;
        next.reference = crate::store::DataBlockRef::Direct(crate::store::BlockId([9; 32]));
        let slot = context.reserve_image(next);
        let bytes = &mut context.member_blocks[slot][..23];
        bytes[..8].copy_from_slice(&3u64.to_le_bytes());
        bytes[8..16].copy_from_slice(&11u64.to_le_bytes());
        bytes[16..20].copy_from_slice(&3u32.to_le_bytes());
        bytes[20..].copy_from_slice(b"end");
        rebuilt.ordinal = 1;
        Storage::cursor_advance(&reader, &first_relation, 0, &mut rebuilt, &mut context).unwrap();
        assert_eq!(
            rebuilt.head,
            Some((crate::store::SstKey::at(3, 11), false, 3))
        );
    });
    assert_eq!(budget.used(), SpillReader::budget_bytes(false, 2));
}

#[test]
fn atomic_block_reads_scan_frame_reuse_invalidates_cursor_and_column_claims() {
    let mut context = ScanContext::new(2);
    let image = |byte| {
        let handle = crate::store::SstHandle {
            index: crate::store::BlockId([byte; 32]),
            filter: crate::store::BlockId([byte + 1; 32]),
            roster: crate::store::BlockId([byte + 2; 32]),
            format: crate::store::RowSstFormat::CURRENT,
        };
        ScanBlockImage {
            handle,
            ordinal: 0,
            reference: crate::store::DataBlockRef::Direct(handle.index),
            successor: ScanSuccessor::End,
            bytes: Some(ScanBlockBytes {
                raw_len: 1,
                decoded_len: 0,
                kind: crate::store::BlockType::SstDataPaxV2,
            }),
        }
    };
    crate::mem::guard::forbid_alloc(|| {
        let first = image(1);
        let second = image(4);
        let first_slot = context.reserve_image(first);
        let second_slot = context.reserve_image(second);
        context.member_blocks[first_slot][0] = 11;
        context.member_blocks[second_slot][0] = 22;
        context.member_extents[first_slot][0] = Some((0, 1));
        context.member_extents[second_slot][1] = Some((0, 1));
        let first_cursor = MemberCursor {
            loaded: Some(0),
            buffer_slot: Some(first_slot),
            ..MemberCursor::EMPTY
        };
        let second_cursor = MemberCursor {
            loaded: Some(0),
            buffer_slot: Some(second_slot),
            ..MemberCursor::EMPTY
        };
        assert!(context.cursor_loaded(first.handle, &first_cursor));
        assert!(context.cursor_loaded(second.handle, &second_cursor));
        let mut pending = image(7);
        pending.bytes = None;
        let replacement = context.reserve_image(pending);
        assert_eq!(replacement, first_slot);
        assert!(!context.cursor_loaded(first.handle, &first_cursor));
        assert!(!context.cursor_loaded(pending.handle, &first_cursor));
        assert!(
            context.member_extents[replacement]
                .iter()
                .all(Option::is_none)
        );
        assert!(context.cursor_loaded(second.handle, &second_cursor));
        assert_eq!(context.member_extents[second_slot][1], Some((0, 1)));
        assert_eq!(context.member_blocks[second_slot][0], 22);
    });
}

#[test]
fn atomic_block_reads_scratch_reuses_two_sources_and_preserves_live_leases() {
    let mut budget = Budget::new(SpillReader::budget_bytes(false, 1));
    let reader = SpillReader::new(&mut budget, 1, None, None).unwrap();
    let first = Some(crate::store::BlockId([1; 32]));
    let second = Some(crate::store::BlockId([2; 32]));
    crate::mem::guard::forbid_alloc(|| {
        let mut lease = try_source_scratch(&reader.scratch, "row scratch", first).unwrap();
        lease.index_buf[0] = 11;
        drop(lease);
        let mut lease = try_source_scratch(&reader.scratch, "row scratch", second).unwrap();
        lease.index_buf[0] = 22;
        drop(lease);
        let first_lease = try_source_scratch(&reader.scratch, "row scratch", first).unwrap();
        assert_eq!(first_lease.index_buf[0], 11);
        let second_lease = try_source_scratch(&reader.scratch, "row scratch", second).unwrap();
        assert_eq!(second_lease.index_buf[0], 22);
        assert!(try_source_scratch(&reader.scratch, "row scratch", first).is_none());
        assert!(try_source_scratch(&reader.scratch, "row scratch", None).is_none());
        drop(first_lease);
        let mut available = try_source_scratch(&reader.scratch, "row scratch", None).unwrap();
        available.index_buf[0] = 33;
        assert_eq!(second_lease.index_buf[0], 22);
    });
    assert_eq!(budget.used(), SpillReader::budget_bytes(false, 1));
}

#[test]
fn spill_walk_identity_exhaustion_preserves_pending_scan_images() {
    let mut budget = Budget::new(SpillReader::budget_bytes(false, 1));
    let spill = SpillReader::new(&mut budget, 1, None, None).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        *spill.next_walk_id.lock().unwrap() = u64::MAX - 1;
        assert_eq!(spill.next_walk_id().unwrap(), u64::MAX - 1);
        let handle = crate::store::SstHandle {
            index: crate::store::BlockId([1; 32]),
            filter: crate::store::BlockId([2; 32]),
            roster: crate::store::BlockId([3; 32]),
            format: crate::store::RowSstFormat::CURRENT,
        };
        let mut context = spill.scan_contexts[0].lock().unwrap();
        let slot = context.reserve_image(ScanBlockImage {
            handle,
            ordinal: 0,
            reference: crate::store::DataBlockRef::Direct(handle.index),
            successor: ScanSuccessor::End,
            bytes: None,
        });
        context.member_blocks[slot][0] = 1;
        drop(context);
        let error = spill.next_walk_id().unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(*spill.next_walk_id.lock().unwrap(), u64::MAX);
        let context = spill.scan_contexts[0].lock().unwrap();
        assert_eq!(context.find_image(handle, 0), Some(slot));
        assert!(context.member_images[slot].unwrap().bytes.is_none());
        assert_eq!(context.member_blocks[slot][0], 1);
    });
}

#[test]
fn index_expression_scratch_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Mutex<Arena>>();
    assert_send_sync::<Storage>();

    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    std::thread::scope(|scope| {
        for worker in 0..4u8 {
            let arena = &storage.index_arena;
            scope.spawn(move || {
                for _ in 0..1_000 {
                    let arena = arena.lock().expect("index expression arena lock poisoned");
                    let mark = arena.mark();
                    let bytes = arena
                        .alloc_slice_copy(&[worker; 16])
                        .expect("index expression scratch capacity");
                    assert!(bytes.iter().all(|byte| *byte == worker));
                    // SAFETY: this worker retains the arena lock and no
                    // allocation above the mark escapes the iteration.
                    unsafe { arena.rewind_to(mark) };
                }
            });
        }
    });
    let arena = storage
        .index_arena
        .lock()
        .expect("index expression arena lock poisoned");
    assert_eq!(arena.capacity(), INDEX_ARENA_BYTES);
    assert_eq!(arena.used(), 0);
    assert!(arena.high_water() >= 16);
}

#[test]
fn snapshot_state_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SnapshotState>();

    let mut config = test_config();
    config.max_connections = 4;
    let transaction_capacity = config.max_connections as usize + config.max_prepared_transactions;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let snapshots = &storage.snapshots;
    std::thread::scope(|scope| {
        for worker in 0..4u32 {
            scope.spawn(move || {
                let txid = 100 + worker;
                for value in 0..1_000u64 {
                    let mut snapshots = snapshots.lock().expect("snapshot state lock poisoned");
                    if value == 0 {
                        snapshots.active.push((txid, value)).unwrap();
                        snapshots
                            .serializable
                            .push((txid, 0, value, false))
                            .unwrap();
                    } else {
                        snapshots
                            .active
                            .iter_mut()
                            .find(|(owner, _)| *owner == txid)
                            .unwrap()
                            .1 = value;
                        let serializable = snapshots
                            .serializable
                            .iter_mut()
                            .find(|entry| entry.0 == txid)
                            .unwrap();
                        serializable.2 = value;
                        serializable.3 = value % 2 != 0;
                    }
                }
            });
        }
    });
    let snapshots = storage.snapshot_state();
    assert_eq!(snapshots.active.capacity(), transaction_capacity);
    assert_eq!(snapshots.active.len(), 4);
    assert!(
        snapshots
            .active
            .iter()
            .all(|(_, snapshot)| *snapshot == 999)
    );
    assert_eq!(
        snapshots.serializable.capacity(),
        transaction_capacity * table_slot_capacity(&config)
    );
    assert_eq!(snapshots.serializable.len(), 4);
    assert!(
        snapshots
            .serializable
            .iter()
            .all(|entry| entry.2 == 999 && entry.3)
    );
}

#[test]
fn brin_maintenance_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BrinMaintenance>();

    let mut config = test_config();
    config.max_brin_unsummarized_ranges_per_index = 4;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let maintenance = &storage.brin_maintenance;
    std::thread::scope(|scope| {
        for worker in 0..4 {
            scope.spawn(move || {
                for _ in 0..1_000 {
                    let mut maintenance =
                        maintenance.lock().expect("BRIN maintenance lock poisoned");
                    maintenance.states[0].summarized_until_page += 1;
                    maintenance.unsummarized_ranges[worker] += 1;
                }
            });
        }
    });
    let maintenance = storage.brin_maintenance();
    assert_eq!(maintenance.states.capacity(), config.max_indexes);
    assert_eq!(maintenance.states.len(), config.max_indexes);
    assert_eq!(
        maintenance.unsummarized_ranges.capacity(),
        config.max_indexes * config.max_brin_unsummarized_ranges_per_index
    );
    assert_eq!(maintenance.states[0].summarized_until_page, 4_000);
    assert!(
        maintenance.unsummarized_ranges[..4]
            .iter()
            .all(|updates| *updates == 1_000)
    );
}

#[test]
fn sequence_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SequenceDef>();
    assert_send_sync::<SequenceValues>();
    assert_send_sync::<Mutex<SequenceCatalog>>();
    assert_send_sync::<SequenceIter<'_>>();

    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let sequence = storage
        .create_sequence(
            SequenceCreateSpec {
                allocation: crate::storage::CatalogAllocation::New,
                schema: SqlName::parse("public").unwrap(),
                name: SqlName::parse("concurrent_sequence").unwrap(),
                spec: SeqSpec {
                    data_type: SeqType::Bigint,
                    increment: 1,
                    min_value: 1,
                    max_value: i64::MAX,
                    start_value: 1,
                    cache: 1,
                    cycle: false,
                },
                owner: None,
                generator_for: None,
                persistence: RelationPersistence::Permanent,
            },
            0,
        )
        .unwrap();
    storage.finish_sequence_change(sequence, CatalogDdlDecision::Commit);
    assert_eq!(sequence.slot(), 0);
    let catalog = &storage.sequence_catalog;
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(move || {
                for _ in 0..1_000 {
                    let mut catalog = catalog.lock().expect("sequence catalog lock poisoned");
                    let sequence = catalog.definitions[0];
                    sequence
                        .reserve_values_with(&mut catalog.values[0].committed, 1)
                        .unwrap();
                }
            });
        }
    });
    let catalog = storage.sequence_catalog();
    assert_eq!(catalog.definitions.capacity(), config.max_sequences);
    assert_eq!(catalog.definitions.len(), config.max_sequences);
    assert_eq!(catalog.values.capacity(), config.max_sequences);
    assert_eq!(catalog.values.len(), config.max_sequences);
    assert_eq!(catalog.values[0].committed.last_value, 4_000);
    assert!(catalog.values[0].committed.is_called);
    assert!(catalog.values[0].committed.dirty);
    assert_eq!(catalog.values[0].committed.generation, 4_000);
    drop(catalog);
    storage.mark_sequence_value_staged(0, 0, 4_000);
    storage.reserve_sequence_values(0, 0, 1).unwrap();
    storage.clear_sequence_value_dirty(0, 0);
    assert!(storage.sequence_value_image_for(0, 0).3);
    assert_eq!(storage.sequence_value_for(0, 0).0, 4_001);
    storage.mark_sequence_value_staged(0, 0, 4_001);
    storage.clear_sequence_value_dirty(0, 0);
    assert!(!storage.sequence_value_image_for(0, 0).3);
}

#[test]
fn tablespace_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TablespaceDef>();
    assert_send_sync::<Mutex<FixedVec<TablespaceDef>>>();
    assert_send_sync::<TablespaceIter<'_>>();

    const WORKERS: usize = 4;
    let names = ["space_a", "space_b", "space_c", "space_d"];
    let mut config = test_config();
    config.max_tablespaces = 2 + WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for (worker, worker_name) in names.iter().copied().enumerate() {
            let storage = &storage;
            scope.spawn(move || {
                let txid = worker as u32 + 1;
                let slot = storage
                    .create_tablespace(
                        TablespaceCreation::Fresh,
                        SqlName::parse(worker_name).unwrap(),
                        StackStr::from_str("/data"),
                        TablespaceOptions::DEFAULT,
                        BOOTSTRAP_ROLE,
                        txid,
                    )
                    .unwrap();
                storage.commit_tablespace_create(slot);
            });
        }
    });

    assert_eq!(storage.tablespace_capacity(), 2 + WORKERS);
    assert_eq!(storage.tablespaces_visible_to(0).count(), 2 + WORKERS);
    for (slot, tablespace) in storage.tablespaces_visible_to(0) {
        let nested = storage.tablespace(slot);
        assert_eq!(nested.created_at, tablespace.created_at);
        assert_eq!(nested.name, tablespace.name);
    }
    let created_at: [u64; WORKERS] = std::array::from_fn(|worker| {
        let slot = storage.tablespace_slot(names[worker], 0).unwrap();
        storage.tablespace(slot).created_at
    });
    for left in 0..WORKERS {
        assert_ne!(created_at[left], 0);
        for right in left + 1..WORKERS {
            assert_ne!(created_at[left], created_at[right]);
        }
    }
    assert_eq!(
        storage
            .create_tablespace(
                TablespaceCreation::Fresh,
                SqlName::parse("space_full").unwrap(),
                StackStr::from_str("/data"),
                TablespaceOptions::DEFAULT,
                BOOTSTRAP_ROLE,
                9,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );

    let reclaimed = storage.tablespace_slot(names[0], 0).unwrap();
    storage.drop_tablespace(reclaimed, 10).unwrap();
    storage.commit_tablespace_drop(reclaimed);
    assert_eq!(storage.tablespace(reclaimed).created_at, 0);
    assert_eq!(storage.tablespace(reclaimed).name, SqlName::EMPTY);
    assert!(storage.tablespace(reclaimed).location.as_str().is_empty());
    assert_eq!(
        storage.tablespace(reclaimed).ddl_state,
        CatalogDdlState::Absent
    );

    let reused = storage
        .create_tablespace(
            TablespaceCreation::Fresh,
            SqlName::parse("space_reused").unwrap(),
            StackStr::from_str("/reused"),
            TablespaceOptions::DEFAULT,
            BOOTSTRAP_ROLE,
            11,
        )
        .unwrap();
    assert_eq!(reused, reclaimed);
    storage.rollback_tablespace_create(reused);
    assert_eq!(storage.tablespace(reused).created_at, 0);
    assert!(storage.tablespace(reused).ownership.pending.is_none());

    let pending = storage
        .create_tablespace(
            TablespaceCreation::Fresh,
            SqlName::parse("space_pending").unwrap(),
            StackStr::from_str("/pending"),
            TablespaceOptions::DEFAULT,
            BOOTSTRAP_ROLE,
            20,
        )
        .unwrap();
    assert_eq!(
        storage
            .create_tablespace(
                TablespaceCreation::Fresh,
                SqlName::parse("space_pending").unwrap(),
                StackStr::from_str("/other"),
                TablespaceOptions::DEFAULT,
                BOOTSTRAP_ROLE,
                21,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(21);
    storage.rollback_tablespace_create(pending);

    let altered = storage.tablespace_slot(names[1], 0).unwrap();
    let other = storage.tablespace_slot(names[2], 0).unwrap();
    let renamed = SqlName::parse("space_renamed").unwrap();
    let prior = storage
        .alter_tablespace_definition(altered, renamed, TablespaceOptions::DEFAULT, 30)
        .unwrap();
    assert_eq!(
        storage
            .alter_tablespace_definition(other, renamed, TablespaceOptions::DEFAULT, 31)
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(31);
    assert_eq!(
        storage.drop_tablespace(altered, 32).unwrap_err().sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(32);
    storage.rollback_tablespace_alter(altered, prior);

    let prior_owner = storage
        .set_tablespace_owner(altered, usize::from(BOOTSTRAP_ROLE), 40)
        .unwrap();
    assert_eq!(
        storage
            .set_tablespace_owner(altered, usize::from(BOOTSTRAP_ROLE), 41)
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(41);
    storage.restore_object_owner(
        AccessObject {
            class: AccessClass::Tablespace,
            slot: altered as u16,
        },
        prior_owner,
    );
}

#[test]
fn schema_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SchemaDef>();
    assert_send_sync::<Mutex<FixedVec<SchemaDef>>>();
    assert_send_sync::<SchemaIter<'_>>();

    const WORKERS: usize = 4;
    let names = ["schema_a", "schema_b", "schema_c", "schema_d"];
    let mut config = test_config();
    config.max_schemas = 3 + WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker_name in names.iter().copied() {
            let storage = &storage;
            scope.spawn(move || {
                storage
                    .create_schema(SqlName::parse(worker_name).unwrap())
                    .unwrap();
            });
        }
    });

    assert_eq!(storage.schema_count(), 3 + WORKERS);
    assert_eq!(storage.checkpoint_schemas().count(), 3 + WORKERS);
    assert_eq!(storage.live_schemas().count(), 1 + WORKERS);
    for (slot, schema) in storage.live_schemas() {
        let nested = storage.schema_def(slot);
        assert_eq!(nested.database, schema.database);
        assert_eq!(nested.name, schema.name);
    }
    assert_eq!(
        storage
            .create_schema(SqlName::parse("schema_full").unwrap())
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );

    let reclaimed = storage.find_schema(names[0]).unwrap();
    let object = AccessObject {
        class: AccessClass::Schema,
        slot: reclaimed as u16,
    };
    storage
        .change_acl(
            object,
            PUBLIC_ROLE,
            BOOTSTRAP_ROLE,
            PrivilegeSet::USAGE,
            PrivilegeSet::NONE,
            crate::storage::ObjectAclWrite::Change { transaction: 0 },
        )
        .unwrap();
    assert_eq!(
        storage.acl_from(object, PUBLIC_ROLE, BOOTSTRAP_ROLE, 0).0,
        PrivilegeSet::USAGE
    );
    storage.drop_schema(reclaimed);
    let empty = storage.schema_def(reclaimed);
    assert_eq!(empty.name, SqlName::EMPTY);
    assert_eq!(empty.database, DatabaseOid::POSTGRES);
    assert_eq!(empty.ownership, Ownership::BOOTSTRAP);
    assert_eq!(empty.ddl_state, CatalogDdlState::Absent);
    assert_eq!(
        storage.acl_from(object, PUBLIC_ROLE, BOOTSTRAP_ROLE, 0).0,
        PrivilegeSet::NONE
    );

    let reused = storage
        .alloc_schema(
            SqlName::parse("schema_reused").unwrap(),
            CatalogDdlState::PendingCreate { txid: 11 },
            Ownership {
                owner: BOOTSTRAP_ROLE,
                pending: Some(PendingOwnership {
                    txid: 11,
                    owner: BOOTSTRAP_ROLE,
                }),
            },
            schema_names::SchemaAllocation::New,
        )
        .unwrap();
    assert_eq!(reused, reclaimed);
    storage.rollback_schema_create(reused);
    let empty = storage.schema_def(reused);
    assert_eq!(empty.name, SqlName::EMPTY);
    assert!(empty.ownership.pending.is_none());

    let committed = storage
        .alloc_schema(
            SqlName::parse("schema_committed").unwrap(),
            CatalogDdlState::PendingCreate { txid: 12 },
            Ownership {
                owner: BOOTSTRAP_ROLE,
                pending: Some(PendingOwnership {
                    txid: 12,
                    owner: BOOTSTRAP_ROLE,
                }),
            },
            schema_names::SchemaAllocation::New,
        )
        .unwrap();
    storage.commit_schema_create(committed);
    let definition = storage.schema_def(committed);
    assert_eq!(definition.ddl_state, CatalogDdlState::Present);
    assert!(definition.ownership.pending.is_none());
    storage.drop_schema(committed);

    let pending = storage
        .alloc_schema(
            SqlName::parse("schema_pending").unwrap(),
            CatalogDdlState::PendingCreate { txid: 20 },
            Ownership::BOOTSTRAP,
            schema_names::SchemaAllocation::New,
        )
        .unwrap();
    assert_eq!(
        storage
            .alloc_schema(
                SqlName::parse("schema_pending").unwrap(),
                CatalogDdlState::PendingCreate { txid: 21 },
                Ownership::BOOTSTRAP,
                schema_names::SchemaAllocation::New
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(21);
    storage.rollback_schema_create(pending);

    let altered = storage.find_schema(names[1]).unwrap();
    let prior_owner = storage
        .set_schema_owner(altered, usize::from(BOOTSTRAP_ROLE), 30)
        .unwrap();
    assert_eq!(
        storage.drop_schema_in(altered, 31).unwrap_err().sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(31);
    storage.restore_object_owner(
        AccessObject {
            class: AccessClass::Schema,
            slot: altered as u16,
        },
        prior_owner,
    );
}

#[test]
fn extension_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ExtensionDef>();
    assert_send_sync::<ExtensionDependency>();
    assert_send_sync::<ExtensionConfig>();
    assert_send_sync::<Mutex<ExtensionCatalog>>();
    assert_send_sync::<ExtensionIter<'_>>();
    assert_send_sync::<ExtensionDependencyIter<'_>>();
    assert_send_sync::<ExtensionConfigIter<'_>>();

    const WORKERS: usize = 4;
    let extension_names = ["extension_a", "extension_b", "extension_c", "extension_d"];
    let sequence_names = [
        "extension_seq_a",
        "extension_seq_b",
        "extension_seq_c",
        "extension_seq_d",
    ];
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let namespace = storage.find_schema("public").unwrap();
    let sequences: [usize; WORKERS] = std::array::from_fn(|worker| {
        let slot = storage
            .create_sequence(
                SequenceCreateSpec {
                    allocation: crate::storage::CatalogAllocation::New,
                    schema: SqlName::parse("public").unwrap(),
                    name: SqlName::parse(sequence_names[worker]).unwrap(),
                    spec: SeqSpec {
                        data_type: SeqType::Bigint,
                        increment: 1,
                        min_value: 1,
                        max_value: i64::MAX,
                        start_value: 1,
                        cache: 1,
                        cycle: false,
                    },
                    owner: None,
                    generator_for: None,
                    persistence: RelationPersistence::Permanent,
                },
                0,
            )
            .unwrap();
        storage.finish_sequence_change(slot, CatalogDdlDecision::Commit);
        slot.slot()
    });

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let txid = worker as u32 + 1;
                let extension = storage
                    .create_extension(
                        SqlName::parse(extension_names[worker]).unwrap(),
                        namespace,
                        true,
                        ExtensionVersion::parse("1.0").unwrap(),
                        txid,
                    )
                    .unwrap();
                storage.commit_extension_create(extension, txid);
                let relation = ExtensionConfigRelation::Sequence(sequences[worker] as u16);
                let (dependency, _) = storage
                    .change_extension_dependency(
                        extension,
                        relation.access_object(),
                        ExtensionDependencyKind::Member,
                        true,
                        txid,
                    )
                    .unwrap();
                storage.commit_extension_dependency(dependency, txid);
                let (config, _) = storage
                    .change_extension_config(
                        extension,
                        relation,
                        extension_config_condition("last_value > 0").unwrap(),
                        true,
                        txid,
                    )
                    .unwrap();
                storage.commit_extension_config(config, txid);
            });
        }
    });

    assert_eq!(storage.extension_count(), MAX_EXTENSIONS);
    assert_eq!(storage.extensions_visible_to(0).count(), WORKERS);
    assert_eq!(storage.checkpoint_extensions().count(), WORKERS);
    assert_eq!(
        storage.extension_dependencies_visible_to(0).count(),
        WORKERS
    );
    assert_eq!(storage.extension_configs_visible_to(0).count(), WORKERS);
    for (slot, extension) in storage.extensions_visible_to(0) {
        assert_eq!(storage.extension(slot).created_at, extension.created_at);
    }
    for (slot, dependency) in storage.extension_dependencies_visible_to(0) {
        assert_eq!(storage.extension_dependency(slot).object, dependency.object);
    }
    for (slot, config) in storage.extension_configs_visible_to(0) {
        assert_eq!(storage.extension_config(slot).relation, config.relation);
    }
    let catalog = storage.extension_catalog();
    assert_eq!(catalog.definitions.capacity(), MAX_EXTENSIONS);
    assert_eq!(catalog.dependencies.capacity(), MAX_EXTENSION_DEPENDENCIES);
    assert_eq!(catalog.configs.capacity(), MAX_EXTENSION_CONFIG_RELATIONS);
    drop(catalog);

    let reclaimed = storage.extension_slot(extension_names[0], 0).unwrap();
    storage.drop_extension_in(reclaimed, 10).unwrap();
    storage.commit_extension_drop(reclaimed);
    assert_eq!(storage.extension(reclaimed).name, SqlName::EMPTY);
    assert_eq!(storage.extension(reclaimed).database, DatabaseOid::POSTGRES);
    assert_eq!(storage.extension(reclaimed).ownership, Ownership::BOOTSTRAP);
    assert_eq!(
        storage.extension(reclaimed).ddl_state,
        CatalogDdlState::Absent
    );
    assert_eq!(
        storage.extension_dependencies_visible_to(0).count(),
        WORKERS - 1
    );
    assert_eq!(storage.extension_configs_visible_to(0).count(), WORKERS - 1);

    let reused = storage
        .create_extension(
            SqlName::parse("extension_reused").unwrap(),
            namespace,
            false,
            ExtensionVersion::parse("2.0").unwrap(),
            11,
        )
        .unwrap();
    assert_eq!(reused, reclaimed);
    let relation = ExtensionConfigRelation::Sequence(sequences[0] as u16);
    let (dependency, _) = storage
        .change_extension_dependency(
            reused,
            relation.access_object(),
            ExtensionDependencyKind::Member,
            true,
            11,
        )
        .unwrap();
    storage.commit_extension_dependency(dependency, 11);
    let (config_slot, _) = storage
        .change_extension_config(reused, relation, ExtensionConfigCondition::new(), true, 11)
        .unwrap();
    storage.commit_extension_config(config_slot, 11);
    storage.rollback_extension_create(reused);
    assert_eq!(storage.extension(reused).created_at, 0);
    assert!(storage.extension(reused).ownership.pending.is_none());
    assert_eq!(
        storage.extension_dependencies_visible_to(0).count(),
        WORKERS - 1
    );
    assert_eq!(storage.extension_configs_visible_to(0).count(), WORKERS - 1);

    let pending = storage
        .create_extension(
            SqlName::parse("extension_pending").unwrap(),
            namespace,
            false,
            ExtensionVersion::parse("1.0").unwrap(),
            20,
        )
        .unwrap();
    assert_eq!(
        storage
            .create_extension(
                SqlName::parse("extension_pending").unwrap(),
                namespace,
                false,
                ExtensionVersion::parse("1.0").unwrap(),
                21,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(21);
    storage.rollback_extension_create(pending);

    let altered = storage.extension_slot(extension_names[1], 0).unwrap();
    let prior = storage
        .alter_extension_definition(
            altered,
            namespace,
            true,
            ExtensionVersion::parse("1.1").unwrap(),
            30,
        )
        .unwrap();
    assert_eq!(
        storage
            .set_extension_owner(altered, usize::from(BOOTSTRAP_ROLE), 31)
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(31);
    storage.rollback_extension_alter(altered, prior);
    let prior_owner = storage
        .set_extension_owner(altered, usize::from(BOOTSTRAP_ROLE), 32)
        .unwrap();
    assert_eq!(
        storage.drop_extension_in(altered, 33).unwrap_err().sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.release_row_locks(33);
    storage.restore_object_owner(
        AccessObject {
            class: AccessClass::Extension,
            slot: altered as u16,
        },
        prior_owner,
    );

    let mut fill = 0usize;
    while storage.checkpoint_extensions().count() < MAX_EXTENSIONS {
        let name = crate::stack_format!(63, "extension_fill_{}", fill);
        let slot = storage
            .create_extension(
                SqlName::parse(name.as_str()).unwrap(),
                namespace,
                false,
                ExtensionVersion::parse("1.0").unwrap(),
                0,
            )
            .unwrap();
        storage.commit_extension_create(slot, 0);
        fill += 1;
    }
    assert_eq!(
        storage
            .create_extension(
                SqlName::parse("extension_full").unwrap(),
                namespace,
                false,
                ExtensionVersion::parse("1.0").unwrap(),
                0,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );
}

#[test]
fn foreign_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<foreign::ForeignCatalog>();
    assert_send_sync::<Mutex<foreign::ForeignCatalog>>();
    assert_send_sync::<ForeignCatalogIter<'_, foreign::ForeignDataWrapperDefinition>>();

    const WORKERS: usize = 4;
    let wrapper_names = ["wrapper_a", "wrapper_b", "wrapper_c", "wrapper_d"];
    let server_names = ["server_a", "server_b", "server_c", "server_d"];
    let mut config = test_config();
    config.max_foreign_data_wrappers = WORKERS;
    config.max_foreign_servers = WORKERS;
    config.max_user_mappings = WORKERS;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let txid = worker as u32 + 1;
                let wrapper = storage
                    .create_foreign_wrapper(
                        foreign::ForeignDataWrapperDefinition {
                            name: SqlName::parse(wrapper_names[worker]).unwrap(),
                            handler: foreign::ForeignDataHandler::None,
                            validator: foreign::ForeignDataValidator::None,
                            options: foreign::ForeignOptions::EMPTY,
                        },
                        txid,
                    )
                    .unwrap();
                storage
                    .foreign_catalog_commit_create(foreign::ForeignObjectClass::Wrapper, wrapper);
                let server = storage
                    .create_foreign_server(
                        foreign::ForeignServerDefinition {
                            name: SqlName::parse(server_names[worker]).unwrap(),
                            wrapper: wrapper as u16,
                            server_type: None,
                            version: None,
                            options: foreign::ForeignOptions::EMPTY,
                        },
                        txid,
                    )
                    .unwrap();
                storage.foreign_catalog_commit_create(foreign::ForeignObjectClass::Server, server);
                let mapping = storage
                    .create_foreign_user_mapping(
                        foreign::UserMappingDefinition {
                            server: server as u16,
                            user: foreign::ForeignMappingUser::Public,
                            options: foreign::ForeignOptions::EMPTY,
                        },
                        txid,
                    )
                    .unwrap();
                storage
                    .foreign_catalog_commit_create(foreign::ForeignObjectClass::Mapping, mapping);
                let table = storage
                    .create_foreign_table_binding(
                        foreign::ForeignTableDefinition {
                            table: worker as u16,
                            server: server as u16,
                            options: foreign::ForeignOptions::EMPTY,
                            column_options: foreign::ForeignColumnOptions::EMPTY,
                        },
                        txid,
                    )
                    .unwrap();
                storage.foreign_catalog_commit_create(foreign::ForeignObjectClass::Table, table);
            });
        }
    });

    assert_eq!(storage.foreign_wrappers(0).count(), WORKERS);
    assert_eq!(storage.foreign_servers(0).count(), WORKERS);
    assert_eq!(storage.foreign_user_mappings(0).count(), WORKERS);
    assert_eq!(storage.foreign_tables(0).count(), WORKERS);
    assert_eq!(storage.checkpoint_foreign_wrappers().count(), WORKERS);
    assert_eq!(storage.checkpoint_foreign_servers().count(), WORKERS);
    assert_eq!(
        storage
            .role_object_dependency(usize::from(BOOTSTRAP_ROLE), 0)
            .unwrap(),
        Some(RoleObjectDependency::OwnedObject)
    );
    for (slot, entry) in storage.foreign_wrappers(0) {
        assert_eq!(
            storage.foreign_wrapper_entry(slot).created_at,
            entry.created_at
        );
    }
    let catalog = storage.foreign_catalog();
    assert_eq!(catalog.wrapper_capacity(), config.max_foreign_data_wrappers);
    assert_eq!(catalog.server_capacity(), config.max_foreign_servers);
    assert_eq!(catalog.mapping_capacity(), config.max_user_mappings);
    assert_eq!(catalog.table_capacity(), config.max_tables);
    drop(catalog);

    let wrapper = storage.foreign_wrapper(wrapper_names[0], 0).unwrap().0;
    let server = storage.foreign_server(server_names[0], 0).unwrap().0;
    let mapping = storage
        .foreign_user_mapping(server as u16, foreign::ForeignMappingUser::Public, 0)
        .unwrap()
        .0;
    let table = storage.foreign_table(0, 0).unwrap().0;
    for (class, slot) in [
        (foreign::ForeignObjectClass::Table, table),
        (foreign::ForeignObjectClass::Mapping, mapping),
        (foreign::ForeignObjectClass::Server, server),
        (foreign::ForeignObjectClass::Wrapper, wrapper),
    ] {
        storage.foreign_catalog_drop(class, slot, 10);
        storage.foreign_catalog_commit_drop(class, slot);
    }
    let empty = storage.foreign_wrapper_entry(wrapper);
    assert_eq!(empty.database, DatabaseOid::POSTGRES);
    assert_eq!(empty.created_at, 0);
    assert_eq!(empty.definition.name, SqlName::EMPTY);
    assert_eq!(empty.ownership, Ownership::BOOTSTRAP);
    assert_eq!(empty.ddl_state, CatalogDdlState::Absent);
    let empty = storage.foreign_server_entry(server);
    assert_eq!(empty.created_at, 0);
    assert_eq!(empty.definition.name, SqlName::EMPTY);
    assert_eq!(empty.ownership, Ownership::BOOTSTRAP);
    assert_eq!(empty.ddl_state, CatalogDdlState::Absent);
    let empty = storage.foreign_mapping_entry(mapping);
    assert_eq!(empty.created_at, 0);
    assert_eq!(empty.definition.server, u16::MAX);
    assert_eq!(empty.ddl_state, CatalogDdlState::Absent);
    let empty = storage.foreign_table_entry(table);
    assert_eq!(empty.created_at, 0);
    assert_eq!(empty.definition.table, u16::MAX);
    assert_eq!(empty.ddl_state, CatalogDdlState::Absent);

    let dropped = storage.foreign_wrapper(wrapper_names[1], 0).unwrap().0;
    storage.foreign_catalog_drop(foreign::ForeignObjectClass::Wrapper, dropped, 11);
    let replacement = storage
        .create_foreign_wrapper(
            foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse(wrapper_names[1]).unwrap(),
                handler: foreign::ForeignDataHandler::None,
                validator: foreign::ForeignDataValidator::None,
                options: foreign::ForeignOptions::EMPTY,
            },
            11,
        )
        .unwrap();
    assert_eq!(replacement, wrapper);
    storage.foreign_catalog_rollback_create(foreign::ForeignObjectClass::Wrapper, replacement);
    storage.foreign_catalog_rollback_drop(foreign::ForeignObjectClass::Wrapper, dropped, 11);

    let reused = storage
        .create_foreign_wrapper(
            foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("wrapper_reused").unwrap(),
                handler: foreign::ForeignDataHandler::None,
                validator: foreign::ForeignDataValidator::None,
                options: foreign::ForeignOptions::EMPTY,
            },
            11,
        )
        .unwrap();
    assert_eq!(reused, wrapper);
    storage.foreign_catalog_rollback_create(foreign::ForeignObjectClass::Wrapper, reused);
    assert_eq!(storage.foreign_wrapper_entry(reused).created_at, 0);
    assert!(
        storage
            .foreign_wrapper_entry(reused)
            .ownership
            .pending
            .is_none()
    );

    let pending = storage
        .create_foreign_wrapper(
            foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("wrapper_pending").unwrap(),
                handler: foreign::ForeignDataHandler::None,
                validator: foreign::ForeignDataValidator::None,
                options: foreign::ForeignOptions::EMPTY,
            },
            20,
        )
        .unwrap();
    assert_eq!(
        storage
            .create_foreign_wrapper(
                foreign::ForeignDataWrapperDefinition {
                    name: SqlName::parse("wrapper_pending").unwrap(),
                    handler: foreign::ForeignDataHandler::None,
                    validator: foreign::ForeignDataValidator::None,
                    options: foreign::ForeignOptions::EMPTY,
                },
                21,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.foreign_catalog_rollback_create(foreign::ForeignObjectClass::Wrapper, pending);

    let altered = storage.foreign_wrapper(wrapper_names[1], 0).unwrap().0;
    let definition = storage.foreign_wrapper_entry(altered).definition;
    let prior = storage
        .alter_foreign_wrapper(altered, definition, 30)
        .unwrap();
    assert_eq!(
        storage
            .stage_foreign_catalog_owner(
                foreign::ForeignObjectClass::Wrapper,
                altered,
                BOOTSTRAP_ROLE,
                31,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.rollback_foreign_wrapper_alter(altered, prior);
    let prior_owner = storage
        .stage_foreign_catalog_owner(
            foreign::ForeignObjectClass::Wrapper,
            altered,
            BOOTSTRAP_ROLE,
            32,
        )
        .unwrap();
    assert_eq!(
        storage
            .alter_foreign_wrapper(altered, definition, 33)
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_LOCK_WAIT
    );
    storage.rollback_foreign_catalog_owner(
        foreign::ForeignObjectClass::Wrapper,
        altered,
        prior_owner,
    );

    let fill = storage
        .create_foreign_wrapper(
            foreign::ForeignDataWrapperDefinition {
                name: SqlName::parse("wrapper_fill").unwrap(),
                handler: foreign::ForeignDataHandler::None,
                validator: foreign::ForeignDataValidator::None,
                options: foreign::ForeignOptions::EMPTY,
            },
            0,
        )
        .unwrap();
    storage.foreign_catalog_commit_create(foreign::ForeignObjectClass::Wrapper, fill);
    assert_eq!(
        storage
            .create_foreign_wrapper(
                foreign::ForeignDataWrapperDefinition {
                    name: SqlName::parse("wrapper_full").unwrap(),
                    handler: foreign::ForeignDataHandler::None,
                    validator: foreign::ForeignDataValidator::None,
                    options: foreign::ForeignOptions::EMPTY,
                },
                0,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );
}

#[test]
fn stored_query_dependency_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<StoredQueryDependencyCatalog>();
    assert_send_sync::<Mutex<StoredQueryDependencyCatalog>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_rules = WORKERS;
    config.max_stored_query_dependencies_per_object = 2;
    config.max_catalog_versions_per_object = 2;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let pending_slots =
        core::array::from_fn::<_, WORKERS, _>(|_| std::sync::atomic::AtomicU32::new(u32::MAX));
    let names = ["worker_a", "worker_b", "worker_c", "worker_d"];

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            let pending_slot = &pending_slots[worker];
            scope.spawn(move || {
                let marker = SqlName::parse(names[worker]).unwrap();
                let mut dependencies = StoredQueryDependencies::EMPTY;
                dependencies.limit = 2;
                dependencies
                    .push(StoredQueryDependency {
                        class: DependencyClass::Table,
                        slot: worker as u16,
                        identity: StoredDependencyIdentity::Table(
                            RelationReference::from_storage(worker as u64 + 1).unwrap(),
                        ),
                        referenced_columns: ColumnSet::EMPTY,
                        schema: marker,
                        name: marker,
                        referenced_schema: marker,
                        referenced_name: marker,
                    })
                    .unwrap();
                dependencies
                    .push(StoredQueryDependency {
                        class: DependencyClass::Sequence,
                        slot: worker as u16,
                        identity: StoredDependencyIdentity::Name,
                        referenced_columns: ColumnSet::EMPTY,
                        schema: marker,
                        name: marker,
                        referenced_schema: marker,
                        referenced_name: marker,
                    })
                    .unwrap();
                storage
                    .write_dependency_image_in(
                        &mut storage.stored_query_dependencies.lock().unwrap(),
                        storage.committed_dependency_image(StoredQueryDependencyOwner::Rule(
                            worker as u16,
                        )),
                        dependencies.view(),
                    )
                    .unwrap();
                let image =
                    storage.committed_dependencies(StoredQueryDependencyOwner::Rule(worker as u16));
                assert_eq!(image.entries().len(), 2);
                assert!(
                    image
                        .entries()
                        .iter()
                        .all(|dependency| dependency.schema == marker)
                );
                drop(image);
                let slot = storage
                    .allocate_pending_stored_query_dependencies_in(
                        &mut storage.stored_query_dependencies.lock().unwrap(),
                        StoredQueryDependencyOwner::Rule(worker as u16),
                        None,
                        dependencies.view(),
                    )
                    .unwrap();
                pending_slot.store(slot, std::sync::atomic::Ordering::Relaxed);
            });
        }
    });

    let mut seen = 0u64;
    for pending_slot in &pending_slots {
        let slot = pending_slot.load(std::sync::atomic::Ordering::Relaxed);
        assert_ne!(slot, u32::MAX);
        let bit = 1u64 << slot;
        assert_eq!(seen & bit, 0, "pending slots must have exclusive owners");
        seen |= bit;
        assert_eq!(storage.pending_dependencies(slot).entries().len(), 2);
    }
    assert_eq!(seen.count_ones() as usize, WORKERS);

    let catalog = storage
        .stored_query_dependencies
        .lock()
        .expect("stored-query dependency catalog lock poisoned");
    assert_eq!(
        catalog.counts.len(),
        config.max_rules
            + config.max_policies
            + config.max_routines
            + config.max_materialized_views
    );
    assert_eq!(
        catalog.pending.len(),
        pending_stored_query_dependency_capacity(&config)
    );
    assert_eq!(
        catalog.entries.len(),
        (catalog.counts.len() + catalog.pending.len())
            * config.max_stored_query_dependencies_per_object
    );
}

#[test]
fn policy_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PolicyCatalog>();
    assert_send_sync::<Mutex<PolicyCatalog>>();
    assert_send_sync::<PolicyIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_policies = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let target = storage
        .create_table(make_def("policy_target", &[]))
        .unwrap();

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let mut catalog = storage
                    .policy_catalog
                    .lock()
                    .expect("policy catalog lock poisoned");
                let role_start = worker * storage.policy_roles_per_image;
                catalog.roles[role_start] = worker as u16;
                catalog.definitions[worker] = PolicyDef {
                    database: current_database(),
                    created_at: storage.catalog_sequence.next().unwrap(),
                    name: SqlName::parse(stack_format!(63, "policy_{worker}").as_str()).unwrap(),
                    table: target as u16,
                    command: PolicyCommandKind::Select,
                    permissive: true,
                    definition: PolicyDefinition {
                        using: None,
                        with_check: None,
                    },
                    role_count: 1,
                    pending_definition: None,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });

    assert_eq!(storage.policy_count(), WORKERS);
    assert_eq!(storage.policies_with_slots_visible_to(0).count(), WORKERS);
    for (slot, policy) in storage.policies_with_slots_visible_to(0) {
        assert_eq!(storage.policy(slot).created_at, policy.created_at);
        let roles = storage.policy_roles(slot, 0);
        assert_eq!(&*roles, &[slot as u16]);
    }
    {
        let catalog = storage
            .policy_catalog
            .lock()
            .expect("policy catalog lock poisoned");
        assert_eq!(catalog.definitions.capacity(), WORKERS);
        assert_eq!(
            catalog.roles.capacity(),
            policy_role_image_capacity(&config) * storage.policy_roles_per_image
        );
    }

    let dependencies = StoredQueryDependencies::EMPTY;
    assert_eq!(
        storage
            .create_policy(
                PolicySpec {
                    name: SqlName::parse("policy_full").unwrap(),
                    table: target,
                    command: PolicyCommandKind::Select,
                    permissive: true,
                    definition: PolicyDefinition {
                        using: None,
                        with_check: None,
                    },
                    roles: &[PUBLIC_ROLE],
                },
                &dependencies,
                9,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );

    storage.drop_policy(0, 9);
    storage.commit_policy_drop(0);
    assert_eq!(storage.policy(0).created_at, 0);
    assert_eq!(storage.policy(0).name, SqlName::EMPTY);
    assert_eq!(storage.policy(0).table, u16::MAX);
    assert_eq!(storage.policy(0).ddl_state, CatalogDdlState::Absent);
    assert_eq!(&*storage.policy_roles(0, 0), &[PUBLIC_ROLE]);

    {
        let mut catalog = storage
            .policy_catalog
            .lock()
            .expect("policy catalog lock poisoned");
        catalog.definitions[0] = PolicyDef {
            created_at: 99,
            name: SqlName::parse("pending_policy").unwrap(),
            table: target as u16,
            ddl_state: CatalogDdlState::PendingCreate { txid: 10 },
            ..PolicyDef::EMPTY
        };
    }
    storage.rollback_policy_create(0);
    assert_eq!(storage.policy(0).created_at, 0);
    assert_eq!(storage.policy(0).name, SqlName::EMPTY);
    assert_eq!(storage.policy(0).table, u16::MAX);
    assert_eq!(storage.policy(0).ddl_state, CatalogDdlState::Absent);
    assert_eq!(&*storage.policy_roles(0, 0), &[PUBLIC_ROLE]);
}

#[test]
fn materialized_view_catalog_is_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MatviewDef>();
    assert_send_sync::<Mutex<FixedVec<MatviewDef>>>();
    assert_send_sync::<MatviewIter<'_>>();

    const WORKERS: usize = 4;
    let mut config = test_config();
    config.max_materialized_views = WORKERS;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    for name in [
        "materialized_a",
        "materialized_b",
        "materialized_c",
        "materialized_d",
    ] {
        storage
            .create_table(make_def(name, &[("value", ColType::Int4, true)]))
            .unwrap();
    }

    std::thread::scope(|scope| {
        for worker in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let mut catalog = storage.matview_catalog();
                let created_at = storage.catalog_sequence.next().unwrap();
                catalog[worker] = MatviewDef {
                    database: current_database(),
                    created_at,
                    backing_table: worker as u16,
                    backing_created_at: storage.table(worker).created_at(),
                    ddl_version: created_at,
                    sql: StackStr::from_str("SELECT 1"),
                    creation_path: StackStr::from_str("public"),
                    ownership: Ownership::BOOTSTRAP,
                    populated: false,
                    population_version: created_at,
                    pending_population: None,
                    query_version: created_at,
                    pending_query: None,
                    ddl_state: CatalogDdlState::Present,
                };
            });
        }
    });

    assert_eq!(storage.matviews_visible_to(0).count(), WORKERS);
    assert_eq!(storage.checkpoint_matviews().count(), WORKERS);
    assert_eq!(storage.matview_count(), config.max_materialized_views);
    for (slot, definition) in storage.matviews_visible_to(0) {
        assert_eq!(storage.matview(slot).created_at, definition.created_at);
    }

    std::thread::scope(|scope| {
        for slot in 0..WORKERS {
            let storage = &storage;
            scope.spawn(move || {
                let target = storage.prepare_matview_population(slot, 0).unwrap();
                let undo = storage.stage_matview_population(target, true, 0).unwrap();
                storage.finish_matview_population(undo, CatalogDdlDecision::Commit);
            });
        }
    });
    assert!(
        storage
            .live_matviews()
            .all(|definition| definition.populated)
    );

    let selected = storage.prepare_matview_edit(0, 10).unwrap();
    let retired = storage.drop_matview(selected, 10).unwrap();
    storage.finish_matview_change(retired, CatalogDdlDecision::Commit);
    let created = storage
        .create_matview(
            crate::storage::CatalogAllocation::New,
            0,
            test_view_definition(StoredQueryDependencies::EMPTY).query,
            false,
            10,
        )
        .unwrap();
    storage.finish_matview_change(created, CatalogDdlDecision::Rollback);
    let empty = storage.matview(0);
    assert_eq!(empty.database, DatabaseOid::POSTGRES);
    assert_eq!(empty.created_at, 0);
    assert_eq!(empty.backing_table, u16::MAX);
    assert!(empty.sql.as_str().is_empty());
    assert!(empty.creation_path.as_str().is_empty());
    assert_eq!(empty.ownership, Ownership::BOOTSTRAP);
    assert!(!empty.populated);
    assert_eq!(empty.ddl_state, CatalogDdlState::Absent);
}

#[test]
fn cumulative_statistics_are_synchronized_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<CumulativeStatistics>();

    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let statistics = &storage.cumulative_statistics;
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(move || {
                for _ in 0..1_000 {
                    let mut statistics = statistics
                        .lock()
                        .expect("cumulative statistics lock poisoned");
                    statistics.databases[0].sessions =
                        statistics.databases[0].sessions.saturating_add(1);
                }
            });
        }
    });
    let statistics = storage.cumulative_statistics();
    assert_eq!(statistics.databases[0].sessions, 4_000);
    assert_eq!(
        statistics.relations.capacity(),
        table_slot_capacity(&config)
    );
    assert_eq!(
        statistics.function_transactions.capacity(),
        (config.max_connections as usize + config.max_prepared_transactions) * config.max_routines
    );
}

#[test]
fn foreign_session_pool_is_endpoint_keyed_and_startup_bounded() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ForeignSessionPool>();

    let mut config = test_config();
    config.max_foreign_sessions = 2;
    config.foreign_receive_bytes = 256;
    config.foreign_send_bytes = 128;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    for _ in 0..config.max_foreign_sessions {
        let client = crate::pg::replication_client::ReplicationClient::new_unbound(
            &mut budget,
            1,
            config.foreign_receive_bytes,
            config.foreign_send_bytes,
            None,
        )
        .unwrap();
        storage.install_foreign_client(client);
    }
    let first = crate::pg::replication_client::ConnectionInfo::parse(
        "host=127.0.0.1 port=5432 user=postgres dbname=postgres sslmode=disable",
    )
    .unwrap();
    let second = crate::pg::replication_client::ConnectionInfo::parse(
        "host=127.0.0.2 port=5432 user=postgres dbname=postgres sslmode=disable",
    )
    .unwrap();
    let third = crate::pg::replication_client::ConnectionInfo::parse(
        "host=127.0.0.3 port=5432 user=postgres dbname=postgres sslmode=disable",
    )
    .unwrap();

    let (first_id, active) = storage
        .reserve_foreign_session(41, first, std::time::Duration::from_secs(1))
        .unwrap();
    assert!(!active);
    storage.foreign_client(first_id).unwrap().activate();
    assert_eq!(
        storage
            .reserve_foreign_session(41, first, std::time::Duration::from_secs(1))
            .unwrap(),
        (first_id, true)
    );
    let client = storage.foreign_client(first_id).unwrap();
    std::thread::scope(|scope| {
        let slot = &storage.foreign_sessions.slots[first_id.session_id.index()];
        scope
            .spawn(move || {
                assert!(matches!(
                    slot.try_lock(),
                    Err(std::sync::TryLockError::WouldBlock)
                ));
            })
            .join()
            .unwrap();
    });
    drop(client);

    let (second_id, active) = storage
        .reserve_foreign_session(41, second, std::time::Duration::from_secs(2))
        .unwrap();
    assert!(!active);
    storage.foreign_client(second_id).unwrap().activate();
    assert_ne!(first_id, second_id);
    assert_eq!(storage.foreign_sessions(41).count(), 2);
    assert_eq!(storage.foreign_sessions_used(), 2);
    assert_eq!(
        storage
            .reserve_foreign_session(73, first, std::time::Duration::from_secs(1))
            .unwrap_err()
            .sqlstate,
        sqlstate::TOO_MANY_CONNECTIONS
    );
    let wrong_owner = ForeignSessionLease {
        session_id: first_id.session_id,
        transaction_id: 73,
        endpoint: first,
    };
    assert_eq!(
        storage.foreign_client(wrong_owner).err().unwrap().sqlstate,
        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
    );

    storage.clear_foreign_session(first_id);
    let (replacement, active) = storage
        .reserve_foreign_session(41, third, std::time::Duration::from_secs(3))
        .unwrap();
    assert_eq!(replacement.session_id, first_id.session_id);
    assert!(!active);
    assert_eq!(
        storage.foreign_client(first_id).err().unwrap().sqlstate,
        sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE
    );
    storage.foreign_client(replacement).unwrap().activate();

    for session_id in storage.foreign_sessions(41) {
        storage.clear_foreign_session(session_id);
    }
    assert_eq!(storage.foreign_sessions(41).count(), 0);
    assert_eq!(storage.foreign_sessions_used(), 0);
    let (reused, active) = storage
        .reserve_foreign_session(73, first, std::time::Duration::from_secs(1))
        .unwrap();
    assert_eq!(reused.session_id, first_id.session_id);
    assert_eq!(reused.transaction_id(), 73);
    assert!(!active);
    assert_eq!(storage.foreign_sessions_used(), 1);
}

#[test]
fn user_type_identity_is_atomic() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let mut column = ColumnMeta::EMPTY;
    column.ctype = ColType::Enum(None);
    assert_eq!(
        storage
            .declared_column_type(&column, 0)
            .unwrap_err()
            .sqlstate,
        sqlstate::PROTOCOL_VIOLATION
    );
    column.user_type = Some(UserTypeName::declaration(
        SqlName::parse("public").unwrap(),
        SqlName::parse("missing_type").unwrap(),
    ));
    assert_eq!(
        storage
            .declared_column_type(&column, 0)
            .unwrap_err()
            .sqlstate,
        sqlstate::UNDEFINED_OBJECT
    );
}

#[test]
fn create_find_drop_reuse() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut s = Storage::new(&config, &mut budget, 0).unwrap();
    let def = make_def("t1", &[("id", ColType::Int4, true)]);
    let index = s.create_table(def.clone()).unwrap();
    assert_eq!(s.find_table("public", "t1"), Some(index));
    assert_eq!(
        s.create_table(def).unwrap_err().sqlstate,
        sqlstate::DUPLICATE_TABLE
    );
    s.drop_table(index).unwrap();
    assert_eq!(s.find_table("public", "t1"), None);
    // Slot is reusable; capacity is enforced.
    for i in 0..4u32 {
        let name = crate::stack_format!(8, "x{}", i);
        s.create_table(make_def(name.as_str(), &[("a", ColType::Bool, false)]))
            .unwrap();
    }
    let err = s
        .create_table(make_def("overflow", &[("a", ColType::Bool, false)]))
        .unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
}

#[test]
fn published_generation_cleanup_keeps_newer_table_writes_dirty() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let slot = storage
        .create_table(make_def(
            "checkpoint_generation",
            &[("id", ColType::Int4, true)],
        ))
        .unwrap();
    let captured = storage.table(slot).generation();
    let _ = storage.tables[slot]
        .rows
        .insert(1, RowState::committed_only_at(RowLoc::test(0, 4), 42));
    storage.table_mut(slot).mark_dirty().unwrap();
    storage.finish_row_cleanup(
        RowCleanupTask::SavedMarkers {
            through_lsn: 42,
            generation: captured,
        },
        RowCleanupTarget::Table(slot),
    );
    storage.clear_table_dirty_at_generation(slot, captured);
    assert!(storage.table(slot).dirty());
    assert_eq!(
        storage.tables[slot]
            .rows
            .get(&1)
            .unwrap()
            .checkpoint_change_lsn,
        42
    );
    let current = storage.table(slot).generation();
    storage.finish_row_cleanup(
        RowCleanupTask::SavedMarkers {
            through_lsn: 42,
            generation: current,
        },
        RowCleanupTarget::Table(slot),
    );
    storage.clear_table_dirty_at_generation(slot, current);
    assert!(!storage.table(slot).dirty());
    assert_eq!(
        storage.tables[slot]
            .rows
            .get(&1)
            .unwrap()
            .checkpoint_change_lsn,
        0
    );
}

#[test]
fn replay_table_rewrite_marker_must_be_paired_exactly_once() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage
        .create_table(make_def("t", &[("id", ColType::Int4, true)]))
        .unwrap();
    let mut column_mapping = [u16::MAX; MAX_COLUMNS];
    column_mapping[0] = 0;

    storage
        .begin_replay_table_rewrite("public", "t", false, column_mapping)
        .unwrap();
    assert_eq!(
        storage
            .begin_replay_table_rewrite("public", "t", false, column_mapping)
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_ERROR
    );
    assert_eq!(
        storage
            .ensure_no_pending_replay_table_rewrite()
            .unwrap_err()
            .sqlstate,
        sqlstate::INTERNAL_ERROR
    );
    assert!(
        storage
            .complete_replay_table_rewrite(
                storage.table_position(0),
                &make_def("t", &[("id", ColType::Int4, true)])
            )
            .unwrap()
    );
    storage.ensure_no_pending_replay_table_rewrite().unwrap();
}

#[test]
fn catalog_ddl_state_has_one_visibility_rule() {
    for transaction in [0, 1] {
        assert!(CatalogDdlState::PendingDrop { txid: transaction }.committed_exists());
        assert!(!CatalogDdlState::PendingCreate { txid: transaction }.committed_exists());
        assert!(!CatalogDdlState::PendingCreateDrop { txid: transaction }.committed_exists());
    }
    assert!(!CatalogDdlState::Absent.visible_to(1));
    assert!(CatalogDdlState::Present.visible_to(1));
    assert!(CatalogDdlState::PendingCreate { txid: 1 }.visible_to(1));
    assert!(!CatalogDdlState::PendingCreate { txid: 1 }.visible_to(2));
    assert!(!CatalogDdlState::PendingDrop { txid: 1 }.visible_to(1));
    assert!(CatalogDdlState::PendingDrop { txid: 1 }.visible_to(2));
}

#[test]
fn catalog_ddl_state_transitions_preserve_the_committed_baseline() {
    assert_eq!(
        CatalogDdlState::PendingCreate { txid: 1 }.commit_create(),
        CatalogDdlState::Present
    );
    assert_eq!(
        CatalogDdlState::PendingDrop { txid: 1 }.commit_drop(),
        CatalogDdlState::Absent
    );
    assert_eq!(
        CatalogDdlState::PendingCreate { txid: 1 }.rollback_create(),
        CatalogDdlState::Absent
    );
    assert_eq!(
        CatalogDdlState::PendingDrop { txid: 1 }.rollback_drop(1),
        CatalogDdlState::Present
    );
    let created_then_dropped = CatalogDdlState::PendingCreate { txid: 1 }.drop_by(1);
    assert_eq!(
        created_then_dropped,
        CatalogDdlState::PendingCreateDrop { txid: 1 }
    );
    assert_eq!(
        created_then_dropped.rollback_drop(1),
        CatalogDdlState::PendingCreate { txid: 1 }
    );
    assert_eq!(
        created_then_dropped.commit_create().commit_drop(),
        CatalogDdlState::Absent
    );
    assert_eq!(
        CatalogDdlState::Present.drop_by(1),
        CatalogDdlState::PendingDrop { txid: 1 }
    );
}

#[test]
fn shared_table_creation_wait_does_not_reserve_row_reads_or_publication() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("independent_rows", &[("id", ColType::Int4, true)]))
        .unwrap();
    let old_values = [Datum::Int4(1)];
    let (old, bytes) = storage
        .heap
        .append(rowenc::encoded_len(&old_values))
        .unwrap();
    rowenc::encode(&old_values, bytes);
    let next_values = [Datum::Int4(2)];
    let (next, bytes) = storage
        .heap
        .append(rowenc::encoded_len(&next_values))
        .unwrap();
    rowenc::encode(&next_values, bytes);
    let mut row = RowState::committed_only_at(old, 7);
    push_pending_version(
        &mut storage.row_versions.exclusive().pending_row_versions,
        &mut row.pending,
        8,
        PendingChange {
            txid: 7,
            cid: 1,
            loc: Some(next),
            changed_columns: ColumnSet::EMPTY,
            changes_existence: false,
        },
    )
    .unwrap();
    storage.tables[table].rows.insert(1, row).unwrap();
    let creation = storage.table_creation.lock().unwrap();
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let storage = &storage;
    std::thread::scope(|scope| {
        let creator = scope.spawn(|| {
            crate::mem::guard::forbid_alloc(|| {
                storage.create_table_in(make_def("waiting_creation", &[]), 8)
            })
        });
        scope.spawn(move || {
            let result = crate::mem::guard::forbid_alloc(|| {
                let retained = storage.resident_row_state(table, 1)?.unwrap();
                storage.commit_row(table, 1, 7, 9)?;
                assert_eq!(storage.row_state(table, 1)?.unwrap().committed_lsn, 9);
                let current = storage.resident_row_state(table, 1)?.unwrap();
                for (source, snapshot, expected) in [(&retained, 7, 1), (&current, 9, 2)] {
                    let image = source
                        .select_owned(table, 1, 8, SNAPSHOT_ALL, snapshot)?
                        .unwrap();
                    let VisibleRowBytes::Heap(bytes) = image.bytes else {
                        panic!("heap fixture");
                    };
                    let mut values = [Datum::Null];
                    rowenc::decode(&bytes, &[ColType::Int4], &mut values)?;
                    assert_eq!(values, [Datum::Int4(expected)]);
                }
                Ok::<_, SqlError>(())
            });
            sent.send(result).unwrap();
        });
        let progress = received.recv_timeout(crate::test_wait::HANG);
        // Unblock creation before asserting, so scope teardown cannot
        // strand the waiting worker if publication failed.
        drop(creation);
        assert!(
            progress.is_ok(),
            "row work must progress while creation waits"
        );
        progress.unwrap().unwrap();
        let created = creator.join().unwrap().unwrap();
        assert_ne!(created, table);
    });
}

#[test]
fn shared_table_creation_reserves_distinct_slots_and_one_owner_per_name() {
    let mut config = test_config();
    config.max_tables = 5;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    let names = ["parallel_a", "parallel_b", "parallel_c", "parallel_d"];
    let ready = crate::mem::mutex::Barrier::new(5);
    let storage = &storage;
    let ready = &ready;
    let slots = std::thread::scope(|scope| {
        let handles: [_; 4] = core::array::from_fn(|ordinal| {
            scope.spawn(move || {
                ready.wait();
                crate::mem::guard::forbid_alloc(|| {
                    storage.create_table_in(make_def(names[ordinal], &[]), 7 + ordinal as u32)
                })
            })
        });
        ready.wait();
        handles.map(|handle| handle.join().unwrap().unwrap())
    });
    for (ordinal, slot) in slots.iter().copied().enumerate() {
        assert!(!slots[..ordinal].contains(&slot));
        assert_eq!(
            storage.find_visible("public", names[ordinal], 7 + ordinal as u32),
            Some(slot)
        );
        assert_eq!(storage.find_visible("public", names[ordinal], 99), None);
    }
    let ready = crate::mem::mutex::Barrier::new(3);
    let ready = &ready;
    let results = std::thread::scope(|scope| {
        let handles: [_; 2] = core::array::from_fn(|_| {
            scope.spawn(move || {
                ready.wait();
                crate::mem::guard::forbid_alloc(|| {
                    storage.create_table_in(make_def("same_race", &[]), 17)
                })
            })
        });
        ready.wait();
        handles.map(|handle| handle.join().unwrap())
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| result
                .as_ref()
                .is_err_and(|error| error.sqlstate == sqlstate::DUPLICATE_TABLE))
            .count(),
        1
    );
    for slot in slots {
        crate::mem::guard::forbid_alloc(|| storage.commit_create(slot));
    }
}

#[test]
fn shared_table_creation_skips_retiring_slots_and_retries_capacity_after_release() {
    let mut config = test_config();
    config.max_tables = 2;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let old = storage
        .create_table(make_def("retired_creation", &[]))
        .unwrap();
    let identity = storage.tables[old].created_at();
    storage.commit_drop(old);
    let retiring = storage.tables[old].lifecycle();
    crate::mem::guard::forbid_alloc(|| {
        let other = storage
            .create_table_in(make_def("other_creation", &[]), 7)
            .unwrap();
        assert_ne!(other, old);
        let error = storage
            .create_table_in(make_def("full_creation", &[]), 8)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(storage.find_visible("public", "full_creation", 8), None);
        storage.rollback_create(other);
    });
    drop(retiring);
    crate::mem::guard::forbid_alloc(|| {
        let reused = storage
            .create_table_in(make_def("full_creation", &[]), 8)
            .unwrap();
        assert_eq!(reused, old);
        assert_ne!(storage.tables[reused].created_at(), identity);
        storage.commit_create(reused);
        assert_eq!(
            storage.find_visible("public", "full_creation", 99),
            Some(reused)
        );
    });
}

#[test]
fn shared_table_creation_failure_releases_name_slot_lifecycle_and_index_claims() {
    let mut config = test_config();
    config.max_value_indexes = 1;
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut def = make_def(
            "retry_creation",
            &[("id", ColType::Int4, true), ("other", ColType::Int4, true)],
        );
        def.columns[0].unique = true;
        def.columns[1].unique = true;
        let error = storage.create_table_in(def, 7).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(
            error
                .message
                .as_str()
                .contains("value index pool exhausted")
        );
        assert_eq!(storage.find_visible("public", "retry_creation", 7), None);
        assert!(
            storage
                .tables
                .iter()
                .all(|table| table.lifecycle.try_lock().is_ok())
        );
        assert!(storage.value_indexes.acquire().is_some());
        assert!(storage.pending_table_defs.test_write().is_ok());
        let mut retry = make_def("retry_creation", &[("id", ColType::Int4, true)]);
        retry.columns[0].unique = true;
        let slot = storage.create_table_in(retry, 7).unwrap();
        storage.commit_create(slot);
        storage.commit_drop(slot);
        assert!(storage.value_indexes.acquire().is_some());
    });
}

#[test]
fn table_retirement_waits_for_its_definition_without_reserving_unrelated_tables() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let first = storage.create_table(make_def("held_cleanup", &[])).unwrap();
    let second = storage
        .create_table(make_def("other_cleanup", &[]))
        .unwrap();
    let third = storage
        .create_table_in(make_def("pending_cleanup", &[]), 11)
        .unwrap();
    let replay = storage
        .create_table(make_def("replay_cleanup", &[]))
        .unwrap();
    let mut updated = TableDefBuilder::from_definition(&storage.table_def(first, 7));
    updated.name = SqlName::parse("held_pending_cleanup").unwrap();
    let _change = storage
        .write_table_def(
            storage.select_table_definition(first, 7).0,
            updated,
            &[None; MAX_COLUMNS],
            false,
            crate::storage::ColumnSet::EMPTY,
        )
        .unwrap();
    let target = RuleTarget::Table(second as u16);
    let publication_rule = storage
        .create_rule(
            RuleDefinition {
                name: SqlName::parse("cleanup_rule").unwrap(),
                target,
                event: RewriteEvent::Insert,
                ..RuleDefinition::EMPTY
            },
            StoredQueryDependencies::EMPTY.view(),
            false,
            7,
        )
        .unwrap();
    let rule = publication_rule.slot();
    storage.begin_rule_history(target, 7).unwrap();
    storage.finish_rule_publication(publication_rule, None, CatalogDdlDecision::Commit);
    let held = storage.table_def(first, 7);
    assert_eq!(held.name.as_str(), "held_pending_cleanup");
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    let storage = &storage;
    std::thread::scope(|scope| {
        scope.spawn(|| crate::mem::guard::forbid_alloc(|| storage.commit_drop(first)));
        let deadline = crate::test_wait::deadline();
        let mut selected_is_waiting = false;
        loop {
            match storage.tables[first].lifecycle.try_lock() {
                Err(std::sync::TryLockError::WouldBlock) => {
                    selected_is_waiting = true;
                    break;
                }
                Ok(guard) => drop(guard),
                Err(std::sync::TryLockError::Poisoned(_)) => break,
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::yield_now();
        }
        scope.spawn(move || {
            crate::mem::guard::forbid_alloc(|| {
                let dropped = storage.drop_table_in(second, 7).unwrap();
                storage.finish_table_drop(dropped, crate::storage::CatalogDdlDecision::Rollback);
                storage.drop_table_in(second, 8).unwrap();
                storage.commit_drop(second);
                storage.commit_create(third);
                storage.rollback_create(third);
                storage.drop_table(replay).unwrap();
            });
            sent.send(()).unwrap();
        });
        let unrelated = received.recv_timeout(crate::test_wait::HANG);
        // Release the selected definition before any assertion can unwind
        // the scope while its retiring writer is still waiting.
        drop(held);
        assert!(
            selected_is_waiting,
            "selected retirement owns its lifecycle while waiting"
        );
        assert!(
            unrelated.is_ok(),
            "unrelated cleanup and existence changes must progress"
        );
    });
    for slot in [first, second, third, replay] {
        assert!(!storage.tables[slot].live());
        assert!(storage.tables[slot].lifecycle.try_lock().is_ok());
    }
    assert_eq!(storage.rule(rule).ddl_state, CatalogDdlState::Absent);
    assert!(storage.pending_table_defs.test_write().is_ok());
}

#[test]
fn table_retirement_batches_keep_maps_valid_and_preserve_retained_versions_through_reuse() {
    const ROWS: usize = 128;
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let first = storage
        .create_table(make_def("retiring_rows", &[("id", ColType::Int4, true)]))
        .unwrap();
    let other = storage
        .create_table_in(make_def("unrelated_rows", &[]), 8)
        .unwrap();
    let created_at = storage.tables[first].created_at();
    let mut keys = [0; ROWS];
    let mut pending_slots = [0; ROWS];
    let mut history_slots = [0; ROWS];
    let retained = crate::mem::guard::forbid_alloc(|| {
        let history = storage.heap.append_bytes(b"hist").unwrap();
        let committed = storage.heap.append_bytes(b"keep").unwrap();
        let pending = storage.heap.append_bytes(b"pend").unwrap();
        for (ordinal, key) in keys.iter_mut().enumerate() {
            *key = ordinal as u64 * 65537 + 7;
            let mut row = RowState::committed_only_at(committed, 7);
            {
                let versions = storage.row_versions.exclusive();
                push_committed_version(
                    &mut versions.committed_row_versions,
                    &mut row.history,
                    8,
                    CommittedVersion {
                        lsn: 6,
                        home: Some(RowHome::Heap(history)),
                    },
                )
                .unwrap();
                push_pending_version(
                    &mut versions.pending_row_versions,
                    &mut row.pending,
                    8,
                    PendingChange {
                        txid: 7,
                        cid: 1,
                        loc: Some(pending),
                        changed_columns: ColumnSet::EMPTY,
                        changes_existence: false,
                    },
                )
                .unwrap();
            }
            pending_slots[ordinal] = row.pending.tail.unwrap();
            history_slots[ordinal] = row.history.tail.unwrap();
            storage.tables[first].rows.insert(*key, row).unwrap();
        }
        storage.resident_row_state(first, keys[0]).unwrap().unwrap()
    });
    crate::mem::guard::forbid_alloc(|| {
        let mut cursor = None;
        let mut done = false;
        let mut turns = 0;
        while !done {
            let before = storage.tables[first].rows.len();
            done = storage.retire_table_row_batch(first, &mut cursor);
            turns += 1;
            let after = storage.tables[first].rows.len();
            assert!(
                before - after <= 64,
                "one turn retires at most its fixed work allowance"
            );
            assert!(
                done || after < before,
                "owner retirement must make bounded progress"
            );
            assert!(storage.row_versions.test_write().is_ok());
            assert!(storage.tables[first].rows.test_write().is_ok());
            let dropped = storage.drop_table_in(other, 8).unwrap();
            storage.finish_table_drop(dropped, crate::storage::CatalogDdlDecision::Rollback);
            let rows = storage.tables[first].rows.read();
            for (key, row) in rows.iter() {
                assert_eq!(
                    rows.get(key),
                    Some(row),
                    "cell removal preserves every remaining lookup"
                );
                assert_eq!(row.committed_lsn, 7);
            }
            assert!(turns <= ROWS * 4, "the fixed map drain must terminate");
        }
        assert!(turns > 1);
        assert!(storage.tables[first].rows.is_empty());
        {
            let versions = storage.row_versions.read();
            for ordinal in 0..ROWS {
                assert!(!versions.pending_row_versions[pending_slots[ordinal]].used);
                assert!(!versions.committed_row_versions[history_slots[ordinal]].used);
            }
        }
        storage.commit_drop(first);
        let replacement = storage
            .create_table(make_def("reused_rows", &[("id", ColType::Int4, true)]))
            .unwrap();
        assert_eq!(replacement, first);
        assert_ne!(storage.tables[first].created_at(), created_at);
        let history = storage.heap.append_bytes(b"newh").unwrap();
        let committed = storage.heap.append_bytes(b"newc").unwrap();
        let pending = storage.heap.append_bytes(b"newp").unwrap();
        for key in keys {
            let mut row = RowState::committed_only_at(committed, 9);
            {
                let versions = storage.row_versions.exclusive();
                push_committed_version(
                    &mut versions.committed_row_versions,
                    &mut row.history,
                    8,
                    CommittedVersion {
                        lsn: 8,
                        home: Some(RowHome::Heap(history)),
                    },
                )
                .unwrap();
                push_pending_version(
                    &mut versions.pending_row_versions,
                    &mut row.pending,
                    8,
                    PendingChange {
                        txid: 9,
                        cid: 1,
                        loc: Some(pending),
                        changed_columns: ColumnSet::EMPTY,
                        changes_existence: false,
                    },
                )
                .unwrap();
            }
            storage.tables[first].rows.insert(key, row).unwrap();
        }
        drop(storage);
        for (transaction, snapshot, expected) in [(7, 0, b"pend"), (8, 6, b"hist"), (8, 7, b"keep")]
        {
            let image = retained
                .select_owned(first, keys[0], transaction, SNAPSHOT_ALL, snapshot)
                .unwrap()
                .unwrap();
            assert_eq!(image.snapshot().created_at, created_at);
            let VisibleRowBytes::Heap(bytes) = image.bytes else {
                panic!("heap fixture");
            };
            assert_eq!(&*bytes, expected);
        }
    });
}

#[test]
fn owned_row_lookup_selects_pending_history_and_current_after_retirement() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("lookup_old", &[("id", ColType::Int4, true)]))
        .unwrap();
    let created_at = storage.tables[table].created_at();
    let mut scratch = FixedVec::new(&mut budget, "owned lookup relocation", 4).unwrap();
    let retained = crate::mem::guard::forbid_alloc(|| {
        let history = storage.heap.append_bytes(b"hist").unwrap();
        let committed = storage.heap.append_bytes(b"keep").unwrap();
        let pending = storage.heap.append_bytes(b"pend").unwrap();
        let mut row = RowState::committed_only_at(committed, 7);
        {
            let versions = storage.row_versions.exclusive();
            push_committed_version(
                &mut versions.committed_row_versions,
                &mut row.history,
                8,
                CommittedVersion {
                    lsn: 6,
                    home: Some(RowHome::Heap(history)),
                },
            )
            .unwrap();
            push_pending_version(
                &mut versions.pending_row_versions,
                &mut row.pending,
                8,
                PendingChange {
                    txid: 7,
                    cid: 1,
                    loc: Some(pending),
                    changed_columns: ColumnSet::EMPTY,
                    changes_existence: false,
                },
            )
            .unwrap();
        }
        storage.tables[table].rows.insert(1, row).unwrap();
        storage.resident_row_state(table, 1).unwrap().unwrap()
    });
    crate::mem::guard::forbid_alloc(|| {
        storage.commit_drop(table);
        let replacement = storage
            .create_table(make_def("lookup_new", &[("id", ColType::Text, false)]))
            .unwrap();
        assert_eq!(replacement, table);
        assert_ne!(storage.tables[table].created_at(), created_at);
        let location = storage.heap.append_bytes(b"next").unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 8))
            .unwrap();
        storage.compact_heap(&mut scratch).unwrap();
        drop(storage);
        for (transaction, snapshot, expected) in [(7, 0, b"pend"), (8, 6, b"hist"), (8, 7, b"keep")]
        {
            let image = retained
                .select_owned(table, 1, transaction, SNAPSHOT_ALL, snapshot)
                .unwrap()
                .unwrap();
            assert_eq!(image.snapshot().created_at, created_at);
            let VisibleRowBytes::Heap(bytes) = image.bytes else {
                panic!("heap fixture")
            };
            assert_eq!(&*bytes, expected);
        }
        assert_eq!(
            retained
                .select_owned(table, 2, 7, SNAPSHOT_ALL, 7)
                .err()
                .unwrap()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert_eq!(
            retained
                .select_owned(table + 1, 1, 7, SNAPSHOT_ALL, 7)
                .err()
                .unwrap()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        drop(retained);
    });
}

#[test]
fn owned_heap_rows_survive_table_slot_reuse_and_storage_teardown() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("old_heap", &[("id", ColType::Int4, true)]))
        .unwrap();
    let arena = Arena::new(&mut budget, "owned heap rows", 4).unwrap();
    let (old, created_at) = crate::mem::guard::forbid_alloc(|| {
        let old_location = storage.heap.append_bytes(b"keep").unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(old_location, 7))
            .unwrap();
        let old = storage
            .visible_row_home_at(
                table,
                1,
                storage.row_state(table, 1).unwrap().unwrap(),
                0,
                SNAPSHOT_ALL,
                7,
            )
            .unwrap()
            .unwrap();
        let created_at = old.snapshot().created_at;
        storage.commit_drop(table);
        let replacement = storage
            .create_table(make_def("new_heap", &[("id", ColType::Int4, true)]))
            .unwrap();
        assert_eq!(replacement, table);
        assert_ne!(storage.tables[table].created_at(), created_at);
        let new_location = storage.heap.append_bytes(b"next").unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(new_location, 8))
            .unwrap();
        assert_eq!(
            storage
                .row_bytes(table, 1, old.snapshot(), &arena)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        let new = storage
            .visible_row_home_at(
                table,
                1,
                storage.row_state(table, 1).unwrap().unwrap(),
                0,
                SNAPSHOT_ALL,
                8,
            )
            .unwrap()
            .unwrap();
        assert_eq!(storage.row_bytes(table, 1, new, &arena).unwrap(), b"next");
        (old, created_at)
    });
    crate::mem::guard::forbid_alloc(|| {
        drop(storage);
        assert_eq!(old.snapshot().created_at, created_at);
        assert_eq!(old.snapshot().version, RowVersionIdentity::Committed(7));
        let VisibleRowBytes::Heap(bytes) = old.bytes else {
            panic!("expected heap bytes")
        };
        assert_eq!(&*bytes, b"keep");
        drop(bytes);
    });
}

#[test]
fn owned_heap_rows_compaction_rotation_preserves_versions_and_busy_retry() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("pinned_heap", &[("id", ColType::Int4, true)]))
        .unwrap();
    let mut scratch = FixedVec::new(&mut budget, "owned heap compaction", 4).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage.heap.append_bytes(b"dead").unwrap();
        let location = storage.heap.append_bytes(b"keep").unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        let image = storage
            .visible_row_home_at(
                table,
                1,
                storage.row_state(table, 1).unwrap().unwrap(),
                0,
                SNAPSHOT_ALL,
                7,
            )
            .unwrap()
            .unwrap();
        assert!(storage.row_versions.test_write().is_ok());
        assert!(storage.tables[table].rows.test_write().is_ok());
        let later = storage.heap.append_bytes(b"next").unwrap();
        storage
            .write_pending_untracked(table, 1, 9, 1, Some(later))
            .unwrap();
        storage.compact_heap(&mut scratch).unwrap();
        assert_eq!(storage.heap.used(), 8);
        let first_generation = storage.tables[table].rows.get(&1).unwrap();
        let current = storage
            .visible_row_home_at(
                table,
                1,
                storage.row_state(table, 1).unwrap().unwrap(),
                0,
                SNAPSHOT_ALL,
                7,
            )
            .unwrap()
            .unwrap();
        let used_scratch = scratch.len();
        let error = storage.compact_heap(&mut scratch).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            error.message.as_str(),
            "row heap relocation buffers are retained by readers"
        );
        assert_eq!(storage.heap.used(), 8);
        assert_eq!(scratch.len(), used_scratch);
        assert_eq!(storage.tables[table].rows.get(&1), Some(first_generation));
        storage
            .with_row_bytes(table, 1, image, |bytes| {
                assert_eq!(bytes, b"keep");
                Ok(())
            })
            .unwrap();
        storage.compact_heap(&mut scratch).unwrap();
        assert_eq!(storage.heap.used(), 8);
        assert_eq!(
            storage.tables[table]
                .rows
                .get(&1)
                .unwrap()
                .committed
                .unwrap()
                .heap_loc()
                .unwrap()
                .offset,
            0
        );
        assert_eq!(
            storage
                .row_state(table, 1)
                .unwrap()
                .unwrap()
                .pending_last()
                .unwrap()
                .loc
                .unwrap()
                .offset,
            4
        );
        storage
            .with_row_bytes(table, 1, current, |bytes| {
                assert_eq!(bytes, b"keep");
                Ok(())
            })
            .unwrap();
        assert_eq!(
            storage.heap.get(location).err().unwrap().sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
    });
}

#[test]
fn owned_heap_rows_failed_consumers_release_ownership_and_check_identity() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("error_heap", &[("id", ColType::Int4, true)]))
        .unwrap();
    let tiny = Arena::new(&mut budget, "too small for heap row", 3).unwrap();
    let mut scratch = FixedVec::new(&mut budget, "error heap compaction", 2).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let location = storage.heap.append_bytes(b"keep").unwrap();
        storage.tables[table]
            .rows
            .insert(1, RowState::committed_only_at(location, 7))
            .unwrap();
        for failure in 0..4 {
            let image = storage
                .visible_row_home_at(
                    table,
                    1,
                    storage.row_state(table, 1).unwrap().unwrap(),
                    0,
                    SNAPSHOT_ALL,
                    7,
                )
                .unwrap()
                .unwrap();
            let error = match failure {
                0 => storage
                    .row_bytes(table, 1, image, &tiny)
                    .map(|_| ())
                    .unwrap_err(),
                1 => storage
                    .with_row_bytes(table, 1, image, |_| {
                        Err::<(), _>(sql_err!(sqlstate::QUERY_CANCELED, "cancelled consumer"))
                    })
                    .unwrap_err(),
                2 => storage
                    .with_row_bytes(table, 2, image, |_| -> Result<(), SqlError> {
                        panic!("wrong row accepted")
                    })
                    .unwrap_err(),
                _ => storage
                    .row_bytes(table + 1, 1, image, &tiny)
                    .map(|_| ())
                    .unwrap_err(),
            };
            assert_eq!(
                error.sqlstate,
                match failure {
                    0 => sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    1 => sqlstate::QUERY_CANCELED,
                    _ => sqlstate::INTERNAL_ERROR,
                }
            );
            assert!(storage.heap.test_exclusive_write().is_ok());
            storage.compact_heap(&mut scratch).unwrap();
        }
    });
}

#[test]
fn row_heap_ownership_retained_images_survive_compaction_and_arena_exhaustion() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let _ = storage.heap.append(4).unwrap();
    let (location, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"keep");
    storage.tables[0].identity_mut().existence = CatalogDdlState::Present;
    storage.tables[0]
        .rows
        .insert(1, RowState::committed_only_at(location, 7))
        .unwrap();
    let arena = Arena::new(&mut budget, "retained heap rows", 4).unwrap();
    let mut scratch = FixedVec::new(&mut budget, "heap relocation", 2).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .with_row_bytes(0, 1, RowHome::Heap(location), |bytes| {
                assert!(storage.heap.test_exclusive_write().is_err());
                assert_eq!(bytes, b"keep");
                Ok(())
            })
            .unwrap();
        assert!(storage.heap.test_exclusive_write().is_ok());
        let retained = storage
            .row_bytes(0, 1, RowHome::Heap(location), &arena)
            .unwrap();
        assert!(storage.heap.test_exclusive_write().is_ok());
        let error = storage
            .row_bytes(0, 1, RowHome::Heap(location), &arena)
            .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            error.message.as_str(),
            "heap row images exceed the statement arena; raise work_arena_bytes"
        );
        assert!(storage.heap.test_exclusive_write().is_ok());
        storage.compact_heap(&mut scratch).unwrap();
        assert_eq!(
            storage.tables[0]
                .rows
                .get(&1)
                .unwrap()
                .committed
                .and_then(RowHome::heap_loc)
                .unwrap()
                .offset,
            0
        );
        assert_eq!(retained, b"keep");
        let (_, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"next");
        assert_eq!(retained, b"keep");
    });
}

#[test]
fn row_heap_ownership_compaction_groups_aliases_with_empty_locations() {
    for retained in [false, true] {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let _ = storage.heap.append(4).unwrap();
        let (empty, _) = storage.heap.append(0).unwrap();
        let (location, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"same");
        storage.tables[0].identity_mut().existence = CatalogDdlState::Present;
        for rowid in 1..=4 {
            storage.tables[0]
                .rows
                .insert(
                    rowid,
                    RowState::committed_only_at(if rowid == 2 { empty } else { location }, 7),
                )
                .unwrap();
        }
        let mut scratch = FixedVec::new(&mut budget, "heap aliases", 4).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let held = if retained {
                Some(storage.heap.get(location).unwrap())
            } else {
                None
            };
            storage.compact_heap(&mut scratch).unwrap();
            assert_eq!(storage.heap.used(), 4);
            for rowid in 1..=4 {
                let location = storage.tables[0]
                    .rows
                    .get(&rowid)
                    .unwrap()
                    .committed
                    .and_then(RowHome::heap_loc)
                    .unwrap();
                assert_eq!(location.offset, 0);
                if rowid == 2 {
                    assert!(storage.heap.get(location).unwrap().is_empty());
                } else {
                    assert_eq!(&*storage.heap.get(location).unwrap(), b"same");
                }
            }
            if let Some(held) = held {
                assert_eq!(&*held, b"same");
            }
        });
    }
}

#[test]
fn row_heap_ownership_compaction_preflight_preserves_bytes_and_handles() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let (_, bytes) = storage.heap.append(8).unwrap();
    bytes.copy_from_slice(b"original");
    storage.tables[0].identity_mut().existence = CatalogDdlState::Present;
    let prior = RowState::committed_only_at(RowLoc::test(4, 4), 7);
    storage.tables[0].rows.insert(1, prior).unwrap();
    storage.tables[0]
        .rows
        .insert(2, RowState::committed_only_at(RowLoc::test(7, 2), 8))
        .unwrap();
    let mut scratch = FixedVec::new(&mut budget, "heap preflight", 2).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let error = storage.compact_heap(&mut scratch).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
        assert_eq!(storage.tables[0].rows.get(&1), Some(prior));
        assert_eq!(storage.heap.used(), 8);
        assert_eq!(&*storage.heap.get(RowLoc::test(0, 8)).unwrap(), b"original");
    });
}

#[test]
fn row_heap_generation_compaction_updates_every_visibility_chain_and_rejects_stale_reads() {
    for retained in [false, true] {
        let config = test_config();
        let mut budget = test_budget(&config);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let (_, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"dead");
        let (history, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"old!");
        let (committed, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"live");
        let (pending, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"edit");
        let mut state = RowState::committed_only_at(committed, 7);
        let versions = storage.row_versions.exclusive();
        push_committed_version(
            &mut versions.committed_row_versions,
            &mut state.history,
            storage.max_row_versions_per_row,
            CommittedVersion {
                lsn: 5,
                home: Some(RowHome::Heap(history)),
            },
        )
        .unwrap();
        push_pending_version(
            &mut versions.pending_row_versions,
            &mut state.pending,
            storage.max_row_versions_per_row,
            PendingChange {
                txid: 9,
                cid: 1,
                loc: Some(pending),
                changed_columns: ColumnSet::EMPTY,
                changes_existence: false,
            },
        )
        .unwrap();
        storage.tables[0].identity_mut().existence = CatalogDdlState::Present;
        storage.tables[0].rows.insert(1, state).unwrap();
        let mut scratch = FixedVec::new(&mut budget, "generation compaction", 3).unwrap();
        let arena = Arena::new(&mut budget, "generation images", 32).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let held = if retained {
                Some(storage.heap.get(committed).unwrap())
            } else {
                None
            };
            storage.compact_heap(&mut scratch).unwrap();
            let (replacement, bytes) = storage.heap.append(4).unwrap();
            bytes.copy_from_slice(b"next");
            assert_eq!(replacement.offset, pending.offset);
            for stale in [history, committed, pending] {
                assert_eq!(
                    storage
                        .row_bytes(0, 1, RowHome::Heap(stale), &arena)
                        .unwrap_err()
                        .sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                let error = storage
                    .with_row_bytes(0, 1, RowHome::Heap(stale), |_| -> Result<(), SqlError> {
                        panic!("stale locations must never reach the consumer")
                    })
                    .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
            }
            for (txid, lsn, expected) in [(8, 6, b"old!"), (8, 7, b"live"), (9, 7, b"edit")] {
                let row = storage.resident_row_state(0, 1).unwrap().unwrap();
                let home = storage
                    .visible_row_home_at(0, 1, row, txid, SNAPSHOT_ALL, lsn)
                    .unwrap()
                    .unwrap();
                assert_eq!(storage.row_bytes(0, 1, home, &arena).unwrap(), expected);
            }
            assert!(storage.heap.test_exclusive_write().is_ok());
            assert!(storage.row_versions.test_write().is_ok());
            // A second relocation changes the generation even when offsets
            // are unchanged. Historical and pending images remain readable.
            let previous = storage
                .resident_row_state(0, 1)
                .unwrap()
                .unwrap()
                .committed
                .unwrap();
            storage.compact_heap(&mut scratch).unwrap();
            assert_eq!(
                storage
                    .row_bytes(0, 1, previous, &arena)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::SERIALIZATION_FAILURE
            );
            let row = storage.resident_row_state(0, 1).unwrap().unwrap();
            let home = storage
                .visible_row_home_at(0, 1, row, 9, SNAPSHOT_ALL, 7)
                .unwrap()
                .unwrap();
            assert_eq!(storage.row_bytes(0, 1, home, &arena).unwrap(), b"edit");
            if let Some(held) = held {
                assert_eq!(&*held, b"live");
            }
        });
    }
}

#[test]
fn row_heap_generation_stale_publication_and_preflight_preserve_all_state() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let (_, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"dead");
    let (old, bytes) = storage.heap.append(4).unwrap();
    bytes.copy_from_slice(b"keep");
    storage.tables[0].identity_mut().existence = CatalogDdlState::Present;
    storage.tables[0]
        .rows
        .insert(1, RowState::committed_only_at(old, 7))
        .unwrap();
    let mut scratch = FixedVec::new(&mut budget, "stale preflight", 2).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage.compact_heap(&mut scratch).unwrap();
        let current = storage.tables[0].rows.get(&1).unwrap();
        let (replacement, bytes) = storage.heap.append(4).unwrap();
        bytes.copy_from_slice(b"next");
        for rowid in [1, 2] {
            for tracked in [true, false] {
                let error = storage
                    .write_pending_inner(0, rowid, 9, 1, Some(old), tracked)
                    .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                assert_eq!(storage.tables[0].rows.get(&1), Some(current));
                assert!(storage.tables[0].rows.get(&2).is_none());
            }
        }
        // Even a stale range that remains in bounds must fail the whole
        // preflight before any current image or row handle moves.
        storage.tables[0]
            .rows
            .insert(2, RowState::committed_only_at(old, 8))
            .unwrap();
        let error = storage.compact_heap(&mut scratch).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
        assert_eq!(storage.tables[0].rows.get(&1), Some(current));
        assert_eq!(
            storage.tables[0].rows.get(&2).unwrap().committed,
            Some(RowHome::Heap(old))
        );
        assert_eq!(storage.heap.used(), 8);
        assert_eq!(
            &*storage
                .heap
                .get(current.committed.unwrap().heap_loc().unwrap())
                .unwrap(),
            b"keep"
        );
        assert_eq!(&*storage.heap.get(replacement).unwrap(), b"next");
        assert!(storage.heap.test_exclusive_write().is_ok());
        assert!(storage.row_versions.test_write().is_ok());
    });
}

#[test]
fn heap_append_and_full() {
    let mut config = test_config();
    config.memtable_bytes = 64;
    let mut budget = test_budget(&config);
    let mut s = Storage::new(&config, &mut budget, 0).unwrap();
    let (loc, slice) = s.heap.append(10).unwrap();
    slice.copy_from_slice(b"0123456789");
    assert_eq!(&*s.heap.get(loc).unwrap(), b"0123456789");
    let err = s.heap.append(60).unwrap_err();
    assert_eq!(err.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
}

#[test]
fn heap_compaction_preserves_rows_of_a_pending_create() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let slot = storage
        .create_table_in(make_def("pending", &[("id", ColType::Int4, true)]), 7)
        .unwrap();
    let _ = storage.heap.append(8).unwrap();
    let (location, bytes) = storage.heap.append(7).unwrap();
    bytes.copy_from_slice(b"pending");
    let mut state = RowState {
        committed: None,
        committed_lsn: 0,
        committed_generation: None,
        checkpoint_change_lsn: 0,
        history: CommittedHistory::empty(),
        pending: PendingVersions::empty(),
    };
    let row_versions = storage.row_versions.exclusive();
    push_pending_version(
        &mut row_versions.pending_row_versions,
        &mut state.pending,
        storage.max_row_versions_per_row,
        PendingChange {
            txid: 7,
            cid: 1,
            loc: Some(location),
            changed_columns: ColumnSet::all(MAX_COLUMNS),
            changes_existence: true,
        },
    )
    .unwrap();
    storage.table_mut(slot).rows.insert(1, state).unwrap();
    let mut scratch = FixedVec::new(&mut budget, "compact", 8).unwrap();

    storage.compact_heap(&mut scratch).unwrap();

    let compacted = storage
        .resident_row_state(slot, 1)
        .unwrap()
        .unwrap()
        .pending_last()
        .unwrap()
        .loc
        .unwrap();
    assert_eq!(compacted.offset, 0);
    assert_eq!(&*storage.heap.get(compacted).unwrap(), b"pending");
    let (_, replacement) = storage.heap.append(7).unwrap();
    replacement.copy_from_slice(b"replace");
    assert_eq!(&*storage.heap.get(compacted).unwrap(), b"pending");
}

#[test]
fn name_length_limit() {
    let long = "x".repeat(64);
    assert!(SqlName::parse(&long).is_err());
    let ok = "y".repeat(63);
    assert_eq!(SqlName::parse(&ok).unwrap().as_str(), ok);
}

#[test]
fn replication_slots_are_bounded_and_have_resume_positions() {
    let mut config = test_config();
    config.max_replication_slots = 1;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    storage
        .create_replication_slot(
            ReplicationSlotName::parse("changes").unwrap(),
            42,
            ReplicationSlotBehavior::DEFAULT,
        )
        .unwrap();
    let slot = storage.replication_slot("changes").unwrap();
    assert_eq!(slot.restart_lsn, 42);
    assert_eq!(slot.confirmed_flush_lsn, 42);
    assert!(!slot.active);
    assert_eq!(slot.active_pid, None);
    assert_eq!(
        storage
            .create_replication_slot(
                ReplicationSlotName::parse("other").unwrap(),
                43,
                ReplicationSlotBehavior::DEFAULT,
            )
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );
    let advance = storage
        .prepare_replication_slot_advance("changes", 47)
        .unwrap();
    storage.apply_replication_slot_advance(advance);
    let slot = storage.replication_slot("changes").unwrap();
    assert_eq!(slot.restart_lsn, 47);
    assert_eq!(slot.confirmed_flush_lsn, 47);
    assert_eq!(storage.oldest_replication_restart_lsn(), Some(47));
    assert_eq!(
        storage.activate_replication_slot("changes", 321).unwrap(),
        47
    );
    storage.record_replication_slot_transaction("changes", 49, 500);
    let slot = storage.replication_slot("changes").unwrap();
    assert!(slot.active);
    assert_eq!(slot.active_pid, Some(321));
    assert_eq!(slot.sent_lsn, 49);
    assert_eq!((slot.total_txns, slot.total_bytes), (1, 500));
    storage.deactivate_replication_slot("changes");
    assert_eq!(
        storage.replication_slot("changes").unwrap().active_pid,
        None
    );
    storage.drop_replication_slot("changes").unwrap();
    assert!(storage.replication_slot("changes").is_none());
}

#[test]
fn column_default_encodes_one_execution_state() {
    let expression = StackStr::from_str("7");
    assert!(matches!(
        ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), Some(expression), false),
        Some(ColumnDefault::Constant { .. })
    ));
    assert!(matches!(
        ColumnDefault::from_parts(None, Some(expression), false),
        Some(ColumnDefault::Expression(_))
    ));
    assert!(matches!(
        ColumnDefault::from_parts(None, Some(expression), true),
        Some(ColumnDefault::Generated(_))
    ));
    assert!(ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), None, true).is_none());
    assert!(ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), None, false).is_none());
    assert!(ColumnDefault::from_parts(Some(OwnedDatum::Int4(7)), Some(expression), true).is_none());
}

#[test]
fn publication_filters_share_one_bounded_catalog_payload() {
    let source = [StackStr::from_str("id > 0"), StackStr::from_str("id < 10")];
    let filters = PublicationFilters::from_sql(&source).unwrap();
    assert_eq!(filters.get(0), "id > 0");
    assert_eq!(filters.get(1), "id < 10");

    let member =
        StackStr::from_str("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    let too_large = [member; PUBLICATION_FILTER_STORAGE_BYTES / PUBLICATION_FILTER_SQL_MAX + 1];
    assert_eq!(
        PublicationFilters::from_sql(&too_large)
            .unwrap_err()
            .sqlstate,
        sqlstate::PROGRAM_LIMIT_EXCEEDED
    );
}

#[test]
fn prepare_rechecks_waiters_without_detaching_session_advisory_holders() {
    use crate::sql::ast::TableLockMode;
    use crate::sql::lock::{AdvisoryDecision, AdvisoryKey, AdvisoryMode};

    let mut config = test_config();
    config.max_connections = 3;
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("phase_prepare", &[("id", ColType::Int4, false)]))
        .unwrap();
    let previous = crate::sql::execution_connection_id();
    crate::mem::guard::forbid_alloc(|| {
        crate::sql::set_execution_connection_id(41);
        let holder = storage.start_transaction_identity().unwrap();
        storage
            .lock_table(holder, table, TableLockMode::ShareUpdateExclusive, false)
            .unwrap();
        let key = AdvisoryKey::from_bigint(123);
        assert_eq!(
            storage
                .acquire_advisory_lock(holder, key, AdvisoryMode::Exclusive, false, false)
                .unwrap(),
            AdvisoryDecision::Acquired
        );
        crate::sql::set_execution_connection_id(42);
        let relation_waiter = storage.start_transaction_identity().unwrap();
        assert_eq!(
            storage
                .lock_table(
                    relation_waiter,
                    table,
                    TableLockMode::AccessExclusive,
                    false
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        crate::sql::set_execution_connection_id(43);
        let advisory_waiter = storage.start_transaction_identity().unwrap();
        assert_eq!(
            storage
                .acquire_advisory_lock(advisory_waiter, key, AdvisoryMode::Exclusive, true, false)
                .unwrap(),
            AdvisoryDecision::Waiting
        );
        let mut wait_start = None;
        storage.visit_advisory_locks(|lock| {
            if !lock.granted {
                wait_start = lock.wait_start;
            }
        });
        assert!(wait_start.is_some());
        assert_ne!(storage.lock_wait_generation(42), 0);
        assert_ne!(storage.lock_wait_generation(43), 0);
        storage.prepare_transaction_locks(holder);
        assert_eq!(storage.lock_wait_generation(42), 0);
        assert_eq!(storage.lock_wait_generation(43), 0);
        crate::sql::set_execution_connection_id(42);
        assert_eq!(
            storage
                .lock_table(
                    relation_waiter,
                    table,
                    TableLockMode::AccessExclusive,
                    false
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        assert_ne!(storage.lock_wait_generation(42), 0);
        assert_eq!(storage.blocking_backend_pid_count(42), 0);
        crate::sql::set_execution_connection_id(43);
        assert_eq!(
            storage
                .acquire_advisory_lock(advisory_waiter, key, AdvisoryMode::Exclusive, true, false)
                .unwrap(),
            AdvisoryDecision::Waiting
        );
        let mut blockers = [0; 2];
        assert_eq!(storage.blocking_backend_pids(43, &mut blockers), 1);
        assert_eq!(blockers[0], 41);
        storage.visit_advisory_locks(|lock| {
            if !lock.granted {
                assert_eq!(lock.wait_start, wait_start);
            }
        });
        storage.release_object_locks(holder);
        storage
            .lock_table(
                relation_waiter,
                table,
                TableLockMode::AccessExclusive,
                false,
            )
            .unwrap();
        storage.release_object_locks(relation_waiter);
        storage.release_connection_advisory_locks(41);
        storage.cancel_backend_wait(42);
        storage.cancel_backend_wait(43);
        storage.finish_transaction_identity(holder, false);
        storage.finish_transaction_identity(relation_waiter, false);
        storage.finish_transaction_identity(advisory_waiter, false);
    });
    crate::sql::set_execution_connection_id(previous);
}

#[test]
fn lock_acquisition_identity_exhaustion_preserves_existing_holders() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let storage = Storage::new(&config, &mut budget, 0).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        storage
            .lock_object(
                7,
                ObjectLockTarget::View(0),
                crate::sql::ast::TableLockMode::AccessShare,
                false,
            )
            .unwrap();
        storage.lock_state().sequence = u64::MAX;
        assert_eq!(
            storage
                .lock_object(
                    8,
                    ObjectLockTarget::View(1),
                    crate::sql::ast::TableLockMode::AccessShare,
                    false
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.object_lock_view_count(), 1);
        assert_eq!(storage.lock_mark(), u64::MAX);
        storage.release_object_locks(7);
    });
}

#[test]
fn index_ready_transitions_publish_and_rollback_owned_binding_roots() {
    let mut config = test_config();
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("phase_rows", &[("id", ColType::Int4, false)]))
        .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut definition = IndexDef {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("phase_rows_id_idx").unwrap(),
            table: SqlName::parse("phase_rows").unwrap(),
            n_cols: 1,
            collations: [crate::sql::ast::Collation::None; MAX_INDEX_COLS],
            ..IndexDef::EMPTY
        };
        definition.mutable.kind = IndexKind::OrdinaryUnbuilt;
        let index = storage.create_index(definition, 7).unwrap();
        storage
            .prepare_index_definition_enforcers(index, 7, IndexBindingOperation::Create)
            .unwrap();
        storage
            .prepare_commit_generations(
                7,
                core::iter::empty(),
                core::iter::once(IndexGenerationChange::Created(index)),
            )
            .unwrap();
        storage.publish_index_create(index, 7).unwrap();
        storage.finish_bindings(table, 7);
        storage.release_commit_generations(7);
        assert_eq!(storage.tables[table].enforcer_count(), 0);
        let mut ready = storage.index(index).mutable;
        ready.kind = IndexKind::OrdinaryReady;
        let prior = storage.alter_index_definition(index, ready, 8).unwrap();
        storage
            .prepare_index_definition_enforcers(index, 8, IndexBindingOperation::State)
            .unwrap();
        assert_eq!(storage.tables[table].enforcer_count(), 1);
        assert!(!storage.index(index).mutable.kind.ready());
        assert!(storage.index(index).mutable_for(8).kind.ready());
        storage.rollback_index_definition(index, prior);
        assert_eq!(storage.tables[table].enforcer_count(), 0);
        assert_eq!(
            storage.index(index).mutable.kind,
            IndexKind::OrdinaryUnbuilt
        );
        storage.alter_index_definition(index, ready, 8).unwrap();
        storage
            .prepare_index_definition_enforcers(index, 8, IndexBindingOperation::State)
            .unwrap();
        storage
            .prepare_commit_generations(
                8,
                core::iter::empty(),
                core::iter::once(IndexGenerationChange::State(index)),
            )
            .unwrap();
        storage.publish_index_definition(index, 8).unwrap();
        storage.finish_bindings(table, 8);
        storage.release_commit_generations(8);
        assert_eq!(storage.index(index).mutable.kind, IndexKind::OrdinaryReady);
        assert_eq!(storage.tables[table].enforcer_count(), 1);
        let mut dead = ready;
        dead.kind = IndexKind::OrdinaryDead;
        let prior_dead = storage.alter_index_definition(index, dead, 9).unwrap();
        storage
            .prepare_index_definition_enforcers(index, 9, IndexBindingOperation::State)
            .unwrap();
        assert_eq!(storage.tables[table].enforcer_count(), 0);
        let prior_ready = storage.alter_index_definition(index, ready, 9).unwrap();
        storage
            .prepare_index_definition_enforcers(index, 9, IndexBindingOperation::State)
            .unwrap();
        assert_eq!(storage.tables[table].enforcer_count(), 1);
        let prior_dead_again = storage.alter_index_definition(index, dead, 9).unwrap();
        storage
            .prepare_index_definition_enforcers(index, 9, IndexBindingOperation::State)
            .unwrap();
        assert_eq!(storage.tables[table].enforcer_count(), 0);
        storage
            .prepare_index_definition_enforcers(index, 9, IndexBindingOperation::State)
            .unwrap();
        assert_eq!(
            storage.tables[table].publication().binding_versions.len(),
            3
        );
        storage.rollback_index_definition(index, prior_dead_again);
        assert_eq!(storage.tables[table].enforcer_count(), 1);
        storage.rollback_index_definition(index, prior_ready);
        assert_eq!(storage.tables[table].enforcer_count(), 0);
        storage.rollback_index_definition(index, prior_dead);
        assert_eq!(storage.index(index).mutable.kind, IndexKind::OrdinaryReady);
        assert_eq!(storage.tables[table].enforcer_count(), 1);
    });
}

#[test]
fn session_table_leases_keep_exact_lifetimes_across_transaction_cleanup() {
    use crate::sql::ast::TableLockMode;

    let mut config = test_config();
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def("phase_lock", &[("id", ColType::Int4, false)]))
        .unwrap();
    let previous = crate::sql::execution_connection_id();
    crate::mem::guard::forbid_alloc(|| {
        crate::sql::set_execution_connection_id(41);
        let first = storage.start_transaction_identity().unwrap();
        storage
            .lock_table(first, table, TableLockMode::ShareUpdateExclusive, false)
            .unwrap();
        let lease = storage.hold_table_across_commits(first, table).unwrap();
        assert_eq!(storage.object_lock_view_count(), 1);
        storage.rollback_locks_to(first, 0);
        storage.release_object_locks(first);
        storage.finish_transaction_identity(first, true);
        assert_eq!(storage.object_lock_view_count(), 1);
        let second = storage.start_transaction_identity().unwrap();
        assert_eq!(
            storage.validate_session_table_lock(&lease, second).unwrap(),
            table
        );
        storage
            .lock_table(second, table, TableLockMode::AccessExclusive, false)
            .unwrap();
        storage.release_object_locks(second);
        storage.release_connection_object_locks(41);
        let replacement = storage.hold_table_across_commits(second, table).unwrap();
        assert_eq!(
            storage
                .validate_session_table_lock(&lease, second)
                .unwrap_err()
                .sqlstate,
            sqlstate::SERIALIZATION_FAILURE
        );
        assert_eq!(
            storage
                .validate_session_table_lock(&replacement, second)
                .unwrap(),
            table
        );

        crate::sql::set_execution_connection_id(42);
        let writer = storage.start_transaction_identity().unwrap();
        storage
            .lock_table(writer, table, TableLockMode::RowExclusive, false)
            .unwrap();
        storage.release_object_locks(writer);
        assert_eq!(
            storage
                .lock_table(writer, table, TableLockMode::AccessExclusive, false)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT
        );
        let mut blockers = [0; 2];
        assert_eq!(storage.blocking_backend_pids(42, &mut blockers), 1);
        assert_eq!(blockers[0], 41);
        storage.cancel_backend_wait(42);
        assert_eq!(storage.blocking_backend_pid_count(42), 0);
        assert_eq!(storage.object_lock_view_count(), 1);
        storage.release_connection_object_locks(42);
        assert_eq!(storage.object_lock_view_count(), 1);
        drop(lease);
        assert_eq!(
            storage
                .validate_session_table_lock(&replacement, second)
                .unwrap(),
            table
        );
        drop(replacement);
        storage
            .lock_table(writer, table, TableLockMode::AccessExclusive, false)
            .unwrap();
        storage.release_object_locks(writer);
        storage.finish_transaction_identity(writer, false);
        storage.finish_transaction_identity(second, true);
        assert_eq!(storage.object_lock_view_count(), 0);
    });
    crate::sql::set_execution_connection_id(previous);
}

#[test]
fn session_table_lease_acquisition_exhaustion_does_not_recycle_an_owner() {
    let config = test_config();
    let mut budget = test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let table = storage
        .create_table(make_def(
            "phase_exhaustion",
            &[("id", ColType::Int4, false)],
        ))
        .unwrap();
    let previous = crate::sql::execution_connection_id();
    crate::sql::set_execution_connection_id(41);
    crate::mem::guard::forbid_alloc(|| {
        let transaction = storage.start_transaction_identity().unwrap();
        storage.lock_state().sequence = u64::MAX;
        assert_eq!(
            storage
                .hold_table_across_commits(transaction, table)
                .unwrap_err()
                .sqlstate,
            sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert_eq!(storage.object_lock_view_count(), 0);
        storage.finish_transaction_identity(transaction, false);
    });
    crate::sql::set_execution_connection_id(previous);
}
