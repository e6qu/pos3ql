use super::*;

#[test]
fn trigger_transition_relations_bind_before_atomic_type_analysis() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("transition-input-analysis-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE DOMAIN unrelated_transition_domain AS integer CHECK(VALUE>=0); \
             CREATE TABLE transition_input_source(id integer); \
             CREATE TABLE transition_input_audit(kind text,rows bigint,total bigint); \
             CREATE FUNCTION transition_input_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
             DECLARE initial_count bigint := (SELECT count(*) FROM inserted_rows); current_count bigint; \
             BEGIN SELECT count(*) INTO current_count FROM inserted_rows; \
             IF initial_count<>current_count THEN RAISE EXCEPTION 'transition row count changed'; END IF; \
             INSERT INTO transition_input_audit SELECT 'static',count(*),sum(id) FROM inserted_rows; \
             EXECUTE 'INSERT INTO transition_input_audit SELECT ''dynamic'',count(*),sum(id) FROM inserted_rows'; \
             INSERT INTO transition_input_audit SELECT 'shadow',count(*),sum(id) FROM \
               (WITH inserted_rows AS (SELECT 9 AS id) SELECT id FROM inserted_rows) scoped; \
             RETURN NULL; END$$; \
             CREATE TRIGGER transition_input_trigger AFTER INSERT ON transition_input_source \
             REFERENCING NEW TABLE AS inserted_rows FOR EACH STATEMENT \
             EXECUTE FUNCTION transition_input_trigger()", true);
        session.success(
            &mut engine,
            "INSERT INTO transition_input_source VALUES(1),(2)",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT kind,rows,total FROM transition_input_audit ORDER BY kind",
                true
            )),
            ["dynamic|2|3", "shadow|1|9", "static|2|3"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn planner_constant_facts_release_numeric_scratch_without_heap_allocation() {
    let config = test_config("planner-constant-scratch");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let engine = Engine::new(&config, &mut budget).unwrap();
    let arena = Arena::new(&mut budget, "planner constant scratch", 4096).unwrap();
    let digits = "1234567890".repeat(40);
    let literal = Expr::Str(&digits);
    let expression = Expr::Cast {
        source_position: ast::SourcePosition::UNKNOWN,
        operand: &literal,
        type_name: "numeric",
        type_mod: -1,
        binding: ast::CastBinding::Builtin,
    };
    let before = arena.used();
    crate::mem::guard::forbid_alloc(|| {
        for _ in 0..1024 {
            assert_eq!(
                planner_inputs::inline::constant_value(
                    &expression,
                    &exec::NoCols,
                    &engine.storage,
                    0,
                    &arena,
                )
                .unwrap(),
                Some(planner_inputs::inline::ConstantValue::NonNull),
            );
            assert_eq!(arena.used(), before);
        }
    });
}

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
// PostgreSQL compares these assignment counts in differential/205_planner_checked_inputs.sql.
fn assignment_cast_planner_attempts_share_one_backend_runtime_body() {
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
            ("INSERT INTO assignment_values VALUES(1,1),(2,2)", "3"),
            (
                "INSERT INTO assignment_values SELECT value,value FROM generate_series(3,5) source(value)",
                "4",
            ),
            ("UPDATE assignment_values SET value=id", "5"),
            (
                "UPDATE assignment_values target SET value=source.value FROM (VALUES(1,11),(2,12)) source(id,value) WHERE target.id=source.id",
                "6",
            ),
            (
                "INSERT INTO assignment_values VALUES(1,'ignored'),(2,'ignored') ON CONFLICT(id) DO UPDATE SET value=excluded.id",
                "7",
            ),
            (
                "MERGE INTO assignment_values target USING (VALUES(1,21),(6,26)) source(id,value) ON target.id=source.id \
              WHEN MATCHED THEN UPDATE SET value=source.value \
              WHEN NOT MATCHED THEN INSERT(id,value) VALUES(source.id,source.value)",
                "9",
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
fn accepted_assignment_inline_inputs_follow_expanded_projection_positions() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("expanded-assignment-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE projection_checks; CREATE TYPE projection_choice AS ENUM('a'); \
             CREATE DOMAIN projection_checked AS projection_choice CHECK(nextval('projection_checks')>0); \
             CREATE FUNCTION projection_text(integer) RETURNS text LANGUAGE sql AS \
             $$SELECT format('%s',$1) || CASE WHEN cardinality('{a}'::projection_checked[])=1 THEN '' ELSE '' END$$; \
             CREATE CAST(integer AS text) WITH FUNCTION projection_text(integer) AS ASSIGNMENT; \
             CREATE TABLE projection_values(id integer PRIMARY KEY,value text); \
             ALTER SEQUENCE projection_checks RESTART WITH 1", true);
        {
            let routine = engine
                .storage
                .function_for_call_syntax_oids(
                    "projection_text",
                    &[crate::sql::types::oid::INT4],
                    false,
                    session.transaction.txid,
                )
                .unwrap();
            let body = routine.body.as_str();
            let mut parser = Parser::new(body, &session.arena)
                .unwrap()
                .with_routine_parameters("projection_text", routine.arguments())
                .unwrap();
            parser.capture_type_inputs();
            let statement = parser.next_stmt().unwrap().unwrap();
            assert!(parser.next_stmt().unwrap().is_none());
            {
                let catalog = query::storage_catalog(
                    &engine.storage,
                    &session.arena,
                    session.transaction.txid,
                );
                let _formal = exec::enter_routine_parameter_types(
                    routine.arguments(),
                    Some(&catalog),
                    ast::Collation::None,
                )
                .unwrap();
                query::capture_contextual_type_inputs(
                    &parser,
                    &statement,
                    &engine.storage,
                    session.transaction.txid,
                    &session.arena,
                )
                .unwrap();
            }
            let bindings = type_input_plans::with_owner(type_input_plans::Owner::Backend, || {
                engine.bind_type_inputs(
                    parser.type_input_candidates(),
                    &session.arena,
                    &mut session.transaction,
                    &mut session.pool,
                    &mut session.cursors,
                    &session.guc,
                    &mut Responder::new(&mut session.send),
                )
            })
            .unwrap()
            .unwrap();
            assert!(
                bindings.iter().any(|binding| !binding.encoded.is_empty()),
                "checked input was admitted"
            );
            let mut parser = Parser::new(body, &session.arena)
                .unwrap()
                .with_routine_parameters("projection_text", routine.arguments())
                .unwrap();
            parser.capture_type_inputs();
            parser.set_type_inputs(bindings);
            let statements = [parser.next_stmt().unwrap().unwrap()];
            type_inputs::validate_restored(&parser, &engine.storage, session.transaction.txid)
                .unwrap();
            let catalog =
                query::storage_catalog(&engine.storage, &session.arena, session.transaction.txid);
            let _formal = exec::enter_routine_parameter_types(
                routine.arguments(),
                Some(&catalog),
                ast::Collation::None,
            )
            .unwrap();
            let columns = exec::CatalogCols {
                definition: None,
                alias: None,
                output_aliases: &[],
                storage: &engine.storage,
                txid: session.transaction.txid,
                arena: &session.arena,
            };
            let arguments = [planner_inputs::inline::literal_argument(
                Datum::Int4(11),
                crate::sql::types::oid::INT4,
                &session.arena,
            )
            .unwrap()];
            assert!(
                planner_inputs::inline::accepted(
                    &routine,
                    &statements,
                    &arguments,
                    &planner_inputs::inline::Context {
                        storage: &engine.storage,
                        txid: session.transaction.txid,
                        arena: &session.arena,
                        body_columns: &columns
                    }
                )
                .unwrap(),
                "bound checked format body must be eligible: {statements:?}"
            );
        }
        session.success(
            &mut engine,
            "ROLLBACK; ALTER SEQUENCE projection_checks RESTART WITH 1",
            true,
        );
        for (query, expected) in [
            (
                "INSERT INTO projection_values(value,id) SELECT value,id FROM (VALUES(11,1)) source(value,id)",
                "1",
            ),
            (
                "INSERT INTO projection_values(value,id) SELECT * FROM (VALUES(22,2)) source(value,id)",
                "2",
            ),
            (
                "INSERT INTO projection_values(value,id) SELECT source.* FROM (VALUES(33,3)) source(value,id)",
                "3",
            ),
            (
                "INSERT INTO projection_values(value,id) SELECT (source).* FROM (VALUES(44,4)) source(value,id)",
                "4",
            ),
            (
                "INSERT INTO projection_values(value,id) SELECT 55,5 UNION ALL SELECT 66,6",
                "5",
            ),
        ] {
            session.success(&mut engine, query, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT last_value FROM projection_checks",
                    true
                )),
                [expected],
                "{query}"
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT id,value FROM projection_values ORDER BY id",
                true
            )),
            ["1|11", "2|22", "3|33", "4|44", "5|55", "6|66"]
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
            config.object_store_on = true;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.object_store_bucket = format!(
                "procedure-input-lifetime-{}-{}-{checkpoint}",
                std::process::id(),
                if mode == crate::config::TransactionMode::Mvcc {
                    "mvcc"
                } else {
                    "locking"
                }
            );
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
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

// Original PostgreSQL 18.6 cursor-phase probe SQL SHA256:
// 9142d2785fc9532152ee23a1daf38adad5d0ceeab4d7133f614843cdc4e33579.
// Additional constant cases are compared in differential/205_planner_checked_inputs.sql.
#[test]
fn cursor_planning_checks_inline_inputs_without_running_function_bodies() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cursor-inline-input-phase-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE phase_checks; CREATE TYPE phase_choice AS ENUM('a'); \
             CREATE TABLE phase_audit(value phase_choice); CREATE TABLE phase_mutations(id integer); \
             CREATE TABLE phase_empty(id integer); \
             CREATE FUNCTION phase_allowed(value phase_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
              PERFORM nextval('phase_checks'); INSERT INTO phase_audit VALUES(value); RETURN true; END$$; \
             CREATE DOMAIN phase_checked AS phase_choice CHECK(phase_allowed(VALUE)); \
             CREATE FUNCTION phase_plain() RETURNS phase_checked[] LANGUAGE sql AS 'SELECT ''{a}''::phase_checked[]'; \
             CREATE FUNCTION phase_scalar() RETURNS integer LANGUAGE sql AS 'SELECT cardinality(''{a}''::phase_checked[])'; \
             CREATE FUNCTION phase_opaque() RETURNS phase_checked[] LANGUAGE sql SET search_path=public AS 'SELECT ''{a}''::phase_checked[]'; \
             CREATE FUNCTION phase_where() RETURNS phase_checked[] LANGUAGE sql AS 'SELECT ''{a}''::phase_checked[] WHERE true'; \
             CREATE FUNCTION phase_dml() RETURNS phase_checked[] LANGUAGE sql AS 'INSERT INTO phase_mutations VALUES(1) RETURNING ''{a}''::phase_checked[]'; \
             CREATE FUNCTION phase_strict(value integer) RETURNS phase_checked[] LANGUAGE sql STRICT AS 'SELECT ''{a}''::phase_checked[]'", true);
        let counter = "SELECT CASE WHEN is_called THEN last_value ELSE 0 END, \
             (SELECT count(*) FROM phase_audit),(SELECT count(*) FROM phase_mutations) FROM phase_checks";
        for (query, declared, fetched) in [
            (
                "SELECT CASE WHEN false THEN phase_plain() ELSE NULL END FROM generate_series(1,2)",
                "0|0|0",
                "0|0|0",
            ),
            (
                "SELECT CASE WHEN NOT true THEN phase_plain() ELSE NULL END",
                "0|0|0",
                "0|0|0",
            ),
            (
                "SELECT CASE WHEN 1=2 THEN phase_plain() ELSE NULL END",
                "0|0|0",
                "0|0|0",
            ),
            (
                "SELECT CASE 1 WHEN 2 THEN phase_plain() ELSE NULL END",
                "0|0|0",
                "0|0|0",
            ),
            (
                "SELECT CASE 1 WHEN 1 THEN NULL ELSE phase_plain() END",
                "0|0|0",
                "0|0|0",
            ),
            (
                "SELECT false AND cardinality(phase_plain())=1",
                "0|0|0",
                "0|0|0",
            ),
            (
                "SELECT true OR cardinality(phase_plain())=1",
                "0|0|0",
                "0|0|0",
            ),
            ("SELECT coalesce(1+1,phase_scalar())", "0|0|0", "0|0|0"),
            (
                "SELECT phase_plain() FROM generate_series(1,0)",
                "1|1|0",
                "1|1|0",
            ),
            ("SELECT phase_plain() WHERE false", "1|1|0", "1|1|0"),
            (
                "SELECT phase_opaque() FROM generate_series(1,0)",
                "0|0|0",
                "0|0|0",
            ),
            ("SELECT phase_where()", "1|1|0", "2|2|0"),
            ("SELECT phase_dml()", "1|1|0", "2|2|1"),
            ("SELECT phase_strict(NULL)", "0|0|0", "0|0|0"),
            ("SELECT phase_strict(id) FROM phase_empty", "1|1|0", "1|1|0"),
            (
                "SELECT phase_strict(id) FROM (VALUES(1)) source(id)",
                "1|1|0",
                "2|2|0",
            ),
        ] {
            session.success(&mut engine,
                "ALTER SEQUENCE phase_checks RESTART WITH 1; TRUNCATE phase_audit,phase_mutations; BEGIN", true);
            session.success(
                &mut engine,
                &format!("DECLARE phase_cursor NO SCROLL CURSOR FOR {query}"),
                true,
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [declared],
                "DECLARE {query}"
            );
            session.success(&mut engine, "FETCH ALL FROM phase_cursor", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [fetched],
                "FETCH {query}"
            );
            session.success(&mut engine, "CLOSE phase_cursor; COMMIT", true);
        }
    }
}
