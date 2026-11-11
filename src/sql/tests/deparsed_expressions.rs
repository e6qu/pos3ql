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
