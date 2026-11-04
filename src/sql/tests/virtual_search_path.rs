use super::*;

#[test]
fn virtual_search_path_names_preserve_explicit_order_and_creation_target() {
    let config = test_config("virtual-search-path-order");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SCHEMA path_before; CREATE SCHEMA path_after",
        true,
    );
    let rows = session.success(
        &mut engine,
        "SET search_path=path_before,information_schema,path_after,pg_catalog; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text; \
         SET search_path=information_schema,path_before,information_schema,missing_schema; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text; \
         SET search_path=information_schema; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text; \
         SET search_path=pg_toast,public; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text; \
         SET search_path=missing_schema; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text; \
         SET search_path=\"INFORMATION_SCHEMA\",path_before; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text",
        true,
    );
    assert_eq!(
        data_rows(&rows),
        [
            "path_before|{path_before,information_schema,path_after,pg_catalog}|{path_before,information_schema,path_after,pg_catalog}",
            "information_schema|{information_schema,path_before}|{pg_catalog,information_schema,path_before}",
            "information_schema|{information_schema}|{pg_catalog,information_schema}",
            "pg_toast|{pg_toast,public}|{pg_catalog,pg_toast,public}",
            "NULL|{}|{pg_catalog}",
            "path_before|{path_before}|{pg_catalog,path_before}",
        ]
    );

    crate::mem::guard::forbid_alloc(|| {
        let selected = engine.storage.compute_path(
            "path_before,information_schema,path_after,pg_catalog",
            "postgres",
            0,
        );
        let previous = engine.storage.swap_path(selected);
        assert!(crate::storage::information_schema_on_path());
        assert_eq!(
            engine.storage.path_namespace_rank("path_before", 0),
            Some(0)
        );
        assert_eq!(
            engine.storage.path_namespace_rank("information_schema", 0),
            Some(1)
        );
        assert_eq!(engine.storage.path_namespace_rank("path_after", 0), Some(2));
        assert_eq!(engine.storage.path_namespace_rank("pg_catalog", 0), Some(3));
        assert_eq!(
            engine.storage.path_namespace_rank("missing_schema", 0),
            None
        );
        engine.storage.swap_path(previous);
    });
}

#[test]
fn virtual_relations_and_user_types_follow_the_selected_namespace() {
    let config = test_config("virtual-search-path-relations");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SCHEMA path_before; CREATE TABLE public.path_visible(id integer); \
         CREATE TABLE path_before.tables(label text); INSERT INTO path_before.tables VALUES('user table'); \
         CREATE TABLE path_before.pg_class(marker integer); INSERT INTO path_before.pg_class VALUES(17); \
         CREATE DOMAIN path_before.path_number AS integer; \
         SET search_path=information_schema,path_before,public", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT table_name FROM tables WHERE table_schema='public' AND table_name='path_visible'; \
         SELECT column_name FROM columns WHERE table_schema='public' AND table_name='path_visible' ORDER BY ordinal_position; \
         SELECT (7::path_number)::integer; \
         CREATE VIEW public.path_bound AS SELECT 9::path_number AS value; SELECT value::integer FROM public.path_bound; \
         SET search_path=path_before,information_schema,public; SELECT label FROM tables; \
         SET search_path=path_before,pg_catalog,information_schema; SELECT marker FROM pg_class; \
         SELECT relname FROM pg_catalog.pg_class WHERE relname='path_visible'", true)),
        ["path_visible", "id", "7", "9", "user table", "17", "path_visible"]);
}

#[test]
fn read_only_information_schema_never_silently_creates_in_a_later_schema() {
    let config = test_config("virtual-search-path-creation");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE path_unprivileged; GRANT USAGE,CREATE ON SCHEMA public TO path_unprivileged; \
         SET search_path=information_schema,public",
        true,
    );
    for statement in [
        "CREATE TABLE virtual_owner_target(id integer)",
        "CREATE TABLE information_schema.virtual_owner_target(id integer)",
        "CREATE TYPE virtual_owner_enum AS ENUM ('one')",
        "CREATE TYPE information_schema.virtual_owner_enum AS ENUM ('one')",
    ] {
        let output = session.execute(&mut engine, statement, true);
        assert_eq!(error_sqlstates(&output), ["0A000"]);
        assert!(String::from_utf8_lossy(&output).contains(
            "creating objects in read-only virtual schema \"information_schema\" is not supported"
        ));
    }
    session.success(&mut engine, "SET ROLE path_unprivileged", true);
    for statement in [
        "CREATE TABLE virtual_denied_target(id integer)",
        "CREATE TABLE information_schema.virtual_denied_target(id integer)",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, statement, true)),
            ["42501"]
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SET search_path=pg_toast,public; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text; \
         CREATE TABLE visible_after_toast(id integer); SET search_path=pg_toast; \
         SELECT current_schema(),current_schemas(false)::text,current_schemas(true)::text",
            true
        )),
        [
            "public|{public}|{pg_catalog,public}",
            "NULL|{}|{pg_catalog}"
        ]
    );
    assert!(
        engine
            .storage
            .find_table("public", "visible_after_toast")
            .is_some()
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE TABLE no_visible_schema(id integer)",
            true
        )),
        ["3F000"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE TYPE pg_toast.virtual_denied_enum AS ENUM ('one')",
            true
        )),
        ["42501"]
    );
    session.success(
        &mut engine,
        "RESET ROLE; SET search_path=pg_catalog,public",
        true,
    );
    for statement in [
        "CREATE TABLE virtual_catalog_target(id integer)",
        "CREATE VIEW virtual_catalog_view AS SELECT 1 AS value",
        "CREATE SEQUENCE virtual_catalog_sequence",
        "CREATE TYPE virtual_catalog_composite AS (value integer)",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, statement, true)),
            ["42501"]
        );
    }
    for statement in [
        "CREATE TYPE virtual_catalog_enum AS ENUM ('one')",
        "CREATE FUNCTION virtual_catalog_function() RETURNS integer LANGUAGE sql AS $$ SELECT 1 $$",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, statement, true)),
            ["0A000"]
        );
    }
    session.success(&mut engine, "SET search_path=pg_toast,public", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE TABLE virtual_toast_target(id integer)",
            true
        )),
        ["42501"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE TYPE virtual_toast_enum AS ENUM ('one')",
            true
        )),
        ["0A000"]
    );
    for name in [
        "virtual_owner_target",
        "virtual_denied_target",
        "virtual_catalog_target",
    ] {
        assert!(engine.storage.find_table("public", name).is_none());
    }
}

#[test]
fn unsupported_virtual_relation_oids_never_bind_a_later_user_relation() {
    let config = test_config("virtual-search-path-relation-oids");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SCHEMA path_user; CREATE TABLE path_user.tables(id integer); \
         CREATE INDEX columns ON path_user.tables(id); \
         SET search_path=information_schema,path_user,public",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT pg_table_is_visible('path_user.tables'::regclass),pg_table_is_visible('path_user.columns'::regclass); \
         SELECT to_regclass('information_schema.no_such_relation') IS NULL", true)), ["f|f", "t"]);
    for statement in [
        "SELECT 'tables'::regclass",
        "SELECT to_regclass('tables')",
        "SELECT 'information_schema.tables'::regclass",
        "SELECT to_regclass('information_schema.tables')",
        "SELECT has_table_privilege('information_schema.tables','SELECT')",
        "SELECT pg_get_serial_sequence('tables','id')",
    ] {
        let output = session.execute(&mut engine, statement, true);
        assert_eq!(error_sqlstates(&output), ["0A000"]);
        assert!(
            String::from_utf8_lossy(&output)
                .contains("relation OIDs for information_schema views are not supported")
        );
    }
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT 'information_schema.no_such_relation'::regclass",
            true
        )),
        ["42P01"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SET search_path=path_user,information_schema,public; \
         SELECT 'tables'::regclass='path_user.tables'::regclass, \
                to_regclass('tables')='path_user.tables'::regclass, \
                pg_table_is_visible('path_user.tables'::regclass), \
                pg_table_is_visible('path_user.columns'::regclass), \
                pg_get_serial_sequence('tables','id') IS NULL",
            true
        )),
        ["t|t|t|t|t"]
    );
}
