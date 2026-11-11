use super::*;

#[test]
fn builtin_routine_comments_are_owned_by_the_bootstrap_role_and_recover() {
    let config = test_config("builtin-routine-comments");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let describe = "SELECT obj_description('lower(text)'::regprocedure, 'pg_proc'), \
                    (SELECT count(*) FROM pg_description WHERE objoid = 'lower(text)'::regprocedure)";
    let output = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE commenter; SET ROLE commenter; COMMENT ON FUNCTION lower(text) IS 'x'",
    );
    assert_eq!(error_sqlstates(&output), ["42501"]);
    let output = run_with(
        &mut engine,
        &mut budget,
        "COMMENT ON FUNCTION lower(text) IS 'my lower'",
    );
    assert!(error_sqlstates(&output).is_empty());
    let output = run_with(
        &mut engine,
        &mut budget,
        "COMMENT ON FUNCTION lower(integer) IS 'missing'",
    );
    assert_eq!(error_sqlstates(&output), ["42883"]);
    drop(engine);
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, describe)),
        ["my lower|1"]
    );
    run_with(
        &mut engine,
        &mut budget,
        "COMMENT ON FUNCTION lower(text) IS NULL",
    );
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, describe)),
        ["NULL|0"]
    );
}
