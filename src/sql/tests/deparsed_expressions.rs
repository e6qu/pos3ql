use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output)
}

/// Stored defaults, checks and routine bodies are printed in PostgreSQL's
/// analyzed form, with resolved literal types and explicit coercions.
#[test]
fn catalog_expressions_print_postgresql_analyzed_forms() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE shown(a text DEFAULT 'x', v varchar(10) DEFAULT 'y' CHECK (v <> ''), \
         b bigint DEFAULT 0, j integer DEFAULT -3, d date DEFAULT current_date, \
         nl text DEFAULT NULL, c char(3) DEFAULT 'ab', n numeric CHECK (n > 1), \
         k integer CHECK (k BETWEEN 1 AND 5), m text CHECK (m IN ('a', 'b')), \
         s text DEFAULT upper('a') || lower('B'))",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT attname, pg_get_expr(adbin, adrelid) FROM pg_attrdef \
             JOIN pg_attribute ON attrelid = adrelid AND attnum = adnum \
             WHERE adrelid = 'shown'::regclass ORDER BY adnum"
        ),
        [
            "a|'x'::text",
            "v|'y'::character varying",
            "b|0",
            "j|'-3'::integer",
            "d|CURRENT_DATE",
            "c|'ab'::bpchar",
            "s|(upper('a'::text) || lower('B'::text))",
        ]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'shown'::regclass ORDER BY conname"
        ),
        [
            "CHECK (((k >= 1) AND (k <= 5)))",
            "CHECK ((m = ANY (ARRAY['a'::text, 'b'::text])))",
            "CHECK ((n > (1)::numeric))",
            "CHECK (((v)::text <> ''::text))",
        ]
    );
    rows(
        &mut engine,
        &mut budget,
        "CREATE FUNCTION shown_call(a text DEFAULT 'x', b integer DEFAULT 1) \
         RETURNS text LANGUAGE sql RETURN a || b",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_function_arguments('shown_call'::regproc), \
                    pg_get_expr(proargdefaults, 0) FROM pg_proc WHERE proname = 'shown_call'"
        ),
        ["a text DEFAULT 'x'::text, b integer DEFAULT 1|'x'::text, 1"]
    );
    assert!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_functiondef('shown_call'::regproc)"
        )[0]
        .contains("RETURN (a || b)")
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT format_type(1042, -1), format_type(1042, NULL)"
        ),
        ["bpchar|character"]
    );
}

#[test]
fn single_table_view_definitions_use_postgresql_layout() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE listed(id integer, a text, b integer); \
         CREATE VIEW listed_view AS SELECT id, a || 'z' AS az, count(*) AS c FROM listed \
         WHERE b > 0 GROUP BY id, a ORDER BY id DESC LIMIT 5",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_viewdef('listed_view'::regclass)"
        ),
        [
            " SELECT id,\n    (a || 'z'::text) AS az,\n    count(*) AS c\n   FROM listed\n  WHERE (b > 0)\n  GROUP BY id, a\n  ORDER BY id DESC\n LIMIT 5;"
        ]
    );
}

#[test]
fn pretty_definitions_drop_only_redundant_parentheses() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE tidy(b integer, n numeric, v varchar(5), q integer \
         CHECK ((q + 1) * 2 > 3 - (4 - b)), c integer CHECK (c::text <> '1' AND n > 1)); \
         CREATE TABLE tidy_notes(id integer, note text); \
         CREATE VIEW tidy_view AS SELECT t.b, j.note FROM tidy t \
         JOIN tidy_notes j ON j.id = t.b AND j.note IS NOT NULL WHERE t.b > 0 AND (t.n > 1 OR t.v <> 'x')",
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_constraintdef(oid, true) FROM pg_constraint \
             WHERE conrelid = 'tidy'::regclass ORDER BY conname"
        ),
        [
            "CHECK (((q + 1) * 2) > (3 - (4 - b)))",
            "CHECK (c::text <> '1'::text AND n > 1::numeric)",
        ]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_viewdef('tidy_view'::regclass, true)"
        ),
        [
            " SELECT t.b,\n    j.note\n   FROM tidy t\n     JOIN tidy_notes j ON j.id = t.b AND j.note IS NOT NULL\n  WHERE t.b > 0 AND (t.n > 1::numeric OR t.v::text <> 'x'::text);"
        ]
    );
}

/// View and domain defaults are assigned to the view column's and the domain
/// base type's type, not printed as conditions.
#[test]
fn view_and_domain_defaults_deparse_as_assignments() {
    let (mut engine, mut budget) = test_engine();
    for sql in [
        "CREATE TABLE base_rows(n bigint, t text)",
        "CREATE VIEW shown_rows AS SELECT n, t FROM base_rows",
        "ALTER VIEW shown_rows ALTER COLUMN n SET DEFAULT 4",
        "ALTER VIEW shown_rows ALTER COLUMN t SET DEFAULT 'z'",
        "CREATE DOMAIN small_count AS int DEFAULT 5",
    ] {
        rows(&mut engine, &mut budget, sql);
    }
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef \
             WHERE adrelid = 'shown_rows'::regclass ORDER BY adnum"
        ),
        ["4", "'z'::text"]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT pg_get_expr(typdefaultbin, 0) FROM pg_type WHERE typname = 'small_count'"
        ),
        ["5"]
    );
}
