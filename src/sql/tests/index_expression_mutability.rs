use super::*;

#[test]
fn index_keys_and_predicates_reject_stable_casts_and_non_inlinable_functions() {
    let config = test_config("index-expression-mutability");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for setup in [
        "CREATE TABLE mu(stamp timestamptz, ts timestamp, t text, n int, iv interval)",
        "CREATE FUNCTION fs(int) RETURNS int LANGUAGE plpgsql STABLE AS 'begin return $1; end'",
        "CREATE FUNCTION sv(int) RETURNS int LANGUAGE sql VOLATILE AS 'select $1'",
        "CREATE FUNCTION sv2(int) RETURNS int LANGUAGE sql VOLATILE \
         AS 'select $1 + extract(day from now())::int'",
        "CREATE FUNCTION sd(int) RETURNS int LANGUAGE sql STABLE SECURITY DEFINER AS 'select $1'",
        "CREATE FUNCTION sa(int) RETURNS int STABLE BEGIN ATOMIC SELECT $1; END",
    ] {
        session.success(&mut engine, setup, true);
    }
    for (sql, accepted) in [
        ("CREATE INDEX ON mu ((stamp::date))", false),
        ("CREATE INDEX ON mu ((ts::date))", true),
        ("CREATE INDEX ON mu ((t::date))", false),
        ("CREATE INDEX ON mu ((stamp::text))", false),
        ("CREATE INDEX ON mu ((stamp + iv))", false),
        ("CREATE INDEX ON mu ((ts + iv))", true),
        ("CREATE INDEX ON mu ((stamp > '2020-01-01'))", true),
        (
            "CREATE INDEX ON mu (n) WHERE stamp::date > '2020-01-01'",
            false,
        ),
        ("CREATE INDEX ON mu ((fs(n)))", false),
        ("CREATE INDEX ON mu ((sv(n)))", true),
        ("CREATE INDEX ON mu ((sv2(n)))", false),
        ("CREATE INDEX ON mu ((sd(n)))", false),
        ("CREATE INDEX ON mu ((sa(n)))", true),
        ("CREATE INDEX ON mu (n) WHERE fs(n) > 0", false),
    ] {
        let response = session.execute(&mut engine, sql, true);
        let expected: &[&str] = if accepted { &[] } else { &["42P17"] };
        assert_eq!(error_sqlstates(&response), expected, "{sql}");
    }
}
