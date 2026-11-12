//! SQL-standard routine bodies (`RETURN`, `BEGIN ATOMIC`) and views are bound
//! to the objects they named at CREATE: renaming or moving those objects
//! changes neither their results nor their printed form, before and after
//! recovery from the journal alone or from a checkpoint.

use super::*;

const SETUP: &str = r#"
CREATE SCHEMA sb;
CREATE SCHEMA sb_moved;
CREATE FUNCTION sb.f(integer) RETURNS integer LANGUAGE sql IMMUTABLE AS 'SELECT $1 + 1';
CREATE TYPE sb.e AS ENUM ('a', 'b');
CREATE FUNCTION sb_select() RETURNS integer LANGUAGE sql BEGIN ATOMIC SELECT sb.f(1); END;
CREATE FUNCTION sb_return() RETURNS integer LANGUAGE sql RETURN sb.f(2);
CREATE FUNCTION sb_enum() RETURNS sb.e LANGUAGE sql RETURN 'b'::sb.e;
CREATE FUNCTION sb_cast() RETURNS text LANGUAGE sql BEGIN ATOMIC SELECT 'a'::sb.e::text; END;
CREATE FUNCTION sb_set() RETURNS SETOF integer LANGUAGE sql BEGIN ATOMIC SELECT sb.f(1) UNION ALL SELECT sb.f(2); END;
ALTER FUNCTION sb.f(integer) RENAME TO f2;
ALTER TYPE sb.e RENAME TO e2;
ALTER FUNCTION sb.f2(integer) SET SCHEMA sb_moved;
ALTER TYPE sb.e2 SET SCHEMA sb_moved"#;

const PROBE: &str = "
SELECT sb_select(), sb_return(), sb_enum(), sb_cast();
SELECT * FROM sb_set();
SELECT pg_get_function_sqlbody(oid) FROM pg_proc WHERE proname LIKE 'sb\\_%' AND proname <> 'sb_set' ORDER BY proname;";

const EXPECTED: [&str; 7] = [
    "2|3|b|a",
    "2",
    "3",
    "BEGIN ATOMIC\n SELECT ('a'::sb_moved.e2)::text AS text;\nEND",
    "RETURN 'b'::sb_moved.e2",
    "RETURN sb_moved.f2(2)",
    "BEGIN ATOMIC\n SELECT sb_moved.f2(1) AS f;\nEND",
];

#[test]
fn standard_bodies_follow_renames_through_journal_and_checkpoint_recovery() {
    assert_recovered("standard-body-renames", SETUP, PROBE, &EXPECTED);
}

const VIEW_SETUP: &str = r#"
CREATE TYPE ve AS ENUM ('a', 'b');
CREATE TABLE vt (x ve, k integer);
CREATE VIEW vv_filter AS SELECT x FROM vt WHERE x = 'a';
CREATE VIEW vv_cast AS SELECT 'b'::ve AS c, k FROM vt;
ALTER TYPE ve RENAME VALUE 'a' TO 'z';
ALTER TYPE ve RENAME TO ve2;
ALTER TABLE vt RENAME TO vt2"#;

const VIEW_PROBE: &str = "
SELECT pg_get_viewdef('vv_filter'::regclass);
SELECT pg_get_viewdef('vv_cast'::regclass);";

/// Enum literals print as PostgreSQL's constants, and renamed relations,
/// types and enum values print under their current names.
const VIEW_EXPECTED: [&str; 2] = [
    " SELECT x\n   FROM vt2\n  WHERE (x = 'z'::ve2);",
    " SELECT 'b'::ve2 AS c,\n    k\n   FROM vt2;",
];

#[test]
fn view_definitions_follow_renames_through_journal_and_checkpoint_recovery() {
    assert_recovered(
        "view-definition-renames",
        VIEW_SETUP,
        VIEW_PROBE,
        &VIEW_EXPECTED,
    );
}

fn assert_recovered(prefix: &str, setup: &str, probe: &str, expected: &[&str]) {
    for checkpoint in [false, true] {
        let name = format!("{prefix}-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        for statement in setup.split(";\n").filter(|sql| !sql.trim().is_empty()) {
            session.success(&mut engine, statement, false);
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, probe, false)),
            expected,
            "{name}"
        );
        if checkpoint {
            assert!(engine.checkpoint().unwrap());
        }
        drop(session);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(
            data_rows(&session.success(&mut cold, probe, false)),
            expected,
            "{name}"
        );
        drop(session);
        drop(cold);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}

/// `enum_in` names the type as `format_type_be` does, qualified when the
/// search path does not find it.
#[test]
fn enum_input_errors_qualify_types_off_the_search_path() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE SCHEMA es; CREATE TYPE es.e AS ENUM ('a'); CREATE TABLE et (x es.e, t text); \
         INSERT INTO et VALUES ('a', 'c')",
    );
    for statement in ["INSERT INTO et VALUES ('b')", "SELECT t::es.e FROM et"] {
        let output = run_with(&mut engine, &mut budget, statement);
        let text = String::from_utf8_lossy(&output);
        assert!(
            text.contains("invalid input value for enum es.e: \""),
            "{statement}: {text}"
        );
    }
}
