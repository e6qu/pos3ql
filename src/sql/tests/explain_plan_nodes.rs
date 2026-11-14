use super::*;

/// Expected plans were read from PostgreSQL 18.6 (`initdb --locale=C`).
fn setup() -> (ConfiguredTransactionSession, Engine) {
    let config = test_config("explain-plan-nodes");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE pn_t (a int, b int); CREATE TABLE pn_u (a int, c text); \
         INSERT INTO pn_t SELECT g % 10, g FROM generate_series(1, 100) g; \
         INSERT INTO pn_u SELECT g, 'x' || g FROM generate_series(1, 20) g; \
         ANALYZE pn_t; ANALYZE pn_u",
        true,
    );
    (session, engine)
}

fn plan(
    session: &mut ConfiguredTransactionSession,
    engine: &mut Engine,
    options: &str,
    query: &str,
) -> Vec<String> {
    data_rows(&session.success(engine, &format!("EXPLAIN ({options}) {query}"), true))
}

/// Memory figures are pos3ql's own; the rest of each line is PostgreSQL's.
fn without_memory(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(
            |line| match line.find("Memory: ").or(line.find("Storage: ")) {
                Some(at) if line.ends_with("kB") => {
                    let label_end = line[at..].find(": ").map_or(line.len(), |end| at + end + 2);
                    format!("{}NkB", &line[..label_end])
                }
                _ => line,
            },
        )
        .collect()
}

#[test]
fn locking_clauses_show_lock_rows_below_the_limit() {
    let (mut session, mut engine) = setup();
    let costs = "COSTS OFF";
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT * FROM pn_t WHERE a = 1 FOR SHARE"
        ),
        [
            "LockRows",
            "  ->  Seq Scan on pn_t",
            "        Filter: (a = 1)"
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT * FROM pn_t ORDER BY b LIMIT 1 FOR NO KEY UPDATE SKIP LOCKED"
        ),
        [
            "Limit",
            "  ->  LockRows",
            "        ->  Sort",
            "              Sort Key: b",
            "              ->  Seq Scan on pn_t",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT * FROM (SELECT * FROM pn_t FOR UPDATE) s"
        ),
        [
            "Subquery Scan on s",
            "  ->  LockRows",
            "        ->  Seq Scan on pn_t"
        ]
    );
    // A query without FROM has no rows to lock.
    assert_eq!(
        plan(&mut session, &mut engine, costs, "SELECT 1 FOR UPDATE"),
        ["Result"]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            "ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF",
            "SELECT * FROM pn_t WHERE a = 1 ORDER BY b OFFSET 3 LIMIT 4 FOR UPDATE"
        )
        .into_iter()
        .filter(|line| !line.contains("Sort Method"))
        .collect::<Vec<_>>(),
        [
            "Limit (actual rows=4.00 loops=1)",
            "  ->  LockRows (actual rows=7.00 loops=1)",
            "        ->  Sort (actual rows=7.00 loops=1)",
            "              Sort Key: b",
            "              ->  Seq Scan on pn_t (actual rows=10.00 loops=1)",
            "                    Filter: (a = 1)",
            "                    Rows Removed by Filter: 90",
        ]
    );
}

#[test]
fn analyzed_sorts_report_their_method() {
    let (mut session, mut engine) = setup();
    let analyze = "ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF";
    // A limit bounds the sort only past twice its rows.
    for (query, method) in [
        ("SELECT * FROM pn_t ORDER BY b", "quicksort"),
        (
            "SELECT * FROM pn_t ORDER BY b DESC LIMIT 3",
            "top-N heapsort",
        ),
        ("SELECT * FROM pn_t ORDER BY b LIMIT 50", "quicksort"),
        ("SELECT * FROM pn_t ORDER BY b LIMIT 49", "top-N heapsort"),
        (
            "SELECT * FROM pn_t ORDER BY b FETCH FIRST 3 ROWS WITH TIES",
            "quicksort",
        ),
        (
            "SELECT a, count(*) FROM pn_t GROUP BY a ORDER BY count(*) DESC, a LIMIT 2",
            "top-N heapsort",
        ),
        (
            "SELECT a FROM pn_t UNION SELECT a FROM pn_u ORDER BY 1 LIMIT 3",
            "top-N heapsort",
        ),
        (
            "SELECT generate_series(1, 10) g ORDER BY 1 DESC",
            "quicksort",
        ),
    ] {
        let lines = without_memory(plan(&mut session, &mut engine, analyze, query));
        assert!(
            lines
                .iter()
                .any(|line| line.trim_start() == format!("Sort Method: {method}  Memory: NkB")),
            "{query}: {lines:?}"
        );
    }
    // The bounded sort returns the rows a full sort would put first.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT b FROM pn_t ORDER BY a DESC, b LIMIT 3 OFFSET 2",
            true
        )),
        ["29", "39", "49"]
    );
    let json = plan(
        &mut session,
        &mut engine,
        &format!("{analyze}, FORMAT JSON"),
        "SELECT * FROM pn_t WHERE a = 3 ORDER BY b",
    )
    .join("\n");
    assert!(json.contains("\"Sort Method\": \"quicksort\","), "{json}");
    assert!(json.contains("\"Sort Space Type\": \"Memory\""), "{json}");
}

#[test]
fn window_clauses_show_their_definitions_and_input_sorts() {
    let (mut session, mut engine) = setup();
    let costs = "COSTS OFF";
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a, sum(b) OVER () FROM pn_t"
        ),
        ["WindowAgg", "  Window: w1 AS ()", "  ->  Seq Scan on pn_t"]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a, sum(b) OVER (PARTITION BY a ORDER BY b NULLS FIRST) FROM pn_t ORDER BY a"
        ),
        [
            "WindowAgg",
            "  Window: w1 AS (PARTITION BY a ORDER BY b)",
            "  ->  Sort",
            "        Sort Key: a, b NULLS FIRST",
            "        ->  Seq Scan on pn_t",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a, sum(b) OVER (PARTITION BY a) FROM pn_t ORDER BY a, b"
        ),
        [
            "Incremental Sort",
            "  Sort Key: a, b",
            "  Presorted Key: a",
            "  ->  WindowAgg",
            "        Window: w1 AS (PARTITION BY a)",
            "        ->  Sort",
            "              Sort Key: a",
            "              ->  Seq Scan on pn_t",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a, row_number() OVER (ORDER BY b), rank() OVER (PARTITION BY a ORDER BY b) \
             FROM pn_t"
        ),
        [
            "WindowAgg",
            "  Window: w2 AS (ORDER BY b ROWS UNBOUNDED PRECEDING)",
            "  ->  Sort",
            "        Sort Key: b",
            "        ->  WindowAgg",
            "              Window: w1 AS (PARTITION BY a ORDER BY b ROWS UNBOUNDED PRECEDING)",
            "              ->  Sort",
            "                    Sort Key: a, b",
            "                    ->  Seq Scan on pn_t",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a, sum(b) OVER w, count(*) OVER (ORDER BY b ROWS BETWEEN 1 PRECEDING AND \
             CURRENT ROW) FROM pn_t WINDOW w AS (ORDER BY b ROWS BETWEEN 1 PRECEDING AND CURRENT ROW)"
        ),
        [
            "WindowAgg",
            "  Window: w AS (ORDER BY b ROWS BETWEEN '1'::bigint PRECEDING AND CURRENT ROW)",
            "  ->  Sort",
            "        Sort Key: b",
            "        ->  Seq Scan on pn_t",
        ]
    );
    assert_eq!(
        without_memory(plan(
            &mut session,
            &mut engine,
            "ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF",
            "SELECT a, sum(b) OVER (PARTITION BY a) FROM pn_t ORDER BY b DESC LIMIT 4"
        )),
        [
            "Limit (actual rows=4.00 loops=1)",
            "  ->  Sort (actual rows=4.00 loops=1)",
            "        Sort Key: b DESC",
            "        Sort Method: top-N heapsort  Memory: NkB",
            "        ->  WindowAgg (actual rows=100.00 loops=1)",
            "              Window: w1 AS (PARTITION BY a)",
            "              Storage: Memory  Maximum Storage: NkB",
            "              ->  Sort (actual rows=100.00 loops=1)",
            "                    Sort Key: a",
            "                    Sort Method: quicksort  Memory: NkB",
            "                    ->  Seq Scan on pn_t (actual rows=100.00 loops=1)",
        ]
    );
}

#[test]
fn constant_false_conditions_keep_the_upper_nodes() {
    let (mut session, mut engine) = setup();
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            "COSTS OFF",
            "SELECT * FROM pn_t JOIN pn_u ON pn_t.b = pn_u.a WHERE false ORDER BY pn_t.b LIMIT 2"
        ),
        [
            "Limit",
            "  ->  Sort",
            "        Sort Key: pn_t.b",
            "        ->  Result",
            "              One-Time Filter: false",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            "ANALYZE, COSTS OFF, TIMING OFF, SUMMARY OFF, BUFFERS OFF",
            "SELECT * FROM pn_t WHERE false FOR UPDATE"
        ),
        [
            "LockRows (actual rows=0.00 loops=1)",
            "  ->  Result (actual rows=0.00 loops=1)",
            "        One-Time Filter: false",
        ]
    );
}

#[test]
fn set_operation_sort_keys_name_the_first_branch_column() {
    let (mut session, mut engine) = setup();
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            "COSTS OFF",
            "SELECT a AS x, b FROM pn_t tt UNION ALL SELECT a, a FROM pn_u ORDER BY 2 DESC, x"
        ),
        [
            "Sort",
            "  Sort Key: tt.b DESC, tt.a",
            "  ->  Append",
            "        ->  Seq Scan on pn_t tt",
            "        ->  Seq Scan on pn_u",
        ]
    );
}

#[test]
fn with_ties_without_order_by_points_at_with() {
    let (mut session, mut engine) = setup();
    let query = "SELECT * FROM pn_t FETCH FIRST 2 ROWS WITH TIES";
    let bytes = session.execute(&mut engine, query, true);
    let position = (query.find("WITH").unwrap() + 1).to_string();
    let mut offset = 0;
    let mut fields = None;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if bytes[offset] == b'E' {
            let payload = &bytes[offset + 5..offset + 1 + length];
            let field = |key: u8| {
                payload
                    .split(|byte| *byte == 0)
                    .find(|value| value.first() == Some(&key))
                    .map(|value| String::from_utf8(value[1..].to_vec()).unwrap())
            };
            fields = Some((field(b'C'), field(b'M'), field(b'P')));
        }
        offset += 1 + length;
    }
    assert_eq!(
        fields,
        Some((
            Some("42601".to_owned()),
            Some("WITH TIES cannot be specified without ORDER BY clause".to_owned()),
            Some(position),
        ))
    );
}

#[test]
fn subqueries_run_as_numbered_init_plans_and_subplans() {
    let (mut session, mut engine) = setup();
    let costs = "COSTS OFF";
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT (SELECT count(*) FROM pn_u), a FROM pn_t \
             WHERE b > (SELECT count(*) FROM pn_u u2) ORDER BY b"
        ),
        [
            "Sort",
            "  Sort Key: pn_t.b",
            "  InitPlan 1",
            "    ->  Aggregate",
            "          ->  Seq Scan on pn_u",
            "  InitPlan 2",
            "    ->  Aggregate",
            "          ->  Seq Scan on pn_u u2",
            "  ->  Seq Scan on pn_t",
            "        Filter: (b > (InitPlan 2).col1)",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a FROM pn_t WHERE b = (SELECT max(b) FROM pn_t t2 \
             WHERE t2.a = pn_t.a + (SELECT count(*) FROM pn_u))"
        ),
        [
            "Seq Scan on pn_t",
            "  Filter: (b = (SubPlan 2))",
            "  SubPlan 2",
            "    ->  Aggregate",
            "          InitPlan 1",
            "            ->  Aggregate",
            "                  ->  Seq Scan on pn_u",
            "          ->  Seq Scan on pn_t t2",
            "                Filter: (a = (pn_t.a + (InitPlan 1).col1))",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT * FROM pn_t WHERE a = 1 AND EXISTS (SELECT 1 FROM pn_u WHERE a > 5)"
        ),
        [
            "Result",
            "  One-Time Filter: (InitPlan 1).col1",
            "  InitPlan 1",
            "    ->  Seq Scan on pn_u",
            "          Filter: (a > 5)",
            "  ->  Seq Scan on pn_t",
            "        Filter: (a = 1)",
        ]
    );
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT * FROM pn_t JOIN pn_u ON pn_t.b = pn_u.a \
             WHERE pn_t.b > (SELECT count(*) FROM pn_u)"
        )[2..5],
        [
            "  InitPlan 1",
            "    ->  Aggregate",
            "          ->  Seq Scan on pn_u pn_u_1",
        ]
    );
    // A new statement numbers its subplans afresh.
    assert_eq!(
        plan(
            &mut session,
            &mut engine,
            costs,
            "SELECT a, (SELECT c FROM pn_u WHERE pn_u.a = pn_t.b) FROM pn_t"
        ),
        [
            "Seq Scan on pn_t",
            "  SubPlan 1",
            "    ->  Seq Scan on pn_u",
            "          Filter: (a = pn_t.b)",
        ]
    );
}
