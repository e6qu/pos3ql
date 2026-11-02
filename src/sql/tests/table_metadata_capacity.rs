use super::*;

#[test]
fn rewritten_rows_keep_complete_index_caches_after_later_updates_and_prepared_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (prepared, cold, checkpoint) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let name = format!("metadata-rewrite-cache-{mode:?}-{prepared}-{cold}-{checkpoint}");
            let mut config = test_config(&name);
            config.transaction_mode = mode;
            config.max_tables = 1;
            config.max_prepared_transactions = 1;
            config.max_ddl_per_transaction = 16;
            config.row_overlay_slots = 32;
            config.txn_rows = 16;
            config.max_value_indexes = 4;
            config.value_index_rows = 32;
            config.work_arena_bytes = 1 << 20;
            config.object_store_on = cold;
            config.object_store_sim = cold;
            config.wal_upload = cold;
            config.wal_upload_sync = cold;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            if cold {
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            }
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            session.success(
                &mut engine,
                "CREATE TABLE rewritten_cache(id integer PRIMARY KEY,amount integer); \
                 INSERT INTO rewritten_cache VALUES(1,10),(2,20); \
                 BEGIN; ALTER TABLE rewritten_cache ALTER COLUMN amount TYPE bigint; \
                 UPDATE rewritten_cache SET id=11 WHERE id=1",
                !cold,
            );
            if prepared {
                session.success(&mut engine, "PREPARE TRANSACTION 'rewrite-cache'", !cold);
            } else {
                session.success(&mut engine, "COMMIT", true);
            }
            if cold {
                if checkpoint {
                    assert!(engine.checkpoint().unwrap());
                }
                engine.commit_wal().unwrap();
                drop((session, engine));
                std::fs::remove_dir_all(&config.data_dir).unwrap();
                let mut cold_budget = test_engine_budget(&config, 1 << 29);
                let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
                let mut resolver = ConfiguredTransactionSession::new(&config, &mut cold_budget);
                resolver.success(&mut recovered, "COMMIT PREPARED 'rewrite-cache'", false);
                assert_rewritten_cache(&mut recovered, &mut resolver, false);
                drop((resolver, recovered));
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            } else {
                if prepared {
                    session.success(&mut engine, "COMMIT PREPARED 'rewrite-cache'", true);
                }
                assert_rewritten_cache(&mut engine, &mut session, true);
            }
        }
    }
}

fn assert_rewritten_cache(
    engine: &mut Engine,
    session: &mut ConfiguredTransactionSession,
    forbid_alloc: bool,
) {
    assert_eq!(
        data_rows(&session.success(
            engine,
            "SELECT id,amount FROM rewritten_cache WHERE id=11",
            forbid_alloc,
        )),
        ["11|10"],
    );
    assert!(
        data_rows(&session.success(
            engine,
            "SELECT id FROM rewritten_cache WHERE id=1",
            forbid_alloc,
        ))
        .is_empty()
    );
    let table = engine
        .storage
        .find_table("public", "rewritten_cache")
        .unwrap();
    assert!(engine.storage.value_cache_complete(table, &[0]));
    engine.storage.set_commit_snapshot(u64::MAX);
    crate::mem::guard::forbid_alloc(|| {
        for (id, expected) in [(1, 0), (11, 1)] {
            let hash =
                crate::sql::eval::hash_key_catalog(&engine.storage, 0, &[Datum::Int4(id)], &[0])
                    .unwrap();
            let mut candidates = 0;
            assert!(
                engine
                    .storage
                    .probe_value(table, &[0], hash, |_, _| {
                        candidates += 1;
                        Ok(())
                    })
                    .unwrap()
            );
            assert_eq!(candidates, expected);
        }
    });
}

#[test]
fn commit_admission_waits_preserve_defaults_triggers_and_command_responses() {
    fn resume(
        engine: &mut Engine,
        session: &mut ConfiguredTransactionSession,
        text: &str,
        completed: usize,
        extended: bool,
        timeout: bool,
    ) -> ExecutionStatus {
        session.arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            let mut responder = Responder::new(&mut session.send);
            if extended {
                match engine
                    .execute_extended(
                        text,
                        &session.arena,
                        &[],
                        &[],
                        &mut session.transaction,
                        &mut session.pool,
                        &mut session.cursors,
                        &mut session.guc,
                        &mut responder,
                        session.connection_id,
                        timeout,
                    )
                    .unwrap()
                {
                    ExtendedExecutionStatus::Complete(_) => ExecutionStatus::Complete,
                    ExtendedExecutionStatus::Blocked { wait } => ExecutionStatus::Blocked {
                        completed_statements: 0,
                        output_mark: responder.buffer.mark(),
                        wait,
                    },
                }
            } else {
                engine
                    .execute_simple_from(
                        text,
                        completed,
                        &session.arena,
                        &mut session.transaction,
                        &mut session.pool,
                        &mut session.cursors,
                        &mut session.guc,
                        &mut responder,
                        session.connection_id,
                        timeout,
                    )
                    .unwrap()
            }
        })
    }
    fn message_count(mut bytes: &[u8], kind: u8) -> usize {
        let mut count = 0;
        while !bytes.is_empty() {
            let length = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize + 1;
            count += usize::from(bytes[0] == kind);
            bytes = &bytes[length..];
        }
        count
    }
    for (explicit, extended, prepare) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (true, false, true),
        (true, true, true),
    ] {
        for timeout in [false, true] {
            let name =
                format!("metadata-commit-admission-{explicit}-{extended}-{prepare}-{timeout}");
            let mut config = test_config(&name);
            config.max_tables = 3;
            config.max_connections = 3;
            config.max_sequences = 1;
            config.max_prepared_transactions = 1;
            config.max_routines = 1;
            config.row_overlay_slots = 16;
            config.txn_rows = 8;
            config.max_value_indexes = 4;
            config.value_index_rows = 16;
            config.max_ddl_per_transaction = 16;
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut writer =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(11);
            let mut blocker =
                ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(12);
            writer.success(&mut engine,
                "CREATE SEQUENCE commit_admission_calls; \
                 CREATE TABLE commit_admission_parent(id integer PRIMARY KEY DEFAULT nextval('commit_admission_calls')); \
                 CREATE TABLE commit_admission_child(parent_id integer REFERENCES commit_admission_parent(id)); \
                 CREATE TABLE commit_admission_audit(id integer); \
                 CREATE FUNCTION commit_admission_trigger() RETURNS trigger LANGUAGE plpgsql AS \
                 $$BEGIN INSERT INTO commit_admission_audit VALUES(nextval('commit_admission_calls')); RETURN NEW; END$$; \
                 CREATE CONSTRAINT TRIGGER commit_admission_deferred AFTER INSERT ON commit_admission_parent \
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION commit_admission_trigger()", true);
            blocker.success(
                &mut engine,
                "BEGIN; LOCK TABLE commit_admission_child IN ROW EXCLUSIVE MODE",
                true,
            );
            let effects = "INSERT INTO commit_admission_parent DEFAULT VALUES RETURNING id; ALTER TABLE commit_admission_parent RENAME TO commit_admission_renamed";
            if explicit {
                writer.success(&mut engine, "BEGIN", true);
                writer.success(&mut engine, effects, true);
            } else {
                writer.send.clear();
            }
            let query = if prepare {
                "PREPARE TRANSACTION 'admission'"
            } else if explicit {
                "COMMIT"
            } else {
                effects
            };
            let before_completion = writer.send.readable().to_vec();
            let first = resume(&mut engine, &mut writer, query, 0, extended, false);
            let ExecutionStatus::Blocked {
                completed_statements,
                output_mark,
                wait,
            } = first
            else {
                panic!(
                    "expected pre-WAL admission wait: {first:?}, {}",
                    String::from_utf8_lossy(writer.send.readable())
                );
            };
            assert_eq!(wait, StatementWait::Lock);
            assert_eq!(completed_statements, if explicit { 0 } else { 2 });
            writer.send.truncate_to(output_mark);
            if prepare {
                assert!(matches!(
                    writer.transaction.commit_admission(),
                    Some(txn::CommitTarget::Prepare { .. })
                ));
            } else {
                assert_eq!(
                    writer.transaction.commit_admission(),
                    Some(txn::CommitTarget::Ordinary)
                );
            }
            assert!(writer.transaction.staged_commit().is_none());
            let sequence = engine
                .storage
                .sequence_slot("public", "commit_admission_calls", writer.transaction.txid)
                .unwrap();
            assert_eq!(
                engine
                    .storage
                    .sequence_value_for(sequence, writer.transaction.txid),
                (2, true)
            );
            let prefix = writer.send.readable().to_vec();
            let deadline = writer.transaction.suspended_statement_deadline();
            for _ in 0..2 {
                assert!(matches!(
                    resume(
                        &mut engine,
                        &mut writer,
                        query,
                        completed_statements,
                        extended,
                        false
                    ),
                    ExecutionStatus::Blocked {
                        wait: StatementWait::Lock,
                        ..
                    }
                ));
                assert_eq!(writer.send.readable(), prefix);
                assert_eq!(writer.transaction.suspended_statement_deadline(), deadline);
                assert_eq!(
                    engine
                        .storage
                        .sequence_value_for(sequence, writer.transaction.txid),
                    (2, true)
                );
            }
            if timeout {
                assert_eq!(
                    resume(
                        &mut engine,
                        &mut writer,
                        query,
                        completed_statements,
                        extended,
                        true
                    ),
                    ExecutionStatus::Complete
                );
                assert!(!writer.transaction.is_active());
                assert!(writer.transaction.commit_admission().is_none());
                assert!(
                    engine
                        .prepared_transactions
                        .find(ast::PreparedTransactionId::parse("admission").unwrap())
                        .is_none()
                );
                assert!(String::from_utf8_lossy(writer.send.readable()).contains("55P03"));
                assert!(
                    engine
                        .storage
                        .find_table("public", "commit_admission_parent")
                        .is_some()
                );
                assert_eq!(data_rows(&writer.success(&mut engine, "SELECT count(*) FROM commit_admission_audit; SELECT count(*) FROM commit_admission_parent", true)), ["0", "0"]);
            } else {
                blocker.success(&mut engine, "ROLLBACK", true);
                assert_eq!(
                    resume(
                        &mut engine,
                        &mut writer,
                        query,
                        completed_statements,
                        extended,
                        false
                    ),
                    ExecutionStatus::Complete
                );
                assert!(writer.transaction.commit_admission().is_none());
                assert!(!String::from_utf8_lossy(writer.send.readable()).contains("ERROR"));
                assert_eq!(
                    message_count(writer.send.readable(), b'T'),
                    message_count(&before_completion, b'T') + usize::from(!explicit)
                );
                assert_eq!(
                    message_count(writer.send.readable(), b'C'),
                    if explicit { 3 } else { 2 }
                );
                assert_eq!(data_rows(writer.send.readable()), ["1"]);
                if prepare {
                    writer.success(&mut engine, "COMMIT PREPARED 'admission'", true);
                }
                assert_eq!(data_rows(&writer.success(&mut engine, "SELECT id FROM commit_admission_renamed; SELECT id FROM commit_admission_audit; SELECT last_value FROM commit_admission_calls", true)), ["1", "2", "2"]);
            }
            if blocker.transaction.is_active() {
                blocker.success(&mut engine, "ROLLBACK", true);
            }
            assert!(engine.fatal_commit_failure().is_none());
        }
    }
}
