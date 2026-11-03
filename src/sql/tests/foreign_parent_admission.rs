use super::*;

#[test]
fn foreign_child_catalog_edits_wait_for_the_parent_before_recording_effects() {
    for (parent_edit, child_edit) in [
        (
            "ALTER FOREIGN DATA WRAPPER parent_wrapper OPTIONS(ADD marker 'pending')",
            "CREATE SERVER waiting_server FOREIGN DATA WRAPPER parent_wrapper",
        ),
        (
            "ALTER FOREIGN DATA WRAPPER parent_wrapper OPTIONS(ADD marker 'pending')",
            "ALTER SERVER parent_server VERSION 'later'",
        ),
        (
            "ALTER SERVER parent_server OPTIONS(ADD host '127.0.0.1')",
            "CREATE USER MAPPING FOR PUBLIC SERVER parent_server",
        ),
        (
            "ALTER SERVER parent_server OPTIONS(ADD host '127.0.0.1')",
            "ALTER USER MAPPING FOR CURRENT_USER SERVER parent_server OPTIONS(ADD user 'later')",
        ),
        (
            "ALTER SERVER parent_server OPTIONS(ADD host '127.0.0.1')",
            "CREATE FOREIGN TABLE waiting_table(id integer) SERVER parent_server",
        ),
        (
            "ALTER SERVER parent_server OPTIONS(ADD host '127.0.0.1')",
            "ALTER FOREIGN TABLE parent_existing OPTIONS(SET table_name 'later')",
        ),
    ] {
        let mut config = test_config(&format!(
            "foreign-parent-admission-{}",
            child_edit
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join("-")
        ));
        config.max_connections = 2;
        config.max_tables = 2;
        config.max_foreign_data_wrappers = 1;
        config.max_foreign_servers = 2;
        config.max_user_mappings = 2;
        config.max_ddl_per_transaction = 16;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut blocker =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(11);
        let mut writer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(12);
        blocker.success(&mut engine,
            "CREATE FOREIGN DATA WRAPPER parent_wrapper; CREATE SERVER parent_server FOREIGN DATA WRAPPER parent_wrapper; \
             CREATE USER MAPPING FOR CURRENT_USER SERVER parent_server; \
             CREATE FOREIGN TABLE parent_existing(id integer) SERVER parent_server OPTIONS(table_name 'source')", true);
        blocker.success(&mut engine, &format!("BEGIN; {parent_edit}"), true);
        writer.send.clear();
        writer.arena.reset();
        let first = crate::mem::guard::forbid_alloc(|| {
            engine.execute_simple_from(
                child_edit,
                0,
                &writer.arena,
                &mut writer.transaction,
                &mut writer.pool,
                &mut writer.cursors,
                &writer.guc,
                &mut Responder::new(&mut writer.send),
                writer.connection_id,
                false,
            )
        })
        .unwrap();
        assert!(
            matches!(
                first,
                ExecutionStatus::Blocked {
                    wait: StatementWait::Lock,
                    completed_statements: 0,
                    ..
                }
            ),
            "{child_edit}: {first:?}, {}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert!(writer.transaction.ddl().is_empty());
        assert!(writer.transaction.touched().is_empty());
        assert!(writer.send.readable().is_empty());
        blocker.success(&mut engine, "ROLLBACK", true);
        writer.arena.reset();
        let completed = crate::mem::guard::forbid_alloc(|| {
            engine.execute_simple_from(
                child_edit,
                0,
                &writer.arena,
                &mut writer.transaction,
                &mut writer.pool,
                &mut writer.cursors,
                &writer.guc,
                &mut Responder::new(&mut writer.send),
                writer.connection_id,
                false,
            )
        })
        .unwrap();
        assert_eq!(completed, ExecutionStatus::Complete);
        assert!(
            error_sqlstates(writer.send.readable()).is_empty(),
            "{child_edit}: {}",
            String::from_utf8_lossy(writer.send.readable())
        );
        assert_eq!(
            message_types(writer.send.readable())
                .iter()
                .filter(|&&kind| kind == b'C')
                .count(),
            1
        );
        assert!(engine.fatal_commit_failure().is_none());
    }
}
