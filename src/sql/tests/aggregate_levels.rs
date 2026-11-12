use super::*;

/// The first error's SQLSTATE and message.
pub(super) fn error_message(bytes: &[u8]) -> Option<String> {
    let mut offset = 0;
    while offset < bytes.len() {
        let tag = bytes[offset];
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if tag == b'E' {
            let (mut state, mut message) = (String::new(), String::new());
            let mut fields = &bytes[offset + 5..offset + 1 + length];
            while let Some((&field, remaining)) = fields.split_first() {
                if field == 0 {
                    break;
                }
                let end = remaining.iter().position(|&byte| byte == 0).unwrap();
                let value = String::from_utf8_lossy(&remaining[..end]).to_string();
                match field {
                    b'C' => state = value,
                    b'M' => message = value,
                    _ => {}
                }
                fields = &remaining[end + 1..];
            }
            return Some(format!("{state}: {message}"));
        }
        offset += 1 + length;
    }
    None
}

/// Runs `sql` and returns its data rows, or its error.
pub(super) fn rows(
    engine: &mut Engine,
    budget: &mut Budget,
    txn: &mut TxnState,
    sql: &str,
) -> Vec<String> {
    let bytes = run_txn_bytes(engine, budget, txn, sql);
    match error_message(&bytes) {
        Some(error) => vec![format!("ERROR {error}")],
        None => data_rows(&bytes),
    }
}

/// Expected values were read from PostgreSQL 18.6 (`initdb --locale=C`).
#[test]
fn outer_level_aggregates_belong_to_the_query_whose_columns_they_read() {
    let (mut e, mut b) = test_engine();
    let mut t = TxnState::new(&mut b, 256).unwrap();
    for sql in [
        "CREATE TABLE t (id int, g int)",
        "CREATE TABLE u (x int)",
        "CREATE TABLE z (x int)",
        "INSERT INTO t VALUES (1, 1), (2, 1), (3, 2)",
        "INSERT INTO u VALUES (10)",
        "INSERT INTO z VALUES (100)",
    ] {
        run_txn(&mut e, &mut b, &mut t, sql);
    }
    let mut check = |sql: &str, expected: &[&str]| {
        assert_eq!(rows(&mut e, &mut b, &mut t, sql), expected, "{sql}");
    };
    check("SELECT (SELECT max(t.id) FROM u) FROM t", &["3"]);
    check(
        "SELECT (SELECT max(t.id) + min(u.x) FROM u) FROM t",
        &["13"],
    );
    check(
        "SELECT g, (SELECT max(t.id) FROM u) FROM t GROUP BY g ORDER BY g",
        &["1|2", "2|3"],
    );
    check(
        "SELECT g, (SELECT sum(t.id) + count(*) FROM u) FROM t GROUP BY g ORDER BY g",
        &["1|4", "2|4"],
    );
    check(
        "SELECT g FROM t GROUP BY g HAVING (SELECT max(t.id) FROM u) > 2 ORDER BY 1",
        &["2"],
    );
    check(
        "SELECT g FROM t GROUP BY g ORDER BY (SELECT max(t.id) FROM u) DESC",
        &["2", "1"],
    );
    check(
        "SELECT g, max(t.id) IN (SELECT u.x - 8 FROM u) FROM t GROUP BY g ORDER BY g",
        &["1|t", "2|f"],
    );
    // Owned by the outer query, so allowed in the subquery's WHERE.
    check(
        "SELECT (SELECT 1 FROM u WHERE max(t.id) > 2) FROM t",
        &["1"],
    );
    check(
        "SELECT (SELECT max(u.x) FROM u) FROM t",
        &["10", "10", "10"],
    );
    check(
        "SELECT (SELECT (SELECT max(u.x) + max(t.id)) FROM u) FROM t",
        &["13"],
    );
    check(
        "SELECT (SELECT count(*) FROM generate_series(1, max(t.id))) FROM t",
        &["3"],
    );
    check(
        "WITH c AS (SELECT * FROM t) SELECT (SELECT max(c.id) FROM u) FROM c",
        &["3"],
    );
    check(
        "SELECT (SELECT max(t.id) FROM u) FROM t HAVING false",
        &[] as &[&str],
    );
    // Correlated subqueries nested two levels read both enclosing rows.
    check(
        "SELECT (SELECT (SELECT u.x + t.id FROM z) FROM u) FROM t ORDER BY 1",
        &["11", "12", "13"],
    );
    check(
        "SELECT (SELECT count(*) FROM u WHERE (SELECT u.x + t.id FROM z) > 11) FROM t ORDER BY 1",
        &["0", "1", "1"],
    );
    for (sql, error) in [
        (
            "SELECT (SELECT max(t.id) FROM u), id FROM t",
            "42803: column \"t.id\" must appear in the GROUP BY clause or be used in an aggregate function",
        ),
        (
            "SELECT * FROM t WHERE (SELECT max(t.id) FROM u) > 0",
            "42803: aggregate functions are not allowed in WHERE",
        ),
        (
            "SELECT * FROM t JOIN u ON (SELECT max(t.id) FROM z) > 0",
            "42803: aggregate functions are not allowed in JOIN conditions",
        ),
        (
            "SELECT g FROM t GROUP BY (SELECT max(t.id) FROM u)",
            "42803: aggregate functions are not allowed in GROUP BY",
        ),
        (
            "SELECT max((SELECT max(t.id) FROM u)) FROM t",
            "42803: aggregate function calls cannot be nested",
        ),
        (
            "SELECT (SELECT sum(max(t.id)) FROM u) FROM t",
            "42803: aggregate function calls cannot be nested",
        ),
        (
            "SELECT count(*) FILTER (WHERE (SELECT max(t.id) FROM z) > 0) FROM t",
            "42803: aggregate functions are not allowed in FILTER",
        ),
        (
            "SELECT * FROM t WHERE max(id) > 0",
            "42803: aggregate functions are not allowed in WHERE",
        ),
        (
            "SELECT * FROM t LIMIT max(1)",
            "42803: aggregate functions are not allowed in LIMIT",
        ),
        (
            "SELECT * FROM generate_series(1, max(2))",
            "42803: aggregate functions are not allowed in functions in FROM",
        ),
        (
            "SELECT * FROM t, LATERAL (SELECT max(t.id)) s",
            "42803: aggregate functions are not allowed in FROM clause of their own query level",
        ),
        (
            "SELECT max(id) FILTER (WHERE max(id) > 0) FROM t",
            "42803: aggregate functions are not allowed in FILTER",
        ),
    ] {
        check(sql, &[format!("ERROR {error}").as_str()]);
    }
    check("INSERT INTO u VALUES (20)", &[] as &[&str]);
    check(
        "SELECT (SELECT max(t.id) FROM u) FROM t",
        &["ERROR 21000: more than one row returned by a subquery used as an expression"],
    );
}
