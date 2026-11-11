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
