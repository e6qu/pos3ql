//! SQL-standard routine bodies (`RETURN`, `BEGIN ATOMIC`) are bound to the
//! routines and types they named at CREATE: renaming or moving those objects
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
    for checkpoint in [false, true] {
        let name = format!("standard-body-renames-{checkpoint}");
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
        for statement in SETUP.split(";\n").filter(|sql| !sql.trim().is_empty()) {
            session.success(&mut engine, statement, false);
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, PROBE, false)),
            EXPECTED,
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
            data_rows(&session.success(&mut cold, PROBE, false)),
            EXPECTED,
            "{name}"
        );
        drop(session);
        drop(cold);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}
