use super::*;

#[test]
fn implicit_constraint_indexes_support_ddl_without_prior_rebuild() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("implicit-index-ddl-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/235_implicit_index_ddl.sql");
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            let output = session.execute(&mut engine, statement, true);
            let expected = statement
                .contains("RENAME TO forbidden_index_name")
                .then_some("42501");
            assert_eq!(
                error_sqlstates(&output),
                expected.into_iter().collect::<Vec<_>>(),
                "{mode:?}: {statement}"
            );
            rows.extend(data_rows(&output));
        }
        assert_eq!(
            rows,
            [
                "t|t|primary constraint|primary index",
                "t|t|primary constraint|primary index",
                "{fillfactor=77}",
                "implicit_index_exclusion_done|t|t|NULL|NULL",
                "implicit_index_primary_done|t|t|primary constraint|primary index",
                "implicit_index_unique_done|t|t|NULL|NULL",
                "3|3",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn materialized_inline_keys_keep_one_catalog_row_and_their_constraint_identity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("materialized-inline-indexes-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/233_inline_index_materialization.sql"
        );
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "created|2|2|1|2",
                "materialized|2|2|1|2",
                "inline_key_materialization_pkey|t|t|t",
                "inline_key_materialization_value_key|f|t|t",
                "2|2",
                "replaced|2|2|1|2",
                "inline_key_materialization_pkey|t|t|t",
                "inline_key_materialization_value_key|f|t|t",
                "0",
                "1|10|one",
                "2|20|two"
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn table_drop_rollback_restores_only_its_own_index_drops() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("table-index-drop-ownership-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        for setup in [
            "CREATE TABLE drop_owner(id integer PRIMARY KEY, value integer)",
            "CREATE INDEX drop_first ON drop_owner(value)",
            "CREATE INDEX drop_parent ON drop_owner(value)",
            "BEGIN",
            "DROP INDEX drop_first",
            "SAVEPOINT before_table_drop",
            "DROP TABLE drop_owner",
            "ROLLBACK TO before_table_drop",
        ] {
            session.success(&mut engine, setup, true);
        }
        let indexes = "SELECT relname FROM pg_class WHERE relname IN \
            ('drop_first','drop_parent','drop_owner_pkey') ORDER BY relname";
        assert_eq!(
            data_rows(&session.success(&mut engine, indexes, true)),
            ["drop_owner_pkey", "drop_parent"],
            "{mode:?}: the earlier explicit drop survives the savepoint"
        );
        session.success(&mut engine, "DROP TABLE drop_owner", true);
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, indexes, true)),
            ["drop_first", "drop_owner_pkey", "drop_parent"],
            "{mode:?}: full rollback restores every index exactly once"
        );
        for statement in [
            "BEGIN",
            "CREATE INDEX drop_created ON drop_owner(value)",
            "DROP INDEX drop_created",
            "DROP TABLE drop_owner",
            "ROLLBACK",
        ] {
            session.success(&mut engine, statement, true);
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, indexes, true)),
            ["drop_first", "drop_owner_pkey", "drop_parent"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*) FROM pg_class WHERE relname='drop_created'",
                true,
            )),
            ["0"]
        );
    }
}

#[test]
fn reordered_partition_keys_keep_parent_constraint_and_index_links_through_reindex() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("reordered-index-links-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/229_reordered_partition_index_links.sql"
        );
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "before|p|t|t|{2}|{1}",
                "before|u|t|t|{2,1}|{1,2}",
                "after|p|t|t|{2}|{1}",
                "after|u|t|t|{2,1}|{1,2}",
                "retained|t|t|t|t",
                "row|1|7",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn generated_multicolumn_and_copied_key_names_follow_the_schema_namespace() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("generated-key-names-{mode:?}"));
        config.transaction_mode = mode;
        // The fixture keeps twenty relations, including occupied names, live.
        config.max_tables = 20;
        config.max_value_indexes = 20;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/228_generated_key_names.sql");
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "child|f|child_x_y_fkey|t",
                "copied|c|source_positive|f",
                "copied|u|copied_a_b_key1|t",
                "deferred|u|deferred_v_key1|t",
                "leaf|u|leaf_x_y_key1|t",
                "multi|u|multi_x_y_key1|t",
                "next|f|next_x_y_fkey1|t",
                "ranges|x|ranges_a_b_excl1|t",
                "third|f|third_x_y_fkey1|t",
                "balanced-multi|mmmmmmmmmmmmmmmmmmmmmmmmmmmmm_ccccccccccccccccccccccccccccc_key|63|t",
                "unicode-multi|éééééééééééééé_界界界界界界界界界_key|60|t",
                "explicit_key|t",
                "multi_x_y_key1|t",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn pending_index_and_check_names_follow_their_distinct_catalog_visibility() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (source, pending, creation, relation, expected) in [
            (
                "index",
                "CREATE TABLE a_b(c integer UNIQUE)",
                "CREATE TABLE a(b_c integer UNIQUE)",
                "a",
                "a_b_c_key1",
            ),
            (
                "table-check",
                "CREATE TABLE holder(value integer CONSTRAINT a_b_c_key CHECK(value>0))",
                "CREATE TABLE a_b(c integer UNIQUE)",
                "a_b",
                "a_b_c_key",
            ),
            (
                "domain-check",
                "CREATE DOMAIN holder AS integer CONSTRAINT a_b_c_key CHECK(VALUE>0)",
                "CREATE TABLE a_b(c integer UNIQUE)",
                "a_b",
                "a_b_c_key",
            ),
        ] {
            for outcome in ["COMMIT", "ROLLBACK"] {
                let mut config =
                    test_config(&format!("pending-key-name-{mode:?}-{source}-{outcome}"));
                config.transaction_mode = mode;
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
                let mut second =
                    ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
                first.success(&mut engine, "BEGIN", true);
                first.success(&mut engine, pending, true);
                second.success(&mut engine, "BEGIN", true);
                second.success(&mut engine, creation, true);
                let query = format!(
                    "SELECT conname FROM pg_constraint WHERE conrelid='public.{relation}'::regclass AND contype='u'"
                );
                assert_eq!(
                    data_rows(&second.success(&mut engine, &query, true)),
                    [expected],
                    "{mode:?}, {source}, {outcome}: before first transaction resolves"
                );
                first.success(&mut engine, outcome, true);
                assert_eq!(
                    data_rows(&second.success(&mut engine, &query, true)),
                    [expected],
                    "{mode:?}, {source}, {outcome}: after first transaction resolves"
                );
                second.success(&mut engine, "COMMIT", true);
                assert_eq!(
                    data_rows(&first.success(&mut engine, &query, true)),
                    [expected],
                    "{mode:?}, {source}, {outcome}: after second transaction commits"
                );
            }
        }
    }
}

#[test]
fn owner_command_events_report_only_the_table_or_materialized_view_root() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("owner-command-events-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/227_owner_event_commands.sql");
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "table-changed|1|table|public.owner_event_rows|ALTER TABLE",
                "table-unchanged|1|table|public.owner_event_rows|ALTER TABLE",
                "view-changed|1|materialized view|public.owner_event_saved|ALTER MATERIALIZED VIEW",
                "view-unchanged|1|materialized view|public.owner_event_saved|ALTER MATERIALIZED VIEW",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn inline_key_names_survive_renames_collisions_and_http_cold_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("inline-key-names-{mode:?}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        let mode_name = match mode {
            crate::config::TransactionMode::Mvcc => "mvcc",
            crate::config::TransactionMode::Locking => "locking",
        };
        config.object_store_bucket = format!("inline-key-names-{mode_name}-{}", std::process::id());
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = object_reads::HttpObjectServer::start(&mut config);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!("../../../tests/external/differential/225_inline_key_names.sql");
        let (setup, _) = fixture
            .split_once("CREATE TABLE inline_key_names.before_names AS")
            .unwrap();
        let remaining = fixture.strip_prefix(setup).unwrap();
        let expected_names = [
            "long-primary|pppppppppppppppppppppppppppppppppppppppppppppppppppppppppp_pkey|63|t",
            "balanced-unique|ttttttttttttttttttttttttttttt_ccccccccccccccccccccccccccccc_key|63|t",
            "unicode-unique|éééééééééééééé_界界界界界界界界界_key|60|t",
            "namespace-collision|a|a_b_c_key1|t",
            "namespace-collision|a_b|a_b_c_key|t",
        ];
        let mut rows = Vec::new();
        for statement in setup
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(rows, expected_names, "{mode:?}");
        engine.commit_wal().unwrap();
        assert!(server.uploads.load(std::sync::atomic::Ordering::Acquire) > 0);
        drop((session, engine, budget));
        for checkpoint in [false, true] {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
            let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            let mut recovered_names = Vec::new();
            for statement in setup
                .split(";\n")
                .map(str::trim)
                .filter(|sql| sql.starts_with("SELECT"))
            {
                recovered_names.extend(data_rows(&verifier.success(
                    &mut recovered,
                    statement,
                    true,
                )));
            }
            assert_eq!(
                recovered_names, expected_names,
                "{mode:?}, checkpoint={checkpoint}"
            );
            if checkpoint {
                let mut rows = Vec::new();
                for statement in remaining
                    .split(";\n")
                    .map(str::trim)
                    .filter(|sql| !sql.is_empty())
                {
                    rows.extend(data_rows(&verifier.success(
                        &mut recovered,
                        statement,
                        true,
                    )));
                }
                assert_eq!(
                    rows,
                    [
                        "retained-after-renames|a_b_c_key|t|t",
                        "after-reindex|t|t|t|t|t",
                        "dropped-by-retained-name|t",
                        "backing-relations-removed|t",
                    ],
                    "{mode:?}"
                );
            } else {
                assert!(recovered.checkpoint().unwrap());
            }
            recovered.commit_wal().unwrap();
            assert!(recovered.fatal_commit_failure().is_none());
        }
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}

#[test]
fn index_creation_and_reindex_keep_owners_and_grants_through_http_cold_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("reindex-ownership-{mode:?}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        let mode_name = match mode {
            crate::config::TransactionMode::Mvcc => "mvcc",
            crate::config::TransactionMode::Locking => "locking",
        };
        config.object_store_bucket =
            format!("reindex-ownership-{mode_name}-{}", std::process::id());
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = object_reads::HttpObjectServer::start(&mut config);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/224_reindex_ownership.sql");
        let (setup, cleanup) = fixture.split_once("DROP TABLE reindex_owned;").unwrap();
        let mut rows = Vec::new();
        for statement in setup
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
            if statement.starts_with("CREATE INDEX") || statement.starts_with("REINDEX") {
                let owner = engine
                    .storage
                    .find_role_visible("reindex_owner", 0)
                    .unwrap();
                crate::mem::guard::forbid_alloc(|| {
                    for slot in 0..engine.storage.index_count() {
                        if let Some(index) = engine.storage.index_visible_to(slot, 0) {
                            assert_eq!(
                                usize::from(index.ownership.owner_to(0)),
                                owner,
                                "{mode:?}: {statement}"
                            );
                            assert!(
                                index.resolved_operator_classes[..index.n_cols]
                                    .iter()
                                    .all(Option::is_some),
                                "{mode:?}: {statement} must preserve every key's comparison class"
                            );
                        }
                    }
                });
            }
        }
        assert_eq!(rows, ["t|t", "t|t", "t|t", "t|t"], "{mode:?}");
        engine.commit_wal().unwrap();
        assert!(server.uploads.load(std::sync::atomic::Ordering::Acquire) > 0);
        drop((session, engine, budget));
        // First recover solely from uploaded journal records, then from a
        // checkpoint. Deleting the entire local directory removes both caches.
        for checkpoint in [false, true] {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
            let mut verifier = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            let mut recovered_rows = Vec::new();
            for statement in setup
                .split(";\n")
                .map(str::trim)
                .filter(|sql| sql.starts_with("SELECT"))
            {
                recovered_rows.extend(data_rows(&verifier.success(
                    &mut recovered,
                    statement,
                    true,
                )));
            }
            assert_eq!(
                recovered_rows,
                ["t|t", "t|t", "t|t", "t|t"],
                "{mode:?}, checkpoint={checkpoint}"
            );
            assert_eq!(
                data_rows(&verifier.success(
                    &mut recovered,
                    "SELECT id,payload FROM reindex_owned",
                    true
                )),
                ["1|value"]
            );
            let owner = recovered
                .storage
                .find_role_visible("reindex_owner", 0)
                .unwrap();
            crate::mem::guard::forbid_alloc(|| {
                for slot in 0..recovered.storage.index_count() {
                    if let Some(index) = recovered.storage.index_visible_to(slot, 0) {
                        assert_eq!(
                            usize::from(index.ownership.owner_to(0)),
                            owner,
                            "{mode:?}, checkpoint={checkpoint}"
                        );
                    }
                }
            });
            if checkpoint {
                verifier.success(&mut recovered, "DROP TABLE reindex_owned", true);
                verifier.success(&mut recovered, cleanup, true);
            } else {
                verifier.success(&mut recovered, "SET ROLE reindex_maintainer", true);
                verifier.success(
                    &mut recovered,
                    "REINDEX TABLE CONCURRENTLY reindex_owned",
                    true,
                );
                verifier.success(&mut recovered, "RESET ROLE", true);
                assert!(recovered.checkpoint().unwrap());
            }
            recovered.commit_wal().unwrap();
            assert!(recovered.fatal_commit_failure().is_none());
        }
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}

#[test]
fn reindex_event_receipts_resolve_renames_and_skip_dropped_or_reused_index_slots() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("reindex-event-receipts-{mode:?}"));
        config.transaction_mode = mode;
        config.max_indexes = 1;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let receipt_arena =
            Arena::new(&mut budget, "completed index event receipt", 1 << 16).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE receipt_table(value integer); CREATE INDEX receipt_index ON receipt_table(value)", true);
        let table = engine
            .storage
            .find_visible("public", "receipt_table", 0)
            .unwrap();
        let slot = engine
            .storage
            .index_slot("public", "receipt_index", 0)
            .unwrap();
        let birth = engine.storage.index_visible_to(slot, 0).unwrap().created_at;
        let replacement = crate::storage::IndexReplacement {
            table,
            old_slot: slot,
            old_created_at: birth,
            new_slot: slot,
            new_created_at: birth,
        };
        let graphs = crate::mem::guard::forbid_alloc(|| {
            event_trigger::completed_reindex_graphs(
                &engine.storage,
                0,
                &[replacement],
                "REINDEX",
                false,
                &receipt_arena,
            )
            .unwrap()
        });
        assert_eq!(graphs.commands.len(), 1);
        for (command, count, identity) in [
            (
                "ALTER INDEX receipt_index RENAME TO renamed_receipt",
                "1",
                Some("public.renamed_receipt"),
            ),
            ("DROP INDEX renamed_receipt", "0", None),
            (
                "CREATE INDEX reused_receipt ON receipt_table(value)",
                "0",
                None,
            ),
        ] {
            session.success(&mut engine, command, true);
            let _commands = event_trigger::enter_ddl_commands(graphs.commands);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT count(*) FROM pg_event_trigger_ddl_commands()",
                    true
                )),
                [count],
                "{mode:?}: {command}"
            );
            let values = data_rows(&session.success(
                &mut engine,
                "SELECT object_identity FROM pg_event_trigger_ddl_commands()",
                true,
            ));
            assert_eq!(
                values,
                identity.into_iter().collect::<Vec<_>>(),
                "{mode:?}: {command}"
            );
            if identity.is_none() {
                crate::mem::guard::forbid_alloc(|| {
                    let absent = event_trigger::completed_reindex_graphs(
                        &engine.storage,
                        0,
                        &[replacement],
                        "REINDEX",
                        false,
                        &receipt_arena,
                    )
                    .unwrap();
                    assert!(absent.commands.is_empty());
                });
            }
        }
        assert_eq!(
            engine.storage.index_slot("public", "reused_receipt", 0),
            Some(slot)
        );
        assert_ne!(
            engine.storage.index_visible_to(slot, 0).unwrap().created_at,
            birth
        );
        crate::mem::guard::forbid_alloc(|| {
            assert!(
                graphs.commands[0]
                    .object(&engine.storage, 0)
                    .unwrap()
                    .is_none()
            );
        });
        let current = crate::mem::guard::forbid_alloc(|| {
            event_trigger::completed_reindex_graphs(
                &engine.storage,
                0,
                &[crate::storage::IndexReplacement {
                    new_created_at: engine.storage.index_visible_to(slot, 0).unwrap().created_at,
                    ..replacement
                }],
                "REINDEX",
                false,
                &receipt_arena,
            )
            .unwrap()
        });
        let mixed = [graphs.commands[0], current.commands[0]];
        let _commands = event_trigger::enter_ddl_commands(&mixed);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*) FROM pg_event_trigger_ddl_commands()",
                true
            )),
            ["1"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT object_identity FROM pg_event_trigger_ddl_commands()",
                true
            )),
            ["public.reused_receipt"]
        );
    }
}

#[test]
fn deferrable_index_replacement_keeps_its_physical_immediacy_and_constraint_timing() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("deferrable-index-identities-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/223_deferrable_index_identities.sql"
        );
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            ["before|f", "after|f|t|t", "2", "ordinary|f|t|t"],
            "{mode:?}"
        );
    }
}

#[test]
fn toast_index_replacement_preserves_virtual_relation_metadata_and_dependencies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("toast-index-identities-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/222_toast_index_identities.sql");
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            if statement.starts_with("DROP TABLE") {
                let table = engine
                    .storage
                    .find_visible("public", "reindex_toast_identity", 0)
                    .unwrap();
                let table_oid = catalog::user_table_oid(&engine.storage, table);
                let expected = format!(
                    "CREATE UNIQUE INDEX pg_toast_{table_oid}_index ON pg_toast.pg_toast_{table_oid} USING btree (chunk_id, chunk_seq)|chunk_id|chunk_seq|"
                );
                let definition = session.success(
                    &mut engine,
                    "SELECT pg_get_indexdef(i.indexrelid),pg_get_indexdef(i.indexrelid,1,true),\
                     pg_get_indexdef(i.indexrelid,2,true),pg_get_indexdef(i.indexrelid,3,true) \
                     FROM pg_class c JOIN pg_index i ON i.indrelid=c.reltoastrelid \
                     WHERE c.oid='reindex_toast_identity'::regclass",
                    true,
                );
                assert_eq!(data_rows(&definition), [expected], "{mode:?}");
            }
            rows.extend(data_rows(&session.success(&mut engine, statement, true)));
        }
        assert_eq!(
            rows,
            [
                "before|t|t|t|t|t|t|1 2|2|2|0 0|oid_ops,int4_ops",
                "attributes|1|chunk_id|26|f",
                "attributes|2|chunk_seq|23|f",
                "before-dependencies|pg_class|index|0|pg_class|toast|1|a",
                "before-dependencies|pg_class|index|0|pg_class|toast|2|a",
                "before-dependencies|pg_class|toast|0|pg_class|owner|0|i",
                "after|t|t|t|t|t|t|1 2|2|2|0 0|oid_ops,int4_ops|t|t",
                "after-dependencies|pg_class|index|0|pg_class|toast|1|a",
                "after-dependencies|pg_class|index|0|pg_class|toast|2|a",
                "after-dependencies|pg_class|toast|0|pg_class|owner|0|i",
                "t",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn constraint_identities_survive_replaced_indexes_and_keep_exact_catalog_edges() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("constraint-index-identities-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/221_constraint_index_identities.sql"
        );
        let mut rows = Vec::new();
        // The fixture ends statements on their own lines; routine-body
        // semicolons stay inside the same statement.
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            let output = session.success(&mut engine, statement, true);
            rows.extend(data_rows(&output));
        }
        assert_eq!(
            rows,
            [
                "t|t|t",
                "constraint retained|index retained",
                "t",
                "t|named retained",
                "t|t|t",
                "t",
                "t",
                "t",
                "t",
                "t",
                "t",
            ],
            "{mode:?}"
        );
    }
}
