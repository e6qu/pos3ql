//! Notices and warnings PostgreSQL 18.6 sends, in the order it sends them:
//! a PL/pgSQL `INTO` data change stops at its second row, a dropped column's
//! cascade is one report, analysis passes do not repeat input notices, the
//! planner folds a constant once (also for EXPLAIN), a folding error follows
//! the warnings folded before it, and an index answers in its rows' order.

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
        run_with(
            engine,
            budget,
            "CREATE FUNCTION shown(i int) RETURNS int LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'row %', i; RETURN i; END $$",
        );
        // Each case writes a fresh table whose row 1 was updated, so its new
        // version follows rows 2 and 3. A sequential scan reads that order;
        // a condition on an index's leading column reaches the rows through
        // their index entries, where an updated row keeps its first position.
        for (case, (key, sql, expected)) in [
            ("", "UPDATE {t} SET a = shown(id)", &["2", "3", "1"][..]),
            ("", "DELETE FROM {t} WHERE shown(id) > 0", &["2", "3", "1"]),
            (
                " PRIMARY KEY",
                "UPDATE {t} SET a = shown(id) WHERE id < 3",
                &["1", "2"],
            ),
            (
                " PRIMARY KEY",
                "UPDATE {t} SET a = shown(id) WHERE id IN (1, 2)",
                &["1", "2"],
            ),
            (
                " PRIMARY KEY",
                "DELETE FROM {t} WHERE (id = 1 OR id = 2) AND shown(id) > 0",
                &["1", "2"],
            ),
            (
                " PRIMARY KEY",
                "UPDATE {t} SET a = shown(id)",
                &["2", "3", "1"],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let table = format!("ordered_{case}");
            for setup in [
                format!("CREATE TABLE {table} (id int{key}, a int)"),
                format!("INSERT INTO {table} VALUES (1, 0), (2, 0), (3, 0)"),
                format!("UPDATE {table} SET a = 1 WHERE id = 1"),
            ] {
                run_with(engine, budget, &setup);
            }
            let sql = sql.replace("{t}", &table);
            let output = messages(engine, budget, &sql);
            let expected: Vec<String> = expected
                .iter()
                .map(|id| format!("NOTICE 00000 row {id}"))
                .collect();
            assert_eq!(output[..output.len() - 1], expected[..], "{sql}");
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

#[test]
fn grants_without_grant_options_warn_like_postgresql() {
    with_engine("notice-grant-options", |engine, budget| {
        for sql in [
            "CREATE ROLE rv1",
            "CREATE ROLE rv2",
            "CREATE TABLE rvt (id int)",
            "GRANT SELECT ON rvt TO rv1, rv2",
            "CREATE ROLE rv_none",
        ] {
            run_with(engine, budget, sql);
        }
        let revoked =
            |target: &str| format!("WARNING 01006 no privileges could be revoked for {target}");
        let mut expected = vec![revoked("\"rvt\"")];
        for column in ["tableoid", "cmax", "xmax", "cmin", "xmin", "ctid", "id"] {
            expected.push(revoked(&format!("column \"{column}\" of relation \"rvt\"")));
        }
        expected.push("REVOKE".to_owned());
        assert_eq!(
            messages(
                engine,
                budget,
                "SET ROLE rv1; REVOKE SELECT ON rvt FROM rv2; RESET ROLE",
            )[1..expected.len() + 1],
            expected[..]
        );
        assert_eq!(
            messages(
                engine,
                budget,
                "SET ROLE rv1; GRANT UPDATE ON rvt TO rv2; RESET ROLE"
            )[1..3],
            [
                "WARNING 01007 no privileges were granted for \"rvt\"",
                "GRANT"
            ]
        );
        // A role with no privilege at all on the object is refused.
        assert_eq!(
            messages(
                engine,
                budget,
                "SET ROLE rv_none; GRANT SELECT ON rvt TO rv2; RESET ROLE"
            )[1],
            "ERROR 42501 permission denied for table rvt"
        );
        run_with(engine, budget, "RESET ROLE");
    });
}

#[test]
fn catalog_and_role_notices_match_postgresql() {
    with_engine("notice-catalog-roles", |engine, budget| {
        assert_eq!(
            messages(engine, budget, "CREATE ROLE empty_password PASSWORD ''"),
            [
                "NOTICE 00000 empty string is not a valid password, clearing password",
                "CREATE ROLE"
            ]
        );
        run_with(
            engine,
            budget,
            "CREATE ROLE md5_role PASSWORD 'md547fcb6615d41c53cd39822141eb05da2'",
        );
        assert_eq!(
            messages(engine, budget, "ALTER ROLE md5_role RENAME TO md5_renamed"),
            [
                "NOTICE 00000 MD5 password cleared because of role rename",
                "ALTER ROLE"
            ]
        );
        for sql in [
            "CREATE ACCESS METHOD dam TYPE TABLE HANDLER heap_tableam_handler",
            "CREATE TABLE damt (id integer) USING dam",
            "CREATE TABLE ppd (id integer PRIMARY KEY, discarded text, retained text)",
            "CREATE PUBLICATION ppd_changes FOR TABLE ppd (id, retained)",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            messages(engine, budget, "DROP ACCESS METHOD dam CASCADE"),
            [
                "NOTICE 00000 drop cascades to table damt",
                "DROP ACCESS METHOD"
            ]
        );
        assert_eq!(
            messages(
                engine,
                budget,
                "ALTER TABLE ppd DROP COLUMN retained CASCADE"
            ),
            [
                "NOTICE 00000 drop cascades to publication of table ppd in publication ppd_changes",
                "ALTER TABLE"
            ]
        );
        assert_eq!(
            messages(
                engine,
                budget,
                "ALTER TABLE ppd ADD COLUMN IF NOT EXISTS id integer",
            ),
            [
                "NOTICE 42701 column \"id\" of relation \"ppd\" already exists, skipping",
                "ALTER TABLE"
            ]
        );
    });
}

/// The notices and errors `sql` sends, without its command tags.
fn notices(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    messages(engine, budget, sql)
        .into_iter()
        .filter(|message| message.starts_with("NOTICE") || message.starts_with("ERROR"))
        .collect()
}

#[test]
fn planner_folds_each_constant_once() {
    with_engine("notice-constant-folding", |engine, budget| {
        for sql in [
            "CREATE TABLE folded (a int)",
            "INSERT INTO folded VALUES (1), (2), (3)",
            "CREATE FUNCTION note(int) RETURNS int IMMUTABLE LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'note %', $1; RETURN $1; END $$",
            "CREATE FUNCTION steady(int) RETURNS int STABLE LANGUAGE plpgsql AS \
             $$ BEGIN RAISE NOTICE 'steady %', $1; RETURN $1; END $$",
        ] {
            run_with(engine, budget, sql);
        }
        let empty = "NOTICE 00000 text-search query doesn't contain lexemes: \"\"";
        assert_eq!(
            notices(engine, budget, "SELECT a, ''::tsquery FROM folded"),
            [empty]
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "SELECT a FROM folded WHERE false AND a > note(0)"
            ),
            Vec::<String>::new()
        );
        // An arm no row reaches still folds; only an immutable call folds.
        assert_eq!(
            notices(
                engine,
                budget,
                "SELECT CASE WHEN a > 9 THEN note(1) END, steady(2) FROM folded",
            ),
            [
                "NOTICE 00000 note 1",
                "NOTICE 00000 steady 2",
                "NOTICE 00000 steady 2",
                "NOTICE 00000 steady 2",
            ]
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "UPDATE folded SET a = a + note(0) WHERE a > note(10)",
            ),
            ["NOTICE 00000 note 0", "NOTICE 00000 note 10"]
        );
        // A constant operand settles which arms a CASE keeps.
        assert!(
            notices(
                engine,
                budget,
                "SELECT CASE 1 WHEN 2 THEN 1 / 0 ELSE a END FROM folded",
            )
            .is_empty()
        );
        assert_eq!(
            notices(
                engine,
                budget,
                "SELECT CASE WHEN a > 9 THEN 1 / 0 END FROM folded"
            ),
            ["ERROR 22012 division by zero"]
        );
    });
}

#[test]
fn index_answered_queries_return_rows_at_their_first_position() {
    with_engine("index-answered-row-order", |engine, budget| {
        for sql in [
            "CREATE TABLE keyed (id int PRIMARY KEY, v text)",
            "INSERT INTO keyed VALUES (1, 'a'), (2, 'b'), (3, 'c')",
            "UPDATE keyed SET v = 'x' WHERE v = 'a'",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SELECT id FROM keyed WHERE id > 0"
            )),
            ["1", "2", "3"]
        );
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "SELECT id FROM keyed WHERE v <> ''"
            )),
            ["2", "3", "1"]
        );
    });
}

/// PostgreSQL's planner folds a statement's constants in order: a constant
/// that fails is the statement's error, sent after the warnings the
/// constants folded before it raised. EXPLAIN plans the folded statement,
/// so plain EXPLAIN raises those warnings and EXPLAIN ANALYZE raises them
/// once.
#[test]
fn folding_errors_follow_earlier_warnings_and_explain_folds() {
    with_engine("notice-folding-order", |engine, budget| {
        run_with(engine, budget, "CREATE TABLE folded (a int)");
        run_with(engine, budget, "INSERT INTO folded VALUES (1)");
        let reduced = "WARNING 22023 TIMESTAMP(7) precision reduced to maximum allowed, 6";
        let division = "ERROR 22012 division by zero";
        for sql in [
            "SELECT timestamptypmodin('{7}'), 1/0",
            "SELECT timestamptypmodin('{7}'), 1/0 FROM folded",
            "UPDATE folded SET a = timestamptypmodin('{7}') + 1/0",
        ] {
            assert_eq!(messages(engine, budget, sql), [reduced, division], "{sql}");
        }
        assert_eq!(
            messages(engine, budget, "SELECT 1/0, timestamptypmodin('{7}')"),
            [division]
        );
        assert_eq!(
            messages(
                engine,
                budget,
                "EXPLAIN (COSTS OFF) SELECT a FROM folded WHERE a > timestamptypmodin('{7}')"
            ),
            [reduced, "EXPLAIN"]
        );
        assert_eq!(
            messages(
                engine,
                budget,
                "EXPLAIN (ANALYZE, COSTS OFF) SELECT a FROM folded WHERE a > timestamptypmodin('{7}')"
            ),
            [reduced, "EXPLAIN"]
        );
    });
}
