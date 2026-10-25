use super::*;

#[test]
fn schema_rename_without_a_public_replacement_survives_empty_disk_recovery() {
    for checkpoint in [false, true] {
        let name = format!("schema-no-public-recovery-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine, "CREATE TABLE public.items(id integer);
            INSERT INTO public.items VALUES(23);
            ALTER SCHEMA public RENAME TO archived", false);
        if checkpoint { assert!(engine.checkpoint().unwrap()); }
        drop(session);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut cold = Engine::new(&config, &mut cold_budget).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(data_rows(&session.success(&mut cold, "
            SELECT to_regnamespace('public') IS NULL;
            SELECT 'archived'::regnamespace::oid=2200;
            SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='archived';
            SELECT id FROM archived.items;
        ", true)), ["t", "t", "pg_database_owner", "23"], "{name}");
        session.success(&mut cold, "CREATE SCHEMA public; CREATE TABLE public.items(id integer)", false);
        assert_eq!(data_rows(&session.success(&mut cold, "
            SELECT 'public'::regnamespace::oid<>2200;
            SELECT id FROM archived.items;
        ", true)), ["t", "23"], "{name}");
        drop(session);
        drop(cold);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}

#[test]
fn schema_rename_prepared_owners_remain_reserved_after_empty_disk_recovery() {
    for checkpoint in [false, true] {
      for decision in ["COMMIT", "ROLLBACK"] {
        let name = format!("schema-owner-recovery-{checkpoint}-{decision}");
        let mut config = test_config(&name);
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
        session.success(&mut engine, "CREATE ROLE retained_schema_owner; CREATE SCHEMA existing_schema_owner", false);
        session.success(&mut engine, "BEGIN;
            ALTER SCHEMA existing_schema_owner OWNER TO retained_schema_owner;
            CREATE SCHEMA created_schema_owner AUTHORIZATION retained_schema_owner;
            PREPARE TRANSACTION 'schema_owner'", false);
        if checkpoint { assert!(engine.checkpoint().unwrap()); }
        drop(session);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut cold = Engine::new(&config, &mut cold_budget).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(data_rows(&session.success(&mut cold, "
            SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='existing_schema_owner';
            SELECT to_regnamespace('created_schema_owner') IS NULL;
        ", true)), ["postgres", "t"]);
        let prepared = cold.prepared_transactions.find(ast::PreparedTransactionId::parse("schema_owner").unwrap()).unwrap();
        let transaction = cold.prepared_transactions.slot(prepared).metadata().transaction_id;
        let owner = cold.storage.find_role_visible("retained_schema_owner", 0).unwrap();
        for schema in ["existing_schema_owner", "created_schema_owner"] {
            let slot = cold.storage.find_schema_visible(schema, transaction).unwrap();
            assert_eq!(cold.storage.object_owner(crate::storage::AccessObject {
                class: crate::storage::AccessClass::Schema, slot: slot as u16,
            }, transaction), owner, "{name}: {schema}");
        }
        let target = crate::storage::ObjectLockTarget::Role(cold.storage.role(owner).oid());
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(cold.storage.lock_object(1000, target,
                ast::TableLockMode::AccessExclusive, true).unwrap_err().sqlstate,
                crate::sql::eval::sqlstate::LOCK_NOT_AVAILABLE);
        });
        session.success(&mut cold, &format!("{decision} PREPARED 'schema_owner'"), false);
        crate::mem::guard::forbid_alloc(|| {
            cold.storage.lock_object(1000, target, ast::TableLockMode::AccessExclusive, true).unwrap();
            cold.storage.release_object_locks(1000);
        });
        if decision == "COMMIT" {
            assert_eq!(data_rows(&session.success(&mut cold, "
                SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='created_schema_owner';
                SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='existing_schema_owner';
            ", true)), ["retained_schema_owner", "retained_schema_owner"]);
            session.success(&mut cold, "DROP SCHEMA created_schema_owner; DROP SCHEMA existing_schema_owner", false);
        }
        session.success(&mut cold, "DROP ROLE retained_schema_owner", false);
        assert!(cold.storage.find_role_visible("retained_schema_owner", 0).is_none());
        drop(session);
        drop(cold);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
      }
    }
}

#[test]
fn schema_rename_identities_and_reused_slots_survive_wal_and_empty_disk_recovery() {
    for checkpoint in [false, true] {
        let name = format!("schema-identity-recovery-{checkpoint}");
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine, "
            COMMENT ON SCHEMA public IS 'original public identity';
            ALTER SCHEMA public RENAME TO public_before;
            CREATE SCHEMA public;
            CREATE SCHEMA reused_namespace;
        ", false);
        let previous = data_rows(&session.success(&mut engine,
            "SELECT 'reused_namespace'::regnamespace::oid", true));
        session.success(&mut engine, "DROP SCHEMA reused_namespace", false);
        if checkpoint { assert!(engine.checkpoint().unwrap()); }
        drop(session);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut cold = Engine::new(&config, &mut cold_budget).unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(data_rows(&session.success(&mut cold, "
            SELECT 'public_before'::regnamespace::oid=2200,'public'::regnamespace::oid<>2200;
            SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='public_before';
            SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='public';
            SELECT obj_description(2200,'pg_namespace');
        ", true)), ["t|t", "pg_database_owner", "postgres", "original public identity"], "{name}");
        session.success(&mut cold, "CREATE SCHEMA reused_namespace", false);
        assert_ne!(data_rows(&session.success(&mut cold,
            "SELECT 'reused_namespace'::regnamespace::oid", true)), previous, "{name}");
        drop(session);
        drop(cold);
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
    }
}

#[test]
fn schema_rename_preserves_public_identity_and_a_replacement_gets_its_own_identity() {
    let config = test_config("public-schema-identity");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "
        COMMENT ON SCHEMA public IS 'initial public namespace';
        BEGIN; ALTER SCHEMA public RENAME TO public_before;
    ", true);
    assert_eq!(data_rows(&session.success(&mut engine, "
        SELECT oid=2200,nspowner::regrole::text FROM pg_namespace WHERE nspname='public_before';
        SELECT to_regnamespace('public') IS NULL;
        SELECT obj_description(2200,'pg_namespace');
    ", true)), ["t|pg_database_owner", "t", "initial public namespace"]);
    session.success(&mut engine, "CREATE SCHEMA public", true);
    assert_eq!(data_rows(&session.success(&mut engine, "
        SELECT oid<>2200,nspowner::regrole::text FROM pg_namespace WHERE nspname='public';
        SELECT to_regnamespace('public_before')::oid=2200;
    ", true)), ["t|postgres", "t"]);
    session.success(&mut engine, "COMMIT", true);
    assert_eq!(data_rows(&session.success(&mut engine, "
        SELECT oid=2200,nspowner::regrole::text FROM pg_namespace WHERE nspname='public_before';
        SELECT oid<>2200,nspowner::regrole::text FROM pg_namespace WHERE nspname='public';
        SELECT obj_description(2200,'pg_namespace');
    ", true)), ["t|pg_database_owner", "t|postgres", "initial public namespace"]);
    session.success(&mut engine, "BEGIN; SAVEPOINT before_owner;
        ALTER SCHEMA public_before OWNER TO postgres", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='public_before'", true)), ["postgres"]);
    session.success(&mut engine, "ROLLBACK TO before_owner", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='public_before'", true)), ["pg_database_owner"]);
    session.success(&mut engine, "ALTER SCHEMA public_before OWNER TO postgres; COMMIT", true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname='public_before'", true)), ["postgres"]);
}

#[test]
fn schema_rename_preserves_aliases_and_distinguishes_quoted_schema_names() {
    let config = test_config("bound-schema-qualifiers");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, r#"
        CREATE SCHEMA alias_schema;
        CREATE TABLE alias_schema.items(id integer);
        INSERT INTO alias_schema.items VALUES (21);
        CREATE VIEW alias_schema.item_view AS
          SELECT alias_schema.id FROM alias_schema /* namespace */ .items AS alias_schema;
        CREATE SCHEMA "CaseSchema";
        CREATE SCHEMA caseschema;
        CREATE TABLE "CaseSchema".items(id integer);
        CREATE TABLE caseschema.items(id integer);
        INSERT INTO "CaseSchema".items VALUES (31);
        INSERT INTO caseschema.items VALUES (41);
        CREATE VIEW both_case_schemas AS SELECT a.id AS quoted,b.id AS unquoted
          FROM "CaseSchema".items AS a CROSS JOIN CASESCHEMA.items AS b;
        BEGIN;
        ALTER SCHEMA alias_schema RENAME TO alias_after;
        ALTER SCHEMA "CaseSchema" RENAME TO "MovedCaseSchema";
    "#, true);
    assert_eq!(data_rows(&session.success(&mut engine, "
        SELECT id FROM alias_after.item_view;
        SELECT quoted,unquoted FROM both_case_schemas;
    ", true)), ["21", "31|41"]);
    session.success(&mut engine, "COMMIT", true);
    assert_eq!(data_rows(&session.success(&mut engine, "
        SELECT id FROM alias_after.item_view;
        SELECT quoted,unquoted FROM both_case_schemas;
    ", true)), ["21", "31|41"]);
}

#[test]
fn schema_rename_preserves_quoted_search_paths_and_unqualified_bound_queries() {
    let config = test_config("quoted-schema-paths");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, r#"
        CREATE SCHEMA "schema,""quoted""";
        SET search_path TO "schema,""quoted""";
        CREATE TABLE items(id integer);
        INSERT INTO items VALUES (12);
        CREATE VIEW item_view AS SELECT id FROM items;
        CREATE VIEW qualified_view AS SELECT id FROM "schema,""quoted""".items;
        CREATE FUNCTION bound() RETURNS integer LANGUAGE SQL BEGIN ATOMIC SELECT id FROM items; END;
        CREATE FUNCTION qualified_bound() RETURNS integer LANGUAGE SQL
          BEGIN ATOMIC SELECT id FROM "schema,""quoted""".items; END;
        BEGIN;
        ALTER SCHEMA "schema,""quoted""" RENAME TO "Next,""quoted""";
        SET LOCAL search_path TO "Next,""quoted""";
    "#, true);
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT current_schema(); SELECT id FROM item_view; SELECT bound(); SELECT id FROM qualified_view; SELECT qualified_bound()", true)),
        ["Next,\"quoted\"", "12", "12", "12", "12"]);
    session.success(&mut engine, "COMMIT", true);
    assert_eq!(data_rows(&session.success(&mut engine, r#"
        SET search_path TO "Next,""quoted""";
        SELECT current_schema(); SELECT id FROM item_view; SELECT bound();
        SELECT id FROM qualified_view; SELECT qualified_bound();
    "#, true)), ["Next,\"quoted\"", "12", "12", "12", "12"]);
}

#[test]
fn schema_rename_prepared_decisions_preserve_private_definitions_after_disk_loss() {
    for checkpoint in [false, true] {
        for decision in ["COMMIT PREPARED", "ROLLBACK PREPARED"] {
            let name = format!("schema-prepared-{checkpoint}-{decision}");
            let mut config = test_config(&name);
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
            session.success(&mut engine, "
                CREATE SCHEMA prepared_schema_before;
                CREATE TYPE prepared_schema_before.mood AS ENUM ('ready');
                CREATE TABLE prepared_schema_before.items(id integer,state prepared_schema_before.mood);
                INSERT INTO prepared_schema_before.items VALUES (8,'ready');
                CREATE VIEW prepared_schema_before.item_view AS SELECT id,state FROM prepared_schema_before.items;
                CREATE MATERIALIZED VIEW prepared_schema_cache AS SELECT id FROM prepared_schema_before.items;
                CREATE FUNCTION prepared_schema_before.bound() RETURNS integer LANGUAGE SQL
                  BEGIN ATOMIC SELECT id FROM prepared_schema_before.items; END;
                COMMENT ON SCHEMA prepared_schema_before IS 'prepared namespace';
            ", false);
            session.success(&mut engine, "BEGIN; ALTER SCHEMA prepared_schema_before RENAME TO prepared_schema_after", true);
            assert_eq!(data_rows(&session.success(&mut engine,
                "SELECT prepared_schema_after.bound(); SELECT id,state FROM prepared_schema_after.items", true)),
                ["8", "8|ready"]);
            session.success(&mut engine, "PREPARE TRANSACTION 'schema_move'", false);
            if checkpoint { assert!(engine.checkpoint().unwrap()); }
            drop(session);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut cold = Engine::new(&config, &mut cold_budget).unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            assert_eq!(data_rows(&session.success(&mut cold, "
                SELECT to_regnamespace('prepared_schema_before') IS NOT NULL,to_regnamespace('prepared_schema_after') IS NULL;
                SELECT prepared_schema_before.bound();
                SELECT pg_get_viewdef('prepared_schema_cache'::regclass) LIKE '%prepared_schema_before%';
            ", true)), ["t|t", "8", "t"], "{name}");
            session.success(&mut cold, &format!("{decision} 'schema_move'"), false);
            let schema = if decision == "COMMIT PREPARED" { "prepared_schema_after" } else { "prepared_schema_before" };
            assert_eq!(data_rows(&session.success(&mut cold, &format!("
                SELECT {schema}.bound(); SELECT id,state FROM {schema}.items;
                SELECT obj_description('{schema}'::regnamespace,'pg_namespace');
            "), true)), ["8", "8|ready", "prepared namespace"], "{name}");
            // View materialization writes simulator objects, whose fixture
            // vectors grow. The rename and direct metadata reads stay guarded.
            assert_eq!(data_rows(&session.success(&mut cold, &format!("
                SELECT id,state FROM {schema}.item_view;
                REFRESH MATERIALIZED VIEW prepared_schema_cache;
                SELECT id FROM prepared_schema_cache;
            "), false)), ["8|ready", "8"], "{name}");
            drop(session);
            drop(cold);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }
}

#[test]
fn schema_rename_rewrites_private_materialized_queries_and_index_expressions() {
    let config = test_config("private-schema-query-text");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(&mut engine, "
        CREATE SCHEMA query_schema_before;
        CREATE FUNCTION query_schema_before.score(value integer) RETURNS integer
          LANGUAGE SQL IMMUTABLE AS 'SELECT value+1';
        CREATE TABLE query_schema_before.items(id integer);
        INSERT INTO query_schema_before.items VALUES (4);
        CREATE INDEX scored_items ON query_schema_before.items
          ((query_schema_before.score(id))) WHERE query_schema_before.score(id)>0;
        CREATE MATERIALIZED VIEW cached_schema_items AS
          SELECT query_schema_before.score(id) AS score FROM query_schema_before.items;
    ", true);
    writer.success(&mut engine, "BEGIN; SAVEPOINT before_move;
        ALTER SCHEMA query_schema_before RENAME TO query_schema_after;
        REFRESH MATERIALIZED VIEW cached_schema_items", true);
    assert_eq!(data_rows(&writer.success(&mut engine, "
        SELECT query_schema_after.score(id) FROM query_schema_after.items;
        SELECT score FROM cached_schema_items;
        SELECT pg_get_viewdef('cached_schema_items'::regclass) LIKE '%query_schema_after%';
    ", true)), ["5", "5", "t"]);
    assert_eq!(data_rows(&reader.success(&mut engine, "
        SELECT query_schema_before.score(id) FROM query_schema_before.items;
        SELECT pg_get_viewdef('cached_schema_items'::regclass) LIKE '%query_schema_before%';
    ", true)), ["5", "t"]);
    writer.success(&mut engine, "ROLLBACK TO before_move", true);
    assert_eq!(data_rows(&writer.success(&mut engine,
        "SELECT query_schema_before.score(id) FROM query_schema_before.items", true)), ["5"]);
    writer.success(&mut engine, "ALTER SCHEMA query_schema_before RENAME TO query_schema_after;
        REFRESH MATERIALIZED VIEW cached_schema_items; COMMIT", true);
    assert_eq!(data_rows(&reader.success(&mut engine, "
        REFRESH MATERIALIZED VIEW cached_schema_items;
        SELECT score FROM cached_schema_items;
        SELECT pg_get_viewdef('cached_schema_items'::regclass) LIKE '%query_schema_after%';
    ", true)), ["5", "t"]);
}

#[test]
fn schema_rename_keeps_names_and_dependent_definitions_private_until_commit() {
    let setup = "
        CREATE SCHEMA private_schema_before;
        CREATE TYPE private_schema_before.mood AS ENUM ('ready');
        CREATE SEQUENCE private_schema_before.ticket;
        CREATE TABLE private_schema_before.orders(id bigint DEFAULT nextval('private_schema_before.ticket'),state private_schema_before.mood);
        INSERT INTO private_schema_before.orders(state) VALUES ('ready');
        CREATE VIEW private_schema_before.order_view AS SELECT id,state FROM private_schema_before.orders;
        CREATE FUNCTION private_schema_before.bound() RETURNS bigint LANGUAGE SQL
          BEGIN ATOMIC SELECT id FROM private_schema_before.orders; END;
        CREATE FUNCTION private_schema_before.unbound() RETURNS bigint LANGUAGE SQL AS 'SELECT id FROM private_schema_before.orders';
        COMMENT ON SCHEMA private_schema_before IS 'private namespace';
        COMMENT ON TABLE private_schema_before.orders IS 'private table';
    ";
    let old = "SELECT to_regnamespace('private_schema_before') IS NOT NULL,to_regnamespace('private_schema_after') IS NULL;
        SELECT nspname FROM pg_namespace WHERE nspname IN ('private_schema_before','private_schema_after');
        SELECT id,state FROM private_schema_before.order_view;
        SELECT private_schema_before.bound();";
    let new = "SELECT to_regnamespace('private_schema_before') IS NULL,to_regnamespace('private_schema_after') IS NOT NULL;
        SELECT nspname FROM pg_namespace WHERE nspname IN ('private_schema_before','private_schema_after');
        SELECT id,state FROM private_schema_after.order_view;
        SELECT private_schema_after.bound();
        SELECT obj_description('private_schema_after.orders'::regclass);
        SELECT obj_description('private_schema_after'::regnamespace,'pg_namespace');";
    for mode in [crate::config::TransactionMode::Mvcc,crate::config::TransactionMode::Locking] {
        let mut config = test_config("private-schema-rename");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, setup, true);
        let old_rows = ["t|t", "private_schema_before", "1|ready", "1"];
        let new_rows = ["t|t", "private_schema_after", "1|ready", "1", "private table", "private namespace"];
        assert_eq!(data_rows(&reader.success(&mut engine, old, true)), old_rows);
        writer.success(&mut engine, "BEGIN; SAVEPOINT schema_change; ALTER SCHEMA private_schema_before RENAME TO private_schema_after", true);
        assert_eq!(data_rows(&writer.success(&mut engine, new, true)), new_rows);
        assert_eq!(data_rows(&reader.success(&mut engine, old, true)), old_rows);
        writer.success(&mut engine, "ROLLBACK TO schema_change", true);
        assert_eq!(data_rows(&writer.success(&mut engine, old, true)), old_rows);
        writer.success(&mut engine, "ALTER SCHEMA private_schema_before RENAME TO private_schema_after; COMMIT", true);
        assert_eq!(data_rows(&reader.success(&mut engine, new, true)), new_rows);
        assert_eq!(error_sqlstates(&reader.execute(&mut engine, "SELECT private_schema_after.unbound()", true)), ["42P01"]);
    }
}

#[test]
fn catalog_snapshot_privilege_reports_preserve_selected_grants_and_public_role_rows() {
    let report = "
        SELECT grantor,grantee,table_name,privilege_type,is_grantable FROM information_schema.table_privileges
          WHERE table_name IN ('snapshot_grant_before','snapshot_grant_after') AND grantee IN ('PUBLIC','snapshot_grantee') ORDER BY grantee;
        SELECT grantor,grantee,table_name,privilege_type FROM information_schema.role_table_grants
          WHERE table_name IN ('snapshot_grant_before','snapshot_grant_after') AND grantee='PUBLIC';
        SELECT grantor,grantee,routine_name,is_grantable FROM information_schema.routine_privileges
          WHERE routine_name='snapshot_grant_function' AND grantee='PUBLIC';
        SELECT grantor,grantee,routine_name FROM information_schema.role_routine_grants
          WHERE routine_name='snapshot_grant_function' AND grantee='PUBLIC';
    ";
    let old = [
        "postgres|PUBLIC|snapshot_grant_before|SELECT|NO",
        "postgres|PUBLIC|snapshot_grant_before|SELECT",
        "postgres|PUBLIC|snapshot_grant_function|NO",
        "postgres|PUBLIC|snapshot_grant_function",
    ];
    let new = ["postgres|snapshot_grantee|snapshot_grant_after|UPDATE|YES"];
    for isolation in ["READ COMMITTED", "REPEATABLE READ"] {
        let config = test_config("snapshot-grant-reports");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "CREATE ROLE snapshot_grantee; CREATE TABLE snapshot_grant_before(id integer);
            CREATE FUNCTION snapshot_grant_function() RETURNS integer LANGUAGE SQL AS 'SELECT 1';
            GRANT SELECT ON snapshot_grant_before TO PUBLIC",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                &format!("BEGIN ISOLATION LEVEL {isolation}; {report}"),
                true
            )),
            old
        );
        writer.success(
            &mut engine,
            "REVOKE SELECT ON snapshot_grant_before FROM PUBLIC;
            GRANT UPDATE ON snapshot_grant_before TO snapshot_grantee WITH GRANT OPTION;
            ALTER TABLE snapshot_grant_before RENAME TO snapshot_grant_after;
            REVOKE EXECUTE ON FUNCTION snapshot_grant_function() FROM PUBLIC",
            true,
        );
        let selected = data_rows(&reader.success(&mut engine, report, true));
        assert_eq!(
            selected,
            if isolation == "REPEATABLE READ" {
                old.as_slice()
            } else {
                new.as_slice()
            },
            "{isolation}"
        );
        reader.success(&mut engine, "COMMIT", true);
        assert_eq!(data_rows(&reader.success(&mut engine, report, true)), new);
    }
}

#[test]
fn catalog_snapshot_default_acls_keep_missing_owner_identifiers() {
    let (mut engine, budget) = test_engine();
    let output = run_with_fixed_memory(
        &mut engine,
        &budget,
        "
        SELECT acldefault('f',12345::oid)::text,acldefault('c',12345::oid)::text,
               acldefault('r',12345::oid) IS NOT NULL;
    ",
        4 << 20,
    );
    assert_eq!(data_rows(&output), ["{=X/12345,12345=X/12345}|{}|t"]);
}

#[test]
fn catalog_snapshot_column_grants_union_before_calculating_owner_grant_options() {
    let config = test_config("snapshot-column-grants");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE snapshot_column_reader;
        CREATE TABLE snapshot_column_grants(id integer,payload text);
        GRANT SELECT ON snapshot_column_grants TO snapshot_column_reader;
        GRANT SELECT(id) ON snapshot_column_grants TO snapshot_column_reader",
        true,
    );
    let report = "SELECT column_name,is_grantable FROM information_schema.column_privileges
        WHERE table_name='snapshot_column_grants' AND grantee='snapshot_column_reader' ORDER BY column_name,is_grantable";
    assert_eq!(
        data_rows(&session.success(&mut engine, report, true)),
        ["id|NO", "payload|NO"]
    );
    session.success(
        &mut engine,
        "GRANT SELECT(id) ON snapshot_column_grants TO snapshot_column_reader WITH GRANT OPTION",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, report, true)),
        ["id|NO", "id|YES", "payload|NO"]
    );
    session.success(
        &mut engine,
        "GRANT postgres TO snapshot_column_reader",
        true,
    );
    assert_eq!(
        data_rows(&session.success(&mut engine, report, true)),
        ["id|YES", "id|YES", "payload|YES"]
    );
}

#[test]
fn catalog_snapshot_usage_reports_include_domains_collations_and_sequence_acl_rows() {
    let config = test_config("snapshot-usage-grants");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE snapshot_usage_reader;
        CREATE DOMAIN snapshot_usage_domain AS integer;
        CREATE COLLATION snapshot_usage_collation FROM \"C\";
        CREATE SEQUENCE snapshot_usage_sequence",
        true,
    );
    let report = "SELECT object_name,object_type,grantee,is_grantable FROM information_schema.usage_privileges
        WHERE object_name LIKE 'snapshot_usage_%' ORDER BY object_name,grantee";
    assert_eq!(
        data_rows(&session.success(&mut engine, report, true)),
        [
            "snapshot_usage_collation|COLLATION|PUBLIC|NO",
            "snapshot_usage_domain|DOMAIN|PUBLIC|NO",
            "snapshot_usage_domain|DOMAIN|postgres|YES",
        ]
    );
    session.success(
        &mut engine,
        "GRANT USAGE ON SEQUENCE snapshot_usage_sequence TO snapshot_usage_reader",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine, "SELECT object_name,grantee,is_grantable FROM information_schema.role_usage_grants WHERE object_name='snapshot_usage_sequence' ORDER BY grantee", true)), [
        "snapshot_usage_sequence|postgres|YES", "snapshot_usage_sequence|snapshot_usage_reader|NO",
    ]);
}

#[test]
fn catalog_snapshot_role_reports_join_selected_memberships_and_database_owners() {
    let report = "SELECT grantee,role_name,is_grantable FROM information_schema.applicable_roles
        WHERE role_name IN ('snapshot_role_target','pg_database_owner') ORDER BY role_name,grantee;
        SELECT grantee,role_name FROM information_schema.administrable_role_authorizations
        WHERE role_name='snapshot_role_target';
        SELECT role_name FROM information_schema.enabled_roles WHERE role_name LIKE 'snapshot_role_%' ORDER BY role_name";
    let old = [
        "postgres|pg_database_owner|NO",
        "snapshot_role_member|snapshot_role_target|NO",
        "snapshot_role_member",
        "snapshot_role_target",
    ];
    let new = [
        "snapshot_role_target|pg_database_owner|NO",
        "snapshot_role_renamed|snapshot_role_target|YES",
        "snapshot_role_renamed|snapshot_role_target",
        "snapshot_role_renamed",
        "snapshot_role_target",
    ];
    for isolation in ["REPEATABLE READ", "READ COMMITTED"] {
        let config = test_config("snapshot-role-membership-reports");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "CREATE ROLE snapshot_role_target; CREATE ROLE snapshot_role_member;
            GRANT snapshot_role_target TO snapshot_role_member",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                &format!("BEGIN ISOLATION LEVEL {isolation}; {report}"),
                true
            )),
            old
        );
        writer.success(
            &mut engine,
            "ALTER ROLE snapshot_role_member RENAME TO snapshot_role_renamed;
            GRANT snapshot_role_target TO snapshot_role_renamed WITH ADMIN OPTION;
            ALTER DATABASE postgres OWNER TO snapshot_role_target",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(&mut engine, report, true)),
            if isolation == "REPEATABLE READ" {
                old.as_slice()
            } else {
                new.as_slice()
            },
            "{isolation}"
        );
        reader.success(&mut engine, "COMMIT", true);
        assert_eq!(data_rows(&reader.success(&mut engine, report, true)), new);
    }
}

#[test]
fn catalog_snapshot_account_reports_join_selected_roles_memberships_and_settings() {
    let report = "
        SELECT usename,usecreatedb,useconfig,passwd FROM pg_user
          WHERE usename LIKE 'snapshot_account_%' ORDER BY usename;
        SELECT usename,usecreatedb,useconfig,passwd IS NULL FROM pg_shadow
          WHERE usename LIKE 'snapshot_account_%' ORDER BY usename;
        SELECT groname,array_length(grolist,1) FROM pg_group
          WHERE groname='snapshot_group';
    ";
    let old = [
        "snapshot_account_before|f|{search_path=public}|********",
        "snapshot_account_before|f|{search_path=public}|t",
        "snapshot_group|1",
    ];
    let new = [
        "snapshot_account_after|t|{search_path=pg_catalog}|********",
        "snapshot_account_after|t|{search_path=pg_catalog}|t",
        "snapshot_group|2",
    ];
    for isolation in ["REPEATABLE READ", "READ COMMITTED"] {
        let config = test_config("snapshot-account-reports");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "
            CREATE ROLE snapshot_group;
            CREATE ROLE snapshot_account_before LOGIN;
            CREATE ROLE snapshot_member;
            GRANT snapshot_group TO snapshot_account_before;
            ALTER ROLE snapshot_account_before SET search_path=public;
        ",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                &format!("BEGIN ISOLATION LEVEL {isolation}; {report}"),
                true
            )),
            old
        );
        writer.success(
            &mut engine,
            "
            ALTER ROLE snapshot_account_before RENAME TO snapshot_account_after;
            ALTER ROLE snapshot_account_after CREATEDB;
            ALTER ROLE snapshot_account_after SET search_path=pg_catalog;
            GRANT snapshot_group TO snapshot_member;
        ",
            true,
        );
        let selected = reader.success(&mut engine, report, true);
        assert_eq!(
            data_rows(&selected),
            if isolation == "REPEATABLE READ" {
                old
            } else {
                new
            },
            "{isolation}: {}",
            String::from_utf8_lossy(&selected)
        );
        reader.success(&mut engine, "COMMIT", true);
        assert_eq!(data_rows(&reader.success(&mut engine, report, true)), new);
        writer.success(&mut engine, "BEGIN; SAVEPOINT account_change; ALTER ROLE snapshot_account_after NOLOGIN; REVOKE snapshot_group FROM snapshot_member", true);
        assert_eq!(
            data_rows(&writer.success(&mut engine, report, true)),
            ["snapshot_group|1"]
        );
        assert_eq!(data_rows(&reader.success(&mut engine, report, true)), new);
        writer.success(&mut engine, "ROLLBACK TO account_change", true);
        assert_eq!(data_rows(&writer.success(&mut engine, report, true)), new);
        writer.success(&mut engine, "ROLLBACK; SET ROLE snapshot_member", true);
        assert_eq!(
            error_sqlstates(&writer.execute(&mut engine, "SELECT * FROM pg_shadow", true)),
            ["42501"]
        );
        let public = writer.success(
            &mut engine,
            "SELECT passwd FROM pg_user WHERE usename='snapshot_account_after'",
            true,
        );
        assert_eq!(data_rows(&public), ["********"]);
    }
}

#[test]
fn catalog_snapshot_schemata_joins_selected_owner_names_and_current_permissions() {
    let report = "SELECT schema_name,schema_owner,pg_typeof(catalog_name)::text,pg_typeof(sql_path)::text
        FROM information_schema.schemata WHERE schema_name LIKE 'snapshot_space_%' ORDER BY schema_name";
    let config = test_config("snapshot-schema-reports");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "
        CREATE ROLE snapshot_schema_reader;
        CREATE ROLE snapshot_schema_owner;
        CREATE SCHEMA snapshot_space_visible AUTHORIZATION snapshot_schema_owner;
        CREATE SCHEMA snapshot_space_hidden;
        GRANT USAGE ON SCHEMA snapshot_space_visible TO snapshot_schema_reader;
        CREATE TABLE snapshot_permission_refresh(id integer);
        GRANT SELECT ON snapshot_permission_refresh TO snapshot_schema_reader;
    ",
        true,
    );
    reader.success(
        &mut engine,
        "SET ROLE snapshot_schema_reader; BEGIN ISOLATION LEVEL REPEATABLE READ",
        true,
    );
    assert_eq!(
        data_rows(&reader.success(&mut engine, report, true)),
        ["snapshot_space_visible|snapshot_schema_owner|name|character varying"]
    );
    writer.success(&mut engine, "ALTER ROLE snapshot_schema_owner RENAME TO snapshot_schema_owner_after; GRANT CREATE ON SCHEMA snapshot_space_hidden TO snapshot_schema_reader", true);
    reader.success(
        &mut engine,
        "LOCK TABLE snapshot_permission_refresh IN ACCESS SHARE MODE",
        true,
    );
    assert_eq!(
        data_rows(&reader.success(&mut engine, report, true)),
        [
            "snapshot_space_hidden|postgres|name|character varying",
            "snapshot_space_visible|snapshot_schema_owner|name|character varying",
        ]
    );
    writer.success(
        &mut engine,
        "REVOKE USAGE ON SCHEMA snapshot_space_visible FROM snapshot_schema_reader",
        true,
    );
    reader.success(
        &mut engine,
        "LOCK TABLE snapshot_permission_refresh IN ACCESS SHARE MODE",
        true,
    );
    assert_eq!(
        data_rows(&reader.success(&mut engine, report, true)),
        ["snapshot_space_hidden|postgres|name|character varying"]
    );
    reader.success(&mut engine, "COMMIT; RESET ROLE", true);
    assert_eq!(
        data_rows(&reader.success(&mut engine, report, true)),
        [
            "snapshot_space_hidden|postgres|name|character varying",
            "snapshot_space_visible|snapshot_schema_owner_after|name|character varying",
        ]
    );
}

#[test]
fn catalog_snapshot_routine_reports_preserve_definitions_and_check_current_execution_access() {
    let report = "
        SELECT routine_name,routine_type,data_type,routine_body,external_language,
               is_deterministic,is_null_call,security_type,routine_definition='SELECT a+b'
          FROM information_schema.routines WHERE routine_name LIKE 'snapshot_routine_%';
        SELECT ordinal_position,parameter_name,parameter_mode,data_type,udt_name,parameter_default
          FROM information_schema.parameters WHERE specific_schema='public' AND specific_name LIKE 'snapshot_routine_%'
          ORDER BY ordinal_position;
    ";
    let old = [
        "snapshot_routine_before|FUNCTION|integer|SQL|SQL|YES|YES|DEFINER|t",
        "1|a|IN|integer|int4|NULL",
        "2|b|IN|integer|int4|17",
    ];
    let new = [
        "snapshot_routine_after|FUNCTION|integer|SQL|SQL|NO|NO|INVOKER|f",
        "1|a|IN|integer|int4|NULL",
        "2|b|IN|integer|int4|23",
    ];
    for isolation in ["REPEATABLE READ", "READ COMMITTED"] {
        let config = test_config("snapshot-routine-reports");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(
            &mut engine,
            "
            CREATE ROLE snapshot_routine_reader;
            CREATE FUNCTION snapshot_routine_before(a integer,b integer DEFAULT 17)
              RETURNS integer LANGUAGE SQL IMMUTABLE STRICT SECURITY DEFINER AS 'SELECT a+b';
        ",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                &format!("BEGIN ISOLATION LEVEL {isolation}; {report}"),
                true
            )),
            old
        );
        writer.success(&mut engine, "
            CREATE OR REPLACE FUNCTION snapshot_routine_before(a integer,b integer DEFAULT 23)
              RETURNS integer LANGUAGE SQL VOLATILE CALLED ON NULL INPUT SECURITY INVOKER AS 'SELECT a-b';
            ALTER FUNCTION snapshot_routine_before(integer,integer) RENAME TO snapshot_routine_after;
        ", true);
        let selected = reader.success(&mut engine, report, true);
        let expected = if isolation == "REPEATABLE READ" {
            [old[0], old[1], new[2]]
        } else {
            new
        };
        assert_eq!(
            data_rows(&selected),
            expected,
            "{isolation}: {}",
            String::from_utf8_lossy(&selected)
        );
        reader.success(
            &mut engine,
            "COMMIT; SET ROLE snapshot_routine_reader",
            true,
        );
        assert_eq!(data_rows(&reader.success(&mut engine, "SELECT routine_name,routine_definition IS NULL FROM information_schema.routines WHERE routine_name='snapshot_routine_after'", true)), ["snapshot_routine_after|t"]);
        writer.success(
            &mut engine,
            "REVOKE EXECUTE ON FUNCTION snapshot_routine_after(integer,integer) FROM PUBLIC",
            true,
        );
        assert_eq!(data_rows(&reader.success(&mut engine, "SELECT count(*) FROM information_schema.routines WHERE routine_name='snapshot_routine_after'; SELECT count(*) FROM information_schema.parameters WHERE specific_schema='public' AND specific_name LIKE 'snapshot_routine_%'", true)), ["0", "0"]);
        writer.success(&mut engine, "GRANT EXECUTE ON FUNCTION snapshot_routine_after(integer,integer) TO snapshot_routine_reader", true);
        assert_eq!(data_rows(&reader.success(&mut engine, "SELECT routine_name,routine_definition IS NULL FROM information_schema.routines WHERE routine_name='snapshot_routine_after'", true)), ["snapshot_routine_after|t"]);
    }
}

const SETUP: &str = "
    CREATE TABLE derived_source(id integer, payload text);
    CREATE VIEW derived_view AS SELECT id, payload FROM derived_source WITH LOCAL CHECK OPTION;
    CREATE MATERIALIZED VIEW derived_materialized AS SELECT id FROM derived_source WITH NO DATA;
    CREATE RULE derived_rule AS ON INSERT TO derived_source DO ALSO NOTHING;
    CREATE SEQUENCE derived_sequence AS integer START 3 INCREMENT 2;
";

const REPORT: &str = "
    SELECT viewname FROM pg_views WHERE viewname IN ('derived_view','derived_renamed_view');
    SELECT table_name, check_option FROM information_schema.views
      WHERE table_name IN ('derived_view','derived_renamed_view');
    SELECT matviewname,hasindexes,ispopulated FROM pg_matviews
      WHERE matviewname IN ('derived_materialized','derived_renamed_materialized');
    SELECT rulename,tablename FROM pg_rules WHERE rulename IN ('derived_rule','derived_renamed_rule');
    SELECT sequencename,increment_by,pg_typeof(data_type)::text FROM pg_sequences
      WHERE sequencename IN ('derived_sequence','derived_renamed_sequence');
    SELECT sequence_name, increment FROM information_schema.sequences
      WHERE sequence_name IN ('derived_sequence','derived_renamed_sequence');
    SELECT view_name,table_name FROM information_schema.view_table_usage
      WHERE view_name IN ('derived_view','derived_renamed_view');
    SELECT view_name,table_name,column_name FROM information_schema.view_column_usage
      WHERE view_name IN ('derived_view','derived_renamed_view') ORDER BY column_name;
";

const OLD: [&str; 9] = [
    "derived_view",
    "derived_view|LOCAL",
    "derived_materialized|f|f",
    "derived_rule|derived_source",
    "derived_sequence|2|regtype",
    "derived_sequence|2",
    "derived_view|derived_source",
    "derived_view|derived_source|id",
    "derived_view|derived_source|payload",
];

const NEW: [&str; 9] = [
    "derived_renamed_view",
    "derived_renamed_view|CASCADED",
    "derived_renamed_materialized|t|f",
    "derived_renamed_rule|derived_renamed_source",
    "derived_renamed_sequence|5|regtype",
    "derived_renamed_sequence|5",
    "derived_renamed_view|derived_renamed_source",
    "derived_renamed_view|derived_renamed_source|id",
    "derived_renamed_view|derived_renamed_source|renamed_payload",
];

const CHANGE: &str = "
    ALTER VIEW derived_view RENAME TO derived_renamed_view;
    ALTER VIEW derived_renamed_view SET (check_option=cascaded);
    ALTER MATERIALIZED VIEW derived_materialized RENAME TO derived_renamed_materialized;
    CREATE INDEX derived_materialized_index ON derived_renamed_materialized(id);
    ALTER RULE derived_rule ON derived_source RENAME TO derived_renamed_rule;
    ALTER TABLE derived_source RENAME TO derived_renamed_source;
    CREATE TABLE derived_source(id integer, payload text);
    ALTER TABLE derived_renamed_source RENAME payload TO renamed_payload;
    INSERT INTO derived_renamed_source VALUES(42,'retained');
    INSERT INTO derived_source VALUES(99,'replacement');
    ALTER SEQUENCE derived_sequence RENAME TO derived_renamed_sequence;
    ALTER SEQUENCE derived_renamed_sequence INCREMENT 5;
";

#[test]
fn catalog_snapshot_derived_reports_preserve_rows_without_freezing_current_values() {
    for (isolation, retained) in [("REPEATABLE READ", OLD), ("READ COMMITTED", NEW)] {
        let config = test_config("derived-catalog-reports");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        let initial = reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL {isolation}; {REPORT}"),
            true,
        );
        assert_eq!(data_rows(&initial), OLD);
        writer.success(&mut engine, CHANGE, true);
        assert_eq!(
            data_rows(&writer.success(&mut engine, "SELECT * FROM derived_renamed_view", true)),
            ["42|retained"]
        );
        writer.success(
            &mut engine,
            "SELECT nextval('derived_renamed_sequence')",
            true,
        );
        let selected = reader.success(&mut engine, REPORT, true);
        assert_eq!(
            data_rows(&selected),
            retained,
            "{isolation}: {}",
            String::from_utf8_lossy(&selected)
        );
        let live = reader.success(&mut engine,
            "SELECT last_value FROM pg_sequences WHERE sequencename IN ('derived_sequence','derived_renamed_sequence');
             SELECT to_regclass('derived_view') IS NULL, to_regclass('derived_renamed_view') IS NOT NULL;
             COMMIT;", true);
        assert_eq!(data_rows(&live), ["3", "t|t"]);
        assert_eq!(data_rows(&reader.success(&mut engine, REPORT, true)), NEW);
    }
}

#[test]
fn catalog_snapshot_derived_reports_keep_private_edits_and_savepoint_rollback() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config("private-derived-catalog-reports");
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut owner =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut observer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        owner.success(&mut engine, SETUP, true);
        owner.success(&mut engine, "BEGIN; SAVEPOINT report_change", true);
        owner.success(&mut engine, CHANGE, true);
        assert_eq!(data_rows(&owner.success(&mut engine, REPORT, true)), NEW);
        assert_eq!(data_rows(&observer.success(&mut engine, REPORT, true)), OLD);
        owner.success(&mut engine, "ROLLBACK TO report_change", true);
        assert_eq!(data_rows(&owner.success(&mut engine, REPORT, true)), OLD);
        owner.success(&mut engine, CHANGE, true);
        owner.success(&mut engine, "ROLLBACK", true);
        assert_eq!(data_rows(&observer.success(&mut engine, REPORT, true)), OLD);
    }
}

#[test]
fn catalog_snapshot_derived_usage_is_distinct_and_checks_referenced_ownership() {
    let config = test_config("derived-catalog-privileges");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "
        CREATE ROLE derived_reader;
        CREATE TABLE derived_private(id integer, payload text);
        CREATE VIEW derived_repeated AS SELECT a.id, b.id AS other, a.payload
          FROM derived_private a JOIN derived_private b ON a.id=b.id;
        CREATE SEQUENCE derived_hidden_sequence;
        SELECT nextval('derived_hidden_sequence');
        GRANT SELECT ON derived_repeated TO derived_reader;
    ",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT count(*) FROM information_schema.view_table_usage WHERE view_name='derived_repeated';
         SELECT column_name FROM information_schema.view_column_usage WHERE view_name='derived_repeated' ORDER BY column_name;
         SET ROLE derived_reader;
         SELECT table_name,view_definition IS NULL FROM information_schema.views WHERE table_name='derived_repeated';
         SELECT count(*) FROM information_schema.view_table_usage WHERE view_name='derived_repeated';
         SELECT count(*) FROM information_schema.view_column_usage WHERE view_name='derived_repeated';
         SELECT count(*) FROM information_schema.sequences WHERE sequence_name='derived_hidden_sequence';
         SELECT last_value IS NULL FROM pg_sequences WHERE sequencename='derived_hidden_sequence';
         RESET ROLE;", true)),
        ["1", "id", "payload", "derived_repeated|t", "0", "0", "0", "t"]);
}

#[test]
fn catalog_snapshot_derived_reports_survive_prepared_decisions_and_empty_disk_recovery() {
    // SQL ORDER BY writes simulator fixture objects. Sort the client results
    // outside the guard so the complete metadata reads still forbid allocation.
    let report = REPORT.replace(" ORDER BY column_name", "");
    let read = |session: &mut ConfiguredTransactionSession, engine: &mut Engine| {
        let mut rows = data_rows(&session.success(engine, &report, true));
        rows.sort_unstable();
        rows
    };
    let mut old = OLD.map(str::to_owned);
    old.sort_unstable();
    let mut new = NEW.map(str::to_owned);
    new.sort_unstable();
    for checkpoint in [false, true] {
        for decision in ["COMMIT", "COMMIT PREPARED", "ROLLBACK PREPARED"] {
            let name = format!("derived-recovery-{checkpoint}-{decision}");
            let mut config = test_config(&name);
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
            // Simulator uploads own growing fixture vectors; execution and
            // metadata reads remain under the production allocation guard.
            session.success(&mut engine, SETUP, false);
            session.success(&mut engine, "BEGIN", true);
            session.success(&mut engine, CHANGE, true);
            assert_eq!(read(&mut session, &mut engine), new);
            if decision == "COMMIT" {
                session.success(&mut engine, "COMMIT", false);
            } else {
                session.success(&mut engine, "PREPARE TRANSACTION 'derived-reports'", false);
                assert_eq!(read(&mut session, &mut engine), old);
            }
            if checkpoint {
                assert!(engine.checkpoint().unwrap());
            }
            drop(session);
            drop(engine);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut cold = Engine::new(&config, &mut cold_budget)
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            if decision != "COMMIT" {
                assert_eq!(read(&mut session, &mut cold), old);
                session.success(&mut cold, &format!("{decision} 'derived-reports'"), false);
            }
            assert_eq!(
                read(&mut session, &mut cold).as_slice(),
                if decision == "ROLLBACK PREPARED" {
                    old.as_slice()
                } else {
                    new.as_slice()
                },
                "{name}"
            );
            drop(session);
            drop(cold);
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            std::fs::remove_dir_all(&config.data_dir).unwrap();
        }
    }
}

#[test]
fn catalog_snapshot_derived_builtins_return_postgresql_definitions() {
    let (mut engine, budget) = test_engine();
    let result = run_with_fixed_memory(&mut engine, &budget,
        "SELECT viewname,definition IS NOT NULL,definition=pg_get_viewdef(('pg_catalog.'||viewname)::regclass)
           FROM pg_views WHERE schemaname='pg_catalog' AND viewname IN ('pg_views','pg_matviews','pg_sequences') ORDER BY viewname", 4 << 20);
    assert_eq!(
        data_rows(&result),
        ["pg_matviews|t|t", "pg_sequences|t|t", "pg_views|t|t"]
    );
}

#[test]
fn catalog_snapshot_privilege_lists_accept_any_grant_and_validate_every_item() {
    let config = test_config("catalog-privilege-lists");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE list_reader;
         CREATE TABLE list_table(id integer);
         CREATE SEQUENCE list_sequence;
         GRANT SELECT ON list_table TO list_reader;
         GRANT UPDATE ON SEQUENCE list_sequence TO list_reader;
         GRANT SET ON PARAMETER track_functions TO list_reader;
         SET ROLE list_reader",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT has_table_privilege('list_table','SELECT, INSERT'),
                has_column_privilege('list_table','id','UPDATE, SELECT'),
                has_any_column_privilege('list_table','SELECT, UPDATE'),
                has_sequence_privilege('list_sequence','SELECT, UPDATE, USAGE'),
                has_parameter_privilege('track_functions','SET, ALTER SYSTEM'),
                has_table_privilege('list_table','SELECT WITH GRANT OPTION, INSERT');
         SELECT sequence_name FROM information_schema.sequences WHERE sequence_name='list_sequence';
         SELECT last_value IS NULL FROM pg_sequences WHERE sequencename='list_sequence';
         SELECT has_table_privilege('pg_shadow','SELECT'),has_table_privilege('pg_views','SELECT'),has_table_privilege('pg_authid','SELECT')", true)),
        ["t|t|t|t|t|f", "list_sequence", "t", "f|t|f"]);
    for sql in [
        "SELECT has_table_privilege('list_table','SELECT, USAGE')",
        "SELECT has_table_privilege('list_table','ALL')",
        "SELECT has_column_privilege('list_table','id','SELECT, DELETE')",
        "SELECT has_sequence_privilege('list_sequence','UPDATE, INSERT')",
        "SELECT has_parameter_privilege('track_functions','SET, ALL')",
        "SELECT has_table_privilege('list_table','🦀🦀🦀🦀🦀')",
    ] {
        let output = session.execute(&mut engine, sql, true);
        assert_eq!(
            error_sqlstates(&output),
            ["22023"],
            "{sql}: {}",
            String::from_utf8_lossy(&output)
        );
    }
}

#[test]
fn catalog_snapshot_derived_permissions_keep_oid_identity_outside_the_search_path() {
    let config = test_config("catalog-permission-identities");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    engine.commit_progress = CommitProgressMode::Reactor;
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE identity_reader;
         CREATE SCHEMA identity_other;
         CREATE VIEW public.identity_collision AS SELECT 1 AS value;
         CREATE VIEW identity_other.identity_collision AS SELECT 2 AS value;
         CREATE SEQUENCE public.identity_sequence START 4;
         CREATE SEQUENCE identity_other.identity_sequence START 9;
         SELECT nextval('public.identity_sequence'),nextval('identity_other.identity_sequence');
         GRANT SELECT ON identity_other.identity_collision TO identity_reader;
         GRANT USAGE ON SEQUENCE identity_other.identity_sequence TO identity_reader;
         SET ROLE identity_reader;",
        true,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT n.nspname,has_table_privilege(c.oid,'SELECT, INSERT'),
                has_any_column_privilege(c.oid,'SELECT, UPDATE'),
                has_column_privilege(c.oid,1::smallint,'SELECT')
           FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
          WHERE c.relname='identity_collision' ORDER BY n.nspname;
         SELECT table_schema,table_name FROM information_schema.views WHERE table_name='identity_collision';
         SELECT table_schema,table_name FROM information_schema.tables WHERE table_name='identity_collision';
         SELECT table_schema,column_name FROM information_schema.columns WHERE table_name='identity_collision';
         SELECT sequence_schema,sequence_name FROM information_schema.sequences WHERE sequence_name='identity_sequence';
         SELECT schemaname,last_value FROM pg_sequences WHERE sequencename='identity_sequence' ORDER BY schemaname;", true)),
        ["identity_other|t|t|t", "public|f|f|f", "identity_other|identity_collision", "identity_other|identity_collision",
         "identity_other|value", "identity_other|identity_sequence", "identity_other|9", "public|NULL"]);
}

#[test]
fn catalog_snapshot_derived_arena_exhaustion_leaves_reports_usable() {
    let (mut engine, mut budget) = test_engine();
    let setup = run_with(&mut engine, &mut budget, SETUP);
    assert!(
        error_sqlstates(&setup).is_empty(),
        "{}",
        String::from_utf8_lossy(&setup)
    );
    let mut small = Arena::new(&mut budget, "small derived catalog arena", 16 << 10).unwrap();
    for (namespace, name) in [
        ("pg_catalog", "pg_views"),
        ("pg_catalog", "pg_matviews"),
        ("pg_catalog", "pg_rules"),
        ("pg_catalog", "pg_sequences"),
        ("information_schema", "views"),
        ("information_schema", "sequences"),
        ("information_schema", "view_table_usage"),
        ("information_schema", "view_column_usage"),
    ] {
        small.reset();
        let result = crate::mem::guard::forbid_alloc(|| {
            super::super::catalog::synthesize(
                &engine.storage,
                Some(namespace),
                name,
                0,
                crate::sql::catalog_snapshot::CatalogScan::Sequential,
                &small,
            )
        });
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("{namespace}.{name} exceeded its arena"),
        };
        assert_eq!(
            error.sqlstate,
            crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED
        );
        assert!(
            error.message.as_str().contains("arena"),
            "{}",
            error.message
        );
    }
    assert_eq!(
        data_rows(&run_with_fixed_memory(
            &mut engine,
            &budget,
            REPORT,
            4 << 20
        )),
        OLD
    );
}

#[test]
fn catalog_snapshot_definition_text_retains_rule_rows_while_view_execution_is_current() {
    for (isolation, replaced) in [
        ("READ COMMITTED", "t|t|t|t"),
        ("REPEATABLE READ", "f|f|f|f"),
    ] {
        let config = test_config("catalog-definition-rows");
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        engine.commit_progress = CommitProgressMode::Reactor;
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine,
            "CREATE VIEW definition_view AS SELECT 1 AS value;
             CREATE TABLE definition_target(id integer);
             CREATE TABLE definition_audit(id integer);
             CREATE RULE definition_rule AS ON INSERT TO definition_target DO ALSO INSERT INTO definition_audit VALUES(1)", true);
        let query = "SELECT
            (SELECT position('2' IN definition)>0 FROM pg_views WHERE viewname='definition_view'),
            position('2' IN pg_get_viewdef('definition_view'::regclass))>0,
            (SELECT position('2' IN definition)>0 FROM pg_rules WHERE rulename='definition_rule'),
            (SELECT position('2' IN pg_get_ruledef(oid))>0 FROM pg_rewrite WHERE rulename='definition_rule')";
        assert_eq!(
            data_rows(&writer.success(&mut engine, query, true)),
            ["f|f|f|f"]
        );
        assert_eq!(data_rows(&reader.success(&mut engine, &format!("BEGIN ISOLATION LEVEL {isolation}; SELECT count(*) FROM pg_rewrite WHERE rulename='definition_rule'"), true)), ["1"]);
        writer.success(&mut engine,
            "CREATE OR REPLACE VIEW definition_view AS SELECT 2 AS value;
             CREATE OR REPLACE RULE definition_rule AS ON INSERT TO definition_target DO ALSO INSERT INTO definition_audit VALUES(2)", true);
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            [replaced]
        );
        reader.success(
            &mut engine,
            "CREATE OR REPLACE VIEW definition_view AS SELECT 3 AS value",
            true,
        );
        assert_eq!(data_rows(&reader.success(&mut engine,
            "SELECT position('3' IN definition)>0 FROM pg_views WHERE viewname='definition_view';
             SELECT position('3' IN pg_get_viewdef('definition_view'::regclass))>0", true)),
            if isolation == "REPEATABLE READ" { ["f", "f"] } else { ["t", "t"] });
        assert_eq!(
            data_rows(&reader.success(
                &mut engine,
                "SELECT * FROM definition_view; ROLLBACK",
                true
            )),
            ["3"]
        );
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            ["t|t|t|t"]
        );
        for sql in [
            "SELECT _pos3ql_view_definition FROM pg_rewrite",
            "SELECT _pos3ql_rule_body FROM pg_rewrite",
        ] {
            assert_eq!(
                error_sqlstates(&reader.execute(&mut engine, sql, true)),
                ["42703"]
            );
        }
        let oid = data_rows(&writer.success(
            &mut engine,
            "SELECT oid FROM pg_class WHERE relname='definition_view'",
            true,
        ))
        .pop()
        .unwrap();
        reader.success(&mut engine, &format!("BEGIN ISOLATION LEVEL {isolation}; SELECT oid FROM pg_class WHERE relname='definition_view'"), true);
        writer.success(&mut engine, "DROP VIEW definition_view", true);
        for sql in [
            format!("SELECT pg_get_viewdef({oid}::oid) IS NOT NULL"),
            "SELECT definition IS NOT NULL FROM pg_views WHERE viewname='definition_view'"
                .to_owned(),
        ] {
            reader.success(&mut engine, "SAVEPOINT dropped_definition", true);
            let output = reader.execute(&mut engine, &sql, true);
            if isolation == "REPEATABLE READ" {
                assert_eq!(error_sqlstates(&output), ["XX000"]);
                reader.success(&mut engine, "ROLLBACK TO dropped_definition", true);
            } else {
                assert!(error_sqlstates(&output).is_empty());
                assert_eq!(
                    data_rows(&output),
                    if sql.starts_with("SELECT pg_get_viewdef") {
                        vec!["f"]
                    } else {
                        vec![]
                    }
                );
            }
            reader.success(&mut engine, "RELEASE dropped_definition", true);
        }
        reader.success(&mut engine, "ROLLBACK", true);
    }
}

#[test]
fn catalog_snapshot_builtin_metadata_keeps_objects_with_unsupported_row_reads() {
    let (mut engine, budget) = test_engine();
    assert_eq!(
        data_rows(&run_with_fixed_memory(
            &mut engine,
            &budget,
            "SELECT to_regclass('pg_statistic') IS NOT NULL,
                EXISTS(SELECT 1 FROM pg_class WHERE oid='pg_statistic'::regclass);
         SELECT attname FROM pg_attribute WHERE attrelid='pg_statistic'::regclass AND attnum=1",
            4 << 20
        )),
        ["t|t", "starelid"]
    );
    assert_eq!(
        error_sqlstates(&run_with_fixed_memory(
            &mut engine,
            &budget,
            "SELECT * FROM pg_statistic",
            4 << 20
        )),
        ["0A000"]
    );
}

#[test]
fn catalog_snapshot_index_hints_follow_existing_or_private_catalog_tuples() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for forked in [false, true] {
            let mut config = test_config("catalog-private-index-hints");
            config.transaction_mode = mode;
            let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 29));
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            engine.commit_progress = CommitProgressMode::Reactor;
            let mut owner =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
            let mut observer =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
            owner.success(
                &mut engine,
                "CREATE TABLE private_hint(id integer); BEGIN; SAVEPOINT hint_start",
                true,
            );
            let query = "SELECT relhasindex FROM pg_class WHERE relname='private_hint'";
            for rolled_back in [true, false] {
                if forked {
                    owner.success(&mut engine, "ALTER TABLE private_hint RENAME TO private_hint_away; ALTER TABLE private_hint_away RENAME TO private_hint", true);
                }
                owner.success(&mut engine, "CREATE INDEX private_hint_index ON private_hint(id); DROP INDEX private_hint_index", true);
                assert_eq!(data_rows(&owner.success(&mut engine, query, true)), ["t"]);
                assert_eq!(
                    data_rows(&observer.success(&mut engine, query, true)),
                    [if forked { "f" } else { "t" }]
                );
                owner.success(
                    &mut engine,
                    if rolled_back {
                        "ROLLBACK TO hint_start"
                    } else {
                        "COMMIT"
                    },
                    true,
                );
                assert_eq!(
                    data_rows(&observer.success(&mut engine, query, true)),
                    [if forked && rolled_back { "f" } else { "t" }]
                );
            }
            owner.success(&mut engine, "VACUUM private_hint", true);
            assert_eq!(
                data_rows(&observer.success(&mut engine, query, true)),
                ["f"]
            );
        }
    }
}
