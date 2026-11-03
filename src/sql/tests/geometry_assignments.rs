use super::*;

// PostgreSQL 18.6 reference SQL SHA256:
// 9d652291010b05af1db990218c988f1290d3d42bc115096c8717858096b8430d.
#[test]
fn named_array_folding_retains_empty_constructor_context() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("named-array-folding-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TYPE fold_enum AS ENUM('a','b'); CREATE DOMAIN fold_domain AS fold_enum CHECK(VALUE<>'b')", true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT array_length(ARRAY[]::fold_enum[],1) IS NULL, \
                    array_length(ARRAY[ARRAY[]]::fold_enum[],1) IS NULL, \
                    array_length(ARRAY[]::fold_domain[],1) IS NULL, \
                    array_length(ARRAY[ARRAY[]]::fold_domain[],1) IS NULL",
                true
            )),
            ["t|t|t|t"]
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "SELECT ARRAY['missing']::fold_enum[] WHERE false",
                true
            )),
            ["22P02"]
        );
    }
}

// PostgreSQL 18.6 reference SQL SHA256:
// f6e98e8dcf3eebd33c10b2f41dad32b3d6634609e977b9475174de50ea0dfb72.
#[test]
fn array_valued_children_convert_before_later_source_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("array-child-conversion-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE SEQUENCE array_child_calls; \
             CREATE FUNCTION array_bad_child() RETURNS bigint[] LANGUAGE sql VOLATILE \
               SET search_path TO public,pg_catalog AS $$SELECT ARRAY[2147483648::bigint]$$; \
             CREATE FUNCTION array_good_child() RETURNS bigint[] LANGUAGE sql VOLATILE \
               SET search_path TO public,pg_catalog AS $$SELECT ARRAY[7::bigint]$$",
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "SELECT ARRAY[array_bad_child(),ARRAY[nextval('array_child_calls')]]::integer[]",
                true
            )),
            ["22003"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_called FROM array_child_calls; \
             SELECT ARRAY[array_good_child(),ARRAY[nextval('array_child_calls')]]::integer[]; \
             SELECT currval('array_child_calls')",
                true
            )),
            ["f", "{{7},{1}}", "1"]
        );
    }
}

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

// PostgreSQL 18.6 reference SQL SHA256s:
// 937118941cd4013222a80aef69e00d0d8c79e80c0800eca30b7241b04f48e0f6;
// 27bc473d8424e34b8ddf56427e6106beb972b8bffda116f576b0da290889776a.
#[test]
fn insert_select_assignment_casts_follow_source_projection_order_once() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("select-assignment-order-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TYPE select_cast_input AS ENUM('a'); CREATE SEQUENCE select_cast_calls; \
             CREATE FUNCTION select_cast_source(integer) RETURNS select_cast_input LANGUAGE sql VOLATILE \
               SET search_path TO public,pg_catalog AS \
               $$SELECT CASE WHEN nextval('select_cast_calls')>0 THEN 'a'::select_cast_input END$$; \
             CREATE FUNCTION select_cast_box(select_cast_input) RETURNS box LANGUAGE sql VOLATILE \
               SET search_path TO public,pg_catalog AS $$SELECT box(point(nextval('select_cast_calls')::float8,9))$$; \
             CREATE CAST(select_cast_input AS box) WITH FUNCTION select_cast_box(select_cast_input) AS ASSIGNMENT; \
             CREATE TABLE select_cast_source_rows(id integer); INSERT INTO select_cast_source_rows VALUES(2),(1),(3); \
             CREATE TABLE select_cast_target(id integer,value box)", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "INSERT INTO select_cast_target SELECT id,select_cast_source(id) FROM select_cast_source_rows; \
             SELECT id,value::text FROM select_cast_target ORDER BY id; SELECT currval('select_cast_calls')", true)),
            ["1|(4,9),(4,9)", "2|(2,9),(2,9)", "3|(6,9),(6,9)", "6"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "TRUNCATE select_cast_target; ALTER SEQUENCE select_cast_calls RESTART WITH 1; \
             INSERT INTO select_cast_target SELECT id,select_cast_source(id) FROM select_cast_source_rows ORDER BY id LIMIT 2; \
             SELECT id,value::text FROM select_cast_target ORDER BY id; SELECT currval('select_cast_calls')", true)),
            ["1|(2,9),(2,9)", "2|(4,9),(4,9)", "4"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "TRUNCATE select_cast_target; ALTER SEQUENCE select_cast_calls RESTART WITH 1; \
             INSERT INTO select_cast_target SELECT id,select_cast_source(id) AS value FROM select_cast_source_rows ORDER BY value,id LIMIT 2; \
             SELECT id,value::text FROM select_cast_target ORDER BY id; SELECT currval('select_cast_calls')", true)),
            ["1|(4,9),(4,9)", "2|(5,9),(5,9)", "5"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "CREATE TABLE select_cast_pair(first_value box,second_value box); ALTER SEQUENCE select_cast_calls RESTART WITH 1; \
             INSERT INTO select_cast_pair SELECT select_cast_source(id),select_cast_source(id) FROM select_cast_source_rows; \
             SELECT first_value::text,second_value::text FROM select_cast_pair; SELECT currval('select_cast_calls')", true)),
            ["(3,9),(3,9)|(4,9),(4,9)", "(7,9),(7,9)|(8,9),(8,9)", "(11,9),(11,9)|(12,9),(12,9)", "12"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "TRUNCATE select_cast_pair; ALTER SEQUENCE select_cast_calls RESTART WITH 1; \
             INSERT INTO select_cast_pair SELECT select_cast_source(id),select_cast_source(id) FROM select_cast_source_rows ORDER BY id LIMIT 2; \
             SELECT first_value::text,second_value::text FROM select_cast_pair; SELECT currval('select_cast_calls')", true)),
            ["(3,9),(3,9)|(4,9),(4,9)", "(7,9),(7,9)|(8,9),(8,9)", "8"]);
        session.success(&mut engine, "CREATE SEQUENCE select_cast_width_calls", true);
        assert_eq!(error_sqlstates(&session.execute(&mut engine,
            "INSERT INTO select_cast_target(id,value) SELECT nextval('select_cast_width_calls')::integer", true)), ["42601"]);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT is_called FROM select_cast_width_calls; INSERT INTO select_cast_target SELECT 99; \
             SELECT id,value IS NULL FROM select_cast_target WHERE id=99", true)), ["f", "99|t"]);
        session.success(&mut engine,
            "CREATE TABLE select_cast_fire(id integer); \
             CREATE FUNCTION select_cast_trigger() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
               INSERT INTO select_cast_target SELECT id,select_cast_source(id) FROM select_cast_source_rows; RETURN NEW; END$$; \
             CREATE TRIGGER select_cast_trigger AFTER INSERT ON select_cast_fire FOR EACH ROW EXECUTE FUNCTION select_cast_trigger()", true);
        for dynamic in [false, true] {
            if dynamic {
                session.success(&mut engine,
                    "CREATE OR REPLACE FUNCTION select_cast_trigger() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN \
                       EXECUTE 'INSERT INTO select_cast_target SELECT id,select_cast_source(id) FROM select_cast_source_rows'; RETURN NEW; END$$", true);
            }
            assert_eq!(data_rows(&session.success(&mut engine,
                "TRUNCATE select_cast_target; ALTER SEQUENCE select_cast_calls RESTART WITH 1; INSERT INTO select_cast_fire VALUES(1); \
                 SELECT id,value::text FROM select_cast_target ORDER BY id; SELECT currval('select_cast_calls')", true)),
                ["1|(4,9),(4,9)", "2|(2,9),(2,9)", "3|(6,9),(6,9)", "6"]);
        }
    }
}

#[test]
fn projection_type_visits_keep_selected_cast_snapshots() {
    use crate::sql::eval::CatalogAccess as _;
    let config = test_config("projection-type-scratch");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TYPE projection_cast_input AS ENUM('a'); \
         CREATE FUNCTION projection_cast_box(projection_cast_input) RETURNS box LANGUAGE sql IMMUTABLE \
           CALLED ON NULL INPUT AS $$SELECT '(7,9),(7,9)'::box$$; \
         CREATE CAST(projection_cast_input AS box) WITH FUNCTION projection_cast_box(projection_cast_input) AS ASSIGNMENT; \
         CREATE VIEW projection_type_input AS SELECT NULL::projection_cast_input AS value", true);
    let arena = Arena::new(&mut budget, "projection type scratch", 64 << 10).unwrap();
    let select =
        crate::sql::parser::parse_query("SELECT value FROM projection_type_input", &arena).unwrap();
    let catalog = query::storage_catalog(&engine.storage, &arena, 0);
    crate::mem::guard::forbid_alloc(|| {
        let mut selected = None;
        query::visit_projection_types(select, &engine.storage, 0, &arena, None, &mut |source| {
            let cast = catalog
                .bind_defined_cast(
                    source,
                    ColType::Geometry(crate::sql::types::GeometryKind::Box).oid(),
                    crate::sql::eval::DefinedCastContext::Assignment,
                )?
                .expect("registered cast");
            selected = Some(
                &arena
                    .alloc_persistent_slice_with(1, |_| cast)
                    .map_err(|_| crate::sql::eval::arena_full())?[0],
            );
            Ok(())
        })
        .unwrap();
        arena.alloc_slice_with(8192, |_| 0xcc_u8).unwrap();
        let value = catalog
            .execute_bound_cast(selected.expect("retained cast"), Datum::Null, &arena)
            .unwrap();
        assert!(matches!(
            value,
            Datum::Geometry {
                kind: crate::sql::types::GeometryKind::Box,
                text: "(7,9),(7,9)"
            }
        ));
    });
}
