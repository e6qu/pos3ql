//! `DISTINCT` with an `ORDER BY` on some of its columns, which PostgreSQL
//! plans as a `HashAggregate` under a `Sort` of the `ORDER BY` items: rows
//! tied on the `ORDER BY` come back in the order the sort leaves the hash
//! table's rows (expected rows from PostgreSQL 18.6).

use super::compatibility_diagnostics::with_engine;
use super::*;

#[test]
fn distinct_orders_ties_as_the_sort_of_its_hash_table_leaves_them() {
    with_engine("distinct-order-keys", |engine, budget| {
        for sql in [
            "CREATE TABLE ties (id int, c text, flag bool)",
            "INSERT INTO ties SELECT i, 'c' || i, i % 2 = 0 FROM generate_series(0, 9) i",
        ] {
            run_with(engine, budget, sql);
        }
        for (sql, expected) in [
            (
                "SELECT DISTINCT c, flag FROM ties ORDER BY flag",
                "c5|f c7|f c1|f c3|f c9|f c6|t c0|t c8|t c4|t c2|t",
            ),
            (
                "SELECT DISTINCT c, flag FROM ties ORDER BY flag DESC",
                "c8|t c4|t c6|t c2|t c0|t c1|f c7|f c5|f c3|f c9|f",
            ),
            (
                "SELECT DISTINCT flag, c FROM ties ORDER BY flag",
                "f|c5 f|c7 f|c1 f|c3 f|c9 t|c6 t|c0 t|c8 t|c4 t|c2",
            ),
        ] {
            assert_eq!(
                data_rows(&run_with(engine, budget, sql)).join(" "),
                expected,
                "{sql}"
            );
        }
    });
}
