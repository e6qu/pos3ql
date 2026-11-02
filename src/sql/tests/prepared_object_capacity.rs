use super::*;

#[test]
fn cold_prepared_object_and_foreign_catalogs_install_or_roll_back_private_images() {
    for (prepared, commit) in [(true, false), (true, true), (false, true)] {
        let name = format!(
            "prepared-object-catalog-{prepared}-{commit}-{}",
            std::process::id()
        );
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
        config.max_foreign_data_wrappers = 3;
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
        session.success(
            &mut engine,
            "CREATE FOREIGN DATA WRAPPER cycle_left HANDLER postgres_fdw_handler; \
             CREATE FOREIGN DATA WRAPPER cycle_right",
            false,
        );
        let transaction_sql = "BEGIN; CREATE EXTENSION resource_ext; \
             CREATE ACCESS METHOD prepared_heap TYPE TABLE HANDLER heap_tableam_handler; \
             CREATE FOREIGN DATA WRAPPER prepared_wrapper HANDLER postgres_fdw_handler VALIDATOR postgres_fdw_validator; \
             CREATE SERVER prepared_server FOREIGN DATA WRAPPER prepared_wrapper OPTIONS(host '127.0.0.1'); \
             ALTER SERVER prepared_server VERSION 'v2'; \
             CREATE USER MAPPING FOR CURRENT_USER SERVER prepared_server OPTIONS(user 'source_reader'); \
             CREATE FOREIGN TABLE prepared_remote(id integer) SERVER prepared_server OPTIONS(schema_name 'public',table_name 'source'); \
             ALTER FOREIGN TABLE prepared_remote OPTIONS(SET table_name 'source_v2'); \
             ALTER FOREIGN DATA WRAPPER prepared_wrapper RENAME TO prepared_wrapper_new; \
             ALTER FOREIGN DATA WRAPPER cycle_left RENAME TO cycle_temporary; \
             ALTER FOREIGN DATA WRAPPER cycle_right RENAME TO cycle_left; \
             ALTER FOREIGN DATA WRAPPER cycle_temporary RENAME TO cycle_right; \
             ALTER DATABASE postgres CONNECTION LIMIT 9; \
             ALTER TABLESPACE prepared_space RENAME TO prepared_space_changed";
        let finish = if prepared {
            "PREPARE TRANSACTION 'objects'"
        } else {
            "COMMIT"
        };
        session.success(&mut engine, &format!("{transaction_sql}; {finish}"), false);
        if prepared {
            assert!(engine.checkpoint().unwrap());
        }
        engine.commit_wal().unwrap();
        drop((session, engine));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        let mut resolver = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        if prepared {
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
        }
        if commit {
            assert_eq!(data_rows(&resolver.success(&mut recovered,
                "SELECT extname,array_length(extconfig,1) FROM pg_extension WHERE extname='resource_ext'; \
                 SELECT fdwname FROM pg_foreign_data_wrapper WHERE fdwname='prepared_wrapper_new'; \
                 SELECT fdwname,fdwhandler=0::oid FROM pg_foreign_data_wrapper WHERE fdwname LIKE 'cycle_%' ORDER BY fdwname; \
                 SELECT srvversion FROM pg_foreign_server WHERE srvname='prepared_server'; \
                 SELECT umuser::regrole::text FROM pg_user_mapping; \
                 SELECT ftoptions::text FROM pg_foreign_table WHERE ftrelid='prepared_remote'::regclass; \
                 SELECT amname FROM pg_am WHERE amname='prepared_heap'; \
                 SELECT spcname FROM pg_tablespace WHERE spcname LIKE 'prepared_space%'; \
                 SELECT datconnlimit FROM pg_database WHERE datname='postgres'", true)),
                ["resource_ext|1", "prepared_wrapper_new", "cycle_left|t", "cycle_right|f", "v2", "postgres", "{schema_name=public,table_name=source_v2}", "prepared_heap", "prepared_space_changed", "9"]);
        } else {
            assert_eq!(
                data_rows(&resolver.success(
                    &mut recovered,
                    "SELECT count(*) FROM pg_extension WHERE extname='resource_ext'; \
                 SELECT count(*) FROM pg_foreign_data_wrapper WHERE fdwname='prepared_wrapper'; \
                 SELECT fdwname,fdwhandler=0::oid FROM pg_foreign_data_wrapper WHERE fdwname LIKE 'cycle_%' ORDER BY fdwname; \
                 SELECT count(*) FROM pg_foreign_server WHERE srvname='prepared_server'; \
                 SELECT count(*) FROM pg_user_mapping; \
                 SELECT count(*) FROM pg_foreign_table; \
                 SELECT count(*) FROM pg_am WHERE amname='prepared_heap'; \
                 SELECT spcname FROM pg_tablespace WHERE spcname LIKE 'prepared_space%'; \
                 SELECT datconnlimit FROM pg_database WHERE datname='postgres'",
                    true
                )),
                ["0", "0", "cycle_left|f", "cycle_right|t", "0", "0", "0", "0", "prepared_space", "-1"]
            );
        }
        assert!(recovered.fatal_commit_failure().is_none());
        drop((resolver, recovered));
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        std::fs::remove_dir_all(package).unwrap();
    }
}

#[test]
fn cold_prepared_role_renames_retain_original_and_target_name_reservations() {
    for commit in [false, true] {
        let name = format!("prepared-role-names-{commit}-{}", std::process::id());
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = name;
        config.max_roles = 4;
        config.max_prepared_transactions = 1;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(&mut engine,
            "BEGIN; CREATE ROLE prepared_original; ALTER ROLE prepared_original RENAME TO prepared_target; PREPARE TRANSACTION 'role_names'", false);
        assert!(engine.checkpoint().unwrap());
        engine.commit_wal().unwrap();
        drop((session, engine));
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for name in ["prepared_original", "prepared_target"] {
                assert_eq!(recovered.storage.find_role_visible(name, 0), None);
                let error = recovered
                    .storage
                    .create_role(
                        crate::storage::SqlName::parse(name).unwrap(),
                        crate::storage::RoleAttributes::ORDINARY,
                        900,
                    )
                    .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
            }
        });
        let mut resolver = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        resolver.success(
            &mut recovered,
            if commit {
                "COMMIT PREPARED 'role_names'"
            } else {
                "ROLLBACK PREPARED 'role_names'"
            },
            false,
        );
        assert_eq!(
            recovered
                .storage
                .find_role_visible("prepared_target", 0)
                .is_some(),
            commit
        );
        assert_eq!(
            recovered.storage.find_role_visible("prepared_original", 0),
            None
        );
        resolver.success(&mut recovered, "CREATE ROLE prepared_original", false);
        assert!(recovered.fatal_commit_failure().is_none());
        drop((resolver, recovered));
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}
