use super::*;
use crate::sql::commit::{PublicationPoint, PublicationState};

impl Engine {
    pub(crate) fn publication_storage_for_tests(&self) -> &Storage {
        &self.storage
    }
}

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
    match engine.transaction_publication {
        PublicationState::Installing(_) => assert_eq!(
            engine
                .storage
                .statement_snapshot_lsn(observer)
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_LOCK_WAIT,
            "new reader entered unfinished publication at {point:?}"
        ),
        PublicationState::RecoveryRequired(_) => assert_eq!(
            engine
                .storage
                .statement_snapshot_lsn(observer)
                .unwrap_err()
                .sqlstate,
            sqlstate::TRANSACTION_RESOLUTION_UNKNOWN,
            "new reader entered failed publication at {point:?}"
        ),
        PublicationState::Idle => {}
    }
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
                engine.storage.visible_lsn() < publication.lsn,
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

#[test]
fn transaction_admission_sql_preserves_complete_transactions_across_isolation_levels() {
    for isolation in ["READ COMMITTED", "REPEATABLE READ"] {
        let config = test_config(&format!("admission-isolation-{isolation}"));
        let mut budget = test_engine_budget(&config, 1 << 29);
        let mut engine = Engine::new(&config, &mut budget).unwrap();
        let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
        let mut reader =
            ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
        writer.success(&mut engine, SETUP, true);
        reader.success(
            &mut engine,
            &format!("BEGIN ISOLATION LEVEL {isolation}"),
            true,
        );
        let query = "SELECT o.total,i.total FROM publication_orders o JOIN publication_invoices i ON o.id=i.id";
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            ["10|10"]
        );
        writer.success(
            &mut engine,
            "BEGIN; UPDATE publication_orders SET total=20",
            true,
        );
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            ["10|10"]
        );
        writer.success(
            &mut engine,
            "UPDATE publication_invoices SET total=20; COMMIT",
            true,
        );
        let expected = if isolation == "REPEATABLE READ" {
            "10|10"
        } else {
            "20|20"
        };
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            [expected]
        );
        reader.success(&mut engine, "COMMIT", true);
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            ["20|20"]
        );
        writer.success(&mut engine, "BEGIN; UPDATE publication_orders SET total=30; UPDATE publication_invoices SET total=30; ROLLBACK", true);
        assert_eq!(
            data_rows(&reader.success(&mut engine, query, true)),
            ["20|20"]
        );
    }
}

#[test]
fn transaction_admission_sql_retries_keep_one_command_and_do_not_repeat_effects() {
    let config = test_config("admission-wait-before-command");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(&mut engine, SETUP, true);
    writer.success(&mut engine, "CREATE SEQUENCE admission_counter", true);
    writer.success(&mut engine, "BEGIN", true);
    reader.success(&mut engine, "BEGIN ISOLATION LEVEL REPEATABLE READ", true);
    let command = reader.transaction.command_id();
    let installation = engine
        .storage
        .begin_transaction_installation(writer.transaction.txid, engine.storage.visible_lsn())
        .unwrap();
    for _ in 0..2 {
        let output = reader.execute(&mut engine, "SELECT nextval('admission_counter')", true);
        assert!(
            output.is_empty(),
            "an unadmitted statement emitted a response"
        );
        assert_eq!(reader.transaction.command_id(), command + 1);
        assert!(!reader.transaction.snapshot_taken());
        assert_eq!(reader.transaction.snapshot_lsn(), None);
    }
    installation.complete();
    assert_eq!(
        data_rows(&reader.success(&mut engine, "SELECT nextval('admission_counter')", true)),
        ["1"]
    );
    assert_eq!(reader.transaction.command_id(), command + 1);
    assert_eq!(
        data_rows(&reader.success(&mut engine, "SELECT nextval('admission_counter')", true)),
        ["2"]
    );
    reader.success(&mut engine, "ROLLBACK", true);
    writer.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn transaction_admission_fast_path_yields_before_effects_and_resumes_once() {
    let config = test_config("admission-fast-path-resume");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(&mut engine, "BEGIN", true);
    reader.success(&mut engine, "BEGIN", true);
    let command = reader.transaction.command_id();
    let installing = engine
        .storage
        .begin_transaction_installation(writer.transaction.txid, engine.storage.visible_lsn())
        .unwrap();
    let call = |engine: &mut Engine, reader: &mut ConfiguredTransactionSession| {
        reader.send.clear();
        let mut responder = Responder::new(&mut reader.send);
        crate::mem::guard::forbid_alloc(|| {
            engine.execute_large_object_fast_path(
                715,
                &[Datum::Oid(4242)],
                true,
                &reader.arena,
                &mut reader.transaction,
                &reader.guc,
                &mut reader.cursors,
                &mut responder,
                reader.connection_id,
            )
        })
        .unwrap()
    };
    assert_eq!(
        call(&mut engine, &mut reader),
        ExtendedExecutionStatus::Blocked {
            wait: StatementWait::Lock
        }
    );
    assert!(reader.send.readable().is_empty());
    assert_eq!(
        engine.statement_wait_generation(StatementWait::Lock, reader.connection_id),
        engine.lock_generation()
    );
    assert_eq!(reader.transaction.command_id(), command + 1);
    assert!(reader.transaction.suspended_statement_context().is_some());
    assert_eq!(query::statement_deadline(), 0);
    assert_eq!(
        engine
            .storage
            .large_objects_visible_to(reader.transaction.txid)
            .count(),
        0
    );
    installing.complete();
    assert_eq!(
        engine.statement_wait_generation(StatementWait::Lock, reader.connection_id),
        0,
        "release before reactor parking must force an immediate retry"
    );
    assert_eq!(
        call(&mut engine, &mut reader),
        ExtendedExecutionStatus::Complete(true)
    );
    assert_eq!(reader.transaction.command_id(), command + 1);
    assert!(reader.transaction.suspended_statement_context().is_none());
    assert_eq!(
        engine
            .storage
            .large_objects_visible_to(reader.transaction.txid)
            .count(),
        1
    );
    reader.success(&mut engine, "COMMIT", true);
    assert_eq!(
        data_rows(&reader.success(&mut engine, "SELECT oid FROM pg_largeobject_metadata", true)),
        ["4242"]
    );
    writer.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn transaction_admission_fast_path_expired_wait_has_no_effects_and_can_roll_back() {
    let config = test_config("admission-fast-path-timeout");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(&mut engine, "BEGIN", true);
    reader.success(&mut engine, "BEGIN", true);
    let installing = engine
        .storage
        .begin_transaction_installation(writer.transaction.txid, engine.storage.visible_lsn())
        .unwrap();
    let call = |engine: &mut Engine, reader: &mut ConfiguredTransactionSession| {
        reader.send.clear();
        let mut responder = Responder::new(&mut reader.send);
        crate::mem::guard::forbid_alloc(|| {
            engine.execute_large_object_fast_path(
                715,
                &[Datum::Oid(4242)],
                true,
                &reader.arena,
                &mut reader.transaction,
                &reader.guc,
                &mut reader.cursors,
                &mut responder,
                reader.connection_id,
            )
        })
        .unwrap()
    };
    assert_eq!(
        call(&mut engine, &mut reader),
        ExtendedExecutionStatus::Blocked {
            wait: StatementWait::Lock
        }
    );
    // Expire the saved deadline without sleeping or changing timeout limits.
    reader.transaction.release_suspended_statement();
    reader.transaction.suspend_statement(
        None,
        datetime::statement_clock(),
        1,
        exec::PlpgsqlTransactionContext::Atomic,
    );
    assert_eq!(
        call(&mut engine, &mut reader),
        ExtendedExecutionStatus::Complete(false)
    );
    assert!(String::from_utf8_lossy(reader.send.readable()).contains("57014"));
    assert!(reader.transaction.failed);
    assert!(reader.transaction.suspended_statement_context().is_none());
    assert_eq!(
        engine
            .storage
            .large_objects_visible_to(reader.transaction.txid)
            .count(),
        0
    );
    assert_eq!(query::statement_deadline(), 0);
    installing.complete();
    reader.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        data_rows(&reader.success(&mut engine, "SELECT lo_create(4242)", true)),
        ["4242"]
    );
    writer.success(&mut engine, "ROLLBACK", true);
}

#[test]
fn transaction_admission_fast_path_private_object_wait_resumes_once() {
    let config = test_config("admission-fast-path-object-owner");
    let mut budget = test_engine_budget(&config, 1 << 29);
    let mut engine = Engine::new(&config, &mut budget).unwrap();
    let mut writer = ConfiguredTransactionSession::new(&config, &mut budget);
    let mut reader = ConfiguredTransactionSession::new(&config, &mut budget).with_connection_id(2);
    writer.success(&mut engine, "BEGIN; SELECT lo_create(4242)", true);
    reader.success(&mut engine, "BEGIN", true);
    let command = reader.transaction.command_id();
    let call = |engine: &mut Engine, reader: &mut ConfiguredTransactionSession| {
        reader.send.clear();
        let mut responder = Responder::new(&mut reader.send);
        crate::mem::guard::forbid_alloc(|| {
            engine.execute_large_object_fast_path(
                715,
                &[Datum::Oid(4242)],
                true,
                &reader.arena,
                &mut reader.transaction,
                &reader.guc,
                &mut reader.cursors,
                &mut responder,
                reader.connection_id,
            )
        })
        .unwrap()
    };
    assert_eq!(
        call(&mut engine, &mut reader),
        ExtendedExecutionStatus::Blocked {
            wait: StatementWait::Lock
        }
    );
    assert!(
        reader.send.readable().is_empty(),
        "an internal wait escaped onto the wire"
    );
    assert_eq!(reader.transaction.command_id(), command + 1);
    writer.success(&mut engine, "ROLLBACK", true);
    assert_eq!(
        call(&mut engine, &mut reader),
        ExtendedExecutionStatus::Complete(true)
    );
    assert_eq!(reader.transaction.command_id(), command + 1);
    reader.success(&mut engine, "COMMIT", true);
    assert_eq!(
        data_rows(&reader.success(&mut engine, "SELECT oid FROM pg_largeobject_metadata", true)),
        ["4242"]
    );
    let duplicate = reader.execute(&mut engine, "SELECT lo_create(4242)", true);
    assert!(String::from_utf8_lossy(&duplicate).contains("23505"));
    assert!(String::from_utf8_lossy(&duplicate).contains("pg_largeobject_metadata_oid_index"));
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
        let frontier = engine.storage.visible_lsn();
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
        assert_eq!(engine.storage.visible_lsn(), frontier);
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
