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

#[test]
fn prepared_resolution_installs_admitted_metadata_with_no_free_lease() {
    for cold in [false, true] {
        let name = format!("prepared-metadata-admission-{cold}-{}", std::process::id());
        let mut config = test_config(&name);
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = name;
        config.max_tables = 1;
        config.max_indexes = 1;
        config.max_prepared_transactions = 1;
        config.txn_rows = 8;
        config.row_overlay_slots = 16;
        config.max_value_indexes = 4;
        config.value_index_rows = 16;
        config.table_metadata_bytes = 64 << 10;
        config.table_metadata_leases = 256;
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE prepared_metadata(id integer PRIMARY KEY,amount integer); \
             INSERT INTO prepared_metadata VALUES(1,10); \
             BEGIN; ALTER TABLE prepared_metadata ALTER COLUMN amount TYPE bigint; \
             UPDATE prepared_metadata SET id=11 WHERE id=1; \
             CREATE INDEX prepared_metadata_amount ON prepared_metadata(amount); \
             PREPARE TRANSACTION 'metadata-admitted'",
            false,
        );
        if cold {
            assert!(engine.checkpoint().unwrap());
            engine.commit_wal().unwrap();
            drop((session, engine));
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut recovered_budget = test_engine_budget(&config, 1 << 29);
            engine = Engine::new(&config, &mut recovered_budget).unwrap();
            session = ConfiguredTransactionSession::new(&config, &mut recovered_budget);
        }
        engine.enable_async_durability();
        session.send.clear();
        session.arena.reset();
        let status = crate::mem::guard::forbid_alloc(|| {
            engine.execute_simple_from(
                "COMMIT PREPARED 'metadata-admitted'",
                0,
                &session.arena,
                &mut session.transaction,
                &mut session.pool,
                &mut session.cursors,
                &mut session.guc,
                &mut Responder::new(&mut session.send),
                session.connection_id,
                false,
            )
        })
        .unwrap();
        assert!(matches!(
            status,
            ExecutionStatus::Blocked {
                wait: StatementWait::ObjectIo,
                ..
            }
        ));
        assert!(session.transaction.staged_commit().is_some());
        let mut held: [Option<crate::mem::lease_pool::MutableSlice<u8>>; 256] =
            core::array::from_fn(|_| None);
        let mut count = 0;
        while let Ok(lease) = engine.storage.metadata_pool_for_tests().lease::<u8>(64) {
            held[count] = Some(lease);
            count += 1;
        }
        assert!(count != 0);
        engine.commit_wal().unwrap();
        session.arena.reset();
        let status = crate::mem::guard::forbid_alloc(|| {
            engine.execute_simple_from(
                "COMMIT PREPARED 'metadata-admitted'",
                0,
                &session.arena,
                &mut session.transaction,
                &mut session.pool,
                &mut session.cursors,
                &mut session.guc,
                &mut Responder::new(&mut session.send),
                session.connection_id,
                false,
            )
        })
        .unwrap();
        assert_eq!(status, ExecutionStatus::Complete);
        assert!(error_sqlstates(session.send.readable()).is_empty());
        assert!(engine.fatal_commit_failure().is_none());
        drop(held);
        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT id,amount FROM prepared_metadata WHERE id=11; SELECT count(*) FROM prepared_metadata WHERE id=1; SELECT count(*) FROM pg_indexes WHERE indexname='prepared_metadata_amount'", false)),
            ["11|10", "0", "1"]);
        drop((session, engine));
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    }
}

#[test]
fn commit_admission_rewinds_generated_wal_after_repeated_cold_row_waits() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let name = format!("commit-cold-wal-prefix-{}", std::process::id());
    let mut config = test_config(&name);
    config.object_store_on = true;
    config.object_store_sim = true;
    config.object_store_bucket = name;
    config.max_tables = 2;
    config.row_overlay_slots = 8;
    config.txn_rows = 8;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(&mut engine, "CREATE TABLE cold_commit_rows(id integer); CREATE TABLE cold_commit_empty(id integer); INSERT INTO cold_commit_rows VALUES(1)", false);
    assert!(engine.checkpoint().unwrap());
    let table = engine
        .storage
        .find_table("public", "cold_commit_rows")
        .unwrap();
    engine.storage.finish_row_cleanup(
        RowCleanupTask::HeapRows(HeapCleanup::AllSaved),
        RowCleanupTarget::Table(table),
    );
    engine.storage.finish_row_cleanup(
        RowCleanupTask::SavedEntries(EntryCleanup::AllSaved),
        RowCleanupTarget::Table(table),
    );
    let server = object_reads::HttpObjectServer::start(&mut config);
    let mut io_budget = Budget::new(
        Checkpointer::budget_bytes(&config)
            + crate::storage::SpillReader::budget_bytes(
                true,
                config.max_spill_generations_per_table,
            ),
    );
    let checkpoint = Checkpointer::new(&config, &mut io_budget).unwrap();
    let blocks = checkpoint.block_stack();
    let reader = crate::storage::SpillReader::new(
        &mut io_budget,
        config.max_spill_generations_per_table,
        Some(Arc::clone(&blocks)),
        engine
            .temporary_spiller
            .as_ref()
            .map(TemporarySpiller::block_store),
    )
    .unwrap();
    engine.storage.attach_spill(reader);
    engine.ckpt = Some(checkpoint);
    engine.enable_async_block_reads();
    session.success(
        &mut engine,
        "BEGIN; TRUNCATE cold_commit_empty; UPDATE cold_commit_rows SET id=2",
        true,
    );
    // UPDATE may retain its source block in the spill reader's decode scratch.
    // A fresh fixture reader gives the WAL builder a genuinely cold old image.
    let mut commit_read_budget = Budget::new(crate::storage::SpillReader::budget_bytes(
        true,
        config.max_spill_generations_per_table,
    ));
    let reader = crate::storage::SpillReader::new(
        &mut commit_read_budget,
        config.max_spill_generations_per_table,
        Some(Arc::clone(&blocks)),
        engine
            .temporary_spiller
            .as_ref()
            .map(TemporarySpiller::block_store),
    )
    .unwrap();
    engine.storage.attach_spill(reader);
    let rowid = session.transaction.touched()[0].1;
    assert!(matches!(
        engine
            .storage
            .row_state(table, rowid)
            .unwrap()
            .unwrap()
            .committed,
        Some(crate::storage::RowHome::Spilled { .. })
    ));
    server.hold_response.store(true, Ordering::Release);
    let requests_before = server.requests.load(Ordering::Acquire);
    let original = engine.wal.stage_stats(session.transaction.txid);
    // Exercise restartable admission directly: the frontend commit wrapper
    // keeps effectful deferred SQL inside its synchronous read scope.
    for attempt in 0..3 {
        let error = crate::mem::guard::forbid_alloc(|| {
            engine.finish_txn(
                &mut session.transaction,
                &session.guc,
                crate::sql::txn::CommitTarget::Ordinary,
            )
        })
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_IO_WAIT);
        assert_eq!(
            engine.wal.stage_stats(session.transaction.txid).0,
            original.0 + 1,
            "exactly one TRUNCATE is staged before the cold UPDATE image"
        );
        assert!(session.transaction.staged_commit().is_none());
        if attempt == 0 {
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.requests.load(Ordering::Acquire) == requests_before {
                let _ = engine.advance_pending_block_read(0).unwrap();
                assert!(Instant::now() < deadline, "cold old-row GET was not issued");
                std::thread::yield_now();
            }
        }
    }
    assert!(server.requests.load(Ordering::Acquire) > requests_before);
    server.hold_response.store(false, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let _ = engine.advance_pending_block_read(0).unwrap();
        match crate::mem::guard::forbid_alloc(|| {
            engine.finish_txn(
                &mut session.transaction,
                &session.guc,
                crate::sql::txn::CommitTarget::Ordinary,
            )
        }) {
            Ok(()) => break,
            Err(error) if error.sqlstate == sqlstate::INTERNAL_IO_WAIT => {
                assert!(Instant::now() < deadline, "cold commit did not finish");
                std::thread::yield_now();
            }
            Err(error) => panic!(
                "cold commit failed: {} {}",
                error.sqlstate,
                error.message.as_str()
            ),
        }
    }
    assert!(session.transaction.commit_admission().is_none());
    assert_eq!(
        data_rows(&session.success(&mut engine, "SELECT id FROM cold_commit_rows", true)),
        ["2"]
    );
    let mut truncates = 0;
    let mut updates = 0;
    // Inspect the complete journal after the final engine operation.
    engine
        .wal
        .replay(0, |_, record| {
            match crate::wal::decode_record(record).unwrap() {
                crate::wal::WalOp::Truncate { .. } => truncates += 1,
                crate::wal::WalOp::Upsert {
                    table: "cold_commit_rows",
                    is_update: true,
                    old_row: Some(_),
                    ..
                } => updates += 1,
                _ => {}
            }
            Ok(())
        })
        .unwrap();
    assert_eq!((truncates, updates), (1, 1));
    drop((session, engine, blocks));
    drop(server);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}

#[test]
fn cold_prepared_type_index_and_query_declarations_stay_private_until_resolution() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for commit in [false, true] {
            let name = format!("prepared-catalog-types-{mode:?}-{commit}");
            let mut config = test_config(&name);
            config.transaction_mode = mode;
            config.max_tables = 2;
            config.max_prepared_transactions = 1;
            config.max_ddl_per_transaction = 64;
            config.max_views = 2;
            config.max_rules = 8;
            config.max_triggers = 2;
            config.max_policies = 2;
            config.max_domains = 4;
            config.max_enums = 4;
            config.max_composites = 4;
            config.row_overlay_slots = 32;
            config.txn_rows = 32;
            config.value_index_rows = 32;
            config.object_store_on = true;
            config.object_store_sim = true;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.object_store_bucket = format!("{name}-{}", std::process::id());
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            let mut budget = test_engine_budget(&config, 1 << 29);
            let mut engine = Engine::new(&config, &mut budget).unwrap();
            let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
            session.success(
                &mut engine,
                "CREATE DOMAIN original_domain AS integer; \
                 CREATE TYPE original_mood AS ENUM ('first'); \
                 CREATE TYPE original_pair AS (id integer); \
                 CREATE TABLE prepared_objects(id integer,amount integer); \
                 CREATE TABLE prepared_audit(id integer); \
                 INSERT INTO prepared_objects VALUES(1,10),(2,20)",
                false,
            );
            session.success(&mut engine,
                "BEGIN; \
                 ALTER DOMAIN original_domain SET DEFAULT 7; \
                 ALTER TYPE original_mood ADD VALUE 'second'; \
                 ALTER TYPE original_pair ADD ATTRIBUTE note text; \
                 CREATE DOMAIN prepared_domain AS integer CHECK(VALUE > 0); \
                 CREATE TYPE prepared_mood AS ENUM ('saved'); \
                 CREATE TYPE prepared_pair AS (id integer); \
                 CREATE UNIQUE INDEX prepared_index ON prepared_objects(id); \
                 CREATE VIEW prepared_view AS SELECT id FROM prepared_objects; \
                 CREATE FUNCTION prepared_trigger_fn() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$; \
                 CREATE TRIGGER prepared_trigger BEFORE INSERT ON prepared_objects FOR EACH ROW EXECUTE FUNCTION prepared_trigger_fn(); \
                 CREATE POLICY prepared_policy ON prepared_objects USING(id > 0); \
                 CREATE RULE prepared_rule AS ON INSERT TO prepared_objects DO ALSO INSERT INTO prepared_audit VALUES(NEW.id); \
                 CREATE STATISTICS prepared_stats ON id,amount FROM prepared_objects; \
                 ANALYZE prepared_objects; \
                 PREPARE TRANSACTION 'catalog-images'", false);
            engine.commit_wal().unwrap();
            drop((session, engine));
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            let mut cold_budget = test_engine_budget(&config, 1 << 29);
            let mut recovered = Engine::new(&config, &mut cold_budget).unwrap();
            let mut resolver = ConfiguredTransactionSession::new(&config, &mut cold_budget);
            let storage = &recovered.storage;
            assert!(
                storage
                    .domain_slot("public", "prepared_domain", 0)
                    .is_none()
            );
            assert!(storage.enum_slot("public", "prepared_mood", 0).is_none());
            assert!(
                storage
                    .composite_slot("public", "prepared_pair", 0)
                    .is_none()
            );
            assert!(storage.index_slot("public", "prepared_index", 0).is_none());
            assert!(storage.view_slot("public", "prepared_view", 0).is_none());
            let original_enum = storage.enum_slot("public", "original_mood", 0).unwrap();
            let original_composite = storage
                .composite_slot("public", "original_pair", 0)
                .unwrap();
            let original_domain = storage.domain_slot("public", "original_domain", 0).unwrap();
            assert_eq!(storage.enum_for(original_enum, 0).members().len(), 1);
            assert_eq!(storage.composite_for(original_composite, 0).n_fields, 1);
            assert!(
                storage
                    .domain_for(original_domain, 0)
                    .default_expr
                    .is_none()
            );
            resolver.success(
                &mut recovered,
                if commit {
                    "COMMIT PREPARED 'catalog-images'"
                } else {
                    "ROLLBACK PREPARED 'catalog-images'"
                },
                false,
            );
            let storage = &recovered.storage;
            assert_eq!(
                storage
                    .domain_slot("public", "prepared_domain", 0)
                    .is_some(),
                commit
            );
            assert_eq!(
                storage.enum_slot("public", "prepared_mood", 0).is_some(),
                commit
            );
            assert_eq!(
                storage
                    .composite_slot("public", "prepared_pair", 0)
                    .is_some(),
                commit
            );
            assert_eq!(
                storage.index_slot("public", "prepared_index", 0).is_some(),
                commit
            );
            assert_eq!(
                storage.view_slot("public", "prepared_view", 0).is_some(),
                commit
            );
            assert_eq!(
                storage.enum_for(original_enum, 0).members().len(),
                if commit { 2 } else { 1 }
            );
            assert_eq!(
                storage.composite_for(original_composite, 0).n_fields,
                if commit { 2 } else { 1 }
            );
            assert_eq!(
                storage
                    .domain_for(original_domain, 0)
                    .default_expr
                    .is_some(),
                commit
            );
            let table = storage.find_table("public", "prepared_objects").unwrap();
            assert_eq!(
                storage
                    .trigger_slot_on(
                        crate::storage::TriggerTarget::Table(table as u16),
                        "prepared_trigger",
                        0
                    )
                    .is_some(),
                commit
            );
            assert_eq!(
                storage
                    .policy_slot_on(table, "prepared_policy", 0)
                    .is_some(),
                commit
            );
            assert_eq!(
                storage
                    .rule_slot(
                        crate::storage::RuleTarget::Table(table as u16),
                        "prepared_rule",
                        0
                    )
                    .is_some(),
                commit
            );
            let statistics = storage.extended_statistics_slot("public", "prepared_stats", 0);
            assert_eq!(statistics.is_some(), commit);
            if let Some(slot) = statistics {
                assert!(storage.extended_statistics_data(slot, 0).valid);
            }
            if commit {
                assert_eq!(
                    error_sqlstates(&resolver.execute(
                        &mut recovered,
                        "INSERT INTO prepared_objects VALUES(1,99)",
                        false
                    )),
                    ["23505"]
                );
                resolver.success(
                    &mut recovered,
                    "INSERT INTO prepared_objects VALUES(3,30)",
                    false,
                );
                assert_eq!(
                    data_rows(&resolver.success(
                        &mut recovered,
                        "SELECT id FROM prepared_view ORDER BY id; SELECT id FROM prepared_audit",
                        false
                    )),
                    ["1", "2", "3", "3"]
                );
            } else {
                resolver.success(
                    &mut recovered,
                    "INSERT INTO prepared_objects VALUES(1,99)",
                    false,
                );
                assert_eq!(
                    data_rows(&resolver.success(
                        &mut recovered,
                        "SELECT count(*) FROM prepared_audit",
                        false
                    )),
                    ["0"]
                );
            }
            drop((resolver, recovered));
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }
}
