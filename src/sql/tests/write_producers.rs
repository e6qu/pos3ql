//! A data-modifying `WITH` query runs as its rows are read: each read past
//! the rows it returned runs one more step of its write, so its row
//! triggers and calls interleave with its reader's, as in PostgreSQL.

use super::compatibility_diagnostics::with_engine;
use super::*;

const SETUP: &str = "CREATE TABLE wp_a (x int); \
    CREATE TABLE wp_log (n serial, msg text); \
    CREATE FUNCTION wp_note(t text, v int) RETURNS int LANGUAGE plpgsql VOLATILE AS $$ \
    BEGIN INSERT INTO wp_log (msg) VALUES (t || ' ' || v); RETURN v; END $$; \
    CREATE FUNCTION wp_row() RETURNS trigger LANGUAGE plpgsql AS $$ \
    BEGIN INSERT INTO wp_log (msg) VALUES (TG_WHEN || ' ' || TG_TABLE_NAME || ' ' || NEW.x); \
    RETURN NEW; END $$; \
    CREATE TRIGGER wp_a_before BEFORE INSERT ON wp_a FOR EACH ROW EXECUTE FUNCTION wp_row()";

fn log(engine: &mut Engine, budget: &mut Budget) -> String {
    let output = run_with(
        engine,
        budget,
        "SELECT string_agg(msg, ',' ORDER BY n) FROM wp_log; TRUNCATE wp_log, wp_a",
    );
    data_rows(&output).join("|")
}

#[test]
fn a_select_reads_a_data_modifying_with_query_as_it_runs() {
    with_engine("write-producer-select", |engine, budget| {
        run_with(engine, budget, SETUP);
        let output = run_with(
            engine,
            budget,
            "WITH w AS (INSERT INTO wp_a SELECT g FROM generate_series(1, 3) g RETURNING x) \
             SELECT wp_note('main', x) FROM w",
        );
        let text = String::from_utf8_lossy(&output).into_owned();
        assert!(
            !text.contains("ERROR"),
            "{text} {:?}",
            crate::sql::sequence::take_replay_error_diagnostic()
        );
        assert_eq!(
            log(engine, budget),
            "BEFORE wp_a 1,main 1,BEFORE wp_a 2,main 2,BEFORE wp_a 3,main 3"
        );
    });
}

#[test]
fn chained_queries_interleave_and_unread_queries_run_last_one_first() {
    with_engine("write-producer-chain", |engine, budget| {
        run_with(engine, budget, SETUP);
        run_with(
            engine,
            budget,
            "CREATE TABLE wp_b (x int); CREATE TABLE wp_c (x int); \
             CREATE TRIGGER wp_b_before BEFORE INSERT ON wp_b FOR EACH ROW EXECUTE FUNCTION wp_row(); \
             CREATE TRIGGER wp_c_before BEFORE INSERT ON wp_c FOR EACH ROW EXECUTE FUNCTION wp_row()",
        );
        // Results from PostgreSQL 18.6.
        let output = run_with(
            engine,
            budget,
            "WITH w1 AS (INSERT INTO wp_a SELECT g FROM generate_series(1, 2) g RETURNING x), \
                  w2 AS (INSERT INTO wp_b SELECT wp_note('b', x) FROM w1 RETURNING x), \
                  w3 AS (INSERT INTO wp_c VALUES (7) RETURNING x) \
             SELECT wp_note('main', x) FROM w2",
        );
        assert_eq!(data_rows(&output), ["1", "2"]);
        assert_eq!(
            log(engine, budget),
            "BEFORE wp_a 1,b 1,BEFORE wp_b 1,main 1,BEFORE wp_a 2,b 2,BEFORE wp_b 2,main 2,\
             BEFORE wp_c 7"
        );
        // A query read only partly still writes every row after the reads.
        let output = run_with(
            engine,
            budget,
            "WITH w AS (INSERT INTO wp_a SELECT g FROM generate_series(1, 3) g RETURNING x) \
             SELECT wp_note('main', x) FROM w LIMIT 1",
        );
        assert_eq!(data_rows(&output), ["1"]);
        assert_eq!(
            log(engine, budget),
            "BEFORE wp_a 1,main 1,BEFORE wp_a 2,BEFORE wp_a 3"
        );
        // A UNION ALL reads its queries one after another.
        let output = run_with(
            engine,
            budget,
            "WITH w1 AS (INSERT INTO wp_a VALUES (1), (2) RETURNING x), \
                  w2 AS (INSERT INTO wp_c VALUES (3) RETURNING x) \
             SELECT wp_note('a', x) FROM w1 UNION ALL SELECT wp_note('c', x) FROM w2",
        );
        assert_eq!(data_rows(&output), ["1", "2", "3"]);
        assert_eq!(
            log(engine, budget),
            "BEFORE wp_a 1,a 1,BEFORE wp_a 2,a 2,BEFORE wp_c 3,c 3"
        );
    });
}

#[test]
fn a_write_reading_a_query_takes_sequence_values_in_turn() {
    with_engine("write-producer-insert", |engine, budget| {
        run_with(engine, budget, SETUP);
        // Results from PostgreSQL 18.6: the query and the write reading it
        // draw from one sequence row by row.
        let output = run_with(
            engine,
            budget,
            "CREATE TABLE wp_pairs (x int, y int); CREATE SEQUENCE wp_values; \
             WITH w AS (INSERT INTO wp_a SELECT nextval('wp_values') \
                        FROM generate_series(1, 3) RETURNING x) \
             INSERT INTO wp_pairs SELECT x, nextval('wp_values') FROM w; \
             SELECT x, y FROM wp_pairs ORDER BY x",
        );
        let text = String::from_utf8_lossy(&output).into_owned();
        assert!(!text.contains("ERROR"), "{text}");
        assert_eq!(data_rows(&output), ["1|2", "3|4", "5|6"]);
        assert_eq!(
            log(engine, budget),
            "BEFORE wp_a 1,BEFORE wp_a 3,BEFORE wp_a 5"
        );
    });
}

#[test]
fn a_query_read_row_by_row_keeps_its_memory_fixed() {
    with_engine("write-producer-memory", |engine, budget| {
        run_with(engine, budget, SETUP);
        run_with(engine, budget, "DROP TRIGGER wp_a_before ON wp_a");
        // Each step continues the query's source where the last one stopped
        // and reclaims its own memory, so the statement arena does not grow
        // with the rows.
        let output = run_with_guc(
            engine,
            budget,
            "WITH w AS (INSERT INTO wp_a SELECT g FROM generate_series(1, 400) g RETURNING x) \
             SELECT wp_note('m', x) FROM w",
            2 << 20,
            &GucState::new(),
        );
        let text = String::from_utf8_lossy(&output).into_owned();
        assert!(!text.contains("ERROR"), "{text}");
        assert_eq!(data_rows(&output).len(), 400);
    });
}

#[test]
fn update_delete_and_merge_queries_return_each_row_before_the_next() {
    with_engine("write-producer-update-delete-merge", |engine, budget| {
        run_with(engine, budget, SETUP);
        run_with(
            engine,
            budget,
            "CREATE TRIGGER wp_a_before_change BEFORE UPDATE ON wp_a \
             FOR EACH ROW EXECUTE FUNCTION wp_row(); \
             CREATE TABLE wp_keys (x int); INSERT INTO wp_keys VALUES (2), (5); \
             INSERT INTO wp_a VALUES (1), (2), (3); TRUNCATE wp_log",
        );
        // Results from PostgreSQL 18.6.
        let output = run_with(
            engine,
            budget,
            "WITH w AS (UPDATE wp_a SET x = x + 10 RETURNING x) \
             SELECT wp_note('main', x) FROM w; \
             SELECT string_agg(msg, ',' ORDER BY n) FROM wp_log; TRUNCATE wp_log",
        );
        assert_eq!(
            data_rows(&output),
            [
                "11",
                "12",
                "13",
                "BEFORE wp_a 11,main 11,BEFORE wp_a 12,main 12,BEFORE wp_a 13,main 13"
            ]
        );
        // A reader without calls that reads one row: the unread query runs
        // before the rest of the one read partly.
        let output = run_with(
            engine,
            budget,
            "WITH w1 AS (DELETE FROM wp_a WHERE x > 11 RETURNING x), \
                  w2 AS (INSERT INTO wp_a VALUES (40) RETURNING x) \
             SELECT x FROM w1 LIMIT 1; \
             SELECT string_agg(msg, ',' ORDER BY n) FROM wp_log; TRUNCATE wp_log",
        );
        assert_eq!(data_rows(&output), ["12", "BEFORE wp_a 40"]);
        let output = run_with(
            engine,
            budget,
            "WITH w AS (MERGE INTO wp_a t USING wp_keys s ON t.x = s.x + 9 \
                        WHEN MATCHED THEN UPDATE SET x = t.x * 2 \
                        WHEN NOT MATCHED THEN INSERT VALUES (s.x) RETURNING t.x) \
             SELECT wp_note('main', x) FROM w; \
             SELECT string_agg(msg, ',' ORDER BY n) FROM wp_log; TRUNCATE wp_log",
        );
        assert_eq!(
            data_rows(&output),
            ["22", "5", "BEFORE wp_a 22,main 22,BEFORE wp_a 5,main 5"]
        );
    });
}
