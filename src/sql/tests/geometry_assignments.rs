use super::*;

#[test]
fn geometry_assignment_types_are_checked_before_sources_defaults_and_triggers() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("geometry-assignment-admission-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
        "CREATE SEQUENCE geometry_assignment_effects; \
         CREATE TABLE geometry_assignment_target( \
           id bigint PRIMARY KEY DEFAULT nextval('geometry_assignment_effects'), \
           value point, members point[]); \
         CREATE DOMAIN geometry_assignment_box AS box; \
         CREATE FUNCTION geometry_assignment_trigger() RETURNS trigger LANGUAGE plpgsql AS \
           'BEGIN PERFORM nextval(''geometry_assignment_effects''); RETURN NULL; END'; \
         CREATE TRIGGER geometry_assignment_before BEFORE INSERT OR UPDATE ON geometry_assignment_target \
           FOR EACH STATEMENT EXECUTE FUNCTION geometry_assignment_trigger()", true);
        for sql in [
            "INSERT INTO geometry_assignment_target(value) VALUES(NULL::box)",
            "INSERT INTO geometry_assignment_target(members) VALUES(ARRAY[]::box[])",
            "INSERT INTO geometry_assignment_target(members) VALUES(ARRAY[NULL]::box[])",
            "INSERT INTO geometry_assignment_target(value) VALUES(NULL::geometry_assignment_box)",
            "INSERT INTO geometry_assignment_target(members) VALUES(ARRAY[NULL]::geometry_assignment_box[])",
            "INSERT INTO geometry_assignment_target(id,value) SELECT nextval('geometry_assignment_effects'),NULL::box WHERE false",
            "UPDATE geometry_assignment_target SET value=NULL::box WHERE false",
            "UPDATE geometry_assignment_target SET value=source.value FROM (SELECT NULL::box AS value) source WHERE false",
            "INSERT INTO geometry_assignment_target(value) VALUES(NULL) ON CONFLICT(id) DO UPDATE SET value=NULL::box",
            "INSERT INTO geometry_assignment_target(value) VALUES(NULL) ON CONFLICT(id) DO UPDATE SET value=excluded.members[1]::box",
            "MERGE INTO geometry_assignment_target target USING(SELECT NULL::box AS value) source ON false WHEN NOT MATCHED THEN INSERT(value) VALUES(source.value)",
            "MERGE INTO geometry_assignment_target target USING(SELECT 1 AS id WHERE false) source ON target.id=source.id WHEN MATCHED THEN UPDATE SET members=ARRAY[]::box[]",
            "WITH source AS MATERIALIZED(SELECT nextval('geometry_assignment_effects') AS id,NULL::box AS value) INSERT INTO geometry_assignment_target(id,value) SELECT id,value FROM source",
            "WITH inserted AS(INSERT INTO geometry_assignment_target(value) VALUES(NULL) RETURNING id) INSERT INTO geometry_assignment_target(value) SELECT NULL::box",
        ] {
            let wire = session.execute(&mut engine, sql, true);
            assert_eq!(
                error_sqlstates(&wire),
                ["42804"],
                "{sql}: {}",
                String::from_utf8_lossy(&wire)
            );
        }
        assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT nextval('geometry_assignment_effects'),count(*) FROM geometry_assignment_target", true)), ["1|0"]);
        session.success(&mut engine,
        "CREATE TABLE geometry_assignment_source(value point); \
         CREATE FUNCTION geometry_assignment_copy() RETURNS trigger LANGUAGE plpgsql AS \
           'BEGIN INSERT INTO geometry_assignment_target(value) SELECT NEW.value; RETURN NEW; END'; \
         CREATE TRIGGER geometry_assignment_copy_row AFTER INSERT ON geometry_assignment_source \
           FOR EACH ROW EXECUTE FUNCTION geometry_assignment_copy(); \
         INSERT INTO geometry_assignment_source VALUES('(1,2)')", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT value::text FROM geometry_assignment_target",
                true
            )),
            ["(1,2)"]
        );
    }
}
