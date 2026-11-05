use super::*;

#[test]
fn caught_procedural_errors_restore_session_authorization_inside_the_effective_owner_scope() {
    let fixture =
        include_str!("../../../tests/external/differential/240_procedural_identity_rollback.sql");
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-identity-rollback-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        assert_eq!(
            data_rows(&session.success(&mut engine, fixture, true)),
            ["t", "t|t", "t", "t|t|t"],
            "{mode:?}"
        );
    }
}

#[test]
fn caught_trigger_errors_keep_transition_values_and_remove_aborted_deferred_events() {
    let fixture = include_str!(
        "../../../tests/external/differential/239_trigger_subtransaction_visibility.sql"
    );
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("trigger-subtransaction-visibility-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        assert_eq!(
            data_rows(&session.success(&mut engine, fixture, true)),
            [
                "11",
                "12",
                "after|11|kept",
                "after|12|kept",
                "before|11|kept",
                "before|12|kept",
                "statement|2|kept",
                "99",
                "99",
                "kept",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn caught_procedural_errors_restore_schema_settings_cursors_and_deferred_triggers() {
    let fixture =
        include_str!("../../../tests/external/differential/236_procedural_subtransactions.sql");
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-subtransactions-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let rows = data_rows(&session.success(&mut engine, fixture, true));
        assert_eq!(
            rows,
            [
                "9",
                "handler|9:kept:ISO, MDY",
                "3|60",
                "t|t|t",
                "1|t",
                "drop|60",
                "inner|outer",
                "kept|0",
                "7",
                "7",
                "1",
                "subtx_old",
                "2",
                "12",
                "kept|ISO, MDY",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn savepoint_rollback_closes_new_failed_cursors_and_preserves_old_failed_names() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("savepoint-failed-cursors-{mode:?}"));
        config.transaction_mode = mode;
        config.max_cursors = 2;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/237_cursor_savepoint_lifetimes.sql");
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            let expected = if statement.contains("-- error: 22012") {
                Some("22012")
            } else if statement.contains("-- error: 55000") {
                Some("55000")
            } else {
                None
            };
            let output = session.execute(&mut engine, statement, true);
            assert_eq!(
                error_sqlstates(&output),
                expected.into_iter().collect::<Vec<_>>(),
                "{mode:?}: {statement}: {}",
                String::from_utf8_lossy(&output)
            );
            rows.extend(data_rows(&output));
        }
        assert_eq!(rows, ["old_cursor", "7", "0"], "{mode:?}");
    }
}

#[test]
fn caught_schema_changes_keep_only_successful_journal_and_checkpoint_state_after_cold_restart() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-subtransaction-recovery-{mode:?}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.object_store_bucket = format!(
            "procedural-subtransaction-recovery-{mode:?}-{}",
            std::process::id()
        )
        .to_lowercase();
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.block_cache_bytes = 0;
        config.disk_cache_bytes = 0;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let server = object_reads::HttpObjectServer::start(&mut config);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            include_str!("../../../tests/external/differential/238_caught_schema_recovery.sql"),
            true,
        );
        engine.commit_wal().unwrap();
        assert!(server.uploads.load(std::sync::atomic::Ordering::Acquire) > 0);
        drop((session, engine, budget));
        for checkpoint in [false, true] {
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            assert_eq!(data_rows(&session.success(&mut engine,
                "SELECT id FROM subtx_durable ORDER BY id; \
                 SELECT to_regclass('subtx_not_durable') IS NULL, \
                   NOT EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='subtx_durable'::regclass AND attname='discarded' AND NOT attisdropped)", true)),
                ["1", "3", "t|t"], "{mode:?}, checkpoint={checkpoint}");
            if !checkpoint {
                assert!(engine.checkpoint().unwrap());
            }
            engine.commit_wal().unwrap();
            assert!(engine.fatal_commit_failure().is_none());
        }
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}
