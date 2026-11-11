use super::*;

const LOWER: &str = "SELECT provolatile, proisstrict, procost, prorows, proparallel, \
     proleakproof, prosecdef, proconfig, proowner::regrole \
     FROM pg_proc WHERE oid = 'lower(text)'::regprocedure";
const MD5: &str = "SELECT proacl FROM pg_proc WHERE oid = 'md5(text)'::regprocedure";

fn assert_changed(engine: &mut Engine, budget: &mut Budget) {
    assert_eq!(
        data_rows(&run_with(engine, budget, LOWER)),
        ["s|t|5|0|r|t|f|NULL|changer"]
    );
    assert_eq!(
        data_rows(&run_with(engine, budget, MD5)),
        ["{postgres=X/postgres,grantee=X*/postgres}"]
    );
    assert_eq!(
        data_rows(&run_with(
            engine,
            budget,
            "SELECT has_function_privilege('changer', 'md5(text)', 'execute'), \
             has_function_privilege('grantee', 'md5(text)', 'execute WITH GRANT OPTION')"
        )),
        ["f|t"]
    );
    assert_eq!(
        data_rows(&run_with(
            engine,
            budget,
            "SELECT prorows FROM pg_proc \
             WHERE oid = 'generate_series(integer, integer)'::regprocedure"
        )),
        ["10"]
    );
}

#[test]
fn builtin_routine_changes_are_per_database_enforced_and_recovered() {
    for checkpoint in [false, true] {
        let name = format!("builtin-routine-changes-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        run_with(&mut engine, &mut budget, "CREATE DATABASE other");
        let output = run_with(
            &mut engine,
            &mut budget,
            "CREATE ROLE changer; CREATE ROLE grantee; \
             ALTER FUNCTION lower(text) STABLE STRICT COST 5 PARALLEL RESTRICTED LEAKPROOF \
               SECURITY INVOKER; \
             ALTER FUNCTION generate_series(integer, integer) ROWS 10; \
             ALTER FUNCTION lower(text) OWNER TO changer; \
             REVOKE EXECUTE ON FUNCTION md5(text) FROM PUBLIC; \
             GRANT EXECUTE ON FUNCTION md5(text) TO grantee WITH GRANT OPTION",
        );
        assert!(
            error_sqlstates(&output).is_empty(),
            "{:?}",
            String::from_utf8_lossy(&output)
        );
        for (sql, state) in [
            ("ALTER FUNCTION lower(text) ROWS 10", "22023"),
            (
                "ALTER FUNCTION lower(text) SET search_path = public",
                "0A000",
            ),
            ("ALTER FUNCTION lower(text) RESET ALL", "0A000"),
            (
                "CREATE TABLE generated_lower (a text, b text GENERATED ALWAYS AS (lower(a)) STORED)",
                "42P17",
            ),
            ("ALTER FUNCTION lower(text) RENAME TO lower2", "0A000"),
            ("ALTER FUNCTION upper(text) SET SCHEMA public", "0A000"),
            ("SET ROLE changer; SELECT md5('a')", "42501"),
            ("SET ROLE grantee; ALTER FUNCTION md5(text) STABLE", "42501"),
            ("DROP ROLE grantee", "2BP01"),
            (
                "BEGIN; ALTER FUNCTION lower(text) VOLATILE; SELECT 1/0",
                "22012",
            ),
        ] {
            let output = run_with(&mut engine, &mut budget, sql);
            assert_eq!(error_sqlstates(&output), [state], "{sql}");
            run_with(&mut engine, &mut budget, "ROLLBACK; RESET ROLE");
        }
        // The owner may change the attributes; the grantee may call it.
        let output = run_with(
            &mut engine,
            &mut budget,
            "SET ROLE changer; ALTER FUNCTION lower(text) STABLE; RESET ROLE; \
             SET ROLE grantee; SELECT md5('a'); RESET ROLE",
        );
        assert!(
            error_sqlstates(&output).is_empty(),
            "{:?}",
            String::from_utf8_lossy(&output)
        );
        assert_changed(&mut engine, &mut budget);
        if checkpoint {
            assert!(engine.checkpoint().unwrap());
        }
        drop(engine);
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        assert_changed(&mut engine, &mut budget);
        let other = data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT oid FROM pg_database WHERE datname = 'other'",
        ));
        engine
            .select_database(crate::storage::DatabaseOid::parse(other[0].parse().unwrap()).unwrap())
            .unwrap();
        assert_eq!(
            data_rows(&run_with(&mut engine, &mut budget, LOWER)),
            ["i|t|1|0|s|f|f|NULL|postgres"]
        );
        assert_eq!(
            data_rows(&run_with(&mut engine, &mut budget, MD5)),
            ["NULL"]
        );
    }
}

/// PostgreSQL stores a built-in's changed `STRICT` and `SECURITY` but runs
/// the compiled function as before; only its planner folds a constant NULL
/// input of a function `pg_proc` marks strict.
#[test]
fn builtin_strictness_and_security_change_only_the_catalog_and_folding() {
    let (mut engine, mut budget) = test_engine();
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE nulls(a integer); INSERT INTO nulls VALUES (NULL); \
         ALTER FUNCTION array_append(anycompatiblearray, anycompatible) STRICT; \
         ALTER FUNCTION lower(text) CALLED ON NULL INPUT SECURITY DEFINER",
    );
    assert!(
        error_sqlstates(&output).is_empty(),
        "{}",
        String::from_utf8_lossy(&output)
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT array_append(ARRAY[1], NULL) IS NULL, array_append(ARRAY[1], a) \
             FROM nulls"
        )),
        ["t|{1,NULL}"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT proisstrict, prosecdef, lower(NULL) IS NULL FROM pg_proc \
             WHERE oid = 'lower(text)'::regprocedure"
        )),
        ["f|t|t"]
    );
}
