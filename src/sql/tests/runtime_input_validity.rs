use super::*;

#[test]
fn ordinary_casts_do_not_reserve_checked_input_request_storage() {
    let mut config = test_config("ordinary-cast-request-capacity");
    config.prepared_bytes = 1024;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql = format!("SELECT length('{}'::text)", "x".repeat(10_000));
    assert!(sql.len() > config.prepared_bytes);
    assert_eq!(
        data_rows(&session.success(&mut engine, &sql, true)),
        ["10000"]
    );
    assert_eq!(session.transaction.mode, TxnMode::Idle);
    assert!(session.pool.request_input_progress().is_none());
}

#[test]
fn planner_constant_classification_releases_nested_query_scratch() {
    let config = test_config("planner-nested-query-scratch");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session =
        ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 256 << 10);
    session.success(
        &mut engine,
        "CREATE TABLE bounded_subquery_rows(value integer); INSERT INTO bounded_subquery_rows VALUES(1),(2)",
        true,
    );
    let cases = [
        ("SELECT CASE WHEN (SELECT 1)=1 THEN 7 ELSE 9 END", vec!["7"]),
        ("SELECT CASE (SELECT 1) WHEN 1 THEN 7 ELSE 9 END", vec!["7"]),
        (
            "SELECT value,CASE WHEN (SELECT value)=1 THEN 'top' ELSE 'other' END FROM bounded_subquery_rows ORDER BY value",
            vec!["1|top", "2|other"],
        ),
    ];
    let arena = Arena::new(&mut budget, "nested planner query scratch", 256 << 10).unwrap();
    for (sql, expected) in cases {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            expected
        );
        crate::mem::guard::forbid_alloc(|| {
            let statement = parser::parse_stored_statement(sql, &arena).unwrap();
            let retained = arena.used();
            for _ in 0..64 {
                crate::sql::planner_inputs::capture_planner_inputs(
                    statement,
                    None,
                    &engine.storage,
                    0,
                    &arena,
                )
                .unwrap();
                assert_eq!(arena.used(), retained);
            }
        });
    }
}

#[test]
fn runtime_checked_inputs_follow_selected_definitions_and_replace_changed_paths() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("runtime-input-validity-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SCHEMA path_alternative; CREATE SEQUENCE validity_checks; \
             CREATE TYPE validity_choice AS ENUM('a'); \
             CREATE FUNCTION validity_allowed(value validity_choice) RETURNS boolean LANGUAGE plpgsql \
             AS $$BEGIN PERFORM nextval('validity_checks'); RETURN true; END$$; \
             CREATE DOMAIN validity_checked AS validity_choice CHECK(validity_allowed(VALUE)); \
             CREATE TABLE validity_anchor(id integer); INSERT INTO validity_anchor VALUES(1); \
             CREATE TABLE validity_unrelated(id integer); \
             CREATE FUNCTION validity_body() RETURNS validity_checked[] LANGUAGE sql SECURITY DEFINER \
             AS 'SELECT ''{a}''::public.validity_checked[] FROM public.validity_anchor LIMIT 1'", true);
        let counter = "SELECT last_value FROM public.validity_checks";
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["1"]
        );
        for expected in ["2", "2"] {
            session.success(&mut engine, "SELECT public.validity_body()", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(&mut engine, "INSERT INTO validity_anchor VALUES(2); ALTER TABLE validity_unrelated ADD COLUMN ignored text", true);
        session.success(&mut engine, "SELECT public.validity_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["2"]
        );
        session.success(
            &mut engine,
            "ALTER TABLE validity_anchor ADD COLUMN added text",
            true,
        );
        session.success(&mut engine, "SELECT public.validity_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["3"]
        );
        for (path, expected) in [("public,path_alternative", "4"), ("public", "5")] {
            session.success(&mut engine, &format!("SET search_path={path}"), true);
            session.success(&mut engine, "SELECT public.validity_body()", true);
            assert_eq!(
                data_rows(&session.success(&mut engine, counter, true)),
                [expected]
            );
        }
        session.success(&mut engine,
            "CREATE FUNCTION validity_outer() RETURNS integer LANGUAGE sql \
             SET search_path=public,path_alternative AS 'SELECT cardinality(public.validity_body())'", true);
        session.success(&mut engine, "SELECT validity_outer()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["6"]
        );
        session.success(&mut engine, "SELECT validity_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["7"]
        );
        session.success(&mut engine,
            "CREATE VIEW validity_view AS SELECT id FROM validity_anchor; \
             CREATE FUNCTION validity_view_body() RETURNS validity_checked[] LANGUAGE sql SECURITY DEFINER \
             AS 'SELECT ''{a}''::public.validity_checked[] FROM public.validity_view LIMIT 1'", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["8"]
        );
        session.success(&mut engine, "SELECT validity_view_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["9"]
        );
        session.success(
            &mut engine,
            "ALTER TABLE validity_anchor ADD COLUMN through_view text",
            true,
        );
        session.success(&mut engine, "SELECT validity_view_body()", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, counter, true)),
            ["10"]
        );
        drop((session, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn selected_operator_and_cast_functions_participate_in_runtime_plan_validity() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("support-function-validity-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE FUNCTION selected_operator(integer,integer) RETURNS integer LANGUAGE sql AS $$SELECT $1+$2$$; \
             CREATE OPERATOR public.##(LEFTARG=integer,RIGHTARG=integer,FUNCTION=selected_operator); \
             CREATE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'x'::varchar$$; \
             CREATE CAST(integer AS varchar) WITH FUNCTION selected_cast(integer,integer,boolean) AS IMPLICIT; \
             CREATE FUNCTION consume_varchar(varchar) RETURNS integer LANGUAGE sql AS $$SELECT 1$$; \
             CREATE FUNCTION unselected_support() RETURNS integer LANGUAGE sql AS $$SELECT 1$$; \
             CREATE TABLE selected_destination(value varchar(5),items varchar(5)[])", true);
        let arena =
            Arena::new(&mut budget, "support function dependency metadata", 2 << 20).unwrap();
        for (statement, support, replacement) in [
            (
                "SELECT 1 OPERATOR(public.##) 2",
                "selected_operator",
                "CREATE OR REPLACE FUNCTION selected_operator(integer,integer) RETURNS integer LANGUAGE sql AS $$SELECT $1-$2$$",
            ),
            (
                "SELECT 7::varchar(5)",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'a'::varchar$$",
            ),
            (
                "SELECT ARRAY[7,8]::varchar(5)[]",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'b'::varchar$$",
            ),
            (
                "SELECT consume_varchar(7)",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'c'::varchar$$",
            ),
            (
                "INSERT INTO selected_destination VALUES(7,ARRAY[7,8])",
                "selected_cast",
                "CREATE OR REPLACE FUNCTION selected_cast(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS $$SELECT 'd'::varchar$$",
            ),
        ] {
            let validity = crate::mem::guard::forbid_alloc(|| {
                let statement = parser::parse_stored_statement(statement, &arena).unwrap();
                let mut dependencies = engine
                    .storage
                    .empty_stored_query_dependencies(&arena)
                    .unwrap();
                query::collect_runtime_statement(
                    statement,
                    &engine.storage,
                    session.transaction.txid,
                    &mut dependencies,
                    &arena,
                    None,
                )
                .unwrap();
                assert!(
                    dependencies.entries().iter().any(|entry| {
                        entry.class == crate::storage::DependencyClass::Routine
                            && entry.name.as_str() == support
                    }),
                    "{statement:?}: {support}"
                );
                engine
                    .storage
                    .capture_runtime_input_validity(&dependencies, &session.transaction, None)
                    .unwrap()
            });
            assert!(
                validity
                    .matches(&engine.storage, session.transaction.txid)
                    .unwrap()
            );
            session.success(&mut engine,
                "CREATE OR REPLACE FUNCTION unselected_support() RETURNS integer LANGUAGE sql AS $$SELECT 2$$", true);
            assert!(
                validity
                    .matches(&engine.storage, session.transaction.txid)
                    .unwrap(),
                "unrelated replacement: {statement}"
            );
            session.success(&mut engine, replacement, true);
            assert!(
                !validity
                    .matches(&engine.storage, session.transaction.txid)
                    .unwrap(),
                "selected replacement: {statement}"
            );
        }
    }
}

#[test]
fn enum_filters_and_repeated_dependency_visits_fit_a_small_statement_arena() {
    let config = test_config("enum-query-bounded-metadata");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut setup = ConfiguredTransactionSession::new(&config, &mut budget);
    setup.success(&mut engine,
        "CREATE TYPE saved_state AS ENUM('old','ready','high'); \
         CREATE TYPE saved_detail AS(state saved_state,note text); \
         CREATE TABLE saved_enum_rows(id integer PRIMARY KEY,state saved_state UNIQUE,detail saved_detail,states saved_state[]); \
         INSERT INTO saved_enum_rows VALUES(1,'old',ROW('old','old')::saved_detail,'{old}'),(2,'high',ROW('high','high')::saved_detail,'{high}')", true);
    for member in 0..30 {
        setup.success(
            &mut engine,
            &format!("ALTER TYPE saved_state ADD VALUE 'middle_{member}' BEFORE 'high'"),
            true,
        );
    }
    let sql = "SELECT 'enum_high_retained' FROM saved_enum_rows WHERE state='high'::saved_state AND state>'middle_29'::saved_state";
    let mut bounded =
        ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 256 << 10);
    for _ in 0..8 {
        assert_eq!(
            data_rows(&bounded.success(&mut engine, sql, true)),
            ["enum_high_retained"]
        );
    }
    let arena = Arena::new(&mut budget, "dependency visit scratch", 256 << 10).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let statement = parser::parse_stored_statement(sql, &arena).unwrap();
        let mut dependencies = engine
            .storage
            .empty_stored_query_dependencies(&arena)
            .unwrap();
        let retained = arena.used();
        for _ in 0..64 {
            crate::sql::planner_inputs::capture_planner_inputs(
                statement,
                None,
                &engine.storage,
                0,
                &arena,
            )
            .unwrap();
            assert_eq!(
                arena.used(),
                retained,
                "planner discovery without candidates must release its scratch"
            );
            query::collect_runtime_statement(
                statement,
                &engine.storage,
                0,
                &mut dependencies,
                &arena,
                None,
            )
            .unwrap();
            assert_eq!(
                arena.used(),
                retained,
                "schema discovery must release its scratch"
            );
        }
        assert!(
            dependencies
                .entries()
                .iter()
                .any(|entry| entry.class == crate::storage::DependencyClass::Table)
        );
        assert!(
            dependencies
                .entries()
                .iter()
                .any(|entry| entry.class == crate::storage::DependencyClass::Enum)
        );
    });
}

#[test]
fn dependency_columns_keep_inner_sql_precedence_and_resolve_outer_locals() {
    struct Locals;
    impl crate::sql::exec::ColTypeResolver for Locals {
        fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
            if qualifier.is_none() && matches!(name, "label" | "id" | "keep" | "max_rows") {
                Ok(match name {
                    "label" => ColType::Text,
                    "keep" => ColType::Bool,
                    _ => ColType::Int4,
                })
            } else {
                Err(crate::sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "unknown outer column"
                ))
            }
        }
    }
    let config = test_config("dependency-outer-columns");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE input_rows(id integer)", true);
    let arena = Arena::new(&mut budget, "outer column dependency scratch", 256 << 10).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for source in [
            "SELECT id,label FROM input_rows WHERE keep LIMIT max_rows",
            "UPDATE input_rows SET id=id+max_rows WHERE keep RETURNING label",
            "DELETE FROM input_rows WHERE id=max_rows AND keep RETURNING id,label",
            "INSERT INTO input_rows VALUES(max_rows) RETURNING id,label",
        ] {
            let statement = parser::parse_stored_statement(source, &arena).unwrap();
            let mut dependencies = engine
                .storage
                .empty_stored_query_dependencies(&arena)
                .unwrap();
            query::collect_runtime_statement(
                statement,
                &engine.storage,
                0,
                &mut dependencies,
                &arena,
                Some(&Locals),
            )
            .unwrap_or_else(|error| panic!("{source}: {error:?}"));
            let table = dependencies
                .entries()
                .iter()
                .find(|entry| entry.class == crate::storage::DependencyClass::Table)
                .unwrap();
            assert!(
                table.referenced_columns.contains(0),
                "inner id remains a table dependency"
            );
        }
    });
}

#[test]
fn dependency_analysis_preserves_record_field_errors_with_and_without_relation_scope() {
    let config = test_config("dependency-record-field-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE input_rows(id integer)", true);
    let arena = Arena::new(&mut budget, "record dependency errors", 256 << 10).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for source in [
            "SELECT (ROW(1)).f2::integer",
            "SELECT (ROW(1)).f2::integer FROM input_rows",
            "UPDATE input_rows SET id=(ROW(1)).f2::integer",
        ] {
            let statement = parser::parse_stored_statement(source, &arena).unwrap();
            let mut dependencies = engine
                .storage
                .empty_stored_query_dependencies(&arena)
                .unwrap();
            let error = query::collect_runtime_statement(
                statement,
                &engine.storage,
                0,
                &mut dependencies,
                &arena,
                None,
            )
            .unwrap_err();
            assert_eq!(error.sqlstate, sqlstate::UNDEFINED_COLUMN, "{source}");
            assert_eq!(
                error.message.as_str(),
                "could not identify column \"f2\" in record data type",
                "{source}"
            );
        }
    });
}

#[test]
fn cte_dependency_capture_uses_bound_sources_without_running_modifications() {
    let config = test_config("cte-dependency-source-scopes");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE cte_source(id integer); CREATE TABLE cte_target(id integer); \
         INSERT INTO cte_source VALUES(1); \
         CREATE AGGREGATE public.sum(integer)(SFUNC=pg_catalog.int4pl,STYPE=integer,INITCOND='0')",
        true,
    );
    let arena = Arena::new(&mut budget, "CTE dependency metadata", 2 << 20).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for source in [
            "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<4) INSERT INTO cte_target SELECT n FROM numbers",
            "WITH moved AS (DELETE FROM cte_source RETURNING id), later AS (SELECT id+1 AS id FROM moved) INSERT INTO cte_target SELECT id FROM later",
            "SELECT public.sum(id) FROM cte_source",
        ] {
            let statement = parser::parse_stored_statement(source, &arena).unwrap();
            let mut dependencies = engine
                .storage
                .empty_stored_query_dependencies(&arena)
                .unwrap();
            query::collect_runtime_statement(
                statement,
                &engine.storage,
                0,
                &mut dependencies,
                &arena,
                None,
            )
            .unwrap();
            assert!(
                dependencies
                    .entries()
                    .iter()
                    .any(|entry| entry.class == crate::storage::DependencyClass::Table)
            );
            if source.starts_with("SELECT") {
                assert!(dependencies.entries().iter().any(|entry| entry.class
                    == crate::storage::DependencyClass::Routine
                    && entry.name.as_str() == "sum"));
            }
        }
    });
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*) FROM cte_source; SELECT count(*) FROM cte_target",
            true
        )),
        ["1", "0"]
    );
}

#[test]
fn set_query_metadata_keeps_shared_cte_shapes_without_running_sources() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("set-query-cte-metadata-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE set_query_source(id integer); INSERT INTO set_query_source VALUES(1),(2); \
             CREATE SEQUENCE set_query_effects; \
             CREATE FUNCTION set_query_identity(integer) RETURNS integer LANGUAGE sql AS $$SELECT $1$$",
            true);
        let arena = Arena::new(&mut budget, "set query CTE metadata", 256 << 10).unwrap();
        let source = "WITH c AS (SELECT id FROM set_query_source) SELECT id FROM c UNION ALL SELECT 99 ORDER BY 1";
        crate::mem::guard::forbid_alloc(|| {
            let statement = parser::parse_stored_statement(source, &arena).unwrap();
            let mut dependencies = engine
                .storage
                .empty_stored_query_dependencies(&arena)
                .unwrap();
            let retained = arena.used();
            for _ in 0..64 {
                crate::sql::planner_inputs::capture_planner_inputs(
                    statement,
                    None,
                    &engine.storage,
                    0,
                    &arena,
                )
                .unwrap();
                query::collect_runtime_statement(
                    statement,
                    &engine.storage,
                    0,
                    &mut dependencies,
                    &arena,
                    None,
                )
                .unwrap();
                assert_eq!(arena.used(), retained);
            }
            assert!(dependencies.entries().iter().any(|entry| entry.class
                == crate::storage::DependencyClass::Table
                && entry.name.as_str() == "set_query_source"));
            let statement = parser::parse_stored_statement(
                "WITH c AS (SELECT nextval('set_query_effects')::integer AS id) SELECT set_query_identity(id) FROM c UNION ALL SELECT 99",
                &arena,
            ).unwrap();
            let mut dependencies = engine
                .storage
                .empty_stored_query_dependencies(&arena)
                .unwrap();
            query::collect_runtime_statement(
                statement,
                &engine.storage,
                0,
                &mut dependencies,
                &arena,
                None,
            )
            .unwrap();
            assert!(dependencies.entries().iter().any(|entry| entry.class
                == crate::storage::DependencyClass::Routine
                && entry.name.as_str() == "set_query_identity"));
        });
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_called FROM set_query_effects",
                true
            )),
            ["f"]
        );
        for (sql, expected) in [
            (source, vec!["1", "2", "99"]),
            (
                "WITH c(x) AS (SELECT 10) SELECT x FROM c UNION SELECT x+1 FROM c ORDER BY 1",
                vec!["10", "11"],
            ),
            (
                "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n<4) SELECT n FROM r UNION SELECT 100 ORDER BY 1",
                vec!["1", "2", "3", "4", "100"],
            ),
            (
                "WITH c AS (SELECT 5 AS x) SELECT x FROM c EXCEPT SELECT 6",
                vec!["5"],
            ),
            (
                "WITH c AS (SELECT * FROM unnest(ARRAY[1,2], ARRAY['p']) AS u(id,label)) SELECT id,label FROM c UNION ALL SELECT 3,'tail' ORDER BY 1",
                vec!["1|p", "2|NULL", "3|tail"],
            ),
            (
                "WITH c AS (SELECT nextval('set_query_effects')::integer AS id) SELECT set_query_identity(id) FROM c UNION ALL SELECT 99 ORDER BY 1",
                vec!["1", "99"],
            ),
        ] {
            assert_eq!(
                data_rows(&session.success(&mut engine, sql, true)),
                expected,
                "{mode:?}: {sql}"
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM set_query_effects",
                true
            )),
            ["1"]
        );
        assert_eq!(error_sqlstates(&session.execute(&mut engine,
            "WITH c AS (SELECT 1 AS id) SELECT id FROM c UNION ALL SELECT id FROM missing_set_source", true)), ["42P01"]);
    }
}

#[test]
fn describing_typed_routine_arguments_uses_complete_catalog_metadata_without_checks() {
    let config = test_config("typed-routine-describe-metadata");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SEQUENCE describe_checks; CREATE TYPE describe_choice AS ENUM('a'); \
         CREATE DOMAIN describe_count AS integer CHECK(nextval('describe_checks')>0); \
         CREATE TYPE describe_pair AS(id integer,label text); \
         CREATE FUNCTION describe_enum(describe_choice) RETURNS describe_choice LANGUAGE sql AS $$SELECT $1$$; \
         CREATE FUNCTION describe_domain(describe_count) RETURNS describe_count LANGUAGE sql AS $$SELECT $1$$; \
         CREATE FUNCTION describe_record(describe_pair) RETURNS describe_pair LANGUAGE sql AS $$SELECT $1$$", true);
    let arena = Arena::new(&mut budget, "typed routine description", 256 << 10).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let select = parser::parse_query("SELECT describe_enum('a'::describe_choice)::text,describe_domain(1::describe_count)::text,describe_record(ROW(1,'a')::describe_pair)::text", &arena).unwrap();
        let mut columns = [ColDesc::new("", 0, 0); 3];
        let count = exec::describe_items(
            select.items,
            None,
            None,
            Some(&engine.storage),
            0,
            &arena,
            &mut columns,
        )
        .unwrap();
        assert_eq!(count, 3);
        assert!(
            columns
                .iter()
                .all(|column| column.type_oid == types::oid::TEXT)
        );
    });
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value,is_called FROM describe_checks",
            true
        )),
        ["1|f"]
    );
}
