use super::*;

#[test]
fn catalog_constraint_policy_nodes_bind_views_and_outer_columns() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE TABLE policy_nodes(id integer);
         CREATE VIEW policy_nodes_view AS SELECT id FROM policy_nodes;
         CREATE POLICY policy_nodes_rule ON policy_nodes USING
           (EXISTS (SELECT 1 FROM policy_nodes_view v WHERE v.id=policy_nodes.id));
         SELECT pg_get_expr(polqual,polrelid) IS NOT NULL
           FROM pg_policy WHERE polname='policy_nodes_rule'",
    );
    assert!(
        !message_types(&setup).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    assert_eq!(data_rows(&setup), ["t"]);
    let blocked = run_with(&mut engine, &mut budget, "DROP VIEW policy_nodes_view");
    assert_eq!(error_sqlstates(&blocked), ["2BP01"]);
    let removed = run_with(
        &mut engine,
        &mut budget,
        "DROP VIEW policy_nodes_view CASCADE;
         SELECT count(*) FROM pg_policy WHERE polname='policy_nodes_rule'",
    );
    assert_eq!(data_rows(&removed), ["0"]);
}

#[test]
fn catalog_constraint_attributes_use_postgresql_physical_type_properties() {
    let (mut engine, mut budget) = test_engine();
    run_with(
        &mut engine,
        &mut budget,
        "CREATE DOMAIN layout_domain AS bigint;
         CREATE TYPE layout_enum AS ENUM ('a');
         CREATE TYPE layout_record AS (a bigint);
         CREATE TABLE layout_columns (
           a boolean, b smallint, c bigint, d text, e bigint[], f smallint[],
           g layout_domain, h layout_domain[], i layout_enum,
           j layout_enum[], k layout_record, l layout_record[], m inet, n cidr, o point)",
    );
    let result = run_with(
        &mut engine,
        &mut budget,
        "SELECT attname,attlen,attbyval,attalign,attstorage FROM pg_attribute
           WHERE attrelid='layout_columns'::regclass AND attnum>0 ORDER BY attnum",
    );
    assert!(
        !message_types(&result).contains(&b'E'),
        "{}",
        String::from_utf8_lossy(&result)
    );
    assert_eq!(
        data_rows(&result),
        [
            "a|1|t|c|p",
            "b|2|t|s|p",
            "c|8|t|d|p",
            "d|-1|f|i|x",
            "e|-1|f|d|x",
            "f|-1|f|i|x",
            "g|8|t|d|p",
            "h|-1|f|d|x",
            "i|4|t|i|p",
            "j|-1|f|i|x",
            "k|-1|f|d|x",
            "l|-1|f|d|x",
            "m|-1|f|i|m",
            "n|-1|f|i|m",
            "o|16|f|d|p",
        ]
    );
}

const SETUP: &str = "
    CREATE TABLE constraint_parent(a integer, b integer,
        CONSTRAINT constraint_parent_key PRIMARY KEY(a,b));
    CREATE TABLE constraint_child(x integer, y integer,
        CONSTRAINT constraint_child_key UNIQUE NULLS NOT DISTINCT(x,y),
        CONSTRAINT constraint_child_parent FOREIGN KEY(x,y) REFERENCES constraint_parent(b,a));
";

#[test]
fn catalog_constraint_nodes_copy_as_nodes_and_reject_text_input() {
    let config = test_config("constraint-node-input");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine,
        "CREATE TABLE node_source(a integer CONSTRAINT node_positive CHECK(a>0));
         CREATE TABLE saved_nodes(expression pg_node_tree, relation oid);
         INSERT INTO saved_nodes SELECT conbin,conrelid FROM pg_constraint WHERE conname='node_positive'", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT pg_typeof(expression),pg_get_expr(expression::pg_node_tree,relation,false) FROM saved_nodes", true)),
        ["pg_node_tree|(a > 0)"]);
    for command in [
        "INSERT INTO saved_nodes VALUES('a > 0',0)",
        "INSERT INTO saved_nodes VALUES('pos3ql-catalog-expression-v1
expression
0

NULL',0)",
        "UPDATE saved_nodes SET expression='a > 0'",
        "SELECT 'a > 0'::pg_node_tree",
        "SELECT expression::text::pg_node_tree FROM saved_nodes",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, command, true)),
            ["0A000"],
            "{command}"
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER TABLE node_source RENAME COLUMN a TO amount;
         SELECT pg_get_expr(expression,relation,false) FROM saved_nodes",
            true
        )),
        ["(amount > 0)"]
    );
    let arena = Arena::new(&mut budget, "node parameter input", 1 << 20).unwrap();
    for bytes in [
        b"not a node".as_slice(),
        crate::sql::catalog_node::CatalogNode::NULL
            .as_str()
            .as_bytes(),
    ] {
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                engine
                    .decode_binary_parameter(crate::sql::types::oid::PG_NODE_TREE, bytes, &arena, 0)
                    .unwrap_err()
                    .sqlstate,
                "0A000"
            );
            assert_eq!(
                engine
                    .decode_text_parameter(crate::sql::types::oid::PG_NODE_TREE, bytes, &arena, 0)
                    .unwrap_err()
                    .sqlstate,
                "0A000"
            );
        });
    }
    session.success(
        &mut engine,
        "CREATE FUNCTION node_query() RETURNS integer LANGUAGE SQL BEGIN ATOMIC SELECT 1; END",
        true,
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT pg_get_expr(prosqlbody,0) FROM pg_proc WHERE proname='node_query'",
            true
        )),
        ["22023"]
    );
}

#[test]
fn catalog_constraint_internal_statistics_types_reject_client_values() {
    let config = test_config("catalog-statistics-input");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE opaque_statistics(a pg_ndistinct,b pg_dependencies,c pg_mcv_list);
         INSERT INTO opaque_statistics VALUES(NULL,NULL,NULL)",
        true,
    );
    for (name, oid) in [
        ("pg_ndistinct", crate::sql::types::oid::PG_NDISTINCT),
        ("pg_dependencies", crate::sql::types::oid::PG_DEPENDENCIES),
        ("pg_mcv_list", crate::sql::types::oid::PG_MCV_LIST),
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("SELECT 'invalid'::{name}"),
                true
            )),
            ["0A000"]
        );
        let arena = Arena::new(&mut budget, "statistics input", 1 << 20).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(
                engine
                    .decode_text_parameter(oid, b"invalid", &arena, 0)
                    .unwrap_err()
                    .sqlstate,
                "0A000"
            );
            assert_eq!(
                engine
                    .decode_binary_parameter(oid, b"invalid", &arena, 0)
                    .unwrap_err()
                    .sqlstate,
                "0A000"
            );
        });
    }
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO opaque_statistics(a) VALUES('invalid')",
            true
        )),
        ["0A000"]
    );
    session.success(&mut engine,
        "CREATE TABLE statistics_source(a integer,b integer);
         INSERT INTO statistics_source VALUES(1,1),(1,1),(2,2);
         CREATE STATISTICS generated_statistics(ndistinct,dependencies) ON a,b FROM statistics_source;
         ANALYZE statistics_source", true);
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO opaque_statistics(a) SELECT stxdndistinct FROM pg_statistic_ext_data
          WHERE stxoid=(SELECT oid FROM pg_statistic_ext WHERE stxname='generated_statistics')",
            true
        )),
        ["0A000"]
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT count(*) FROM opaque_statistics", true)),
        ["1"]
    );
}

#[test]
fn catalog_constraint_default_identifiers_do_not_overlap_on_wide_tables() {
    let config = test_config("constraint-default-identifiers");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    let columns = (0..101)
        .map(|column| format!("c{column} integer DEFAULT 1"))
        .collect::<Vec<_>>()
        .join(",");
    session.success(&mut engine, &format!("CREATE TABLE wide_defaults({columns}); CREATE TABLE following_defaults(a integer DEFAULT 1)"), true);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*),count(DISTINCT oid) FROM pg_attrdef
          WHERE adrelid IN ('wide_defaults'::regclass,'following_defaults'::regclass)",
            true
        )),
        ["102|102"]
    );
}

#[test]
fn catalog_constraint_dropped_columns_keep_numbers_and_retained_expression_bindings() {
    let mut config = test_config("constraint-attribute-numbers");
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "CREATE TABLE ordinal_columns(lost text,kept bigint NOT NULL,
            g bigint GENERATED ALWAYS AS(kept+1) STORED,
            CONSTRAINT ordinal_positive CHECK(kept>0), CONSTRAINT ordinal_key UNIQUE(kept));
         COMMENT ON COLUMN ordinal_columns.kept IS 'retained comment';
         COMMENT ON COLUMN ordinal_columns.lost IS 'removed comment';
         CREATE ROLE ordinal_reader; GRANT SELECT(kept) ON ordinal_columns TO ordinal_reader",
        true,
    );
    let report = "SELECT pg_get_expr(conbin,conrelid,false) FROM pg_constraint WHERE conname='ordinal_positive'";
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL REPEATABLE READ; {report}"),
            true
        )),
        ["(kept > 0)"]
    );
    writer.success(
        &mut engine,
        "ALTER TABLE ordinal_columns DROP COLUMN lost, ADD COLUMN fresh smallint",
        true,
    );
    assert_eq!(
        data_rows(&reader.success(&mut engine, report, true)),
        ["(kept > 0)"]
    );
    assert_eq!(data_rows(&writer.success(&mut engine,
        "SELECT attname,attnum,atttypid,attlen,attbyval,attalign,attisdropped FROM pg_attribute
           WHERE attrelid='ordinal_columns'::regclass AND attnum>0 ORDER BY attnum;
         SELECT conname,conkey FROM pg_constraint WHERE conname IN ('ordinal_positive','ordinal_key') ORDER BY conname;
         SELECT relnatts FROM pg_class WHERE oid='ordinal_columns'::regclass", true)),
        ["........pg.dropped.1........|1|0|-1|f|i|t", "kept|2|20|8|t|d|f",
         "g|3|20|8|t|d|f", "fresh|4|21|2|t|s|f", "ordinal_key|{2}", "ordinal_positive|{2}", "4"]);
    assert_eq!(
        data_rows(&writer.success(
            &mut engine,
            "SELECT has_column_privilege('ordinal_reader','ordinal_columns',2::smallint,'SELECT'),
                has_column_privilege('ordinal_reader','ordinal_columns',3::smallint,'SELECT'),
                col_description('ordinal_columns'::regclass,2);
         SELECT objsubid,description FROM pg_description WHERE objoid='ordinal_columns'::regclass;
         SELECT (pg_identify_object('pg_class'::regclass,'ordinal_columns'::regclass,2)).identity",
            true
        )),
        [
            "t|f|retained comment",
            "2|retained comment",
            "public.ordinal_columns.kept"
        ]
    );
    assert_eq!(
        data_rows(&writer.success(
            &mut engine,
            "COMMENT ON COLUMN ordinal_columns.kept IS 'updated comment';
         SELECT objsubid,description FROM pg_description WHERE objoid='ordinal_columns'::regclass",
            true
        )),
        ["2|updated comment"]
    );
    reader.success(&mut engine, "ROLLBACK", true);
}

const REPORT: &str = "
    SELECT table_name,constraint_name,constraint_type,nulls_distinct
      FROM information_schema.table_constraints
      WHERE constraint_name IN ('constraint_parent_key','constraint_child_key','constraint_child_parent')
      ORDER BY constraint_name;
    SELECT table_name,constraint_name FROM information_schema.constraint_table_usage
      WHERE constraint_name='constraint_child_parent';
    SELECT tablename,indexname FROM pg_indexes WHERE indexname='constraint_parent_key';
";

#[test]
fn catalog_constraint_key_reports_preserve_order_duplicates_and_selected_columns() {
    for (isolation, retained) in [("READ COMMITTED", false), ("REPEATABLE READ", true)] {
        let mut config = test_config("constraint-key-report-rows");
        config.transaction_mode = crate::config::TransactionMode::Mvcc;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        writer.success(&mut engine,
            "ALTER TABLE constraint_parent ADD CONSTRAINT parent_positive CHECK(b>0);
             CREATE TABLE another_child(x integer,y integer,
                 CONSTRAINT constraint_child_parent FOREIGN KEY(x,y) REFERENCES constraint_parent(b,a))", true);
        let report = "
            SELECT table_name,column_name,ordinal_position,position_in_unique_constraint
              FROM information_schema.key_column_usage WHERE constraint_name='constraint_child_parent'
              ORDER BY table_name,ordinal_position;
            SELECT table_name,column_name,count(*) FROM information_schema.constraint_column_usage
              WHERE constraint_name='constraint_child_parent' GROUP BY table_name,column_name ORDER BY column_name;
            SELECT table_name,column_name FROM information_schema.constraint_column_usage WHERE constraint_name='parent_positive';
            SELECT constraint_name,unique_constraint_name FROM information_schema.referential_constraints
              WHERE constraint_name='constraint_child_parent' ORDER BY constraint_name;
        ";
        let old = data_rows(&reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL {isolation}; {report}"),
            true,
        ));
        assert_eq!(
            old,
            [
                "another_child|x|1|2",
                "another_child|y|2|1",
                "constraint_child|x|1|2",
                "constraint_child|y|2|1",
                "constraint_parent|a|2",
                "constraint_parent|b|2",
                "constraint_parent|b",
                "constraint_child_parent|constraint_parent_key",
                "constraint_child_parent|constraint_parent_key"
            ]
        );
        writer.success(
            &mut engine,
            "ALTER TABLE constraint_parent RENAME COLUMN b TO new_b",
            true,
        );
        let expected = old
            .iter()
            .map(|row| row.replacen("constraint_parent|b", "constraint_parent|new_b", 1))
            .collect::<Vec<_>>();
        assert_eq!(
            data_rows(&reader.success(&mut engine, report, true)),
            if retained { old } else { expected }
        );
        reader.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn catalog_constraint_index_columns_are_strict_and_cover_expressions_and_includes() {
    let config = test_config("constraint-index-columns");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE expression_columns(x integer,y integer);
        CREATE INDEX expression_columns_index ON expression_columns((x+1)) INCLUDE(y)",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT
        pg_get_indexdef('expression_columns_index'::regclass,1,false),
        pg_get_indexdef('expression_columns_index'::regclass,2,false),
        pg_get_indexdef('expression_columns_index'::regclass,3,false),
        pg_get_indexdef('expression_columns_index'::regclass,-1,false),
        pg_get_indexdef('expression_columns_index'::regclass,'-2147483648'::integer,false),
        pg_get_indexdef('expression_columns_index'::regclass,NULL::integer,false),
        pg_get_indexdef('expression_columns_index'::regclass,1,NULL::boolean)",
            true
        )),
        ["((x + 1))|y||||NULL|NULL"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "SELECT pg_get_indexdef('expression_columns_index'::regclass,1)",
            true
        )),
        ["42883"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT
        pg_get_indexdef('expression_columns_index'::regclass,0,true),
        pg_get_indexdef('expression_columns_index'::regclass,1,true)",
            true
        )),
        [
            "CREATE INDEX expression_columns_index ON expression_columns USING btree ((x + 1)) INCLUDE (y)|(x + 1)"
        ]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_get_indexdef(2683,0,true);
         CREATE TABLE public.pg_largeobject(loid oid,pageno integer);
         SET search_path=public,pg_catalog;
         SELECT pg_get_indexdef(2683,0,true),pg_table_is_visible(2613);
         RESET search_path",
            true
        )),
        [
            "CREATE UNIQUE INDEX pg_largeobject_loid_pn_index ON pg_largeobject USING btree (loid, pageno)",
            "CREATE UNIQUE INDEX pg_largeobject_loid_pn_index ON pg_catalog.pg_largeobject USING btree (loid, pageno)|f"
        ]
    );
}

#[test]
fn catalog_constraint_check_nodes_use_current_columns_and_strict_arguments() {
    let mut config = test_config("constraint-check-node-rows");
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "CREATE TABLE expression_checks(a integer PRIMARY KEY,b integer,
        g integer GENERATED ALWAYS AS (b+1) STORED, CONSTRAINT expression_positive CHECK(b>0));
        CREATE SEQUENCE expression_arguments",
        true,
    );
    let report = "SELECT constraint_name,check_clause FROM information_schema.check_constraints WHERE constraint_name='expression_positive'";
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL REPEATABLE READ; {report}"),
            true
        )),
        ["expression_positive|(b > 0)"]
    );
    writer.success(
        &mut engine,
        "ALTER TABLE expression_checks RENAME b TO renamed_b",
        true,
    );
    assert_eq!(
        data_rows(&reader.success(&mut engine, report, true)),
        ["expression_positive|(renamed_b > 0)"]
    );
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            "SELECT generation_expression FROM information_schema.columns
        WHERE table_name='expression_checks' AND column_name='g'",
            true
        )),
        ["(renamed_b + 1)"]
    );
    assert_eq!(data_rows(&reader.success(&mut engine, "SELECT
        pg_get_expr(conbin,NULL::oid),pg_get_expr(conbin,conrelid,NULL::boolean),pg_get_expr(conbin,conrelid,true)
        FROM pg_constraint WHERE conname='expression_positive'", true)), ["NULL|NULL|renamed_b > 0"]);
    reader.success(&mut engine, "ROLLBACK", true);
    assert_eq!(data_rows(&writer.success(&mut engine, "SELECT pg_get_expr(NULL::pg_node_tree,nextval('expression_arguments')::oid,false) IS NULL;
        SELECT is_called FROM expression_arguments;
        SELECT pg_get_expr(conbin,nextval('expression_arguments')::oid,false) IS NULL FROM pg_constraint WHERE conname='expression_checks_pkey';
        SELECT last_value,is_called FROM expression_arguments", true)), ["t", "f", "t", "1|t"]);
    assert_eq!(error_sqlstates(&writer.execute(&mut engine, "SELECT pg_get_expr(conbin,0,false) FROM pg_constraint WHERE conname='expression_positive'", true)), ["22023"]);
    assert_eq!(
        error_sqlstates(&writer.execute(
            &mut engine,
            "SELECT pg_get_expr('not a node'::pg_node_tree,0,false)",
            true
        )),
        ["0A000"]
    );
}

#[test]
fn catalog_constraint_domains_keep_selected_names_and_current_type_permissions() {
    for (isolation, retained) in [("READ COMMITTED", false), ("REPEATABLE READ", true)] {
        let mut config = test_config("constraint-domain-rows");
        config.transaction_mode = crate::config::TransactionMode::Mvcc;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "CREATE DOMAIN constraint_domain AS integer CONSTRAINT domain_positive CHECK(VALUE>0);
            CREATE ROLE domain_reader",
            true,
        );
        reader.success(&mut engine, "SET ROLE domain_reader", true);
        let report = "SELECT domain_name,constraint_name FROM information_schema.domain_constraints WHERE constraint_name='domain_positive'";
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                &format!("BEGIN ISOLATION LEVEL {isolation}; {report}"),
                true
            )),
            ["constraint_domain|domain_positive"]
        );
        writer.success(
            &mut engine,
            "ALTER DOMAIN constraint_domain RENAME TO renamed_constraint_domain",
            true,
        );
        let expected = if retained {
            "constraint_domain|domain_positive"
        } else {
            "renamed_constraint_domain|domain_positive"
        };
        assert_eq!(
            data_rows(&reader.success(&mut engine, report, true)),
            [expected]
        );
        writer.success(
            &mut engine,
            "REVOKE USAGE ON DOMAIN renamed_constraint_domain FROM PUBLIC",
            true,
        );
        assert!(data_rows(&reader.success(&mut engine, report, true)).is_empty());
        writer.success(
            &mut engine,
            "GRANT USAGE ON DOMAIN renamed_constraint_domain TO domain_reader",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(&mut engine, report, true)),
            [expected]
        );
        assert_eq!(data_rows(&reader.success(&mut engine,
            "SELECT has_type_privilege('integer','USAGE'),has_type_privilege(23::oid,'USAGE'),
                has_type_privilege(23::oid,'USAGE WITH GRANT OPTION'),has_type_privilege(2147483647::oid,'USAGE');
             SELECT has_type_privilege(oid,'USAGE') FROM pg_type WHERE typname IN ('constraint_domain','renamed_constraint_domain')", true)),
            ["t|t|f|NULL", "t"]);
        reader.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn catalog_constraint_reports_keep_selected_names_and_private_edits() {
    for (isolation, retained) in [("READ COMMITTED", false), ("REPEATABLE READ", true)] {
        let mut config = test_config("constraint-report-rows");
        config.transaction_mode = crate::config::TransactionMode::Mvcc;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        let old = data_rows(&reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL {isolation}; {REPORT}"),
            true,
        ));
        assert_eq!(
            old,
            [
                "constraint_child|constraint_child_key|UNIQUE|NO",
                "constraint_child|constraint_child_parent|FOREIGN KEY|NULL",
                "constraint_parent|constraint_parent_key|PRIMARY KEY|NULL",
                "constraint_parent|constraint_child_parent",
                "constraint_parent|constraint_parent_key",
            ]
        );
        writer.success(&mut engine, "ALTER TABLE constraint_parent RENAME TO constraint_renamed_parent; ALTER TABLE constraint_child RENAME TO constraint_renamed_child", true);
        let new = old
            .iter()
            .map(|value| {
                value
                    .replacen("constraint_child|", "constraint_renamed_child|", 1)
                    .replacen("constraint_parent|", "constraint_renamed_parent|", 1)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            data_rows(&reader.success(&mut engine, REPORT, true)),
            if retained { old.clone() } else { new.clone() }
        );
        reader.success(&mut engine, "COMMIT; BEGIN; SAVEPOINT before_rename", true);
        reader.success(
            &mut engine,
            "ALTER TABLE constraint_renamed_parent RENAME TO constraint_private_parent",
            true,
        );
        let private = data_rows(&reader.success(&mut engine, REPORT, true));
        assert!(
            private
                .iter()
                .any(|row| row.starts_with("constraint_private_parent|"))
        );
        reader.success(&mut engine, "ROLLBACK TO before_rename", true);
        assert_eq!(data_rows(&reader.success(&mut engine, REPORT, true)), new);
        reader.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn catalog_constraint_reports_use_postgresql_visibility_and_declared_types() {
    let mut config = test_config("constraint-report-access");
    config.transaction_mode = crate::config::TransactionMode::Mvcc;
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut owner = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    owner.success(&mut engine, SETUP, true);
    owner.success(
        &mut engine,
        "CREATE ROLE constraint_reader; GRANT SELECT ON constraint_child TO constraint_reader",
        true,
    );
    reader.success(&mut engine, "SET ROLE constraint_reader", true);
    let count = "SELECT count(*) FROM information_schema.table_constraints WHERE table_name='constraint_child'";
    assert_eq!(data_rows(&reader.success(&mut engine, count, true)), ["0"]);
    assert_eq!(data_rows(&reader.success(&mut engine,
        "SELECT count(*) FROM information_schema.constraint_table_usage WHERE constraint_name='constraint_child_parent'", true)), ["0"]);
    owner.success(
        &mut engine,
        "GRANT UPDATE(y) ON constraint_child TO constraint_reader",
        true,
    );
    assert_eq!(data_rows(&reader.success(&mut engine, count, true)), ["2"]);
    assert_eq!(data_rows(&owner.success(&mut engine,
        "SELECT pg_typeof(constraint_catalog)::text,pg_typeof(constraint_name)::text,
                pg_typeof(constraint_type)::text,pg_typeof(enforced)::text
           FROM information_schema.table_constraints WHERE constraint_name='constraint_child_parent';
         SELECT pg_typeof(table_catalog)::text,pg_typeof(table_name)::text
           FROM information_schema.constraint_table_usage WHERE constraint_name='constraint_child_parent'", true)),
        ["name|name|character varying|character varying", "name|name"]);
}

#[test]
fn catalog_constraint_null_modes_cover_writes_conflicts_like_and_rewrites() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("constraint-null-modes");
        config.transaction_mode = mode;
        if matches!(mode, crate::config::TransactionMode::Locking) {
            config.catalog_history_bytes = 0;
        }
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE null_distinct(a integer UNIQUE NULLS DISTINCT);
             INSERT INTO null_distinct VALUES(NULL),(NULL);
             CREATE TABLE null_equal(a integer,b integer,CONSTRAINT null_equal_key UNIQUE NULLS NOT DISTINCT(a,b));
             INSERT INTO null_equal VALUES(1,NULL)", true);
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "INSERT INTO null_equal VALUES(1,NULL)",
                true
            )),
            ["23505"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "INSERT INTO null_equal VALUES(1,NULL) ON CONFLICT ON CONSTRAINT null_equal_key DO UPDATE SET b=excluded.b RETURNING a;
             SELECT count(*) FROM null_equal", true)), ["1", "1"]);
        session.success(&mut engine,
            "CREATE TABLE null_overlap(a integer UNIQUE,CONSTRAINT null_overlap_strong UNIQUE NULLS NOT DISTINCT(a));
             INSERT INTO null_overlap VALUES(NULL);
             INSERT INTO null_overlap VALUES(NULL) ON CONFLICT(a) DO UPDATE SET a=excluded.a;
             CREATE TABLE null_clone(LIKE null_equal INCLUDING INDEXES);
             INSERT INTO null_clone VALUES(1,NULL)", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT count(*) FROM null_overlap", true)),
            ["1"]
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "INSERT INTO null_clone VALUES(1,NULL)",
                true
            )),
            ["23505"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT nulls_distinct FROM information_schema.table_constraints
             WHERE table_name='null_clone' AND constraint_type='UNIQUE'",
                true
            )),
            ["NO"]
        );
        session.success(
            &mut engine,
            "CREATE TABLE null_rewrite(a integer,b integer);
             INSERT INTO null_rewrite VALUES(1,NULL),(2,NULL)",
            true,
        );
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "ALTER TABLE null_rewrite ALTER COLUMN a TYPE bigint USING 0,
             ADD CONSTRAINT null_rewrite_key UNIQUE NULLS NOT DISTINCT(a,b)",
                true
            )),
            ["23505"]
        );
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT a FROM null_rewrite ORDER BY a;
             SELECT count(*) FROM information_schema.table_constraints WHERE constraint_name='null_rewrite_key'", true)),
            ["1", "2", "0"]);
    }
}

#[test]
fn catalog_constraint_null_modes_keep_deferred_savepoint_semantics() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("constraint-null-deferred");
        config.transaction_mode = mode;
        if matches!(mode, crate::config::TransactionMode::Locking) {
            config.catalog_history_bytes = 0;
        }
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "CREATE TABLE null_deferred(a integer CONSTRAINT null_deferred_key UNIQUE NULLS NOT DISTINCT DEFERRABLE INITIALLY DEFERRED);
             BEGIN; INSERT INTO null_deferred VALUES(NULL),(NULL)", true);
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, "COMMIT", true)),
            ["23505"]
        );
        session.success(&mut engine, "ROLLBACK", true);
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT count(*) FROM null_deferred", true)),
            ["0"]
        );
        session.success(
            &mut engine,
            "BEGIN; INSERT INTO null_deferred VALUES(NULL); SAVEPOINT before_duplicate;
             INSERT INTO null_deferred VALUES(NULL); ROLLBACK TO before_duplicate; COMMIT",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT count(*) FROM null_deferred", true)),
            ["1"]
        );
    }
}

#[test]
fn catalog_constraint_null_modes_survive_prepared_decisions_and_empty_disk() {
    for checkpoint in [false, true] {
        for commit in [false, true] {
            let name = format!("constraint-null-recovery-{checkpoint}-{commit}");
            let mut config = test_config(&name);
            config.transaction_mode = crate::config::TransactionMode::Mvcc;
            config.max_prepared_transactions = 1;
            config.object_store_on = true;
            config.object_store_sim = true;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            // Simulator uploads own growing fixture vectors. SQL reads and
            // failed writes below still use the production allocation guard.
            session.success(&mut engine,
                "CREATE TABLE saved_nulls(lost text,a integer NOT NULL,b integer,\" spaced column\" integer,CONSTRAINT saved_nulls_key UNIQUE NULLS NOT DISTINCT(a,b));
                 CREATE DOMAIN saved_domain AS integer NOT NULL;
                 COMMENT ON COLUMN saved_nulls.lost IS 'removed comment';
                 COMMENT ON COLUMN saved_nulls.a IS 'retained comment';
                 ALTER TABLE saved_nulls ADD CONSTRAINT saved_positive CHECK(a>0);
                 CREATE TABLE saved_nodes(expression pg_node_tree, relation oid);
                 INSERT INTO saved_nodes SELECT conbin,conrelid FROM pg_constraint WHERE conname='saved_positive';
                 BEGIN; ALTER TABLE saved_nulls DROP COLUMN lost, ADD COLUMN fresh smallint;
                 ALTER TABLE saved_nulls RENAME TO saved_renamed_nulls;
                 ALTER DOMAIN saved_domain RENAME TO saved_renamed_domain;
                 INSERT INTO saved_renamed_nulls(a,b) VALUES(1,NULL);
                 PREPARE TRANSACTION 'saved-null-mode'", false);
            if checkpoint {
                assert!(engine.checkpoint().unwrap());
            }
            drop(session);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut cold = Engine::new(&config, &mut cold_budget)
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            for slot in 0..cold.storage.table_count() {
                assert!(
                    !cold.storage.statistics_wal_dirty(slot),
                    "{name}: table {slot}"
                );
            }
            let recovered_journal = cold.journal_lsn();
            let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            let metadata = "SELECT table_name,nulls_distinct FROM information_schema.table_constraints WHERE constraint_name='saved_nulls_key'";
            assert_eq!(
                data_rows(&session.success(&mut cold, metadata, true)),
                ["saved_nulls|NO"]
            );
            assert_eq!(cold.journal_lsn(), recovered_journal);
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT pg_typeof(expression),pg_get_expr(expression,relation,false) FROM saved_nodes", true)),
                ["pg_node_tree|(a > 0)"]);
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT column_name FROM information_schema.columns WHERE table_name='saved_nulls' AND column_name=' spaced column'", true)), [" spaced column"]);
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT domain_name,constraint_name FROM information_schema.domain_constraints WHERE constraint_name='saved_domain_not_null';
                 SELECT check_clause FROM information_schema.check_constraints WHERE constraint_name='saved_nulls_a_not_null'", true)),
                ["saved_domain|saved_domain_not_null", "a IS NOT NULL"]);
            session.success(
                &mut cold,
                if commit {
                    "COMMIT PREPARED 'saved-null-mode'"
                } else {
                    "ROLLBACK PREPARED 'saved-null-mode'"
                },
                false,
            );
            let table = if commit {
                "saved_renamed_nulls"
            } else {
                "saved_nulls"
            };
            // SQL sorting uploads temporary objects to the growing simulator
            // fixture. Keep the guarded recovery read free of fixture writes.
            let mut comments = data_rows(&session.success(&mut cold,
                &format!("SELECT objsubid,description FROM pg_description WHERE objoid='{table}'::regclass"), true));
            comments.sort();
            assert_eq!(
                comments,
                if commit {
                    vec!["2|retained comment"]
                } else {
                    vec!["1|removed comment", "2|retained comment"]
                }
            );
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT pg_typeof(expression),pg_get_expr(expression,relation,false) FROM saved_nodes", true)),
                ["pg_node_tree|(a > 0)"]);
            assert_eq!(
                data_rows(&session.success(&mut cold, metadata, true)),
                [format!("{table}|NO")]
            );
            assert_eq!(data_rows(&session.success(&mut cold,
                "SELECT domain_name,constraint_name FROM information_schema.domain_constraints WHERE constraint_name='saved_domain_not_null';
                 SELECT check_clause FROM information_schema.check_constraints WHERE constraint_name='saved_nulls_a_not_null'", true)),
                [if commit { "saved_renamed_domain|saved_domain_not_null" } else { "saved_domain|saved_domain_not_null" }, "a IS NOT NULL"]);
            if !commit {
                session.success(
                    &mut cold,
                    "INSERT INTO saved_nulls(a,b) VALUES(1,NULL)",
                    false,
                );
            }
            assert_eq!(
                error_sqlstates(&session.execute(
                    &mut cold,
                    &format!("INSERT INTO {table}(a,b) VALUES(1,NULL)"),
                    true
                )),
                ["23505"]
            );
            assert_eq!(
                data_rows(&session.success(
                    &mut cold,
                    &format!("SELECT count(*) FROM {table}"),
                    true
                )),
                ["1"]
            );
            drop(session);
            drop(cold);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }
}
