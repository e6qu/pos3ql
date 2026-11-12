use super::*;

fn rows(engine: &mut Engine, budget: &mut Budget, sql: &str) -> Vec<String> {
    let output = run_with(engine, budget, sql);
    assert!(
        error_sqlstates(&output).is_empty(),
        "{sql}: {}",
        String::from_utf8_lossy(&output)
    );
    data_rows(&output)
}

/// `pg_shdepend` is shared: it lists owners and grantees of objects in every
/// database with that database's OID, and `DROP ROLE` counts them.
#[test]
fn shared_dependencies_include_other_databases() {
    let (mut engine, mut budget) = test_engine();
    rows(
        &mut engine,
        &mut budget,
        "CREATE ROLE other_owner; CREATE ROLE other_reader",
    );
    rows(
        &mut engine,
        &mut budget,
        "CREATE DATABASE other_db TEMPLATE template0",
    );
    let current = engine.storage.current_database_oid();
    let slot = engine.storage.database_slot("other_db", 0).unwrap();
    let other = engine.storage.database(slot).oid;
    engine.select_database(other).unwrap();
    rows(
        &mut engine,
        &mut budget,
        "CREATE TABLE owned(a int); ALTER TABLE owned OWNER TO other_owner; \
         GRANT SELECT ON owned TO other_reader; \
         CREATE SCHEMA owned_schema AUTHORIZATION other_owner",
    );
    engine.select_database(current).unwrap();
    assert_eq!(
        rows(
            &mut engine,
            &mut budget,
            "SELECT d.datname, s.classid::regclass, s.refobjid::regrole, s.deptype \
             FROM pg_shdepend s JOIN pg_database d ON d.oid = s.dbid \
             WHERE s.refobjid IN ('other_owner'::regrole, 'other_reader'::regrole) \
             ORDER BY 1, 2, 3, 4"
        ),
        [
            "other_db|pg_class|other_owner|o",
            "other_db|pg_class|other_reader|a",
            "other_db|pg_namespace|other_owner|o",
        ]
    );
    let refused = run_with(&mut engine, &mut budget, "DROP ROLE other_owner");
    let text = String::from_utf8_lossy(&refused);
    assert_eq!(error_sqlstates(&refused), ["2BP01"], "{text}");
    assert!(text.contains("2 objects in database other_db"), "{text}");
}
