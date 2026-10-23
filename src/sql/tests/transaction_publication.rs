use super::*;
use crate::sql::commit::{PublicationPoint, PublicationState};

const SETUP: &str = "CREATE TABLE publication_orders(id integer PRIMARY KEY, total integer); \
    CREATE TABLE publication_invoices(id integer PRIMARY KEY, total integer); \
    INSERT INTO publication_orders VALUES(1,10); INSERT INTO publication_invoices VALUES(1,10)";
const CHANGE: &str = "BEGIN; SELECT pg_current_xact_id(); \
    SELECT pg_advisory_xact_lock(7001); \
    UPDATE publication_orders SET total=20 WHERE id=1; \
    UPDATE publication_invoices SET total=20 WHERE id=1; \
    ALTER TABLE publication_orders RENAME COLUMN total TO amount; \
    COMMENT ON TABLE publication_orders IS 'confirmed order'; \
    CREATE INDEX publication_order_amount ON publication_orders(amount)";

#[test]
fn transaction_publication_rows_definitions_indexes_and_comments_match_postgresql() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("publication-reference-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut rows = Vec::new();
        for sql in include_str!(
            "../../../tests/external/differential_exact/17_transaction_publication.sql"
        )
        .split(';')
        .filter(|sql| !sql.trim().is_empty())
        {
            rows.extend(data_rows(&session.success(&mut engine, sql, true)));
        }
        assert_eq!(
            rows,
            ["20|20", "confirmed order", "20|20", "confirmed order", "1"]
        );
    }
}

fn observe_owners(engine: &Engine, point: PublicationPoint, observer: u32) -> Result<(), SqlError> {
    if let PublicationState::Installing(publication) = engine.transaction_publication {
        if publication.transactions().count() == 2 {
            assert_eq!(
                engine
                    .prepared_transactions
                    .find(ast::PreparedTransactionId::parse("publication-order").unwrap()),
                Some(0),
                "prepared slot became reusable before its decision was visible at {point:?}"
            );
        }
        if engine.wal_upload {
            assert!(
                engine.durable_lsn >= publication.lsn,
                "data installation preceded object-store durability at {point:?}"
            );
        }
        if point != PublicationPoint::Visible {
            assert!(
                engine.visible_lsn < publication.lsn,
                "visible journal position crossed unfinished installation at {point:?}"
            );
        }
        for (transaction, _) in publication.transactions() {
            assert_eq!(
                engine.storage.transaction_status(u64::from(transaction))?,
                Some("in progress"),
                "transaction status changed before all publication owners were released at {point:?}"
            );
        }
    }
    for name in ["publication_orders", "publication_invoices"] {
        let table = engine.storage.find_table("public", name).unwrap();
        assert_eq!(
            engine
                .storage
                .lock_table(observer, table, ast::TableLockMode::AccessExclusive, true)
                .unwrap_err()
                .sqlstate,
            sqlstate::LOCK_NOT_AVAILABLE,
            "{name} lost its owner at {point:?}"
        );
    }
    if let PublicationPoint::RowInstalled { table, rowid } = point {
        assert_eq!(
            engine
                .storage
                .acquire_row_lock(
                    table,
                    rowid,
                    observer,
                    ast::LockStrength::Update,
                    ast::LockWait::NoWait
                )
                .unwrap_err()
                .sqlstate,
            sqlstate::LOCK_NOT_AVAILABLE,
            "installed row lost its lock before the other rows were visible"
        );
    }
    assert_eq!(
        engine.storage.advisory_lock_view_count(),
        1,
        "transaction advisory lock released at {point:?}"
    );
    Ok(())
}

fn inject_definitions(
    engine: &Engine,
    point: PublicationPoint,
    observer: u32,
) -> Result<(), SqlError> {
    observe_owners(engine, point, observer)?;
    if point == PublicationPoint::DefinitionsInstalled {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "injected definition installation failure"
        ));
    }
    Ok(())
}

fn inject_row(engine: &Engine, point: PublicationPoint, observer: u32) -> Result<(), SqlError> {
    observe_owners(engine, point, observer)?;
    if matches!(point, PublicationPoint::RowInstalled { .. }) {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "injected row installation failure"
        ));
    }
    Ok(())
}

fn inject_installed(
    engine: &Engine,
    point: PublicationPoint,
    observer: u32,
) -> Result<(), SqlError> {
    observe_owners(engine, point, observer)?;
    if point == PublicationPoint::DataInstalled {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "injected completed installation failure"
        ));
    }
    Ok(())
}

fn inject_visible(engine: &Engine, point: PublicationPoint, observer: u32) -> Result<(), SqlError> {
    observe_owners(engine, point, observer)?;
    if point == PublicationPoint::Visible {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "injected visibility installation failure"
        ));
    }
    Ok(())
}

fn inject_prepared(
    engine: &Engine,
    point: PublicationPoint,
    observer: u32,
) -> Result<(), SqlError> {
    observe_owners(engine, point, observer)?;
    if point == PublicationPoint::PreparedInstalled {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "injected prepared installation failure"
        ));
    }
    Ok(())
}

#[test]
fn transaction_publication_keeps_row_catalog_and_advisory_owners_until_visible() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("publication-owners-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut observer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        writer.success(&mut engine, CHANGE, true);
        let transaction = writer.transaction.txid;
        observer.success(&mut engine, "BEGIN; SELECT 1", true);
        engine.publication_observer = Some((observe_owners, observer.transaction.txid));
        crate::mem::guard::forbid_alloc(|| engine.commit_txn(&mut writer.transaction, &writer.guc))
            .unwrap();
        engine.publication_observer = None;
        assert!(matches!(
            engine.transaction_publication,
            PublicationState::Idle
        ));
        assert_eq!(
            engine
                .storage
                .transaction_status(u64::from(transaction))
                .unwrap(),
            Some("committed")
        );
        assert_eq!(engine.storage.advisory_lock_view_count(), 0);
        assert_eq!(
            data_rows(&observer.success(
                &mut engine,
                "SELECT amount FROM publication_orders; SELECT total FROM publication_invoices; \
             SELECT obj_description('publication_orders'::regclass)",
                true
            )),
            ["20", "20", "confirmed order"]
        );
        observer.success(&mut engine,
            "LOCK TABLE publication_orders,publication_invoices IN ACCESS EXCLUSIVE MODE NOWAIT; COMMIT", true);
    }
}

#[test]
fn transaction_publication_partial_failure_requires_recovery_without_undoing_durable_data() {
    let faults: [crate::sql::commit::PublicationObserver; 4] = [
        inject_definitions,
        inject_row,
        inject_installed,
        inject_visible,
    ];
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for (point, fault) in faults.into_iter().enumerate() {
            for remote in [false, true] {
                let name = format!("publication-fault-{mode:?}-{point}-{remote}");
                let mut config = test_config(&name);
                config.transaction_mode = mode;
                config.object_store_on = remote;
                config.object_store_sim = remote;
                config.wal_upload = remote;
                config.wal_upload_sync = remote;
                config.object_store_bucket = format!("{name}-{}", std::process::id());
                if remote {
                    crate::object_store::sim::drop_namespace(&config.object_store_bucket);
                }
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
                let mut observer =
                    ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
                writer.success(&mut engine, SETUP, !remote);
                writer.success(&mut engine, CHANGE, !remote);
                let transaction = writer.transaction.txid;
                observer.success(&mut engine, "BEGIN; SELECT 1", !remote);
                let observer_id = observer.transaction.txid;
                engine.publication_observer = Some((fault, observer_id));
                let mut commit = || engine.commit_txn(&mut writer.transaction, &writer.guc);
                let error = if remote {
                    commit()
                } else {
                    crate::mem::guard::forbid_alloc(commit)
                }
                .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
                assert!(error.message.as_str().starts_with("injected "));
                assert!(matches!(
                    engine.transaction_publication,
                    PublicationState::RecoveryRequired(_)
                ));
                assert!(engine.transaction_publication.owns(transaction));
                engine.publication_observer = None;
                engine.rollback_txn(&mut writer.transaction, &writer.guc);
                assert_eq!(engine.checkpoint().unwrap_err().sqlstate, "08007");
                assert_eq!(engine.commit_wal().unwrap_err().sqlstate, "08007");
                assert!(!engine.checkpoint_can_progress());
                assert_eq!(engine.checkpoint_beat(), CheckpointBeat::LocalFailure);
                assert_eq!(
                    engine.fatal_commit_failure().unwrap().message,
                    error.message
                );
                assert_eq!(
                    engine
                        .storage
                        .transaction_status(u64::from(transaction))
                        .unwrap(),
                    Some("in progress")
                );
                observe_owners(&engine, PublicationPoint::Visible, observer_id).unwrap();
                assert_eq!(
                    error_sqlstates(&observer.execute(
                        &mut engine,
                        "SELECT total FROM publication_invoices",
                        !remote
                    )),
                    ["08007"]
                );
                assert_eq!(
                    error_sqlstates(&writer.execute(&mut engine, "SELECT 1", !remote)),
                    ["08007"]
                );
                assert_eq!(
                    engine
                        .commit_txn(&mut writer.transaction, &writer.guc)
                        .unwrap_err()
                        .sqlstate,
                    "08007"
                );
                drop(observer);
                drop(writer);
                drop(engine);
                if remote {
                    std::fs::remove_dir_all(&config.data_dir).unwrap();
                }
                let mut cold_budget = test_engine_budget(&config, 1 << 29);
                let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
                let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
                assert_eq!(data_rows(&session.success(&mut cold,
                    "SELECT amount FROM publication_orders; SELECT total FROM publication_invoices; \
                     SELECT obj_description('publication_orders'::regclass)", !remote)),
                    ["20", "20", "confirmed order"]);
                assert_eq!(cold.storage.advisory_lock_view_count(), 0);
                assert!(cold.fatal_commit_failure().is_none());
            }
        }
    }
}

#[test]
fn transaction_publication_prepared_owners_survive_live_and_recovered_resolution() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for recovered in [false, true] {
            for commit in [false, true] {
                let name = format!("publication-prepared-{mode:?}-{recovered}-{commit}");
                let mut config = test_config(&name);
                config.transaction_mode = mode;
                config.max_prepared_transactions = 1;
                config.object_store_on = true;
                config.object_store_sim = true;
                config.wal_upload = true;
                config.wal_upload_sync = true;
                config.object_store_bucket = format!("{name}-{}", std::process::id());
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
                writer.success(&mut engine, SETUP, false);
                writer.success(&mut engine, CHANGE, false);
                let prepared = writer.transaction.txid;
                writer.success(
                    &mut engine,
                    "PREPARE TRANSACTION 'publication-order'",
                    false,
                );
                drop(writer);
                if recovered {
                    drop(engine);
                    std::fs::remove_dir_all(&config.data_dir).unwrap();
                    budget = test_engine_budget(&config, 1 << 29);
                    engine = Engine::new(&config, &mut budget).unwrap();
                }
                let mut resolving = ConfiguredTransactionSession::new(&config, &mut budget);
                let mut observer =
                    ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
                observer.success(&mut engine, "BEGIN; SELECT 1", false);
                engine.bind_query_connection(resolving.connection_id);
                engine
                    .ensure_txn(
                        &mut resolving.transaction,
                        TxnMode::Implicit,
                        &resolving.guc,
                    )
                    .unwrap();
                let coordinator = resolving.transaction.txid;
                engine.storage.assign_transaction_identity(coordinator);
                engine.publication_observer = Some((observe_owners, observer.transaction.txid));
                engine
                    .resolve_prepared_transaction(
                        ast::PreparedTransactionId::parse("publication-order").unwrap(),
                        commit,
                        &mut resolving.transaction,
                        &resolving.guc,
                    )
                    .unwrap();
                engine.publication_observer = None;
                assert_eq!(
                    engine
                        .storage
                        .transaction_status(u64::from(coordinator))
                        .unwrap(),
                    Some("committed")
                );
                assert_eq!(
                    engine
                        .storage
                        .transaction_status(u64::from(prepared))
                        .unwrap(),
                    Some(if commit { "committed" } else { "aborted" })
                );
                assert!(matches!(
                    engine.transaction_publication,
                    PublicationState::Idle
                ));
                let sql = if commit {
                    "SELECT amount FROM publication_orders; SELECT total FROM publication_invoices"
                } else {
                    "SELECT total FROM publication_orders; SELECT total FROM publication_invoices"
                };
                let expected = if commit { "20" } else { "10" };
                assert_eq!(
                    data_rows(&observer.success(&mut engine, sql, false)),
                    [expected, expected]
                );
                observer.success(&mut engine,
                    "LOCK TABLE publication_orders,publication_invoices IN ACCESS EXCLUSIVE MODE NOWAIT; COMMIT", false);
                assert_eq!(engine.storage.advisory_lock_view_count(), 0);
            }
        }
    }
}

#[test]
fn transaction_publication_rollback_restores_rows_and_definitions_before_unlocking() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let mut config = test_config(&format!("publication-rollback-{mode:?}"));
        config.transaction_mode = mode;
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut observer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        writer.success(&mut engine, CHANGE, true);
        let transaction = writer.transaction.txid;
        observer.success(&mut engine, "BEGIN; SELECT 1", true);
        engine.publication_observer = Some((observe_owners, observer.transaction.txid));
        crate::mem::guard::forbid_alloc(|| {
            engine.rollback_txn(&mut writer.transaction, &writer.guc)
        });
        engine.publication_observer = None;
        assert_eq!(
            engine
                .storage
                .transaction_status(u64::from(transaction))
                .unwrap(),
            Some("aborted")
        );
        assert_eq!(
            data_rows(&observer.success(
                &mut engine,
                "SELECT total FROM publication_orders; SELECT total FROM publication_invoices; \
             SELECT obj_description('publication_orders'::regclass) IS NULL",
                true
            )),
            ["10", "10", "t"]
        );
        observer.success(&mut engine,
            "LOCK TABLE publication_orders,publication_invoices IN ACCESS EXCLUSIVE MODE NOWAIT; COMMIT", true);
        assert_eq!(engine.storage.advisory_lock_view_count(), 0);
    }
}

#[test]
fn transaction_publication_queued_failure_preserves_later_durable_work_for_recovery() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        let name = format!("publication-queued-fault-{mode:?}");
        let mut config = test_config(&name);
        config.transaction_mode = mode;
        config.object_store_on = true;
        config.object_store_sim = true;
        config.wal_upload = true;
        config.wal_upload_sync = true;
        config.object_store_bucket = format!("{name}-{}", std::process::id());
        crate::object_store::sim::drop_namespace(&config.object_store_bucket);
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut later =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        let mut observer =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(3);
        writer.success(&mut engine, SETUP, false);
        writer.success(
            &mut engine,
            "CREATE TABLE publication_later(id integer)",
            false,
        );
        writer.success(&mut engine, CHANGE, false);
        let transaction = writer.transaction.txid;
        later.success(
            &mut engine,
            "BEGIN; INSERT INTO publication_later VALUES(30)",
            false,
        );
        observer.success(&mut engine, "BEGIN; SELECT 1", false);
        engine.enable_async_durability();
        engine.bind_query_connection(writer.connection_id);
        assert_eq!(
            crate::mem::guard::forbid_alloc(
                || engine.commit_txn(&mut writer.transaction, &writer.guc)
            )
            .unwrap_err()
            .sqlstate,
            sqlstate::INTERNAL_IO_WAIT
        );
        engine.bind_query_connection(later.connection_id);
        assert_eq!(
            crate::mem::guard::forbid_alloc(
                || engine.commit_txn(&mut later.transaction, &later.guc)
            )
            .unwrap_err()
            .sqlstate,
            sqlstate::INTERNAL_IO_WAIT
        );
        let frontier = engine.visible_lsn;
        engine.publication_observer = Some((inject_row, observer.transaction.txid));
        let mut failed = false;
        for _ in 0..16 {
            if let Err(error) = engine
                .advance_wal_publication(std::time::Instant::now(), CommitProgressMode::Reactor)
            {
                assert_eq!(error.sqlstate, sqlstate::TRANSACTION_RESOLUTION_UNKNOWN);
                failed = true;
                break;
            }
        }
        assert!(failed, "injected publication failure was not reached");
        assert_eq!(engine.visible_lsn, frontier);
        assert!(engine.durable_lsn >= later.transaction.staged_commit().unwrap().lsn);
        assert!(writer.transaction.staged_commit().is_some());
        assert!(engine.transaction_publication.owns(transaction));
        engine.publication_observer = None;
        engine.rollback_txn(&mut writer.transaction, &writer.guc);
        engine.rollback_txn(&mut later.transaction, &later.guc);
        assert_eq!(
            engine.fatal_commit_failure().unwrap().sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert_eq!(
            engine
                .finish_staged_transaction(&mut writer.transaction, &writer.guc)
                .unwrap_err()
                .sqlstate,
            "08007"
        );
        assert_eq!(
            engine
                .finish_staged_transaction(&mut later.transaction, &later.guc)
                .unwrap_err()
                .sqlstate,
            "08007"
        );
        assert_eq!(engine.checkpoint().unwrap_err().sqlstate, "08007");
        drop(observer);
        drop(later);
        drop(writer);
        drop(engine);
        std::fs::remove_dir_all(&config.data_dir).unwrap();
        let mut cold_budget = test_engine_budget(&config, 1 << 29);
        let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
        let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
        assert_eq!(data_rows(&session.success(&mut cold,
            "SELECT amount FROM publication_orders; SELECT total FROM publication_invoices; SELECT id FROM publication_later", false)),
            ["20", "20", "30"]);
    }
}

#[test]
fn transaction_publication_failed_prepared_resolution_keeps_owners_and_recovers_the_decision() {
    for mode in [
        crate::config::TransactionMode::Mvcc,
        crate::config::TransactionMode::Locking,
    ] {
        for recovered in [false, true] {
            for commit in [false, true] {
                let name = format!("publication-prepared-fault-{mode:?}-{recovered}-{commit}");
                let mut config = test_config(&name);
                config.transaction_mode = mode;
                config.max_prepared_transactions = 1;
                config.object_store_on = true;
                config.object_store_sim = true;
                config.wal_upload = true;
                config.wal_upload_sync = true;
                config.object_store_bucket = format!("{name}-{}", std::process::id());
                crate::object_store::sim::drop_namespace(&config.object_store_bucket);
                let mut budget = test_engine_budget(&config, 1 << 29);
                let mut engine = Engine::new(&config, &mut budget).unwrap();
                let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
                writer.success(&mut engine, SETUP, false);
                writer.success(&mut engine, CHANGE, false);
                let prepared = writer.transaction.txid;
                writer.success(
                    &mut engine,
                    "PREPARE TRANSACTION 'publication-order'",
                    false,
                );
                drop(writer);
                if recovered {
                    drop(engine);
                    std::fs::remove_dir_all(&config.data_dir).unwrap();
                    budget = test_engine_budget(&config, 1 << 29);
                    engine = Engine::new(&config, &mut budget).unwrap();
                }
                let mut resolving = ConfiguredTransactionSession::new(&config, &mut budget);
                let mut observer =
                    ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
                observer.success(&mut engine, "BEGIN; SELECT 1", false);
                engine.bind_query_connection(resolving.connection_id);
                engine
                    .ensure_txn(
                        &mut resolving.transaction,
                        TxnMode::Implicit,
                        &resolving.guc,
                    )
                    .unwrap();
                let coordinator = resolving.transaction.txid;
                engine.storage.assign_transaction_identity(coordinator);
                engine.publication_observer = Some((inject_prepared, observer.transaction.txid));
                let gid = ast::PreparedTransactionId::parse("publication-order").unwrap();
                let error = engine
                    .resolve_prepared_transaction(
                        gid,
                        commit,
                        &mut resolving.transaction,
                        &resolving.guc,
                    )
                    .unwrap_err();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
                assert_eq!(engine.prepared_transactions.find(gid), Some(0));
                assert!(engine.transaction_publication.owns(prepared));
                assert!(engine.transaction_publication.owns(coordinator));
                engine.publication_observer = None;
                engine.rollback_txn(&mut resolving.transaction, &resolving.guc);
                for transaction in [prepared, coordinator] {
                    assert_eq!(
                        engine
                            .storage
                            .transaction_status(u64::from(transaction))
                            .unwrap(),
                        Some("in progress")
                    );
                }
                observe_owners(
                    &engine,
                    PublicationPoint::Visible,
                    observer.transaction.txid,
                )
                .unwrap();
                assert_eq!(engine.checkpoint().unwrap_err().sqlstate, "08007");
                drop(observer);
                drop(resolving);
                drop(engine);
                std::fs::remove_dir_all(&config.data_dir).unwrap();
                let mut cold_budget = test_engine_budget(&config, 1 << 29);
                let mut cold = Engine::new(&config, &mut cold_budget).unwrap();
                let mut session = ConfiguredTransactionSession::new(&config, &mut cold_budget);
                assert!(cold.prepared_transactions.find(gid).is_none());
                let sql = if commit {
                    "SELECT amount FROM publication_orders; SELECT total FROM publication_invoices"
                } else {
                    "SELECT total FROM publication_orders; SELECT total FROM publication_invoices"
                };
                let expected = if commit { "20" } else { "10" };
                assert_eq!(
                    data_rows(&session.success(&mut cold, sql, false)),
                    [expected, expected]
                );
            }
        }
    }
}
