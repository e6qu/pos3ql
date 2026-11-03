use super::*;

#[test]
fn selected_relation_inputs_follow_truncates_and_structural_index_changes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("relation-input-granularity-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(
            &mut engine,
            r#"CREATE SEQUENCE granularity_checks;
CREATE TYPE granularity_choice AS ENUM('a');
CREATE DOMAIN granularity_checked AS granularity_choice CHECK(nextval('granularity_checks')>0);
CREATE TABLE granularity_anchor(id integer);
INSERT INTO granularity_anchor VALUES(1);
CREATE FUNCTION granularity_body() RETURNS granularity_checked[] LANGUAGE sql
SET search_path=public AS $$SELECT '{a}'::granularity_checked[] FROM granularity_anchor LIMIT 1$$;
ALTER SEQUENCE granularity_checks RESTART WITH 1;"#,
            true,
        );
        first.success(&mut engine, "SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["1"],
            "step 0: SELECT granularity_body()"
        );
        second.success(&mut engine, "SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&second.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["2"],
            "step 1: SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "ANALYZE granularity_anchor; SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["2"],
            "step 2: ANALYZE granularity_anchor; SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "CREATE INDEX granularity_index ON granularity_anchor(id); SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["3"],
            "step 3: CREATE INDEX granularity_index ON granularity_anchor(id); SELECT granularity_body()"
        );
        second.success(&mut engine, "SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&second.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["4"],
            "step 4: SELECT granularity_body()"
        );
        first.success(&mut engine, "ALTER INDEX granularity_index RENAME TO granularity_renamed; SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["4"],
            "step 5: ALTER INDEX granularity_index RENAME TO granularity_renamed; SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "ALTER INDEX granularity_renamed SET(fillfactor=80); SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["4"],
            "step 6: ALTER INDEX granularity_renamed SET(fillfactor=80); SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "BEGIN; TRUNCATE granularity_anchor; SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["5"],
            "step 7: BEGIN; TRUNCATE granularity_anchor; SELECT granularity_body()"
        );
        first.success(&mut engine, "ROLLBACK; SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["6"],
            "step 8: ROLLBACK; SELECT granularity_body()"
        );
        second.success(&mut engine, "SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&second.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["6"],
            "step 9: SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "BEGIN; TRUNCATE granularity_anchor; COMMIT; SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["7"],
            "step 10: BEGIN; TRUNCATE granularity_anchor; COMMIT; SELECT granularity_body()"
        );
        second.success(&mut engine, "SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&second.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["8"],
            "step 11: SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "INSERT INTO granularity_anchor VALUES(2); SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["8"],
            "step 12: INSERT INTO granularity_anchor VALUES(2); SELECT granularity_body()"
        );
        first.success(
            &mut engine,
            "DROP INDEX granularity_renamed; SELECT granularity_body()",
            true,
        );
        assert_eq!(
            data_rows(&first.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["9"],
            "step 13: DROP INDEX granularity_renamed; SELECT granularity_body()"
        );
        second.success(&mut engine, "SELECT granularity_body()", true);
        assert_eq!(
            data_rows(&second.success(
                &mut engine,
                "SELECT last_value FROM granularity_checks",
                true
            )),
            ["10"],
            "step 14: SELECT granularity_body()"
        );
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
