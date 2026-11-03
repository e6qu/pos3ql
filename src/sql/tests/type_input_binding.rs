use super::*;

// PG18.6 execution-reference.zip, SQL SHA256:
// f0793cd42fbcbffd777ee574a5283fd2a159bbfff1df3063b6bb30ee5222c5b1.
#[test]
fn procedural_queries_finish_nested_calls_before_publishing_rows_or_locals() {
    let fixture =
        include_str!("../../../tests/external/differential/218_live_procedural_queries.sql");
    let setup = fixture
        .split("-- Execute and inspect the completed results.")
        .next()
        .unwrap();
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("live-procedural-queries-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine, setup, true);
        session.success(&mut engine, "CALL procedural_query_run()", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT kind,value FROM procedural_query_log ORDER BY position; \
             SELECT last_value FROM procedural_query_calls; SELECT last_value FROM procedural_query_checks", true)),
            ["perform|3", "select|6", "for|7", "for|9", "record|9", "record|11",
             "execute_into|11", "execute_for|12", "execute_for|14", "execute_record|14",
             "execute_record|16", "execute_discard|1", "14", "1"], "{mode:?}");
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT * FROM procedural_query_rows(); SELECT last_value FROM procedural_query_calls; \
             SELECT last_value FROM procedural_query_checks", true)),
            ["17", "19", "19", "21", "18", "1"], "{mode:?}");
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT * FROM procedural_query_caught(); SELECT last_value FROM procedural_query_calls; \
             SELECT * FROM procedural_query_caught_dynamic(); SELECT last_value FROM procedural_query_calls; \
             SELECT last_value FROM procedural_query_checks", true)),
            ["31", "99", "20", "33", "98", "22", "1"], "{mode:?}");
        let setup = fixture
            .split("-- Bounded INTO setup.")
            .nth(1)
            .unwrap()
            .split("-- Execute bounded INTO cases.")
            .next()
            .unwrap();
        session.success(&mut engine, setup, true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "CALL procedural_into_run(); SELECT kind,selected,effects,consumed FROM procedural_into_log ORDER BY kind", true)),
            ["dynamic|4|6|3", "dynamic_strict|4|9|3", "static|1|1|1", "static_strict|1|3|2"], "{mode:?}");
    }
}

#[test]
fn type_cascades_keep_copied_routines_views_and_materialized_views_in_their_database() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("type-cascade-database-scope-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        engine
            .select_database(crate::storage::DatabaseOid::TEMPLATE1)
            .unwrap();
        session.success(
            &mut engine,
            "CREATE TYPE cascade_choice AS ENUM('a'); \
             CREATE FUNCTION cascade_value() RETURNS cascade_choice LANGUAGE sql \
               BEGIN ATOMIC SELECT 'a'::cascade_choice; END; \
             CREATE VIEW cascade_view AS SELECT cascade_value() AS value; \
             CREATE MATERIALIZED VIEW cascade_matview AS SELECT cascade_value() AS value",
            true,
        );
        engine
            .select_database(crate::storage::DatabaseOid::POSTGRES)
            .unwrap();
        session.success(
            &mut engine,
            "CREATE DATABASE cascade_copy TEMPLATE template1",
            true,
        );
        let copied = engine
            .storage
            .database_named("cascade_copy", 0)
            .unwrap()
            .1
            .oid;
        engine
            .select_database(crate::storage::DatabaseOid::TEMPLATE1)
            .unwrap();
        session.success(&mut engine, "DROP TYPE cascade_choice CASCADE", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*) FROM pg_proc WHERE proname='cascade_value'; \
             SELECT count(*) FROM pg_class WHERE relname IN ('cascade_view','cascade_matview')",
                true
            )),
            ["0", "0"]
        );
        engine.select_database(copied).unwrap();
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT cascade_value()::text; SELECT value::text FROM cascade_view; \
             SELECT value::text FROM cascade_matview",
                true
            )),
            ["a", "a", "a"],
            "{mode:?}"
        );
    }
}

// PostgreSQL 18.6 SQL SHA256:
// a6d12a932def651411044e1d08dbd2bb3d67c958d9c9ada897433d434a310f1a.
#[test]
fn procedural_for_queries_execute_source_effects_once() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("pl-for-input-effects-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "SET application_name=''; CREATE FUNCTION for_input_effects() RETURNS text LANGUAGE plpgsql AS $$ \
             DECLARE scalar_value integer; record_value record; BEGIN \
              FOR scalar_value IN SELECT length(set_config('application_name',current_setting('application_name')||'s',false)) LOOP NULL; END LOOP; \
              FOR record_value IN SELECT set_config('application_name',current_setting('application_name')||'r',false) AS value LOOP NULL; END LOOP; \
              FOR scalar_value IN EXECUTE 'SELECT length(set_config(''application_name'',current_setting(''application_name'')||''d'',false))' LOOP NULL; END LOOP; \
              RETURN current_setting('application_name'); END$$", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT for_input_effects()", true)),
            ["srd"]
        );
    }
}

// PostgreSQL 18.6 event-trigger visibility reference SQL SHA256:
// 88cbb1c2ca5aad8e5b7883ef3cda840d008be0f3f790f60cdcf620435a6e8cfe.
#[test]
fn checked_trigger_inputs_preserve_visibility_or_reject_before_constraint_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("trigger-input-boundary-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE trigger_input_checks; CREATE TABLE trigger_input_rows(id integer); \
             CREATE TABLE trigger_input_audit(observed integer); CREATE TABLE event_input_audit(tag text); \
             INSERT INTO trigger_input_rows VALUES(1); \
             CREATE FUNCTION trigger_input_allowed(value integer) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               PERFORM nextval('trigger_input_checks'); \
               INSERT INTO trigger_input_audit SELECT count(*) FROM trigger_input_rows; RETURN true; END$$; \
             CREATE DOMAIN trigger_input_checked AS integer CHECK(trigger_input_allowed(VALUE)); \
             CREATE FUNCTION checked_row_input() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
               IF NEW.id=0 THEN RETURN NEW; END IF; \
               PERFORM cardinality('{1}'::trigger_input_checked[]); RETURN NEW; END$$; \
             CREATE TRIGGER checked_row_input AFTER INSERT ON trigger_input_rows \
               FOR EACH ROW EXECUTE FUNCTION checked_row_input(); \
             INSERT INTO trigger_input_rows VALUES(0)", true);
        let rejected = session.execute(
            &mut engine,
            "INSERT INTO trigger_input_rows VALUES(2),(3)",
            true,
        );
        let diagnostic = String::from_utf8_lossy(&rejected);
        assert!(diagnostic.contains("0A000"), "{diagnostic}");
        assert!(
            diagnostic.contains(
                "checked type inputs in data-change triggers require a resumable execution context"
            ),
            "{diagnostic}"
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT id FROM trigger_input_rows ORDER BY id; \
             SELECT count(*) FROM trigger_input_audit; SELECT last_value,is_called FROM trigger_input_checks", true)),
            ["0", "1", "0", "1|f"]);
        session.success(&mut engine,
            "DROP TRIGGER checked_row_input ON trigger_input_rows; \
             CREATE FUNCTION checked_event_input() RETURNS event_trigger LANGUAGE plpgsql AS $$BEGIN \
               PERFORM cardinality('{1}'::trigger_input_checked[]); \
               INSERT INTO event_input_audit VALUES(TG_TAG); END$$; \
             CREATE EVENT TRIGGER checked_event_input ON ddl_command_end WHEN TAG IN ('CREATE TABLE') \
               EXECUTE FUNCTION checked_event_input()", true);
        session.success(&mut engine,
            "BEGIN; INSERT INTO trigger_input_rows VALUES(2); \
             CREATE TABLE checked_event_one(id integer); CREATE TABLE checked_event_two(id integer); COMMIT", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT observed FROM trigger_input_audit; SELECT last_value FROM trigger_input_checks; \
             SELECT count(*) FROM event_input_audit; SELECT count(*) FROM trigger_input_rows", true)),
            ["3", "1", "2", "3"]);
        drop((session, engine));
    }
}

// PostgreSQL 18.6 phase oracle SQL SHA256:
// e9f16455ade1b384282cc4a6ecdb3d49d782edf3853e760ce66cf0e40c8e8345.
#[test]
fn procedural_expression_inputs_are_lazy_backend_plans() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("pl-input-plans-{mode:?}"));
        config.transaction_mode = mode;
        config.max_routines = 9;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE pl_checks; CREATE TABLE pl_audit(label text); \
             CREATE TYPE pl_choice AS ENUM('a'); \
             CREATE FUNCTION pl_allowed(value pl_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               PERFORM nextval('pl_checks'); INSERT INTO pl_audit VALUES(value::text); RETURN true; END$$; \
             CREATE DOMAIN pl_checked AS pl_choice CHECK(pl_allowed(VALUE)); \
             CREATE FUNCTION pl_take(value pl_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(value)'; \
             CREATE FUNCTION pl_assignment() RETURNS integer LANGUAGE plpgsql AS $$DECLARE v pl_checked[]; BEGIN v:='{a}'::pl_checked[]; RETURN cardinality(v); END$$; \
             CREATE FUNCTION pl_return() RETURNS pl_checked[] LANGUAGE plpgsql AS $$BEGIN RETURN '{a}'::pl_checked[]; END$$; \
             CREATE FUNCTION pl_call() RETURNS integer LANGUAGE plpgsql AS $$BEGIN RETURN pl_take('{a}'); END$$; \
             CREATE FUNCTION pl_unreached() RETURNS integer LANGUAGE plpgsql AS $$BEGIN IF false THEN RETURN pl_take('{a}'); END IF; RETURN 0; END$$; \
             CREATE FUNCTION pl_select() RETURNS integer LANGUAGE plpgsql AS $$DECLARE n integer; BEGIN SELECT cardinality('{a}'::pl_checked[]) INTO n; RETURN n; END$$; \
             CREATE FUNCTION pl_open() RETURNS integer LANGUAGE plpgsql AS $$DECLARE c refcursor; n integer; BEGIN OPEN c FOR SELECT cardinality('{a}'::pl_checked[]); FETCH c INTO n; CLOSE c; RETURN n; END$$; \
             CREATE FUNCTION pl_dynamic() RETURNS integer LANGUAGE plpgsql AS $$DECLARE n integer; BEGIN EXECUTE 'SELECT cardinality(''{a}''::pl_checked[])' INTO n; RETURN n; END$$", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT pl_unreached(),count(*) FROM pl_audit",
                true
            )),
            ["0|0"]
        );
        for (query, count) in [
            ("SELECT pl_assignment()", "1|1"),
            ("SELECT pl_return()::text", "2|2"),
            ("SELECT pl_call()", "3|3"),
            ("SELECT pl_select()", "4|4"),
            ("SELECT pl_open()", "5|5"),
            ("SELECT pl_dynamic()", "7|7"),
        ] {
            session.success(&mut engine, query, true);
            session.success(&mut engine, query, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT currval('pl_checks'),count(*) FROM pl_audit",
                    true
                )),
                [count]
            );
        }
        for count in ["8|8", "9|9"] {
            session.success(&mut engine, "DO $$DECLARE i integer; BEGIN FOR i IN 1..3 LOOP PERFORM cardinality('{a}'::pl_checked[]); END LOOP; END$$", true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT currval('pl_checks'),count(*) FROM pl_audit",
                    true
                )),
                [count]
            );
        }
    }
}

#[test]
fn template_inputs_keep_copied_type_and_member_lifetimes_after_source_drop_and_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cloned-input-owners-{mode:?}"));
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!(
            "cloned-input-owners-{}-{}",
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
        engine
            .select_database(crate::storage::DatabaseOid::TEMPLATE1)
            .unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE copied_input_checks; CREATE TYPE copied_choice AS ENUM('a','b'); \
             CREATE DOMAIN copied_checked AS copied_choice CHECK(nextval('copied_input_checks')>0); \
             CREATE DOMAIN copied_array AS copied_checked[] DEFAULT '{a}'; \
             CREATE TABLE copied_defaults(id integer,value copied_checked[] DEFAULT '{b}'); \
             CREATE TABLE copied_domain_defaults(id integer,value copied_array); \
             CREATE VIEW copied_constant AS SELECT '{a}'::copied_checked[] AS value; \
             CREATE VIEW copied_view_defaults AS SELECT id,value FROM copied_defaults; \
             ALTER VIEW copied_view_defaults ALTER COLUMN value SET DEFAULT '{a}'; \
             CREATE FUNCTION copied_atomic() RETURNS copied_checked[] LANGUAGE sql \
               BEGIN ATOMIC SELECT '{b}'::copied_checked[]; END", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM copied_input_checks",
                true
            )),
            ["5"]
        );
        engine
            .select_database(crate::storage::DatabaseOid::POSTGRES)
            .unwrap();
        session.success(
            &mut engine,
            "CREATE DATABASE input_copy TEMPLATE template1",
            true,
        );
        let copied = engine
            .storage
            .database_named("input_copy", 0)
            .unwrap()
            .1
            .oid;
        engine.select_database(copied).unwrap();
        session.success(
            &mut engine,
            "INSERT INTO copied_defaults(id) VALUES(1); \
             INSERT INTO copied_view_defaults(id) VALUES(2); \
             INSERT INTO copied_domain_defaults(id) VALUES(1)",
            true,
        );
        let verify = "SELECT value::text FROM copied_constant; SELECT copied_atomic()::text; \
                      SELECT value::text FROM copied_defaults ORDER BY id; \
                      SELECT value::text FROM copied_domain_defaults; SELECT last_value FROM copied_input_checks";
        assert_eq!(
            data_rows(&session.success(&mut engine, verify, true)),
            ["{a}", "{b}", "{b}", "{a}", "{a}", "5"]
        );
        engine
            .select_database(crate::storage::DatabaseOid::TEMPLATE1)
            .unwrap();
        session.success(&mut engine, "DROP TYPE copied_choice CASCADE", true);
        engine.select_database(copied).unwrap();
        assert_eq!(
            data_rows(&session.success(&mut engine, verify, true)),
            ["{a}", "{b}", "{b}", "{a}", "{a}", "5"]
        );
        assert!(engine.checkpoint().unwrap());
        engine.commit_wal().unwrap();
        drop((session, engine));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        recovered.select_database(copied).unwrap();
        let mut reader = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(
            data_rows(&reader.success(&mut recovered, verify, true)),
            ["{a}", "{b}", "{b}", "{a}", "{a}", "5"]
        );
        drop((reader, recovered));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        drop(server);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
// PostgreSQL 18.6 reference SQL SHA256:
// 143cf93eb9053325deab30b35e325ed0f56f4855a849e31f5cd22ab1e83d68a7.
fn stored_query_inputs_follow_enum_resolution_and_schema_edits_without_rechecking() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for checkpoint in [false, true] {
            let mut config = test_config(&format!("resolved-input-sources-{mode:?}-{checkpoint}"));
            config.transaction_mode = mode;
            config.object_store_on = true;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.object_store_bucket = format!(
                "resolved-input-sources-{}-{}-{checkpoint}",
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
                "CREATE SEQUENCE source_input_checks; CREATE SCHEMA input_sources; \
                 CREATE TYPE input_sources.choice AS ENUM('a','b'); \
                 CREATE DOMAIN input_sources.checked AS input_sources.choice \
                   CHECK(nextval('source_input_checks')>0); \
                 CREATE VIEW bound_source_view AS SELECT 'a'::input_sources.choice AS label, \
                   '{b}'::input_sources.checked[] AS value; \
                 CREATE TABLE bound_source_defaults(id integer,value input_sources.checked[] DEFAULT '{a}'); \
                 INSERT INTO bound_source_defaults(id) VALUES(1),(2)", true);
            assert_eq!(data_rows(&session.success(&mut engine,
                "SELECT label,value::text FROM bound_source_view; SELECT last_value FROM source_input_checks", true)),
                ["a|{b}", "2"]);
            session.success(
                &mut engine,
                "ALTER TYPE input_sources.choice RENAME VALUE 'a' TO 'longer_label'",
                true,
            );
            session.success(
                &mut engine,
                "BEGIN; ALTER SCHEMA input_sources RENAME TO transient_input_sources; ROLLBACK; \
                 ALTER SCHEMA input_sources RENAME TO retained_input_sources; \
                 INSERT INTO bound_source_defaults(id) VALUES(3)",
                true,
            );
            let verify = "SELECT label,value::text FROM bound_source_view; \
                          SELECT value::text FROM bound_source_defaults; \
                          SELECT last_value FROM source_input_checks";
            assert_eq!(
                data_rows(&session.success(&mut engine, verify, true)),
                [
                    "longer_label|{b}",
                    "{longer_label}",
                    "{longer_label}",
                    "{longer_label}",
                    "2"
                ]
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
                data_rows(&reader.success(&mut recovered, verify, true)),
                [
                    "longer_label|{b}",
                    "{longer_label}",
                    "{longer_label}",
                    "{longer_label}",
                    "2"
                ]
            );
            drop((reader, recovered));
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            drop(server);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }
}

#[test]
fn domain_and_view_defaults_retain_checked_inputs_across_edits_and_rollback() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("default-input-owners-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE default_checks; CREATE TABLE default_audit(value text); \
             CREATE TYPE default_choice AS ENUM('a','b'); \
             CREATE FUNCTION check_default(value default_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               PERFORM nextval('default_checks'); INSERT INTO default_audit VALUES(value::text); RETURN true; END$$; \
             CREATE DOMAIN default_checked AS default_choice CHECK(check_default(VALUE)); \
             CREATE DOMAIN default_array AS default_checked[] DEFAULT '{a}'; \
             CREATE TABLE domain_defaults(id integer,value default_array); \
             CREATE TABLE view_default_values(id integer,value default_checked[]); \
             CREATE VIEW owner_defaults AS SELECT id,value FROM view_default_values; \
             ALTER VIEW owner_defaults ALTER COLUMN value SET DEFAULT '{a}'", true);
        let counter = "SELECT currval('default_checks'),count(*) FROM default_audit";
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["2|2"]
        );
        let default = engine.storage.find_domain("default_array", 0).unwrap();
        let scratch = Arena::new(&mut budget, "domain default SQL identity", 64 << 10).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let source = crate::sql::enum_values::checks::resolve_default(
                default.default_expr.as_ref().unwrap().as_str(),
                default.base,
                &engine.storage,
                0,
                &scratch,
            )
            .unwrap();
            let expression = parser::parse_expression(source, &scratch).unwrap();
            assert_eq!(
                exec::infer_type_catalog(expression, None, &engine.storage, 0, &scratch)
                    .unwrap()
                    .0,
                default.base.oid(),
                "generated SQL must retain the checked domain-array type"
            );
        });
        session.success(
            &mut engine,
            "INSERT INTO domain_defaults(id) VALUES(1),(2); \
             INSERT INTO owner_defaults(id) VALUES(1),(2); \
             CREATE OR REPLACE VIEW owner_defaults AS SELECT id,value FROM view_default_values; \
             INSERT INTO owner_defaults(id) VALUES(3)",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["2|2"]
        );
        session.success(&mut engine,
            "BEGIN; SAVEPOINT previous_defaults; \
             ALTER DOMAIN default_array SET DEFAULT '{b}'; \
             ALTER VIEW owner_defaults ALTER COLUMN value SET DEFAULT '{b}'; \
             INSERT INTO domain_defaults(id) VALUES(3); INSERT INTO owner_defaults(id) VALUES(4); \
             ROLLBACK TO previous_defaults; \
             INSERT INTO domain_defaults(id) VALUES(4); INSERT INTO owner_defaults(id) VALUES(5); COMMIT", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["4|2"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT id,value::text FROM domain_defaults ORDER BY id; \
             SELECT id,value::text FROM owner_defaults ORDER BY id",
                true
            )),
            [
                "1|{a}", "2|{a}", "4|{a}", "1|{a}", "2|{a}", "3|{a}", "5|{a}"
            ]
        );
    }
}

// PostgreSQL 18.6 PREPARE reference SQL SHA256:
// daf5154b24ffeb86c7deae60b70570fa4bef142d2865b12b9c93d650e32c77a5.
#[test]
fn implicit_domain_array_inputs_are_checked_at_prepare_and_reused() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("implicit-domain-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE implicit_checks; CREATE TABLE implicit_audit(value text); \
             CREATE TYPE implicit_choice AS ENUM('a','b'); \
             CREATE FUNCTION implicit_allowed(value implicit_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               PERFORM nextval('implicit_checks'); INSERT INTO implicit_audit VALUES(value::text); RETURN value='a'; END$$; \
             CREATE DOMAIN implicit_checked AS implicit_choice CHECK(implicit_allowed(VALUE)); \
             CREATE TABLE implicit_values(id integer PRIMARY KEY,value implicit_checked[]); \
             CREATE FUNCTION take_checked(value implicit_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(value)'", true);
        let counter = "SELECT currval('implicit_checks'),count(*) FROM implicit_audit";
        for (prepare, execute, expected) in [
            (
                "PREPARE implicit_insert AS INSERT INTO implicit_values VALUES(1,'{a}')",
                "EXECUTE implicit_insert",
                "1|1",
            ),
            (
                "PREPARE implicit_call AS SELECT take_checked('{a}')",
                "EXECUTE implicit_call; EXECUTE implicit_call",
                "2|2",
            ),
            (
                "PREPARE implicit_update AS UPDATE implicit_values SET value='{a}' WHERE id=1",
                "EXECUTE implicit_update",
                "3|3",
            ),
            (
                "PREPARE explicit_insert AS INSERT INTO implicit_values VALUES(2,'{a}'::implicit_checked[])",
                "EXECUTE explicit_insert",
                "4|4",
            ),
        ] {
            session.success(&mut engine, prepare, true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
            session.success(&mut engine, execute, true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(
            &mut engine,
            "INSERT INTO implicit_values SELECT 3,'{a}'",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["5|5"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT 7 WHERE take_checked('{a}')=1", true)),
            ["7"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["6|6"]
        );
        for (statement, expected) in [
            (
                "PREPARE bad_implicit_insert AS INSERT INTO implicit_values VALUES(4,'{b}')",
                "7|6",
            ),
            (
                "PREPARE bad_implicit_call AS SELECT take_checked('{b}')",
                "8|6",
            ),
            (
                "PREPARE bad_implicit_update AS UPDATE implicit_values SET value='{b}'",
                "9|6",
            ),
        ] {
            let error = session.execute(&mut engine, statement, true);
            assert!(
                error.windows(5).any(|bytes| bytes == b"23514"),
                "{}",
                String::from_utf8_lossy(&error)
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(&mut engine,
            "BEGIN; PREPARE retained_implicit_input AS SELECT take_checked('{a}'); ROLLBACK; EXECUTE retained_implicit_input", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["10|6"]
        );
    }
}

// PostgreSQL 18.6 input-order SQL SHA256:
// de9332b4b78c97aa4ed4e18c5d78717ca526d7183df3eb532afbcc7dbfeee16a,
// 78f032b6ce047aa906c1e33276c0af00ea519cd360ad975d7fd6fdae7d8bc512.
#[test]
fn domain_input_discovery_preserves_expression_and_row_coercion_order() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("domain-input-order-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE ordered_inputs; CREATE TABLE input_order(position bigint,label text); \
             CREATE TYPE ordered_choice AS ENUM('a','b'); \
             CREATE FUNCTION record_ordered_input(value ordered_choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO input_order VALUES(nextval('ordered_inputs'),value::text); RETURN true; END$$; \
             CREATE DOMAIN ordered_checked AS ordered_choice CHECK(record_ordered_input(VALUE)); \
             CREATE FUNCTION take_ordered(value ordered_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(value)'; \
             CREATE FUNCTION take_two_ordered(first_value ordered_checked[],second_value ordered_checked[]) RETURNS integer LANGUAGE sql AS 'SELECT cardinality(first_value)+cardinality(second_value)'; \
             CREATE TABLE ordered_values(first_value ordered_checked[],second_value ordered_checked[])", true);
        for (query, expected) in [
            ("SELECT take_ordered('{a}'),'{b}'::ordered_checked[]", "a,b"),
            (
                "SELECT take_two_ordered(second_value=>'{b}',first_value=>'{a}')",
                "b,a",
            ),
            (
                "SELECT take_two_ordered('{a}','{b}'::ordered_checked[])",
                "b,a",
            ),
            (
                "INSERT INTO ordered_values VALUES('{a}','{b}'::ordered_checked[])",
                "b,a",
            ),
            (
                "UPDATE ordered_values SET first_value='{a}',second_value='{b}'::ordered_checked[]",
                "b,a",
            ),
            (
                "INSERT INTO ordered_values SELECT '{a}','{b}'::ordered_checked[]",
                "b,a",
            ),
            (
                "TRUNCATE ordered_values; INSERT INTO ordered_values VALUES('{a}','{b}'::ordered_checked[]),('{b}','{a}'::ordered_checked[])",
                "b,a,a,b",
            ),
        ] {
            session.success(&mut engine, query, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT string_agg(label,',' ORDER BY position) FROM input_order",
                    true
                )),
                [expected],
                "{query}"
            );
            session.success(&mut engine, "TRUNCATE input_order", true);
        }
    }
}

#[test]
fn routine_input_binding_uses_its_owner_and_settings_then_restores_the_caller() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("routine-input-context-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let owner = data_rows(&session.success(&mut engine, "SELECT current_user", true)).remove(0);
        session.success(&mut engine,
            "CREATE ROLE input_context_caller; CREATE SCHEMA input_context; \
             CREATE TABLE input_context.audit(role_name text,schema_name text,application text); \
             CREATE TYPE input_context.choice AS ENUM('a'); \
             CREATE FUNCTION input_context.record(value input_context.choice) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO input_context.audit VALUES(current_user,current_schema(),current_setting('application_name')); RETURN true; END$$; \
             CREATE DOMAIN input_context.checked AS input_context.choice CHECK(input_context.record(VALUE)); \
             CREATE FUNCTION public.input_scope_scalar() RETURNS integer LANGUAGE sql SECURITY DEFINER \
               SET search_path TO input_context,pg_catalog SET application_name TO 'routine-input' \
               AS 'SELECT cardinality(''{a}''::checked[])'; \
             CREATE FUNCTION public.input_scope_rows() RETURNS SETOF integer LANGUAGE sql SECURITY DEFINER \
               SET search_path TO input_context,pg_catalog SET application_name TO 'routine-input' \
               AS 'SELECT cardinality(''{a}''::checked[])'; \
             TRUNCATE input_context.audit; REVOKE ALL ON SCHEMA input_context FROM PUBLIC; \
             SET search_path TO public; SET application_name TO 'caller-input'; SET ROLE input_context_caller", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT public.input_scope_scalar(),current_user,current_schema(),current_setting('application_name')", true)),
            ["1|input_context_caller|public|caller-input"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT value,current_user,current_schema(),current_setting('application_name') FROM public.input_scope_rows() AS value", true)),
            ["1|input_context_caller|public|caller-input"]);
        session.success(&mut engine, "RESET ROLE", true);
        let audit = data_rows(&session.success(
            &mut engine,
            "SELECT role_name,schema_name,application FROM input_context.audit",
            true,
        ));
        let expected = format!("{owner}|input_context|routine-input");
        assert_eq!(audit, [expected.clone(), expected]);
    }
}

// PostgreSQL 18.6 reference SQL SHA256:
// 5c64f93bcac58bdf82796ebe724256807e79bd0934d30b6137c33d7299652e7d.
#[test]
fn sql_string_validation_owns_inputs_separately_from_each_invocation_plan() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("create-input-validation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE validation_checks; CREATE TYPE validation_choice AS ENUM('a'); \
             CREATE DOMAIN validation_checked AS validation_choice CHECK(nextval('validation_checks')>0); \
             CREATE FUNCTION checked_body() RETURNS validation_checked[] LANGUAGE sql \
               AS 'SELECT ''{a}''::validation_checked[]'", true);
        let counter = "SELECT last_value FROM validation_checks";
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["1"]
        );
        for (sql, state) in [
            (
                "CREATE FUNCTION checked_body() RETURNS validation_checked[] LANGUAGE sql AS 'SELECT ''{a}''::validation_checked[]'",
                "42723",
            ),
            (
                "CREATE OR REPLACE FUNCTION checked_body() RETURNS text LANGUAGE sql AS 'SELECT ''{a}''::validation_checked[]'",
                "42P13",
            ),
        ] {
            let error = session.execute(&mut engine, sql, true);
            assert!(
                error.windows(5).any(|bytes| bytes == state.as_bytes()),
                "{}",
                String::from_utf8_lossy(&error)
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                ["1"]
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT checked_body() FROM generate_series(1,3)",
                true
            )),
            ["{a}", "{a}", "{a}"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["2"]
        );
        session.success(&mut engine, "SELECT checked_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["3"]
        );
        session.success(
            &mut engine,
            "PREPARE cached_validation AS SELECT checked_body()",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["3"]
        );
        session.success(
            &mut engine,
            "EXECUTE cached_validation; EXECUTE cached_validation",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["4"]
        );
        session.success(
            &mut engine,
            "SET check_function_bodies=off; \
             CREATE FUNCTION unchecked_body() RETURNS validation_checked[] LANGUAGE sql \
               AS 'SELECT ''{a}''::validation_checked[]'; \
             CREATE FUNCTION invalid_body() RETURNS int LANGUAGE sql AS 'SELECT unknown_column'; \
             CREATE FUNCTION invalid_pl() RETURNS int LANGUAGE plpgsql AS 'malformed body'; \
             SET check_function_bodies=on",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["4"]
        );
        session.success(&mut engine, "SELECT unchecked_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["5"]
        );
        session.success(&mut engine,
            "CREATE SCHEMA private_validation; CREATE TYPE private_validation.choice AS ENUM('a'); \
             CREATE DOMAIN private_validation.checked AS private_validation.choice CHECK(nextval('validation_checks')>0); \
             CREATE FUNCTION configured_body() RETURNS private_validation.checked[] LANGUAGE sql \
               SET search_path=private_validation,public AS 'SELECT ''{a}''::checked[]'", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["6"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SHOW search_path", true)),
            ["\"$user\", public"]
        );
    }
}
