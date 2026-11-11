use super::*;

/// psql's `\d` asks for `pg_partition_ancestors` of every described table.
#[test]
fn partition_ancestry_functions_follow_postgresql() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE plain(a int); \
         CREATE TABLE tree(a int) PARTITION BY RANGE (a); \
         CREATE TABLE branch PARTITION OF tree FOR VALUES FROM (0) TO (10) PARTITION BY RANGE (a); \
         CREATE TABLE leaf PARTITION OF branch FOR VALUES FROM (0) TO (5); \
         CREATE TABLE twig PARTITION OF tree FOR VALUES FROM (10) TO (20)",
    );
    for (sql, expected) in [
        ("SELECT pg_partition_ancestors('plain')", vec![]),
        ("SELECT pg_partition_root('plain') IS NULL", vec!["t"]),
        (
            "SELECT pg_partition_ancestors('leaf'), pg_partition_root('leaf')",
            vec!["leaf|tree", "branch|tree", "tree|tree"],
        ),
        (
            "SELECT relid, pg_typeof(relid) FROM pg_partition_ancestors('branch'::regclass)",
            vec!["branch|regclass", "tree|regclass"],
        ),
        (
            "SELECT conname FROM pg_constraint WHERE confrelid IN \
             (SELECT pg_catalog.pg_partition_ancestors('leaf') \
              UNION ALL VALUES ('leaf'::pg_catalog.regclass))",
            vec![],
        ),
        ("SELECT * FROM pg_partition_ancestors(0)", vec![]),
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert!(
            error_sqlstates(&output).is_empty(),
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
        assert_eq!(data_rows(&output), expected, "{sql}");
    }
    for (sql, expected) in [
        (
            "SELECT relid, parentrelid, isleaf, level FROM pg_partition_tree('tree')",
            vec![
                "tree|NULL|f|0",
                "branch|tree|f|1",
                "twig|tree|t|1",
                "leaf|branch|t|2",
            ],
        ),
        (
            "SELECT pg_partition_tree('branch')",
            vec!["(branch,tree,f,0)", "(leaf,branch,t,1)"],
        ),
        ("SELECT count(*) FROM pg_partition_tree('plain')", vec!["0"]),
    ] {
        let output = run_with(&mut engine, &mut budget, sql);
        assert_eq!(
            data_rows(&output),
            expected,
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
    }
}
