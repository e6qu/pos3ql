use super::*;

#[test]
fn selected_scalar_overloads_keep_rows_grouping_types_and_locks() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("selected-aggregate-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE FUNCTION public.sum(value integer) RETURNS integer LANGUAGE sql AS 'SELECT $1+100'; \
             CREATE TABLE aggregate_choice(v integer); INSERT INTO aggregate_choice VALUES(1),(2); \
             CREATE SEQUENCE aggregate_effects; SET search_path=public,pg_catalog", true);
        for (sql, expected) in [
            (
                "SELECT sum(v) FROM aggregate_choice ORDER BY v",
                vec!["101", "102"],
            ),
            (
                "SELECT pg_typeof(sum(v)) FROM aggregate_choice ORDER BY v",
                vec!["integer", "integer"],
            ),
            (
                "SELECT sum(v),count(*) FROM aggregate_choice GROUP BY v ORDER BY v",
                vec!["101|1", "102|1"],
            ),
            ("SELECT sum(1)", vec!["101"]),
            (
                "SELECT sum(v) FROM aggregate_choice ORDER BY v FOR UPDATE",
                vec!["101", "102"],
            ),
            (
                "SELECT sum(v),row_number() OVER(ORDER BY v) FROM aggregate_choice ORDER BY v",
                vec!["101|1", "102|2"],
            ),
            (
                "SELECT sum(1) FROM aggregate_choice HAVING true",
                vec!["101"],
            ),
            ("SELECT sum(1) FROM aggregate_choice HAVING false", vec![]),
            (
                "SELECT sum(1) FROM aggregate_choice WHERE false HAVING true",
                vec!["101"],
            ),
            (
                "SELECT result.value FROM (SELECT sum(1) AS value FROM aggregate_choice HAVING true) result",
                vec!["101"],
            ),
            (
                "SELECT sum(nextval('aggregate_effects')::integer) FROM aggregate_choice HAVING false",
                vec![],
            ),
        ] {
            assert_eq!(
                data_rows(&session.success(&mut engine, sql, true)),
                expected,
                "{sql}"
            );
        }
        for (sql, state) in [
            ("SELECT sum(v),count(*) FROM aggregate_choice", "42803"),
            ("SELECT sum(v) FROM aggregate_choice HAVING true", "42803"),
            (
                "SELECT sum(DISTINCT nextval('aggregate_effects')::integer)",
                "42809",
            ),
            (
                "SELECT sum(nextval('aggregate_effects')::integer) FILTER(WHERE true)",
                "42809",
            ),
            (
                "SELECT sum(nextval('aggregate_effects')::integer) OVER()",
                "42809",
            ),
            ("SELECT row_number()", "42809"),
        ] {
            let failed = session.execute(&mut engine, sql, true);
            assert!(
                String::from_utf8_lossy(&failed).contains(state),
                "{sql}: {}",
                String::from_utf8_lossy(&failed)
            );
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT nextval('aggregate_effects')", true)),
            ["1"]
        );
        session.success(&mut engine, "SET search_path=pg_catalog,public", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT sum(v),pg_typeof(sum(v)) FROM aggregate_choice",
                true
            )),
            ["3|bigint"]
        );
    }
}
