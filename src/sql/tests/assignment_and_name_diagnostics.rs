//! Multiple-column assignment arity, `*` positions, database-qualified names
//! (including DROP lists), whole-row stars inside expressions and references
//! to an aliased table's name, compared with PostgreSQL 18.6.

use super::compatibility_diagnostics::{diagnostics, with_engine};
use super::*;

#[test]
fn assignment_sources_and_names_report_postgresql_errors() {
    with_engine("assignment-name-diagnostics", |engine, budget| {
        for sql in [
            "CREATE TABLE u (a int, b text, c date)",
            "CREATE TABLE s (x text, y int, z int)",
            "INSERT INTO u VALUES (3, 'q', '2024-01-01')",
        ] {
            run_with(engine, budget, sql);
        }
        for (sql, expected) in [
            (
                "UPDATE u SET (a, b) = (SELECT * FROM u)",
                "ERROR 42601 number of columns does not match number of values @23",
            ),
            (
                "UPDATE u SET b = 'x', (a, c) = (SELECT 1)",
                "ERROR 42601 number of columns does not match number of values @32",
            ),
            (
                "INSERT INTO u (c)   SELECT  * FROM (SELECT 5) q",
                "ERROR 42804 column \"c\" is of type date but expression is of type integer @29 \
                 hint: You will need to rewrite or cast the expression.",
            ),
            (
                "UPDATE u SET b = 'z' RETURNING missing.*",
                "ERROR 42P01 missing FROM-clause entry for table \"missing\" @32",
            ),
            (
                "SELECT u.a FROM u x",
                "ERROR 42P01 invalid reference to FROM-clause entry for table \"u\" @8 \
                 hint: Perhaps you meant to reference the table alias \"x\".",
            ),
            (
                "SELECT a.b.c.d FROM u",
                "ERROR 0A000 cross-database references are not implemented: a.b.c.d @8",
            ),
            (
                "SELECT u.a.b.c.d FROM u",
                "ERROR 42601 improper qualified name (too many dotted names): u.a.b.c.d @8",
            ),
            (
                "SELECT * FROM foo.public.u",
                "ERROR 0A000 cross-database references are not implemented: \"foo.public.u\" @15",
            ),
            (
                "SELECT * FROM a.b.c.d",
                "ERROR 42601 improper qualified name (too many dotted names): a.b.c.d @15",
            ),
            // DROP lists resolve names when the command runs: no position.
            (
                "DROP TABLE foo.public.u",
                "ERROR 0A000 cross-database references are not implemented: \"foo.public.u\"",
            ),
            (
                "DROP TABLE a.b.c.d",
                "ERROR 42601 improper relation name (too many dotted names): a.b.c.d",
            ),
            (
                "DROP TYPE a.b.c.d",
                "ERROR 42601 improper qualified name (too many dotted names): a.b.c.d",
            ),
        ] {
            assert_eq!(
                diagnostics(&run_with(engine, budget, sql)),
                [expected],
                "{sql}"
            );
        }
        let output = run_with(
            engine,
            budget,
            "SELECT postgres.public.u.a, postgres.public.u.* FROM postgres.public.u",
        );
        assert_eq!(data_rows(&output), ["3|3|q|2024-01-01"]);
        // `u.*` followed by an operator is one whole-row expression; with a
        // label it still expands.
        let output = run_with(engine, budget, "SELECT u.* IS NOT NULL, u.* AS x FROM u");
        assert_eq!(data_rows(&output), ["t|3|q|2024-01-01"]);
        run_with(engine, budget, "CREATE TABLE dropped (a int)");
        run_with(engine, budget, "DROP TABLE postgres.public.dropped");
        assert_eq!(
            diagnostics(&run_with(engine, budget, "SELECT * FROM dropped")),
            ["ERROR 42P01 relation \"dropped\" does not exist @15"]
        );
    });
}
