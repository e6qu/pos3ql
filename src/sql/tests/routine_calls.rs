use super::*;

#[test]
fn plpgsql_input_plans_specialize_collation_and_polymorphism_without_eager_checks() {
    // PG18.6 run41661367458, oracle SQL SHA256:
    // 3df4b20befdb50122161465bd83cc7a870e63ccc78bd6b9cb8e8c0bafc444e38.
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("plpgsql-input-instances-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE specialization_checks; CREATE TYPE specialization_choice AS ENUM('a'); \
             CREATE DOMAIN specialization_checked AS specialization_choice CHECK(nextval('specialization_checks')>0); \
             CREATE FUNCTION pl_collated(input text DEFAULT ('a' COLLATE \"C\")) RETURNS specialization_checked[] \
               LANGUAGE plpgsql AS $$BEGIN RETURN '{a}'::specialization_checked[]; END$$; \
             CREATE FUNCTION pl_polymorphic(input anyelement) RETURNS specialization_checked[] \
               LANGUAGE plpgsql AS $$BEGIN RETURN '{a}'::specialization_checked[]; END$$; \
             CREATE FUNCTION pl_unreached(input anyelement,choose boolean) RETURNS specialization_checked[] \
               LANGUAGE plpgsql AS $$BEGIN IF choose THEN RETURN '{a}'::specialization_checked[]; END IF; RETURN NULL; END$$", true);
        for (query, expected) in [
            ("SELECT pl_collated()", "1"),
            ("SELECT pl_collated('a' COLLATE \"C\")", "2"),
            ("SELECT pl_collated('a' COLLATE \"POSIX\")", "3"),
            ("SELECT pl_collated()", "3"),
            ("SELECT pl_polymorphic(1)", "4"),
            ("SELECT pl_polymorphic(2)", "4"),
            ("SELECT pl_polymorphic('a'::text)", "5"),
            ("SELECT pl_polymorphic(3)", "5"),
            ("SELECT pl_unreached(1,false)", "5"),
            ("SELECT pl_unreached('a'::text,false)", "5"),
            ("SELECT pl_unreached(1,true)", "6"),
            ("SELECT pl_unreached('a'::text,true)", "7"),
            ("SELECT pl_unreached(2,true)", "7"),
        ] {
            session.success(&mut engine, query, true);
            let output = session.success(
                &mut engine,
                "SELECT last_value FROM specialization_checks",
                true,
            );
            assert_eq!(data_rows(&output), [expected], "{query}, {mode:?}");
        }
    }
}

#[test]
fn sql_runtime_body_inputs_share_declared_types_and_separate_input_collations() {
    // PostgreSQL REL_18_6 funccache.c:268-300 keys by input collation and
    // declared argument OIDs, resolving only polymorphic/RECORD declarations.
    // PG18.6 run41661367458 fixture SHA256:
    // 43c3f393296f801672182516141a08e3ba69f50a56f706bf9292691054fb23c0.
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("runtime-body-collation-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE body_collation_checks; \
             CREATE TYPE body_collation_choice AS ENUM('a'); \
             CREATE DOMAIN body_collation_checked AS body_collation_choice \
               CHECK(nextval('body_collation_checks')>0); \
             CREATE FUNCTION body_collation(input text) RETURNS body_collation_checked[] \
               LANGUAGE sql SET search_path=public AS 'SELECT ''{a}''::body_collation_checked[]'; \
             CREATE FUNCTION body_collation_cast(input text) RETURNS integer \
               LANGUAGE sql SET search_path=public AS 'SELECT cardinality(''{a}''::body_collation_checked[])'; \
             CREATE CAST(text AS integer) WITH FUNCTION body_collation_cast(text) AS ASSIGNMENT; \
             CREATE TABLE body_collation_numbers(value integer); \
             CREATE TABLE body_collation_texts(value text COLLATE \"C\"); \
             INSERT INTO body_collation_texts VALUES('a'); \
             CREATE FUNCTION body_collation_default(input text DEFAULT ('a' COLLATE \"C\")) \
               RETURNS body_collation_checked[] LANGUAGE sql SET search_path=public \
               AS 'SELECT ''{a}''::body_collation_checked[]'; \
             CREATE FUNCTION body_collation_pair(first_input text, second_input text DEFAULT ('a' COLLATE \"POSIX\")) \
               RETURNS body_collation_checked[] LANGUAGE sql SET search_path=public \
               AS 'SELECT ''{a}''::body_collation_checked[]'; \
             CREATE FUNCTION body_collation_srf(input text DEFAULT ('a' COLLATE \"C\")) \
               RETURNS SETOF body_collation_checked[] LANGUAGE sql SET search_path=public \
               AS 'SELECT ''{a}''::body_collation_checked[]'; \
             CREATE PROCEDURE body_collation_procedure(input text DEFAULT ('a' COLLATE \"C\")) \
               LANGUAGE sql SET search_path=public AS 'SELECT ''{a}''::body_collation_checked[]'; \
             CREATE FUNCTION body_collation_identity(input text) RETURNS text LANGUAGE sql AS 'SELECT input'; \
             CREATE FUNCTION body_collation_nested(input text DEFAULT body_collation_identity('a' COLLATE \"C\")) \
               RETURNS body_collation_checked[] LANGUAGE sql SET search_path=public \
               AS 'SELECT ''{a}''::body_collation_checked[]'; \
             ALTER SEQUENCE body_collation_checks RESTART WITH 1", true);
        for (query, expected) in [
            ("SELECT body_collation('first' COLLATE \"C\")", "1"),
            (
                "SELECT body_collation('second'::varchar COLLATE \"C\")",
                "1",
            ),
            ("SELECT body_collation('third' COLLATE \"POSIX\")", "2"),
            ("SELECT body_collation('fourth'::text)", "3"),
            ("SELECT body_collation('fifth'::varchar)", "3"),
            ("SELECT body_collation(NULL::text COLLATE \"C\")", "3"),
            ("SELECT ('42'::text COLLATE \"C\")::integer", "4"),
            ("SELECT ('43'::text COLLATE \"C\")::integer", "4"),
            ("SELECT ('44'::text COLLATE \"POSIX\")::integer", "4"),
            (
                "SELECT ARRAY['45'::text COLLATE \"C\", '46'::text COLLATE \"C\"]::integer[]",
                "4",
            ),
            (
                "INSERT INTO body_collation_numbers SELECT '47'::text COLLATE \"C\" FROM generate_series(1,2)",
                "5",
            ),
            (
                "INSERT INTO body_collation_numbers VALUES('48'::text COLLATE \"POSIX\")",
                "5",
            ),
            ("SELECT body_collation_default()", "6"),
            ("SELECT body_collation_default('a' COLLATE \"C\")", "7"),
            ("SELECT body_collation_default('a'::text)", "8"),
            (
                "SELECT body_collation_pair(value) FROM body_collation_texts",
                "9",
            ),
            ("SELECT body_collation_pair('a' COLLATE \"POSIX\")", "10"),
            ("SELECT * FROM body_collation_srf()", "11"),
            ("SELECT * FROM body_collation_srf('a' COLLATE \"C\")", "12"),
            ("SELECT * FROM body_collation_srf('a'::text)", "13"),
            ("CALL body_collation_procedure()", "14"),
            ("CALL body_collation_procedure('a' COLLATE \"C\")", "15"),
            ("CALL body_collation_procedure('a'::text)", "16"),
            ("SELECT body_collation_nested()", "17"),
            ("SELECT body_collation_nested('a' COLLATE \"C\")", "18"),
            ("SELECT body_collation_nested('a'::text)", "19"),
        ] {
            session.success(&mut engine, query, true);
            let output = session.success(
                &mut engine,
                "SELECT last_value FROM body_collation_checks",
                true,
            );
            assert_eq!(data_rows(&output), [expected], "{query}, {mode:?}");
        }
        session.success(
            &mut engine,
            "SELECT body_collation_pair('a' COLLATE \"C\")",
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "SELECT body_collation_pair('a' COLLATE \"C\", 'a' COLLATE \"POSIX\")",
                true
            )),
            [sqlstate::COLLATION_MISMATCH]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM body_collation_checks",
                true
            )),
            ["19"]
        );
        session.success(
            &mut engine,
            "DROP FUNCTION body_collation_pair(text,text)",
            true,
        );
        session.success(
            &mut engine,
            "CREATE FUNCTION body_collation_echo(first_input text DEFAULT ('a' COLLATE \"POSIX\"), \
               second_input text DEFAULT ('b' COLLATE \"POSIX\")) RETURNS text \
               LANGUAGE sql SET search_path=public AS 'SELECT first_input'",
            true,
        );
        for expression in [
            "body_collation_echo(value)",
            "body_collation_echo(first_input=>value)",
            "body_collation_echo()",
            "body_collation_echo('a' COLLATE \"C\")",
        ] {
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    &format!("SELECT {expression} = ('a' COLLATE \"C\") FROM body_collation_texts"),
                    true
                )),
                ["t"],
                "{expression}, {mode:?}"
            );
        }
    }
}

#[test]
fn sorted_and_paged_projection_effects_follow_postgresql_in_memory() {
    sorted_and_paged_projection_effects(false);
}

#[test]
fn sorted_and_paged_projection_effects_follow_postgresql_through_http() {
    sorted_and_paged_projection_effects(true);
}

fn sorted_and_paged_projection_effects(external: bool) {
    let mut config = test_config(if external {
        "projection-window-external"
    } else {
        "projection-window-ram"
    });
    config.object_store_on = external;
    config.object_store_sim = external;
    config.object_store_bucket = format!("projection-window-{}-{external}", std::process::id());
    // All nine fixture relations coexist; statement work remains 256 KiB.
    config.max_tables = 9;
    config.work_arena_bytes = 256 << 10;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    let server = external.then(|| object_reads::HttpObjectServer::start(&mut config));
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    assert_eq!(engine.storage.spill_attached(), external);
    let initial_uploads = server
        .as_ref()
        .map(|server| server.uploads.load(std::sync::atomic::Ordering::Relaxed));
    let mut session =
        ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, 256 << 10);
    session.success(&mut engine,
            "CREATE TABLE projection_inputs(id integer,payload text); \
             CREATE TABLE projection_outer(id integer); INSERT INTO projection_outer VALUES(10); \
             CREATE SEQUENCE projection_calls; \
             CREATE FUNCTION projection_default(value bigint DEFAULT nextval('public.projection_calls')) \
             RETURNS bigint IMMUTABLE LANGUAGE sql SET search_path TO pg_catalog AS $$ SELECT value $$", true);
    for id in [2, 1, 3] {
        session.success(
            &mut engine,
            &format!("INSERT INTO projection_inputs VALUES({id},repeat('p',65536))"),
            true,
        );
    }
    for (query, expected) in [
        (
            "SELECT id,nextval('projection_calls') AS marker FROM projection_inputs ORDER BY id LIMIT 1",
            &["1|1", "1"][..],
        ),
        (
            "SELECT id,nextval('projection_calls') AS marker FROM projection_inputs ORDER BY id",
            &["1|1", "2|2", "3|3", "3"][..],
        ),
        (
            "SELECT * FROM (SELECT id,nextval('projection_calls') AS marker FROM projection_inputs LIMIT 1) AS selected",
            &["2|1", "1"][..],
        ),
        (
            "SELECT id,nextval('projection_calls') AS marker FROM projection_inputs ORDER BY id OFFSET 1 LIMIT 1",
            &["2|2", "2"][..],
        ),
        (
            "SELECT * FROM (SELECT id,nextval('projection_calls') AS marker FROM projection_inputs OFFSET 1 LIMIT 1) AS selected",
            &["1|2", "2"][..],
        ),
        (
            "SELECT id,nextval('projection_calls') AS marker FROM projection_inputs ORDER BY marker LIMIT 1",
            &["2|1", "3"][..],
        ),
        (
            "SELECT id,nextval('projection_calls') AS marker FROM projection_inputs ORDER BY nextval('projection_calls') OFFSET 1 LIMIT 1",
            &["1|2", "3"][..],
        ),
        (
            "SELECT id,projection_default() AS marker FROM projection_inputs ORDER BY id LIMIT 1",
            &["1|1", "1"][..],
        ),
        (
            "SELECT * FROM (SELECT id,projection_default() AS marker FROM projection_inputs ORDER BY id LIMIT 1) AS selected",
            &["1|1", "1"][..],
        ),
        (
            "SELECT source.id,selected.marker FROM projection_outer AS source CROSS JOIN LATERAL (SELECT source.id+nextval('projection_calls') AS marker FROM projection_inputs ORDER BY id LIMIT 1) AS selected",
            &["10|11", "1"][..],
        ),
    ] {
        let command = format!(
            "ALTER SEQUENCE projection_calls RESTART WITH 1; {query}; SELECT currval('projection_calls')"
        );
        let output = session.success(&mut engine, &command, true);
        assert_eq!(data_rows(&output), expected, "external={external}: {query}");
    }
    let ties = session.success(
        &mut engine,
        "CREATE TABLE projection_ties(id integer); INSERT INTO projection_ties VALUES(2),(1),(1); \
             ALTER SEQUENCE projection_calls RESTART WITH 1; \
             SELECT * FROM (SELECT id,nextval('projection_calls') AS marker FROM projection_ties \
                 ORDER BY id FETCH FIRST 1 ROW WITH TIES) AS selected; \
             SELECT currval('projection_calls')",
        true,
    );
    assert_eq!(data_rows(&ties), ["1|1", "1|2", "3"], "external={external}");
    let offset_ties = session.success(
        &mut engine,
        "ALTER SEQUENCE projection_calls RESTART WITH 1; \
             SELECT * FROM (SELECT id,nextval('projection_calls') AS marker FROM projection_ties \
                 ORDER BY id OFFSET 1 FETCH FIRST 1 ROW WITH TIES) AS selected; \
             SELECT currval('projection_calls')",
        true,
    );
    assert_eq!(data_rows(&offset_ties), ["1|2", "3"], "external={external}");
    let zero_limit = session.success(
        &mut engine,
        "ALTER SEQUENCE projection_calls RESTART WITH 1; \
             SELECT * FROM (SELECT id,nextval('projection_calls') AS marker FROM projection_inputs \
                 ORDER BY nextval('projection_calls') LIMIT 0) AS selected; \
             SELECT nextval('projection_calls')",
        true,
    );
    assert_eq!(data_rows(&zero_limit), ["1"], "external={external}");
    let nested_zero_limit = session.success(
        &mut engine,
        "ALTER SEQUENCE projection_calls RESTART WITH 1; \
             SELECT * FROM (SELECT * FROM (SELECT nextval('projection_calls') AS marker \
                 FROM projection_inputs) AS source ORDER BY 1 LIMIT 0) AS selected; \
             SELECT nextval('projection_calls')",
        true,
    );
    assert_eq!(data_rows(&nested_zero_limit), ["1"], "external={external}");
    session.success(
        &mut engine,
        "CREATE TABLE projection_locks(row_id integer PRIMARY KEY,order_key integer); \
             INSERT INTO projection_locks VALUES(1,1),(2,1),(3,2); \
             ALTER SEQUENCE projection_calls RESTART WITH 1; BEGIN",
        true,
    );
    let locked = session.success(&mut engine,
            "SELECT order_key,marker FROM (SELECT row_id,order_key,nextval('projection_calls') AS marker \
                 FROM projection_locks ORDER BY order_key FETCH FIRST 1 ROW WITH TIES FOR UPDATE) AS selected; \
             SELECT currval('projection_calls')", true);
    assert_eq!(
        data_rows(&locked),
        ["1|1", "1|2", "3"],
        "external={external}"
    );
    let mut other = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    let refused = other.execute(
        &mut engine,
        "SELECT row_id FROM projection_locks WHERE row_id=3 FOR UPDATE NOWAIT",
        true,
    );
    assert_eq!(error_sqlstates(&refused), ["55P03"]);
    // PostgreSQL names the relation whose row is locked.
    assert!(
        String::from_utf8_lossy(&refused)
            .contains("could not obtain lock on row in relation \"projection_locks\""),
        "{}",
        String::from_utf8_lossy(&refused)
    );
    session.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&other.success(
            &mut engine,
            "SELECT row_id FROM projection_locks WHERE row_id=3 FOR UPDATE NOWAIT",
            true
        )),
        ["3"]
    );
    session.success(
        &mut engine,
        "ALTER SEQUENCE projection_calls RESTART WITH 1; BEGIN; \
             SELECT row_id FROM projection_locks WHERE row_id=3 FOR UPDATE",
        true,
    );
    let waiting_query = "SELECT order_key,marker FROM (SELECT row_id,order_key,nextval('projection_calls') AS marker \
            FROM projection_locks ORDER BY order_key FETCH FIRST 1 ROW WITH TIES FOR UPDATE) AS selected";
    let status = other.execute_status(&mut engine, waiting_query, true);
    assert!(matches!(status, ExecutionStatus::Blocked { .. }));
    assert!(
        other.send.is_empty(),
        "nested lock wait emitted a partial result: external={external}"
    );
    session.success(&mut engine, "ROLLBACK", true);
    let status = crate::mem::guard::forbid_alloc(|| {
        engine.execute_simple_from(
            waiting_query,
            0,
            &other.arena,
            &mut other.transaction,
            &mut other.pool,
            &mut other.cursors,
            &other.guc,
            &mut Responder::new(&mut other.send),
            2,
            false,
        )
    })
    .unwrap();
    assert_eq!(status, ExecutionStatus::Complete);
    assert_eq!(
        data_rows(other.send.readable()),
        ["1|1", "1|2"],
        "external={external}"
    );
    assert_eq!(
        data_rows(&other.success(&mut engine, "SELECT currval('projection_calls')", true)),
        ["3"]
    );
    session.success(&mut engine,
            "CREATE TABLE projection_left(id integer); INSERT INTO projection_left VALUES(1),(2); \
             CREATE TABLE projection_right(id integer,payload text); INSERT INTO projection_right VALUES(NULL,NULL); \
             CREATE TABLE projection_empty(); INSERT INTO projection_empty DEFAULT VALUES", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT right_row::text FROM projection_empty AS right_row",
            true
        )),
        ["()"]
    );
    for (query, expected) in [
        (
            "SELECT left_row::text||nextval('projection_calls')::text \
              FROM projection_left AS left_row ORDER BY left_row.id",
            &["(1)1", "(2)2", "2"][..],
        ),
        (
            "SELECT COALESCE(right_row::text,'missing')||nextval('projection_calls')::text \
              FROM projection_left AS left_row LEFT JOIN projection_right AS right_row ON left_row.id=1 \
              ORDER BY left_row.id",
            &["(,)1", "missing2", "2"][..],
        ),
        (
            "SELECT COALESCE(right_row::text,'missing')||nextval('projection_calls')::text \
              FROM projection_left AS left_row LEFT JOIN projection_empty AS right_row ON left_row.id=1 \
              ORDER BY left_row.id",
            &["()1", "missing2", "2"][..],
        ),
        (
            "SELECT ROW(merged.*,nextval('projection_calls'))::text FROM projection_left AS left_row \
              LEFT JOIN projection_right AS right_row USING(id) AS merged ORDER BY left_row.id",
            &["(1,1)", "(2,2)", "2"][..],
        ),
    ] {
        let command = format!(
            "ALTER SEQUENCE projection_calls RESTART WITH 1; {query}; SELECT currval('projection_calls')"
        );
        let output = session.success(&mut engine, &command, true);
        assert_eq!(data_rows(&output), expected, "external={external}: {query}");
    }
    session.success(
        &mut engine,
        "CREATE TABLE projection_special_forms(id integer,z integer,f double precision,n numeric); \
         INSERT INTO projection_special_forms VALUES \
           (1,5,1.5,2.5),(2,4,2.5,3.5),(3,3,3.5,4.5),(9,0,9.5,9.5)",
        true,
    );
    for expression in [
        "greatest(100/z,f)",
        "greatest(100/z,f,n)",
        "least(100/z,f,n,id)",
        "coalesce(100/z,id)",
        "coalesce(100/z+f*2-f/3+f*4,n)",
        "nullif(100/z,id)",
        "(current_date-current_date)+100/z",
        "json_object('n':100/z)",
        "json_object('n':100/z+f*2-f/3+f*4-f/5)",
        "row(100/z+f*2-f/3+f*4-f/5)",
        "xmlparse(document('<n>'||(100/z)::text||'</n>'))",
    ] {
        let query =
            format!("SELECT {expression} FROM projection_special_forms ORDER BY id LIMIT 2");
        let output = session.execute(&mut engine, &query, true);
        assert_eq!(
            error_sqlstates(&output),
            ["22012"],
            "external={external}: {query}"
        );
    }
    for expression in [
        "greatest(100/z+f*2-f/3+f*4-f/5,n,id)",
        "coalesce(100/z+f*2-f/3+f*4-f/5,n)",
        "json_object('n':100/z+f*2-f/3+f*2-f/3+f*2-f/3+f*2-f/3+f*2-f/3)",
        "xmlparse(document('<n>'||(100/z+f*2-f/3+f*4-f/5)::text||'</n>'))",
        "row(100/z+f*2-f/3+f*4-f/5)::text",
    ] {
        let query =
            format!("SELECT {expression} FROM projection_special_forms ORDER BY id LIMIT 2");
        let output = session.success(&mut engine, &query, true);
        assert_eq!(data_rows(&output).len(), 2, "external={external}: {query}");
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER SEQUENCE projection_calls RESTART WITH 1; \
             SELECT id,greatest(100/z,nextval('projection_calls')) \
               FROM projection_special_forms ORDER BY id LIMIT 2; \
             SELECT nextval('projection_calls')",
            true,
        )),
        ["1|20", "2|25", "3"],
        "external={external}",
    );
    for (expression, expected) in [
        (
            "json_object('n':nextval('projection_calls'))->>'n'",
            &["1|1", "2|2", "3"][..],
        ),
        (
            "xmlserialize(content xmlelement(name n,nextval('projection_calls')) as text)",
            &["1|<n>1</n>", "2|<n>2</n>", "3"][..],
        ),
    ] {
        let query = format!(
            "ALTER SEQUENCE projection_calls RESTART WITH 1; \
             SELECT id,{expression} FROM projection_special_forms ORDER BY id LIMIT 2; \
             SELECT nextval('projection_calls')"
        );
        let output = session.success(&mut engine, &query, true);
        assert_eq!(data_rows(&output), expected, "external={external}: {query}");
    }
    let promoted_nullif = session.success(
        &mut engine,
        "SELECT NULLIF(1::smallint,2.0),pg_typeof(NULLIF(1::smallint,2.0)), \
                NULLIF(1::smallint,2::integer),pg_typeof(NULLIF(1::smallint,2::integer)), \
                NULLIF(1,2.2),pg_typeof(NULLIF(1,2.2))",
        true,
    );
    assert_eq!(
        data_rows(&promoted_nullif),
        ["1|numeric|1|smallint|1|numeric"],
        "external={external}"
    );
    assert_eq!(
        row_description_type_oids(&promoted_nullif),
        [1700, 2206, 21, 2206, 1700, 2206]
    );
    session.success(
        &mut engine,
        "CREATE TYPE projection_pair AS(value integer); \
         CREATE SEQUENCE projection_equality_calls; \
         CREATE FUNCTION projection_pair_equals(projection_pair,projection_pair) RETURNS boolean \
           VOLATILE LANGUAGE sql SET search_path TO pg_catalog \
           AS $$ SELECT nextval('public.projection_equality_calls')<0 $$; \
         CREATE OPERATOR public.= (FUNCTION=projection_pair_equals, \
           LEFTARG=projection_pair,RIGHTARG=projection_pair); \
         CREATE TABLE projection_pairs(id integer,value projection_pair); \
         INSERT INTO projection_pairs VALUES(2,ROW(2)),(1,ROW(1)),(3,ROW(3))",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT id,NULLIF(value,ROW(0)::projection_pair)::text FROM projection_pairs \
               ORDER BY id LIMIT 1; SELECT currval('projection_equality_calls')",
            true,
        )),
        ["1|(1)", "1"],
        "external={external}",
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT NULLIF(NULL::projection_pair,ROW(0)::projection_pair)::text; \
             SELECT nextval('projection_equality_calls')",
            true,
        )),
        ["NULL", "1"],
        "external={external}",
    );
    session.success(
        &mut engine,
        "CREATE TYPE projection_mood AS ENUM('x','y'); \
         ALTER TABLE projection_pairs ADD COLUMN mood projection_mood DEFAULT 'x'; \
         CREATE FUNCTION projection_enum_json(projection_mood) RETURNS json \
           VOLATILE LANGUAGE sql SET search_path TO pg_catalog \
           AS $$ SELECT json_build_object('value',$1::text, \
                    'call',nextval('public.projection_equality_calls')) $$; \
         CREATE CAST(projection_mood AS json) WITH FUNCTION projection_enum_json(projection_mood); \
         CREATE FUNCTION projection_pair_json(projection_pair) RETURNS json \
           VOLATILE LANGUAGE sql SET search_path TO pg_catalog \
           AS $$ SELECT json_build_object('value',($1).value, \
                    'call',nextval('public.projection_equality_calls')) $$; \
         CREATE CAST(projection_pair AS json) WITH FUNCTION projection_pair_json(projection_pair)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT id,json_object('n':mood)->'n'->>'call' FROM projection_pairs \
               ORDER BY id LIMIT 1; SELECT currval('projection_equality_calls')",
            true,
        )),
        ["1|2", "3"],
        "external={external}",
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT id,json_object('n':value)->'n'->>'value' FROM projection_pairs \
               ORDER BY id LIMIT 1; SELECT last_value,is_called FROM projection_equality_calls; \
             SELECT (value::json)->>'call' FROM projection_pairs WHERE id=1; \
             SELECT currval('projection_equality_calls')",
            true,
        )),
        ["1|1", "1|f", "1", "1"],
        "external={external}",
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT (mood::json)->>'call' FROM projection_pairs WHERE id=1; \
             SELECT currval('projection_equality_calls')",
            true,
        )),
        ["1", "1"],
        "external={external}",
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT NULLIF(mood,'x'),NULLIF(mood,'y'),NULLIF('x',mood), \
                    NULLIF(NULL::projection_mood,'x'),NULLIF('x',NULL::projection_mood) \
               FROM projection_pairs WHERE id=1",
            true,
        )),
        ["NULL|x|NULL|NULL|x"],
        "external={external}",
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT NULLIF(mood,'missing') FROM projection_pairs WHERE id=1",
            true,
        )),
        ["22P02"],
        "external={external}",
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT json_array(mood)->0->>'call', \
                    json_object('n':ARRAY[mood])->'n'->0->>'call', \
                    json_object('n':ROW(mood))->'n'->'f1'->>'call' \
               FROM projection_pairs WHERE id=1; SELECT currval('projection_equality_calls')",
            true,
        )),
        ["1|2|3", "3"],
        "external={external}",
    );
    session.success(
        &mut engine,
        "CREATE OR REPLACE FUNCTION projection_enum_json(projection_mood) RETURNS json \
           VOLATILE LANGUAGE sql SET search_path TO pg_catalog AS $$ SELECT NULL::json $$",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT json_object('n':mood) FROM projection_pairs WHERE id=1",
            true,
        )),
        ["XX000"],
        "external={external}",
    );
    let array_comparisons = session.success(
        &mut engine,
        "SELECT array_eq(ARRAY[1,NULL],ARRAY[1,NULL]),array_ne(ARRAY[1],ARRAY[2]), \
                array_lt(ARRAY[1],ARRAY[2]),array_le(ARRAY[1],ARRAY[1]), \
                array_gt(ARRAY[2],ARRAY[1]),array_ge(ARRAY[1],ARRAY[1]), \
                btarraycmp(ARRAY[1],ARRAY[2]),btarraycmp(ARRAY[1,2],ARRAY[1]); \
         SELECT array_eq(NULL::integer[],ARRAY[1]); \
         SELECT array_eq('{1}',ARRAY[1]),array_eq(ARRAY['x']::projection_mood[],'{x}'); \
         SELECT array_eq('[0:1]={1,2}'::integer[],ARRAY[1,2]), \
                array_lt('[0:1]={1,2}'::integer[],ARRAY[1,2]), \
                btarraycmp('[0:1]={1,2}'::integer[],ARRAY[1,2]); \
         SELECT array_eq('[1:2][1:1][1:3]={{{1,2,3}},{{4,5,6}}}'::integer[], \
                         '[1:2][1:3][1:1]={{{1},{2},{3}},{{4},{5},{6}}}'::integer[]), \
                btarraycmp('[1:2][1:1][1:3]={{{1,2,3}},{{4,5,6}}}'::integer[], \
                           '[1:2][1:3][1:1]={{{1},{2},{3}},{{4},{5},{6}}}'::integer[])",
        true,
    );
    assert_eq!(
        data_rows(&array_comparisons),
        ["t|t|t|t|t|t|-1|1", "NULL", "t|t", "f|t|-1", "f|-1"],
        "external={external}",
    );
    let mut comparison_types = Vec::new();
    let mut remaining = array_comparisons.as_slice();
    while !remaining.is_empty() {
        let length = i32::from_be_bytes(remaining[1..5].try_into().unwrap()) as usize;
        if remaining[0] == b'T' {
            comparison_types.extend(row_description_type_oids(&remaining[..1 + length]));
        }
        remaining = &remaining[1 + length..];
    }
    assert_eq!(
        comparison_types,
        [
            16, 16, 16, 16, 16, 16, 23, 23, 16, 16, 16, 16, 16, 23, 16, 23
        ],
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT array_gt(ARRAY[NULL::integer],ARRAY[1]), \
                    array_lt(ARRAY[1],ARRAY[NULL::integer]), \
                    btarraycmp(ARRAY[NULL::integer],ARRAY[NULL::integer]), \
                    ARRAY[NULL::integer]=ARRAY[NULL::integer], \
                    ARRAY[NULL::integer]>ARRAY[1],ARRAY[1]<ARRAY[NULL::integer]; \
             SELECT ARRAY[1,2,3,4,5,6]<'{{1,2,3},{4,5,6}}'::integer[], \
                    array_lt(ARRAY[1,2,3,4,5,6],'{{1,2,3},{4,5,6}}'::integer[]), \
                    btarraycmp(ARRAY[1,2,3,4,5,6],'{{1,2,3},{4,5,6}}'::integer[]), \
                    '[1:2][1:1][1:3]={{{1,2,3}},{{4,5,6}}}'::integer[] \
                        <'[0:1][1:3][1:1]={{{1},{2},{3}},{{4},{5},{6}}}'::integer[], \
                    array_lt('[1:2][1:1][1:3]={{{1,2,3}},{{4,5,6}}}'::integer[], \
                             '[0:1][1:3][1:1]={{{1},{2},{3}},{{4},{5},{6}}}'::integer[]), \
                    btarraycmp('[1:2][1:1][1:3]={{{1,2,3}},{{4,5,6}}}'::integer[], \
                               '[0:1][1:3][1:1]={{{1},{2},{3}},{{4},{5},{6}}}'::integer[])",
            true,
        )),
        ["t|t|0|t|t|t", "t|t|-1|t|t|-1"],
        "external={external}",
    );
    for (query, state) in [
        ("SELECT array_eq(NULL,NULL)", "42804"),
        ("SELECT array_eq(NULL::integer[],NULL::text[])", "42883"),
        (
            "SELECT array_eq(ARRAY[1]::integer[],ARRAY[1]::bigint[])",
            "42883",
        ),
        (
            "SELECT array_eq(NULL::projection_mood[],'{missing}')",
            "22P02",
        ),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, query, true)),
            [state],
            "external={external}: {query}"
        );
    }
    session.success(
        &mut engine,
        "CREATE DOMAIN projection_array AS integer[]; \
         CREATE FUNCTION projection_array_value(integer,integer) RETURNS projection_array \
           VOLATILE LANGUAGE plpgsql AS $$ DECLARE value projection_array; BEGIN \
             PERFORM nextval('public.projection_equality_calls'); value:=ARRAY[$1,$2]; RETURN value; END $$; \
         CREATE FUNCTION projection_array_equals(projection_array,projection_array) RETURNS boolean \
           LANGUAGE plpgsql AS $$ BEGIN RETURN pg_catalog.array_eq($1,$2); END $$; \
         CREATE OPERATOR public.= (FUNCTION=projection_array_equals, \
           LEFTARG=projection_array,RIGHTARG=projection_array)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT array_eq(ARRAY[1]::projection_array,ARRAY[1]), \
                    array_eq(NULL::projection_array,NULL::integer[]); \
             ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT CASE projection_array_value(1,2) \
               WHEN ARRAY[2,4]::projection_array THEN 'wrong' \
               WHEN ARRAY[2,5]::projection_array THEN 'still wrong' \
               WHEN ARRAY[1,2]::projection_array THEN 'right' END; \
             SELECT currval('projection_equality_calls'); \
             ALTER SEQUENCE projection_equality_calls RESTART WITH 1; \
             SELECT NULLIF(projection_array_value(1,2),ARRAY[2,3]::projection_array); \
             SELECT currval('projection_equality_calls')",
            true,
        )),
        ["t|NULL", "right", "1", "{1,2}", "1"],
        "external={external}",
    );
    if let Some(server) = server.as_ref() {
        assert!(
            server.uploads.load(std::sync::atomic::Ordering::Relaxed) > initial_uploads.unwrap(),
            "sort must upload through the HTTP provider"
        );
        assert!(
            server.requests.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "sort must read through the HTTP provider"
        );
    }
    drop((session, other, engine));
    drop(server);
    if external {
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
fn query_positions_do_not_repeat_completed_projection_effects() {
    let config = test_config("retained-query-positions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    // Independent cases commit retired definitions within the eight routine slots.
    let sql =
        include_str!("../../../tests/external/differential_exact/55_retained_query_positions.sql");
    let output = session.success(&mut engine, sql, true);
    assert_eq!(
        data_rows(&output),
        [
            &["0", "1|1", "2|2", "3|3", "3", "1", "2", "3"][..],
            &["0", "20", "20", "20", "3", "0|3", "20|3"],
            &["0", "3|99", "3", "0|3", "99|3"],
            &["0", "1|7|2|8|3", "3", "7|2", "8|1"],
            &["query_second,public|12", "12", "22", "12", "22"],
            &["query_second,pg_catalog,public|2|2", "shadow"],
            &["2|2", "777"],
            &["0|101", "109", "101", "109"],
            &[
                "0", "2", "0", "7", "9", "11", "15", "0", "0", "2", "7", "9", "11", "15"
            ],
            &["0|7", "NULL", "7", "0|NULL", "7", "7"],
            &["0|7", "NULL", "7", "0|NULL", "7", "7"],
            &["9", "7", "9", "11"],
            &["2|2", "9"],
        ]
        .concat()
    );
    let mut remaining = output.as_slice();
    let mut descriptions = 0;
    let mut completions = 0;
    while !remaining.is_empty() {
        let length = i32::from_be_bytes(remaining[1..5].try_into().unwrap()) as usize;
        descriptions += usize::from(remaining[0] == b'T');
        completions += usize::from(remaining[0] == b'C');
        remaining = &remaining[length + 1..];
    }
    assert_eq!(descriptions, 53);
    assert_eq!(completions, 137);
}

#[test]
fn named_routine_arguments_preserve_quoted_identifier_case() {
    let config = test_config("quoted-routine-arguments");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE quoted_argument_source(value integer); \
         INSERT INTO quoted_argument_source VALUES(2); \
         CREATE FUNCTION quoted_argument_input(\"Value\" integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS $$SELECT $1$$; \
         CREATE FUNCTION folded_argument_input(VALUE integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS $$SELECT $1$$; \
         CREATE PROCEDURE quoted_argument_output(OUT \"Result\" integer,\"Value\" integer, \
         VARIADIC \"Members\" integer[]) LANGUAGE sql AS $$SELECT $1+cardinality($2)$$",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT quoted_argument_input(\"Value\"=>value), \
             folded_argument_input(VALUE=>value) FROM quoted_argument_source; \
             CALL quoted_argument_output(\"Result\"=>NULL,\"Value\"=>9, \
             VARIADIC \"Members\"=>ARRAY[]::integer[])",
            true,
        )),
        ["2|2", "9"]
    );
    for sql in [
        "SELECT quoted_argument_input(value=>value) FROM quoted_argument_source",
        "SELECT quoted_argument_input(\"value\"=>value) FROM quoted_argument_source",
        "CALL quoted_argument_output(\"result\"=>NULL,\"Value\"=>9, \
         VARIADIC \"Members\"=>ARRAY[]::integer[])",
        "CALL quoted_argument_output(\"Result\"=>NULL,value=>9, \
         VARIADIC \"Members\"=>ARRAY[]::integer[])",
        "CALL quoted_argument_output(\"Result\"=>NULL,\"Value\"=>9, \
         VARIADIC members=>ARRAY[]::integer[])",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42883"],
            "{sql}"
        );
    }
    let actual = [
        types::oid::UNKNOWN,
        types::oid::INT4,
        ColType::Array(types::ArrElem::Int4).oid(),
    ];
    let expected = engine
        .storage
        .procedure_call_parameter_oids(
            "quoted_argument_output",
            &[Some("Result"), Some("Value"), Some("Members")],
            true,
            &actual,
            0,
        )
        .expect("quoted argument identities resolve protocol types");
    assert_eq!(
        &expected[..actual.len()],
        &[types::oid::INT4, types::oid::INT4, actual[2]]
    );
    assert!(
        engine
            .storage
            .procedure_call_parameter_oids(
                "quoted_argument_output",
                &[Some("result"), Some("Value"), Some("Members")],
                true,
                &actual,
                0,
            )
            .is_none()
    );
}

#[test]
fn retained_variadic_calls_require_explicit_empty_arrays_or_declared_defaults() {
    let config = test_config("retained-variadic-arity");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE retained_variadic_source(value integer); \
         INSERT INTO retained_variadic_source VALUES(1); \
         CREATE FUNCTION retained_variadic_required(VARIADIC members integer[]) \
         RETURNS integer LANGUAGE sql VOLATILE AS $$SELECT cardinality($1)$$; \
         CREATE FUNCTION retained_variadic_named_default(marker integer DEFAULT 7, \
         VARIADIC members integer[] DEFAULT ARRAY[]::integer[]) RETURNS integer \
         LANGUAGE sql VOLATILE AS $$SELECT $1+cardinality($2)$$",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT retained_variadic_required() FROM retained_variadic_source",
            true,
        )),
        ["42883"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT retained_variadic_named_default(marker=>9) FROM retained_variadic_source",
            true,
        )),
        ["42883"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CREATE FUNCTION retained_variadic_invalid(marker integer DEFAULT 7, \
             VARIADIC members integer[]) RETURNS integer LANGUAGE sql \
             AS $$SELECT $1+cardinality($2)$$",
            true,
        )),
        ["42P13"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT retained_variadic_required(VARIADIC ARRAY[]::integer[]) \
             FROM retained_variadic_source",
            true,
        )),
        ["0"]
    );
}

#[test]
fn variadic_procedures_preserve_output_positions_and_named_array_syntax() {
    let config = test_config("variadic-procedure-mapping");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE PROCEDURE variadic_output(OUT result integer,marker integer DEFAULT 7, \
         VARIADIC members integer[] DEFAULT ARRAY[]::integer[]) LANGUAGE sql \
         AS $$SELECT $1+cardinality($2)$$",
        true,
    );
    for sql in [
        "CALL variadic_output(result=>NULL)",
        "CALL variadic_output(result=>NULL,marker=>9)",
        "CALL variadic_output(result=>NULL,marker=>9,members=>ARRAY[]::integer[])",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42883"],
            "{sql}"
        );
    }
    for sql in [
        "CREATE PROCEDURE invalid_variadic_output(VARIADIC members integer[], \
         OUT result integer) LANGUAGE sql AS $$SELECT cardinality($1)$$",
        "CREATE PROCEDURE invalid_variadic_default_output(marker integer DEFAULT 7, \
         VARIADIC members integer[] DEFAULT ARRAY[]::integer[],OUT result integer) \
         LANGUAGE sql AS $$SELECT $1+cardinality($2)$$",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42P13"],
            "{sql}"
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CALL variadic_output(result=>NULL,marker=>9,VARIADIC members=>ARRAY[]::integer[]); \
             CALL variadic_output(NULL); \
             CALL variadic_output(NULL,9,VARIADIC ARRAY[]::integer[]); \
             CALL variadic_output(NULL,9,4,5)",
            true,
        )),
        ["9", "7", "9", "11"]
    );
}

#[test]
fn retained_query_errors_rollback_completed_writes_and_session_changes() {
    let config = test_config("retained-query-rollback");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE query_rollback_source(value integer); \
         INSERT INTO query_rollback_source VALUES(1); \
         CREATE TABLE query_rollback_audit(value integer); \
         CREATE SEQUENCE query_rollback_numbers; \
         CREATE FUNCTION query_rollback_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO query_rollback_audit VALUES($1); SELECT $1$$; \
         SELECT set_config('application_name','0',false)",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    let output = session.execute(
        &mut engine,
        "SELECT nextval('query_rollback_numbers'), \
         set_config('application_name','changed',false), \
         query_rollback_write(value), 1/(value-1) FROM query_rollback_source",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["22012"]);
    session.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT current_setting('application_name'); \
             SELECT count(*) FROM query_rollback_audit; \
             SELECT last_value,is_called FROM query_rollback_numbers; \
             SELECT query_rollback_write(value) FROM query_rollback_source; \
             SELECT value FROM query_rollback_audit",
            true,
        )),
        ["0", "0", "1|t", "1", "1"]
    );
}

#[test]
fn retained_query_call_state_is_reused_between_rows() {
    const ROWS: usize = 1100;
    let config = test_config("retained-query-bounded-rows");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let statement_bytes = crate::storage::RowCursor::position_bytes(ROWS, 0).unwrap() + (128 << 10);
    let mut session =
        ConfiguredTransactionSession::with_arena_bytes(&config, &mut budget, statement_bytes);
    session.success(
        &mut engine,
        "CREATE TABLE query_bounded_source(value integer); \
         CREATE TABLE query_bounded_audit(value integer); \
         CREATE FUNCTION query_bounded_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO query_bounded_audit VALUES($1); SELECT $1$$",
        true,
    );
    for value in 1..=ROWS {
        session.success(
            &mut engine,
            &format!("INSERT INTO query_bounded_source VALUES({value})"),
            true,
        );
    }
    session.success(
        &mut engine,
        "SELECT set_config('application_name','0',false)",
        true,
    );
    let output = session.success(
        &mut engine,
        "SELECT set_config('application_name', \
         (current_setting('application_name')::integer+1)::text,false), \
         query_bounded_write(value) FROM query_bounded_source",
        true,
    );
    let rows = data_rows(&output);
    assert_eq!(rows.len(), ROWS);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.split('|').next().unwrap(), (index + 1).to_string());
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT current_setting('application_name'); \
             SELECT count(*),count(DISTINCT value),sum(value) FROM query_bounded_audit",
            true,
        )),
        ["1100", "1100|1100|605550"]
    );
}

#[test]
fn intrinsic_handoffs_retain_completed_builtin_inputs() {
    let config = test_config("retained-intrinsic-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/54_retained_intrinsic_inputs.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        ["0", "0.", "0.", "0", "1", "0."]
    );
}

#[test]
fn named_routine_rows_expose_fields_in_table_sources() {
    let config = test_config("named-routine-table-rows");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/53_named_routine_table_rows.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        [
            "7|one|1",
            "NULL|NULL|2",
            "8|two|1",
            "NULL|NULL|2",
            "9|sql",
            "10|pl|1",
            "NULL|NULL|2",
            "11|out",
            "1",
            "7|one|20|1",
            "NULL|NULL|21|2",
            "NULL|NULL|22|3",
            "1|7|one",
            "1|NULL|NULL",
            "2|7|one",
            "2|NULL|NULL",
            "(7,one)",
            "NULL",
            "4|new",
        ]
    );
}

#[test]
fn retained_routine_inputs_preserve_argument_effects_across_pending_calls() {
    let config = test_config("retained-routine-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/50_retained_routine_inputs.sql");
    let output = session.success(&mut engine, sql, true);
    assert_eq!(
        data_rows(&output),
        [
            &["0", "1", "1", "1"][..],
            &["0", "8", "1", "7", "8"],
            &["0", "8", "1", "7", "8"],
            &["0", "8", "1", "7", "8"],
            &["0", "8", "1", "7", "8"],
            &["0", "17", "1", "7", "9", "17"],
            &["0", "8", "1", "7", "8"],
            &["0", "1", "2", "1", "1"],
            &["0", "1", "2", "1", "1"],
            &["0", "1", "1", "1"],
            &["0", "1/7", "1", "7"],
            &["0", "8/value", "1", "7"],
            &["0", "1", "2", "3", "1", "3"],
            &["0", "1", "2", "3", "1", "3"],
            &["0", "108", "0", "7", "108"],
            &["11", "7", "11"],
            &["5", "6", "integer", "integer"],
        ]
        .concat()
    );
    let mut descriptions = Vec::new();
    let mut remaining = output.as_slice();
    while !remaining.is_empty() {
        let length = i32::from_be_bytes(remaining[1..5].try_into().unwrap()) as usize;
        if remaining[0] == b'T' {
            descriptions.extend(row_description_type_oids(remaining));
        }
        remaining = &remaining[1 + length..];
    }
    assert_eq!(&descriptions[descriptions.len() - 2..], &[23, 2206]);
}

#[test]
fn retained_routine_targets_survive_argument_search_path_changes() {
    let config = test_config("retained-routine-targets");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/51_retained_routine_targets.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        ["11", "11", "22", "11", "22", "11", "11"]
    );
}

#[test]
fn constant_catalog_identifier_errors_do_not_depend_on_input_rows() {
    let config = test_config("routine-constant-identifiers");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE dispatch_empty(id integer)", true);
    for (sql, state) in [
        (
            "SELECT 'dispatch_missing_relation'::regclass FROM dispatch_empty",
            "42P01",
        ),
        (
            "SELECT 'dispatch_missing_type'::regtype FROM dispatch_empty",
            "42704",
        ),
        (
            "SELECT 'dispatch_missing_function'::regproc FROM dispatch_empty",
            "42883",
        ),
        (
            "SELECT 'dispatch_missing_function()'::regprocedure FROM dispatch_empty",
            "42883",
        ),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state],
            "{sql}"
        );
    }
}

#[test]
fn stored_strict_calls_skip_constant_nulls_without_skipping_reached_inputs() {
    let config = test_config("routine-strict-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE dispatch_strict_effects START 10; \
        CREATE FUNCTION dispatch_strict(first_arg bigint,second_arg bigint) RETURNS bigint \
        LANGUAGE sql VOLATILE STRICT AS 'SELECT $2'; \
        CREATE FUNCTION dispatch_strict_default(first_arg bigint,second_arg bigint DEFAULT NULL) \
        RETURNS bigint LANGUAGE sql VOLATILE STRICT AS 'SELECT $1'; \
        CREATE FUNCTION dispatch_strict_variadic(marker bigint,VARIADIC values_ bigint[]) \
        RETURNS bigint LANGUAGE sql VOLATILE STRICT AS 'SELECT $2[2]'; \
        CREATE FUNCTION dispatch_strict_variadic_default(marker bigint,VARIADIC values_ bigint[] DEFAULT NULL) \
        RETURNS bigint LANGUAGE sql VOLATILE STRICT AS 'SELECT $2[2]'",
        true,
    );
    for sql in [
        "SELECT dispatch_strict(NULL::bigint,nextval('dispatch_strict_effects'))",
        "SELECT dispatch_strict(second_arg=>nextval('dispatch_strict_effects'),first_arg=>NULL::bigint)",
        "SELECT dispatch_strict_default(nextval('dispatch_strict_effects'))",
        "SELECT dispatch_strict_variadic_default(nextval('dispatch_strict_effects'))",
        "SELECT dispatch_strict_variadic(nextval('dispatch_strict_effects'),VARIADIC NULL::bigint[])",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["NULL"],
            "{sql}"
        );
    }
    for (sql, state) in [
        ("SELECT dispatch_strict(NULL::bigint,1/0)", "22012"),
        ("SELECT dispatch_strict(NULL::bigint,'bad')", "22P02"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state],
            "{sql}"
        );
    }
    assert_eq!(
        data_rows(&session.success(&mut engine,
            "SELECT last_value,is_called FROM dispatch_strict_effects; \
            SELECT dispatch_strict_variadic(0,NULL::bigint,nextval('dispatch_strict_effects')); \
            SELECT dispatch_strict_variadic(0,VARIADIC ARRAY[NULL::bigint,nextval('dispatch_strict_effects')]); \
            CREATE TABLE dispatch_strict_inputs(id integer,value bigint); \
            INSERT INTO dispatch_strict_inputs VALUES (1,NULL),(2,1); \
            SELECT dispatch_strict(value,nextval('dispatch_strict_effects')) FROM dispatch_strict_inputs ORDER BY id; \
            SELECT last_value,is_called FROM dispatch_strict_effects",true)),
        ["10|f", "10", "11", "NULL", "13", "13|t"]
    );
}

#[test]
fn routine_arguments_run_once_and_preserve_strict_null_effects() {
    let mut config = test_config("routine-argument-effects");
    config.max_sequences = 2;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE dispatch_target START 1000; CREATE SEQUENCE dispatch_effects START 10",
        true,
    );
    for (sql, expected) in [
        (
            "SELECT setval('dispatch_target',nextval('dispatch_effects')),currval('dispatch_effects')",
            "10|10",
        ),
        (
            "SELECT setval('dispatch_target',nextval('dispatch_effects')+nextval('dispatch_effects')),currval('dispatch_effects')",
            "23|12",
        ),
        (
            "SELECT setval('dispatch_target',nextval('dispatch_effects'),nextval('dispatch_effects')>0),currval('dispatch_effects')",
            "13|14",
        ),
        (
            "SELECT setval('dispatch_target',NULL::bigint,nextval('dispatch_effects')>0)",
            "NULL",
        ),
        ("SELECT last_value FROM dispatch_effects", "14"),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [expected],
            "{sql}"
        );
    }
    session.success(
        &mut engine,
        "CREATE TABLE dispatch_inputs(id integer,name regclass,called boolean); \
        INSERT INTO dispatch_inputs VALUES (1,NULL,true),(2,'dispatch_target',NULL)",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT setval(name,nextval('dispatch_effects'),called) FROM dispatch_inputs ORDER BY id; \
        SELECT last_value FROM dispatch_effects",true)),["NULL","NULL","16"]);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT dispatch_missing(nextval('dispatch_effects'))",
            true
        )),
        ["42883"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT last_value FROM dispatch_effects", true)),
        ["16"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT setval(NULL::regclass,1/0,true)",
            true
        )),
        ["22012"]
    );
    session.success(
        &mut engine,
        "CREATE FUNCTION dispatch_step() RETURNS bigint LANGUAGE sql VOLATILE \
        AS 'SELECT nextval(''dispatch_effects'')'",
        true,
    );
    {
        let catalog = query::storage_catalog(&engine.storage, &engine.work, 0);
        let expression = parser::parse_expression("dispatch_step()", &engine.work).unwrap();
        assert!(matches!(
            eval::constant_evaluation(expression, Some(&catalog)).unwrap(),
            Some(eval::ExpressionEvaluation::Runtime)
        ));
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT dispatch_step(); SELECT last_value FROM dispatch_effects",
            true
        )),
        ["17", "17"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT setval('dispatch_target',dispatch_step()),currval('dispatch_effects')",
            true
        )),
        ["18|18"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT last_value FROM dispatch_effects", true)),
        ["18"]
    );
    for sql in [
        "SELECT hashtextextended(NULL::text,dispatch_step())",
        "SELECT setval(NULL::regclass,dispatch_step())",
        "SELECT setval(NULL::text::regclass,dispatch_step())",
        "SELECT setval((CASE WHEN true THEN NULL::text ELSE 'dispatch_target' END)::regclass,dispatch_step())",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["NULL"],
            "{sql}"
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM dispatch_effects",
                true
            )),
            ["18"],
            "{sql}"
        );
    }
    let oversized = format!(
        "SELECT dispatch_missing({})",
        vec!["nextval('dispatch_effects')"; 24].join(",")
    );
    let rejected = session.execute(&mut engine, &oversized, true);
    assert_eq!(error_sqlstates(&rejected), ["54000"]);
    assert!(String::from_utf8_lossy(&rejected).contains("192 bytes"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT last_value FROM dispatch_effects", true)),
        ["18"]
    );
    session.success(
        &mut engine,
        "SELECT set_config('application_name','',false); \
        CREATE FUNCTION dispatch_comparison(regclass,integer) RETURNS boolean LANGUAGE sql VOLATILE \
        AS 'SELECT set_config(''application_name'',current_setting(''application_name'')||''x'',false) IS NOT NULL'; \
        CREATE OPERATOR === (LEFTARG=regclass,RIGHTARG=integer,FUNCTION=dispatch_comparison); \
        CREATE OPERATOR + (LEFTARG=regclass,RIGHTARG=integer,FUNCTION=dispatch_comparison)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT 'dispatch_target'::regclass === 1; SELECT current_setting('application_name'); \
            SELECT 'dispatch_target'::regclass + 1; SELECT current_setting('application_name')",
            true
        )),
        ["t", "x", "t", "xx"]
    );
}

#[test]
fn routine_named_variadic_and_default_inputs_keep_the_callers_effect_context() {
    let config = test_config("routine-bound-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE dispatch_effects START 10; \
        CREATE FUNCTION dispatch_pair(left_value bigint,right_value bigint) RETURNS bigint \
        LANGUAGE sql VOLATILE AS 'SELECT $1*100+$2'; \
        CREATE FUNCTION dispatch_default(value bigint DEFAULT nextval('dispatch_effects')) \
        RETURNS bigint LANGUAGE sql VOLATILE AS 'SELECT $1'; \
        CREATE FUNCTION dispatch_variadic(VARIADIC values_ bigint[]) RETURNS bigint \
        LANGUAGE sql VOLATILE AS 'SELECT $1[1]*100+$1[2]'; \
        CREATE FUNCTION dispatch_defaults(left_value bigint DEFAULT nextval('dispatch_effects'), \
        right_value bigint DEFAULT nextval('dispatch_effects')) RETURNS bigint \
        LANGUAGE sql VOLATILE AS 'SELECT $1*100+$2'",
        true,
    );
    for (sql, expected) in [
        (
            "SELECT dispatch_pair(right_value=>nextval('dispatch_effects'),left_value=>nextval('dispatch_effects')),currval('dispatch_effects')",
            "1011|11",
        ),
        (
            "SELECT dispatch_variadic(nextval('dispatch_effects'),nextval('dispatch_effects')),currval('dispatch_effects')",
            "1213|13",
        ),
        (
            "SELECT dispatch_default(),currval('dispatch_effects')",
            "14|14",
        ),
        (
            "SELECT dispatch_default(nextval('dispatch_effects')),currval('dispatch_effects')",
            "15|15",
        ),
        (
            "SELECT dispatch_defaults(right_value=>nextval('dispatch_effects')),currval('dispatch_effects')",
            "1617|17",
        ),
        (
            "SELECT dispatch_defaults(),currval('dispatch_effects')",
            "1819|19",
        ),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [expected],
            "{sql}"
        );
    }
}

#[test]
fn routine_defaults_retain_creation_bindings_and_recover_without_a_cache() {
    let mut config = test_config("routine-default-creation-bindings");
    config.max_sequences = 6;
    config.max_routines = 16;
    config.object_store_on = true;
    config.object_store_sim = true;
    config.wal_upload = false;
    config.wal_upload_sync = false;
    config.object_store_bucket = format!("routine-default-bindings-{}", std::process::id());
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let fixture =
        include_str!("../../../tests/external/differential_exact/45_routine_default_bindings.sql");
    let (exercise, remainder) = fixture.split_once("-- dependency errors").unwrap();
    let (dependency_errors, cleanup) = remainder.split_once("-- cleanup").unwrap();
    let mut budget = test_engine_budget(&config, 1 << 30);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(&mut engine, exercise, true)),
        [
            "41|f",
            "41|42|7|901|301",
            "43",
            "44",
            "45|46|7|701|302",
            "47:702",
            "703",
            "48",
            "801|f",
            "901",
            "1|2",
            "100|100"
        ]
    );
    for sql in dependency_errors
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["2BP01"],
            "{sql}"
        );
    }
    assert!(engine.checkpoint().unwrap());
    drop(session);
    drop(engine);
    std::fs::remove_dir_all(&config.data_dir).unwrap();

    let mut cold_budget = test_engine_budget(&config, 1 << 30);
    let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
    let mut cold_session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
    // The object-store simulator allocates fixture keys during empty-cache reads.
    assert_eq!(
        data_rows(&cold_session.success(
            &mut cold,
            "SELECT public.default_scalar(),public.default_helper(),public.default_quoted()",
            false
        )),
        ["49|7|303"]
    );
    cold_session.success(&mut cold, cleanup, false);
    drop(cold_session);
    drop(cold);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    std::fs::remove_dir_all(&config.data_dir).unwrap();
}

#[test]
fn routine_default_dependency_exhaustion_precedes_creation_and_effects() {
    let mut config = test_config("routine-default-dependency-capacity");
    config.max_stored_query_dependencies_per_object = 1;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE default_limit_one; CREATE SEQUENCE default_limit_two",
        true,
    );
    assert_eq!(error_sqlstates(&session.execute(&mut engine,
        "CREATE FUNCTION default_limit(left_value bigint DEFAULT nextval('default_limit_one'),right_value bigint DEFAULT nextval('default_limit_two')) RETURNS bigint LANGUAGE sql AS 'SELECT $1+$2'", true)), ["54000"]);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT to_regprocedure('default_limit(bigint,bigint)') IS NULL; SELECT is_called FROM default_limit_one; SELECT is_called FROM default_limit_two", true)), ["t", "f", "f"]);
}

#[test]
fn procedure_inputs_bind_before_effects_and_run_in_declaration_order() {
    let config = test_config("procedure-input-effects");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SEQUENCE procedure_effects START 10; \
         CREATE TABLE procedure_audit(value bigint); \
         CREATE PROCEDURE procedure_pair(left_value bigint DEFAULT nextval('procedure_effects'),right_value bigint DEFAULT nextval('procedure_effects')) \
         LANGUAGE sql AS 'INSERT INTO procedure_audit VALUES ($1*100+$2)'", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "CALL procedure_pair(right_value=>nextval('procedure_effects')); \
         CALL procedure_pair(right_value=>nextval('procedure_effects'),left_value=>nextval('procedure_effects')); \
         SELECT value FROM procedure_audit ORDER BY value; SELECT last_value FROM procedure_effects", true)), ["1011", "1213", "13"]);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "CALL procedure_missing(nextval('procedure_effects'))",
            true
        )),
        ["42883"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value FROM procedure_effects",
            true
        )),
        ["13"]
    );
}

#[test]
fn routine_default_calls_keep_their_captured_overload() {
    let mut config = test_config("routine-default-overloads");
    config.max_routines = 12;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            include_str!(
                "../../../tests/external/differential_exact/46_routine_default_overloads.sql"
            ),
            true
        )),
        ["11|1144", "22|11|1144", "11|1144", "11", "11"]
    );
}

#[test]
fn routine_inputs_share_conversions_order_and_strictness_in_every_context() {
    let mut config = test_config("routine-input-contexts");
    config.max_routines = 8;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let fixture =
        include_str!("../../../tests/external/differential_exact/47_routine_input_contexts.sql");
    let (setup, rest) = fixture.split_once("-- conversion errors").unwrap();
    let (errors, finish) = rest.split_once("-- after errors").unwrap();
    assert_eq!(
        data_rows(&session.success(&mut engine, setup, true)),
        ["1011", "1213", "14"]
    );
    let errors: Vec<_> = errors
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(errors.len(), 7);
    for (sql, state) in errors.into_iter().zip([
        "22P02", "22012", "22P02", "22012", "22012", "22P02", "42883",
    ]) {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state],
            "{sql}"
        );
    }
    assert_eq!(
        data_rows(&session.success(&mut engine, finish, true)),
        [
            "15",
            "16",
            "1",
            "17",
            "2",
            "18",
            "19",
            "0",
            "19",
            "1025",
            "20",
            "0",
            "21",
            "7|22",
            "7|7",
            "22",
            "bigint",
            "23",
            "fillfactor|80",
            "flag|NULL",
            "0"
        ]
    );
}

#[test]
fn pending_routine_calls_keep_bound_types_and_structural_values() {
    let config = test_config("pending-routine-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/48_pending_routine_types.sql");
    let (calls, replacement) = sql.split_once("-- replacement timing").unwrap();
    let mut rows = data_rows(&session.success(&mut engine, calls, true));
    rows.extend(data_rows(&session.success(&mut engine, replacement, true)));
    assert_eq!(
        rows,
        [
            "7|9|{1,NULL,3}",
            "7|9|{1,NULL,3}",
            "1|sql",
            "2|pl",
            "7|8",
            "3",
            "3",
            "4",
            "4",
            "5",
            "6",
            "7|7",
            "{1,NULL,3}|7",
            "8|8",
            "{2,NULL,4}|8",
            "12|named",
            "13|fallback",
            "101",
            "6|2|4|2|0|1"
        ]
    );
}

#[test]
fn record_call_expansion_preserves_scalar_effects_and_shared_set_producers() {
    let config = test_config("record-call-expansion");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/49_record_call_expansion.sql");
    let (valid, _) = sql.split_once("-- invalid field").unwrap();
    assert_eq!(
        data_rows(&session.success(&mut engine, valid, true)),
        [
            "10|11",
            "11",
            "12|13",
            "13",
            "1|14|15",
            "2|16|17",
            "17",
            "18|19",
            "set|set",
            "untyped|untyped",
            "integer",
            "20|20",
            "21|21",
            "20",
            "(21,21)|(22,22)",
            "(22,22)|(23,23)",
            "22",
            "(23,23)|(24,24)",
            "(24,24)|(25,25)",
            "24",
            "25|25",
            "26|26",
            "27|27",
            "26",
            "27|28",
            "29|30",
            "31|32",
            "33|34",
            "34",
            "a|1",
            "NULL|2",
            "c|3",
            "x|1",
            "NULL|2",
            "z|3",
            "7|named",
            "8|default",
            "4|7",
            "5|7"
        ]
    );
    for sql in ["SELECT (ARRAY[1,2]).f1", "SELECT (ARRAY[1,2]).x"] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42809"]
        );
    }
    session.success(
        &mut engine,
        "CREATE FUNCTION capacity_record(OUT x integer,OUT y integer) LANGUAGE sql AS 'SELECT 1,2'",
        true,
    );
    for (additional, state) in [(MAX_PROJ - 2, None), (MAX_PROJ - 1, Some("54011"))] {
        let mut sql = String::from("SELECT (capacity_record()).*");
        for _ in 0..additional {
            sql.push_str(",1");
        }
        match state {
            None => {
                let output = session.success(&mut engine, &sql, true);
                assert_eq!(data_rows(&output)[0].split('|').count(), MAX_PROJ);
            }
            Some(state) => assert_eq!(
                error_sqlstates(&session.execute(&mut engine, &sql, true)),
                [state]
            ),
        }
    }
}

#[test]
fn retained_query_handoffs_count_one_scan_and_each_source_row_once() {
    let config = test_config("retained-query-scan-counts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE query_scan_source(value integer); \
         INSERT INTO query_scan_source VALUES(1),(2),(3); \
         CREATE TABLE query_scan_audit(value integer); \
         CREATE FUNCTION query_scan_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO query_scan_audit VALUES($1); SELECT $1$$",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT query_scan_write(value) FROM query_scan_source; \
             SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables \
             WHERE relid='query_scan_source'::regclass",
            true,
        )),
        ["1", "2", "3", "1|3"]
    );
    session.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM query_scan_audit", true,)),
        ["0"]
    );
}

#[test]
fn qualified_builtin_types_survive_mutable_function_handoffs() {
    let config = test_config("qualified-builtin-handoff-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE qualified_builtin_source(value integer); \
         INSERT INTO qualified_builtin_source VALUES(2); \
         CREATE TABLE qualified_builtin_audit(value integer); \
         CREATE FUNCTION qualified_builtin_write(value integer) RETURNS integer \
         LANGUAGE sql VOLATILE AS \
         $$INSERT INTO qualified_builtin_audit VALUES($1); SELECT $1$$",
        true,
    );
    for prefix in ["", "pg_catalog."] {
        let sql = format!(
            "SELECT {prefix}abs(-7.5)+1.0, \
             {prefix}ln({prefix}abs(-7.5)+1.0), \
             {prefix}hashfloat8extended(0::float8,23)+1, \
             {prefix}hashtext(NULL::text), \
             {prefix}hashtextextended(NULL::text,0), \
             qualified_builtin_write(value) FROM qualified_builtin_source"
        );
        let output = session.success(&mut engine, &sql, true);
        assert_eq!(
            row_description_type_oids(&output),
            [1700, 1700, 20, 23, 20, 23]
        );
        let rows = data_rows(&output);
        assert_eq!(rows.len(), 1);
        let values = rows[0].split('|').collect::<Vec<_>>();
        assert_eq!(values[0], "8.5");
        assert!((values[1].parse::<f64>().unwrap() - 8.5f64.ln()).abs() < 1e-12);
        assert_eq!(&values[2..], &["24", "NULL", "NULL", "2"]);
    }
    session.success(
        &mut engine,
        "CREATE SCHEMA qualified_builtin_user; \
         CREATE FUNCTION qualified_builtin_user.abs(value numeric) RETURNS text \
         LANGUAGE sql IMMUTABLE AS $$SELECT 'custom'::text$$; \
         CREATE FUNCTION qualified_builtin_user.hashtextextended(value text,seed bigint) \
         RETURNS numeric LANGUAGE sql IMMUTABLE AS $$SELECT 123.5$$",
        true,
    );
    let output = session.success(
        &mut engine,
        "SELECT qualified_builtin_user.abs(-7.5), \
         qualified_builtin_user.hashtextextended('label',0)+1.0, \
         qualified_builtin_write(value) FROM qualified_builtin_source",
        true,
    );
    assert_eq!(row_description_type_oids(&output), [25, 1700, 23]);
    assert_eq!(data_rows(&output), ["custom|124.5|2"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*) FROM qualified_builtin_audit",
            true,
        )),
        ["3"]
    );
}

#[test]
fn decoded_guc_values_keep_their_spelling_and_resolve_search_path_names() {
    let config = test_config("decoded-guc-values");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql = include_str!(
        "../../../tests/external/differential_exact/56_search_path_value_assignment.sql"
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        [
            "GUC_RAW,public",
            "GUC_RAW,public|7",
            "  \"Guc Space\" , public  ",
            "8",
            "\x0bpublic\x0b",
            "public",
            "public\x0b,\x0bpg_catalog",
            "public",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "\"ééééééééééééééééééééééééééééééééa\"",
            "\"éééééééééééééééééééééééééééééééééééééééééééééééééé\"",
            "guc_raw, public",
            "\"guc_raw,public\"",
            "'public'",
            "  abc  ",
            "'abc'",
            "DEFAULT",
            "DEFAULT",
            "DEFAULT",
            "\"$user\", public",
            "GUC_RAW,public",
            "public",
            "GUC_RAW,public|public",
            "GUC_RAW,public",
            "  \"Guc Space\" , public  ",
            "GUC_RAW,public",
            "public",
        ]
    );
    session.success(
        &mut engine,
        "SELECT set_config('search_path','GUC_RAW,public',false)",
        true,
    );
    for value in [
        "foo bar",
        "public\x0bpg_catalog",
        "public,\x0b",
        "public,,pg_catalog",
        "public,",
        "\"unclosed",
        "\"public\"tail",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("SELECT set_config('search_path','{value}',false)"),
                true
            )),
            ["22023"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT current_setting('search_path')", true)),
            ["GUC_RAW,public"]
        );
    }
}

#[test]
fn sql_setting_strings_are_decoded_once_before_guc_assignment() {
    let config = test_config("decoded-sql-setting-values");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let sql =
        include_str!("../../../tests/external/differential_exact/57_decoded_sql_settings.sql");
    assert_eq!(
        data_rows(&session.success(&mut engine, sql, true)),
        [
            "a'b",
            "dollar'quote",
            "doubled'quote",
            "a\\x09b",
            "DEFAULT",
            "1",
            "\"Case\tName\", public",
            "\"quote'name\", \"dollar,name\", \"Mixed Case\"",
            "ISO, DMY",
            "function'value|outside",
            "changed'value|outside",
            "outside|t",
            "outside|t",
            "DEFAULT",
            "application_name=role'value",
            "application_name=database'value",
            "UTC",
            "<+02>-02",
            "<+02>-02",
            "<+02>-02",
            "<-02>+02",
            "<+02:30>-02:30",
            "9000",
            "+02:00",
            "-7200",
            "<+02:30>-02:30|TimeZone=2.5",
            "<+00:00:01>-00:00:01|TimeZone=0.0005",
            "TimeZone=0.0005|t|DateStyle=sql, dmy",
            "TimeZone=0.0005",
            "DateStyle=sql, dmy",
        ]
    );
    for sql in [
        "SET application_name TO one,two",
        "SET nonexistent_parameter TO one,two",
        "SET server_version TO one,two",
        "SET TIME ZONE 'DEFAULT'",
        "SET TIME ZONE 'LOCAL'",
        "SET TIME ZONE $$DEFAULT$$",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["22023"],
            "{sql}"
        );
    }
    for sql in [
        "ALTER ROLE postgres SET nonexistent_parameter FROM CURRENT",
        "ALTER DATABASE postgres SET nonexistent_parameter FROM CURRENT",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42704"],
            "{sql}"
        );
    }

    for sql in [
        "ALTER ROLE postgres RESET nonexistent_parameter",
        "ALTER DATABASE postgres RESET nonexistent_parameter",
        "ALTER ROLE postgres RESET server_version",
        "ALTER DATABASE postgres RESET server_version",
        "ALTER ROLE postgres SET nonexistent_parameter TO DEFAULT",
        "ALTER DATABASE postgres SET nonexistent_parameter TO DEFAULT",
        "ALTER ROLE postgres SET server_version TO DEFAULT",
        "ALTER DATABASE postgres SET server_version TO DEFAULT",
    ] {
        session.success(&mut engine, sql, true);
    }
    session.success(&mut engine, "CREATE FUNCTION public.current_timezone_source() RETURNS integer LANGUAGE sql AS 'SELECT 1'; SELECT set_config('timezone','0.0005',false)", true);
    for sql in [
        "CREATE FUNCTION public.invalid_captured_timezone() RETURNS integer LANGUAGE sql SET timezone FROM CURRENT AS 'SELECT 1'",
        "ALTER FUNCTION public.current_timezone_source() SET timezone FROM CURRENT",
        "ALTER ROLE postgres SET timezone FROM CURRENT",
        "ALTER DATABASE postgres SET timezone FROM CURRENT",
        "SET timezone FROM CURRENT",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["22023"],
            "{sql}"
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT current_setting('timezone')", true)),
            ["<+00:00:01>-00:00:01"]
        );
    }
    session.success(&mut engine, "RESET timezone; ALTER FUNCTION public.current_timezone_source() SET application_name TO 'saved-function'; ALTER ROLE postgres SET application_name TO 'saved-role'; ALTER DATABASE postgres SET application_name TO 'saved-database'", true);
    session.success(
        &mut engine,
        "ALTER SYSTEM SET application_name TO 'saved-system'",
        true,
    );
    for (sql, expected) in [
        (
            "ALTER ROLE postgres RESET nonexistent_parameter",
            Some("42704"),
        ),
        (
            "ALTER DATABASE postgres RESET nonexistent_parameter",
            Some("42704"),
        ),
        ("ALTER ROLE postgres RESET max_connections", Some("55P02")),
        (
            "ALTER DATABASE postgres RESET max_connections",
            Some("55P02"),
        ),
        ("ALTER SYSTEM RESET max_connections", None),
        (
            "ALTER ROLE postgres SET nonexistent_parameter TO DEFAULT",
            Some("42704"),
        ),
        (
            "ALTER DATABASE postgres SET nonexistent_parameter TO DEFAULT",
            Some("42704"),
        ),
        (
            "ALTER ROLE postgres SET server_version TO DEFAULT",
            Some("55P02"),
        ),
        (
            "ALTER DATABASE postgres SET server_version TO DEFAULT",
            Some("55P02"),
        ),
        ("ALTER SYSTEM SET max_connections TO DEFAULT", None),
        (
            "ALTER FUNCTION public.current_timezone_source() RESET nonexistent_parameter",
            Some("42704"),
        ),
        ("ALTER SYSTEM RESET nonexistent_parameter", Some("42704")),
        ("ALTER ROLE postgres RESET server_version", Some("55P02")),
        (
            "ALTER DATABASE postgres RESET server_version",
            Some("55P02"),
        ),
        (
            "ALTER FUNCTION public.current_timezone_source() RESET server_version",
            Some("55P02"),
        ),
        ("ALTER SYSTEM RESET server_version", Some("55P02")),
    ] {
        if let Some(expected) = expected {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, sql, true)),
                [expected],
                "{sql}"
            );
        } else {
            session.success(&mut engine, sql, true);
        }
        assert_eq!(data_rows(&session.success(&mut engine, "SELECT proconfig[1] FROM pg_proc WHERE proname='current_timezone_source'; SELECT setconfig[1] FROM pg_db_role_setting WHERE setrole='postgres'::regrole AND setdatabase=0; SELECT setconfig[1] FROM pg_db_role_setting WHERE setrole=0 AND setdatabase=(SELECT oid FROM pg_database WHERE datname='postgres')", true)), ["application_name=saved-function", "application_name=saved-role", "application_name=saved-database"]);
        assert!(
            engine
                .storage
                .system_settings()
                .any(|(_, setting)| setting.live
                    && setting.name.as_str() == "application_name"
                    && setting.value.as_str() == "saved-system")
        );
    }
    session.success(&mut engine, "ALTER SYSTEM RESET application_name", true);
    session.success(&mut engine, "ALTER ROLE postgres RESET application_name; ALTER DATABASE postgres RESET application_name; DROP FUNCTION public.current_timezone_source()", true);
}

#[test]
fn search_path_sql_capacity_errors_preserve_the_selected_value() {
    let config = test_config("search-path-sql-capacity");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for (name, expected) in [
        ("a".repeat(63), "a".repeat(63)),
        (
            format!("{}a", "é".repeat(31)),
            format!("\"{}a\"", "é".repeat(31)),
        ),
    ] {
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                &format!("SET search_path TO {name}; SHOW search_path"),
                true,
            )),
            [expected]
        );
    }
    session.success(
        &mut engine,
        "SELECT set_config('search_path','GUC_RAW,public',false)",
        true,
    );
    for name in [
        "a".repeat(64),
        "a".repeat(65),
        "é".repeat(32),
        format!("{}a", "é".repeat(32)),
    ] {
        for sql in [
            format!("SET search_path TO {name}"),
            format!("SET search_path TO \"{name}\""),
        ] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, &sql, true)),
                ["42622"]
            );
            assert_eq!(
                data_rows(&session.success(&mut engine, "SHOW search_path", true)),
                ["GUC_RAW,public"]
            );
        }
    }
    for value in ["a".repeat(129), "é".repeat(64), "é".repeat(65)] {
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("SET search_path TO '{value}'"),
                true,
            )),
            ["22023"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SHOW search_path", true)),
            ["GUC_RAW,public"]
        );
    }
}

#[test]
fn saved_guc_values_preserve_raw_spelling_without_runtime_allocations() {
    let guc = GucState::new();
    let raw = "  \"Mixed Case\" , public  ";
    let configs = [crate::storage::RoutineConfig {
        name: crate::storage::SqlName::parse("search_path").unwrap(),
        value: crate::util::StackStr::from_str(raw),
    }];
    crate::mem::guard::forbid_alloc(|| {
        guc.set_connection_default(
            "search_path",
            "GUC_RAW,public",
            guc::ConnectionDefaultSource::Role,
        )
        .unwrap();
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
        guc.begin_transaction();
        guc.set_config("search_path", Some("public"), false)
            .unwrap();
        guc.commit_transaction();
        guc.begin_transaction();
        guc.set_config("search_path", None, false).unwrap();
        guc.commit_transaction();
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
        {
            let _scope = guc.enter_routine_configs(&configs).unwrap();
            assert_eq!(guc.get_owned("search_path").unwrap().as_str(), raw);
        }
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
        guc.begin_transaction();
        guc.set_config("search_path", Some(raw), true).unwrap();
        guc.commit_transaction();
        assert_eq!(
            guc.get_owned("search_path").unwrap().as_str(),
            "GUC_RAW,public"
        );
    });
}

#[test]
fn reset_xmloption_restores_the_connection_default_without_allocating() {
    let guc = GucState::new();
    let configs = [crate::storage::RoutineConfig {
        name: crate::storage::SqlName::parse("application_name").unwrap(),
        value: crate::util::StackStr::from_str("scoped"),
    }];
    crate::mem::guard::forbid_alloc(|| {
        guc.set_connection_default("xmloption", "document", guc::ConnectionDefaultSource::Role)
            .unwrap();
        guc.begin_transaction();
        guc.set_config("xmloption", Some("content"), false).unwrap();
        guc.commit_transaction();
        assert_eq!(guc.get_owned("xmloption").unwrap().as_str(), "content");
        guc.begin_transaction();
        guc.reset("xmloption").unwrap();
        guc.commit_transaction();
        assert_eq!(guc.get_owned("xmloption").unwrap().as_str(), "document");
        assert!(guc.render().xml_document);
        guc.begin_transaction();
        {
            let _scope = guc.enter_routine_configs(&configs).unwrap();
            guc.set_config("xmloption", Some("content"), false).unwrap();
        }
        assert_eq!(guc.get_owned("xmloption").unwrap().as_str(), "content");
        guc.commit_transaction();
        assert!(!guc.render().xml_document);
    });
}

#[test]
fn bound_builtin_casts_keep_indexed_point_reads() {
    let config = test_config("retained-query-indexed-casts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE query_indexed_casts(id integer PRIMARY KEY,payload integer); \
         INSERT INTO query_indexed_casts \
         SELECT value,value*10 FROM generate_series(1,1024) AS source(value)",
        true,
    );
    session.success(&mut engine, "BEGIN", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT payload::text,-payload,payload+1 FROM query_indexed_casts WHERE id=511; \
             SELECT seq_scan,seq_tup_read,idx_scan,idx_tup_fetch \
             FROM pg_stat_xact_user_tables WHERE relname='query_indexed_casts'",
            true,
        )),
        ["5110|-5110|5111", "0|0|1|1"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}
#[test]
fn application_names_escape_non_printable_bytes_without_allocating() {
    let guc = GucState::new();
    let exactly_full = "é".repeat(8);
    let full_expected = "\\xc3\\xa9".repeat(8);
    let overflowing = "é".repeat(9);
    crate::mem::guard::forbid_alloc(|| {
        for (value, expected) in [
            (" plain !~\\ ", " plain !~\\ "),
            ("a\tb", "a\\x09b"),
            (
                "\x01\t\n\x0b\x0c\r\x1b\x1f\x7f",
                "\\x01\\x09\\x0a\\x0b\\x0c\\x0d\\x1b\\x1f\\x7f",
            ),
            ("é中🦀", "\\xc3\\xa9\\xe4\\xb8\\xad\\xf0\\x9f\\xa6\\x80"),
            ("\\x09", "\\x09"),
        ] {
            assert_eq!(
                guc.set_config("application_name", Some(value), false)
                    .unwrap()
                    .as_str(),
                expected
            );
            assert_eq!(
                guc.get_owned("application_name").unwrap().as_str(),
                expected
            );
        }
        assert_eq!(
            guc.set_config("application_name", Some(&exactly_full), false)
                .unwrap()
                .as_str(),
            full_expected
        );
        assert_eq!(
            guc.set_config("application_name", Some(&overflowing), false)
                .unwrap_err()
                .sqlstate,
            "22023"
        );
        assert_eq!(
            guc.get_owned("application_name").unwrap().as_str(),
            full_expected
        );
        assert_eq!(
            guc.set_config("application_name", Some("zero\0byte"), false)
                .unwrap_err()
                .sqlstate,
            "22021"
        );
        assert_eq!(
            guc.get_owned("application_name").unwrap().as_str(),
            full_expected
        );
    });
}
#[test]
fn datestyle_assignments_preserve_reset_defaults_and_reject_conflicts_atomically() {
    let guc = GucState::new();
    guc.set_connection_default(
        "DateStyle",
        "SQL, YMD",
        guc::ConnectionDefaultSource::Database,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        guc.begin_transaction();
        for (value, expected) in [
            ("", "German, DMY"),
            (" \t\n\u{b}", "German, DMY"),
            ("DEFAULT", "SQL, YMD"),
            ("DEFAULT,ISO", "ISO, YMD"),
            ("ISO,DEFAULT", "ISO, YMD"),
            ("DEFAULT,German", "German, DMY"),
            ("German,DEFAULT", "German, YMD"),
            ("MDY,DEFAULT,German", "German, MDY"),
            ("German,DEFAULT,MDY", "German, MDY"),
            ("ISO,ISO", "ISO, DMY"),
            ("\"ISO\", \"DMY\"", "ISO, DMY"),
            ("PostgreSQL", "Postgres, DMY"),
            ("European", "German, DMY"),
            ("US", "German, MDY"),
            ("NonEuro", "German, MDY"),
            ("German,MDY,German", "German, MDY"),
            ("DEFAULT,DEFAULT", "SQL, YMD"),
        ] {
            guc.set_config("DateStyle", Some("German,DMY"), false)
                .unwrap();
            assert_eq!(
                guc.set_config("DateStyle", Some(value), false)
                    .unwrap()
                    .as_str(),
                expected,
                "{value:?}"
            );
        }
        guc.set_config("DateStyle", Some("DEFAULT"), false).unwrap();
        assert_eq!(guc.get_owned("DateStyle").unwrap().as_str(), "SQL, YMD");
        guc.set_config("DateStyle", Some("German,MDY"), false)
            .unwrap();
        for value in [
            "ISO,SQL",
            "DMY,MDY",
            "ISO SQL",
            "ISO,",
            "ISO,,MDY",
            "\"ISO,DMY\"",
            "\"ISO\"suffix",
            "\"ISO",
            "\"IS\"\"O\"",
            "'DEFAULT'",
            "German,Euro,US",
            "ISO,SQL,DEFAULT",
            "SQL,DEFAULT,ISO",
        ] {
            assert_eq!(
                guc.set_config("DateStyle", Some(value), false)
                    .unwrap_err()
                    .sqlstate,
                "22023",
                "{value:?}"
            );
            assert_eq!(guc.get_owned("DateStyle").unwrap().as_str(), "German, MDY");
        }
        assert_eq!(
            guc.set_config("DateStyle", None, false).unwrap().as_str(),
            "SQL, YMD"
        );
        guc.commit_transaction();
    });
}

#[test]
fn datestyle_sql_values_apply_cumulative_components_and_quoted_defaults() {
    let config = test_config("decoded-datestyle-components");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SET DateStyle TO SQL,YMD; SELECT current_setting('DateStyle'); \
         SET DateStyle TO German; SELECT current_setting('DateStyle'); \
         SELECT set_config('DateStyle','German,DEFAULT',false); \
         SET DateStyle TO 'DEFAULT'; SELECT current_setting('DateStyle'); \
         SELECT set_config('DateStyle','\"SQL\",\"DMY\"',false); \
         SELECT set_config('DateStyle','',false)",
            true
        )),
        [
            "SQL, YMD",
            "German, DMY",
            "German, MDY",
            "ISO, MDY",
            "SQL, DMY",
            "SQL, DMY"
        ]
    );
    for sql in [
        "SET DateStyle TO ISO,SQL",
        "SELECT set_config('DateStyle','DMY,MDY',false)",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["22023"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT current_setting('DateStyle')", true)),
            ["SQL, DMY"]
        );
    }
}

#[test]
fn invalid_setting_values_use_parameter_errors_without_changing_values() {
    let guc = GucState::new();
    crate::mem::guard::forbid_alloc(|| {
        for (name, invalid) in [
            ("check_function_bodies", "not_a_value"),
            ("row_security", "not_a_value"),
            ("event_triggers", "not_a_value"),
            ("default_transaction_read_only", "not_a_value"),
            ("default_transaction_deferrable", "not_a_value"),
            ("xmloption", "not_a_value"),
            ("track_functions", "not_a_value"),
            ("default_text_search_config", ""),
            ("standard_conforming_strings", "not_a_value"),
            ("synchronize_seqscans", "not_a_value"),
            ("idle_in_transaction_session_timeout", "not_a_value"),
            ("transaction_timeout", "not_a_value"),
            ("lock_timeout", "'0'"),
            ("statement_timeout", "2147483648"),
        ] {
            let prior = guc.get_owned(name).unwrap();
            assert_eq!(
                guc.set_config(name, Some(invalid), false)
                    .unwrap_err()
                    .sqlstate,
                "22023",
                "{name}"
            );
            assert_eq!(guc.get_owned(name).unwrap().as_str(), prior.as_str());
        }
        for name in ["transaction_read_only", "transaction_deferrable"] {
            assert_eq!(
                guc.current_transaction_value(name, Some("not_a_value"))
                    .unwrap()
                    .unwrap_err()
                    .sqlstate,
                "22023"
            );
        }
    });
}

#[test]
fn rendered_routine_definitions_preserve_quoted_names_and_saved_setting_input() {
    let config = test_config("quoted-routine-definition");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        r#"
        CREATE SCHEMA "Quoted App";
        CREATE FUNCTION "Quoted App"."Quoted Routine"("input label" integer)
        RETURNS TABLE ("output label" integer) LANGUAGE sql
        SET timezone TO '0.0005'
        AS $$SELECT $1 + EXTRACT(timezone FROM current_timestamp)::integer$$;
    "#,
        true,
    );
    let definitions = data_rows(&session.success(
        &mut engine,
        r#"SELECT pg_get_functiondef('"Quoted App"."Quoted Routine"(integer)'::regprocedure)"#,
        true,
    ));
    assert_eq!(definitions.len(), 1);
    let definition = &definitions[0];
    for quoted in [
        r#""Quoted App"."Quoted Routine""#,
        r#""input label""#,
        r#""output label""#,
        r#"SET "TimeZone" TO '0.0005'"#,
    ] {
        assert!(definition.contains(quoted), "{definition}");
    }
    session.success(
        &mut engine,
        r#"DROP FUNCTION "Quoted App"."Quoted Routine"(integer)"#,
        true,
    );
    session.success(&mut engine, definition, true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            r#"SELECT * FROM "Quoted App"."Quoted Routine"(7)"#,
            true
        )),
        ["8"]
    );
}

#[test]
fn fixed_settings_parse_values_before_checking_supported_behavior() {
    let guc = GucState::new();
    crate::mem::guard::forbid_alloc(|| {
        for value in ["on", "t", "tr", "tru", "true", "y", "yes", "1"] {
            assert_eq!(
                guc.set_config("standard_conforming_strings", Some(value), false)
                    .unwrap()
                    .as_str(),
                "on"
            );
        }
        for value in ["off", "of", "f", "false", "n", "no", "0"] {
            assert_eq!(
                guc.set_config("synchronize_seqscans", Some(value), false)
                    .unwrap()
                    .as_str(),
                "off"
            );
        }
        for name in ["idle_in_transaction_session_timeout", "transaction_timeout"] {
            assert_eq!(
                guc.set_config(name, Some("0ms"), false).unwrap().as_str(),
                "0"
            );
            assert_eq!(
                guc.set_config(name, Some("1ms"), false)
                    .unwrap_err()
                    .sqlstate,
                "0A000"
            );
        }
        for name in ["standard_conforming_strings", "synchronize_seqscans"] {
            assert_eq!(
                guc.set_config(name, Some("o"), false).unwrap_err().sqlstate,
                "22023"
            );
        }
        for (name, value) in [
            ("standard_conforming_strings", "off"),
            ("synchronize_seqscans", "on"),
            ("default_table_access_method", "HEAP"),
        ] {
            assert_eq!(
                guc.set_config(name, Some(value), false)
                    .unwrap_err()
                    .sqlstate,
                "0A000"
            );
        }
    });
}

#[test]
fn timezone_numeric_offsets_and_intervals_use_postgresql_bounds() {
    let guc = GucState::new();
    crate::mem::guard::forbid_alloc(|| {
        for (value, expected) in [
            ("16", "<+16>-16"),
            ("24", "<+24>-24"),
            ("1e1", "<+10>-10"),
            ("0.0001", "<+00>-00"),
            ("-0.0005", "<-00:00:01>+00:00:01"),
            ("167.999999", "<+167:59:59>-167:59:59"),
            ("<+02:30>-02:30", "<+02:30>-02:30"),
            ("<+00>-00", "<+00>-00"),
        ] {
            guc.set_config("timezone", Some(value), false).unwrap();
            assert_eq!(
                guc.get_owned("timezone").unwrap().as_str(),
                expected,
                "{value}"
            );
        }
        for value in [
            "168",
            "-168",
            "1000",
            "Infinity",
            "<+167:59:59>-167:59:59",
            "<-167:59:59>+167:59:59",
        ] {
            let prior = guc.get_owned("timezone").unwrap();
            assert_eq!(
                guc.set_config("timezone", Some(value), false)
                    .unwrap_err()
                    .sqlstate,
                "22023",
                "{value}"
            );
            assert_eq!(guc.get_owned("timezone").unwrap().as_str(), prior.as_str());
        }
        let prior = guc.get_owned("timezone").unwrap();
        assert_eq!(
            guc.set_config("timezone", Some("NaN"), false)
                .unwrap_err()
                .sqlstate,
            "0A000"
        );
        assert_eq!(guc.get_owned("timezone").unwrap().as_str(), prior.as_str());
        for (micros, expected) in [
            (500_000, "<+00>-00"),
            (1_500_000, "<+00:00:01>-00:00:01"),
            (-1_500_000, "<-00:00:01>+00:00:01"),
            (604_799_000_000, "<+167:59:59>-167:59:59"),
            (-604_799_000_000, "<-167:59:59>+167:59:59"),
        ] {
            guc.set_time_zone_interval(
                super::super::types::Interval {
                    months: 0,
                    days: 0,
                    micros,
                },
                false,
            )
            .unwrap();
            assert_eq!(guc.get_owned("timezone").unwrap().as_str(), expected);
        }
        for interval in [
            super::super::types::Interval {
                months: 1,
                days: 0,
                micros: 0,
            },
            super::super::types::Interval {
                months: 0,
                days: 1,
                micros: 0,
            },
            super::super::types::Interval {
                months: 0,
                days: 0,
                micros: 604_800_000_000,
            },
            super::super::types::Interval {
                months: 0,
                days: 0,
                micros: -604_800_000_000,
            },
        ] {
            let prior = guc.get_owned("timezone").unwrap();
            assert_eq!(
                guc.set_time_zone_interval(interval, false)
                    .unwrap_err()
                    .sqlstate,
                "22023"
            );
            assert_eq!(guc.get_owned("timezone").unwrap().as_str(), prior.as_str());
        }
    });
}

#[test]
fn qualified_catalog_calls_keep_their_namespace_and_sql_syntax_boundaries() {
    let config = test_config("qualified-catalog-call-boundaries");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let fixture =
        include_str!("../../../tests/external/differential_exact/60_qualified_routine_calls.sql");
    let mut requests = fixture.split("-- next request");
    let valid = session.success(&mut engine, requests.next().unwrap(), true);
    assert_eq!(
        data_rows(&valid),
        [
            "999|999|shadow",
            "1|2|3|3|t|t",
            "3",
            "103|3|3|2|2",
            "1|1|1",
            "2|2|2",
            "2|0|0|t|t",
            "t|t|t|x",
            "7|8|1.25",
            "qualified_call_values",
            "17",
        ]
    );
    for request in requests {
        let output = session.execute(&mut engine, request, true);
        if request.contains("SELECT pg_catalog.nextval('qualified_error_values')") {
            assert!(
                error_sqlstates(&output).is_empty(),
                "{}",
                String::from_utf8_lossy(&output)
            );
            assert_eq!(data_rows(&output), ["1"]);
            continue;
        }
        let expected = if request.contains("'x':1") {
            "42601"
        } else if request.contains("pg_catalog.array[")
            || request.contains("pg_catalog.current_date;")
            || request.contains("AS hidden")
        {
            "42P01"
        } else {
            "42883"
        };
        assert_eq!(
            error_sqlstates(&output),
            [expected],
            "{}: {}",
            request,
            String::from_utf8_lossy(&output)
        );
        if request.contains("pg_catalog.json_object('x':1)") {
            assert!(String::from_utf8_lossy(&output).contains("syntax error at or near \":\""));
        }
    }
    session.success(
        &mut engine,
        "CREATE SCHEMA reference_scope; CREATE TABLE reference_scope.source(value integer); \
         INSERT INTO reference_scope.source VALUES(7)",
        true,
    );
    for query in [
        "SELECT reference_scope.source.value FROM reference_scope.source",
        "SELECT selected.value FROM reference_scope.source AS selected",
        "SELECT (SELECT selected.value) FROM reference_scope.source AS selected",
        "SELECT (SELECT reference_scope.source.value) FROM reference_scope.source",
        "SELECT (SELECT selected.value FROM (VALUES(8)) AS inner_row(other)) FROM reference_scope.source AS selected",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, query, true)),
            ["7"],
            "{query}"
        );
    }
    for (query, state) in [
        ("SELECT missing.value", "42P01"),
        ("SELECT pg_catalog.pg_class.relname", "42P01"),
        ("SELECT \"a.b\".value", "42P01"),
        ("SELECT value", "42703"),
        (
            "SELECT source.value FROM reference_scope.source AS selected",
            "42P01",
        ),
        (
            "SELECT selected.missing FROM reference_scope.source AS selected",
            "42703",
        ),
        (
            "SELECT (SELECT selected.missing) FROM reference_scope.source AS selected",
            "42703",
        ),
        (
            "SELECT (SELECT selected.value FROM (VALUES(8)) AS selected(other)) FROM reference_scope.source AS selected",
            "42703",
        ),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, query, true)),
            [state],
            "{query}"
        );
    }
    let missing = session.execute(
        &mut engine,
        "SELECT (SELECT selected.missing) FROM reference_scope.source AS selected",
        true,
    );
    assert_eq!(error_sqlstates(&missing), ["42703"]);
    assert!(String::from_utf8_lossy(&missing).contains("column selected.missing does not exist"));
    for query in [
        "SELECT reference_scope.source.value FROM reference_scope.source AS selected",
        "SELECT (SELECT reference_scope.source.value) FROM reference_scope.source AS selected",
    ] {
        let hidden = session.execute(&mut engine, query, true);
        assert_eq!(error_sqlstates(&hidden), ["42P01"], "{query}");
        assert!(
            String::from_utf8_lossy(&hidden)
                .contains("invalid reference to FROM-clause entry for table \"source\""),
            "{query}"
        );
    }
    for (query, table) in [
        ("SELECT (SELECT pg_catalog.pg_class.relname)", "pg_class"),
        ("SELECT (SELECT \"a.b\".value)", "a.b"),
    ] {
        let output = session.execute(&mut engine, query, true);
        assert_eq!(error_sqlstates(&output), ["42P01"], "{query}");
        assert!(
            String::from_utf8_lossy(&output)
                .contains(&format!("missing FROM-clause entry for table \"{table}\"")),
            "{query}"
        );
    }
    session.success(
        &mut engine,
        "DROP TABLE reference_scope.source; DROP SCHEMA reference_scope",
        true,
    );
}

#[test]
fn qualified_set_routines_preserve_namespaces_records_and_argument_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("qualified-set-routine-records");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential_exact/61_qualified_set_routines.sql"
        );
        let requests = fixture.split("-- next request").collect::<Vec<_>>();
        let output = session.success(&mut engine, requests[0], true);
        assert_eq!(
            data_rows(&output),
            [
                "10|1",
                "NULL|2",
                "30|3",
                "x|1",
                "NULL|2",
                "z|3",
                "0",
                "0",
                "1|1",
                "2|2",
                "3|3",
                "4|4",
                "40|1",
                "50|2",
                "2",
                "3",
                "4",
                "5",
                "host|localhost",
                "port|5432",
                "a",
                "a",
                "1|1",
                "2|2",
            ]
        );
        for request in &requests[1..4] {
            let output = session.execute(&mut engine, request, true);
            assert_eq!(
                error_sqlstates(&output),
                ["42883"],
                "{}: {}",
                request,
                String::from_utf8_lossy(&output)
            );
        }
        assert_eq!(
            data_rows(&session.success(&mut engine, requests[4], true)),
            [
                "1",
                "8|1",
                "9|2",
                "102|3|103|104|(5)",
                "1|3",
                "2|4",
                "99",
                "1",
                "2"
            ]
        );
        for request in &requests[5..] {
            assert_eq!(
                error_sqlstates(&session.execute(&mut engine, request, true)),
                ["42601"],
                "{}",
                request
            );
        }
        let output = session.success(
            &mut engine,
            "SELECT (information_schema._pg_expandarray(ARRAY['typed'])).*",
            true,
        );
        assert_eq!(row_description_type_oids(&output), [25, 23]);
        assert_eq!(data_rows(&output), ["typed|1"]);
    }
}

#[test]
fn bound_builtin_support_functions_convert_inputs_without_name_resolution() {
    use crate::sql::eval::CatalogAccess as _;
    let config = test_config("bound-builtin-support-functions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let engine = Engine::new(&config, &mut budget).unwrap();
    let arena = Arena::new(&mut budget, "bound builtin arguments", 64 << 10).unwrap();
    let catalog = query::storage_catalog(&engine.storage, &arena, 0);
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            catalog
                .call_routine_oid(
                    177,
                    &[Datum::Int2(1), Datum::Int4(2)],
                    &[crate::sql::types::oid::INT2, crate::sql::types::oid::INT4],
                    ast::Collation::None,
                    eval::RoutineCallAccess::Caller,
                    &arena
                )
                .unwrap(),
            Some(Datum::Int4(3))
        );
        assert_eq!(
            catalog
                .call_routine_oid(
                    141,
                    &[Datum::Int2(3), Datum::Int4(2)],
                    &[crate::sql::types::oid::INT2, crate::sql::types::oid::INT4],
                    ast::Collation::None,
                    eval::RoutineCallAccess::Caller,
                    &arena
                )
                .unwrap(),
            Some(Datum::Int4(6))
        );
        assert_eq!(
            catalog
                .call_routine_oid(
                    177,
                    &[Datum::Null, Datum::Int4(2)],
                    &[crate::sql::types::oid::INT4, crate::sql::types::oid::INT4],
                    ast::Collation::None,
                    eval::RoutineCallAccess::Caller,
                    &arena
                )
                .unwrap(),
            Some(Datum::Null)
        );
    });
}

#[test]
fn builtin_aggregate_support_identities_survive_catalog_publication() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("builtin-aggregate-catalog-identity");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE AGGREGATE public.sum(integer) (SFUNC=pg_catalog.int4pl,STYPE=integer,INITCOND='100'); \
             SET search_path=public,pg_catalog", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT sum(value),pg_catalog.sum(value) FROM (VALUES(1),(2)) AS input(value); \
             SELECT aggtransfn::oid FROM pg_aggregate WHERE aggfnoid='public.sum(integer)'::regprocedure; \
             SELECT 177::regproc,177::regprocedure, \
                    to_regproc('pg_catalog.int4pl')::oid, \
                    to_regprocedure('pg_catalog.int4pl(integer,integer)')::oid; \
             SELECT to_regprocedure('pg_catalog.array_append(anycompatiblearray,anycompatible)')::oid", true)),
            ["103|3", "177", "int4pl|int4pl(integer,integer)|177|177", "378"]);
        session.success(&mut engine, "DROP AGGREGATE public.sum(integer)", true);
        session.success(&mut engine,
            "CREATE FUNCTION public.int4pl(integer,integer) RETURNS integer LANGUAGE sql AS 'SELECT 999'", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT 177::regproc,177::regprocedure,pg_function_is_visible(177), \
                    to_regproc('int4pl')='public.int4pl(integer,integer)'::regprocedure, \
                    to_regprocedure('int4pl(integer,integer)')='public.int4pl(integer,integer)'::regprocedure, \
                    to_regproc('_pg_expandarray') IS NULL", true)),
            ["pg_catalog.int4pl|pg_catalog.int4pl(integer,integer)|f|t|t|t"]);
        session.success(
            &mut engine,
            "CREATE PROCEDURE public.visible_proc(integer) LANGUAGE sql AS 'SELECT 1'; \
             CREATE SCHEMA z_first; CREATE SCHEMA z_second; \
             CREATE FUNCTION z_first.echo(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
             CREATE FUNCTION z_second.echo(integer) RETURNS integer LANGUAGE sql AS 'SELECT $1'; \
             SET search_path=z_first,z_second,public,pg_catalog",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT pg_function_is_visible('public.visible_proc(integer)'::regprocedure), \
                    to_regproc('visible_proc')='public.visible_proc(integer)'::regprocedure; \
             SELECT 'z_second.echo(integer)'::regprocedure,to_regproc('z_second.echo'), \
                    pg_function_is_visible('z_first.echo(integer)'::regprocedure), \
                    pg_function_is_visible('z_second.echo(integer)'::regprocedure), \
                    to_regprocedure('echo(integer)')='z_first.echo(integer)'::regprocedure",
                true
            )),
            ["t|t", "z_second.echo(integer)|z_second.echo|t|f|t"]
        );
    }
}

// PostgreSQL 18.6 reference SQL SHA256:
// 9ceddc11f76f05b4af9d2234b0d8b08d8c02646f891affbfb07354d41294ddc6.
#[test]
fn conflict_condition_types_preserve_qualified_scope_errors() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("conflict-qualified-scope");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE SCHEMA reference_scope; \
             CREATE TABLE reference_scope.source(id integer PRIMARY KEY,value integer); \
             INSERT INTO reference_scope.source VALUES(1,7)",
            true,
        );
        for (condition, state, message) in [
            (
                "abs(ghost.missing)>0",
                "42P01",
                "missing FROM-clause entry for table \"ghost\"",
            ),
            (
                "abs(excluded.missing)>0",
                "42703",
                "column excluded.missing does not exist",
            ),
            (
                "excluded.missing>0",
                "42703",
                "column excluded.missing does not exist",
            ),
        ] {
            let sql = format!(
                "INSERT INTO reference_scope.source VALUES(1,9) \
                 ON CONFLICT(id) DO UPDATE SET value=excluded.value WHERE {condition}"
            );
            let output = session.execute(&mut engine, &sql, true);
            assert_eq!(error_sqlstates(&output), [state], "{mode:?}: {sql}");
            assert!(
                String::from_utf8_lossy(&output).contains(&format!("M{message}\0")),
                "{mode:?}: {sql}: {}",
                String::from_utf8_lossy(&output)
            );
        }
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT value FROM reference_scope.source",
                true,
            )),
            ["7"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "INSERT INTO reference_scope.source VALUES(1,9) ON CONFLICT(id) \
                 DO UPDATE SET value=excluded.value WHERE abs(excluded.value)>0 \
                 RETURNING value",
                true,
            )),
            ["9"]
        );
    }
}

// PostgreSQL 18.6 reference SQL SHA256:
// 2baf7a415c2246607b549a3e1a2cef33191f7e0ca15a8bd311dbfe9dec3b9a89.
#[test]
fn auto_updatable_views_map_public_columns_under_target_aliases() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("view-target-alias-columns");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE alias_view_base(id integer,value text,secret text); \
             INSERT INTO alias_view_base VALUES(1,'a','private'),(2,'b','private'),(3,'c','private'); \
             CREATE VIEW alias_view(visible_id,visible_value) AS \
               SELECT id,value FROM alias_view_base WHERE id<=2",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "UPDATE alias_view AS exposed SET visible_value=upper(exposed.visible_value) \
                 WHERE exposed.visible_id=1 RETURNING exposed.*; \
                 DELETE FROM alias_view AS exposed WHERE exposed.visible_id=2 RETURNING exposed.*",
                true,
            )),
            ["1|A", "2|b"]
        );
        let hidden = session.execute(
            &mut engine,
            "UPDATE alias_view AS exposed SET secret='leak' WHERE exposed.visible_id=1",
            true,
        );
        assert_eq!(error_sqlstates(&hidden), ["42703"]);
        assert!(
            String::from_utf8_lossy(&hidden)
                .contains("Mcolumn \"secret\" of relation \"alias_view\" does not exist\0"),
            "{}",
            String::from_utf8_lossy(&hidden)
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "UPDATE alias_view AS exposed SET visible_value='hidden' WHERE alias_view.visible_id=1",
                true,
            )),
            ["42P01"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT id,value,secret FROM alias_view_base ORDER BY id",
                true,
            )),
            ["1|A|private", "3|c|private"]
        );
    }
}

#[test]
fn function_casts_receive_target_type_modifiers_and_conversion_context() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cast-type-modifiers-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE FUNCTION target_varchar(integer,integer,boolean) RETURNS varchar LANGUAGE sql AS \
               $$SELECT (CASE WHEN $2=9 THEN CASE WHEN $3 THEN 'e9' ELSE 'a9' END ELSE 'none' END)::varchar$$; \
             CREATE CAST(integer AS varchar) WITH FUNCTION target_varchar(integer,integer,boolean) AS ASSIGNMENT; \
             CREATE DOMAIN target_short AS varchar(5); \
             CREATE TABLE target_modifiers(value varchar(5),items varchar(5)[],domain_value target_short)", true);
        for (statement, expected) in [
            ("SELECT 7::varchar(5)", "e9"),
            ("SELECT 7::varchar", "none"),
            ("SELECT ARRAY[7,8]::varchar(5)[]", "{e9,e9}"),
            ("SELECT (ARRAY[7,8]::integer[])::varchar(5)[]", "{e9,e9}"),
            (
                "SELECT ARRAY[ARRAY[7],ARRAY[8]]::varchar(5)[]",
                "{{e9},{e9}}",
            ),
            ("SELECT 7::target_short", "e9"),
            ("SELECT ARRAY[7,8]::target_short[]", "{e9,e9}"),
        ] {
            assert_eq!(
                data_rows(&session.success(&mut engine, statement, true)),
                [expected],
                "{statement}, {mode:?}"
            );
        }
        session.success(
            &mut engine,
            "INSERT INTO target_modifiers VALUES(7,ARRAY[7,8],7)",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT value,items,domain_value FROM target_modifiers",
                true
            )),
            ["a9|{a9,a9}|a9"],
            "{mode:?}"
        );
    }
}

#[test]
fn sql_and_plpgsql_formals_preserve_observed_postgresql_input_collations() {
    // PostgreSQL 18.6 oracle at 233fb021, fixture SHA256:
    // b903aff00968acddaa4815009a1836ee74c4630016800d904c5588402ae11b79.
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("formal-input-collations-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/208_routine_parameter_collations.sql"
        );
        let (definitions, _) = fixture
            .split_once("ALTER SEQUENCE parameter_checks RESTART WITH 1;")
            .unwrap();
        session.success(&mut engine, definitions, true);
        for (statement, expected) in [
            (
                "ALTER SEQUENCE parameter_checks RESTART WITH 1; SELECT parameter_sql('a' COLLATE \"C\")",
                "1",
            ),
            ("SELECT parameter_sql('a' COLLATE \"POSIX\")", "2"),
            ("SELECT parameter_sql('a' COLLATE \"C\")", "2"),
            (
                "DISCARD PLANS; ALTER SEQUENCE parameter_checks RESTART WITH 1; SELECT parameter_pl('a' COLLATE \"C\")",
                "1",
            ),
            ("SELECT parameter_pl('a' COLLATE \"POSIX\")", "2"),
            ("SELECT parameter_pl('a' COLLATE \"C\")", "2"),
            (
                "SELECT * FROM parameter_pl_table('a' COLLATE \"C\"); SELECT * FROM parameter_pl_table('a' COLLATE \"POSIX\")",
                "2",
            ),
            (
                "SELECT parameter_local('a' COLLATE \"C\"); SELECT parameter_local('a' COLLATE \"POSIX\")",
                "2",
            ),
            (
                "CALL parameter_sql_procedure('a' COLLATE \"C\"); CALL parameter_sql_procedure('a' COLLATE \"POSIX\")",
                "2",
            ),
            (
                "CALL parameter_pl_procedure('a' COLLATE \"C\"); CALL parameter_pl_procedure('a' COLLATE \"POSIX\")",
                "2",
            ),
            (
                "PREPARE parameter_statement(text) AS SELECT parameter_sql($1 COLLATE \"C\"); EXECUTE parameter_statement('a'); EXECUTE parameter_statement('b')",
                "2",
            ),
        ] {
            session.success(&mut engine, statement, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT last_value FROM parameter_checks",
                    true
                )),
                [expected],
                "{statement}, {mode:?}"
            );
        }
    }
}

#[test]
fn domain_checks_keep_sequence_context_through_casts_arrays_and_assignments() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("domain-check-sequences-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE SEQUENCE direct_domain_checks; \
             CREATE DOMAIN direct_checked AS integer CHECK(nextval('direct_domain_checks')>0); \
             CREATE TABLE direct_values(value direct_checked,items direct_checked[])",
            true,
        );
        for (statement, expected) in [
            ("SELECT 7::direct_checked", "1"),
            ("SELECT ARRAY[8,9]::direct_checked[]", "3"),
            ("SELECT '{8,9}'::direct_checked[]", "5"),
            ("INSERT INTO direct_values VALUES(1,ARRAY[2,3])", "8"),
            ("SELECT NULL::direct_checked", "9"),
            ("SELECT ARRAY[NULL]::direct_checked[]", "10"),
        ] {
            session.success(&mut engine, statement, true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT last_value FROM direct_domain_checks",
                    true
                )),
                [expected],
                "{statement}, {mode:?}"
            );
        }
        session.success(&mut engine,
            "CREATE FUNCTION direct_body() RETURNS direct_checked[] LANGUAGE sql SET search_path=public \
               AS $$SELECT '{11,12}'::direct_checked[]$$", true);
        let created = session.success(
            &mut engine,
            "SELECT last_value FROM direct_domain_checks",
            true,
        );
        assert_eq!(data_rows(&created), ["12"]);
        for expected in ["14", "14"] {
            session.success(&mut engine, "SELECT direct_body()", true);
            assert_eq!(
                data_rows(&session.success(
                    &mut engine,
                    "SELECT last_value FROM direct_domain_checks",
                    true
                )),
                [expected]
            );
        }
        let input_arena = Arena::new(&mut budget, "domain parameter inputs", 256 << 10).unwrap();
        let oid = crate::sql::catalog::user_type_oid(&engine.storage, 0, "direct_checked").unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                engine
                    .decode_text_parameter(
                        oid,
                        b"15",
                        &input_arena,
                        0,
                        Some(session.guc.seq_session())
                    )
                    .unwrap(),
                Datum::Int4(15)
            );
            assert_eq!(
                engine
                    .decode_binary_parameter(
                        oid,
                        &16_i32.to_be_bytes(),
                        &input_arena,
                        0,
                        Some(session.guc.seq_session())
                    )
                    .unwrap(),
                Datum::Int4(16)
            );
            assert_eq!(
                engine
                    .coerce_parameter_null(oid, &input_arena, 0, Some(session.guc.seq_session()))
                    .unwrap(),
                Datum::Null
            );
        });
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT last_value FROM direct_domain_checks",
                true
            )),
            ["17"]
        );
    }
}

#[test]
fn foreign_row_inputs_preserve_the_request_sequence_context_without_allocating() {
    let config = test_config("foreign-domain-sequence-inputs");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE foreign_domain_checks; \
         CREATE DOMAIN foreign_checked AS integer CHECK(nextval('foreign_domain_checks')>0); \
         CREATE TABLE foreign_layout(value foreign_checked,items foreign_checked[])",
        true,
    );
    let slot = engine
        .storage
        .find_visible("public", "foreign_layout", 0)
        .unwrap();
    let table = (*engine.storage.table_def(slot, 0)).clone();
    let arena = Arena::new(&mut budget, "foreign value conversion", 256 << 10).unwrap();
    let sequences =
        crate::sql::sequence::SeqEval::new(&engine.storage, session.guc.seq_session(), 0);
    crate::mem::guard::forbid_alloc(|| {
        assert_eq!(
            crate::sql::foreign::decode_foreign_field(
                &engine.storage,
                &table,
                0,
                Some(b"7"),
                0,
                &arena,
                Some(&sequences)
            )
            .unwrap(),
            Datum::Int4(7)
        );
        let array = crate::sql::foreign::decode_foreign_field(
            &engine.storage,
            &table,
            1,
            Some(b"{8,9}"),
            0,
            &arena,
            Some(&sequences),
        )
        .unwrap();
        assert!(matches!(array, Datum::Array { .. }));
        assert_eq!(
            crate::sql::foreign::decode_foreign_field(
                &engine.storage,
                &table,
                0,
                None,
                0,
                &arena,
                Some(&sequences)
            )
            .unwrap(),
            Datum::Null
        );
    });
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value FROM foreign_domain_checks",
            true
        )),
        ["4"]
    );
}

#[test]
fn planner_metadata_preserves_record_shapes_target_names_and_index_comments() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("planner-record-metadata-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture =
            include_str!("../../../tests/external/differential/232_planner_record_metadata.sql");
        let mut rows = Vec::new();
        let mut errors = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            let state = if statement.contains("excluded.missing") {
                Some("42703")
            } else if statement.contains("length(DISTINCT") {
                Some("42809")
            } else if statement.contains(" AS hidden") {
                Some("42P01")
            } else {
                None
            };
            let output = session.execute(&mut engine, statement, true);
            assert_eq!(
                error_sqlstates(&output),
                state.into_iter().collect::<Vec<_>>(),
                "{mode:?}: {statement}"
            );
            errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
            rows.extend(data_rows(&output));
        }
        assert_eq!(errors, ["42703", "42809", "42P01", "42P01"]);
        assert_eq!(
            rows,
            [
                "7|xy|integer|character varying",
                "x|1",
                "NULL|2",
                "z|3",
                "9",
                "1|11|10|11",
                "2|20|25",
                "kept",
                "planner_metadata_index|replacement index",
                "planner_metadata_retired|original index",
                "replacement index",
                "1978|403|int4_ops|integer_ops|23|t|0",
                "1979|403|int2_ops|integer_ops|21|t|0",
                "1981|403|oid_ops|oid_ops|26|t|0",
                "4217|403|text_pattern_ops|text_pattern_ops|25|f|0",
                "10028|403|name_ops|text_ops|19|t|2275",
                "10031|405|oid_ops|oid_ops|26|t|0",
                "10044|403|varchar_ops|text_ops|25|f|0",
                "10053|403|xid8_ops|xid8_ops|5069|t|0",
                "85",
                "0",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn nested_query_inputs_preserve_caller_scratch_record_shapes_and_index_metadata() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("nested-query-inputs-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let fixture = include_str!(
            "../../../tests/external/differential/234_nested_query_and_index_metadata.sql"
        );
        let mut rows = Vec::new();
        for statement in fixture
            .split(";\n")
            .map(str::trim)
            .filter(|sql| !sql.is_empty())
        {
            let expected = if statement.contains("'SELECT target FROM nested_input_rules")
                || statement.contains("'SELECT 1,2'")
            {
                Some("22023")
            } else if statement.contains("\"a.b\".value") {
                Some("42P01")
            } else if statement.starts_with("SELECT substring(") {
                Some("22011")
            } else if statement.starts_with("SELECT 1/0,") {
                Some("22012")
            } else {
                None
            };
            let output = session.execute(&mut engine, statement, true);
            assert_eq!(
                error_sqlstates(&output),
                expected.into_iter().collect::<Vec<_>>(),
                "{mode:?}: {statement}"
            );
            rows.extend(data_rows(&output));
        }
        assert_eq!(
            rows,
            [
                "'c' & 'd'",
                "64|'c' & 'd'",
                "'a'",
                "'c' & 'd'",
                "kept",
                "1|one|character varying",
                "2|two|character varying",
                "1|one",
                "2|two",
                "t|t|key comment|index comment",
                "t|t|key comment|index comment",
                "t|t|key comment|index comment",
                "1|2",
                "1|2",
            ],
            "{mode:?}"
        );
    }
}

#[test]
fn empty_sql_bodies_are_void_routines_that_do_nothing() {
    use super::routine_identity_diagnostics::{NONE, check};
    check(&[
        (
            "CREATE FUNCTION empty_string() RETURNS void LANGUAGE sql AS ''; \
             CREATE FUNCTION empty_statements() RETURNS void LANGUAGE sql AS ';;'; \
             CREATE PROCEDURE empty_procedure(int) LANGUAGE sql AS ''; \
             CREATE FUNCTION empty_atomic() RETURNS void LANGUAGE sql BEGIN ATOMIC END; \
             CREATE PROCEDURE empty_atomic_procedure() LANGUAGE sql BEGIN ATOMIC END",
            NONE,
        ),
        (
            "SELECT empty_string(), empty_statements(), empty_atomic(); \
             SELECT * FROM empty_string(); \
             SELECT x, empty_string() FROM generate_series(1, 2) AS x; \
             CALL empty_procedure(1); CALL empty_atomic_procedure()",
            NONE,
        ),
        (
            "CREATE FUNCTION empty_integer() RETURNS integer LANGUAGE sql AS ''",
            &[(
                'E',
                "42P13",
                "return type mismatch in function declared to return integer",
            )],
        ),
    ]);
    let config = test_config("empty-sql-body-definition");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(
        &mut engine,
        "CREATE FUNCTION empty_atomic() RETURNS void LANGUAGE sql BEGIN ATOMIC END; \
         SELECT pg_get_functiondef('empty_atomic'::regproc), prosqlbody IS NOT NULL \
         FROM pg_proc WHERE proname = 'empty_atomic'",
        true,
    );
    assert_eq!(
        data_rows(&output),
        [
            "CREATE OR REPLACE FUNCTION public.empty_atomic()\n RETURNS void\n LANGUAGE sql\nBEGIN ATOMIC\nEND\n|t"
        ]
    );
}

/// PostgreSQL 18.6 `check_sql_stmt_retval` results, captured from a reference
/// server: the declared type is named by `format_type_be`, and output
/// parameters of a procedure form a `record`.
#[test]
fn sql_bodies_must_end_with_the_declared_result_statement() {
    use super::routine_identity_diagnostics::error_fields;
    const STATEMENT: &str =
        "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.";
    let config = test_config("sql-body-result-statement");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE result_rows(a integer); CREATE SCHEMA hidden; \
         CREATE TYPE hidden.pair AS (a integer)",
        true,
    );
    let mismatch = |session: &mut ConfiguredTransactionSession,
                    engine: &mut Engine,
                    sql: &str,
                    declared: &str,
                    detail: &str| {
        let output = session.execute(engine, sql, true);
        assert_eq!(
            error_fields(&output),
            [(
                "42P13".to_owned(),
                format!("return type mismatch in function declared to return {declared}"),
                Some(detail.to_owned()),
                None,
            )],
            "{sql}"
        );
    };
    for (sql, declared) in [
        (
            "CREATE FUNCTION r() RETURNS integer LANGUAGE sql AS ''",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS integer LANGUAGE sql AS ';;'",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS integer LANGUAGE sql BEGIN ATOMIC END",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS integer LANGUAGE sql \
             AS 'INSERT INTO result_rows VALUES (1)'",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS integer LANGUAGE sql \
             AS 'SELECT 1; INSERT INTO result_rows VALUES (1)'",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS integer LANGUAGE sql AS 'SELECT 1; COMMIT'",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS SETOF integer LANGUAGE sql AS ''",
            "integer",
        ),
        (
            "CREATE FUNCTION r(OUT a integer, OUT b integer) LANGUAGE sql AS ''",
            "record",
        ),
        (
            "CREATE FUNCTION r(OUT a integer) LANGUAGE sql AS ''",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS TABLE(a integer) LANGUAGE sql AS ''",
            "integer",
        ),
        (
            "CREATE FUNCTION r() RETURNS TABLE(a integer, b text) LANGUAGE sql AS ''",
            "record",
        ),
        (
            "CREATE FUNCTION r() RETURNS SETOF record LANGUAGE sql AS ''",
            "record",
        ),
        (
            "CREATE FUNCTION r() RETURNS integer[] LANGUAGE sql AS ''",
            "integer[]",
        ),
        (
            "CREATE FUNCTION r() RETURNS varchar(3) LANGUAGE sql AS ''",
            "character varying",
        ),
        (
            "CREATE FUNCTION r() RETURNS timestamptz LANGUAGE sql AS ''",
            "timestamp with time zone",
        ),
        (
            "CREATE FUNCTION r() RETURNS hidden.pair LANGUAGE sql AS ''",
            "hidden.pair",
        ),
        (
            "CREATE PROCEDURE r(OUT a integer) LANGUAGE sql AS ''",
            "record",
        ),
        (
            "CREATE PROCEDURE r(INOUT a integer) LANGUAGE sql BEGIN ATOMIC END",
            "record",
        ),
        (
            "CREATE PROCEDURE r(INOUT a integer) LANGUAGE sql \
             AS 'SELECT 1; INSERT INTO result_rows VALUES (1)'",
            "record",
        ),
    ] {
        mismatch(&mut session, &mut engine, sql, declared, STATEMENT);
    }
    // With body checks off, PostgreSQL stores the routine and reports the
    // mismatch when a call reaches it. Polymorphic arguments skip the check.
    type Calls<'a> = &'a [(&'a str, &'a str, &'a str)];
    let deferred: [(&str, Calls); 2] = [
        (
            "CREATE FUNCTION deferred_empty() RETURNS integer LANGUAGE sql AS ''; \
             CREATE FUNCTION deferred_atomic() RETURNS integer LANGUAGE sql BEGIN ATOMIC END; \
             CREATE FUNCTION deferred_write() RETURNS integer LANGUAGE sql \
                 AS 'INSERT INTO result_rows VALUES (1)'; \
             CREATE FUNCTION deferred_rows() RETURNS SETOF integer LANGUAGE sql AS ''; \
             CREATE FUNCTION deferred_wide() RETURNS integer LANGUAGE sql AS 'SELECT 1, 2'",
            &[
                ("SELECT deferred_empty()", "integer", STATEMENT),
                ("SELECT deferred_atomic()", "integer", STATEMENT),
                ("SELECT deferred_write()", "integer", STATEMENT),
                ("SELECT * FROM deferred_rows()", "integer", STATEMENT),
                (
                    "SELECT deferred_wide()",
                    "integer",
                    "Final statement must return exactly one column.",
                ),
            ],
        ),
        (
            "CREATE PROCEDURE deferred_output(OUT a integer) LANGUAGE sql AS ''; \
             CREATE PROCEDURE deferred_inout(INOUT a integer) LANGUAGE sql BEGIN ATOMIC END; \
             CREATE PROCEDURE deferred_wide_output(INOUT a integer) LANGUAGE sql \
                 AS 'SELECT 1, 2'; \
             CREATE PROCEDURE deferred_narrow_output(INOUT a integer, OUT b integer) \
                 LANGUAGE sql AS 'SELECT 1'",
            &[
                ("CALL deferred_output(NULL)", "record", STATEMENT),
                ("CALL deferred_inout(1)", "record", STATEMENT),
                (
                    "CALL deferred_wide_output(1)",
                    "record",
                    "Final statement returns too many columns.",
                ),
                (
                    "CALL deferred_narrow_output(1, NULL)",
                    "record",
                    "Final statement returns too few columns.",
                ),
            ],
        ),
    ];
    for (definitions, calls) in deferred {
        let output = session.execute(
            &mut engine,
            &format!("SET check_function_bodies = off; {definitions}; RESET check_function_bodies"),
            true,
        );
        assert_eq!(error_fields(&output), [], "{definitions}");
        for (sql, declared, detail) in calls {
            mismatch(&mut session, &mut engine, sql, declared, detail);
        }
        session.success(
            &mut engine,
            "DROP ROUTINE IF EXISTS deferred_empty, deferred_atomic, deferred_write, deferred_rows, \
                 deferred_wide, deferred_output, deferred_inout, deferred_wide_output, \
                 deferred_narrow_output",
            true,
        );
    }
    session.success(
        &mut engine,
        "CREATE FUNCTION polymorphic_empty(x anyelement) RETURNS anyelement \
             LANGUAGE sql AS ''",
        true,
    );
    let output = session.success(
        &mut engine,
        "SELECT count(*) FROM result_rows; \
         SELECT proname FROM pg_proc WHERE proname = 'polymorphic_empty'",
        true,
    );
    assert_eq!(data_rows(&output), ["0", "polymorphic_empty"]);
}

/// Procedures with output parameters take the final statement's first row,
/// including a data-modifying statement's RETURNING row, as PostgreSQL does.
#[test]
fn sql_procedure_outputs_come_from_the_final_statement_first_row() {
    use super::routine_identity_diagnostics::error_fields;
    let config = test_config("sql-procedure-output-rows");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(
        &mut engine,
        "CREATE TABLE output_rows(a integer); \
         CREATE PROCEDURE first_row(INOUT a integer) LANGUAGE sql \
             AS 'SELECT * FROM generate_series(a, a + 2)'; \
         CREATE PROCEDURE returned_row(INOUT a integer, OUT b text) LANGUAGE sql \
             AS 'INSERT INTO output_rows VALUES (a), (a + 1) RETURNING a, ''x'''; \
         CALL first_row(4); CALL returned_row(7, NULL); \
         SELECT a FROM output_rows ORDER BY a",
        true,
    );
    assert_eq!(data_rows(&output), ["4", "7|x", "7", "8"]);
    let output = session.execute(
        &mut engine,
        "CREATE PROCEDURE no_row(INOUT a integer) LANGUAGE sql AS 'SELECT 1 WHERE false'; \
         CALL no_row(1)",
        true,
    );
    assert_eq!(
        error_fields(&output),
        [(
            "XX000".to_owned(),
            "procedure returned null record".to_owned(),
            None,
            None
        )]
    );
}

/// PostgreSQL stores transaction control in an SQL body and rejects it, under
/// `CreateCommandName`'s spelling, when execution reaches the statement.
#[test]
fn sql_bodies_reject_transaction_control_when_reached() {
    use super::routine_identity_diagnostics::diagnostics;
    let config = test_config("sql-body-transaction-control");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE control_rows(a integer); \
         CREATE FUNCTION control_function() RETURNS void LANGUAGE sql \
             AS 'INSERT INTO control_rows VALUES (1); COMMIT'; \
         CREATE FUNCTION control_vacuum() RETURNS void LANGUAGE sql AS 'VACUUM'",
        true,
    );
    let mut expect = |sql: &str, state: &str, message: &str| {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(
            diagnostics(&output),
            [('E', state.to_owned(), message.to_owned())],
            "{sql}"
        );
    };
    for (index, (statement, name)) in [
        ("BEGIN", "BEGIN"),
        ("START TRANSACTION", "START TRANSACTION"),
        ("COMMIT", "COMMIT"),
        ("END", "COMMIT"),
        ("ROLLBACK", "ROLLBACK"),
        ("ABORT", "ROLLBACK"),
        ("SAVEPOINT a", "SAVEPOINT"),
        ("RELEASE SAVEPOINT a", "RELEASE"),
        ("ROLLBACK TO SAVEPOINT a", "ROLLBACK"),
    ]
    .into_iter()
    .enumerate()
    {
        expect(
            &format!(
                "CREATE PROCEDURE control_{index}() LANGUAGE sql \
                 AS 'INSERT INTO control_rows VALUES (1); {statement}'; \
                 CALL control_{index}()"
            ),
            "0A000",
            &format!("{name} is not allowed in an SQL function"),
        );
    }
    expect(
        "SELECT control_function()",
        "0A000",
        "COMMIT is not allowed in an SQL function",
    );
    expect(
        "SELECT control_vacuum()",
        "25001",
        "VACUUM cannot be executed from a function",
    );
    // CHECKPOINT is not transaction control; PostgreSQL runs it in a body.
    session.success(
        &mut engine,
        "CREATE FUNCTION control_checkpoint() RETURNS void LANGUAGE sql AS 'CHECKPOINT'; \
         SELECT control_checkpoint()",
        true,
    );
    let output = session.success(
        &mut engine,
        "START TRANSACTION; SELECT count(*) FROM control_rows; END",
        true,
    );
    assert_eq!(data_rows(&output), ["0"]);
    assert!(
        output
            .windows(b"START TRANSACTION\0".len())
            .any(|window| window == b"START TRANSACTION\0")
    );
}

/// `pg_get_functiondef` output captured from PostgreSQL 18.6.
#[test]
fn function_definitions_use_postgresql_layout() {
    let config = test_config("function-definition-layout");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(
        &mut engine,
        r#"CREATE SCHEMA layout; CREATE TYPE layout."My Type" AS (a integer);
        CREATE FUNCTION layout.a(a int, b int DEFAULT 1 + 2, OUT c int)
            LANGUAGE sql IMMUTABLE STRICT AS 'SELECT a';
        CREATE FUNCTION layout.b() RETURNS SETOF int LANGUAGE sql STABLE LEAKPROOF
            SECURITY DEFINER PARALLEL SAFE COST 5 ROWS 7
            SET search_path = public, "My Schema", pg_temp SET datestyle = 'iso, mdy'
            AS 'SELECT 1';
        CREATE FUNCTION layout.c() RETURNS int LANGUAGE sql COST 1000000
            AS $q$SELECT 1 /* $function$ $functionx$ */$q$;
        CREATE FUNCTION layout.d() RETURNS setof int LANGUAGE sql COST 0.00001 ROWS 1000
            AS 'SELECT 1';
        CREATE PROCEDURE layout."Pr oc"("A b" int, INOUT "c" layout."My Type"[])
            LANGUAGE sql SET search_path = '' AS 'SELECT c';
        SELECT pg_get_functiondef(p.oid) FROM pg_proc p
          JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE nspname = 'layout' ORDER BY proname;
        SELECT pg_get_function_result('layout."Pr oc"'::regproc) IS NULL,
               pg_get_function_arguments('layout."Pr oc"'::regproc)"#,
        true,
    );
    assert_eq!(
        data_rows(&output),
        [
            "CREATE OR REPLACE PROCEDURE layout.\"Pr oc\"(IN \"A b\" integer, INOUT c layout.\"My Type\"[])\n \
             LANGUAGE sql\n \
             SET search_path TO ''\n\
             AS $procedure$SELECT c$procedure$\n",
            "CREATE OR REPLACE FUNCTION layout.a(a integer, b integer DEFAULT (1 + 2), OUT c integer)\n \
             RETURNS integer\n \
             LANGUAGE sql\n \
             IMMUTABLE STRICT\n\
             AS $function$SELECT a$function$\n",
            "CREATE OR REPLACE FUNCTION layout.b()\n \
             RETURNS SETOF integer\n \
             LANGUAGE sql\n \
             STABLE PARALLEL SAFE SECURITY DEFINER LEAKPROOF COST 5 ROWS 7\n \
             SET search_path TO 'public', 'My Schema', 'pg_temp'\n \
             SET \"DateStyle\" TO 'iso, mdy'\n\
             AS $function$SELECT 1$function$\n",
            "CREATE OR REPLACE FUNCTION layout.c()\n \
             RETURNS integer\n \
             LANGUAGE sql\n \
             COST 1e+06\n\
             AS $functionxx$SELECT 1 /* $function$ $functionx$ */$functionxx$\n",
            "CREATE OR REPLACE FUNCTION layout.d()\n \
             RETURNS SETOF integer\n \
             LANGUAGE sql\n \
             COST 1e-05\n\
             AS $function$SELECT 1$function$\n",
            "t|IN \"A b\" integer, INOUT c layout.\"My Type\"[]",
        ]
    );
}
