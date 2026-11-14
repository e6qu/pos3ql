//! Notices and warnings PostgreSQL 18.6 sends, in the order it sends them:
//! a PL/pgSQL `INTO` data change stops at its second row, a dropped column's
//! cascade is one report, and analysis passes do not repeat input notices.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

fn messages(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    diagnostics(&run_with(engine, budget, sql))
}

#[test]
fn into_data_changes_stop_after_their_second_returned_row() {
    with_engine("notice-into-second-row", |engine, budget| {
        for sql in [
            "CREATE TABLE many (id int, a int)",
            "INSERT INTO many VALUES (1, 0), (2, 0), (3, 0), (4, 0)",
            "CREATE FUNCTION seen(label text) RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'seen %', label; RETURN 1; END $$",
            "CREATE FUNCTION many_row() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'row %', coalesce(NEW.id, OLD.id); RETURN NULL; END $$",
            "CREATE FUNCTION many_statement() RETURNS trigger LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'statement %', TG_OP; RETURN NULL; END $$",
            "CREATE TRIGGER many_rows AFTER INSERT OR UPDATE OR DELETE ON many \
             FOR EACH ROW EXECUTE FUNCTION many_row()",
            "CREATE TRIGGER many_statements AFTER INSERT OR UPDATE OR DELETE ON many \
             FOR EACH STATEMENT EXECUTE FUNCTION many_statement()",
        ] {
            run_with(engine, budget, sql);
        }
        let too_many = "ERROR P0003 query returned more than one row \
                        hint: Make sure the query returns a single row, or use LIMIT 1.";
        // The AFTER ROW triggers of the two rows fire; the write never
        // reaches the end of its rows, so its AFTER STATEMENT trigger does not.
        assert_eq!(
            messages(
                engine,
                budget,
                "DO $$ DECLARE x int; BEGIN \
                 UPDATE many SET a = seen('update ' || id) RETURNING id INTO x; END $$",
            ),
            [
                "NOTICE 00000 seen update 1",
                "NOTICE 00000 seen update 2",
                "NOTICE 00000 row 1",
                "NOTICE 00000 row 2",
                too_many,
            ]
        );
        // An INSERT stops reading its source.
        assert_eq!(
            messages(
                engine,
                budget,
                "DO $$ DECLARE x int; BEGIN INSERT INTO many \
                 SELECT g, seen('insert ' || g) FROM generate_series(5, 8) g \
                 RETURNING id INTO STRICT x; END $$",
            ),
            [
                "NOTICE 00000 seen insert 5",
                "NOTICE 00000 seen insert 6",
                "NOTICE 00000 row 5",
                "NOTICE 00000 row 6",
                too_many,
            ]
        );
        // Exactly one returned row completes the statement normally.
        assert_eq!(
            messages(
                engine,
                budget,
                "DO $$ DECLARE x int; BEGIN \
                 DELETE FROM many WHERE id = 4 RETURNING seen('delete ' || id) INTO x; \
                 RAISE NOTICE 'deleted %', x; END $$",
            ),
            [
                "NOTICE 00000 seen delete 4",
                "NOTICE 00000 row 4",
                "NOTICE 00000 statement DELETE",
                "NOTICE 00000 deleted 1",
                "DO",
            ]
        );
    });
}

#[test]
fn into_keeps_its_row_across_a_later_rows_routine_call() {
    with_engine("notice-into-kept-row", |engine, budget| {
        for sql in [
            "CREATE TABLE merged (id int, a int)",
            "INSERT INTO merged VALUES (1, 0), (2, 0)",
            "CREATE FUNCTION seen(i int) RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'seen %', i; RETURN i; END $$",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            messages(
                engine,
                budget,
                "DO $$ DECLARE x int; BEGIN \
                 MERGE INTO merged t USING generate_series(1, 2) s(id) ON t.id = s.id \
                 WHEN MATCHED AND seen(s.id) > 0 AND s.id = 1 THEN UPDATE SET a = 1 \
                 RETURNING t.id INTO x; RAISE NOTICE 'x % found %', x, found; END $$",
            ),
            [
                "NOTICE 00000 seen 1",
                "NOTICE 00000 seen 2",
                "NOTICE 00000 x 1 found t",
                "DO"
            ]
        );
    });
}

#[test]
fn data_changes_select_their_target_rows_in_physical_order() {
    with_engine("notice-physical-target-order", |engine, budget| {
        for sql in [
            "CREATE TABLE ordered (id int, a int)",
            "INSERT INTO ordered VALUES (1, 0), (2, 0), (3, 0)",
            // The new version of row 1 follows rows 2 and 3.
            "UPDATE ordered SET a = 1 WHERE id = 1",
            "CREATE FUNCTION shown(i int) RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'row %', i; RETURN i; END $$",
        ] {
            run_with(engine, budget, sql);
        }
        for sql in [
            "UPDATE ordered SET a = shown(id)",
            "DELETE FROM ordered WHERE shown(id) > 0",
        ] {
            run_with(engine, budget, "BEGIN");
            let output = messages(engine, budget, sql);
            assert_eq!(
                output[..3],
                [
                    "NOTICE 00000 row 2",
                    "NOTICE 00000 row 3",
                    "NOTICE 00000 row 1"
                ],
                "{sql}"
            );
            run_with(engine, budget, "ROLLBACK");
        }
        // A condition on an index's leading column reaches the rows through
        // their index entries, where an updated row keeps its first position.
        for sql in [
            "CREATE TABLE keyed (id int PRIMARY KEY, a int)",
            "INSERT INTO keyed VALUES (1, 0), (2, 0), (3, 0)",
            "UPDATE keyed SET a = 1 WHERE id = 1",
        ] {
            run_with(engine, budget, sql);
        }
        for (sql, expected) in [
            (
                "UPDATE keyed SET a = shown(id) WHERE id < 3",
                &["1", "2"][..],
            ),
            (
                "UPDATE keyed SET a = shown(id) WHERE id IN (1, 2)",
                &["1", "2"],
            ),
            (
                "DELETE FROM keyed WHERE (id = 1 OR id = 2) AND shown(id) > 0",
                &["1", "2"],
            ),
            ("UPDATE keyed SET a = shown(id)", &["2", "3", "1"]),
        ] {
            run_with(engine, budget, "BEGIN");
            let output = messages(engine, budget, sql);
            let expected: Vec<String> = expected
                .iter()
                .map(|id| format!("NOTICE 00000 row {id}"))
                .collect();
            assert_eq!(output[..expected.len()], expected[..], "{sql}");
            run_with(engine, budget, "ROLLBACK");
        }
    });
}

#[test]
fn ddl_notices_match_postgresql() {
    with_engine("notice-ddl", |engine, budget| {
        for sql in [
            "CREATE TABLE adopt (id int NOT NULL)",
            "CREATE UNIQUE INDEX adopt_index ON adopt (id)",
            "CREATE TABLE b (k int, x int)",
            "CREATE VIEW v AS SELECT x FROM b",
            "CREATE TABLE t (c v)",
            "CREATE TYPE ct AS (a int, f v)",
            "CREATE DOMAIN dv AS v",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            messages(
                engine,
                budget,
                "ALTER TABLE adopt ADD CONSTRAINT adopt_key UNIQUE USING INDEX adopt_index",
            ),
            [
                "NOTICE 00000 ALTER TABLE / ADD CONSTRAINT USING INDEX will rename index \
                 \"adopt_index\" to \"adopt_key\"",
                "ALTER TABLE",
            ]
        );
        // Every object the column's drop reaches is one report.
        assert_eq!(
            messages(engine, budget, "ALTER TABLE b DROP COLUMN x CASCADE"),
            [
                "NOTICE 00000 drop cascades to 4 other objects detail: \
                 drop cascades to view v\n\
                 drop cascades to column c of table t\n\
                 drop cascades to column f of composite type ct\n\
                 drop cascades to type dv",
                "ALTER TABLE",
            ]
        );
    });
}

#[test]
fn trigger_rows_and_record_variables_match_postgresql() {
    with_engine("notice-trigger-records", |engine, budget| {
        for sql in [
            "CREATE DOMAIN label AS text CHECK (VALUE <> 'bad')",
            "CREATE TABLE g (a int, name label, \
             d int GENERATED ALWAYS AS (a * 3) STORED, v int GENERATED ALWAYS AS (a * 5))",
            "CREATE FUNCTION shown() RETURNS trigger LANGUAGE plpgsql AS $$ \
             DECLARE r record; BEGIN \
             r := coalesce(NEW, OLD); \
             RAISE NOTICE '% % % %', TG_WHEN, NEW, OLD, pg_typeof(r.name); \
             RETURN NEW; END $$",
            "CREATE TRIGGER g_before BEFORE INSERT OR UPDATE ON g \
             FOR EACH ROW EXECUTE FUNCTION shown()",
            "CREATE TRIGGER g_after AFTER INSERT OR UPDATE ON g \
             FOR EACH ROW EXECUTE FUNCTION shown()",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            messages(engine, budget, "INSERT INTO g (a, name) VALUES (1, 'x')"),
            [
                "NOTICE 00000 BEFORE (1,x,,) <NULL> label",
                "NOTICE 00000 AFTER (1,x,3,) <NULL> label",
                "INSERT 0 1",
            ]
        );
        // A BEFORE trigger's NEW holds no generated value, stored ones
        // included; the write computes them afterwards.
        assert_eq!(
            messages(engine, budget, "UPDATE g SET name = 'y'"),
            [
                "NOTICE 00000 BEFORE (1,y,,) (1,x,3,) label",
                "NOTICE 00000 AFTER (1,y,3,) (1,x,3,) label",
                "UPDATE 1",
            ]
        );
    });
}

#[test]
fn analysis_does_not_repeat_literal_input_notices() {
    with_engine("notice-literal-input", |engine, budget| {
        let empty = "NOTICE 00000 text-search query doesn't contain lexemes: \"\"";
        for sql in [
            "SELECT ''::tsquery",
            "SELECT numnode(''::tsquery)",
            "SELECT ts_rewrite('a | (b & c)'::tsquery, 'b & c'::tsquery, ''::tsquery)",
        ] {
            let output = messages(engine, budget, sql);
            assert_eq!(output, [empty, "SELECT 1"], "{sql}");
        }
    });
}
