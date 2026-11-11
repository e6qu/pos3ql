use super::*;

/// Runs each query and returns its data rows, `|`-separated. Expected values
/// were captured from PostgreSQL 18.6.
fn rows(queries: &[&str]) -> Vec<Vec<String>> {
    let config = test_config("polymorphic-resolution");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    queries
        .iter()
        .map(|sql| data_rows(&session.success(&mut engine, sql, true)))
        .collect()
}

/// `anycompatible` array built-ins choose one common element type from all
/// their typed inputs before running, so an untyped literal is read as that
/// type and typed inputs convert implicitly, as in PostgreSQL's parse analysis.
#[test]
fn compatible_array_functions_resolve_untyped_and_mixed_inputs() {
    assert_eq!(
        rows(&[
            "SELECT array_replace('{a}'::varchar[], 'a', 'b'), \
                    pg_typeof(array_replace('{a}'::varchar[], 'a'::text, 'b'))",
            "SELECT array_replace(ARRAY[1], 1, 2.5), \
                    pg_typeof(array_replace(ARRAY[1], 1, 2.5))",
            "SELECT array_replace(ARRAY['2020-01-01'::date], '2020-01-01', '2021-01-01')",
            "SELECT array_positions(ARRAY['2020-01-01'::date], '2020-01-01'), \
                    array_remove(ARRAY['2020-01-01'::date], '2020-01-01')",
            "SELECT array_append(ARRAY['2020-01-01'::date], '2020-01-02'), \
                    array_prepend('2020-01-02', ARRAY['2020-01-01'::date])",
            "SELECT array_cat(ARRAY['2020-01-01'::date], '{2020-01-02}'), \
                    array_cat('{a}'::refcursor[], '{b}'), \
                    pg_typeof(array_cat('{a}'::refcursor[], '{b}'))",
            "SELECT array_append('{a}'::varchar[], 'b'::text), \
                    pg_typeof(array_append('{a}'::varchar[], 'b'::text))",
            "SELECT array_append('{1}', 2), array_append(NULL, 2), array_append(ARRAY[1], '2')",
            "SELECT array_replace('{1,2}'::int2[], 2::int2, 70000), \
                    pg_typeof(array_replace('{1,2}'::int2[], 2, 7))",
            "SELECT width_bucket(5, ARRAY[1, 10]), \
                    width_bucket('2020-01-05'::date, '{2020-01-01,2020-02-01}'), \
                    width_bucket(5, ARRAY[1.5, 10])",
            "SELECT array_fill(7, '{2}'), array_fill(7, ARRAY[2], '{0}')",
        ]),
        [
            vec!["{b}|character varying[]"],
            vec!["{2.5}|numeric[]"],
            vec!["{2021-01-01}"],
            vec!["{1}|{}"],
            vec!["{2020-01-01,2020-01-02}|{2020-01-02,2020-01-01}"],
            vec!["{2020-01-01,2020-01-02}|{a,b}|refcursor[]"],
            vec!["{a,b}|character varying[]"],
            vec!["{1,2}|{2}|{1,2}"],
            vec!["{1,70000}|integer[]"],
            vec!["1|1|1"],
            vec!["{7,7}|[0:1]={7,7}"],
        ]
    );
}

/// Without a common type the call does not resolve; element types without a
/// default equality fail when the search or replacement needs it.
#[test]
fn compatible_array_functions_report_postgresql_failures() {
    use super::routine_identity_diagnostics::check;
    check(&[
        (
            "SELECT array_replace('{a}'::refcursor[], 'a', 'b')",
            &[(
                'E',
                "42883",
                "could not identify an equality operator for type refcursor",
            )],
        ),
        (
            "SELECT array_replace('{a}'::refcursor[], 'a'::text, 'b')",
            &[(
                'E',
                "42883",
                "function array_replace(refcursor[], text, unknown) does not exist",
            )],
        ),
        (
            "SELECT array_append('{a}'::text[], 1)",
            &[(
                'E',
                "42883",
                "function array_append(text[], integer) does not exist",
            )],
        ),
        (
            "SELECT array_replace(ARRAY['(1,2)'::point], '(1,2)', '(3,4)')",
            &[(
                'E',
                "42883",
                "could not identify an equality operator for type point",
            )],
        ),
        (
            "SELECT width_bucket(5, ARRAY[[1],[10]])",
            &[('E', "2202E", "thresholds must be one-dimensional array")],
        ),
        (
            "SELECT width_bucket(5, ARRAY[1, NULL])",
            &[('E', "22004", "thresholds array must not contain NULLs")],
        ),
    ]);
}

/// `COALESCE`, `GREATEST`, `LEAST`, range functions and range operators read
/// an untyped literal as the type the other inputs select.
#[test]
fn untyped_literals_take_the_resolved_type_of_other_inputs() {
    assert_eq!(
        rows(&[
            "SELECT greatest(1, '2'), least('a'::varchar, 'b'), coalesce(NULL, 1, '2')",
            "SELECT pg_typeof(coalesce(NULL, '2', 1))",
            "SELECT range_merge(daterange('2020-01-01', '2020-01-05'), '[2020-01-07,2020-01-09)'), \
                    range_merge('[1,2)', int4range(3, 4))",
            "SELECT range_adjacent(int4range(1, 5), '[5,7)'), range_before(int4range(1, 5), '[5,7)')",
            "SELECT '[1,5)'::int4range @> '[2,3)', '[2,3)' <@ '[1,5)'::int4range, \
                    '{[1,5)}'::int4multirange @> '{[2,3)}', int4range(1, 5) + '[3,7)'",
        ]),
        [
            vec!["2|a|1"],
            vec!["integer"],
            vec!["[2020-01-01,2020-01-09)|[1,4)"],
            vec!["t|t"],
            vec!["t|t|t|[1,7)"],
        ]
    );
}

/// DISTINCT, grouping and set operations need the type's default equality,
/// which geometric types, `xml` and `json` lack even where an `=` operator
/// exists (box area equality).
#[test]
fn keys_without_default_equality_are_rejected() {
    use super::routine_identity_diagnostics::check;
    let equality = |name| [('E', "42883", name)];
    check(&[
        (
            "SELECT DISTINCT '(1,2)'::point",
            &equality("could not identify an equality operator for type point"),
        ),
        (
            "SELECT DISTINCT x FROM (VALUES ('(1,2),(3,4)'::box)) v(x)",
            &equality("could not identify an equality operator for type box"),
        ),
        (
            "SELECT x FROM (VALUES ('(1,2)'::point)) v(x) GROUP BY x",
            &equality("could not identify an equality operator for type point"),
        ),
        (
            "SELECT '<a/>'::xml UNION SELECT '<a/>'::xml",
            &equality("could not identify an equality operator for type xml"),
        ),
        (
            "SELECT DISTINCT '{}'::json",
            &equality("could not identify an equality operator for type json"),
        ),
        (
            "SELECT array_position(ARRAY['<a/>'::xml], '<a/>')",
            &equality("could not identify an equality operator for type xml"),
        ),
        ("SELECT '(1,2),(3,4)'::box = '(1,2),(3,4)'::box", &[]),
    ]);
}

/// `+` has no timestamp/timestamp or time/time form, so an untyped operand
/// beside one is an interval; `date` and `timetz` have several candidates.
/// `-` reads the untyped operand as the other operand's type.
#[test]
fn untyped_datetime_operands_follow_postgresql_operator_choice() {
    assert_eq!(
        rows(&[
            "SET TIME ZONE 'UTC'; SELECT '2020-01-01 00:00+00'::timestamptz + '200ms', \
                    '200ms' + '2020-01-01 00:00+00'::timestamptz, \
                    '2020-01-01 00:00'::timestamp + '1 day', '1 hour' + '10:00'::time",
            "SELECT '2020-01-01'::date - '2019-12-31', \
                    pg_typeof('2020-01-01'::date - '2019-12-31'), \
                    pg_typeof('10:00'::time - '09:00'), \
                    pg_typeof('2020-01-01 00:00+00'::timestamptz - '2019-12-31 00:00+00')",
        ]),
        [
            vec!["2020-01-01 00:00:00.2+00|2020-01-01 00:00:00.2+00|2020-01-02 00:00:00|11:00:00"],
            vec!["1|integer|interval|interval"],
        ]
    );
    use super::routine_identity_diagnostics::check;
    check(&[
        (
            "SELECT '2020-01-01'::date + '1'",
            &[('E', "42725", "operator is not unique: date + unknown")],
        ),
        (
            "SELECT '1 hour' + '10:00+02'::timetz",
            &[(
                'E',
                "42725",
                "operator is not unique: unknown + time with time zone",
            )],
        ),
    ]);
}

/// Interval input accepts PostgreSQL's compact and verbose forms.
#[test]
fn interval_input_accepts_postgresql_field_forms() {
    assert_eq!(
        rows(&[
            "SELECT '200ms'::interval, '2h30m'::interval, '1y2mon'::interval, \
                    '@ 1 minute ago'::interval, '1week'::interval, '3 4:05:06'::interval",
            "SELECT '1-2'::interval, '-1-2'::interval, '1 DAY'::interval, \
                    '1 day ago'::interval, '2 centuries'::interval, '1.5 years'::interval, \
                    '90'::interval",
        ]),
        [
            vec!["00:00:00.2|02:30:00|1 year 2 mons|-00:01:00|7 days|3 days 04:05:06"],
            vec!["1 year 2 mons|-1 years -2 mons|1 day|-1 days|200 years|1 year 6 mons|00:01:30"],
        ]
    );
    use super::routine_identity_diagnostics::check;
    let invalid = |text: &'static str| [('E', "22007", text)];
    check(&[
        (
            "SELECT 'ago'::interval",
            &invalid("invalid input syntax for type interval: \"ago\""),
        ),
        (
            "SELECT '1 fortnight'::interval",
            &invalid("invalid input syntax for type interval: \"1 fortnight\""),
        ),
        (
            "SELECT '1 2'::interval",
            &invalid("invalid input syntax for type interval: \"1 2\""),
        ),
    ]);
}

/// The sleep functions are callable PostgreSQL catalog entries. Parking on
/// their timer is covered with real connections in `pg::conn::sleep_tests`.
#[test]
fn sleep_functions_have_postgresql_catalog_contracts() {
    assert_eq!(
        rows(&[
            "SELECT proname, prolang, provolatile, proparallel, proisstrict, \
                    proargtypes::regtype[]::text, prorettype::regtype \
             FROM pg_proc WHERE proname LIKE 'pg\\_sleep%' ORDER BY oid",
            "SELECT pg_sleep(0), pg_sleep(-1), pg_sleep('NaN'), pg_sleep(NULL) IS NULL, \
                    pg_sleep_for('0'), pg_sleep_until('-infinity')",
        ]),
        [
            vec![
                "pg_sleep|12|v|s|t|[0:0]={\"double precision\"}|void",
                "pg_sleep_for|14|v|s|t|[0:0]={interval}|void",
                "pg_sleep_until|14|v|s|t|[0:0]={\"timestamp with time zone\"}|void",
            ],
            vec!["|||t||"],
        ]
    );
}
