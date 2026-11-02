use super::*;

#[test]
fn hot_table_uses_all_shared_rows_and_full_admission_preserves_other_owners() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("shared-row-capacity-hot-{mode:?}"));
        config.transaction_mode = mode;
        config.row_history_slots = if mode == crate::config::TransactionMode::Mvcc {
            16
        } else {
            0
        };
        config.catalog_history_bytes = if mode == crate::config::TransactionMode::Mvcc {
            1 << 20
        } else {
            0
        };
        config.row_overlay_slots = 4;
        config.row_work_bytes = 4096;
        config.row_work_leases = 32;
        config.large_object_pages = 2;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        session.success(
            &mut engine,
            "CREATE TABLE shared_hot(id integer, amount integer); \
             CREATE TABLE shared_other(id integer, amount integer); \
             CREATE TABLE shared_empty_a(id integer); CREATE TABLE shared_empty_b(id integer); \
             CREATE TABLE shared_empty_c(id integer); CREATE TABLE shared_empty_d(id integer); \
             CREATE TABLE shared_empty_e(id integer); CREATE TABLE shared_empty_f(id integer); \
             INSERT INTO shared_hot VALUES(1,10),(2,20),(3,30),(4,40)",
            true,
        );
        let hot = engine.storage.find_table("public", "shared_hot").unwrap();
        let other = engine.storage.find_table("public", "shared_other").unwrap();
        assert_eq!(
            engine.storage.table(hot).rows.len(),
            config.row_overlay_slots
        );
        assert!(engine.storage.table(other).rows.is_empty());
        let before: Vec<_> = engine.storage.table(hot).rows.iter().collect();
        let output = session.execute(&mut engine, "INSERT INTO shared_other VALUES(9,90)", true);
        assert_eq!(error_sqlstates(&output), ["54000"]);
        assert!(String::from_utf8_lossy(&output).contains("row_overlay_slots"));
        assert_eq!(
            engine.storage.table(hot).rows.iter().collect::<Vec<_>>(),
            before
        );
        assert!(engine.storage.table(other).rows.is_empty());
        assert!(session.transaction.touched().is_empty());
        session.success(
            &mut engine,
            "UPDATE shared_hot SET amount = amount + 1 WHERE id = 1",
            true,
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT sum(amount) FROM shared_hot", true)),
            ["101"]
        );
        assert_eq!(engine.storage.table(hot).rows.len(), 4);

        assert_eq!(data_rows(&session.success(&mut engine,
            "SELECT lo_from_bytea(55001::oid, decode('6162','hex')); \
             SELECT lo_put(55001::oid, 2048::bigint, decode('63','hex')); \
             SELECT octet_length(lo_get(55001::oid)), encode(lo_get(55001::oid,2048::bigint,1),'hex')", true)).last().unwrap(), "2049|63");
        assert_eq!(engine.storage.table(hot).rows.len(), 4);
        assert_eq!(
            error_sqlstates(&session.execute(
                &mut engine,
                "SELECT lo_put(55001::oid,4096::bigint,decode('64','hex'))",
                true
            )),
            ["54000"]
        );
        assert_eq!(
            data_rows(&session.success(&mut engine, "SELECT sum(amount) FROM shared_hot", true)),
            ["101"]
        );

        session.success(&mut engine, "DROP TABLE shared_hot", true);
        session.success(&mut engine, "CREATE TABLE shared_reused(id integer)", true);
        session.success(
            &mut engine,
            "BEGIN; INSERT INTO shared_other VALUES(1,1),(2,2),(3,3); \
             SAVEPOINT row_slot; INSERT INTO shared_other VALUES(4,4); \
             ROLLBACK TO row_slot; INSERT INTO shared_reused VALUES(9); ROLLBACK",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT (SELECT count(*) FROM shared_other),(SELECT count(*) FROM shared_reused)",
                true
            )),
            ["0|0"]
        );
        session.success(
            &mut engine,
            "INSERT INTO shared_reused VALUES(1),(2),(3),(4)",
            true,
        );
        assert_eq!(
            data_rows(&session.success(
                &mut engine,
                "SELECT count(*),sum(id) FROM shared_reused",
                true
            )),
            ["4|10"]
        );
    }
}

#[test]
fn repeatable_read_keeps_versions_when_the_global_overlay_is_full() {
    let mut config = test_config("shared-row-capacity-retained-reader");
    config.row_overlay_slots = 2;
    config.row_history_slots = 4;
    config.row_work_bytes = 4096;
    config.row_work_leases = 16;
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(1);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(
        &mut engine,
        "CREATE TABLE shared_versioned(id integer, amount integer); \
         CREATE TABLE shared_version_other(id integer); \
         INSERT INTO shared_versioned VALUES(1,10),(2,20)",
        true,
    );
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            "BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT amount FROM shared_versioned WHERE id=1",
            true
        )),
        ["10"]
    );
    writer.success(
        &mut engine,
        "UPDATE shared_versioned SET amount=11 WHERE id=1",
        true,
    );
    let table = engine
        .storage
        .find_table("public", "shared_versioned")
        .unwrap();
    let before: Vec<_> = engine.storage.table(table).rows.iter().collect();
    assert_eq!(before.len(), 2);
    assert!(before.iter().any(|(_, row)| !row.history.is_empty()));
    let output = writer.execute(
        &mut engine,
        "INSERT INTO shared_version_other VALUES(3)",
        true,
    );
    assert_eq!(error_sqlstates(&output), ["54000"]);
    assert_eq!(
        engine.storage.table(table).rows.iter().collect::<Vec<_>>(),
        before
    );
    assert_eq!(
        data_rows(&reader.success(
            &mut engine,
            "SELECT amount FROM shared_versioned WHERE id=1; COMMIT",
            true
        )),
        ["10"]
    );
    assert_eq!(
        data_rows(&writer.success(
            &mut engine,
            "SELECT amount FROM shared_versioned WHERE id=1",
            true
        )),
        ["11"]
    );
}

#[test]
fn ordinary_row_pool_startup_draw_is_independent_of_configured_table_count() {
    use core::sync::atomic::{AtomicUsize, Ordering};
    static OVERLAY: AtomicUsize = AtomicUsize::new(0);
    static WORK: AtomicUsize = AtomicUsize::new(0);
    let mut expected = None;
    for tables in [1, 8, 32, 128] {
        let mut config = test_config(&format!("shared-row-capacity-budget-{tables}"));
        config.transaction_mode = crate::config::TransactionMode::Locking;
        config.catalog_history_bytes = 0;
        config.row_history_slots = 0;
        config.max_tables = tables;
        config.max_connections = 1;
        config.max_prepared_transactions = 0;
        config.max_subscriptions = 0;
        config.max_ddl_per_transaction = 1;
        config.max_analyze_per_transaction = 1;
        config.max_deferred_constraints_per_transaction = 2;
        config.txn_rows = 4;
        config.row_overlay_slots = 4;
        config.row_work_bytes = 4096;
        config.row_work_leases = 16;
        config.large_object_pages = 2;
        let mut budget = Budget::new(config.memtable_bytes + Storage::extra_budget_bytes(&config));
        OVERLAY.store(0, Ordering::Relaxed);
        WORK.store(0, Ordering::Relaxed);
        budget.observe_draws(|name, bytes| {
            if name == "row_overlay_slots" {
                OVERLAY.fetch_add(bytes, Ordering::Relaxed);
            }
            if matches!(name, "row work pool" | "row work free ranges") {
                WORK.fetch_add(bytes, Ordering::Relaxed);
            }
        });
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let measured = (
            OVERLAY.load(Ordering::Relaxed),
            WORK.load(Ordering::Relaxed),
        );
        assert!(measured.0 > 0 && measured.1 >= config.row_work_bytes);
        if let Some(first) = expected {
            assert_eq!(measured, first, "configured tables: {tables}");
        } else {
            expected = Some(measured);
        }
        drop(storage);
    }
}

#[test]
fn global_overlay_pressure_starts_the_first_immutable_generation() {
    let mut config = test_config("shared-row-capacity-first-generation");
    config.row_overlay_slots = 4;
    config.row_history_slots = 16;
    config.row_work_bytes = 4096;
    config.row_work_leases = 32;
    config.object_store_on = true;
    config.object_store_sim = true;
    config.wal_upload = true;
    config.wal_upload_sync = true;
    config.block_cache_bytes = 0;
    config.disk_cache_bytes = 0;
    config.object_store_bucket = format!(
        "shared-row-capacity-first-generation-{}",
        std::process::id()
    );
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
    session.success(
        &mut engine,
        "CREATE TABLE shared_first_generation(id integer, amount integer); \
         CREATE TABLE shared_after_flush(id integer); \
         INSERT INTO shared_first_generation VALUES(1,10),(2,20)",
        false,
    );
    let table = engine
        .storage
        .find_table("public", "shared_first_generation")
        .unwrap();
    assert_eq!(engine.storage.spill_generation_count(table), 0);
    assert!(engine.storage.map_pressure());
    assert!(!engine.storage.history_pressure());
    assert!(engine.storage.heap.used() * 100 < engine.storage.heap.capacity() * 65);
    assert!(engine.wal.used_bytes() * 100 < engine.wal.capacity_bytes() * 50);
    assert!(engine.checkpoint_work_pending());
    for _ in 0..256 {
        engine
            .advance_wal_publication(
                std::time::Instant::now(),
                crate::sql::CommitProgressMode::Reactor,
            )
            .unwrap();
        let beat = engine.checkpoint_beat();
        assert!(
            !beat.failed(),
            "first immutable generation failed: {beat:?}"
        );
        if engine.storage.spill_generation_count(table) != 0
            && engine.storage.table(table).rows.is_empty()
        {
            break;
        }
    }
    assert_eq!(engine.storage.spill_generation_count(table), 1);
    engine.finish_local_cleanup().unwrap();
    assert!(engine.storage.table(table).rows.is_empty());
    session.success(
        &mut engine,
        "INSERT INTO shared_after_flush VALUES(1),(2),(3),(4)",
        false,
    );
    assert_eq!(data_rows(&session.success(&mut engine,
        "SELECT (SELECT sum(amount) FROM shared_first_generation),(SELECT count(*) FROM shared_after_flush)", false)), ["30|4"]);
    drop(session);
    drop(engine);
    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
}
