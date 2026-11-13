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

#[test]
fn information_schema_domains_resolve_check_and_describe_like_postgresql() {
    let (mut engine, mut budget) = test_engine();
    for (sql, expected) in [
        (
            "SELECT 5::information_schema.cardinal_number + 1, \
                    pg_typeof(5::information_schema.cardinal_number)",
            vec!["6|information_schema.cardinal_number"],
        ),
        (
            "SELECT 'YESX'::varchar(3)::information_schema.yes_or_no, \
                    pg_typeof('NO'::information_schema.yes_or_no)",
            vec!["YES|information_schema.yes_or_no"],
        ),
        (
            "SELECT 13307::regtype, format_type(13318, NULL), \
                    'information_schema.sql_identifier'::regtype::oid",
            vec!["information_schema.cardinal_number|information_schema.time_stamp|13312"],
        ),
        (
            "SELECT pg_typeof(table_name), pg_typeof(ordinal_position), pg_typeof(is_nullable) \
             FROM information_schema.columns LIMIT 1",
            vec![
                "information_schema.sql_identifier|information_schema.cardinal_number|\
                 information_schema.yes_or_no",
            ],
        ),
        (
            "SELECT typname, typtype, typbasetype, typtypmod, typarray FROM pg_type \
             WHERE oid IN (13307, 13320, 13319) ORDER BY oid",
            vec![
                "cardinal_number|d|23|-1|13306",
                "_yes_or_no|b|0|-1|0",
                "yes_or_no|d|1043|7|13319",
            ],
        ),
        (
            "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE oid IN (13308, 13321) ORDER BY oid",
            vec![
                "cardinal_number_domain_check|CHECK ((VALUE >= 0))",
                "yes_or_no_check|CHECK (((VALUE)::text = ANY ((ARRAY['YES'::character varying, \
                 'NO'::character varying])::text[])))",
            ],
        ),
        (
            "SELECT domain_name, data_type, udt_name FROM information_schema.domains \
             WHERE domain_schema = 'information_schema' ORDER BY domain_name",
            vec![
                "cardinal_number|integer|int4",
                "character_data|character varying|varchar",
                "sql_identifier|name|name",
                "time_stamp|timestamp with time zone|timestamptz",
                "yes_or_no|character varying|varchar",
            ],
        ),
    ] {
        assert_eq!(rows(&mut engine, &mut budget, sql), expected, "{sql}");
    }
    for (sql, state) in [
        ("SELECT (-1)::information_schema.cardinal_number", "23514"),
        (
            "SELECT 'MAYBE'::text::information_schema.yes_or_no",
            "23514",
        ),
        ("SELECT 1::cardinal_number", "42704"),
        ("SELECT 1::information_schema.int4", "42704"),
        (
            "SELECT '{-1}'::information_schema.cardinal_number[]",
            "23514",
        ),
        ("SELECT '{maybe}'::information_schema.yes_or_no[]", "22001"),
        (
            "SELECT ARRAY['maybe']::information_schema.yes_or_no[]",
            "23514",
        ),
        (
            "CREATE TABLE isd_array_rejects(a information_schema.cardinal_number[]); \
             INSERT INTO isd_array_rejects VALUES ('{-3}')",
            "23514",
        ),
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert_eq!(error_sqlstates(&output), [state], "{sql}");
    }
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT '{1,2}'::information_schema.cardinal_number[], \
                    pg_typeof('{a}'::information_schema.sql_identifier[]), \
                    pg_typeof(('{1}'::information_schema.cardinal_number[])[1])"
        ),
        ["{1,2}|information_schema.sql_identifier[]|information_schema.cardinal_number"]
    );
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SET search_path = information_schema, public; \
             SELECT 1::cardinal_number, 13307::regtype"
        ),
        ["1|cardinal_number"]
    );
}

#[test]
fn columns_of_information_schema_domains_enforce_checks_through_recovery() {
    let config = test_config("information-schema-domain-columns");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE answers(n information_schema.cardinal_number, \
         y information_schema.yes_or_no); INSERT INTO answers VALUES (1, 'YES')",
    );
    drop(engine);
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT n, y, pg_typeof(n), pg_typeof(y) FROM answers"
        ),
        ["1|YES|information_schema.cardinal_number|information_schema.yes_or_no"]
    );
    for sql in [
        "INSERT INTO answers VALUES (-1, 'NO')",
        "UPDATE answers SET y = 'NAY'",
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert_eq!(error_sqlstates(&output), ["23514"], "{sql}");
    }
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT column_name, domain_name, udt_name FROM information_schema.columns \
             WHERE table_name = 'answers' ORDER BY ordinal_position"
        ),
        ["n|cardinal_number|int4", "y|yes_or_no|varchar"]
    );
}
