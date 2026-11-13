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
