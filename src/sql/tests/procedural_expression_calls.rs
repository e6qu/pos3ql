use super::*;

// PostgreSQL 18.6 reference SQL SHA256:
// b9ff9c6b7cefb6d32041e6a1bc91cfaa90121cbd06f0d5c262c8aa28cdc5a225.
#[test]
fn procedural_expressions_resume_nested_calls_without_repeating_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-expression-calls-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "SET application_name TO 'outer-call'; CREATE SEQUENCE procedural_calls; \
             CREATE TABLE procedural_audit(value integer); \
             CREATE FUNCTION procedural_value(input_value integer) RETURNS integer LANGUAGE plpgsql \
               SET application_name TO 'nested-call' AS $$BEGIN \
                 IF current_setting('application_name')<>'nested-call' THEN RAISE EXCEPTION 'configuration scope lost'; END IF; \
                 INSERT INTO procedural_audit VALUES(input_value); PERFORM nextval('procedural_calls'); \
                 RETURN input_value; END$$; \
             CREATE FUNCTION procedural_composed() RETURNS integer LANGUAGE plpgsql AS $$ \
               DECLARE initial integer:=procedural_value(1); result integer; \
               BEGIN result:=procedural_value(2); \
                 IF procedural_value(3)=3 THEN result:=result+procedural_value(4); END IF; \
                 RETURN result+procedural_value(5); END$$; \
             CREATE FUNCTION procedural_caught() RETURNS integer LANGUAGE plpgsql AS $$ \
               DECLARE zero integer:=0; result integer; \
               BEGIN BEGIN result:=procedural_value(6)/zero; \
                 EXCEPTION WHEN division_by_zero THEN result:=procedural_value(7); END; \
                 RETURN result; END$$; \
             CREATE FUNCTION procedural_command() RETURNS text LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO procedural_audit VALUES(9); PERFORM nextval('procedural_calls'); \
               RETURN 'SELECT $1'; END$$; \
             CREATE FUNCTION procedural_dynamic() RETURNS integer LANGUAGE plpgsql AS $$ \
               DECLARE result integer; BEGIN \
                 EXECUTE procedural_command() INTO result USING procedural_value(8); \
                 RETURN result; END$$", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT procedural_composed(),current_setting('application_name'); \
             SELECT procedural_caught(),current_setting('application_name'); \
             SELECT procedural_dynamic(),current_setting('application_name'); \
             SELECT count(*),sum(value) FROM procedural_audit; SELECT currval('procedural_calls')",
                true
            )),
            ["11|outer-call", "7|outer-call", "8|outer-call", "8|39", "9"]
        );
    }
}

// PostgreSQL 18.6 accepts PL/pgSQL data changes, static and dynamic, whose
// expressions call a PL/pgSQL function, and runs each call once.
// Reference SQL SHA256:
// 6a10b164dc20a9bbca33c195a1a3067cb471099d6829954daf937b5130a78b80.
#[test]
fn procedural_writes_call_procedural_functions() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("procedural-write-calls-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE t(a int, b text); CREATE TABLE audit(v int); \
             CREATE FUNCTION plfn(x int) RETURNS int LANGUAGE plpgsql AS \
             $$BEGIN INSERT INTO audit VALUES (x); RETURN x * 10; END$$",
            true,
        );
        let output = session.execute(
            &mut engine,
            "DO $$BEGIN INSERT INTO t VALUES (plfn(1), 'a'); END$$; \
             DO $$BEGIN INSERT INTO t SELECT plfn(g), 'g' FROM generate_series(2,3) g; END$$; \
             DO $$BEGIN UPDATE t SET a = plfn(a) WHERE a = 10; END$$; \
             DO $$BEGIN DELETE FROM t WHERE a = plfn(2) / 10 * 10; END$$; \
             DO $$BEGIN INSERT INTO t VALUES (plfn(4), 'r'); END$$; \
             DO $$BEGIN EXECUTE 'INSERT INTO t VALUES (plfn($1), ''e'')' USING 5; END$$; \
             SELECT a, b FROM t ORDER BY a; SELECT v FROM audit ORDER BY v",
            true,
        );
        assert!(
            error_sqlstates(&output).is_empty(),
            "{mode:?}: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(
            data_rows(&output),
            [
                "30|g", "40|r", "50|e", "100|a", "1", "2", "2", "2", "2", "3", "4", "5", "10"
            ],
            "{mode:?}"
        );
    }
}

// Static PL/pgSQL data changes with RETURNING assign the returned row to
// their INTO targets; without INTO the returned rows have no destination, as
// in PostgreSQL 18.
#[test]
fn procedural_writes_return_into_targets() {
    let config = test_config("procedural-write-returning");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE t(a int); CREATE TABLE seen(v text)",
        true,
    );
    let output = session.execute(
        &mut engine,
        "DO $$BEGIN INSERT INTO t VALUES (1) RETURNING a; END$$",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["42601"]);
    assert!(String::from_utf8_lossy(&output).contains("query has no destination for result data"));
    session.success(
        &mut engine,
        "DO $$DECLARE r int; k text; BEGIN \
         INSERT INTO t VALUES (1) RETURNING a INTO r; INSERT INTO seen VALUES (r); \
         UPDATE t SET a = 2 RETURNING a INTO r; INSERT INTO seen VALUES (r); \
         MERGE INTO t USING (VALUES (2)) s(a) ON t.a = s.a \
           WHEN MATCHED THEN UPDATE SET a = 3 RETURNING merge_action(), t.a INTO k, r; \
         INSERT INTO seen VALUES (k || r); \
         DELETE FROM t RETURNING a INTO STRICT r; INSERT INTO seen VALUES (r); END$$",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT v FROM seen; SELECT count(*) FROM t",
            true
        )),
        ["1", "2", "UPDATE3", "3", "0"]
    );
}
