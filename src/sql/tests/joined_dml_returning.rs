//! `RETURNING` of `UPDATE ... FROM` and `DELETE ... USING`, the scope of
//! comma-separated FROM items, and parenthesized joins, compared with
//! PostgreSQL 18.6.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

fn setup(engine: &mut Engine, budget: &mut Budget) {
    for sql in [
        "CREATE TABLE t (a int, b text)",
        "CREATE TABLE s (a int, c text, d numeric(5,2))",
        "INSERT INTO t VALUES (1, 'x'), (2, 'y')",
        "INSERT INTO s VALUES (1, 'p', 10), (2, 'q', 20)",
    ] {
        run_with(engine, budget, sql);
    }
}

fn sorted_rows(bytes: &[u8]) -> Vec<String> {
    let mut rows = data_rows(bytes);
    rows.sort_unstable();
    rows
}

#[test]
fn update_from_returning_includes_every_from_item() {
    with_engine("update-from-returning", |engine, budget| {
        setup(engine, budget);
        let output = run_with(
            engine,
            budget,
            "UPDATE t SET b = s.c FROM s WHERE t.a = s.a RETURNING *",
        );
        assert_eq!(row_description_names(&output), ["a", "b", "a", "c", "d"]);
        assert_eq!(
            row_description_type_modifiers(&output),
            [-1, -1, -1, -1, 327_686]
        );
        assert_eq!(sorted_rows(&output), ["1|p|1|p|10.00", "2|q|2|q|20.00"]);
        let output = run_with(
            engine,
            budget,
            "UPDATE t SET b = x.c FROM s x WHERE t.a = x.a \
             RETURNING x.*, x, old.b, new.b, x.d + 1, (SELECT x.c || t.b), (x).c",
        );
        assert_eq!(
            sorted_rows(&output),
            [
                "1|p|10.00|(1,p,10.00)|p|p|11.00|pp|p",
                "2|q|20.00|(2,q,20.00)|q|q|21.00|qq|q",
            ]
        );
        // A USING join inside the FROM list merges only its own item's columns.
        let output = run_with(
            engine,
            budget,
            "UPDATE t SET b = 'j' FROM s JOIN s s2 USING (a) WHERE t.a = s.a AND t.a = 2 \
             RETURNING *",
        );
        assert_eq!(
            row_description_names(&output),
            ["a", "b", "a", "c", "d", "c", "d"]
        );
        assert_eq!(data_rows(&output), ["2|j|2|q|20.00|q|20.00"]);
        let output = run_with(
            engine,
            budget,
            "WITH u AS (UPDATE t SET b = s.c FROM s WHERE t.a = s.a RETURNING s.*, t.b) \
             SELECT * FROM u ORDER BY a",
        );
        assert_eq!(data_rows(&output), ["1|p|10.00|p", "2|q|20.00|q"]);
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "UPDATE t SET b = c FROM s WHERE t.a = s.a RETURNING a",
            )),
            ["ERROR 42702 column reference \"a\" is ambiguous @53"]
        );
    });
}

#[test]
fn delete_using_returning_includes_every_from_item() {
    with_engine("delete-using-returning", |engine, budget| {
        setup(engine, budget);
        let output = run_with(
            engine,
            budget,
            "DELETE FROM t USING s WHERE t.a = s.a AND s.d = 10 RETURNING *, s.c",
        );
        assert_eq!(
            row_description_names(&output),
            ["a", "b", "a", "c", "d", "c"]
        );
        assert_eq!(data_rows(&output), ["1|x|1|p|10.00|p"]);
        // The USING items are read as of the statement start, so a self-join
        // still finds the rows this statement already deleted.
        let output = run_with(
            engine,
            budget,
            "DELETE FROM t USING t u WHERE t.a = u.a RETURNING *",
        );
        assert_eq!(data_rows(&output), ["2|y|2|y"]);
    });
}

#[test]
fn view_targets_return_from_items() {
    with_engine("view-from-returning", |engine, budget| {
        setup(engine, budget);
        for sql in [
            "CREATE VIEW v AS SELECT a, b FROM t",
            "CREATE TABLE tv (a int, b text)",
            "INSERT INTO tv VALUES (2, 'k')",
            "CREATE VIEW vt AS SELECT * FROM tv",
            "CREATE FUNCTION vt_trigger() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN IF TG_OP = 'DELETE' THEN RETURN OLD; END IF; RETURN NEW; END $$",
            "CREATE TRIGGER vt_instead INSTEAD OF UPDATE OR DELETE ON vt \
             FOR EACH ROW EXECUTE FUNCTION vt_trigger()",
        ] {
            run_with(engine, budget, sql);
        }
        let output = run_with(
            engine,
            budget,
            "UPDATE v SET b = c FROM s WHERE v.a = s.a RETURNING *",
        );
        assert_eq!(row_description_names(&output), ["a", "b", "a", "c", "d"]);
        assert_eq!(sorted_rows(&output), ["1|p|1|p|10.00", "2|q|2|q|20.00"]);
        let output = run_with(
            engine,
            budget,
            "UPDATE vt SET b = s.c FROM s WHERE vt.a = s.a RETURNING *",
        );
        assert_eq!(row_description_names(&output), ["a", "b", "a", "c", "d"]);
        assert_eq!(data_rows(&output), ["2|q|2|q|20.00"]);
        let output = run_with(
            engine,
            budget,
            "DELETE FROM vt USING s WHERE vt.a = s.a RETURNING *",
        );
        assert_eq!(data_rows(&output), ["2|k|2|q|20.00"]);
    });
}

#[test]
fn comma_separated_from_items_scope_their_joins() {
    with_engine("comma-from-items", |engine, budget| {
        setup(engine, budget);
        let output = run_with(
            engine,
            budget,
            "SELECT * FROM t, s JOIN s s2 USING (a) ORDER BY 1, 3",
        );
        assert_eq!(
            row_description_names(&output),
            ["a", "b", "a", "c", "d", "c", "d"]
        );
        assert_eq!(
            data_rows(&output),
            [
                "1|x|1|p|10.00|p|10.00",
                "1|x|2|q|20.00|q|20.00",
                "2|y|1|p|10.00|p|10.00",
                "2|y|2|q|20.00|q|20.00",
            ]
        );
        let output = run_with(
            engine,
            budget,
            "SELECT * FROM t, s NATURAL JOIN t t2 ORDER BY 1, 3",
        );
        assert_eq!(
            row_description_names(&output),
            ["a", "b", "a", "c", "d", "b"]
        );
        assert_eq!(
            data_rows(&output),
            [
                "1|x|1|p|10.00|x",
                "1|x|2|q|20.00|y",
                "2|y|1|p|10.00|x",
                "2|y|2|q|20.00|y",
            ]
        );
        // A RIGHT or FULL join after a comma joins only its own FROM item:
        // each unmatched row still pairs with every earlier item's row.
        let output = run_with(
            engine,
            budget,
            "SELECT t.a, s.a, t2.a FROM t, s RIGHT JOIN t t2 ON s.a = t2.a + 1 ORDER BY 1, 3",
        );
        assert_eq!(
            data_rows(&output),
            ["1|2|1", "1|NULL|2", "2|2|1", "2|NULL|2"]
        );
        let output = run_with(
            engine,
            budget,
            "SELECT count(*) FROM t, s FULL JOIN t t2 ON s.a = t2.a + 5",
        );
        assert_eq!(data_rows(&output), ["8"]);
        // A join condition cannot see an earlier comma-separated item.
        assert_eq!(
            error_sqlstates(&run_with(
                engine,
                budget,
                "SELECT * FROM t, s JOIN t t2 ON t.a = t2.a",
            )),
            ["42P01"]
        );
    });
}

#[test]
fn parenthesized_joins_keep_unaliased_members_addressable() {
    with_engine("parenthesized-joins", |engine, budget| {
        setup(engine, budget);
        // The right side of an outer join is the whole parenthesized join:
        // a row matching `s` but not `t2` drops out of it, so `t` stays
        // unmatched and every member is null-extended.
        let output = run_with(
            engine,
            budget,
            "SELECT t.a, s.c, t2.b, s IS NULL FROM t LEFT JOIN (s JOIN t t2 ON s.a = t2.a + 1) \
             ON t.a = s.a ORDER BY 1",
        );
        assert_eq!(row_description_names(&output), ["a", "c", "b", "?column?"]);
        assert_eq!(data_rows(&output), ["1|NULL|NULL|t", "2|q|x|f"]);
        let output = run_with(
            engine,
            budget,
            "SELECT * FROM (t JOIN s USING (a)) JOIN t t2 USING (a) ORDER BY 1",
        );
        assert_eq!(row_description_names(&output), ["a", "b", "c", "d", "b"]);
        assert_eq!(data_rows(&output), ["1|x|p|10.00|x", "2|y|q|20.00|y"]);
        // An alias hides the members; duplicate join columns stay ambiguous.
        let output = run_with(
            engine,
            budget,
            "SELECT j.* FROM (t JOIN s USING (a)) AS j(k) ORDER BY 1",
        );
        assert_eq!(row_description_names(&output), ["k", "b", "c", "d"]);
        for (sql, sqlstate) in [
            ("SELECT t.a FROM (t JOIN s USING (a)) AS j", "42P01"),
            ("SELECT j.a FROM (t JOIN s ON t.a = s.a) AS j", "42702"),
            (
                "SELECT * FROM t JOIN (s JOIN t t2 ON t2.a = t.a) ON true",
                "42P01",
            ),
            ("SELECT * FROM t JOIN (t JOIN s ON true) ON true", "42712"),
            ("SELECT * FROM (t)", "42601"),
        ] {
            assert_eq!(
                error_sqlstates(&run_with(engine, budget, sql)),
                [sqlstate],
                "{sql}"
            );
        }
        // Ordering by a star position names the member column it copies.
        let output = run_with(
            engine,
            budget,
            "SELECT * FROM t JOIN (s JOIN t t2 ON s.a = t2.a) ON t.a = s.a ORDER BY 5 DESC",
        );
        assert_eq!(
            data_rows(&output),
            ["2|y|2|q|20.00|2|y", "1|x|1|p|10.00|1|x"]
        );
    });
}

#[test]
fn joined_dml_hashes_its_from_item_as_postgresql() {
    with_engine("joined-dml-hash", |engine, budget| {
        for sql in [
            "CREATE TABLE h (id int, v int, note text)",
            "CREATE TABLE k (id bigint, w int, tag text)",
            "INSERT INTO h SELECT g, g % 7, 'n' || g FROM generate_series(1, 50) g",
            "INSERT INTO k SELECT g % 10, g, 't' || g FROM generate_series(1, 40) g",
            "INSERT INTO k VALUES (NULL, 1, 'null key')",
        ] {
            run_with(engine, budget, sql);
        }
        let explain = run_with(
            engine,
            budget,
            "EXPLAIN (COSTS OFF) UPDATE h SET note = k.tag FROM k \
             WHERE h.v = k.id AND h.id > k.w AND k.w > 20",
        );
        assert_eq!(
            data_rows(&explain),
            [
                "Update on h",
                "  ->  Hash Join",
                "        Hash Cond: (h.v = k.id)",
                "        Join Filter: (h.id > k.w)",
                "        ->  Seq Scan on h",
                "        ->  Hash",
                "              ->  Seq Scan on k",
                "                    Filter: (w > 20)",
            ]
        );
        // Each hash chain lists later rows first, as PostgreSQL's does, so a
        // target row joining several FROM rows keeps the last one scanned.
        let before = crate::sql::query::target_hash_builds();
        let output = run_with(
            engine,
            budget,
            "UPDATE h SET note = k.tag FROM k WHERE h.v = k.id AND k.w > 20 \
             AND h.id < 4 RETURNING h.id, h.note, k.w",
        );
        assert_eq!(sorted_rows(&output), ["1|t31|31", "2|t32|32", "3|t33|33"]);
        let output = run_with(
            engine,
            budget,
            "DELETE FROM h USING k WHERE h.v = k.id AND k.tag LIKE 't3%' \
             AND h.id IN (3, 10) RETURNING h.id, k.tag",
        );
        assert_eq!(sorted_rows(&output), ["10|t33", "3|t33"]);
        assert_eq!(crate::sql::query::target_hash_builds() - before, 2);
        // A non-default implicit collation beats the database default; two
        // different ones leave the comparison without a collation.
        for sql in [
            "CREATE TABLE cc (k text COLLATE \"C\")",
            "CREATE TABLE cp (k text COLLATE \"POSIX\")",
            "INSERT INTO cc VALUES ('t1')",
            "INSERT INTO cp VALUES ('t1')",
        ] {
            run_with(engine, budget, sql);
        }
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "UPDATE cc SET k = k.tag FROM k WHERE cc.k = k.tag",
            )),
            ["UPDATE 1"]
        );
        assert_eq!(
            diagnostics(&run_with(
                engine,
                budget,
                "UPDATE cc SET k = 'x' FROM cp WHERE cc.k = cp.k",
            )),
            [
                "ERROR 42P22 could not determine which collation to use for string comparison \
                 hint: Use the COLLATE clause to set the collation explicitly."
            ]
        );
    });
}

#[test]
fn explain_analyze_runs_a_rewritten_statement_once_and_plans_each_query() {
    with_engine("explain-analyze-rewritten", |engine, budget| {
        for sql in [
            "CREATE TABLE rw_source (a int, b text)",
            "CREATE TABLE rw_log (a int, b text)",
            "CREATE RULE rw_log_insert AS ON INSERT TO rw_source \
             DO ALSO INSERT INTO rw_log VALUES (new.a, new.b)",
        ] {
            run_with(engine, budget, sql);
        }
        let explain = run_with(
            engine,
            budget,
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF) \
             INSERT INTO rw_source VALUES (1, 'x')",
        );
        assert_eq!(
            data_rows(&explain),
            [
                "Insert on rw_source (actual rows=0.00 loops=1)",
                "  ->  Result (actual rows=1.00 loops=1)",
                "",
                "Insert on rw_log (actual rows=0.00 loops=1)",
                "  ->  Result (actual rows=1.00 loops=1)",
            ]
        );
        let rows = run_with(
            engine,
            budget,
            "SELECT (SELECT count(*) FROM rw_source), (SELECT count(*) FROM rw_log)",
        );
        assert_eq!(data_rows(&rows), ["1|1"]);
        let json = run_with(
            engine,
            budget,
            "EXPLAIN (ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF, FORMAT JSON) \
             INSERT INTO rw_source VALUES (2, 'y')",
        );
        let document = data_rows(&json).concat();
        assert_eq!(document.matches("\"Operation\": \"Insert\"").count(), 2);
        assert_eq!(document.matches("\"Triggers\": [").count(), 2);
    });
}

#[test]
fn explain_rewrites_each_rule_action_again() {
    with_engine("explain-rewritten-again", |engine, budget| {
        for sql in [
            "CREATE TABLE rr_t (a int, b int)",
            "CREATE TABLE rr_log (a int)",
            "CREATE VIEW rr_v AS SELECT a, b FROM rr_t",
            "CREATE RULE rr_v_insert AS ON INSERT TO rr_v DO INSTEAD \
             INSERT INTO rr_t VALUES (new.a, new.b)",
            "CREATE RULE rr_t_insert AS ON INSERT TO rr_t DO ALSO \
             INSERT INTO rr_log VALUES (new.a)",
        ] {
            run_with(engine, budget, sql);
        }
        // The view's INSTEAD action on rr_t runs rr_t's own rule.
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "EXPLAIN (COSTS OFF) INSERT INTO rr_v VALUES (1, 2)"
            )),
            [
                "Insert on rr_t",
                "  ->  Result",
                "",
                "Insert on rr_log",
                "  ->  Result"
            ]
        );
        // The action reads the aggregated query as the `*SELECT*` subquery.
        assert_eq!(
            data_rows(&run_with(
                engine,
                budget,
                "EXPLAIN (COSTS OFF) INSERT INTO rr_t SELECT max(a), 1 FROM rr_log"
            )),
            [
                "Insert on rr_t",
                "  ->  Aggregate",
                "        ->  Seq Scan on rr_log",
                "",
                "Insert on rr_log",
                "  ->  Subquery Scan on \"*SELECT*\"",
                "        ->  Aggregate",
                "              ->  Seq Scan on rr_log rr_log_1",
            ]
        );
        run_with(
            engine,
            budget,
            "CREATE RULE rr_log_insert AS ON INSERT TO rr_log DO ALSO \
             INSERT INTO rr_v VALUES (new.a, 0)",
        );
        // rr_log, rr_v, rr_t and rr_log again: found before anything runs.
        for sql in [
            "EXPLAIN (COSTS OFF) INSERT INTO rr_log VALUES (1)",
            "INSERT INTO rr_log VALUES (1)",
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                ["ERROR 42P17 infinite recursion detected in rules for relation \"rr_log\""],
                "{sql}"
            );
        }
        assert_eq!(
            data_rows(&run_with(engine, budget, "SELECT count(*) FROM rr_log")),
            ["0"]
        );
    });
}

#[test]
fn joined_dml_hashes_the_first_of_several_from_items() {
    with_engine("joined-dml-hash-several", |engine, budget| {
        for sql in [
            "CREATE TABLE hm (id int, v int, note text)",
            "CREATE TABLE km (id int, w int)",
            "CREATE TABLE um (k int, z text)",
            "INSERT INTO hm SELECT g, g, 'n' FROM generate_series(1, 40) g",
            "INSERT INTO km SELECT g, g + 100 FROM generate_series(1, 40) g",
            "INSERT INTO um SELECT g + 100, 'z' || g FROM generate_series(1, 40) g",
        ] {
            run_with(engine, budget, sql);
        }
        let before = crate::sql::query::target_hash_builds();
        let output = run_with(
            engine,
            budget,
            "UPDATE hm SET note = um.z FROM km JOIN um ON km.w = um.k \
             WHERE hm.v = km.id AND hm.id <= 3 RETURNING hm.id, hm.note",
        );
        assert_eq!(sorted_rows(&output), ["1|z1", "2|z2", "3|z3"]);
        assert_eq!(crate::sql::query::target_hash_builds() - before, 1);
        let explain = run_with(
            engine,
            budget,
            "EXPLAIN (COSTS OFF) DELETE FROM hm USING km JOIN um ON km.w = um.k \
             WHERE hm.v = km.id",
        );
        assert_eq!(
            data_rows(&explain),
            [
                "Delete on hm",
                "  ->  Nested Loop",
                "        Join Filter: (km.w = um.k)",
                "        ->  Hash Join",
                "              Hash Cond: (hm.v = km.id)",
                "              ->  Seq Scan on hm",
                "              ->  Hash",
                "                    ->  Seq Scan on km",
                "        ->  Materialize",
                "              ->  Seq Scan on um",
            ]
        );
    });
}

#[test]
fn update_from_returning_routine_call_resumes_with_the_joined_row() {
    with_engine("update-from-returning-call", |engine, budget| {
        setup(engine, budget);
        run_with(
            engine,
            budget,
            "CREATE FUNCTION note(v text) RETURNS text LANGUAGE plpgsql \
             AS $$BEGIN RETURN v || '!'; END$$",
        );
        // Each RETURNING call stops the write after its row changed; the
        // next attempt keeps the row and the FROM row it joined.
        let output = run_with(
            engine,
            budget,
            "UPDATE t SET b = note(s.c) FROM s WHERE t.a = s.a \
             RETURNING t.a, t.b, note(s.c || t.b), s.d",
        );
        assert_eq!(sorted_rows(&output), ["1|p!|pp!!|10.00", "2|q!|qq!!|20.00"]);
        let output = run_with(
            engine,
            budget,
            "DELETE FROM t USING s WHERE t.a = s.a RETURNING t.a, note(s.c), s.d",
        );
        assert_eq!(sorted_rows(&output), ["1|p!|10.00", "2|q!|20.00"]);
        assert!(data_rows(&run_with(engine, budget, "SELECT * FROM t")).is_empty());
    });
}
