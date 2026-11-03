use super::*;

#[test]
fn expression_source_positions_do_not_change_grouping_or_ordering_identity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("expression-source-position-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT abs(-(v+0))::integer,count(*) FROM (VALUES(1),(2),(1)) input(v) \
             GROUP BY abs(-(v+0))::integer ORDER BY abs(-(v+0))::integer",
                true
            )),
            ["1|2", "2|1"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT abs(-(v+0))::integer FROM (VALUES(2),(1),(1)) input(v) \
             ORDER BY abs(-(v+0))::integer",
                true
            )),
            ["1", "1", "2"]
        );
    }
}

#[test]
fn implicit_assignment_cast_bodies_use_each_static_dml_site_once() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("assignment-source-position-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE assignment_input_checks; CREATE TYPE assignment_choice AS ENUM('a'); \
             CREATE FUNCTION assignment_allowed(value assignment_choice) RETURNS boolean LANGUAGE plpgsql AS \
             $$BEGIN PERFORM nextval('assignment_input_checks'); RETURN true; END$$; \
             CREATE DOMAIN assignment_checked AS assignment_choice CHECK(assignment_allowed(VALUE)); \
             CREATE FUNCTION assignment_text(integer) RETURNS text LANGUAGE sql AS \
             $$SELECT format('%s',$1) WHERE cardinality('{a}'::assignment_checked[])=1$$; \
             CREATE CAST(integer AS text) WITH FUNCTION assignment_text(integer) AS ASSIGNMENT; \
             CREATE TABLE assignment_values(id integer PRIMARY KEY,value text); \
             ALTER SEQUENCE assignment_input_checks RESTART WITH 1",true);
        for (query, expected) in [
            ("INSERT INTO assignment_values VALUES(1,1),(2,2)", "2"),
            (
                "INSERT INTO assignment_values SELECT value,value FROM generate_series(3,5) source(value)",
                "3",
            ),
            ("UPDATE assignment_values SET value=id", "4"),
            (
                "UPDATE assignment_values target SET value=source.value FROM (VALUES(1,11),(2,12)) source(id,value) WHERE target.id=source.id",
                "5",
            ),
            (
                "INSERT INTO assignment_values VALUES(1,'ignored'),(2,'ignored') ON CONFLICT(id) DO UPDATE SET value=excluded.id",
                "6",
            ),
            (
                "MERGE INTO assignment_values target USING (VALUES(1,21),(6,26)) source(id,value) ON target.id=source.id \
              WHEN MATCHED THEN UPDATE SET value=source.value \
              WHEN NOT MATCHED THEN INSERT(id,value) VALUES(source.id,source.value)",
                "8",
            ),
        ] {
            session.success(&mut engine, query, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT currval('assignment_input_checks')",
                    true
                )),
                [expected],
                "{query}"
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT id,value FROM assignment_values ORDER BY id",
                true
            )),
            ["1|21", "2|2", "3|3", "4|4", "5|5", "6|26"]
        );
    }
}

#[test]
fn completed_cursor_receipts_restore_sparse_canonical_sequence_session_values() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cursor-sequence-session-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut connection = ConfiguredTransactionSession::new(&config, &mut budget);
        connection.success(&mut engine, "CREATE SEQUENCE receipt_values CACHE 1", true);
        let slot = engine
            .storage
            .sequence_slot("public", "receipt_values", 0)
            .unwrap();
        let birth = engine.storage.sequence(slot).created_at;
        let mut arena = Arena::new(&mut budget, "cursor sparse session replay", 64 << 10).unwrap();
        let session = connection.guc.seq_session();
        crate::mem::guard::forbid_alloc(|| {
            {
                let _scope = crate::sql::sequence::enter_statement(&arena, session, false);
                let values = crate::sql::sequence::statement_effect(
                    crate::sql::sequence::StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    crate::sql::sequence::StatementEffectRetry::Continue,
                    &arena,
                    || {
                        let value =
                            crate::sql::sequence::next_cached(&engine.storage, session, slot, 0)
                                .unwrap();
                        crate::sql::sequence::StatementEffectAttempt {
                            result: Ok(arena.alloc_slice_copy(&[Datum::Int8(value)]).unwrap()),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                assert!(matches!(values, [Datum::Int8(1)]));
                assert_eq!(
                    crate::sql::sequence::next_cached(&engine.storage, session, slot, 0).unwrap(),
                    2
                );
                assert_eq!(session.currval(slot, birth), Some(2));
            }
            arena.recycle_front();
            {
                let scope = crate::sql::sequence::enter_statement(&arena, session, true);
                let values = crate::sql::sequence::statement_effect(
                    crate::sql::sequence::StatementEffectKind::Cursor,
                    &[Datum::Text("fetch")],
                    crate::sql::sequence::StatementEffectRetry::Continue,
                    &arena,
                    || panic!("completed cursor source must not advance again"),
                )
                .unwrap();
                assert!(matches!(values, [Datum::Int8(1)]));
                assert_eq!(session.currval(slot, birth), Some(1));
                assert_eq!(session.lastval(), Some(1));
                assert_eq!(
                    crate::sql::sequence::next_cached(&engine.storage, session, slot, 0).unwrap(),
                    2
                );
                assert_eq!(session.currval(slot, birth), Some(2));
                scope.finish(session).unwrap();
            }
            assert_eq!(engine.storage.sequence_value_for(slot, 0).0, 2);
            assert!(!session.retains_statement_effects());
        });
    }
}

// PostgreSQL 18.6 procedure phase SQL SHA256:
// 40eacd789e95cee4b796a91ce08125a5f8e77d1a0d36f89e88c1aa828680ea12;
// DISCARD and fresh-backend control:
// d5d8c14547aa1ed34b485c26921cfca7f64f2d0af24c2be8a573740ea0d62ad6.
#[test]
fn sql_procedure_inputs_keep_backend_and_stored_definition_lifetimes_through_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for checkpoint in [false, true] {
            let mut config =
                test_config(&format!("procedure-input-lifetime-{mode:?}-{checkpoint}"));
            config.transaction_mode = mode;
            let server = object_reads::HttpObjectServer::start(&mut config);
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            session.success(&mut engine,
                "CREATE SEQUENCE procedure_checks; CREATE TYPE procedure_choice AS ENUM('a'); \
                 CREATE DOMAIN procedure_checked AS procedure_choice CHECK(nextval('procedure_checks')>0); \
                 CREATE PROCEDURE checked_string_procedure() LANGUAGE sql AS 'SELECT ''{a}''::procedure_checked[]'",true);
            let counter = "SELECT last_value FROM procedure_checks";
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                ["1"]
            );
            for _ in 0..2 {
                session.success(&mut engine, "CALL checked_string_procedure()", true);
                assert_eq!(
                    data_rows(&session.success(&mut engine, counter, true)),
                    ["2"]
                );
            }
            session.success(
                &mut engine,
                "CREATE PROCEDURE checked_atomic_procedure() LANGUAGE sql \
                 BEGIN ATOMIC SELECT '{a}'::procedure_checked[]; END",
                true,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                ["3"]
            );
            session.success(
                &mut engine,
                "CALL checked_atomic_procedure(); CALL checked_atomic_procedure()",
                true,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                ["3"]
            );
            if checkpoint {
                assert!(engine.checkpoint().unwrap());
            }
            engine.commit_wal().unwrap();
            drop((session, engine));
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
            let mut reader = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            assert_eq!(
                data_rows(&reader.success(&mut recovered, counter, true)),
                ["3"]
            );
            reader.success(&mut recovered, "CALL checked_atomic_procedure()", true);
            assert_eq!(
                data_rows(&reader.success(&mut recovered, counter, true)),
                ["3"]
            );
            for _ in 0..2 {
                reader.success(&mut recovered, "CALL checked_string_procedure()", true);
                assert_eq!(
                    data_rows(&reader.success(&mut recovered, counter, true)),
                    ["4"]
                );
            }
            reader.success(
                &mut recovered,
                "DISCARD PLANS; CALL checked_string_procedure(); CALL checked_atomic_procedure()",
                true,
            );
            assert_eq!(
                data_rows(&reader.success(&mut recovered, counter, true)),
                ["5"]
            );
            drop((reader, recovered));
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            drop(server);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }
}
