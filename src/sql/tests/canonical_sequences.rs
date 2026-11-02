use super::*;

#[test]
fn serial_defaults_follow_bound_generators_through_ownership_and_name_changes() {
    let config = test_config("canonical-serial-names");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE TABLE tickets(id serial PRIMARY KEY,payload integer); \
         INSERT INTO tickets(payload) VALUES(1) RETURNING id; \
         ALTER SEQUENCE tickets_id_seq OWNED BY NONE; \
         TRUNCATE tickets RESTART IDENTITY; \
         INSERT INTO tickets(payload) VALUES(2) RETURNING id; \
         SELECT pg_get_serial_sequence('tickets','id') IS NULL; \
         CREATE SCHEMA ticket_sequences; \
         ALTER SEQUENCE tickets_id_seq RENAME TO ticket_counter; \
         ALTER SEQUENCE ticket_counter SET SCHEMA ticket_sequences; \
         ALTER TABLE tickets RENAME TO orders; \
         ALTER TABLE orders RENAME COLUMN id TO ticket; \
         INSERT INTO orders(payload) VALUES(3) RETURNING ticket",
            true
        )),
        ["1", "2", "t", "3"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT pg_get_expr(adbin,adrelid) FROM pg_attrdef \
           WHERE adrelid='orders'::regclass; \
         ALTER TABLE orders ALTER COLUMN ticket SET DEFAULT 99; \
         UPDATE orders SET ticket=DEFAULT WHERE payload=3 RETURNING ticket; \
         ALTER TABLE orders ALTER COLUMN ticket DROP DEFAULT",
            true
        )),
        ["nextval('ticket_sequences.ticket_counter'::regclass)", "99"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO orders(payload) VALUES(4)",
            true
        )),
        ["23502"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE TABLE retained(id serial); \
         INSERT INTO retained DEFAULT VALUES RETURNING id; \
         ALTER SEQUENCE retained_id_seq OWNED BY NONE; \
         DROP TABLE retained; \
         CREATE TABLE retained(id serial); \
         INSERT INTO retained DEFAULT VALUES RETURNING id; \
         SELECT pg_get_serial_sequence('retained','id'); \
         SELECT last_value FROM retained_id_seq",
            true
        )),
        ["1", "1", "public.retained_id_seq1", "1"]
    );
}

#[test]
fn truncate_restart_uses_sequence_ownership_even_without_a_default() {
    let config = test_config("canonical-serial-restart");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "CREATE TABLE restart_values(id serial); \
         SELECT nextval('restart_values_id_seq'); \
         ALTER TABLE restart_values ALTER COLUMN id DROP DEFAULT; \
         TRUNCATE restart_values RESTART IDENTITY; \
         SELECT nextval('restart_values_id_seq'); \
         ALTER SEQUENCE restart_values_id_seq OWNED BY NONE; \
         SELECT setval('restart_values_id_seq',41,true); \
         TRUNCATE restart_values RESTART IDENTITY; \
         SELECT nextval('restart_values_id_seq'); \
         ALTER SEQUENCE restart_values_id_seq OWNED BY restart_values.id; \
         TRUNCATE restart_values RESTART IDENTITY; \
         SELECT nextval('restart_values_id_seq'); \
         BEGIN; TRUNCATE restart_values RESTART IDENTITY; \
         SELECT nextval('restart_values_id_seq'); ROLLBACK; \
         SELECT nextval('restart_values_id_seq')",
            true
        )),
        ["1", "1", "41", "42", "1", "1", "2"]
    );
}

#[test]
fn sequence_drop_cascades_defaults_without_rebinding_to_a_replacement() {
    let config = test_config("canonical-serial-drop");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE drop_values(id serial); \
         CREATE TABLE drop_copy(LIKE drop_values INCLUDING DEFAULTS); \
         ALTER SEQUENCE drop_values_id_seq OWNED BY NONE",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "SELECT count(*) FROM pg_depend WHERE classid=2604 \
           AND refobjid='drop_values_id_seq'::regclass AND deptype='n'",
            true
        )),
        ["2"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(&mut engine, "DROP SEQUENCE drop_values_id_seq", true)),
        ["2BP01"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "BEGIN; DROP SEQUENCE drop_values_id_seq CASCADE; ROLLBACK; \
         INSERT INTO drop_values DEFAULT VALUES RETURNING id; \
         DROP SEQUENCE drop_values_id_seq CASCADE; \
         CREATE SEQUENCE drop_values_id_seq START WITH 100; \
         SELECT column_default IS NULL FROM information_schema.columns \
           WHERE table_name IN('drop_values','drop_copy') AND column_name='id' ORDER BY table_name",
            true
        )),
        ["1", "t", "t"]
    );
    for table in ["drop_values", "drop_copy"] {
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                &format!("INSERT INTO {table} DEFAULT VALUES"),
                true
            )),
            ["23502"]
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "ALTER TABLE drop_values ALTER COLUMN id SET DEFAULT nextval('drop_values_id_seq'); \
         INSERT INTO drop_values DEFAULT VALUES RETURNING id",
            true
        )),
        ["100"]
    );
}

#[test]
fn added_serial_and_identity_columns_backfill_from_their_real_sequences() {
    let config = test_config("canonical-serial-backfill");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    assert_eq!(data_rows(&session.success(&mut engine,
        "CREATE TABLE backfilled(payload integer); INSERT INTO backfilled VALUES(10),(20); \
         ALTER TABLE backfilled ADD COLUMN id serial; \
         ALTER TABLE backfilled ADD COLUMN identity_value bigint GENERATED ALWAYS AS IDENTITY \
           (START WITH 7 INCREMENT BY 3); \
         SELECT id,identity_value,payload FROM backfilled ORDER BY id; \
         INSERT INTO backfilled(payload) VALUES(30) RETURNING id,identity_value; \
         SELECT pg_get_serial_sequence('backfilled','id'),pg_get_serial_sequence('backfilled','identity_value')", true)),
        ["1|7|10", "2|10|20", "3|13", "public.backfilled_id_seq|public.backfilled_identity_value_seq"]);
}

#[test]
fn implicit_identity_generation_does_not_require_sequence_usage_privilege() {
    let config = test_config("canonical-identity-privilege");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE ROLE sequence_writer; \
         CREATE TABLE identity_access(id integer GENERATED ALWAYS AS IDENTITY); \
         CREATE TABLE serial_access(id serial); \
         GRANT INSERT,SELECT ON identity_access,serial_access TO sequence_writer; \
         SET ROLE sequence_writer",
        true,
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "INSERT INTO identity_access DEFAULT VALUES RETURNING id",
            true
        )),
        ["1"]
    );
    assert_eq!(
        error_sqlstates(&session.execute(
            &mut engine,
            "INSERT INTO serial_access DEFAULT VALUES RETURNING id",
            true
        )),
        ["42501"]
    );
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "RESET ROLE; GRANT USAGE ON SEQUENCE serial_access_id_seq TO sequence_writer; \
         SET ROLE sequence_writer; INSERT INTO serial_access DEFAULT VALUES RETURNING id",
            true
        )),
        ["1"]
    );
}

#[test]
fn canonical_defaults_and_ownership_survive_cold_prepared_decisions() {
    for commit in [false, true] {
        for checkpoint in [false, true] {
            let name = format!(
                "canonical-prepared-{commit}-{checkpoint}-{}",
                std::process::id()
            );
            let mut config = test_config(&name);
            config.object_store_on = true;
            config.object_store_sim = true;
            config.object_store_bucket = name;
            config.wal_upload = true;
            config.wal_upload_sync = true;
            config.max_prepared_transactions = 1;
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
            {
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
                assert_eq!(
                    data_rows(&session.success(
                        &mut engine,
                        "CREATE SCHEMA counters; CREATE TABLE saved_default(id serial); \
                     INSERT INTO saved_default DEFAULT VALUES RETURNING id; \
                     BEGIN; ALTER SEQUENCE saved_default_id_seq OWNED BY NONE; \
                     ALTER SEQUENCE saved_default_id_seq RENAME TO saved_counter; \
                     ALTER SEQUENCE saved_counter SET SCHEMA counters; \
                     INSERT INTO saved_default DEFAULT VALUES RETURNING id; \
                     PREPARE TRANSACTION 'saved-default'",
                        false
                    )),
                    ["1", "2"]
                );
                if checkpoint {
                    assert!(engine.checkpoint().unwrap());
                }
                engine.commit_wal().unwrap();
            }
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            {
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
                session.success(
                    &mut engine,
                    if commit {
                        "COMMIT PREPARED 'saved-default'"
                    } else {
                        "ROLLBACK PREPARED 'saved-default'"
                    },
                    false,
                );
                assert_eq!(
                    data_rows(&session.success(
                        &mut engine,
                        "INSERT INTO saved_default DEFAULT VALUES RETURNING id; \
                     SELECT id FROM saved_default ORDER BY id; \
                     SELECT pg_get_serial_sequence('saved_default','id')",
                        false
                    )),
                    if commit {
                        vec!["3", "1", "2", "3", "NULL"]
                    } else {
                        vec!["3", "1", "3", "public.saved_default_id_seq"]
                    }
                );
                let expected = if commit {
                    "nextval('counters.saved_counter'::regclass)"
                } else {
                    "nextval('saved_default_id_seq'::regclass)"
                };
                assert_eq!(
                    data_rows(&session.success(
                        &mut engine,
                        "SELECT pg_get_expr(adbin,adrelid) FROM pg_attrdef \
                       WHERE adrelid='saved_default'::regclass",
                        false
                    )),
                    [expected]
                );
                engine.commit_wal().unwrap();
            }
            std::fs::remove_dir_all(&config.data_dir).unwrap();
            crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        }
    }
}

#[test]
fn identity_defaults_cannot_be_replaced_or_dropped() {
    let config = test_config("canonical-identity-default");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE identity_default(id integer GENERATED ALWAYS AS IDENTITY)",
        true,
    );
    for sql in [
        "ALTER TABLE identity_default ALTER COLUMN id SET DEFAULT 99",
        "ALTER TABLE identity_default ALTER COLUMN id DROP DEFAULT",
    ] {
        assert_eq!(
            error_sqlstates(&session.execute(&mut engine, sql, true)),
            ["42601"]
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT attidentity FROM pg_attribute \
               WHERE attrelid='identity_default'::regclass AND attname='id'",
                true
            )),
            ["a"]
        );
    }
    assert_eq!(
        data_rows(&session.success(
            &mut engine,
            "INSERT INTO identity_default DEFAULT VALUES RETURNING id",
            true
        )),
        ["1"]
    );
}

#[test]
fn sequence_advances_from_another_transaction_survive_cold_commit_retries() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    let name = format!("sequence-publish-cold-{}", std::process::id());
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
    session.success(&mut engine, "CREATE TABLE cold_commit_rows(id integer); CREATE TABLE cold_commit_empty(id integer); CREATE SEQUENCE publication_counter CACHE 1; INSERT INTO cold_commit_rows VALUES(1)", false);
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
        "BEGIN; TRUNCATE cold_commit_empty; UPDATE cold_commit_rows SET id=2; SELECT nextval('publication_counter')",
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
    let mut publisher =
        ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    server.hold_response.store(true, Ordering::Release);
    let requests_before = server.requests.load(Ordering::Acquire);
    assert_eq!(engine.wal.stage_stats(session.transaction.txid), (0, 0));
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
        let staged = engine.wal.staged_bytes_for_tests(session.transaction.txid);
        let mut offset = 0;
        let mut scopes = 0;
        let mut truncates = 0;
        while offset < staged.len() {
            let payload_len =
                u32::from_le_bytes(staged[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let end = offset + crate::wal::HEADER_LEN + payload_len;
            match crate::wal::decode_record(&staged[offset + 16..end]).unwrap() {
                crate::wal::WalOp::DatabaseScope { oid } => {
                    assert_eq!(oid, crate::storage::DatabaseOid::POSTGRES.get());
                    scopes += 1;
                }
                crate::wal::WalOp::Truncate { table_count, .. } => {
                    assert_eq!(table_count, 1);
                    truncates += 1;
                }
                _ => panic!("unexpected WAL before the cold UPDATE image"),
            }
            offset = end;
        }
        assert_eq!((scopes, truncates), (1, 1));
        assert_eq!(engine.wal.stage_stats(session.transaction.txid).0, 2);
        assert!(session.transaction.staged_commit().is_none());
        if attempt == 0 {
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.requests.load(Ordering::Acquire) == requests_before {
                let _ = engine.advance_pending_block_read(0).unwrap();
                assert!(Instant::now() < deadline, "cold old-row GET was not issued");
                std::thread::yield_now();
            }
            // This transaction publishes the newer value while the older
            // transaction is parked. Its receipt must not be lost on retry.
            assert_eq!(
                data_rows(&publisher.success(
                    &mut engine,
                    "SELECT nextval('publication_counter')",
                    true
                )),
                ["2"]
            );
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
    let mut sequence_advances = 0;
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
                crate::wal::WalOp::SequenceAdvance {
                    name: "publication_counter",
                    last,
                    is_called,
                    ..
                } => {
                    assert_eq!((last, is_called), (2, true));
                    sequence_advances += 1;
                }
                _ => {}
            }
            Ok(())
        })
        .unwrap();
    assert_eq!((truncates, updates, sequence_advances), (1, 1, 1));
    assert!(crate::mem::guard::forbid_alloc(|| engine.checkpoint()).unwrap());
    drop((session, publisher, engine, blocks));
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    {
        let mut recovered_budget = test_engine_budget(&config, 1 << 29);
        let mut recovered = Engine::new(&config, &mut recovered_budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut recovered_budget);
        assert_eq!(
            data_rows(&session.success(
                &mut recovered,
                "SELECT last_value,is_called FROM publication_counter; \
             SELECT nextval('publication_counter'); SELECT id FROM cold_commit_rows",
                true
            )),
            ["2|t", "3", "2"]
        );
    }
    std::fs::remove_dir_all(&config.data_dir).unwrap();
    drop(server);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
