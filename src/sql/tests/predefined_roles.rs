use super::*;

/// Primary messages of the error (`E`) or notice (`N`) responses in `bytes`.
fn response_messages(bytes: &[u8], tag: u8) -> Vec<String> {
    let mut messages = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(bytes[offset + 1..offset + 5].try_into().unwrap()) as usize;
        if bytes[offset] == tag {
            let mut fields = &bytes[offset + 5..offset + 1 + length];
            while let Some((&field, remaining)) = fields.split_first() {
                if field == 0 {
                    break;
                }
                let end = remaining.iter().position(|&byte| byte == 0).unwrap();
                if field == b'M' {
                    messages.push(String::from_utf8(remaining[..end].to_vec()).unwrap());
                }
                fields = &remaining[end + 1..];
            }
        }
        offset += 1 + length;
    }
    messages
}

#[test]
fn predefined_roles_exist_reject_changes_and_grant_their_privileges() {
    let config = test_config("predefined-roles");
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 28));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    // PostgreSQL 18.6 lists the bootstrap superuser, then the predefined
    // roles in pg_authid.dat order.
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT string_agg(oid || ':' || rolname, ',') FROM pg_authid"
        )),
        [
            "10:postgres,6171:pg_database_owner,6181:pg_read_all_data,6182:pg_write_all_data,\
          3373:pg_monitor,3374:pg_read_all_settings,3375:pg_read_all_stats,\
          3377:pg_stat_scan_tables,4569:pg_read_server_files,4570:pg_write_server_files,\
          4571:pg_execute_server_program,4200:pg_signal_backend,4544:pg_checkpoint,\
          6337:pg_maintain,4550:pg_use_reserved_connections,6304:pg_create_subscription,\
          6392:pg_signal_autovacuum_worker"
        ]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT oid, roleid::regrole, grantor::regrole, admin_option, inherit_option, set_option \
             FROM pg_auth_members WHERE member = 'pg_monitor'::regrole ORDER BY oid"
        )),
        [
            "10308|pg_read_all_settings|postgres|f|t|t",
            "10309|pg_read_all_stats|postgres|f|t|t",
            "10310|pg_stat_scan_tables|postgres|f|t|t"
        ]
    );
    for (statement, state, message) in [
        (
            "CREATE ROLE pg_custom",
            "42939",
            "role name \"pg_custom\" is reserved",
        ),
        (
            "ALTER ROLE pg_monitor LOGIN",
            "42939",
            "role name \"pg_monitor\" is reserved",
        ),
        (
            "ALTER ROLE pg_missing SET work_mem = '1MB'",
            "42939",
            "role name \"pg_missing\" is reserved",
        ),
        (
            "ALTER ROLE pg_monitor RENAME TO monitor",
            "42939",
            "role name \"pg_monitor\" is reserved",
        ),
        (
            "DROP ROLE pg_monitor",
            "2BP01",
            "cannot drop role pg_monitor because it is required by the database system",
        ),
        (
            "CREATE ROLE member_of_owner IN ROLE pg_database_owner",
            "0A000",
            "role \"pg_database_owner\" cannot have explicit members",
        ),
        (
            "GRANT pg_monitor TO pg_database_owner",
            "0A000",
            "role \"pg_database_owner\" cannot be a member of any role",
        ),
    ] {
        let output = run_with(&mut engine, &mut budget, statement);
        assert_eq!(error_sqlstates(&output), [state], "{statement}");
        assert_eq!(response_messages(&output, b'E'), [message], "{statement}");
    }
    let setup = run_with(
        &mut engine,
        &mut budget,
        "CREATE ROLE reader; CREATE ROLE writer; CREATE ROLE keeper; \
         CREATE SCHEMA closed; CREATE TABLE closed.t(a integer); \
         INSERT INTO closed.t VALUES (1); CREATE SEQUENCE closed.s",
    );
    assert!(error_sqlstates(&setup).is_empty());
    let denied = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE reader; SELECT a FROM closed.t",
    );
    assert_eq!(error_sqlstates(&denied), ["42501"]);
    let granted = run_with(
        &mut engine,
        &mut budget,
        "GRANT pg_read_all_data TO reader; GRANT pg_write_all_data TO writer; \
         GRANT pg_maintain, pg_checkpoint TO keeper",
    );
    assert!(error_sqlstates(&granted).is_empty());
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SET ROLE reader; SELECT a FROM closed.t; SELECT last_value FROM closed.s; \
             SELECT has_table_privilege('closed.t', 'SELECT'), \
             has_table_privilege('closed.t', 'INSERT'), has_schema_privilege('closed', 'USAGE')"
        )),
        ["1", "1", "t|f|t"]
    );
    let read_only = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE reader; INSERT INTO closed.t VALUES (2)",
    );
    assert_eq!(error_sqlstates(&read_only), ["42501"]);
    let written = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE writer; INSERT INTO closed.t VALUES (2); UPDATE closed.t SET a = 3; \
         DELETE FROM closed.t",
    );
    assert!(error_sqlstates(&written).is_empty());
    // Reading a column still needs SELECT.
    let filtered = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE writer; DELETE FROM closed.t WHERE a = 3",
    );
    assert_eq!(error_sqlstates(&filtered), ["42501"]);
    let truncate = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE writer; TRUNCATE closed.t",
    );
    assert_eq!(error_sqlstates(&truncate), ["42501"]);
    // ANALYZE warns and skips a table the role may not maintain (VACUUM,
    // which cannot run in this multi-statement string, is in the SQL corpus).
    let skipped = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE writer; ANALYZE closed.t",
    );
    assert!(error_sqlstates(&skipped).is_empty());
    assert_eq!(
        response_messages(&skipped, b'N'),
        ["permission denied to analyze \"t\", skipping it"]
    );
    let checkpoint = run_with(&mut engine, &mut budget, "SET ROLE writer; CHECKPOINT");
    assert_eq!(
        response_messages(&checkpoint, b'E'),
        ["permission denied to execute CHECKPOINT command"]
    );
    let lock = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE reader; BEGIN; LOCK TABLE closed.t IN EXCLUSIVE MODE",
    );
    assert_eq!(
        response_messages(&lock, b'E'),
        ["permission denied for table t"]
    );
    let maintained = run_with(
        &mut engine,
        &mut budget,
        "SET ROLE keeper; ANALYZE closed.t; CHECKPOINT; \
         BEGIN; LOCK TABLE closed.t; COMMIT",
    );
    assert!(error_sqlstates(&maintained).is_empty());
    assert!(response_messages(&maintained, b'N').is_empty());
    // pg_monitor holds initdb's EXECUTE grant on pg_ls_waldir().
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT has_function_privilege('reader', 'pg_ls_waldir()', 'EXECUTE'); \
             GRANT pg_monitor TO reader; \
             SELECT has_function_privilege('reader', 'pg_ls_waldir()', 'EXECUTE'), \
             pg_has_role('reader', 'pg_read_all_stats', 'USAGE'), \
             pg_has_role('reader', 'pg_monitor', 'MEMBER WITH ADMIN OPTION'); \
             SELECT proacl FROM pg_proc WHERE oid = 'pg_ls_waldir()'::regprocedure"
        )),
        ["f", "t|t|f", "{postgres=X/postgres,pg_monitor=X/postgres}"]
    );
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "GRANT EXECUTE ON FUNCTION pg_ls_waldir() TO writer; \
             SELECT proacl FROM pg_proc WHERE oid = 'pg_ls_waldir()'::regprocedure"
        )),
        ["{postgres=X/postgres,pg_monitor=X/postgres,writer=X/postgres}"]
    );
    // Objects owned by a pinned role have no pg_shdepend row.
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "ALTER TABLE closed.t OWNER TO pg_monitor; \
             SELECT count(*) FROM pg_shdepend WHERE refobjid = 'pg_monitor'::regrole"
        )),
        ["0"]
    );
}

#[test]
fn initdb_role_memberships_change_durably_through_wal_and_checkpoints() {
    let name = "initdb-role-memberships";
    let mut config = test_config(name);
    config.object_store_on = true;
    config.object_store_sim = true;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.object_store_bucket = format!("{name}-{}", std::process::id());
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let memberships = "SELECT string_agg(oid::text || ':' || roleid::regrole::text || ':' || \
                       admin_option::text, ',' ORDER BY roleid::regrole::text) \
                       FROM pg_auth_members WHERE member = 'pg_monitor'::regrole";
    let restart = |engine: Engine| {
        drop(engine);
        let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
        let engine = Engine::new(&config, &mut budget).unwrap();
        (engine, budget)
    };
    let mut budget = test_engine_budget(&config, test_engine_budget_bytes(1 << 27));
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let revoked = run_with(
        &mut engine,
        &mut budget,
        "REVOKE pg_read_all_stats FROM pg_monitor; \
         GRANT pg_stat_scan_tables TO pg_monitor WITH ADMIN OPTION",
    );
    assert!(error_sqlstates(&revoked).is_empty());
    let expected = ["10308:pg_read_all_settings:false,10310:pg_stat_scan_tables:true"];
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, memberships)),
        expected
    );
    // The journal alone restores the changed initdb rows.
    let (mut engine, mut budget) = restart(engine);
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, memberships)),
        expected
    );
    // So does a checkpoint, which lists only the remaining rows.
    assert!(engine.checkpoint().unwrap());
    let (mut engine, mut budget) = restart(engine);
    assert_eq!(
        data_rows(&run_with(&mut engine, &mut budget, memberships)),
        expected
    );
    // Granting a revoked initdb membership again creates a new row.
    let regranted = run_with(
        &mut engine,
        &mut budget,
        "GRANT pg_read_all_stats TO pg_monitor; \
         SELECT oid > 860000000 FROM pg_auth_members \
         WHERE roleid = 'pg_read_all_stats'::regrole",
    );
    assert_eq!(data_rows(&regranted), ["t"]);
    assert!(engine.checkpoint().unwrap());
    let (mut engine, mut budget) = restart(engine);
    assert_eq!(
        data_rows(&run_with(
            &mut engine,
            &mut budget,
            "SELECT count(*), pg_has_role('pg_monitor', 'pg_read_all_stats', 'USAGE') \
             FROM pg_auth_members WHERE member = 'pg_monitor'::regrole"
        )),
        ["3|t"]
    );
}
