//! `DISTINCT` with an `ORDER BY` on some of its columns sorts by the
//! `ORDER BY` items, then the remaining output columns, so rows tied on the
//! `ORDER BY` come back in the order of the rest.

use super::compatibility_diagnostics::with_engine;
use super::*;

#[test]
fn distinct_orders_ties_by_the_remaining_output_columns() {
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
                "c1|f c3|f c5|f c7|f c9|f c0|t c2|t c4|t c6|t c8|t",
            ),
            (
                "SELECT DISTINCT c, flag FROM ties ORDER BY flag DESC",
                "c0|t c2|t c4|t c6|t c8|t c1|f c3|f c5|f c7|f c9|f",
            ),
            (
                "SELECT DISTINCT flag, c FROM ties ORDER BY flag",
                "f|c1 f|c3 f|c5 f|c7 f|c9 t|c0 t|c2 t|c4 t|c6 t|c8",
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
