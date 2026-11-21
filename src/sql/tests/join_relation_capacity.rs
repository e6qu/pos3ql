//! The startup setting `max_join_relations`: the join search plans a query
//! of up to that many relations, by GEQO past `geqo_threshold`, and fails a
//! query of more with `54000` naming the setting rather than run it in
//! another join order.

use super::compatibility_diagnostics::diagnostics;
use super::*;

#[test]
fn joins_past_max_join_relations_fail_by_name() {
    let mut config = test_config("join-relation-capacity");
    config.max_join_relations = 4;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    for sql in [
        "CREATE TABLE jr (id int)",
        "INSERT INTO jr SELECT g FROM generate_series(1, 3) g",
        "ANALYZE jr",
    ] {
        run_with(&mut engine, &mut budget, sql);
    }
    let four = "SELECT count(*) FROM jr a JOIN jr b USING (id) JOIN jr c USING (id) \
                JOIN jr d USING (id)";
    assert_eq!(data_rows(&run_with(&mut engine, &mut budget, four)), ["3"]);
    let five = format!("{four} JOIN jr e USING (id)");
    assert_eq!(
        diagnostics(&run_with(&mut engine, &mut budget, &five)),
        [
            "ERROR 54000 join search capacity exceeded: a query of 5 relations, more than \
          max_join_relations (4)"
        ]
    );
    drop(engine);
    std::fs::remove_dir_all(config.data_dir).unwrap();
}
