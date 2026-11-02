use super::*;

#[test]
fn cold_prepared_object_and_foreign_catalogs_install_or_roll_back_private_images() {
    for commit in [false, true] {
        let name = format!("prepared-object-catalog-{commit}-{}", std::process::id());
        let package = std::env::temp_dir().join(format!("{name}-packages"));
        std::fs::create_dir_all(package.join("extension")).unwrap();
        std::fs::write(
            package.join("extension/resource_ext.control"),
            "default_version='1.0'\nrelocatable=true\n",
        )
        .unwrap();
        std::fs::write(package.join("extension/resource_ext--1.0.sql"),
            "CREATE TABLE extension_values(id integer); SELECT pg_catalog.pg_extension_config_dump('extension_values','');").unwrap();
        let mut config = test_config(&name);
        config.extension_control_path = package.to_string_lossy().into_owned();
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = name;
        config.max_tables = 2;
        config.row_overlay_slots = 8;
        config.txn_rows = 8;
        config.max_ddl_per_transaction = 32;
        config.max_prepared_transactions = 1;
        config.max_foreign_data_wrappers = 1;
        config.max_foreign_servers = 1;
        config.max_user_mappings = 1;
        config.work_arena_bytes = 4 << 20;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLESPACE prepared_space LOCATION '/object/prepared-space'",
            false,
        );
        session.success(&mut engine,
            "BEGIN; CREATE EXTENSION resource_ext; \
             CREATE ACCESS METHOD prepared_heap TYPE TABLE HANDLER heap_tableam_handler; \
             CREATE FOREIGN DATA WRAPPER prepared_wrapper HANDLER postgres_fdw_handler VALIDATOR postgres_fdw_validator; \
             CREATE SERVER prepared_server FOREIGN DATA WRAPPER prepared_wrapper OPTIONS(host '127.0.0.1'); \
             ALTER SERVER prepared_server VERSION 'v2'; \
             CREATE USER MAPPING FOR CURRENT_USER SERVER prepared_server OPTIONS(user 'source_reader'); \
             CREATE FOREIGN TABLE prepared_remote(id integer) SERVER prepared_server OPTIONS(schema_name 'public',table_name 'source'); \
             ALTER FOREIGN TABLE prepared_remote OPTIONS(SET table_name 'source_v2'); \
             ALTER DATABASE postgres CONNECTION LIMIT 9; \
             ALTER TABLESPACE prepared_space RENAME TO prepared_space_changed; \
             PREPARE TRANSACTION 'objects'", false);
        assert!(engine.checkpoint().unwrap());
        engine.commit_wal().unwrap();
        drop((session, engine));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        let mut resolver = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(
            data_rows(&resolver.success(
                &mut recovered,
                "SELECT count(*) FROM pg_extension WHERE extname='resource_ext'; \
             SELECT count(*) FROM pg_foreign_data_wrapper WHERE fdwname='prepared_wrapper'; \
             SELECT spcname FROM pg_tablespace WHERE spcname LIKE 'prepared_space%'; \
             SELECT datconnlimit FROM pg_database WHERE datname='postgres'",
                true
            )),
            ["0", "0", "prepared_space", "-1"]
        );
        resolver.success(
            &mut recovered,
            if commit {
                "COMMIT PREPARED 'objects'"
            } else {
                "ROLLBACK PREPARED 'objects'"
            },
            false,
        );
        if commit {
            assert_eq!(data_rows(&resolver.success(&mut recovered,
                "SELECT extname,array_length(extconfig,1) FROM pg_extension WHERE extname='resource_ext'; \
                 SELECT fdwname FROM pg_foreign_data_wrapper WHERE fdwname='prepared_wrapper'; \
                 SELECT srvversion FROM pg_foreign_server WHERE srvname='prepared_server'; \
                 SELECT umuser::regrole::text FROM pg_user_mapping; \
                 SELECT ftoptions::text FROM pg_foreign_table WHERE ftrelid='prepared_remote'::regclass; \
                 SELECT amname FROM pg_am WHERE amname='prepared_heap'; \
                 SELECT spcname FROM pg_tablespace WHERE spcname LIKE 'prepared_space%'; \
                 SELECT datconnlimit FROM pg_database WHERE datname='postgres'", true)),
                ["resource_ext|1", "prepared_wrapper", "v2", "postgres", "{schema_name=public,table_name=source_v2}", "prepared_heap", "prepared_space_changed", "9"]);
        } else {
            assert_eq!(
                data_rows(&resolver.success(
                    &mut recovered,
                    "SELECT count(*) FROM pg_extension WHERE extname='resource_ext'; \
                 SELECT count(*) FROM pg_foreign_data_wrapper WHERE fdwname='prepared_wrapper'; \
                 SELECT count(*) FROM pg_foreign_server WHERE srvname='prepared_server'; \
                 SELECT count(*) FROM pg_user_mapping; \
                 SELECT count(*) FROM pg_foreign_table; \
                 SELECT count(*) FROM pg_am WHERE amname='prepared_heap'; \
                 SELECT spcname FROM pg_tablespace WHERE spcname LIKE 'prepared_space%'; \
                 SELECT datconnlimit FROM pg_database WHERE datname='postgres'",
                    true
                )),
                ["0", "0", "0", "0", "0", "0", "prepared_space", "-1"]
            );
        }
        assert!(recovered.fatal_commit_failure().is_none());
        drop((resolver, recovered));
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(package).unwrap();
    }
}
