use super::*;

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
        session.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn cursor_binary_fetch_does_not_change_later_statement_formats() {
    let config = test_config("cursor-fetch-format-scope");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(&mut engine,
        "BEGIN; DECLARE binary_values BINARY CURSOR FOR SELECT 17; \
         FETCH NEXT FROM binary_values; SELECT 42; COMMIT", true);
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
    session.success(&mut engine, "CREATE TABLE empty_returning_check(n integer)", true);
    for sql in [
        "SELECT DISTINCT",
        "SELECT DISTINCT ON (1)",
        "INSERT INTO empty_returning_check VALUES (1) RETURNING",
    ] {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(error_sqlstates(&output), ["42601"], "{sql}");
    }
}
