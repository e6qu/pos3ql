use super::*;

#[test]
fn selected_relation_inputs_follow_truncates_and_structural_index_changes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("relation-input-granularity-{mode:?}"));
        config.transaction_mode = mode;
        config.max_prepared_transactions = 1;
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
        // Session two reads only after TRUNCATE releases its transaction locks.
        for (use_second, statement, expected) in [
            (false, "SELECT granularity_body()", "1"),
            (true, "SELECT granularity_body()", "2"),
            (
                false,
                "ANALYZE granularity_anchor; SELECT granularity_body()",
                "3",
            ),
            (
                false,
                "CREATE INDEX granularity_index ON granularity_anchor(id); SELECT granularity_body()",
                "4",
            ),
            (true, "SELECT granularity_body()", "5"),
            (
                false,
                "ALTER INDEX granularity_index RENAME TO granularity_renamed; SELECT granularity_body()",
                "5",
            ),
            (
                false,
                "ALTER INDEX granularity_renamed SET(fillfactor=80); SELECT granularity_body()",
                "5",
            ),
            (
                false,
                "BEGIN; TRUNCATE granularity_anchor; SELECT granularity_body()",
                "6",
            ),
            (false, "ROLLBACK; SELECT granularity_body()", "7"),
            (true, "SELECT granularity_body()", "7"),
            (
                false,
                "BEGIN; TRUNCATE granularity_anchor; COMMIT; SELECT granularity_body()",
                "8",
            ),
            (true, "SELECT granularity_body()", "9"),
            (
                false,
                "INSERT INTO granularity_anchor VALUES(2); SELECT granularity_body()",
                "9",
            ),
            (
                false,
                "DROP INDEX granularity_renamed; SELECT granularity_body()",
                "10",
            ),
            (true, "SELECT granularity_body()", "11"),
            (
                false,
                "BEGIN; TRUNCATE granularity_anchor; SELECT granularity_body(); PREPARE TRANSACTION 'truncate-commit'",
                "12",
            ),
            (
                true,
                "COMMIT PREPARED 'truncate-commit'; SELECT granularity_body()",
                "13",
            ),
            (false, "SELECT granularity_body()", "14"),
            (
                false,
                "BEGIN; TRUNCATE granularity_anchor; SELECT granularity_body(); PREPARE TRANSACTION 'truncate-rollback'",
                "15",
            ),
            (
                true,
                "ROLLBACK PREPARED 'truncate-rollback'; SELECT granularity_body()",
                "15",
            ),
            (false, "SELECT granularity_body()", "16"),
            (true, "SELECT granularity_body()", "16"),
        ] {
            let session = if use_second { &mut second } else { &mut first };
            // Match psql's separate messages while BEGIN keeps explicit transactions open.
            for command in statement.split(';') {
                session.success(&mut engine, command.trim(), true);
            }
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT last_value FROM granularity_checks",
                    true
                )),
                [expected],
                "{statement}"
            );
        }
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn changed_relation_estimates_invalidate_other_sessions_before_commit() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cross-analyze-plan-invalidation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut first = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut second =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        first.success(&mut engine, r#"CREATE SEQUENCE cross_analyze_checks; CREATE TYPE cross_analyze_choice AS ENUM('a'); CREATE DOMAIN cross_analyze_checked AS cross_analyze_choice CHECK(nextval('cross_analyze_checks')>0); CREATE TABLE cross_analyze_anchor(id integer); CREATE TABLE cross_analyze_unrelated(id integer); INSERT INTO cross_analyze_anchor VALUES(1); CREATE FUNCTION cross_analyze_body() RETURNS cross_analyze_checked[] LANGUAGE sql SET search_path=public AS $$SELECT '{a}'::cross_analyze_checked[] FROM cross_analyze_anchor LIMIT 1$$; ALTER SEQUENCE cross_analyze_checks RESTART WITH 1;"#, true);
        for (use_second, statement, expected) in [
            (false, "SELECT cross_analyze_body()", "1"),
            (true, "SELECT cross_analyze_body()", "2"),
            (
                false,
                "BEGIN; INSERT INTO cross_analyze_anchor VALUES(2); ANALYZE cross_analyze_anchor; SELECT cross_analyze_body()",
                "3",
            ),
            (true, "SELECT cross_analyze_body()", "4"),
            (false, "ROLLBACK; SELECT cross_analyze_body()", "5"),
            (true, "SELECT cross_analyze_body()", "5"),
            (
                false,
                "ANALYZE cross_analyze_anchor; SELECT cross_analyze_body()",
                "6",
            ),
            (true, "SELECT cross_analyze_body()", "7"),
            (
                false,
                "BEGIN; INSERT INTO cross_analyze_anchor VALUES(2); ANALYZE cross_analyze_anchor; SELECT cross_analyze_body()",
                "8",
            ),
            (true, "SELECT cross_analyze_body()", "9"),
            (false, "COMMIT; SELECT cross_analyze_body()", "10"),
            (true, "SELECT cross_analyze_body()", "11"),
            (
                false,
                "ANALYZE cross_analyze_unrelated; SELECT cross_analyze_body()",
                "11",
            ),
        ] {
            let session = if use_second { &mut second } else { &mut first };
            // Match psql's separate messages while BEGIN keeps explicit transactions open.
            for command in statement.split(';') {
                session.success(&mut engine, command.trim(), true);
            }
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT last_value FROM cross_analyze_checks",
                    true
                )),
                [expected],
                "{statement}"
            );
        }
        drop((first, second, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}
