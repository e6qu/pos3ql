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
