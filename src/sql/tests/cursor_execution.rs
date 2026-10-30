use super::*;

#[test]
fn cursor_array_domain_defaults_encode_their_composite_base() {
    let config = test_config("cursor-array-composite-domain-default");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TYPE cursor_pair AS (id integer,note text); \
        CREATE DOMAIN approved_pair AS cursor_pair CHECK ((VALUE).id>0); \
        CREATE TABLE pair_arrays(items approved_pair[] DEFAULT ARRAY[ROW(7,'saved')::approved_pair]); \
        INSERT INTO pair_arrays DEFAULT VALUES; BEGIN; \
        DECLARE pairs NO SCROLL CURSOR FOR SELECT items[1] FROM pair_arrays", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM pairs; COMMIT", true)),
        ["(7,saved)"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT (items[1]).id,(items[1]).note,pg_typeof(items) FROM pair_arrays",
            true
        )),
        ["7|saved|approved_pair[]"]
    );
}

#[test]
fn cursor_array_constructors_preserve_common_types_and_fixed_arena_limits() {
    use crate::sql::cursor::FetchMotion;
    use crate::sql::types::ArrElem;
    let config = test_config("cursor-array-common-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE typed_arrays(id integer, vals integer[]); \
        INSERT INTO typed_arrays VALUES(1,ARRAY[10,20]),(2,ARRAY[30,40]); \
        BEGIN; DECLARE typed NO SCROLL CURSOR FOR SELECT \
        CASE WHEN id=1 THEN ARRAY[id,2147483648] ELSE ARRAY[id,id+1] END, \
        CASE WHEN id=1 THEN vals[1:2] ELSE ARRAY[2147483648] END, \
        ARRAY[ARRAY[id],ARRAY[2147483648]], ARRAY[id]||ARRAY[2147483648] FROM typed_arrays",
        true,
    );
    session
        .cursors
        .read_request("typed", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained typed array plan");
    for expected in [
        "{1,2147483648}|{10,20}|{{1},{2147483648}}|{1,2147483648}",
        "{2,3}|{2147483648}|{{2},{2147483648}}|{2,2147483648}",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM typed", true)),
            [expected]
        );
        let mut values = [Datum::Null; 4];
        crate::mem::guard::forbid_alloc(|| {
            session
                .cursors
                .copy_row(
                    "typed",
                    session.cursors.emitted()[0],
                    &session.arena,
                    &mut values,
                )
                .unwrap();
            for value in values {
                assert!(matches!(
                    value,
                    Datum::Array {
                        element: ArrElem::Int8,
                        ..
                    }
                ));
            }
        });
    }
    session.success(&mut engine, "COMMIT", true);
    let members = (0..300)
        .map(|index| format!("id+{index}"))
        .collect::<Vec<_>>()
        .join(",");
    session.success(&mut engine, &format!("BEGIN; DECLARE wide_array NO SCROLL CURSOR FOR SELECT (ARRAY[{members}])[300] FROM typed_arrays"), true);
    session
        .cursors
        .read_request("wide_array", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained 300-element constructor");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM wide_array; COMMIT", true)),
        ["300", "301"]
    );
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT pg_typeof(ARRAY[1::smallint,2]), \
        pg_typeof(ARRAY[1::real,2::numeric]), pg_typeof(ARRAY[DATE '2026-10-01',TIMESTAMP '2026-10-02']), \
        ARRAY[]::integer[],ARRAY[ARRAY[]::integer[],NULL::integer[]]", true)),
        ["integer[]|real[]|timestamp without time zone[]|{}|{}"]);
    for (sql, state) in [
        ("SELECT ARRAY[]", "42P18"),
        ("SELECT ARRAY[1,TRUE]", "42804"),
        ("SELECT ARRAY[1::money,2]", "42846"),
        ("SELECT ARRAY[ARRAY[1],NULL::integer[]]", "2202E"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state]
        );
    }
    session.success(&mut engine, "CREATE DOMAIN array_day AS date; \
        CREATE FUNCTION captured_null_array(input array_day) RETURNS refcursor LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor := 'saved_null_array'; BEGIN \
        OPEN c NO SCROLL FOR SELECT ARRAY[input] FROM typed_arrays; input := DATE '2026-10-30'; RETURN c; END $$; \
        BEGIN; SELECT captured_null_array(NULL)", true);
    session
        .cursors
        .read_request("saved_null_array", FetchMotion::Count(1), false)
        .unwrap()
        .expect("captured typed NULL array plan");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM saved_null_array", true)),
        ["{NULL}"]
    );
    let mut values = [Datum::Null; 1];
    crate::mem::guard::forbid_alloc(|| {
        session
            .cursors
            .copy_row(
                "saved_null_array",
                session.cursors.emitted()[0],
                &session.arena,
                &mut values,
            )
            .unwrap();
        assert!(matches!(
            values[0],
            Datum::Array {
                element: ArrElem::Domain { .. },
                ..
            }
        ));
    });
    session.success(&mut engine, "COMMIT", true);
}

#[test]
fn cursor_retained_arrays_keep_captured_inputs_and_defer_index_errors() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-arrays");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE array_input(id integer, vals integer[]); \
        INSERT INTO array_input VALUES(1,ARRAY[10,20,30]),(2,ARRAY[40,50,60]),(3,NULL); \
        BEGIN; DECLARE arrays NO SCROLL CURSOR FOR SELECT vals[id],(ARRAY[id,id+1])[2],vals[2:3] \
        FROM array_input WHERE (ARRAY[id,0])[1]>0",
        true,
    );
    session
        .cursors
        .read_request("arrays", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained array plan");
    session.success(
        &mut engine,
        "UPDATE array_input SET vals=ARRAY[99,99,99] WHERE id=2",
        true,
    );
    for expected in ["10|2|{20,30}", "50|3|{50,60}", "NULL|4|NULL"] {
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM arrays", true)),
            [expected]
        );
    }
    session.success(&mut engine, "CLOSE arrays; DECLARE index_failure NO SCROLL CURSOR FOR SELECT vals[1/(id-2)] FROM array_input", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM index_failure", true)),
        ["NULL"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM index_failure", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK; CREATE FUNCTION captured_array(input integer[]) RETURNS refcursor LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor := 'saved_array'; BEGIN OPEN c NO SCROLL FOR SELECT input[id] FROM array_input; \
        input := ARRAY[99,99,99]; RETURN c; END $$; BEGIN; SELECT captured_array(ARRAY[7,8,9])", true);
    session
        .cursors
        .read_request("saved_array", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained captured array");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM saved_array", true)),
        ["7", "8", "9"]
    );
    session.success(&mut engine, "COMMIT", true);
}

#[test]
fn cursor_retained_case_preserves_operator_types_and_defers_selected_row_errors() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-case-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE conditional_input(id integer, day date, start_day date, at timestamp, start_at timestamp); \
        INSERT INTO conditional_input VALUES \
        (1,DATE '2026-10-03',DATE '2026-10-01',TIMESTAMP '2026-10-03',TIMESTAMP '2026-10-01'), \
        (2,DATE '2026-10-06',DATE '2026-10-01',TIMESTAMP '2026-10-06',TIMESTAMP '2026-10-01'); \
        BEGIN; DECLARE formulas NO SCROLL CURSOR FOR \
        SELECT (day-start_day)+id AS elapsed, CASE WHEN id%2=0 THEN id*10 ELSE 2147483648 END AS chosen, \
        CASE WHEN id=1 THEN at-start_at ELSE NULL END AS span FROM conditional_input \
        WHERE CASE WHEN id>0 THEN TRUE ELSE FALSE END", true);
    session
        .cursors
        .read_request("formulas", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained conditional plan");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM formulas", true)),
        ["3|2147483648|2 days"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM formulas", true)),
        ["7|20|NULL"]
    );
    let mut values = [Datum::Null; 3];
    crate::mem::guard::forbid_alloc(|| {
        let count = session
            .cursors
            .copy_row(
                "formulas",
                session.cursors.emitted()[0],
                &session.arena,
                &mut values,
            )
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(values[0], Datum::Int4(7));
        assert_eq!(values[1], Datum::Int8(20));
    });
    session.success(&mut engine, "CLOSE formulas; \
        DECLARE conditional_failure NO SCROLL CURSOR FOR SELECT CASE WHEN id=2 THEN 100/(id-2) ELSE id END FROM conditional_input", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM conditional_failure", true)),
        ["1"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM conditional_failure", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn cursor_view_planning_preserves_sources_and_pattern_boolean_types() {
    let config = test_config("cursor-view-pattern-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE pattern_input(label text); \
        INSERT INTO pattern_input VALUES ('a_c'),('abc'); \
        CREATE VIEW pattern_view AS TABLE pattern_input; BEGIN; \
        DECLARE view_rows CURSOR FOR TABLE pattern_view",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM view_rows; COMMIT", true)),
        ["a_c", "abc"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT label,pg_typeof(label SIMILAR TO 'a#_c' ESCAPE '#') FROM pattern_input \
        WHERE label SIMILAR TO 'a#_c' ESCAPE '#' ORDER BY label",
            true
        )),
        ["a_c|boolean"]
    );
    session.success(
        &mut engine,
        "BEGIN; DECLARE pattern_rows NO SCROLL CURSOR FOR \
        SELECT label FROM pattern_view WHERE label SIMILAR TO 'a#_c' ESCAPE '#'",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM pattern_rows; COMMIT", true)),
        ["a_c"]
    );
}

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
        assert_eq!(session.cursors.produced_rows(slot), 0);
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
        assert_eq!(session.cursors.produced_rows(slot), 0);
        let output = session.success(&mut engine, "FETCH NEXT FROM orders", true);
        assert_eq!(data_rows(&output), ["1|first"]);
        assert_eq!(session.cursors.produced_rows(slot), 1);
        session.success(
            &mut engine,
            "UPDATE retained_orders SET label='changed' WHERE id=2; \
            INSERT INTO retained_orders VALUES (4,'later')",
            true,
        );
        let output = session.success(&mut engine, "FETCH FORWARD 1 FROM orders", true);
        assert_eq!(data_rows(&output), ["2|second"]);
        assert_eq!(session.cursors.produced_rows(slot), 2);
        let output = session.success(
            &mut engine,
            "MOVE 0 FROM orders; FETCH ALL FROM orders; FETCH NEXT FROM orders",
            true,
        );
        assert_eq!(data_rows(&output), ["3|third"]);
        assert_eq!(session.cursors.produced_rows(slot), 3);
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
fn cursor_retained_narrow_projection_borrows_wide_resident_rows() {
    let mut config = test_config("cursor-retained-wide-resident");
    config.cursor_query_bytes = 8192;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE wide_input(id integer, payload text)",
        true,
    );
    session.success(
        &mut engine,
        &format!(
            "INSERT INTO wide_input VALUES(7,'{}')",
            "x".repeat(128 << 10)
        ),
        true,
    );
    session.success(
        &mut engine,
        "BEGIN; DECLARE narrow NO SCROLL CURSOR FOR SELECT id FROM wide_input; \
        UPDATE wide_input SET id=9",
        true,
    );
    let output = session.success(&mut engine, "FETCH ALL FROM narrow", true);
    assert_eq!(data_rows(&output), ["7"]);
    session.success(&mut engine, "CLOSE narrow; COMMIT", true);
}

#[test]
fn cursor_retained_reuses_fetch_memory_and_discards_move_and_skipped_rows() {
    use crate::sql::cursor::FetchMotion;
    let mut config = test_config("cursor-retained-fetch-buffer");
    config.max_cursors = 1;
    config.cursor_bytes = 32;
    config.cursor_query_bytes = 8192;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE forward_input(id integer); \
        INSERT INTO forward_input SELECT g FROM generate_series(1,128) AS g; BEGIN; \
        DECLARE forward_rows NO SCROLL CURSOR FOR SELECT id FROM forward_input",
        true,
    );
    let (slot, _) = session
        .cursors
        .read_request("forward_rows", FetchMotion::Count(1), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE BACKWARD ALL FROM forward_rows", true)),
        ["MOVE 0"]
    );
    for id in 1..=128 {
        let output = session.success(&mut engine, "FETCH NEXT FROM forward_rows", true);
        assert_eq!(data_rows(&output), [id.to_string()]);
        assert_eq!(session.cursors.produced_rows(slot), id);
        assert_eq!(session.cursors.buffered_rows(slot), 1);
    }
    session.success(
        &mut engine,
        "CLOSE forward_rows; \
        DECLARE forward_rows NO SCROLL CURSOR FOR SELECT id FROM forward_input",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ABSOLUTE 100 FROM forward_rows", true)),
        ["100"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 1);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH RELATIVE 10 FROM forward_rows", true)),
        ["110"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 1);
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE FORWARD 10 FROM forward_rows", true)),
        ["MOVE 10"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 0);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM forward_rows", true)),
        ["121"]
    );
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE ALL FROM forward_rows", true)),
        ["MOVE 7"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 0);
    session.success(
        &mut engine,
        "CLOSE forward_rows; \
        DECLARE forward_rows NO SCROLL CURSOR FOR SELECT id FROM forward_input; SAVEPOINT last_row",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH LAST FROM forward_rows", true)),
        ["55000"]
    );
    assert_eq!(
        session.cursors.produced_rows(slot),
        128,
        "LAST advances forward before its forbidden backward step"
    );
    assert_eq!(session.cursors.buffered_rows(slot), 0);
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT last_row; CLOSE forward_rows; COMMIT",
        true,
    );
}

#[test]
fn cursor_retained_filters_advance_input_without_saving_rejected_rows() {
    use crate::config::TransactionMode;
    use crate::sql::cursor::FetchMotion;
    for mode in [TransactionMode::Mvcc, TransactionMode::Locking] {
        let mut config = test_config("cursor-retained-filter");
        config.transaction_mode = mode;
        config.cursor_bytes = 32;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE filtered_input(id integer); \
             INSERT INTO filtered_input SELECT g FROM generate_series(1,128) AS g; BEGIN; \
             DECLARE filtered NO SCROLL CURSOR FOR \
             SELECT id*2 FROM filtered_input WHERE id%3=0 AND NOT (id IS NULL)",
            true,
        );
        let (slot, _) = session
            .cursors
            .read_request("filtered", FetchMotion::Count(1), false)
            .unwrap()
            .expect("retained filter plan");
        assert_eq!(session.cursors.produced_rows(slot), 0);
        for n in 1..=42 {
            let output = session.success(&mut engine, "FETCH NEXT FROM filtered", true);
            assert_eq!(data_rows(&output), [(n * 6).to_string()]);
            assert_eq!(session.cursors.buffered_rows(slot), 1);
            let statistics = session.success(&mut engine,
                "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='filtered_input'", true);
            assert_eq!(data_rows(&statistics), [format!("1|{}", n * 3)]);
        }
        assert_eq!(
            command_tags(&session.success(&mut engine, "FETCH NEXT FROM filtered", true)),
            ["FETCH 0"]
        );
        let statistics = session.success(&mut engine,
            "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='filtered_input'", true);
        assert_eq!(data_rows(&statistics), ["1|128"]);
        session.success(&mut engine, "CLOSE filtered; COMMIT", true);
    }
}

#[test]
fn cursor_retained_default_mode_defers_reads_and_matches_plan_scrollability() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-default-mode");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE default_input(id integer); INSERT INTO default_input VALUES (1),(2),(3); \
        BEGIN; DECLARE automatic CURSOR FOR SELECT id FROM default_input",
        true,
    );
    let (slot, _) = session
        .cursors
        .read_request("automatic", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained default cursor");
    assert_eq!(session.cursors.produced_rows(slot), 0);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT is_scrollable FROM pg_cursors WHERE name='automatic'",
            true
        )),
        ["t"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM automatic", true)),
        ["1"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH LAST FROM automatic", true)),
        ["3"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH FIRST FROM automatic", true)),
        ["1"]
    );
    session.success(&mut engine, "CLOSE automatic; COMMIT", true);
    for predicate in ["FALSE", "'f'", "NULL AND TRUE"] {
        session.success(&mut engine, &format!("BEGIN; DECLARE empty_cursor CURSOR FOR SELECT id FROM default_input WHERE {predicate}"), true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_scrollable FROM pg_cursors WHERE name='empty_cursor'",
                true
            )),
            ["f"]
        );
        assert_eq!(
            command_tags(&session.success(&mut engine, "FETCH ALL FROM empty_cursor", true)),
            ["FETCH 0"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='default_input'", true)), ["0|0"]);
        session.success(&mut engine, "CLOSE empty_cursor; COMMIT", true);
    }
}

#[test]
fn cursor_retained_default_filter_is_bound_to_captured_boolean_values() {
    let config = test_config("cursor-retained-captured-filter");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE captured_filter_input(id integer); INSERT INTO captured_filter_input VALUES (1),(2); \
        CREATE FUNCTION captured_filter(keep boolean) RETURNS refcursor LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor := 'captured'; BEGIN OPEN c FOR SELECT id FROM captured_filter_input WHERE keep; \
          keep := NOT keep; RETURN c; END $$", true);
    for (value, scrollable, expected) in [
        ("false", "f", vec![]),
        ("true", "t", vec!["1", "2"]),
        ("NULL", "f", vec![]),
    ] {
        session.success(
            &mut engine,
            &format!("BEGIN; SELECT captured_filter({value})"),
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_scrollable FROM pg_cursors WHERE name='captured'",
                true
            )),
            [scrollable]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH ALL FROM captured", true)),
            expected
        );
        session.success(&mut engine, "COMMIT", true);
    }
}

#[test]
fn cursor_retained_declaration_and_empty_filter_do_not_mark_unread_tables_as_read() {
    let config = test_config("cursor-retained-unread-serializable");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "CREATE TABLE unread_cursor_input(id integer); INSERT INTO unread_cursor_input VALUES (1)",
        true,
    );
    for query in [
        "SELECT id FROM unread_cursor_input",
        "SELECT id FROM unread_cursor_input WHERE FALSE",
    ] {
        reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL SERIALIZABLE; DECLARE unread CURSOR FOR {query}"),
            true,
        );
        if query.ends_with("FALSE") {
            reader.success(&mut engine, "FETCH ALL FROM unread", true);
        }
        writer.success(&mut engine, "UPDATE unread_cursor_input SET id=id+1", true);
        reader.success(&mut engine, "COMMIT", true);
    }
}

#[test]
fn cursor_retained_type_analysis_precedes_execution_permissions() {
    let config = test_config("cursor-retained-type-permission-order");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE permission_input(id integer); CREATE ROLE cursor_denied; SET ROLE cursor_denied; BEGIN", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "DECLARE bad NO SCROLL CURSOR FOR SELECT id+TRUE FROM permission_input",
            true
        )),
        ["42883"]
    );
    session.success(&mut engine, "ROLLBACK; BEGIN", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "DECLARE bad NO SCROLL CURSOR FOR SELECT id FROM permission_input",
            true
        )),
        ["42501"]
    );
    session.success(&mut engine, "ROLLBACK; RESET ROLE", true);
}

#[test]
fn cursor_retained_implicit_commit_triggers_can_inspect_the_connection_cursors() {
    let config = test_config("cursor-retained-commit-cursor-catalog");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE cursor_catalog_input(id integer); INSERT INTO cursor_catalog_input VALUES (1); \
        CREATE TABLE cursor_catalog_gate(id integer); \
        CREATE FUNCTION inspect_commit_cursor() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
          IF (SELECT count(*) FROM pg_cursors WHERE name='held') <> 1 THEN RAISE EXCEPTION 'cursor missing'; END IF; \
          RAISE EXCEPTION 'cursor visible'; END $$; \
        CREATE CONSTRAINT TRIGGER inspect_commit_cursor AFTER INSERT ON cursor_catalog_gate DEFERRABLE INITIALLY DEFERRED \
        FOR EACH ROW EXECUTE FUNCTION inspect_commit_cursor()", true);
    let failed = session.execute(&mut engine,
        "DECLARE held CURSOR WITH HOLD FOR SELECT id FROM cursor_catalog_input; INSERT INTO cursor_catalog_gate VALUES (1)", true);
    assert_eq!(
        error_sqlstates(&failed),
        ["P0001"],
        "{}",
        String::from_utf8_lossy(&failed)
    );
    assert!(
        String::from_utf8_lossy(&failed).contains("cursor visible"),
        "{}",
        String::from_utf8_lossy(&failed)
    );
    assert!(!session.cursors.exists("held"));
}

#[test]
fn cursor_trigger_expressions_resolve_subqueries_with_locals_and_transition_rows() {
    let config = test_config("cursor-trigger-expression-subqueries");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE expression_input(id integer); INSERT INTO expression_input VALUES (1),(2); \
        CREATE FUNCTION expression_value(wanted integer) RETURNS integer LANGUAGE plpgsql AS $$ \
        DECLARE saved integer := (SELECT id FROM expression_input WHERE id=wanted); BEGIN \
          IF EXISTS (SELECT id FROM expression_input WHERE id=saved) THEN \
            saved := (SELECT id+10 FROM expression_input WHERE id=saved); END IF; \
          RETURN (SELECT saved); END $$",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT expression_value(1),expression_value(2),expression_value(3)",
            true
        )),
        ["11|12|NULL"]
    );
    session.success(
        &mut engine,
        "CREATE FUNCTION expression_many() RETURNS integer LANGUAGE plpgsql AS $$ \
        BEGIN RETURN (SELECT id FROM expression_input); END $$",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "SELECT expression_many()", true)),
        ["21000"]
    );
    session.success(&mut engine, "CREATE FUNCTION expression_lazy(choose boolean) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN \
        IF choose OR EXISTS (SELECT id FROM expression_input WHERE 10/(id-id)>0) THEN RETURN 7; END IF; RETURN 0; END $$; \
        CREATE FUNCTION expression_choice(choose boolean) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN \
        RETURN CASE WHEN choose THEN 7 ELSE (SELECT 10/(id-id) FROM expression_input WHERE id=1) END; END $$", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT expression_lazy(true),expression_choice(true)",
            true
        )),
        ["7|7"]
    );
    for query in [
        "SELECT expression_lazy(false)",
        "SELECT expression_choice(false)",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, query, true)),
            ["22012"]
        );
    }
    session.success(&mut engine, "CREATE TABLE expression_target(id integer); \
        CREATE FUNCTION expression_transition() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
          IF EXISTS (SELECT id FROM expression_input WHERE id=NEW.id) THEN NEW.id := NEW.id+10; END IF; RETURN NEW; END $$; \
        CREATE TRIGGER expression_transition BEFORE INSERT ON expression_target FOR EACH ROW EXECUTE FUNCTION expression_transition()", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "INSERT INTO expression_target VALUES (1),(3) RETURNING id",
            true
        )),
        ["11", "3"]
    );
    session.success(&mut engine, "CREATE FUNCTION expression_membership(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN \
        RETURN (SELECT wanted) IN (SELECT id FROM expression_input); END $$; \
        CREATE FUNCTION expression_quantified(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN \
        RETURN wanted > ALL (SELECT id FROM expression_input); END $$", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT expression_membership(1),expression_membership(3),expression_membership(NULL), \
        expression_quantified(3),expression_quantified(1)",
            true
        )),
        ["t|f|NULL|t|f"]
    );
}

#[test]
fn cursor_retained_hold_saves_unread_rows_at_commit_and_preserves_position() {
    use crate::sql::cursor::FetchMotion;
    for scroll in ["NO SCROLL", "SCROLL"] {
        let config = test_config("cursor-retained-hold");
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine, &format!(
            "CREATE TABLE hold_input(id integer); INSERT INTO hold_input VALUES (1),(2),(3),(4); \
             BEGIN; DECLARE held {scroll} CURSOR WITH HOLD FOR SELECT id*10 FROM hold_input"), true);
        let (slot, _) = session
            .cursors
            .read_request("held", FetchMotion::Count(1), false)
            .unwrap()
            .expect("retained held reader before commit");
        assert_eq!(session.cursors.produced_rows(slot), 0);
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["10"]
        );
        session.success(&mut engine, "COMMIT; DROP TABLE hold_input", true);
        assert!(
            session
                .cursors
                .read_request("held", FetchMotion::Count(1), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["20"]
        );
        session.success(&mut engine, "BEGIN", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["30"]
        );
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["40"]
        );
        if scroll == "SCROLL" {
            assert_eq!(
                data_rows(&session.success(&mut engine, "FETCH FIRST FROM held", true)),
                ["10"]
            );
        }
        session.success(&mut engine, "CLOSE held", true);
        session.success(
            &mut engine,
            &format!(
                "BEGIN; CREATE TEMPORARY TABLE temporary_hold_input(id integer) ON COMMIT DROP; \
             INSERT INTO temporary_hold_input VALUES (7); \
             DECLARE held {scroll} CURSOR WITH HOLD FOR SELECT id FROM temporary_hold_input; COMMIT"
            ),
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH ALL FROM held; CLOSE held", true)),
            ["7"]
        );
    }
}

#[test]
fn cursor_retained_autocommit_hold_survives_later_rollback_and_failed_query_message() {
    let config = test_config("cursor-retained-autocommit-hold");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE automatic_hold_input(id integer); \
        INSERT INTO automatic_hold_input VALUES (1),(2); \
        DECLARE permanent NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM automatic_hold_input",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; FETCH NEXT FROM permanent; ROLLBACK",
            true
        )),
        ["1"]
    );
    let failed = session.execute(&mut engine,
        "DECLARE discarded NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM automatic_hold_input; SELECT 1/0", true);
    assert_eq!(error_sqlstates(&failed), ["22012"]);
    assert!(!session.cursors.exists("discarded"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM permanent", true)),
        ["2"]
    );
    session.success(&mut engine, "CLOSE permanent", true);
}

#[test]
fn cursor_retained_hold_finishes_with_a_legacy_large_object_transaction() {
    let config = test_config("cursor-retained-legacy-commit");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE legacy_hold_input(id integer); INSERT INTO legacy_hold_input VALUES (1),(2)",
        true,
    );
    session.send.clear();
    session.arena.reset();
    let result = crate::mem::guard::forbid_alloc(|| {
        engine.execute_extended(
            "DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM legacy_hold_input",
            &session.arena,
            &[],
            &[],
            &mut session.transaction,
            &mut session.pool,
            &mut session.cursors,
            &mut session.guc,
            &mut Responder::new(&mut session.send),
            session.connection_id,
            false,
        )
    })
    .unwrap();
    assert_eq!(result, ExtendedExecutionStatus::Complete(true));
    assert_eq!(session.transaction.mode, TxnMode::Implicit);
    session.send.clear();
    session.arena.reset();
    let result = crate::mem::guard::forbid_alloc(|| {
        engine.execute_large_object_fast_path(
            3457,
            &[Datum::Oid(4242), Datum::Bytea(b"held")],
            true,
            &session.arena,
            &mut session.transaction,
            &session.guc,
            &mut session.cursors,
            &mut Responder::new(&mut session.send),
            session.connection_id,
        )
    })
    .unwrap();
    assert_eq!(result, ExtendedExecutionStatus::Complete(true));
    assert!(!session.transaction.is_active());
    assert_eq!(
        data_rows(&session.success(&mut engine, "BEGIN; FETCH NEXT FROM held; ROLLBACK", true)),
        ["1"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM held; CLOSE held", true)),
        ["2"]
    );
}

#[test]
fn cursor_retained_hold_commit_errors_close_new_cursors_and_rollback() {
    let mut config = test_config("cursor-retained-hold-commit-error");
    config.cursor_bytes = 32;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE hold_error(id integer); INSERT INTO hold_error VALUES (1),(0); \
        BEGIN; DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT 100/id FROM hold_error",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
        ["100"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "COMMIT", true)),
        ["22012"]
    );
    assert!(!session.transaction.is_active());
    assert!(!session.cursors.exists("held"));
    session.success(&mut engine, "TRUNCATE hold_error; INSERT INTO hold_error SELECT g FROM generate_series(1,8) AS g; \
        BEGIN; INSERT INTO hold_error VALUES (9); \
        DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM hold_error; FETCH NEXT FROM held", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "COMMIT", true)),
        ["54000"]
    );
    assert!(!session.transaction.is_active());
    assert!(!session.cursors.exists("held"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM hold_error", true)),
        ["8"]
    );
}

#[test]
fn cursor_retained_hold_runs_deferred_triggers_before_saving_unread_results() {
    let config = test_config("cursor-retained-hold-trigger-order");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE hold_trigger_input(id integer); INSERT INTO hold_trigger_input VALUES (0); \
        CREATE TABLE hold_trigger_gate(id integer); \
        CREATE FUNCTION hold_trigger_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'deferred gate'; END $$; \
        CREATE CONSTRAINT TRIGGER hold_gate AFTER INSERT ON hold_trigger_gate DEFERRABLE INITIALLY DEFERRED \
        FOR EACH ROW EXECUTE FUNCTION hold_trigger_failure(); \
        BEGIN; DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT 100/id FROM hold_trigger_input; \
        INSERT INTO hold_trigger_gate VALUES (1)", true);
    let failed = session.execute(&mut engine, "COMMIT", true);
    assert_eq!(error_sqlstates(&failed), ["P0001"]);
    assert!(String::from_utf8_lossy(&failed).contains("deferred gate"));
    assert!(!session.cursors.exists("held"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM hold_trigger_gate", true)),
        ["0"]
    );
}

#[test]
fn cursor_trigger_functions_validate_return_when_the_execution_path_finishes() {
    let config = test_config("cursor-trigger-return-paths");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE trigger_return_input(id integer); \
        CREATE FUNCTION trigger_conditional_return() RETURNS trigger LANGUAGE plpgsql AS $$ \
        BEGIN IF NEW.id>0 THEN RETURN NEW; END IF; END $$; \
        CREATE TRIGGER conditional_return BEFORE INSERT ON trigger_return_input \
        FOR EACH ROW EXECUTE FUNCTION trigger_conditional_return(); INSERT INTO trigger_return_input VALUES (1)", true);
    let row_error = session.execute(
        &mut engine,
        "INSERT INTO trigger_return_input VALUES (0)",
        true,
    );
    assert_eq!(error_sqlstates(&row_error), ["2F005"]);
    assert!(
        String::from_utf8_lossy(&row_error)
            .contains("control reached end of trigger procedure without RETURN")
    );
    session.success(&mut engine, "DROP TRIGGER conditional_return ON trigger_return_input; \
        CREATE FUNCTION trigger_missing_statement_return() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NULL; END $$; \
        CREATE TRIGGER missing_statement_return AFTER INSERT ON trigger_return_input \
        FOR EACH STATEMENT EXECUTE FUNCTION trigger_missing_statement_return()", true);
    let statement_error = session.execute(
        &mut engine,
        "INSERT INTO trigger_return_input VALUES (2)",
        true,
    );
    assert_eq!(error_sqlstates(&statement_error), ["2F005"]);
    assert!(
        String::from_utf8_lossy(&statement_error)
            .contains("control reached end of trigger procedure without RETURN")
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT id FROM trigger_return_input", true)),
        ["1"]
    );
}

#[test]
fn cursor_retained_scroll_reuses_history_and_extends_only_when_needed() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-scroll");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE scroll_input(id integer); INSERT INTO scroll_input SELECT g FROM generate_series(1,8) AS g; \
         BEGIN; DECLARE scrolling SCROLL CURSOR FOR SELECT id*10 FROM scroll_input WHERE id%2=0; \
         UPDATE scroll_input SET id=id+100", true);
    let (slot, _) = session
        .cursors
        .read_request("scrolling", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained scrolling plan");
    assert_eq!(session.cursors.produced_rows(slot), 0);
    assert_eq!(
        command_tags(&session.success(&mut engine, "FETCH PRIOR FROM scrolling", true)),
        ["FETCH 0"]
    );
    assert_eq!(session.cursors.produced_rows(slot), 0);
    for (sql, expected, produced) in [
        ("FETCH NEXT FROM scrolling", "20", 1),
        ("FETCH 0 FROM scrolling", "20", 1),
        ("FETCH NEXT FROM scrolling", "40", 2),
        ("FETCH PRIOR FROM scrolling", "20", 2),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [expected]
        );
        assert_eq!(session.cursors.produced_rows(slot), produced);
        assert_eq!(session.cursors.buffered_rows(slot), produced);
    }
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE FORWARD 2 FROM scrolling", true)),
        ["MOVE 2"]
    );
    assert_eq!(session.cursors.produced_rows(slot), 3);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH 0 FROM scrolling", true)),
        ["60"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH LAST FROM scrolling", true)),
        ["80"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 4);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH BACKWARD ALL FROM scrolling", true)),
        ["60", "40", "20"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH FIRST FROM scrolling", true)),
        ["20"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ABSOLUTE -2 FROM scrolling", true)),
        ["60"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH RELATIVE -1 FROM scrolling", true)),
        ["40"]
    );
    session.success(&mut engine, "CLOSE scrolling; ROLLBACK", true);
}

#[test]
fn cursor_retained_scroll_capacity_failure_releases_input_and_keeps_portal_failed() {
    let mut config = test_config("cursor-retained-scroll-capacity");
    config.cursor_bytes = 32;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE scroll_capacity(id integer); INSERT INTO scroll_capacity SELECT g FROM generate_series(1,8) AS g; \
         BEGIN; DECLARE scrolling SCROLL CURSOR FOR SELECT id FROM scroll_capacity; \
         FETCH NEXT FROM scrolling; SAVEPOINT full_history", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH ALL FROM scrolling", true)),
        ["54000"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT full_history; DROP TABLE scroll_capacity",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM scrolling", true)),
        ["55000"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn cursor_retained_limit_and_offset_keep_progress_and_stop_input_reads() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-row-bounds");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE bounded_input(id integer); INSERT INTO bounded_input SELECT g FROM generate_series(1,8) AS g; \
         BEGIN; DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM bounded_input LIMIT 1+1 OFFSET 2", true);
    assert!(
        session
            .cursors
            .read_request("bounded", FetchMotion::Count(1), false)
            .unwrap()
            .is_some()
    );
    for (id, scanned) in [(3, 3), (4, 4)] {
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM bounded", true)),
            [id.to_string()]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='bounded_input'", true)), [format!("1|{scanned}")]);
    }
    assert_eq!(
        command_tags(&session.success(&mut engine, "FETCH ALL FROM bounded", true)),
        ["FETCH 0"]
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='bounded_input'", true)), ["1|4"]);
    session.success(&mut engine, "CLOSE bounded; COMMIT; BEGIN; \
        DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM bounded_input LIMIT 0 OFFSET 5; FETCH ALL FROM bounded", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='bounded_input'", true)), ["0|0"]);
    session.success(&mut engine, "CLOSE bounded; COMMIT", true);
    for (clause, state) in [
        ("LIMIT -1", "2201W"),
        ("OFFSET -1", "2201X"),
        ("WHERE FALSE LIMIT -1", "2201W"),
    ] {
        session.success(
            &mut engine,
            &format!(
                "BEGIN; DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM bounded_input {clause}"
            ),
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM bounded", true)),
            [state]
        );
        session.success(&mut engine, "ROLLBACK", true);
    }
    session.success(&mut engine,
        "CREATE FUNCTION bounded_cursor_input() RETURNS integer LANGUAGE plpgsql AS $$ \
         DECLARE c refcursor; max_rows integer := 2; skipped_rows integer := 3; selected integer; BEGIN \
           OPEN c NO SCROLL FOR SELECT id FROM bounded_input LIMIT max_rows OFFSET skipped_rows; \
           max_rows := 0; skipped_rows := 0; FETCH NEXT FROM c INTO selected; CLOSE c; RETURN selected; END $$", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT bounded_cursor_input()", true)),
        ["4"]
    );
}

#[test]
fn cursor_retained_builtin_operators_keep_their_declared_identity() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-operator-binding");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SCHEMA cursor_ops; SET search_path=cursor_ops,pg_catalog; \
         CREATE TABLE operator_input(id integer); INSERT INTO operator_input VALUES (1),(2); \
         BEGIN; DECLARE bound_ops NO SCROLL CURSOR FOR SELECT id+10 FROM operator_input",
        true,
    );
    assert!(
        session
            .cursors
            .read_request("bound_ops", FetchMotion::Count(1), false)
            .unwrap()
            .is_some()
    );
    session.success(&mut engine,
        "CREATE FUNCTION cursor_ops.custom_add(integer,integer) RETURNS integer LANGUAGE SQL RETURN $1-$2; \
         CREATE OPERATOR cursor_ops.+ (FUNCTION=cursor_ops.custom_add,LEFTARG=integer,RIGHTARG=integer)", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM bound_ops", true)),
        ["11"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM bound_ops", true)),
        ["12"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT id+10 FROM operator_input", true)),
        ["-9", "-8"]
    );
    session.success(
        &mut engine,
        "CLOSE bound_ops; ROLLBACK; RESET search_path",
        true,
    );
}

#[test]
fn cursor_retained_errors_occur_when_the_requested_row_is_evaluated() {
    let config = test_config("cursor-retained-expression-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE expression_input(id integer); INSERT INTO expression_input VALUES (1),(2),(0); \
         BEGIN; DECLARE calculated NO SCROLL CURSOR FOR SELECT 100/id FROM expression_input", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["100"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["50"]
    );
    session.success(&mut engine, "SAVEPOINT before_error", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK TO SAVEPOINT before_error", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='expression_input'", true)), ["1|3"]);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["55000"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_error; CLOSE calculated; ROLLBACK",
        true,
    );
    for (select, state) in [
        ("SELECT 1/0 FROM expression_input", "22012"),
        ("SELECT id FROM expression_input WHERE id", "42804"),
    ] {
        session.success(&mut engine, "BEGIN", true);
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("DECLARE invalid NO SCROLL CURSOR FOR {select}"),
                true
            )),
            [state]
        );
        assert!(!session.cursors.exists("invalid"));
        session.success(&mut engine, "ROLLBACK", true);
    }
    session.success(&mut engine,
        "BEGIN; DECLARE empty_filter NO SCROLL CURSOR FOR SELECT id FROM expression_input WHERE FALSE; \
         DROP TABLE expression_input; FETCH ALL FROM empty_filter; COMMIT", true);
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
