use super::*;

#[test]
fn registered_array_casts_keep_shape_null_rules_effects_and_selected_definitions() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("registered-array-casts-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TYPE element_cast_input AS ENUM('a','b'); CREATE SEQUENCE element_cast_calls; \
             CREATE FUNCTION element_cast_box(element_cast_input) RETURNS box LANGUAGE sql VOLATILE \
               SET search_path TO public,pg_catalog AS \
               $$SELECT box(point(nextval('element_cast_calls')::float8,CASE WHEN $1 IS NULL THEN 9 ELSE 1 END))$$; \
             CREATE CAST(element_cast_input AS box) WITH FUNCTION element_cast_box(element_cast_input) AS ASSIGNMENT; \
             CREATE TABLE element_cast_target(value box[])", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT array_dims(value),value[0]::text,value[1]::text,value[2]::text \
               FROM(SELECT '[0:2]={a,NULL,b}'::element_cast_input[]::box[] AS value) converted; \
             SELECT currval('element_cast_calls')",
                true
            )),
            ["[0:2]|(1,1),(1,1)|(2,9),(2,9)|(3,1),(3,1)", "3"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "ALTER SEQUENCE element_cast_calls RESTART WITH 1; \
             SELECT NULL::element_cast_input[]::box[] IS NULL, \
                    array_length(ARRAY[]::element_cast_input[]::box[],1) IS NULL,is_called \
               FROM element_cast_calls",
                true
            )),
            ["t|t|f"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "INSERT INTO element_cast_target VALUES('[0:2]={a,NULL,b}'::element_cast_input[]); \
             SELECT array_dims(value),value[0]::text,value[1]::text,value[2]::text FROM element_cast_target; \
             SELECT currval('element_cast_calls')", true)),
            ["[0:2]|(1,1),(1,1)|(2,9),(2,9)|(3,1),(3,1)", "3"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "ALTER SEQUENCE element_cast_calls RESTART WITH 1; \
             SELECT value[1]::text,value[2]::text FROM(SELECT \
               ARRAY[CASE WHEN nextval('element_cast_calls')=1 THEN 'a'::element_cast_input END, \
                     CASE WHEN nextval('element_cast_calls')=3 THEN 'b'::element_cast_input END]::box[] AS value) converted; \
             SELECT currval('element_cast_calls')", true)), ["(2,1),(2,1)|(4,1),(4,1)", "4"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "CREATE OR REPLACE FUNCTION element_cast_box(element_cast_input) RETURNS box LANGUAGE sql VOLATILE STRICT \
               SET search_path TO public,pg_catalog AS $$SELECT box(point(nextval('element_cast_calls')::float8,1))$$; \
             ALTER SEQUENCE element_cast_calls RESTART WITH 1; \
             SELECT value[0]::text,value[1] IS NULL,value[2]::text \
               FROM(SELECT '[0:2]={a,NULL,b}'::element_cast_input[]::box[] AS value) converted; \
             SELECT currval('element_cast_calls')", true)), ["(1,1),(1,1)|t|(2,1),(2,1)", "2"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "ALTER SEQUENCE element_cast_calls RESTART WITH 1; \
             SELECT array_dims(value),value[1][1]::text,value[1][2]::text FROM(SELECT \
               ARRAY[ARRAY[CASE WHEN nextval('element_cast_calls')=1 THEN 'a'::element_cast_input END, \
                           CASE WHEN nextval('element_cast_calls')=3 THEN 'b'::element_cast_input END]]::box[] AS value) converted; \
             SELECT currval('element_cast_calls')", true)), ["[1:1][1:2]|(2,1),(2,1)|(4,1),(4,1)", "4"]);
        session.success(&mut engine,
            "CREATE FUNCTION element_cast_replacement(element_cast_input) RETURNS box LANGUAGE sql IMMUTABLE \
               AS $$SELECT '(9,9),(9,9)'::box$$; \
             CREATE OR REPLACE FUNCTION element_cast_box(element_cast_input) RETURNS box LANGUAGE plpgsql VOLATILE CALLED ON NULL INPUT AS \
               $$BEGIN PERFORM nextval('element_cast_calls'); DROP CAST(element_cast_input AS box); \
                 CREATE CAST(element_cast_input AS box) WITH FUNCTION element_cast_replacement(element_cast_input) AS ASSIGNMENT; \
                 RETURN '(1,1),(1,1)'::box; END$$; ALTER SEQUENCE element_cast_calls RESTART WITH 1", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT value[1]::text,value[2]::text FROM(SELECT '{a,b}'::element_cast_input[]::box[] AS value) converted; \
             SELECT currval('element_cast_calls'); \
             SELECT value[1]::text,value[2]::text FROM(SELECT '{a,b}'::element_cast_input[]::box[] AS value) converted; \
             SELECT currval('element_cast_calls')", true)),
            ["(1,1),(1,1)|(1,1),(1,1)", "2", "(9,9),(9,9)|(9,9),(9,9)", "2"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "CREATE FUNCTION element_cast_whole(element_cast_input[]) RETURNS box[] LANGUAGE sql VOLATILE CALLED ON NULL INPUT \
               SET search_path TO public,pg_catalog AS $$SELECT ARRAY[box(point(nextval('element_cast_calls')::float8,9))]$$; \
             CREATE CAST(element_cast_input[] AS box[]) WITH FUNCTION element_cast_whole(element_cast_input[]) AS ASSIGNMENT; \
             ALTER SEQUENCE element_cast_calls RESTART WITH 1; \
             SELECT value[1]::text FROM(SELECT NULL::element_cast_input[]::box[] AS value) converted; \
             INSERT INTO element_cast_target VALUES(NULL::element_cast_input[]); \
             SELECT value[1]::text FROM element_cast_target WHERE array_lower(value,1)=1; \
             SELECT currval('element_cast_calls')", true)), ["(1,9),(1,9)", "(2,9),(2,9)", "2"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "UPDATE element_cast_target SET value=NULL::element_cast_input[] WHERE array_lower(value,1)=1; \
             INSERT INTO element_cast_target SELECT NULL::element_cast_input[]; \
             SELECT value[1]::text FROM element_cast_target WHERE array_lower(value,1)=1 ORDER BY value[1]::text; \
             CREATE TABLE element_cast_dml(id integer PRIMARY KEY,value box[]); INSERT INTO element_cast_dml VALUES(1,NULL); \
             MERGE INTO element_cast_dml target USING(VALUES(1)) source(id) ON target.id=source.id \
               WHEN MATCHED THEN UPDATE SET value=NULL::element_cast_input[]; \
             MERGE INTO element_cast_dml target USING(VALUES(2)) source(id) ON target.id=source.id \
               WHEN NOT MATCHED THEN INSERT(id,value) VALUES(source.id,NULL::element_cast_input[]); \
             INSERT INTO element_cast_dml VALUES(1,NULL) ON CONFLICT(id) DO UPDATE SET value=NULL::element_cast_input[]; \
             UPDATE element_cast_dml SET value=NULL::element_cast_input[] WHERE id=2; \
             INSERT INTO element_cast_dml SELECT 3,NULL::element_cast_input[]; \
             INSERT INTO element_cast_dml VALUES(4,NULL::element_cast_input[]); \
             SELECT id,value[1]::text FROM element_cast_dml ORDER BY id; SELECT currval('element_cast_calls')", true)),
            ["(3,9),(3,9)", "(4,9),(4,9)", "1|(7,9),(7,9)", "2|(8,9),(8,9)", "3|(9,9),(9,9)", "4|(10,9),(10,9)", "10"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "CREATE FUNCTION element_cast_text(element_cast_input) RETURNS text LANGUAGE sql IMMUTABLE AS $$SELECT 'converted'$$; \
             CREATE FUNCTION element_cast_install() RETURNS element_cast_input LANGUAGE plpgsql VOLATILE AS \
               $$BEGIN CREATE CAST(element_cast_input AS text) WITH FUNCTION element_cast_text(element_cast_input); \
                 RETURN 'a'::element_cast_input; END$$; \
             SELECT ARRAY[element_cast_install(),'b'::element_cast_input]::text[]; \
             SELECT ARRAY['a'::element_cast_input,'b'::element_cast_input]::text[]", true)), ["{a,b}", "{converted,converted}"]);
        session.success(
            &mut engine,
            "CREATE FUNCTION element_cast_explicit(point[]) RETURNS box[] LANGUAGE sql IMMUTABLE \
               AS $$SELECT ARRAY['(7,7),(7,7)'::box]$$; \
             CREATE CAST(point[] AS box[]) WITH FUNCTION element_cast_explicit(point[]); \
             ALTER SEQUENCE element_cast_calls RESTART WITH 1",
            true,
        );
        for sql in [
            "INSERT INTO element_cast_target VALUES(NULL::point[])",
            "INSERT INTO element_cast_target VALUES(ARRAY[]::point[])",
            "INSERT INTO element_cast_target VALUES(ARRAY[point(nextval('element_cast_calls')::float8,0)])",
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
            "SELECT nextval('element_cast_calls'); SELECT value[1]::text FROM(SELECT NULL::point[]::box[] AS value) converted; \
             DROP CAST(element_cast_input[] AS box[]); DROP CAST(element_cast_input AS box); \
             CREATE CAST(element_cast_input AS box) WITH INOUT AS ASSIGNMENT; \
             SELECT value[1] IS NULL,value[2] IS NULL FROM(SELECT ARRAY[NULL,NULL]::element_cast_input[]::box[] AS value) converted", true)),
            ["1", "(7,7),(7,7)", "t|t"]);
    }
}

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

#[test]
fn view_assignment_plans_keep_public_columns_casts_and_source_scopes() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("view-assignment-contracts-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE view_assignment_ids; CREATE SEQUENCE view_assignment_calls; \
             CREATE SEQUENCE view_assignment_triggers; \
             CREATE TYPE view_assignment_input AS ENUM('x'); \
             CREATE FUNCTION view_assignment_cast(view_assignment_input) RETURNS box \
               LANGUAGE sql VOLATILE CALLED ON NULL INPUT SET search_path TO public,pg_catalog \
               AS $$SELECT box(point(nextval('view_assignment_calls')::float8,9))$$; \
             CREATE CAST(view_assignment_input AS box) WITH FUNCTION view_assignment_cast(view_assignment_input) AS ASSIGNMENT; \
             CREATE TABLE view_assignment_base(id integer DEFAULT nextval('view_assignment_ids'), value box); \
             CREATE VIEW visible_assignment_values(visible_id,visible_value) AS SELECT id,value FROM view_assignment_base; \
             CREATE VIEW visible_assignment_inputs AS SELECT NULL::view_assignment_input AS source_value; \
             CREATE VIEW trigger_assignment_values AS SELECT id AS visible_id,value AS visible_value FROM view_assignment_base; \
             CREATE FUNCTION insert_assignment_view() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
               PERFORM nextval('view_assignment_triggers'); \
               INSERT INTO view_assignment_base VALUES(NEW.visible_id,NEW.visible_value); RETURN NEW; END$$; \
             CREATE TRIGGER insert_assignment_view INSTEAD OF INSERT ON trigger_assignment_values \
               FOR EACH ROW EXECUTE FUNCTION insert_assignment_view()", true);
        for sql in [
            "INSERT INTO visible_assignment_values(visible_value) VALUES(NULL::line)",
            "INSERT INTO trigger_assignment_values(visible_id,visible_value) SELECT 99,NULL::line",
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, sql, true)),
                ["42804"],
                "{sql}"
            );
        }
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT is_called FROM view_assignment_ids; SELECT is_called FROM view_assignment_calls; \
             SELECT is_called FROM view_assignment_triggers", true)), ["f", "f", "f"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "INSERT INTO visible_assignment_values(visible_value) VALUES(NULL::view_assignment_input); \
             UPDATE visible_assignment_values AS exposed SET visible_value=NULL::view_assignment_input WHERE exposed.visible_id=1; \
             INSERT INTO visible_assignment_values(visible_value) SELECT source_value FROM visible_assignment_inputs; \
             INSERT INTO trigger_assignment_values(visible_id,visible_value) SELECT 99,NULL::view_assignment_input; \
             WITH source AS(SELECT NULL::view_assignment_input AS input) \
               UPDATE visible_assignment_values AS exposed SET visible_value=source.input FROM source WHERE exposed.visible_id=2; \
             SELECT id,value::text FROM view_assignment_base ORDER BY id; \
             SELECT currval('view_assignment_calls'),currval('view_assignment_triggers')", true)),
            ["1|(2,9),(2,9)", "2|(5,9),(5,9)", "99|(4,9),(4,9)", "5|1"]);
    }
}
