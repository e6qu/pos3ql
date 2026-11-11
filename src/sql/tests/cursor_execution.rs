use super::*;

#[test]
fn literal_values_cursors_defer_projections_and_preserve_source_positions() {
    let config = test_config("literal-values-cursor-functions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE literal_effects START 10; \
         CREATE FUNCTION literal_value(value integer) RETURNS bigint LANGUAGE sql VOLATILE \
         SET search_path=public AS $$SELECT nextval('literal_effects')+$1$$; BEGIN; \
         DECLARE literal_cursor NO SCROLL CURSOR FOR \
         SELECT literal_value(id) FROM (VALUES(1),(2),(3)) source(id)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value,is_called FROM literal_effects; FETCH 0 FROM literal_cursor; \
         SELECT last_value,is_called FROM literal_effects",
            true
        )),
        ["10|f", "10|f"]
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "FETCH 1 FROM literal_cursor; FETCH 1 FROM literal_cursor; FETCH ALL FROM literal_cursor; \
         FETCH 1 FROM literal_cursor; SELECT last_value FROM literal_effects; \
         CLOSE literal_cursor; COMMIT", true)), ["11", "13", "15", "12"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DECLARE literal_held NO SCROLL CURSOR WITH HOLD FOR \
         SELECT literal_value(id) FROM (VALUES(7),(8)) source(id); \
         SELECT last_value FROM literal_effects; COMMIT; FETCH ALL FROM literal_held; \
         SELECT last_value FROM literal_effects; CLOSE literal_held",
            true
        )),
        ["12", "20", "22", "14"]
    );
    for (source, expected) in [
        ("(VALUES(1))", "f"),
        ("(VALUES(1),(2))", "t"),
        ("(SELECT 1 UNION ALL SELECT 2)", "f"),
    ] {
        session.success(
            &mut engine,
            &format!("BEGIN; DECLARE literal_auto CURSOR FOR SELECT * FROM {source} source(id)"),
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_scrollable FROM pg_cursors WHERE name='literal_auto'",
                true
            )),
            [expected],
            "{source}"
        );
        session.success(&mut engine, "CLOSE literal_auto; COMMIT", true);
    }
}

#[test]
fn literal_values_cursor_retries_keep_the_current_row_and_argument_effects() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("literal-values-cursor-wait-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        reader.success(
            &mut engine,
            "CREATE SEQUENCE literal_wait_effects; CREATE TABLE literal_gate(value integer); \
             INSERT INTO literal_gate VALUES(0); CREATE FUNCTION literal_wait(requested integer) \
             RETURNS integer LANGUAGE plpgsql AS $$BEGIN PERFORM nextval('literal_wait_effects'); \
             UPDATE literal_gate SET value=value+1; RETURN requested; END$$",
            true,
        );
        writer.success(
            &mut engine,
            "BEGIN; UPDATE literal_gate SET value=value+1",
            true,
        );
        reader.success(&mut engine, "BEGIN; DECLARE literal_wait_cursor NO SCROLL CURSOR FOR SELECT literal_wait(id) FROM (VALUES(4),(5)) source(id)", true);
        let status = reader.execute_status(&mut engine, "FETCH 1 FROM literal_wait_cursor", true);
        assert!(
            matches!(
                status,
                ExecutionStatus::Blocked {
                    wait: StatementWait::Lock,
                    ..
                }
            ),
            "{mode:?}: {status:?}: {}",
            String::from_utf8_lossy(reader.send.readable())
        );
        assert_eq!(
            data_rows(&writer.success(
                &mut engine,
                "SELECT last_value FROM literal_wait_effects",
                true
            )),
            ["1"]
        );
        writer.success(&mut engine, "COMMIT", true);
        assert_eq!(
            data_rows(&reader.success(&mut engine, "FETCH 1 FROM literal_wait_cursor", true)),
            ["4"]
        );
        assert_eq!(data_rows(&reader.success(&mut engine, "FETCH ALL FROM literal_wait_cursor; SELECT last_value FROM literal_wait_effects; CLOSE literal_wait_cursor; COMMIT", true)), ["5", "2"]);
        drop((reader, writer, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn singleton_cursor_wildcards_require_relations_and_preserve_zero_column_rows() {
    let config = test_config("singleton-cursor-wildcards");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for (query, expected) in [
        ("SELECT *", sqlstate::SYNTAX_ERROR),
        ("SELECT * LIMIT 0", sqlstate::SYNTAX_ERROR),
        ("SELECT *,1/0 LIMIT 0", sqlstate::SYNTAX_ERROR),
        ("SELECT true+1,*", sqlstate::UNDEFINED_FUNCTION),
        ("SELECT *,true+1", sqlstate::SYNTAX_ERROR),
        ("SELECT missing.*", sqlstate::UNDEFINED_TABLE),
        ("SELECT missing.* LIMIT 0", sqlstate::UNDEFINED_TABLE),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, query, true)),
            [expected],
            "{query}"
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("BEGIN; DECLARE invalid_singleton CURSOR FOR {query}"),
                true
            )),
            [expected],
            "DECLARE {query}"
        );
        session.success(&mut engine, "ROLLBACK", true);
    }
    session.success(&mut engine,
        "CREATE TABLE zero_column_cursor_source(); INSERT INTO zero_column_cursor_source DEFAULT VALUES", true);
    for projection in ["*", "source.*"] {
        session.success(&mut engine,
            &format!("BEGIN; DECLARE valid_empty_columns CURSOR FOR SELECT {projection} FROM zero_column_cursor_source source"), true);
        let output = session.success(&mut engine, "FETCH NEXT FROM valid_empty_columns", true);
        assert_eq!(command_tags(&output), ["FETCH 1"]);
        assert_eq!(data_rows(&output), [""]);
        session.success(&mut engine, "CLOSE valid_empty_columns; COMMIT", true);
    }
}

#[test]
fn singleton_cursors_defer_function_defaults_and_effects_until_fetch() {
    let config = test_config("singleton-cursor-functions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE SEQUENCE singleton_effects START 10; \
         CREATE FUNCTION singleton_value(value bigint DEFAULT nextval('singleton_effects')) \
         RETURNS bigint LANGUAGE sql VOLATILE AS 'SELECT $1'; BEGIN; \
         DECLARE singleton_cursor NO SCROLL CURSOR FOR SELECT singleton_value(),nextval('singleton_effects')", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value,is_called FROM singleton_effects; FETCH 0 FROM singleton_cursor; \
         SELECT last_value,is_called FROM singleton_effects",
            true
        )),
        ["10|f", "10|f"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "FETCH 1 FROM singleton_cursor; SELECT last_value FROM singleton_effects; \
         FETCH 1 FROM singleton_cursor; SELECT last_value FROM singleton_effects; \
         CLOSE singleton_cursor; COMMIT",
            true
        )),
        ["10|11", "11", "11"]
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "BEGIN; DECLARE singleton_held NO SCROLL CURSOR WITH HOLD FOR SELECT singleton_value(); \
         SELECT last_value FROM singleton_effects; COMMIT; FETCH ALL FROM singleton_held; \
         SELECT last_value FROM singleton_effects; CLOSE singleton_held", true)), ["11", "12", "12"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE FUNCTION public.sum(value integer) RETURNS bigint LANGUAGE sql VOLATILE \
         AS $$SELECT nextval('singleton_effects')+$1$$; \
         BEGIN; DECLARE singleton_shadow NO SCROLL CURSOR FOR SELECT public.sum(7); \
         SELECT last_value FROM singleton_effects; FETCH 1 FROM singleton_shadow; \
         SELECT last_value FROM singleton_effects; CLOSE singleton_shadow; COMMIT",
            true
        )),
        ["12", "20", "13"]
    );
}

// PostgreSQL 18.6 opaque SQL-function phase oracle SQL SHA256:
// 71df626e54f2dd353981387010aff2ce26bf2028bb44920a630526c8fc9a5993.
// Backend cache reuse SQL SHA256:
// 622bafc3e7ffb7f83fc3282fab8ecb0c941a1bb8f72fc87a1b382644c92b242b.
#[test]
fn cursor_function_plans_keep_their_owner_and_row_handoffs() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("cursor-function-input-owner-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE SEQUENCE cursor_checks; CREATE SEQUENCE cursor_rows START 10; \
             CREATE TABLE cursor_values(id integer); INSERT INTO cursor_values VALUES(1),(2); \
             CREATE FUNCTION cursor_allowed(value integer) RETURNS boolean LANGUAGE plpgsql AS $$BEGIN \
              PERFORM nextval('cursor_checks'); RETURN true; END$$; \
             CREATE DOMAIN cursor_checked AS integer CHECK(cursor_allowed(VALUE)); \
             CREATE FUNCTION cursor_constant() RETURNS cursor_checked[] LANGUAGE sql SET search_path TO public \
              AS 'SELECT ''{1}''::cursor_checked[]'; \
             CREATE FUNCTION cursor_row(value integer) RETURNS integer LANGUAGE plpgsql AS $$BEGIN \
              RETURN value+nextval('cursor_rows')::integer; END$$", true);
        assert_eq!(data_rows(&session.success(&mut engine,
            "BEGIN; DECLARE values_cursor NO SCROLL CURSOR FOR \
             SELECT cursor_constant(),cursor_row(id) FROM cursor_values; SELECT last_value FROM cursor_checks", true)), ["1"]);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "FETCH 1 FROM values_cursor; SELECT last_value FROM cursor_checks",
                true
            )),
            ["{1}|11", "2"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "FETCH 1 FROM values_cursor; SELECT last_value FROM cursor_checks",
                true
            )),
            ["{1}|13", "2"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "CLOSE values_cursor; DECLARE values_cursor NO SCROLL CURSOR FOR \
             SELECT cursor_constant(),cursor_row(id) FROM cursor_values; FETCH ALL FROM values_cursor; \
             SELECT last_value FROM cursor_checks; CLOSE values_cursor; COMMIT", true)),
            ["{1}|13", "{1}|15", "2"]);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "BEGIN; DECLARE held_values NO SCROLL CURSOR WITH HOLD FOR \
             SELECT cursor_constant(),cursor_row(id) FROM cursor_values; COMMIT; \
             FETCH ALL FROM held_values; SELECT last_value FROM cursor_checks; CLOSE held_values",
                true
            )),
            ["{1}|15", "{1}|17", "2"]
        );
    }
}

#[test]
fn cursor_builtin_hashes_preserve_types_lengths_seeds_and_argument_effects() {
    let config = test_config("cursor-builtin-hashes");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let empty = session.success(
        &mut engine,
        "SELECT hashtext(''),hashtextextended(NULL::text,0), \
        pg_typeof(hashtext(NULL::text)),pg_typeof(hashtextextended(NULL::text,0))",
        true,
    );
    assert_eq!(data_rows(&empty), ["-1477818771|NULL|integer|bigint"]);
    assert_eq!(row_description_type_oids(&empty), [23, 20, 2206, 2206]);
    for length in 0..=60 {
        let sql = format!(
            "SELECT hashtext(repeat('a',{length}))=hashbytea(convert_to(repeat('a',{length}),'UTF8')), \
            hashtextextended(repeat('a',{length}),123)=hashbyteaextended(convert_to(repeat('a',{length}),'UTF8'),123)"
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, &sql, true)),
            ["t|t"],
            "length {length}"
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT hashint2(-1::smallint)=hashint4(-1),hashint4(-1)=hashint8(-1), \
        hashint2extended(-1::smallint,-1)=hashint8extended(-1,-1), \
        hashfloat4(1.5::real)=hashfloat8(1.5::float8), \
        hashfloat4('NaN')=hashfloat8('NaN'),hashfloat8('-0')=0,hashfloat8extended('-0',123)=123, \
        hashbpchar('x  ')=hashbpchar('x'),hashname(repeat('x',64))=hashname(repeat('x',63)), \
        hashchar('a')=hashint4(97),hashbool(true)=hashint4(1),hashoid(42::oid)=hashint4(42)",
            true
        )),
        ["t|t|t|t|t|t|t|t|t|t|t|t"]
    );
    session.success(&mut engine, "CREATE SEQUENCE hash_calls START 10", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT hashtextextended(NULL::text,nextval('hash_calls')); SELECT last_value,is_called FROM hash_calls", true)),
        ["NULL","10|f"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE TABLE hash_inputs(value text); INSERT INTO hash_inputs VALUES(NULL); \
        SELECT hashtextextended(value,nextval('hash_calls')) FROM hash_inputs; \
        SELECT last_value,is_called FROM hash_calls",
            true
        )),
        ["NULL", "10|t"]
    );
    for sql in [
        "SELECT hashtext(1)",
        "SELECT hashint2(1)",
        "SELECT hashtextextended('x',1::numeric)",
        "SELECT hashtext(NULL::date)",
        "SELECT hashtext()",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42883"],
            "{sql}"
        );
    }
}

#[test]
fn cursor_catalog_columns_keep_schema_qualification() {
    let config = test_config("cursor-catalog-column-qualification");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT 'r'::\"char\"=ANY(ARRAY['r','p']), 'r'::\"char\"=ALL(ARRAY['r','r']), \
        'a'::\"char\"='ab'::text, 'a'::\"char\"='ab', 'a'::text='a'::\"char\", \
        ascii(chr(1)::\"char\"::text),ascii(chr(127)::\"char\"::text)",
            true
        )),
        ["t|t|f|t|t|1|127"]
    );
    for sql in [
        "SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class WHERE oid=1259",
        "SELECT pg_catalog.pg_class.relname FROM pg_class WHERE oid=1259",
        "SELECT c.relname FROM pg_catalog.pg_class AS c WHERE c.oid=1259",
        "SELECT (SELECT pg_catalog.pg_class.relname) FROM pg_catalog.pg_class WHERE oid=1259",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            ["pg_class"],
            "{sql}"
        );
    }
    let described = session.success(
        &mut engine,
        "PREPARE catalog_qualified(oid) AS SELECT pg_catalog.pg_class.relname \
        FROM pg_catalog.pg_class WHERE pg_catalog.pg_class.oid=$1; EXECUTE catalog_qualified(1259)",
        true,
    );
    assert_eq!(data_rows(&described), ["pg_class"]);
    assert_eq!(row_description_type_oids(&described), [19]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT information_schema.schemata.schema_name FROM information_schema.schemata \
        WHERE information_schema.schemata.schema_name='public'",
            true
        )),
        ["public"]
    );
    for sql in [
        "SELECT pg_catalog.pg_class.relname FROM pg_catalog.pg_class AS c",
        "SELECT public.pg_class.relname FROM pg_catalog.pg_class",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42P01"],
            "{sql}"
        );
    }
}

#[test]
fn cursor_between_comparisons_preserve_nulls_and_repeated_effects() {
    let config = test_config("cursor-between-comparisons");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT 1 BETWEEN 2 AND NULL,1 NOT BETWEEN 2 AND NULL, \
        1 BETWEEN NULL AND 0,1 NOT BETWEEN NULL AND 0, \
        1 BETWEEN NULL AND 2,1 NOT BETWEEN NULL AND 2",
            true
        )),
        ["f|t|f|t|NULL|NULL"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT 1 BETWEEN SYMMETRIC 2 AND 0,1 NOT BETWEEN SYMMETRIC 2 AND 0, \
        1 BETWEEN SYMMETRIC NULL AND 0,1 NOT BETWEEN SYMMETRIC NULL AND NULL",
            true
        )),
        ["t|f|NULL|NULL"]
    );
    session.success(&mut engine, "CREATE SEQUENCE between_calls START 10", true);
    for (expression, result, last) in [
        ("nextval('between_calls') BETWEEN 10 AND 11", "t", "11"),
        (
            "nextval('between_calls') NOT BETWEEN 100 AND nextval('between_calls')",
            "t",
            "10",
        ),
        (
            "(SELECT nextval('between_calls')) BETWEEN 10 AND 11",
            "t",
            "11",
        ),
        (
            "(1+(SELECT nextval('between_calls'))) BETWEEN 11 AND 12",
            "t",
            "11",
        ),
        (
            "(SELECT (SELECT nextval('between_calls'))) BETWEEN 10 AND 11",
            "t",
            "11",
        ),
        (
            "nextval('between_calls') BETWEEN SYMMETRIC 1 AND 2",
            "f",
            "13",
        ),
        (
            "nextval('between_calls') NOT BETWEEN SYMMETRIC 100 AND 0",
            "f",
            "12",
        ),
    ] {
        session.success(
            &mut engine,
            "ALTER SEQUENCE between_calls RESTART WITH 10",
            true,
        );
        let sql = format!("SELECT {expression}; SELECT last_value FROM between_calls");
        assert_eq!(
            data_rows(&session.success(&mut engine, &sql, true)),
            [result, last],
            "{expression}"
        );
    }
    for sql in [
        "SELECT 1 BETWEEN ASYMMETRIC SYMMETRIC 0 AND 2",
        "SELECT 1 BETWEEN SYMMETRIC ASYMMETRIC 0 AND 2",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42601"]
        );
    }
}

#[test]
fn cursor_between_retains_input_operators_and_deferred_errors() {
    let config = test_config("cursor-between-retained-input");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE between_rows(id integer,lo integer,hi integer); \
        INSERT INTO between_rows VALUES(1,0,2),(3,0,4),(9,5,6); \
        BEGIN; DECLARE between_later NO SCROLL CURSOR FOR \
        SELECT id,id BETWEEN lo AND 100/(id-9) FROM between_rows",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH 2 FROM between_later", true)),
        ["1|f", "3|f"]
    );
    session.success(&mut engine, "SAVEPOINT before_between_error", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM between_later", true)),
        ["22012"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_between_error",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM between_later", true)),
        ["55000"]
    );
    session.success(&mut engine, "ROLLBACK", true);
    assert!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DECLARE between_filtered NO SCROLL CURSOR FOR \
        SELECT id FROM between_rows WHERE id BETWEEN 100 AND 1/(id-9); \
        FETCH ALL FROM between_filtered; COMMIT",
            true
        ))
        .is_empty()
    );
    session.success(&mut engine, "CREATE SCHEMA between_scope; SET search_path=between_scope,pg_catalog; \
        BEGIN; DECLARE between_bound NO SCROLL CURSOR FOR SELECT id,id BETWEEN 0 AND 4 FROM public.between_rows; \
        CREATE FUNCTION between_scope.ge(integer,integer) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT false'; \
        CREATE OPERATOR between_scope.>= (FUNCTION=between_scope.ge,LEFTARG=integer,RIGHTARG=integer)", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT 1>=0; FETCH ALL FROM between_bound; COMMIT",
            true
        )),
        ["f", "1|t", "3|t", "9|f"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SET search_path=public; \
        CREATE VIEW between_view AS SELECT id BETWEEN lo AND hi AS inside FROM between_rows; \
        ALTER TABLE between_rows RENAME COLUMN id TO renamed; SELECT inside FROM between_view",
            true
        )),
        ["t", "t", "f"]
    );
}

#[test]
fn cursor_expression_metadata_preserves_overloads_and_correlated_function_inputs() {
    let config = test_config("cursor-expression-overload-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE DOMAIN cursor_short AS smallint; \
        CREATE DOMAIN cursor_network AS cidr; CREATE DOMAIN cursor_day AS date; \
        CREATE DOMAIN cursor_vector AS integer[]; \
        CREATE TABLE cursor_function_inputs(id integer, options text[], days cursor_day[]); \
        INSERT INTO cursor_function_inputs VALUES(1,ARRAY['a','b'],ARRAY[DATE '2026-10-30',NULL]::cursor_day[]), \
        (2,NULL,NULL),(3,ARRAY['c'],ARRAY[]::cursor_day[])", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT pg_typeof(mod(7::smallint,3::smallint)),pg_typeof(mod(NULL::smallint,3::smallint)), \
        pg_typeof(mod(7::cursor_short,3::cursor_short)), \
        pg_typeof(set_masklen('192.168.1.0/24'::cidr,16)), \
        pg_typeof(set_masklen(NULL::cursor_network,16)), \
        pg_typeof(set_masklen(NULL::inet,16))", true)),
        ["smallint|smallint|smallint|cidr|cidr|inet"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT mod('-9223372036854775808'::bigint,-1::bigint), \
        mod('-2147483648'::integer,-1),mod('-32768'::smallint,-1::smallint)",
            true
        )),
        ["0|0|0"]
    );
    let named = session.success(
        &mut engine,
        "SELECT (SELECT 1 AS first_name UNION ALL SELECT 2 LIMIT 1), \
        (SELECT lower('A') INTERSECT SELECT 'a')",
        true,
    );
    assert_eq!(data_rows(&named), ["1|a"]);
    assert_eq!(row_description_names(&named), ["first_name", "lower"]);
    let arrays = session.success(
        &mut engine,
        "SELECT pg_typeof(ARRAY(SELECT 1::smallint)),pg_typeof(ARRAY(SELECT NULL::numeric)), \
        ARRAY(SELECT 1::smallint)",
        true,
    );
    assert_eq!(data_rows(&arrays), ["smallint[]|numeric[]|{1}"]);
    assert_eq!(row_description_type_oids(&arrays), [2206, 2206, 1005]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_typeof(ARRAY(SELECT ARRAY[1,2]::cursor_vector)), \
        array_ndims(ARRAY(SELECT ARRAY[1,2]::cursor_vector)), \
        cardinality(ARRAY(SELECT ARRAY[1,2]::cursor_vector))",
            true
        )),
        ["cursor_vector[]|1|1"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT id,ARRAY(SELECT 'x.'||option FROM unnest(t.options) option), \
        pg_typeof(ARRAY(SELECT day_value FROM unnest(t.days) day_value)), \
        ARRAY(SELECT day_value FROM unnest(t.days) day_value) \
        FROM cursor_function_inputs t ORDER BY id",
            true
        )),
        [
            "1|{x.a,x.b}|cursor_day[]|{2026-10-30,NULL}",
            "2|{}|cursor_day[]|{}",
            "3|{x.c}|cursor_day[]|{}"
        ]
    );
}

#[test]
fn cursor_named_array_casts_and_function_metadata_use_declared_conversions() {
    let config = test_config("cursor-named-array-conversions");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TYPE array_mood AS ENUM('ready','done'); \
        CREATE DOMAIN array_positive AS integer CHECK(VALUE>0); \
        CREATE DOMAIN array_required AS integer NOT NULL",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT ARRAY['ready',NULL,'done']::array_mood[], \
        ARRAY['1','2']::array_positive[],ARRAY['pg_class']::regclass[],ARRAY['integer']::regtype[]",
            true
        )),
        ["{ready,NULL,done}|{1,2}|{pg_class}|{integer}"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT ARRAY['x'::character(3)]::text[],ARRAY['x'::character(3)]",
            true
        )),
        ["{x}|{\"x  \"}"]
    );
    for (sql, state) in [
        ("SELECT ARRAY['bad']::array_mood[]", "22P02"),
        ("SELECT ARRAY[-1]::array_positive[]", "23514"),
        ("SELECT ARRAY[NULL]::array_required[]", "23502"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state]
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_typeof(log(2,8)),pg_typeof(date_trunc('year',DATE '2020-02-03')); \
        SELECT pg_typeof(bit_and(v)) FROM (VALUES(B'11'::varbit),(NULL::varbit)) AS t(v)",
            true
        )),
        ["numeric|timestamp with time zone", "bit"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "SELECT log(NULL::real,1::real)", true)),
        ["42883"]
    );
}

#[test]
fn cursor_subquery_sequence_context_survives_query_shapes_and_row_changes() {
    let config = test_config("cursor-subquery-sequence-context");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE SEQUENCE effect_calls; CREATE TABLE effect_rows(id integer); INSERT INTO effect_rows VALUES(1),(2),(3)", true);
    for (sql, expected) in [
        ("SELECT (SELECT nextval('effect_calls'))", vec!["1"]),
        (
            "SELECT id,(SELECT nextval('effect_calls')) FROM effect_rows ORDER BY id",
            vec!["1|2", "2|2", "3|2"],
        ),
        (
            "SELECT id,(SELECT nextval('effect_calls')+id) FROM effect_rows ORDER BY id",
            vec!["1|4", "2|6", "3|8"],
        ),
        (
            "SELECT EXISTS(SELECT (SELECT nextval('effect_calls')) FROM effect_rows); SELECT last_value FROM effect_calls",
            vec!["t", "5"],
        ),
        (
            "SELECT sum(id)+(SELECT nextval('effect_calls')) FROM effect_rows",
            vec!["12"],
        ),
        (
            "SELECT id,sum(id) OVER()+(SELECT nextval('effect_calls')) FROM effect_rows ORDER BY id",
            vec!["1|13", "2|13", "3|13"],
        ),
        (
            "SELECT ARRAY(SELECT nextval('effect_calls') FROM effect_rows ORDER BY id)",
            vec!["{8,9,10}"],
        ),
        (
            "SELECT (SELECT nextval('effect_calls') UNION ALL SELECT 999 LIMIT 1)",
            vec!["11"],
        ),
        (
            "UPDATE effect_rows SET id=id WHERE id=1 RETURNING (SELECT nextval('effect_calls')+id)",
            vec!["13"],
        ),
        (
            "INSERT INTO effect_rows VALUES(4) RETURNING (SELECT nextval('effect_calls')+id)",
            vec!["17"],
        ),
        (
            "DELETE FROM effect_rows WHERE id=4 RETURNING (SELECT nextval('effect_calls')+id)",
            vec!["18"],
        ),
        (
            "MERGE INTO effect_rows AS t USING (VALUES(1)) AS x(id) ON t.id=x.id WHEN MATCHED THEN UPDATE SET id=t.id RETURNING (SELECT nextval('effect_calls')+t.id)",
            vec!["16"],
        ),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            expected,
            "{sql}"
        );
    }
    session.success(&mut engine, "CREATE SEQUENCE bound_calls", true);
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT id FROM effect_rows ORDER BY id OFFSET nextval('bound_calls') LIMIT nextval('bound_calls'); SELECT last_value FROM bound_calls", true)), ["2","3","2"]);
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT id FROM effect_rows UNION ALL SELECT 4 ORDER BY 1 OFFSET nextval('bound_calls') LIMIT nextval('bound_calls'); SELECT last_value FROM bound_calls", true)), ["4","4"]);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "SELECT 1 LIMIT -1 OFFSET -1", true)),
        ["2201X"]
    );
}

#[test]
fn cursor_subquery_array_types_resolve_without_running_skipped_effects() {
    let config = test_config("cursor-subquery-array-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_typeof(ARRAY[(SELECT 1)]), \
        pg_typeof(ARRAY[(SELECT NULL::date)]),pg_typeof(ARRAY[(SELECT ARRAY[1,2])]), \
        ARRAY[(SELECT 1),(SELECT 2147483648)]",
            true
        )),
        ["integer[]|date[]|integer[]|{1,2147483648}"]
    );
    session.success(
        &mut engine,
        "CREATE DOMAIN subquery_day AS date; \
        CREATE TABLE subquery_arrays(id integer,d subquery_day); \
        INSERT INTO subquery_arrays VALUES(1,NULL),(2,DATE '2026-10-30')",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT id,pg_typeof(ARRAY[(SELECT d)]),ARRAY[(SELECT d)] FROM subquery_arrays ORDER BY id", true)),
        ["1|subquery_day[]|{NULL}", "2|subquery_day[]|{2026-10-30}"]);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT id,pg_typeof(ARRAY[(SELECT (SELECT d))]) FROM subquery_arrays ORDER BY id",
            true
        )),
        ["1|subquery_day[]", "2|subquery_day[]"]
    );
    session.success(&mut engine, "CREATE SEQUENCE subquery_array_calls; \
        CREATE FUNCTION native_array_choice(choose boolean) RETURNS text LANGUAGE plpgsql AS $$ BEGIN \
          RETURN pg_typeof(CASE WHEN choose THEN ARRAY[7] ELSE ARRAY[(SELECT nextval('subquery_array_calls'))] END)::text; END $$; \
        CREATE FUNCTION native_domain_array(d subquery_day) RETURNS text LANGUAGE plpgsql AS $$ BEGIN \
          RETURN pg_typeof(ARRAY[(SELECT d)])::text; END $$", true);
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT native_array_choice(true),native_domain_array(NULL); SELECT last_value,is_called FROM subquery_array_calls", true)),
        ["bigint[]|subquery_day[]", "1|f"]);
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT native_array_choice(false); SELECT last_value,is_called FROM subquery_array_calls", true)), ["bigint[]", "1|t"]);
    session.success(&mut engine, "CREATE FUNCTION next_array_call() RETURNS bigint LANGUAGE sql VOLATILE AS $$ SELECT nextval('subquery_array_calls') $$", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_typeof(next_array_call()); SELECT last_value FROM subquery_array_calls",
            true
        )),
        ["bigint", "2"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT pg_typeof(ARRAY[(SELECT id FROM subquery_arrays)])",
            true
        )),
        ["21000"]
    );
}

#[test]
fn cursor_array_domain_defaults_encode_their_composite_base() {
    let config = test_config("cursor-array-composite-domain-default");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TYPE cursor_pair AS (id integer,note text); \
        CREATE DOMAIN approved_pair AS cursor_pair CHECK ((VALUE).id>0); \
        CREATE TABLE pair_arrays(items approved_pair[] DEFAULT ARRAY[ROW(7,'saved')::approved_pair]); \
        INSERT INTO pair_arrays DEFAULT VALUES; BEGIN; \
        DECLARE pairs NO SCROLL CURSOR FOR SELECT items[1] FROM pair_arrays", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM pairs; COMMIT", true)),
        ["(7,saved)"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT (items[1]).id,(items[1]).note,pg_typeof(items) FROM pair_arrays",
            true
        )),
        ["7|saved|approved_pair[]"]
    );
}

#[test]
fn cursor_array_constructors_preserve_common_types_and_fixed_arena_limits() {
    use crate::sql::cursor::FetchMotion;
    use crate::sql::types::ArrElem;
    let config = test_config("cursor-array-common-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE typed_arrays(id integer, vals integer[]); \
        INSERT INTO typed_arrays VALUES(1,ARRAY[10,20]),(2,ARRAY[30,40]); \
        BEGIN; DECLARE typed NO SCROLL CURSOR FOR SELECT \
        CASE WHEN id=1 THEN ARRAY[id,2147483648] ELSE ARRAY[id,id+1] END, \
        CASE WHEN id=1 THEN vals[1:2] ELSE ARRAY[2147483648] END, \
        ARRAY[ARRAY[id],ARRAY[2147483648]], ARRAY[id]||ARRAY[2147483648] FROM typed_arrays",
        true,
    );
    session
        .cursors
        .read_request("typed", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained typed array plan");
    for expected in [
        "{1,2147483648}|{10,20}|{{1},{2147483648}}|{1,2147483648}",
        "{2,3}|{2147483648}|{{2},{2147483648}}|{2,2147483648}",
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM typed", true)),
            [expected]
        );
        let mut values = [Datum::Null; 4];
        crate::mem::guard::forbid_alloc(|| {
            session
                .cursors
                .copy_row(
                    "typed",
                    session.cursors.emitted()[0],
                    &session.arena,
                    &mut values,
                )
                .unwrap();
            for value in values {
                assert!(matches!(
                    value,
                    Datum::Array {
                        element: ArrElem::Int8,
                        ..
                    }
                ));
            }
        });
    }
    session.success(&mut engine, "COMMIT", true);
    let members = (0..300)
        .map(|index| format!("id+{index}"))
        .collect::<Vec<_>>()
        .join(",");
    session.success(&mut engine, &format!("BEGIN; DECLARE wide_array NO SCROLL CURSOR FOR SELECT (ARRAY[{members}])[300] FROM typed_arrays"), true);
    session
        .cursors
        .read_request("wide_array", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained 300-element constructor");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM wide_array; COMMIT", true)),
        ["300", "301"]
    );
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT pg_typeof(ARRAY[1::smallint,2]), \
        pg_typeof(ARRAY[1::real,2::numeric]), pg_typeof(ARRAY[DATE '2026-10-01',TIMESTAMP '2026-10-02']), \
        ARRAY[]::integer[],ARRAY[ARRAY[]::integer[],NULL::integer[]]", true)),
        ["integer[]|real[]|timestamp without time zone[]|{}|{}"]);
    for (sql, state) in [
        ("SELECT ARRAY[]", "42P18"),
        ("SELECT ARRAY[1,TRUE]", "42804"),
        ("SELECT ARRAY[1::money,2]", "42846"),
        ("SELECT ARRAY[ARRAY[1],NULL::integer[]]", "2202E"),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            [state]
        );
    }
    session.success(&mut engine, "CREATE DOMAIN array_day AS date; \
        CREATE FUNCTION captured_null_array(input array_day) RETURNS refcursor LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor := 'saved_null_array'; BEGIN \
        OPEN c NO SCROLL FOR SELECT ARRAY[input] FROM typed_arrays; input := DATE '2026-10-30'; RETURN c; END $$; \
        BEGIN; SELECT captured_null_array(NULL)", true);
    session
        .cursors
        .read_request("saved_null_array", FetchMotion::Count(1), false)
        .unwrap()
        .expect("captured typed NULL array plan");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM saved_null_array", true)),
        ["{NULL}"]
    );
    let mut values = [Datum::Null; 1];
    crate::mem::guard::forbid_alloc(|| {
        session
            .cursors
            .copy_row(
                "saved_null_array",
                session.cursors.emitted()[0],
                &session.arena,
                &mut values,
            )
            .unwrap();
        assert!(matches!(
            values[0],
            Datum::Array {
                element: ArrElem::Domain { .. },
                ..
            }
        ));
    });
    session.success(&mut engine, "COMMIT", true);
}

#[test]
fn cursor_retained_arrays_keep_captured_inputs_and_defer_index_errors() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-arrays");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE array_input(id integer, vals integer[]); \
        INSERT INTO array_input VALUES(1,ARRAY[10,20,30]),(2,ARRAY[40,50,60]),(3,NULL); \
        BEGIN; DECLARE arrays NO SCROLL CURSOR FOR SELECT vals[id],(ARRAY[id,id+1])[2],vals[2:3] \
        FROM array_input WHERE (ARRAY[id,0])[1]>0",
        true,
    );
    session
        .cursors
        .read_request("arrays", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained array plan");
    session.success(
        &mut engine,
        "UPDATE array_input SET vals=ARRAY[99,99,99] WHERE id=2",
        true,
    );
    for expected in ["10|2|{20,30}", "50|3|{50,60}", "NULL|4|NULL"] {
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM arrays", true)),
            [expected]
        );
    }
    session.success(&mut engine, "CLOSE arrays; DECLARE index_failure NO SCROLL CURSOR FOR SELECT vals[1/(id-2)] FROM array_input WHERE id<=2", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM index_failure", true)),
        ["NULL"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM index_failure", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK; CREATE FUNCTION captured_array(input integer[]) RETURNS refcursor LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor := 'saved_array'; BEGIN OPEN c NO SCROLL FOR SELECT input[id] FROM array_input; \
        input := ARRAY[99,99,99]; RETURN c; END $$; BEGIN; SELECT captured_array(ARRAY[7,8,9])", true);
    session
        .cursors
        .read_request("saved_array", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained captured array");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM saved_array", true)),
        ["7", "8", "9"]
    );
    session.success(&mut engine, "COMMIT", true);
}

#[test]
fn cursor_retained_case_preserves_operator_types_and_defers_selected_row_errors() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-case-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE conditional_input(id integer, day date, start_day date, at timestamp, start_at timestamp); \
        INSERT INTO conditional_input VALUES \
        (1,DATE '2026-10-03',DATE '2026-10-01',TIMESTAMP '2026-10-03',TIMESTAMP '2026-10-01'), \
        (2,DATE '2026-10-06',DATE '2026-10-01',TIMESTAMP '2026-10-06',TIMESTAMP '2026-10-01'); \
        BEGIN; DECLARE formulas NO SCROLL CURSOR FOR \
        SELECT (day-start_day)+id AS elapsed, CASE WHEN id%2=0 THEN id*10 ELSE 2147483648 END AS chosen, \
        CASE WHEN id=1 THEN at-start_at ELSE NULL END AS span FROM conditional_input \
        WHERE CASE WHEN id>0 THEN TRUE ELSE FALSE END", true);
    session
        .cursors
        .read_request("formulas", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained conditional plan");
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM formulas", true)),
        ["3|2147483648|2 days"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM formulas", true)),
        ["7|20|NULL"]
    );
    let mut values = [Datum::Null; 3];
    crate::mem::guard::forbid_alloc(|| {
        let count = session
            .cursors
            .copy_row(
                "formulas",
                session.cursors.emitted()[0],
                &session.arena,
                &mut values,
            )
            .unwrap();
        assert_eq!(count, 3);
        assert_eq!(values[0], Datum::Int4(7));
        assert_eq!(values[1], Datum::Int8(20));
    });
    session.success(&mut engine, "CLOSE formulas; \
        DECLARE conditional_failure NO SCROLL CURSOR FOR SELECT CASE WHEN id=2 THEN 100/(id-2) ELSE id END FROM conditional_input", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM conditional_failure", true)),
        ["1"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM conditional_failure", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn cursor_view_planning_preserves_sources_and_pattern_boolean_types() {
    let config = test_config("cursor-view-pattern-types");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE pattern_input(label text); \
        INSERT INTO pattern_input VALUES ('a_c'),('abc'); \
        CREATE VIEW pattern_view AS TABLE pattern_input; BEGIN; \
        DECLARE view_rows CURSOR FOR TABLE pattern_view",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM view_rows; COMMIT", true)),
        ["a_c", "abc"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT label,pg_typeof(label SIMILAR TO 'a#_c' ESCAPE '#') FROM pattern_input \
        WHERE label SIMILAR TO 'a#_c' ESCAPE '#' ORDER BY label",
            true
        )),
        ["a_c|boolean"]
    );
    session.success(
        &mut engine,
        "BEGIN; DECLARE pattern_rows NO SCROLL CURSOR FOR \
        SELECT label FROM pattern_view WHERE label SIMILAR TO 'a#_c' ESCAPE '#'",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM pattern_rows; COMMIT", true)),
        ["a_c"]
    );
}

#[test]
fn cursor_retained_fetch_advances_only_requested_rows_and_keeps_opening_values() {
    use crate::config::TransactionMode;
    use crate::sql::cursor::FetchMotion;

    for mode in [TransactionMode::Mvcc, TransactionMode::Locking] {
        let mut config = test_config("cursor-retained-fetch");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE retained_orders(id integer, label text); \
             INSERT INTO retained_orders VALUES (1,'first'),(2,'second'),(3,'third'); \
             BEGIN; DECLARE orders NO SCROLL CURSOR FOR SELECT id,label FROM retained_orders",
            true,
        );
        let (slot, _) = session
            .cursors
            .read_request("orders", FetchMotion::Count(1), false)
            .unwrap()
            .expect("retained physical plan");
        assert_eq!(session.cursors.produced_rows(slot), 0);
        assert_eq!(
            session
                .cursors
                .read_request("orders", FetchMotion::BackwardAll, false)
                .unwrap_err()
                .sqlstate,
            "55000"
        );
        assert!(
            data_rows(&session.success(
                &mut engine,
                "FETCH 0 FROM orders; MOVE 0 FROM orders",
                true
            ))
            .is_empty()
        );
        assert_eq!(session.cursors.produced_rows(slot), 0);
        let output = session.success(&mut engine, "FETCH NEXT FROM orders", true);
        assert_eq!(data_rows(&output), ["1|first"]);
        assert_eq!(session.cursors.produced_rows(slot), 1);
        session.success(
            &mut engine,
            "UPDATE retained_orders SET label='changed' WHERE id=2; \
            INSERT INTO retained_orders VALUES (4,'later')",
            true,
        );
        let output = session.success(&mut engine, "FETCH FORWARD 1 FROM orders", true);
        assert_eq!(data_rows(&output), ["2|second"]);
        assert_eq!(session.cursors.produced_rows(slot), 2);
        let output = session.success(
            &mut engine,
            "MOVE 0 FROM orders; FETCH ALL FROM orders; FETCH NEXT FROM orders",
            true,
        );
        assert_eq!(data_rows(&output), ["3|third"]);
        assert_eq!(session.cursors.produced_rows(slot), 3);
        session.success(&mut engine, "CLOSE orders; COMMIT", true);
        assert!(!session.cursors.exists("orders"));
    }
}

#[test]
fn cursor_retained_native_open_captures_inputs_and_fetch_into_uses_the_reader() {
    let config = test_config("cursor-retained-native");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE retained_native(id integer); \
        INSERT INTO retained_native VALUES (1),(2); \
        CREATE FUNCTION retained_native_fetch() RETURNS text LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor; label text := 'opening'; n integer; saved text; BEGIN \
          OPEN c NO SCROLL FOR SELECT id,label FROM retained_native; label := 'changed'; \
          FETCH NEXT FROM c INTO n,saved; CLOSE c; RETURN saved; END $$",
        true,
    );
    let output = session.success(&mut engine, "SELECT retained_native_fetch()", true);
    assert_eq!(data_rows(&output), ["opening"]);
}

#[test]
fn cursor_retained_table_cannot_be_dropped_or_rewritten_until_close() {
    let config = test_config("cursor-retained-table-use");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE busy_orders(id integer); CREATE INDEX busy_idx ON busy_orders(id); \
        INSERT INTO busy_orders VALUES(1),(2); BEGIN; \
        DECLARE busy NO SCROLL CURSOR FOR SELECT id FROM busy_orders; SAVEPOINT ddl",
        true,
    );
    for sql in [
        "DROP TABLE busy_orders",
        "TRUNCATE busy_orders",
        "ALTER TABLE busy_orders ADD COLUMN extra integer",
        "CLUSTER busy_orders USING busy_idx",
    ] {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(error_sqlstates(&output), ["55006"], "{sql}");
        assert!(
            String::from_utf8_lossy(&output)
                .contains("being used by active queries in this session")
        );
        session.success(&mut engine, "ROLLBACK TO SAVEPOINT ddl", true);
    }
    session.success(
        &mut engine,
        "ALTER TABLE busy_orders RENAME TO renamed_orders",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ALL FROM busy", true)),
        ["1", "2"]
    );
    session.success(&mut engine, "SAVEPOINT finished", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "DROP TABLE renamed_orders", true)),
        ["55006"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT finished; CLOSE busy; \
        ALTER TABLE renamed_orders ADD COLUMN extra integer; DROP TABLE renamed_orders; COMMIT",
        true,
    );
}

#[test]
fn cursor_failed_fetch_stays_failed_after_savepoint_rollback() {
    let config = test_config("cursor-failed-fetch");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for projection in ["id FROM fetch_failure", "1 FROM generate_series(1,2)"] {
        session.success(&mut engine, "CREATE TABLE fetch_failure(id integer); INSERT INTO fetch_failure VALUES(1),(2); BEGIN", true);
        session.success(
            &mut engine,
            &format!("DECLARE failed NO SCROLL CURSOR FOR SELECT {projection}"),
            true,
        );
        session.success(&mut engine, "FETCH NEXT FROM failed; MOVE 0 FROM failed; MOVE RELATIVE 0 FROM failed; SAVEPOINT before_error", true);
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "FETCH 0 FROM failed", true)),
            ["55000"]
        );
        session.success(&mut engine, "ROLLBACK TO SAVEPOINT before_error", true);
        let output = session.execute(&mut engine, "FETCH NEXT FROM failed", true);
        assert_eq!(error_sqlstates(&output), ["55000"]);
        assert!(String::from_utf8_lossy(&output).contains("portal \"failed\" cannot be run"));
        session.success(
            &mut engine,
            "ROLLBACK TO SAVEPOINT before_error; CLOSE failed; DROP TABLE fetch_failure; COMMIT",
            true,
        );
    }
}

#[test]
fn cursor_retained_capacity_error_releases_its_reader_and_stays_failed() {
    let mut config = test_config("cursor-retained-fetch-capacity");
    config.cursor_bytes = 6;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE small_result(id integer); INSERT INTO small_result VALUES(1); \
        BEGIN; DECLARE small NO SCROLL CURSOR FOR SELECT id FROM small_result; SAVEPOINT before_fetch", true);
    let output = session.execute(&mut engine, "FETCH NEXT FROM small", true);
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert!(String::from_utf8_lossy(&output).contains("cursor result exceeds cursor_bytes"));
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_fetch; DROP TABLE small_result",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM small", true)),
        ["55000"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_fetch; CLOSE small; ROLLBACK",
        true,
    );
}

#[test]
fn cursor_retained_input_capacity_names_its_budget_and_releases_failed_open() {
    let mut config = test_config("cursor-retained-input-capacity");
    config.max_cursors = 1;
    config.cursor_query_bytes = 8192;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE many_inputs(id integer); \
        INSERT INTO many_inputs SELECT g FROM generate_series(1,512) AS g; BEGIN",
        true,
    );
    let output = session.execute(
        &mut engine,
        "DECLARE too_many NO SCROLL CURSOR FOR SELECT id FROM many_inputs",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert!(String::from_utf8_lossy(&output).contains("cursor_query_bytes"));
    assert!(!session.cursors.exists("too_many"));
    session.success(
        &mut engine,
        "ROLLBACK; TRUNCATE many_inputs; BEGIN; \
        DECLARE fits NO SCROLL CURSOR FOR SELECT id FROM many_inputs; FETCH ALL FROM fits; COMMIT",
        true,
    );
}

#[test]
fn cursor_retained_narrow_projection_borrows_wide_resident_rows() {
    let mut config = test_config("cursor-retained-wide-resident");
    config.cursor_query_bytes = 8192;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE wide_input(id integer, payload text)",
        true,
    );
    session.success(
        &mut engine,
        &format!(
            "INSERT INTO wide_input VALUES(7,'{}')",
            "x".repeat(128 << 10)
        ),
        true,
    );
    session.success(
        &mut engine,
        "BEGIN; DECLARE narrow NO SCROLL CURSOR FOR SELECT id FROM wide_input; \
        UPDATE wide_input SET id=9",
        true,
    );
    let output = session.success(&mut engine, "FETCH ALL FROM narrow", true);
    assert_eq!(data_rows(&output), ["7"]);
    session.success(&mut engine, "CLOSE narrow; COMMIT", true);
}

#[test]
fn cursor_retained_reuses_fetch_memory_and_discards_move_and_skipped_rows() {
    use crate::sql::cursor::FetchMotion;
    let mut config = test_config("cursor-retained-fetch-buffer");
    config.max_cursors = 1;
    config.cursor_bytes = 32;
    config.cursor_query_bytes = 8192;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE forward_input(id integer); \
        INSERT INTO forward_input SELECT g FROM generate_series(1,128) AS g; BEGIN; \
        DECLARE forward_rows NO SCROLL CURSOR FOR SELECT id FROM forward_input",
        true,
    );
    let (slot, _) = session
        .cursors
        .read_request("forward_rows", FetchMotion::Count(1), false)
        .unwrap()
        .unwrap();
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE BACKWARD ALL FROM forward_rows", true)),
        ["MOVE 0"]
    );
    for id in 1..=128 {
        let output = session.success(&mut engine, "FETCH NEXT FROM forward_rows", true);
        assert_eq!(data_rows(&output), [id.to_string()]);
        assert_eq!(session.cursors.produced_rows(slot), id);
        assert_eq!(session.cursors.buffered_rows(slot), 1);
    }
    session.success(
        &mut engine,
        "CLOSE forward_rows; \
        DECLARE forward_rows NO SCROLL CURSOR FOR SELECT id FROM forward_input",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ABSOLUTE 100 FROM forward_rows", true)),
        ["100"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 1);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH RELATIVE 10 FROM forward_rows", true)),
        ["110"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 1);
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE FORWARD 10 FROM forward_rows", true)),
        ["MOVE 10"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 0);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM forward_rows", true)),
        ["121"]
    );
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE ALL FROM forward_rows", true)),
        ["MOVE 7"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 0);
    session.success(
        &mut engine,
        "CLOSE forward_rows; \
        DECLARE forward_rows NO SCROLL CURSOR FOR SELECT id FROM forward_input; SAVEPOINT last_row",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH LAST FROM forward_rows", true)),
        ["55000"]
    );
    assert_eq!(
        session.cursors.produced_rows(slot),
        128,
        "LAST advances forward before its forbidden backward step"
    );
    assert_eq!(session.cursors.buffered_rows(slot), 0);
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT last_row; CLOSE forward_rows; COMMIT",
        true,
    );
}

#[test]
fn cursor_retained_filters_advance_input_without_saving_rejected_rows() {
    use crate::config::TransactionMode;
    use crate::sql::cursor::FetchMotion;
    for mode in [TransactionMode::Mvcc, TransactionMode::Locking] {
        let mut config = test_config("cursor-retained-filter");
        config.transaction_mode = mode;
        config.cursor_bytes = 32;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE filtered_input(id integer); \
             INSERT INTO filtered_input SELECT g FROM generate_series(1,128) AS g; BEGIN; \
             DECLARE filtered NO SCROLL CURSOR FOR \
             SELECT id*2 FROM filtered_input WHERE id%3=0 AND NOT (id IS NULL)",
            true,
        );
        let (slot, _) = session
            .cursors
            .read_request("filtered", FetchMotion::Count(1), false)
            .unwrap()
            .expect("retained filter plan");
        assert_eq!(session.cursors.produced_rows(slot), 0);
        for n in 1..=42 {
            let output = session.success(&mut engine, "FETCH NEXT FROM filtered", true);
            assert_eq!(data_rows(&output), [(n * 6).to_string()]);
            assert_eq!(session.cursors.buffered_rows(slot), 1);
            let statistics = session.success(&mut engine,
                "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='filtered_input'", true);
            assert_eq!(data_rows(&statistics), [format!("1|{}", n * 3)]);
        }
        assert_eq!(
            command_tags(&session.success(&mut engine, "FETCH NEXT FROM filtered", true)),
            ["FETCH 0"]
        );
        let statistics = session.success(&mut engine,
            "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='filtered_input'", true);
        assert_eq!(data_rows(&statistics), ["1|128"]);
        session.success(&mut engine, "CLOSE filtered; COMMIT", true);
    }
}

#[test]
fn cursor_retained_default_mode_defers_reads_and_matches_plan_scrollability() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-default-mode");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE default_input(id integer); INSERT INTO default_input VALUES (1),(2),(3); \
        BEGIN; DECLARE automatic CURSOR FOR SELECT id FROM default_input",
        true,
    );
    let (slot, _) = session
        .cursors
        .read_request("automatic", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained default cursor");
    assert_eq!(session.cursors.produced_rows(slot), 0);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT is_scrollable FROM pg_cursors WHERE name='automatic'",
            true
        )),
        ["t"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM automatic", true)),
        ["1"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH LAST FROM automatic", true)),
        ["3"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH FIRST FROM automatic", true)),
        ["1"]
    );
    session.success(&mut engine, "CLOSE automatic; COMMIT", true);
    for (option, expected) in [("", "f"), ("SCROLL", "t")] {
        session.success(
            &mut engine,
            &format!("BEGIN; DECLARE singleton_mode {option} CURSOR FOR SELECT 7 AS value"),
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_scrollable FROM pg_cursors WHERE name='singleton_mode'; \
             FETCH NEXT FROM singleton_mode; CLOSE singleton_mode; COMMIT",
                true
            )),
            [expected, "7"]
        );
    }
    for predicate in ["FALSE", "'f'", "NULL AND TRUE"] {
        session.success(&mut engine, &format!("BEGIN; DECLARE empty_cursor CURSOR FOR SELECT id FROM default_input WHERE {predicate}"), true);
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_scrollable FROM pg_cursors WHERE name='empty_cursor'",
                true
            )),
            ["f"]
        );
        assert_eq!(
            command_tags(&session.success(&mut engine, "FETCH ALL FROM empty_cursor", true)),
            ["FETCH 0"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='default_input'", true)), ["0|0"]);
        session.success(&mut engine, "CLOSE empty_cursor; COMMIT", true);
    }
}

#[test]
fn cursor_retained_default_filter_is_bound_to_captured_boolean_values() {
    let config = test_config("cursor-retained-captured-filter");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE captured_filter_input(id integer); INSERT INTO captured_filter_input VALUES (1),(2); \
        CREATE FUNCTION captured_filter(keep boolean) RETURNS refcursor LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor := 'captured'; BEGIN OPEN c FOR SELECT id FROM captured_filter_input WHERE keep; \
          keep := NOT keep; RETURN c; END $$", true);
    for (value, scrollable, expected) in [
        ("false", "f", vec![]),
        ("true", "t", vec!["1", "2"]),
        ("NULL", "f", vec![]),
    ] {
        session.success(
            &mut engine,
            &format!("BEGIN; SELECT captured_filter({value})"),
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT is_scrollable FROM pg_cursors WHERE name='captured'",
                true
            )),
            [scrollable]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH ALL FROM captured", true)),
            expected
        );
        session.success(&mut engine, "COMMIT", true);
    }
}

#[test]
fn cursor_retained_declaration_and_empty_filter_do_not_mark_unread_tables_as_read() {
    let config = test_config("cursor-retained-unread-serializable");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "CREATE TABLE unread_cursor_input(id integer); INSERT INTO unread_cursor_input VALUES (1)",
        true,
    );
    for query in [
        "SELECT id FROM unread_cursor_input",
        "SELECT id FROM unread_cursor_input WHERE FALSE",
    ] {
        reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL SERIALIZABLE; DECLARE unread CURSOR FOR {query}"),
            true,
        );
        if query.ends_with("FALSE") {
            reader.success(&mut engine, "FETCH ALL FROM unread", true);
        }
        writer.success(&mut engine, "UPDATE unread_cursor_input SET id=id+1", true);
        reader.success(&mut engine, "COMMIT", true);
    }
}

#[test]
fn cursor_retained_type_analysis_precedes_execution_permissions() {
    let config = test_config("cursor-retained-type-permission-order");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE permission_input(id integer); CREATE ROLE cursor_denied; SET ROLE cursor_denied; BEGIN", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "DECLARE bad NO SCROLL CURSOR FOR SELECT id+TRUE FROM permission_input",
            true
        )),
        ["42883"]
    );
    session.success(&mut engine, "ROLLBACK; BEGIN", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "DECLARE bad NO SCROLL CURSOR FOR SELECT id FROM permission_input",
            true
        )),
        ["42501"]
    );
    session.success(&mut engine, "ROLLBACK; RESET ROLE", true);
}

#[test]
fn cursor_retained_implicit_commit_triggers_can_inspect_the_connection_cursors() {
    let config = test_config("cursor-retained-commit-cursor-catalog");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE cursor_catalog_input(id integer); INSERT INTO cursor_catalog_input VALUES (1); \
        CREATE TABLE cursor_catalog_gate(id integer); \
        CREATE FUNCTION inspect_commit_cursor() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
          IF (SELECT count(*) FROM pg_cursors WHERE name='held') <> 1 THEN RAISE EXCEPTION 'cursor missing'; END IF; \
          RAISE EXCEPTION 'cursor visible'; END $$; \
        CREATE CONSTRAINT TRIGGER inspect_commit_cursor AFTER INSERT ON cursor_catalog_gate DEFERRABLE INITIALLY DEFERRED \
        FOR EACH ROW EXECUTE FUNCTION inspect_commit_cursor()", true);
    let failed = session.execute(&mut engine,
        "DECLARE held CURSOR WITH HOLD FOR SELECT id FROM cursor_catalog_input; INSERT INTO cursor_catalog_gate VALUES (1)", true);
    assert_eq!(
        error_sqlstates(&failed),
        ["P0001"],
        "{}",
        String::from_utf8_lossy(&failed)
    );
    assert!(
        String::from_utf8_lossy(&failed).contains("cursor visible"),
        "{}",
        String::from_utf8_lossy(&failed)
    );
    assert!(!session.cursors.exists("held"));
}

#[test]
fn cursor_trigger_expressions_resolve_subqueries_with_locals_and_transition_rows() {
    let config = test_config("cursor-trigger-expression-subqueries");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE expression_input(id integer); INSERT INTO expression_input VALUES (1),(2); \
        CREATE FUNCTION expression_value(wanted integer) RETURNS integer LANGUAGE plpgsql AS $$ \
        DECLARE saved integer := (SELECT id FROM expression_input WHERE id=wanted); BEGIN \
          IF EXISTS (SELECT id FROM expression_input WHERE id=saved) THEN \
            saved := (SELECT id+10 FROM expression_input WHERE id=saved); END IF; \
          RETURN (SELECT saved); END $$",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT expression_value(1),expression_value(2),expression_value(3)",
            true
        )),
        ["11|12|NULL"]
    );
    session.success(
        &mut engine,
        "CREATE FUNCTION expression_many() RETURNS integer LANGUAGE plpgsql AS $$ \
        BEGIN RETURN (SELECT id FROM expression_input); END $$",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "SELECT expression_many()", true)),
        ["21000"]
    );
    session.success(&mut engine, "CREATE FUNCTION expression_lazy(choose boolean) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN \
        IF choose OR EXISTS (SELECT id FROM expression_input WHERE 10/(id-id)>0) THEN RETURN 7; END IF; RETURN 0; END $$; \
        CREATE FUNCTION expression_choice(choose boolean) RETURNS integer LANGUAGE plpgsql AS $$ BEGIN \
        RETURN CASE WHEN choose THEN 7 ELSE (SELECT 10/(id-id) FROM expression_input WHERE id=1) END; END $$", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT expression_lazy(true),expression_choice(true)",
            true
        )),
        ["7|7"]
    );
    for query in [
        "SELECT expression_lazy(false)",
        "SELECT expression_choice(false)",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, query, true)),
            ["22012"]
        );
    }
    session.success(&mut engine, "CREATE TABLE expression_target(id integer); \
        CREATE FUNCTION expression_transition() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN \
          IF EXISTS (SELECT id FROM expression_input WHERE id=NEW.id) THEN NEW.id := NEW.id+10; END IF; RETURN NEW; END $$; \
        CREATE TRIGGER expression_transition BEFORE INSERT ON expression_target FOR EACH ROW EXECUTE FUNCTION expression_transition()", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "INSERT INTO expression_target VALUES (1),(3) RETURNING id",
            true
        )),
        ["11", "3"]
    );
    session.success(&mut engine, "CREATE FUNCTION expression_membership(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN \
        RETURN (SELECT wanted) IN (SELECT id FROM expression_input); END $$; \
        CREATE FUNCTION expression_quantified(wanted integer) RETURNS boolean LANGUAGE plpgsql AS $$ BEGIN \
        RETURN wanted > ALL (SELECT id FROM expression_input); END $$", true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT expression_membership(1),expression_membership(3),expression_membership(NULL), \
        expression_quantified(3),expression_quantified(1)",
            true
        )),
        ["t|f|NULL|t|f"]
    );
}

#[test]
fn cursor_retained_hold_saves_unread_rows_at_commit_and_preserves_position() {
    use crate::sql::cursor::FetchMotion;
    for scroll in ["NO SCROLL", "SCROLL"] {
        let config = test_config("cursor-retained-hold");
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine, &format!(
            "CREATE TABLE hold_input(id integer); INSERT INTO hold_input VALUES (1),(2),(3),(4); \
             BEGIN; DECLARE held {scroll} CURSOR WITH HOLD FOR SELECT id*10 FROM hold_input"), true);
        let (slot, _) = session
            .cursors
            .read_request("held", FetchMotion::Count(1), false)
            .unwrap()
            .expect("retained held reader before commit");
        assert_eq!(session.cursors.produced_rows(slot), 0);
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["10"]
        );
        session.success(&mut engine, "COMMIT; DROP TABLE hold_input", true);
        assert!(
            session
                .cursors
                .read_request("held", FetchMotion::Count(1), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["20"]
        );
        session.success(&mut engine, "BEGIN", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["30"]
        );
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
            ["40"]
        );
        if scroll == "SCROLL" {
            assert_eq!(
                data_rows(&session.success(&mut engine, "FETCH FIRST FROM held", true)),
                ["10"]
            );
        }
        session.success(&mut engine, "CLOSE held", true);
        session.success(
            &mut engine,
            &format!(
                "BEGIN; CREATE TEMPORARY TABLE temporary_hold_input(id integer) ON COMMIT DROP; \
             INSERT INTO temporary_hold_input VALUES (7); \
             DECLARE held {scroll} CURSOR WITH HOLD FOR SELECT id FROM temporary_hold_input; COMMIT"
            ),
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH ALL FROM held; CLOSE held", true)),
            ["7"]
        );
    }
}

#[test]
fn cursor_retained_autocommit_hold_survives_later_rollback_and_failed_query_message() {
    let config = test_config("cursor-retained-autocommit-hold");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE automatic_hold_input(id integer); \
        INSERT INTO automatic_hold_input VALUES (1),(2); \
        DECLARE permanent NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM automatic_hold_input",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; FETCH NEXT FROM permanent; ROLLBACK",
            true
        )),
        ["1"]
    );
    let failed = session.execute(&mut engine,
        "DECLARE discarded NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM automatic_hold_input; SELECT 1/0", true);
    assert_eq!(error_sqlstates(&failed), ["22012"]);
    assert!(!session.cursors.exists("discarded"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM permanent", true)),
        ["2"]
    );
    session.success(&mut engine, "CLOSE permanent", true);
}

#[test]
fn cursor_retained_hold_finishes_with_a_legacy_large_object_transaction() {
    let config = test_config("cursor-retained-legacy-commit");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE legacy_hold_input(id integer); INSERT INTO legacy_hold_input VALUES (1),(2)",
        true,
    );
    session.send.clear();
    session.arena.reset();
    let result = crate::mem::guard::forbid_alloc(|| {
        engine.execute_extended(
            "DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM legacy_hold_input",
            &session.arena,
            &[],
            &[],
            &mut session.transaction,
            &mut session.pool,
            &mut session.cursors,
            &session.guc,
            &mut Responder::new(&mut session.send),
            session.connection_id,
            false,
        )
    })
    .unwrap();
    assert_eq!(result, ExtendedExecutionStatus::Complete(true));
    assert_eq!(session.transaction.mode, TxnMode::ImplicitBlock);
    session.send.clear();
    session.arena.reset();
    let result = crate::mem::guard::forbid_alloc(|| {
        engine.execute_large_object_fast_path(
            3457,
            &[Datum::Oid(4242), Datum::Bytea(b"held")],
            true,
            &session.arena,
            &mut session.transaction,
            &mut session.pool,
            &session.guc,
            &mut session.cursors,
            &mut Responder::new(&mut session.send),
            session.connection_id,
        )
    })
    .unwrap();
    assert_eq!(result, ExtendedExecutionStatus::Complete(true));
    assert!(!session.transaction.is_active());
    assert_eq!(
        data_rows(&session.success(&mut engine, "BEGIN; FETCH NEXT FROM held; ROLLBACK", true)),
        ["1"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM held; CLOSE held", true)),
        ["2"]
    );
}

#[test]
fn cursor_retained_hold_commit_errors_close_new_cursors_and_rollback() {
    let mut config = test_config("cursor-retained-hold-commit-error");
    config.cursor_bytes = 32;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE hold_error(id integer); INSERT INTO hold_error VALUES (1),(0); \
        BEGIN; DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT 100/id FROM hold_error",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM held", true)),
        ["100"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "COMMIT", true)),
        ["22012"]
    );
    assert!(!session.transaction.is_active());
    assert!(!session.cursors.exists("held"));
    session.success(&mut engine, "TRUNCATE hold_error; INSERT INTO hold_error SELECT g FROM generate_series(1,8) AS g; \
        BEGIN; INSERT INTO hold_error VALUES (9); \
        DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT id FROM hold_error; FETCH NEXT FROM held", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "COMMIT", true)),
        ["54000"]
    );
    assert!(!session.transaction.is_active());
    assert!(!session.cursors.exists("held"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM hold_error", true)),
        ["8"]
    );
}

#[test]
fn cursor_retained_hold_runs_deferred_triggers_before_saving_unread_results() {
    let config = test_config("cursor-retained-hold-trigger-order");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE hold_trigger_input(id integer); INSERT INTO hold_trigger_input VALUES (0); \
        CREATE TABLE hold_trigger_gate(id integer); \
        CREATE FUNCTION hold_trigger_failure() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'deferred gate'; END $$; \
        CREATE CONSTRAINT TRIGGER hold_gate AFTER INSERT ON hold_trigger_gate DEFERRABLE INITIALLY DEFERRED \
        FOR EACH ROW EXECUTE FUNCTION hold_trigger_failure(); \
        BEGIN; DECLARE held NO SCROLL CURSOR WITH HOLD FOR SELECT 100/id FROM hold_trigger_input; \
        INSERT INTO hold_trigger_gate VALUES (1)", true);
    let failed = session.execute(&mut engine, "COMMIT", true);
    assert_eq!(error_sqlstates(&failed), ["P0001"]);
    assert!(String::from_utf8_lossy(&failed).contains("deferred gate"));
    assert!(!session.cursors.exists("held"));
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM hold_trigger_gate", true)),
        ["0"]
    );
}

#[test]
fn cursor_trigger_functions_validate_return_when_the_execution_path_finishes() {
    let config = test_config("cursor-trigger-return-paths");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE trigger_return_input(id integer); \
        CREATE FUNCTION trigger_conditional_return() RETURNS trigger LANGUAGE plpgsql AS $$ \
        BEGIN IF NEW.id>0 THEN RETURN NEW; END IF; END $$; \
        CREATE TRIGGER conditional_return BEFORE INSERT ON trigger_return_input \
        FOR EACH ROW EXECUTE FUNCTION trigger_conditional_return(); INSERT INTO trigger_return_input VALUES (1)", true);
    let row_error = session.execute(
        &mut engine,
        "INSERT INTO trigger_return_input VALUES (0)",
        true,
    );
    assert_eq!(error_sqlstates(&row_error), ["2F005"]);
    assert!(
        String::from_utf8_lossy(&row_error)
            .contains("control reached end of trigger procedure without RETURN")
    );
    session.success(&mut engine, "DROP TRIGGER conditional_return ON trigger_return_input; \
        CREATE FUNCTION trigger_missing_statement_return() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NULL; END $$; \
        CREATE TRIGGER missing_statement_return AFTER INSERT ON trigger_return_input \
        FOR EACH STATEMENT EXECUTE FUNCTION trigger_missing_statement_return()", true);
    let statement_error = session.execute(
        &mut engine,
        "INSERT INTO trigger_return_input VALUES (2)",
        true,
    );
    assert_eq!(error_sqlstates(&statement_error), ["2F005"]);
    assert!(
        String::from_utf8_lossy(&statement_error)
            .contains("control reached end of trigger procedure without RETURN")
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT id FROM trigger_return_input", true)),
        ["1"]
    );
}

#[test]
fn cursor_retained_scroll_keeps_one_fetch_and_rereads_with_its_snapshot() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-scroll");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE scroll_input(id integer); INSERT INTO scroll_input SELECT g FROM generate_series(1,8) AS g; \
         BEGIN; DECLARE scrolling SCROLL CURSOR FOR SELECT id*10 FROM scroll_input WHERE id%2=0; \
         UPDATE scroll_input SET id=id+100", true);
    let (slot, _) = session
        .cursors
        .read_request("scrolling", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained scrolling plan");
    assert_eq!(session.cursors.produced_rows(slot), 0);
    assert_eq!(
        command_tags(&session.success(&mut engine, "FETCH PRIOR FROM scrolling", true)),
        ["FETCH 0"]
    );
    assert_eq!(session.cursors.produced_rows(slot), 0);
    // The cursor keeps one fetch's rows; returning to an earlier row reads
    // the input again from the first row with the cursor's snapshot.
    for (sql, expected, produced) in [
        ("FETCH NEXT FROM scrolling", "20", 1),
        ("FETCH 0 FROM scrolling", "20", 1),
        ("FETCH NEXT FROM scrolling", "40", 2),
        ("FETCH PRIOR FROM scrolling", "20", 1),
    ] {
        assert_eq!(
            data_rows(&session.success(&mut engine, sql, true)),
            [expected]
        );
        assert_eq!(session.cursors.produced_rows(slot), produced);
        assert_eq!(session.cursors.buffered_rows(slot), 1);
    }
    assert_eq!(
        command_tags(&session.success(&mut engine, "MOVE FORWARD 2 FROM scrolling", true)),
        ["MOVE 2"]
    );
    assert_eq!(session.cursors.produced_rows(slot), 3);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH 0 FROM scrolling", true)),
        ["60"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH LAST FROM scrolling", true)),
        ["80"]
    );
    assert_eq!(session.cursors.buffered_rows(slot), 1);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH BACKWARD ALL FROM scrolling", true)),
        ["60", "40", "20"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH FIRST FROM scrolling", true)),
        ["20"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH ABSOLUTE -2 FROM scrolling", true)),
        ["60"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH RELATIVE -1 FROM scrolling", true)),
        ["40"]
    );
    session.success(&mut engine, "CLOSE scrolling; ROLLBACK", true);
}

#[test]
fn cursor_retained_scroll_capacity_failure_releases_input_and_keeps_portal_failed() {
    let mut config = test_config("cursor-retained-scroll-capacity");
    config.cursor_bytes = 32;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE scroll_capacity(id integer); INSERT INTO scroll_capacity SELECT g FROM generate_series(1,8) AS g; \
         BEGIN; DECLARE scrolling SCROLL CURSOR FOR SELECT id FROM scroll_capacity; \
         FETCH NEXT FROM scrolling; SAVEPOINT full_history", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH ALL FROM scrolling", true)),
        ["54000"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT full_history; DROP TABLE scroll_capacity",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM scrolling", true)),
        ["55000"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn cursor_retained_limit_and_offset_keep_progress_and_stop_input_reads() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-row-bounds");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE bounded_input(id integer); INSERT INTO bounded_input SELECT g FROM generate_series(1,8) AS g; \
         BEGIN; DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM bounded_input LIMIT 1+1 OFFSET 2", true);
    assert!(
        session
            .cursors
            .read_request("bounded", FetchMotion::Count(1), false)
            .unwrap()
            .is_some()
    );
    for (id, scanned) in [(3, 3), (4, 4)] {
        assert_eq!(
            data_rows(&session.success(&mut engine, "FETCH NEXT FROM bounded", true)),
            [id.to_string()]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='bounded_input'", true)), [format!("1|{scanned}")]);
    }
    assert_eq!(
        command_tags(&session.success(&mut engine, "FETCH ALL FROM bounded", true)),
        ["FETCH 0"]
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='bounded_input'", true)), ["1|4"]);
    session.success(&mut engine, "CLOSE bounded; COMMIT; BEGIN; \
        DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM bounded_input LIMIT 0 OFFSET 5; FETCH ALL FROM bounded", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='bounded_input'", true)), ["0|0"]);
    session.success(&mut engine, "CLOSE bounded; COMMIT", true);
    for (clause, state) in [
        ("LIMIT -1", "2201W"),
        ("OFFSET -1", "2201X"),
        ("WHERE FALSE LIMIT -1", "2201W"),
    ] {
        session.success(
            &mut engine,
            &format!(
                "BEGIN; DECLARE bounded NO SCROLL CURSOR FOR SELECT id FROM bounded_input {clause}"
            ),
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM bounded", true)),
            [state]
        );
        session.success(&mut engine, "ROLLBACK", true);
    }
    session.success(&mut engine,
        "CREATE FUNCTION bounded_cursor_input() RETURNS integer LANGUAGE plpgsql AS $$ \
         DECLARE c refcursor; max_rows integer := 2; skipped_rows integer := 3; selected integer; BEGIN \
           OPEN c NO SCROLL FOR SELECT id FROM bounded_input LIMIT max_rows OFFSET skipped_rows; \
           max_rows := 0; skipped_rows := 0; FETCH NEXT FROM c INTO selected; CLOSE c; RETURN selected; END $$", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT bounded_cursor_input()", true)),
        ["4"]
    );
}

#[test]
fn cursor_retained_builtin_operators_keep_their_declared_identity() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-operator-binding");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SCHEMA cursor_ops; SET search_path=cursor_ops,pg_catalog; \
         CREATE TABLE operator_input(id integer); INSERT INTO operator_input VALUES (1),(2); \
         BEGIN; DECLARE bound_ops NO SCROLL CURSOR FOR SELECT id+10 FROM operator_input",
        true,
    );
    assert!(
        session
            .cursors
            .read_request("bound_ops", FetchMotion::Count(1), false)
            .unwrap()
            .is_some()
    );
    session.success(&mut engine,
        "CREATE FUNCTION cursor_ops.custom_add(integer,integer) RETURNS integer LANGUAGE SQL RETURN $1-$2; \
         CREATE OPERATOR cursor_ops.+ (FUNCTION=cursor_ops.custom_add,LEFTARG=integer,RIGHTARG=integer)", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM bound_ops", true)),
        ["11"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM bound_ops", true)),
        ["12"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT id+10 FROM operator_input", true)),
        ["-9", "-8"]
    );
    session.success(
        &mut engine,
        "CLOSE bound_ops; ROLLBACK; RESET search_path",
        true,
    );
}

#[test]
fn cursor_retained_errors_occur_when_the_requested_row_is_evaluated() {
    let config = test_config("cursor-retained-expression-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE expression_input(id integer); INSERT INTO expression_input VALUES (1),(2),(0); \
         BEGIN; DECLARE calculated NO SCROLL CURSOR FOR SELECT 100/id FROM expression_input", true);
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["100"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["50"]
    );
    session.success(&mut engine, "SAVEPOINT before_error", true);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK TO SAVEPOINT before_error", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT seq_scan,seq_tup_read FROM pg_stat_xact_user_tables WHERE relname='expression_input'", true)), ["1|3"]);
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM calculated", true)),
        ["55000"]
    );
    session.success(
        &mut engine,
        "ROLLBACK TO SAVEPOINT before_error; CLOSE calculated; ROLLBACK",
        true,
    );
    for (select, state) in [
        ("SELECT 1/0 FROM expression_input", "22012"),
        ("SELECT id FROM expression_input WHERE id", "42804"),
    ] {
        session.success(&mut engine, "BEGIN", true);
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("DECLARE invalid NO SCROLL CURSOR FOR {select}"),
                true
            )),
            [state]
        );
        assert!(!session.cursors.exists("invalid"));
        session.success(&mut engine, "ROLLBACK", true);
    }
    session.success(&mut engine,
        "BEGIN; DECLARE empty_filter NO SCROLL CURSOR FOR SELECT id FROM expression_input WHERE FALSE; \
         DROP TABLE expression_input; FETCH ALL FROM empty_filter; COMMIT", true);
}

#[test]
fn cursor_dynamic_open_rejects_multiple_commands_before_issuing_sequence_values() {
    let config = test_config("cursor-open-command-count");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE cursor_open_calls; \
        CREATE FUNCTION cursor_extra_commands() RETURNS void LANGUAGE plpgsql AS $$ \
        DECLARE c refcursor; BEGIN \
          OPEN c FOR EXECUTE 'SELECT nextval(''cursor_open_calls''); SELECT 2'; \
          RETURN; END $$",
        true,
    );
    let output = session.execute(&mut engine, "SELECT cursor_extra_commands()", true);
    assert_eq!(error_sqlstates(&output), ["42P11"]);
    assert!(String::from_utf8_lossy(&output).contains("cannot open multi-query plan as cursor"));
    let output = session.success(
        &mut engine,
        "SELECT last_value, is_called FROM cursor_open_calls",
        true,
    );
    assert_eq!(data_rows(&output), ["1|f"]);
}

#[test]
fn cursor_declaration_rejects_conflicting_options_and_write_effects() {
    let config = test_config("cursor-declaration-errors");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut errors = Vec::new();
    let mut rows = Vec::new();
    let mut details = Vec::new();
    for sql in include_str!("../../../tests/external/differential_exact/35_cursor_declarations.sql")
        .split(';')
        .filter(|sql| !sql.trim().is_empty())
    {
        let output = session.execute(&mut engine, sql, true);
        errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
        rows.extend(data_rows(&output));
        let text = String::from_utf8_lossy(&output);
        for detail in ["Holdable", "Scrollable", "Insensitive"] {
            if text.contains(&format!("{detail} cursors must be READ ONLY.")) {
                details.push(detail);
            }
        }
    }
    // PostgreSQL explains each read-only restriction in DETAIL.
    assert_eq!(
        details,
        ["Holdable", "Scrollable", "Insensitive", "Holdable"]
    );
    assert_eq!(
        errors,
        [
            "42P11", "42P11", "42P11", "42P11", "0A000", "0A000", "42P11", "0A000", "0A000",
            "0A000", "0A000"
        ]
    );
    assert_eq!(rows, ["2|3", "1|f"]);
}

#[test]
fn cursor_fetch_renders_saved_values_with_current_session_settings() {
    let config = test_config("cursor-fetch-rendering");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let values = "DATE '2020-01-02', ARRAY[DATE '2020-01-02'], \
        ROW(DATE '2020-01-02', TIMESTAMPTZ '2020-01-02 03:04:05+00'), \
        '\\x5c00'::bytea, 1.234567891234567::float8";
    for hold in [false, true] {
        session.success(
            &mut engine,
            "SET DateStyle = 'ISO, MDY'; SET TimeZone = 'UTC'; \
            SET bytea_output = 'hex'; SET extra_float_digits = 3; BEGIN",
            true,
        );
        session.success(
            &mut engine,
            &format!(
                "DECLARE rendered SCROLL CURSOR {} FOR SELECT {values} FROM generate_series(1,3)",
                if hold { "WITH HOLD" } else { "" },
            ),
            true,
        );
        if hold {
            session.success(&mut engine, "COMMIT", true);
        }
        for setting in [
            "SET DateStyle = 'ISO, MDY'",
            "SET DateStyle = 'SQL, DMY'; SET TimeZone = 'Europe/Bucharest'; \
                SET bytea_output = 'escape'; SET extra_float_digits = 0",
            "SET DateStyle = 'German'; SET TimeZone = 'UTC'",
        ] {
            session.success(&mut engine, setting, true);
            let expected = session.success(&mut engine, &format!("SELECT {values}"), true);
            let fetched = session.success(&mut engine, "FETCH NEXT FROM rendered", true);
            assert_eq!(
                data_rows(&fetched),
                data_rows(&expected),
                "hold={hold}, {setting}"
            );
            assert_eq!(
                row_description_type_oids(&fetched),
                row_description_type_oids(&expected)
            );
        }
        session.success(&mut engine, "CLOSE rendered", true);
        if !hold {
            session.success(&mut engine, "COMMIT", true);
        }
    }
}

#[test]
fn cursor_fetch_into_keeps_dates_typed_after_date_order_changes() {
    let config = test_config("cursor-fetch-typed-assignment");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE FUNCTION fetch_typed_date() RETURNS date \
        LANGUAGE plpgsql AS $$ DECLARE c refcursor; d date; BEGIN \
          OPEN c FOR SELECT DATE '2020-01-02'; \
          SET DateStyle = 'SQL, DMY'; FETCH NEXT FROM c INTO d; CLOSE c; RETURN d; \
        END $$",
        true,
    );
    let output = session.success(
        &mut engine,
        "SET DateStyle = 'ISO, MDY'; SELECT fetch_typed_date() = DATE '2020-01-02'",
        true,
    );
    assert_eq!(data_rows(&output), ["t"]);
}

#[test]
fn cursor_typed_rows_use_exact_capacity_and_release_failed_declarations() {
    let mut config = test_config("cursor-typed-capacity");
    config.max_cursors = 1;
    // One zero-column row needs four length bytes and two column-count bytes.
    config.cursor_bytes = 6;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for _ in 0..2 {
        session.success(
            &mut engine,
            "BEGIN; DECLARE fits CURSOR FOR SELECT FROM (VALUES(1)) source(id) GROUP BY id; FETCH ALL FROM fits; COMMIT",
            true,
        );
        let failed = session.execute(
            &mut engine,
            "BEGIN; DECLARE too_wide CURSOR FOR SELECT id FROM (VALUES(1)) source(id) GROUP BY id",
            true,
        );
        assert_eq!(error_sqlstates(&failed), ["54000"]);
        assert!(String::from_utf8_lossy(&failed).contains("cursor result exceeds cursor_bytes"));
        session.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn cursor_description_exhaustion_names_its_own_capacity_and_releases_the_slot() {
    let alias = "a".repeat(63);
    let projection = (0..20)
        .map(|_| format!("1 AS {alias}"))
        .collect::<Vec<_>>()
        .join(",");
    for bytes in [128, 4096] {
        let mut config = test_config("cursor-description-capacity");
        config.max_cursors = 1;
        config.cursor_bytes = bytes;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            &format!(
                "CREATE FUNCTION wide_cursor_labels() RETURNS void LANGUAGE plpgsql AS $$ \
                 DECLARE c refcursor; BEGIN OPEN c FOR SELECT {projection}; RETURN; END $$"
            ),
            true,
        );
        for statement in [
            format!("DECLARE wide_labels CURSOR FOR SELECT {projection}"),
            "SELECT wide_cursor_labels()".to_owned(),
        ] {
            session.success(&mut engine, "BEGIN", true);
            let failed = session.execute(&mut engine, &statement, true);
            assert_eq!(error_sqlstates(&failed), ["54000"]);
            let diagnostic = String::from_utf8_lossy(&failed);
            assert!(
                diagnostic.contains("cursor row description exceeds 1024 bytes"),
                "{diagnostic}"
            );
            assert!(!diagnostic.contains("cursor_bytes"), "{diagnostic}");
            session.success(&mut engine, "ROLLBACK", true);
            let output = session.success(
                &mut engine,
                "BEGIN; DECLARE reused CURSOR FOR SELECT 1; FETCH ALL FROM reused; COMMIT",
                true,
            );
            assert_eq!(data_rows(&output), ["1"]);
        }
    }
}

#[test]
fn cursor_binary_fetch_does_not_change_later_statement_formats() {
    let config = test_config("cursor-fetch-format-scope");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let output = session.success(
        &mut engine,
        "BEGIN; DECLARE binary_values BINARY CURSOR FOR SELECT 17; \
         FETCH NEXT FROM binary_values; SELECT 42; COMMIT",
        true,
    );
    let rows = data_rows(&output);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].as_bytes(), 17i32.to_be_bytes());
    assert_eq!(rows[1], "42");
}

#[test]
fn cursor_empty_select_lists_preserve_row_counts_and_required_target_lists() {
    let config = test_config("cursor-empty-select-lists");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    for (sql, count) in [
        ("SELECT", 1),
        ("SELECT ALL FROM generate_series(1,2)", 2),
        ("SELECT FROM generate_series(1,3) WHERE false", 0),
        ("SELECT FROM generate_series(1,3) LIMIT 1 OFFSET 1", 1),
        ("SELECT FROM (SELECT) AS empty_source", 1),
        ("SELECT UNION ALL SELECT", 2),
    ] {
        let output = session.success(&mut engine, sql, true);
        assert_eq!(data_rows(&output), vec![""; count], "{sql}");
    }
    session.success(
        &mut engine,
        "CREATE TABLE empty_returning_check(n integer)",
        true,
    );
    for sql in [
        "SELECT DISTINCT",
        "SELECT DISTINCT ON (1)",
        "INSERT INTO empty_returning_check VALUES (1) RETURNING",
    ] {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(error_sqlstates(&output), ["42601"], "{sql}");
    }
}

#[test]
fn cursor_motion_counts_match_postgresql_integer_grammar() {
    let config = test_config("cursor-motion-counts");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    for sql in
        include_str!("../../../tests/external/differential_exact/37_cursor_motion_counts.sql")
            .split(';')
            .filter(|sql| !sql.trim().is_empty())
    {
        let output = session.execute(&mut engine, sql, true);
        rows.extend(data_rows(&output));
        errors.extend(error_sqlstates(&output).into_iter().map(str::to_owned));
    }
    assert_eq!(rows, ["1", "2", "1", "2", "3", "3", "2", "1"]);
    assert_eq!(errors, ["42601"; 5]);
}

#[test]
fn function_source_cursors_generate_rows_as_fetch_reaches_them() {
    let config = test_config("function-source-cursor-generators");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE calls; BEGIN; \
         DECLARE lazy NO SCROLL CURSOR FOR SELECT nextval('calls') FROM generate_series(1,3)",
        true,
    );
    // PostgreSQL evaluates the projection per FETCH, not at DECLARE.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value,is_called FROM calls; FETCH 0 FROM lazy; \
             SELECT last_value,is_called FROM calls; FETCH NEXT FROM lazy; \
             SELECT last_value,is_called FROM calls; FETCH NEXT FROM lazy; CLOSE lazy; \
             SELECT last_value,is_called FROM calls; COMMIT",
            true
        )),
        ["1|f", "1|f", "1", "1|t", "2", "2|t"]
    );
    // A row-dependent error surfaces on the FETCH that reaches it.
    session.success(
        &mut engine,
        "BEGIN; DECLARE later_error NO SCROLL CURSOR FOR \
         SELECT 1/(g-2) FROM generate_series(1,3) AS g",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "FETCH NEXT FROM later_error", true)),
        ["-1"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM later_error", true)),
        ["22012"]
    );
    session.success(&mut engine, "ROLLBACK", true);
    // Each supported overload keeps only its next value: bounds of bigint,
    // numeric and timestamp steps, and array elements.
    for (source, expected) in [
        (
            "generate_series(9223372036854775806, 9223372036854775807)",
            &["9223372036854775806", "9223372036854775807"][..],
        ),
        ("generate_series(1.5, 2.5, 0.5)", &["1.5", "2.0", "2.5"][..]),
        (
            "generate_series(timestamp '2024-01-31', timestamp '2024-03-31', interval '1 month')",
            &[
                "2024-01-31 00:00:00",
                "2024-02-29 00:00:00",
                "2024-03-29 00:00:00",
            ][..],
        ),
        ("unnest(ARRAY['a','b',NULL])", &["a", "b", "NULL"][..]),
        ("generate_series(3, 1)", &[][..]),
    ] {
        let rows = data_rows(&session.success(
            &mut engine,
            &format!(
                "BEGIN; DECLARE generated NO SCROLL CURSOR FOR SELECT * FROM {source} AS v; \
                 FETCH ALL FROM generated; CLOSE generated; COMMIT"
            ),
            true,
        ));
        assert_eq!(rows, expected, "{source}");
    }
    // A zero step is reported by the first fetch, as PostgreSQL starts the
    // function scan there.
    session.success(
        &mut engine,
        "BEGIN; DECLARE zero NO SCROLL CURSOR FOR SELECT * FROM generate_series(1, 3, 0)",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH NEXT FROM zero", true)),
        ["22023"]
    );
    session.success(&mut engine, "ROLLBACK", true);
    // Function scans read backward, so a default cursor is scrollable.
    session.success(
        &mut engine,
        "BEGIN; DECLARE automatic CURSOR FOR SELECT * FROM generate_series(1,2)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT is_scrollable FROM pg_cursors WHERE name='automatic'; CLOSE automatic; COMMIT",
            true
        )),
        ["t"]
    );
    // The materialized path shares the series arithmetic, including at the
    // bigint bound where the next value would overflow.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*) FROM generate_series(9223372036854775806, 9223372036854775807)",
            true
        )),
        ["2"]
    );
}

#[test]
fn scroll_cursor_revisits_evaluate_volatile_rows_again_in_postgresql_order() {
    let config = test_config("scroll-cursor-revisits");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE s; CREATE TABLE t(id integer); \
         INSERT INTO t SELECT g FROM generate_series(1,4) g",
        true,
    );
    // Expected values are PostgreSQL 18.6 output for the same commands.
    for (declare, steps) in [
        (
            "DECLARE c SCROLL CURSOR FOR SELECT id, nextval('s') FROM t",
            &[
                ("FETCH NEXT FROM c", &["1|1"][..]),
                ("FETCH NEXT FROM c", &["2|2"][..]),
                ("FETCH PRIOR FROM c", &["1|3"][..]),
                ("FETCH NEXT FROM c", &["2|4"][..]),
                ("FETCH ABSOLUTE 1 FROM c", &["1|5"][..]),
                ("FETCH RELATIVE 0 FROM c", &["1|6"][..]),
                ("MOVE FORWARD 2 IN c; SELECT last_value FROM s", &["8"][..]),
                ("FETCH LAST FROM c", &["4|10"][..]),
                (
                    "MOVE BACKWARD ALL IN c; SELECT last_value FROM s",
                    &["10"][..],
                ),
            ][..],
        ),
        (
            // A volatile filter runs again for every revisited input.
            "DECLARE c SCROLL CURSOR FOR SELECT id, nextval('s') FROM t \
             WHERE id % 2 = 0 OR nextval('s') < 0",
            &[
                ("FETCH NEXT FROM c", &["2|12"][..]),
                ("FETCH NEXT FROM c", &["4|14"][..]),
                ("FETCH PRIOR FROM c", &["2|16"][..]),
            ][..],
        ),
        (
            // LIMIT returns its edge row again without evaluating it.
            "DECLARE c SCROLL CURSOR FOR SELECT id, nextval('s') FROM t LIMIT 2 OFFSET 1",
            &[
                ("FETCH ALL FROM c", &["2|18", "3|19"][..]),
                ("FETCH PRIOR FROM c", &["3|19"][..]),
                ("FETCH PRIOR FROM c", &["2|20"][..]),
                ("FETCH PRIOR FROM c", &[][..]),
                ("FETCH NEXT FROM c", &["2|20"][..]),
            ][..],
        ),
        (
            "DECLARE c SCROLL CURSOR FOR SELECT g, nextval('s') FROM generate_series(1,3) g",
            &[
                ("FETCH NEXT FROM c", &["1|21"][..]),
                ("FETCH NEXT FROM c", &["2|22"][..]),
                ("FETCH BACKWARD 2 FROM c", &["1|23"][..]),
                ("FETCH FORWARD 0 FROM c", &[][..]),
                ("FETCH ABSOLUTE -1 FROM c", &["3|27"][..]),
            ][..],
        ),
        (
            // A materialized FROM-less row is returned as saved.
            "DECLARE c SCROLL CURSOR FOR SELECT nextval('s')",
            &[
                ("FETCH NEXT FROM c", &["28"][..]),
                ("FETCH PRIOR FROM c", &[][..]),
                ("FETCH NEXT FROM c", &["28"][..]),
            ][..],
        ),
    ] {
        session.success(&mut engine, &format!("BEGIN; {declare}"), true);
        for (sql, expected) in steps {
            assert_eq!(
                data_rows(&session.success(&mut engine, sql, true)),
                *expected,
                "{declare}: {sql}"
            );
        }
        session.success(&mut engine, "CLOSE c; COMMIT", true);
    }
    // Saving a held scrollable cursor rewinds and evaluates every row again;
    // its position is kept.
    session.success(
        &mut engine,
        "SELECT setval('s', 1, false); BEGIN; \
         DECLARE held SCROLL CURSOR WITH HOLD FOR SELECT id, nextval('s') FROM t; \
         FETCH NEXT FROM held; FETCH NEXT FROM held; COMMIT",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT last_value FROM s; FETCH PRIOR FROM held; FETCH ALL FROM held; \
             SELECT is_holdable, is_scrollable FROM pg_cursors WHERE name = 'held'; \
             CLOSE held",
            true
        )),
        ["6", "1|3", "2|4", "3|5", "4|6", "t|t"]
    );
    // FETCH BACKWARD 0 off a row still asks a NO SCROLL cursor to go back.
    session.success(
        &mut engine,
        "BEGIN; DECLARE forward NO SCROLL CURSOR FOR SELECT id FROM t",
        true,
    );
    assert_eq!(
        command_tags(&session.success(
            &mut engine,
            "FETCH FORWARD 0 FROM forward; MOVE BACKWARD 0 IN forward",
            true
        )),
        ["FETCH 0", "MOVE 0"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "FETCH BACKWARD 0 FROM forward", true)),
        ["55000"]
    );
    session.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn scroll_cursor_saved_inputs_name_cursor_bytes_and_fail_the_portal() {
    let mut config = test_config("scroll-cursor-input-capacity");
    config.cursor_bytes = 256;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE s; CREATE TABLE wide(id integer, payload text); \
         INSERT INTO wide SELECT g, repeat('x', 40) FROM generate_series(1,20) g; \
         BEGIN; DECLARE c SCROLL CURSOR FOR SELECT id, nextval('s') FROM wide; \
         FETCH NEXT FROM c",
        true,
    );
    let response = session.execute(&mut engine, "FETCH ALL FROM c", true);
    assert_eq!(error_sqlstates(&response), ["54000"]);
    assert!(
        String::from_utf8_lossy(&response).contains("cursor_bytes"),
        "{}",
        String::from_utf8_lossy(&response)
    );
    session.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn locking_cursors_lock_each_row_when_fetched_and_recheck_changed_rows() {
    // Locking mode protects a row-locking reader's whole table, so only MVCC
    // shows PostgreSQL's per-row conflicts.
    {
        let mode = crate::config::TransactionMode::Mvcc;
        let mut config = test_config("locking-cursor-fetch");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut owner = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut other =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        owner.success(
            &mut engine,
            "CREATE SEQUENCE s; CREATE TABLE lt(id integer PRIMARY KEY, v integer); \
             INSERT INTO lt VALUES (3,30),(1,10),(2,20),(4,40),(5,50)",
            true,
        );
        owner.success(
            &mut engine,
            "BEGIN; DECLARE c CURSOR FOR SELECT id, v, nextval('s') FROM lt \
             WHERE v < 45 ORDER BY id FOR UPDATE",
            true,
        );
        // DECLARE neither locks nor projects.
        assert_eq!(
            data_rows(&other.success(
                &mut engine,
                "BEGIN; SELECT id FROM lt WHERE id = 1 FOR UPDATE NOWAIT; ROLLBACK",
                true
            )),
            ["1"]
        );
        assert_eq!(
            data_rows(&owner.success(
                &mut engine,
                "FETCH 1 FROM c; SELECT is_scrollable FROM pg_cursors",
                true
            )),
            ["1|10|1", "f"]
        );
        assert_eq!(
            error_sqlstates(&other.execute(
                &mut engine,
                "SELECT id FROM lt WHERE id = 1 FOR UPDATE NOWAIT",
                true
            )),
            ["55P03"]
        );
        // Changes committed after DECLARE: an update is rechecked and
        // projected again, a row that no longer qualifies or is deleted is
        // skipped.
        other.success(
            &mut engine,
            "UPDATE lt SET v = v + 1 WHERE id = 2; UPDATE lt SET v = 100 WHERE id = 3; \
             DELETE FROM lt WHERE id = 4",
            true,
        );
        assert_eq!(
            data_rows(&owner.success(&mut engine, "FETCH ALL FROM c", true)),
            ["2|21|3"]
        );
        owner.success(&mut engine, "ROLLBACK", true);
        // A FETCH waits for a conflicting row lock and then sees the newest
        // version; earlier fetched rows stay fetched.
        other.success(
            &mut engine,
            "BEGIN; UPDATE lt SET v = 11 WHERE id = 1",
            true,
        );
        owner.success(
            &mut engine,
            "BEGIN; DECLARE w CURSOR FOR SELECT id, v FROM lt ORDER BY id DESC FOR SHARE",
            true,
        );
        assert_eq!(
            data_rows(&owner.success(&mut engine, "FETCH 2 FROM w", true)),
            ["5|50", "3|100"]
        );
        let status = owner.execute_status(&mut engine, "FETCH ALL FROM w", true);
        assert!(
            matches!(
                status,
                ExecutionStatus::Blocked {
                    wait: StatementWait::Lock,
                    ..
                }
            ),
            "{mode:?}: {status:?}"
        );
        other.success(&mut engine, "COMMIT", true);
        assert_eq!(
            data_rows(&owner.success(&mut engine, "FETCH ALL FROM w", true)),
            ["2|21", "1|11"]
        );
        owner.success(&mut engine, "ROLLBACK", true);
        // Rows changed by the cursor's own transaction after DECLARE are
        // skipped; OFFSET rows are locked too.
        owner.success(
            &mut engine,
            "BEGIN; DECLARE own CURSOR FOR SELECT id FROM lt ORDER BY id LIMIT 2 OFFSET 1 \
             FOR UPDATE; UPDATE lt SET v = 0 WHERE id = 2",
            true,
        );
        assert_eq!(
            data_rows(&owner.success(&mut engine, "FETCH ALL FROM own", true)),
            ["3", "5"]
        );
        assert_eq!(
            error_sqlstates(&other.execute(
                &mut engine,
                "SELECT id FROM lt WHERE id = 1 FOR UPDATE NOWAIT",
                true
            )),
            ["55P03"]
        );
        owner.success(&mut engine, "ROLLBACK", true);
        drop((owner, other, engine));
        std::fs::remove_dir_all(config.data_dir).unwrap();
    }
}

#[test]
fn locking_cursors_reject_repeatable_read_changes_and_bound_sorted_rows() {
    let mut config = test_config("locking-cursor-repeatable");
    config.cursor_bytes = 512;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut owner = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut other = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    owner.success(
        &mut engine,
        "CREATE TABLE lt(id integer PRIMARY KEY, v integer); \
         INSERT INTO lt VALUES (1,10),(2,20),(3,30)",
        true,
    );
    for (change, state) in [
        ("UPDATE lt SET v = 0 WHERE id = 2", "40001"),
        ("DELETE FROM lt WHERE id = 3", "40001"),
    ] {
        owner.success(
            &mut engine,
            "BEGIN ISOLATION LEVEL REPEATABLE READ; \
             DECLARE c CURSOR FOR SELECT id FROM lt ORDER BY id DESC FOR UPDATE",
            true,
        );
        other.success(&mut engine, change, true);
        assert_eq!(
            error_sqlstates(&owner.execute(&mut engine, "FETCH ALL FROM c", true)),
            [state],
            "{change}"
        );
        owner.success(&mut engine, "ROLLBACK", true);
    }
    // Sorting saves every qualifying row in cursor_bytes before the first
    // row returns; exhaustion names the setting.
    owner.success(
        &mut engine,
        "INSERT INTO lt SELECT g, g FROM generate_series(10, 200) g; \
         BEGIN; DECLARE big CURSOR FOR SELECT id FROM lt ORDER BY v FOR UPDATE",
        true,
    );
    let response = owner.execute(&mut engine, "FETCH 1 FROM big", true);
    assert_eq!(error_sqlstates(&response), ["54000"]);
    assert!(String::from_utf8_lossy(&response).contains("cursor_bytes"));
    owner.success(&mut engine, "ROLLBACK", true);
    // No lock was taken by the failed sort.
    assert_eq!(
        data_rows(&other.success(
            &mut engine,
            "SELECT id FROM lt WHERE id = 10 FOR UPDATE NOWAIT",
            true
        )),
        ["10"]
    );
}

#[test]
fn ordered_cursors_sort_at_the_first_fetch_and_project_each_row_when_fetched() {
    let config = test_config("ordered-cursor-projection");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE SEQUENCE sorted_calls; CREATE TABLE sorted_rows(id int, g int, v text); \
         INSERT INTO sorted_rows VALUES (1,2,'a'),(2,1,'b'),(3,2,'c'),(4,1,'d'),(5,3,'e')",
        true,
    );
    // Expected rows come from PostgreSQL 18.6: the output list runs after
    // the sort, one row per fetch, and equal keys keep scan order.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DECLARE ordered NO SCROLL CURSOR FOR \
             SELECT v, nextval('sorted_calls') FROM sorted_rows ORDER BY g, id DESC; \
             SELECT last_value, is_called FROM sorted_calls; FETCH 2 ordered; \
             SELECT last_value, is_called FROM sorted_calls; FETCH ALL ordered; COMMIT",
            true
        )),
        ["1|f", "d|1", "b|2", "2|t", "c|3", "a|4", "e|5"]
    );
    // A scrollable sorted cursor evaluates a revisited row's output again;
    // its filter ran once, before the sort.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT setval('sorted_calls', 100, false); BEGIN; \
             DECLARE scrolled SCROLL CURSOR FOR \
             SELECT v, nextval('sorted_calls') FROM sorted_rows WHERE id > 1 ORDER BY id; \
             FETCH NEXT scrolled; FETCH NEXT scrolled; FETCH PRIOR scrolled; \
             FETCH LAST scrolled; FETCH ABSOLUTE 1 scrolled; COMMIT",
            true
        )),
        ["100", "b|100", "c|101", "b|102", "e|106", "b|107"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DECLARE by_group CURSOR FOR SELECT v FROM sorted_rows ORDER BY g; \
             FETCH ALL by_group; FETCH BACKWARD 2 by_group; \
             DECLARE filtered SCROLL CURSOR FOR \
             SELECT v FROM sorted_rows WHERE random() < 2 ORDER BY id DESC; \
             FETCH 2 filtered; FETCH PRIOR filtered; \
             SELECT is_scrollable FROM pg_cursors WHERE name = 'filtered'; COMMIT",
            true
        )),
        ["b", "d", "a", "c", "e", "e", "c", "e", "d", "e", "t"]
    );
    // LIMIT and OFFSET apply after the sort; a held cursor saves its sorted
    // rows at commit.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DECLARE windowed NO SCROLL CURSOR FOR              SELECT v FROM sorted_rows ORDER BY id DESC LIMIT 2 OFFSET 1; FETCH ALL windowed;              DECLARE scroll_window SCROLL CURSOR FOR              SELECT v, nextval('sorted_calls') FROM sorted_rows ORDER BY id LIMIT 2 OFFSET 1;              FETCH ALL scroll_window; FETCH PRIOR scroll_window; COMMIT;              DECLARE held CURSOR WITH HOLD FOR SELECT v FROM sorted_rows ORDER BY g DESC, id;              FETCH 2 held; FETCH ALL held; CLOSE held",
            true
        )),
        [
            "d", "c", "b|109", "c|110", "c|110", "e", "a", "c", "b", "d"
        ]
    );
}

#[test]
fn generate_subscripts_cursors_produce_rows_as_they_are_fetched() {
    let config = test_config("subscript-cursor-generator");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    // Expected rows come from PostgreSQL 18.6.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE SEQUENCE subscript_calls START 50; BEGIN; \
             DECLARE subs NO SCROLL CURSOR FOR SELECT s, nextval('subscript_calls') \
             FROM generate_subscripts('[3:5]={a,b,c}'::text[], 1, true) s; \
             SELECT last_value, is_called FROM subscript_calls; FETCH 2 subs; FETCH ALL subs; \
             DECLARE missing CURSOR FOR SELECT * FROM generate_subscripts(ARRAY[1,2], 2); \
             FETCH ALL missing; \
             DECLARE forward SCROLL CURSOR FOR \
             SELECT s FROM generate_subscripts(ARRAY[[1,2],[3,4]], 2) s; \
             FETCH ALL forward; FETCH PRIOR forward; \
             SELECT is_scrollable FROM pg_cursors WHERE name = 'forward'; COMMIT",
            true
        )),
        ["50|f", "5|50", "4|51", "3|52", "1", "2", "2", "t"]
    );
}

#[test]
fn regex_json_and_multi_array_cursors_produce_rows_as_they_are_fetched() {
    let config = test_config("multi-value-cursor-generator");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    // Expected rows come from PostgreSQL 18.6: each nextval runs when its row
    // is fetched, after the sequence was read untouched.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE SEQUENCE gen_calls START 70; BEGIN; \
             DECLARE u NO SCROLL CURSOR FOR SELECT a, b, nextval('gen_calls') \
             FROM unnest(ARRAY[1,2,3], ARRAY['x','y']) AS t(a, b); \
             SELECT last_value, is_called FROM gen_calls; FETCH 2 u; FETCH ALL u; \
             DECLARE m NO SCROLL CURSOR FOR SELECT m, nextval('gen_calls') \
             FROM regexp_matches('a1b22c333', '([a-z])(\\d+)', 'g') m; \
             FETCH 1 m; FETCH ALL m; \
             DECLARE sp CURSOR FOR SELECT * FROM regexp_split_to_table('one, two,,three', ',\\s*'); \
             FETCH ALL sp; FETCH PRIOR sp; \
             DECLARE je NO SCROLL CURSOR FOR SELECT key, value, nextval('gen_calls') \
             FROM json_each('{\"b\": [1, 2], \"a\": \"x\", \"b\": null}'); \
             FETCH ALL je; CLOSE u; CLOSE m; CLOSE sp; CLOSE je; \
             DECLARE jbe CURSOR FOR SELECT * FROM jsonb_each_text('{\"b\": [1,  2], \"a\": \"x\\ty\", \"c\": null, \"b\": 3}'); \
             FETCH ALL jbe; \
             DECLARE ae CURSOR FOR SELECT * FROM json_array_elements_text('[1, \"two\", null, {\"k\" :  1}]'); \
             FETCH ALL ae; \
             DECLARE ab CURSOR FOR SELECT * FROM jsonb_array_elements('[1.50, \"two\", null, {\"k\" :  1}]'); \
             FETCH ALL ab; CLOSE jbe; CLOSE ae; \
             DECLARE k CURSOR FOR SELECT * FROM jsonb_object_keys('{\"bb\": 1, \"a\": 2, \"bb\": 3}'); \
             FETCH ALL k; \
             DECLARE k2 CURSOR FOR SELECT * FROM json_object_keys('{\"bb\": 1, \"a\": 2, \"bb\": 3}'); \
             FETCH ALL k2; COMMIT",
            true
        )),
        [
            "70|f",
            "1|x|70",
            "2|y|71",
            "3|NULL|72",
            "{a,1}|73",
            "{b,22}|74",
            "{c,333}|75",
            "one",
            "two",
            "",
            "three",
            "three",
            "b|[1, 2]|76",
            "a|\"x\"|77",
            "b|null|78",
            "a|x\ty",
            "b|3",
            "c|NULL",
            "1",
            "two",
            "NULL",
            "{\"k\" :  1}",
            "1.50",
            "\"two\"",
            "null",
            "{\"k\": 1}",
            "a",
            "bb",
            "bb",
            "a",
            "bb",
        ]
    );
}

#[test]
fn rows_from_and_ordinality_cursors_advance_their_calls_in_lockstep() {
    let config = test_config("rows-from-cursor-generator");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    // Expected rows come from PostgreSQL 18.6.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE SEQUENCE ord_calls START 5; BEGIN; \
             DECLARE r NO SCROLL CURSOR FOR SELECT *, nextval('ord_calls') FROM ROWS FROM \
             (json_each('{\"a\":1,\"b\":2}'), regexp_split_to_table('x-y-z', '-'), \
             generate_series(1, 2)) WITH ORDINALITY AS t(k, v, piece, n, ord); \
             SELECT last_value, is_called FROM ord_calls; FETCH 1 r; FETCH ALL r; \
             DECLARE o SCROLL CURSOR FOR \
             SELECT * FROM jsonb_array_elements_text('[\"p\", \"q\"]') WITH ORDINALITY; \
             FETCH ALL o; FETCH PRIOR o; FETCH FIRST o; COMMIT",
            true
        )),
        [
            "5|f",
            "a|1|x|1|1|5",
            "b|2|y|2|2|6",
            "NULL|NULL|z|NULL|3|7",
            "p|1",
            "q|2",
            "q|2",
            "p|1",
        ]
    );
    // Argument errors surface at the first FETCH, before any row.
    for (query, code) in [
        ("json_each('[1]')", sqlstate::INVALID_PARAMETER_VALUE),
        (
            "regexp_matches('abc', '(', 'g')",
            sqlstate::INVALID_REGULAR_EXPRESSION,
        ),
        (
            "regexp_split_to_table('abc', 'b', 'g')",
            sqlstate::INVALID_PARAMETER_VALUE,
        ),
        (
            "jsonb_object_keys('[1]')",
            sqlstate::INVALID_PARAMETER_VALUE,
        ),
        (
            "json_array_elements('{\"a\":1}')",
            sqlstate::INVALID_PARAMETER_VALUE,
        ),
    ] {
        session.success(
            &mut engine,
            &format!("BEGIN; DECLARE failing CURSOR FOR SELECT * FROM {query}"),
            true,
        );
        let reply = session.execute(&mut engine, "FETCH 1 failing", true);
        assert_eq!(error_sqlstates(&reply), [code], "{query}");
        session.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn scroll_cursors_keep_one_fetch_of_rows_and_reread_their_input_to_move_back() {
    // Every visited row of these cursors exceeds cursor_bytes; each FETCH's
    // own rows fit it. Expected rows come from PostgreSQL 18.6.
    let mut config = test_config("windowed-scroll-cursor");
    config.cursor_bytes = 1024;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE TABLE win(id integer, payload text); INSERT INTO win SELECT g, \
             repeat('x', 40) FROM generate_series(1,400) g; BEGIN; DECLARE c CURSOR FOR \
             SELECT id, length(payload) FROM win WHERE id % 2 = 0; FETCH 3 FROM c; FETCH 3 \
             FROM c; FETCH PRIOR FROM c; FETCH BACKWARD 2 FROM c; MOVE 150 FROM c; FETCH \
             RELATIVE -3 FROM c; FETCH ABSOLUTE -1 FROM c; FETCH ABSOLUTE 2 FROM c; FETCH \
             LAST FROM c; FETCH NEXT FROM c; FETCH PRIOR FROM c; FETCH FIRST FROM c; FETCH \
             0 FROM c; MOVE ABSOLUTE 120 FROM c; FETCH BACKWARD 3 FROM c; FETCH 2 FROM c; \
             SELECT is_scrollable FROM pg_cursors WHERE name = 'c'; DECLARE g CURSOR FOR \
             SELECT * FROM generate_series(1, 400) WITH ORDINALITY; MOVE 300 FROM g; FETCH \
             BACKWARD 2 FROM g; FETCH ABSOLUTE 5 FROM g; FETCH RELATIVE 390 FROM g; DECLARE \
             h CURSOR WITH HOLD FOR SELECT id FROM win WHERE id > 390; MOVE 7 FROM h; \
             COMMIT; FETCH PRIOR FROM h; FETCH FIRST FROM h; FETCH ALL FROM h; CLOSE h;",
            true
        )),
        [
            "2|40", "4|40", "6|40", "8|40", "10|40", "12|40", "10|40", "8|40", "6|40", "300|40",
            "400|40", "4|40", "400|40", "400|40", "2|40", "2|40", "238|40", "236|40", "234|40",
            "236|40", "238|40", "t", "299|299", "298|298", "5|5", "395|395", "396", "391", "392",
            "393", "394", "395", "396", "397", "398", "399", "400",
        ]
    );
}

#[test]
fn join_cursors_evaluate_rows_per_fetch_and_materialize_scrolled_output() {
    use crate::sql::cursor::FetchMotion;
    let config = test_config("cursor-retained-joins");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    // PostgreSQL 18.6 returns the rows below for these statements.
    session.success(
        &mut engine,
        "CREATE TABLE ja(id int, v text); INSERT INTO ja VALUES (1,'a'),(2,'b'),(3,'c'); \
         CREATE TABLE jb(k int, w text); INSERT INTO jb VALUES (1,'x'),(3,'y'); \
         CREATE SEQUENCE js; BEGIN; \
         DECLARE c CURSOR FOR SELECT nextval('js'), ja.id, jb.w FROM ja JOIN jb \
         ON jb.k = ja.id + 0",
        true,
    );
    // The join is read as the cursor fetches, not at DECLARE.
    let (slot, _) = session
        .cursors
        .read_request("c", FetchMotion::Count(1), false)
        .unwrap()
        .expect("retained join cursor");
    assert_eq!(session.cursors.produced_rows(slot), 0);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT is_scrollable FROM pg_cursors WHERE name='c'; \
             SELECT last_value, is_called FROM js; FETCH 1 FROM c; \
             SELECT last_value FROM js; FETCH ALL FROM c; CLOSE c",
            true
        )),
        ["f", "1|f", "1|1|x", "1", "2|3|y"]
    );
    // A scrollable join returns its saved output on revisit, as
    // PostgreSQL's Material node does: nextval is not called again.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "DECLARE s SCROLL CURSOR FOR SELECT nextval('js'), ja.id, jb.w FROM ja \
             LEFT JOIN jb ON jb.k = ja.id + 0; \
             SELECT is_scrollable FROM pg_cursors WHERE name='s'; FETCH 2 FROM s; \
             FETCH PRIOR FROM s; FETCH ALL FROM s; FETCH FIRST FROM s; \
             SELECT last_value FROM js; CLOSE s",
            true
        )),
        [
            "t", "3|1|x", "4|2|NULL", "3|1|x", "4|2|NULL", "5|3|y", "3|1|x", "5"
        ]
    );
    // Without volatile output a scrollable join rereads its input to move
    // back.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "DECLARE p SCROLL CURSOR FOR SELECT ja.id, jb.w FROM ja LEFT JOIN jb \
             ON jb.k = ja.id + 0 WHERE ja.id > 1; \
             FETCH ALL FROM p; FETCH ABSOLUTE 1 FROM p; FETCH RELATIVE 1 FROM p; \
             FETCH BACKWARD 1 FROM p; CLOSE p; COMMIT",
            true
        )),
        ["2|NULL", "3|y", "2|NULL", "3|y", "2|NULL"]
    );
    // A column equality is a hash join, as in the general executor: its
    // build table stays while the scrollable cursor rereads its probe side.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DECLARE h CURSOR FOR SELECT ja.id, jb.w FROM ja LEFT JOIN jb \
             ON jb.k = ja.id; SELECT is_scrollable FROM pg_cursors WHERE name='h'; \
             FETCH 2 FROM h; FETCH ALL FROM h; \
             DECLARE h2 SCROLL CURSOR FOR SELECT ja.id, jb.w FROM ja JOIN jb \
             ON jb.k = ja.id; FETCH ALL FROM h2; FETCH FIRST FROM h2; \
             FETCH NEXT FROM h2; FETCH PRIOR FROM h2; COMMIT",
            true
        )),
        [
            "f", "1|x", "2|NULL", "3|y", "1|x", "3|y", "1|x", "3|y", "1|x"
        ]
    );
    // A function source on the nullable side of a RIGHT or FULL join is
    // read once, not per left-hand row.
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT * FROM (VALUES (1)) v RIGHT JOIN generate_series(1, 3) g \
             ON v.column1 = g; \
             SELECT * FROM (VALUES (1), (2)) v(x) FULL JOIN generate_series(2, 3) g \
             ON x = g ORDER BY 1, 2",
            true
        )),
        ["1|1", "NULL|2", "NULL|3", "1|NULL", "2|2", "NULL|3"]
    );
}
