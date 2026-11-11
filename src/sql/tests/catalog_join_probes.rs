//! A catalog relation joined below a join's first level is probed by its join
//! key instead of being scanned once per outer row.

use super::*;

#[test]
fn foreign_key_reflection_probes_descriptions_by_key() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA cjp; CREATE TABLE cjp.parent (id int PRIMARY KEY); \
         CREATE TABLE cjp.child (id int PRIMARY KEY, \
           parent_id int CONSTRAINT child_parent_fk REFERENCES cjp.parent(id)); \
         CREATE TABLE cjp.other (id int, \
           child_id int CONSTRAINT other_child_fk REFERENCES cjp.child(id)); \
         COMMENT ON CONSTRAINT child_parent_fk ON cjp.child IS 'child to parent'",
    );
    let descriptions = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "SELECT count(*) FROM pg_description",
    ))[0]
        .parse::<u64>()
        .unwrap();
    let classes = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "SELECT count(*) FROM pg_class",
    ))[0]
        .parse::<u64>()
        .unwrap();
    let before = crate::sql::query::derived_candidates_visited();
    // SQLAlchemy's get_multi_foreign_keys query; results from PostgreSQL 18.6.
    let rows = data_rows(&run_with(
        &mut engine,
        &mut budget,
        "SELECT pg_catalog.pg_class.relname, pg_catalog.pg_constraint.conname, \
           nsp_ref.nspname, cls_ref.relname, pg_catalog.pg_description.description \
         FROM pg_catalog.pg_class \
         LEFT OUTER JOIN pg_catalog.pg_constraint \
           ON pg_catalog.pg_class.oid = pg_catalog.pg_constraint.conrelid \
          AND pg_catalog.pg_constraint.contype = 'f' \
         LEFT OUTER JOIN pg_catalog.pg_class AS cls_ref \
           ON cls_ref.oid = pg_catalog.pg_constraint.confrelid \
         LEFT OUTER JOIN pg_catalog.pg_namespace AS nsp_ref \
           ON cls_ref.relnamespace = nsp_ref.oid \
         LEFT OUTER JOIN pg_catalog.pg_description \
           ON pg_catalog.pg_description.objoid = pg_catalog.pg_constraint.oid \
          AND pg_catalog.pg_description.classoid = \
              CAST('pg_catalog.pg_constraint' AS REGCLASS) \
         JOIN pg_catalog.pg_namespace \
           ON pg_catalog.pg_namespace.oid = pg_catalog.pg_class.relnamespace \
         WHERE pg_catalog.pg_class.relkind = ANY (ARRAY['r', 'p', 'f']) \
           AND pg_catalog.pg_namespace.nspname = 'cjp' ORDER BY 1, 2",
    ));
    let visited = crate::sql::query::derived_candidates_visited() - before;
    assert_eq!(
        rows,
        [
            "child|child_parent_fk|cjp|parent|child to parent",
            "other|other_child_fk|cjp|child|NULL",
            "parent|NULL|NULL|NULL|NULL",
        ]
    );
    // A scan per outer row would visit every description for each class
    // row; probing visits only rows sharing a key, and sources smaller than
    // the probe threshold.
    assert!(
        visited < classes * 32 && visited < classes * descriptions / 64,
        "visited {visited} rows for {classes} classes and {descriptions} descriptions"
    );
}
