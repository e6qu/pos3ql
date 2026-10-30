use super::*;

#[test]
fn cursor_retained_fetch_advances_only_requested_rows_and_keeps_opening_values() {
    use crate::config::TransactionMode;
    use crate::sql::cursor::FetchMotion;

    for mode in [TransactionMode::Mvcc, TransactionMode::Locking] {
        let mut config = test_config("cursor-retained-fetch");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE retained_orders(id integer, label text); \
             INSERT INTO retained_orders VALUES (1,'first'),(2,'second'),(3,'third'); \
             BEGIN; DECLARE orders NO SCROLL CURSOR FOR SELECT id,label FROM retained_orders",
            true,
        );
        let (slot, _) = session
            .cursors
            .read_request("orders", FetchMotion::Count(1), false)
            .unwrap()
            .expect("retained physical plan");
        assert_eq!(session.cursors.cached_rows(slot), 0);
        assert_eq!(
            session
                .cursors
                .read_request("orders", FetchMotion::BackwardAll, false)
                .unwrap_err()
                .sqlstate,
            "55000"
        );
        assert!(
            data_rows(&session.success(
                &mut engine,
                "FETCH 0 FROM orders; MOVE 0 FROM orders",
                true
            ))
            .is_empty()
        );
        assert_eq!(session.cursors.cached_rows(slot), 0);
        let output = session.success(&mut engine, "FETCH NEXT FROM orders", true);
        assert_eq!(data_rows(&output), ["1|first"]);
        assert_eq!(session.cursors.cached_rows(slot), 1);
        session.success(
            &mut engine,
            "UPDATE retained_orders SET label='changed' WHERE id=2; \
            INSERT INTO retained_orders VALUES (4,'later')",
            true,
        );
        let output = session.success(&mut engine, "FETCH FORWARD 1 FROM orders", true);
        assert_eq!(data_rows(&output), ["2|second"]);
        assert_eq!(session.cursors.cached_rows(slot), 2);
        let output = session.success(
            &mut engine,
            "MOVE 0 FROM orders; FETCH ALL FROM orders; FETCH NEXT FROM orders",
            true,
        );
        assert_eq!(data_rows(&output), ["3|third"]);
        assert_eq!(session.cursors.cached_rows(slot), 3);
        session.success(&mut engine, "CLOSE orders; COMMIT", true);
        assert!(!session.cursors.exists("orders"));
    }
}

#[test]
fn cursor_retained_native_open_captures_inputs_and_fetch_into_uses_the_reader() {
    let config = test_config("cursor-retained-native");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE retained_native(id integer); \
        INSERT INTO retained_native VALUES (1),(2); \
        CREATE FUNCTION retained_native_fetch() RETURNS text LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor; label text := 'opening'; n integer; saved text; BEGIN \
          OPEN c NO SCROLL FOR SELECT id,label FROM retained_native; label := 'changed'; \
          FETCH NEXT FROM c INTO n,saved; CLOSE c; RETURN saved; END $$",
        true,
    );
    let output = session.success(&mut engine, "SELECT retained_native_fetch()", true);
    assert_eq!(data_rows(&output), ["opening"]);
}

#[test]
fn cursor_retained_table_cannot_be_dropped_or_rewritten_until_close() {
    let config = test_config("cursor-retained-table-use");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE busy_orders(id integer); CREATE INDEX busy_idx ON busy_orders(id); \
        INSERT INTO busy_orders VALUES(1),(2); BEGIN; \
        DECLARE busy NO SCROLL CURSOR FOR SELECT id FROM busy_orders; SAVEPOINT ddl",
        true,
    );
    for sql in [
        "DROP TABLE busy_orders",
        "TRUNCATE busy_orders",
        "ALTER TABLE busy_orders ADD COLUMN extra integer",
        "CLUSTER busy_orders USING busy_idx",
    ] {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(error_sqlstates(&output), ["55006"], "{sql}");
        assert!(
            String::from_utf8_lossy(&output)
                .contains("being used by active queries in this session")
        );
        session.success(&mut engine, "ROLLBACK TO SAVEPOINT ddl", true);
    }
    session.success(
        &mut engine,
        "ALTER TABLE busy_orders RENAME TO renamed_orders",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM busy", true)),
        ["1", "2"]
    );
    session.success(&mut engine, "SAVEPOINT finished", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "DROP TABLE renamed_orders", true)),
        ["55006"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT finished; CLOSE busy; \
        ALTER TABLE renamed_orders ADD COLUMN extra integer; DROP TABLE renamed_orders; COMMIT",
        true,
    );
}

#[test]
fn cursor_failed_fetch_stays_failed_after_savepoint_rollback() {
    let config = test_config("cursor-failed-fetch");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for projection in ["id FROM fetch_failure", "1 FROM generate_series(1,2)"] {
        session.success(&mut engine, "CREATE TABLE fetch_failure(id integer); INSERT INTO fetch_failure VALUES(1),(2); BEGIN", true);
        session.success(
            &mut engine,
            &format!("DECLARE failed NO SCROLL CURSOR FOR SELECT {projection}"),
            true,
        );
        session.success(&mut engine, "FETCH NEXT FROM failed; MOVE 0 FROM failed; MOVE RELATIVE 0 FROM failed; SAVEPOINT before_error", true);
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "FETCH 0 FROM failed", true)),
            ["55000"]
        );
        session.success(&mut engine, "ROLLBACK TO SAVEPOINT before_error", true);
        let output = session.execute(&mut engine, "FETCH NEXT FROM failed", true);
        assert_eq!(error_sqlstates(&output), ["55000"]);
        assert!(String::from_utf8_lossy(&output).contains("portal \"failed\" cannot be run"));
        session.success(
            &mut engine,
            "ROLLBACK TO SAVEPOINT before_error; CLOSE failed; DROP TABLE fetch_failure; COMMIT",
            true,
        );
    }
}

#[test]
fn cursor_retained_capacity_error_releases_its_reader_and_stays_failed() {
    let mut config = test_config("cursor-retained-fetch-capacity");
    config.cursor_bytes = 6;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE small_result(id integer); INSERT INTO small_result VALUES(1); \
        BEGIN; DECLARE small NO SCROLL CURSOR FOR SELECT id FROM small_result; SAVEPOINT before_fetch", true);
    let output = session.execute(&mut engine, "FETCH NEXT FROM small", true);
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert!(String::from_utf8_lossy(&output).contains("cursor result exceeds cursor_bytes"));
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_fetch; DROP TABLE small_result",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM small", true)),
        ["55000"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_fetch; CLOSE small; ROLLBACK",
        true,
    );
}

#[test]
fn cursor_retained_input_capacity_names_its_budget_and_releases_failed_open() {
    let mut config = test_config("cursor-retained-input-capacity");
    config.max_cursors = 1;
    config.cursor_query_bytes = 8192;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE many_inputs(id integer); \
        INSERT INTO many_inputs SELECT g FROM generate_series(1,512) AS g; BEGIN",
        true,
    );
    let output = session.execute(
        &mut engine,
        "DECLARE too_many NO SCROLL CURSOR FOR SELECT id FROM many_inputs",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert!(String::from_utf8_lossy(&output).contains("cursor_query_bytes"));
    assert!(!session.cursors.exists("too_many"));
    session.success(
        &mut engine,
        "ROLLBACK; TRUNCATE many_inputs; BEGIN; \
        DECLARE fits NO SCROLL CURSOR FOR SELECT id FROM many_inputs; FETCH ALL FROM fits; COMMIT",
        true,
    );
}

#[test]
fn cursor_dynamic_open_rejects_multiple_commands_before_issuing_sequence_values() {
    let config = test_config("cursor-open-command-count");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE cursor_open_calls; \
        CREATE FUNCTION cursor_extra_commands() RETURNS void LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor; BEGIN \
          OPEN c FOR EXECUTE 'SELECT nextval(''cursor_open_calls''); SELECT 2'; \
          RETURN; END $$",
        true,
    );
    let output = session.execute(&mut engine, "SELECT cursor_extra_commands()", true);
    assert_eq!(error_sqlstates(&output), ["42P11"]);
    assert!(String::from_utf8_lossy(&output).contains("cannot open multi-query plan as cursor"));
    let output = session.success(
        &mut engine,
        "SELECT last_value, is_called FROM cursor_open_calls",
        true,
    );
    assert_eq!(data_rows(&output), ["1|f"]);
}

#[test]
fn cursor_declaration_rejects_conflicting_options_and_write_effects() {
    let config = test_config("cursor-declaration-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut errors = Vec::new();
    let mut rows = Vec::new();
    for sql in include_str!("../../../tests/external/differential_exact/35_cursor_declarations.sql")
        .split(';')
        .filter(|sql| !sql.trim().is_empty())
    {
        let output = session.execute(&mut engine, sql, true);
        errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
        rows.extend(data_rows(&output));
    }
    assert_eq!(
        errors,
        [
            "42P11", "42P11", "42P11", "42P11", "0A000", "0A000", "42P11", "0A000", "0A000",
            "0A000", "0A000"
        ]
    );
    assert_eq!(rows, ["2|3", "1|f"]);
}

#[test]
fn cursor_fetch_renders_saved_values_with_current_session_settings() {
    let config = test_config("cursor-fetch-rendering");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let values = "DATE '2020-01-02', ARRAY[DATE '2020-01-02'], \
        ROW(DATE '2020-01-02', TIMESTAMPTZ '2020-01-02 03:04:05+00'), \
        '\\x5c00'::bytea, 1.234567891234567::float8";
    for hold in [false, true] {
        session.success(
            &mut engine,
            "SET DateStyle = 'ISO, MDY'; SET TimeZone = 'UTC'; \
            SET bytea_output = 'hex'; SET extra_float_digits = 3; BEGIN",
            true,
        );
        session.success(
            &mut engine,
            &format!(
                "DECLARE rendered SCROLL CURSOR {} FOR SELECT {values} FROM generate_series(1,3)",
                if hold { "WITH HOLD" } else { "" },
            ),
            true,
        );
        if hold {
            session.success(&mut engine, "COMMIT", true);
        }
        for setting in [
            "SET DateStyle = 'ISO, MDY'",
            "SET DateStyle = 'SQL, DMY'; SET TimeZone = 'Europe/Bucharest'; \
                SET bytea_output = 'escape'; SET extra_float_digits = 0",
            "SET DateStyle = 'German'; SET TimeZone = 'UTC'",
        ] {
            session.success(&mut engine, setting, true);
            let expected = session.success(&mut engine, &format!("SELECT {values}"), true);
            let fetched = session.success(&mut engine, "FETCH NEXT FROM rendered", true);
            assert_eq!(
                data_rows(&fetched),
                data_rows(&expected),
                "hold={hold}, {setting}"
            );
            assert_eq!(
                row_description_type_oids(&fetched),
                row_description_type_oids(&expected)
            );
        }
        session.success(&mut engine, "CLOSE rendered", true);
        if !hold {
            session.success(&mut engine, "COMMIT", true);
        }
    }
}

#[test]
fn cursor_fetch_into_keeps_dates_typed_after_date_order_changes() {
    let config = test_config("cursor-fetch-typed-assignment");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE FUNCTION fetch_typed_date() RETURNS date \
        LANGUAGE plpgsql AS $$ DECLARE c refcursor; d date; BEGIN \
          OPEN c FOR SELECT DATE '2020-01-02'; \
          SET DateStyle = 'SQL, DMY'; FETCH NEXT FROM c INTO d; CLOSE c; RETURN d; \
        END $$",
        true,
    );
    let output = session.success(
        &mut engine,
        "SET DateStyle = 'ISO, MDY'; SELECT fetch_typed_date() = DATE '2020-01-02'",
        true,
    );
    assert_eq!(data_rows(&output), ["t"]);
}

#[test]
fn cursor_typed_rows_use_exact_capacity_and_release_failed_declarations() {
    let mut config = test_config("cursor-typed-capacity");
    config.max_cursors = 1;
    // One zero-column row needs four length bytes and two column-count bytes.
    config.cursor_bytes = 6;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for _ in 0..2 {
        session.success(
            &mut engine,
            "BEGIN; DECLARE fits CURSOR FOR SELECT; FETCH ALL FROM fits; COMMIT",
            true,
        );
        let failed = session.execute(
            &mut engine,
            "BEGIN; DECLARE too_wide CURSOR FOR SELECT 1",
            true,
        );
        assert_eq!(error_sqlstates(&failed), ["54000"]);
        assert!(String::from_utf8_lossy(&failed).contains("cursor result exceeds cursor_bytes"));
        session.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn cursor_description_exhaustion_names_its_own_capacity_and_releases_the_slot() {
    let alias = "a".repeat(63);
    let projection = (0..20)
        .map(|_| format!("1 AS {alias}"))
        .collect::<Vec<_>>()
        .join(",");
    for bytes in [128, 4096] {
        let mut config = test_config("cursor-description-capacity");
        config.max_cursors = 1;
        config.cursor_bytes = bytes;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            &format!(
                "CREATE FUNCTION wide_cursor_labels() RETURNS void LANGUAGE plpgsql AS $$ \
                 DECLARE c refcursor; BEGIN OPEN c FOR SELECT {projection}; RETURN; END $$"
            ),
            true,
        );
        for statement in [
            format!("DECLARE wide_labels CURSOR FOR SELECT {projection}"),
            "SELECT wide_cursor_labels()".to_owned(),
        ] {
            session.success(&mut engine, "BEGIN", true);
            let failed = session.execute(&mut engine, &statement, true);
            assert_eq!(error_sqlstates(&failed), ["54000"]);
            let diagnostic = String::from_utf8_lossy(&failed);
            assert!(
                diagnostic.contains("cursor row description exceeds 1024 bytes"),
                "{diagnostic}"
            );
            assert!(!diagnostic.contains("cursor_bytes"), "{diagnostic}");
            session.success(&mut engine, "ROLLBACK", true);
            let output = session.success(
                &mut engine,
                "BEGIN; DECLARE reused CURSOR FOR SELECT 1; FETCH ALL FROM reused; COMMIT",
                true,
            );
            assert_eq!(data_rows(&output), ["1"]);
        }
    }
}

#[test]
fn cursor_binary_fetch_does_not_change_later_statement_formats() {
    let config = test_config("cursor-fetch-format-scope");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(
        &mut engine,
        "BEGIN; DECLARE binary_values BINARY CURSOR FOR SELECT 17; \
         FETCH NEXT FROM binary_values; SELECT 42; COMMIT",
        true,
    );
    let rows = data_rows(&output);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].as_bytes(), 17i32.to_be_bytes());
    assert_eq!(rows[1], "42");
}

#[test]
fn cursor_empty_select_lists_preserve_row_counts_and_required_target_lists() {
    let config = test_config("cursor-empty-select-lists");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for (sql, count) in [
        ("SELECT", 1),
        ("SELECT ALL FROM generate_series(1,2)", 2),
        ("SELECT FROM generate_series(1,3) WHERE false", 0),
        ("SELECT FROM generate_series(1,3) LIMIT 1 OFFSET 1", 1),
        ("SELECT FROM (SELECT) AS empty_source", 1),
        ("SELECT UNION ALL SELECT", 2),
    ] {
        let output = session.success(&mut engine, sql, true);
        assert_eq!(data_rows(&output), vec![""; count], "{sql}");
    }
    session.success(
        &mut engine,
        "CREATE TABLE empty_returning_check(n integer)",
        true,
    );
    for sql in [
        "SELECT DISTINCT",
        "SELECT DISTINCT ON (1)",
        "INSERT INTO empty_returning_check VALUES (1) RETURNING",
    ] {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(error_sqlstates(&output), ["42601"], "{sql}");
    }
}

#[test]
fn cursor_motion_counts_match_postgresql_integer_grammar() {
    let config = test_config("cursor-motion-counts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for sql in
        include_str!("../../../tests/external/differential_exact/37_cursor_motion_counts.sql")
            .split(';')
            .filter(|sql| !sql.trim().is_empty())
    {
        let output = session.execute(&mut engine, sql, true);
        rows.extend(data_rows(&output));
        errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
    }
    assert_eq!(rows, ["1", "2", "1", "2", "3", "3", "2", "1"]);
    assert_eq!(errors, ["42601"; 5]);
}
