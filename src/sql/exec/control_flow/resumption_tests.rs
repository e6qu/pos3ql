use super::*;
use crate::config::{Config, TransactionMode};
use crate::mem::{Budget, FixedBuf};
use crate::sql::guc::GucState;
use crate::sql::lock::{AdvisoryDecision, AdvisoryKey, AdvisoryMode};

#[test]
fn real_lock_waits_resume_nested_procedural_work_without_repeating_prior_statements() {
    let mut test_cursors = crate::sql::cursor::CursorPool::for_tests();
    let mut test_row_buffers = crate::storage::RowCursorBuffers::for_tests();
    const SOURCE: &str = "DECLARE n integer := 0; marker text := 'seed'; BEGIN \
        PERFORM pg_advisory_lock(41); n := n + 1; \
        FOR i IN 1..2 LOOP marker := marker || i::text; \
        BEGIN PERFORM pg_advisory_xact_lock(1234); \
        EXCEPTION WHEN OTHERS THEN n := 999; END; n := n + 1; END LOOP; \
        ASSERT n = 3; ASSERT marker = 'seed12'; END";
    for mode in [TransactionMode::Mvcc, TransactionMode::Locking] {
        let mut config = Config::default_dev();
        config.transaction_mode = mode;
        config.max_connections = 3;
        config.max_tables = 1;
        config.memtable_bytes = 64 << 10;
        config.txn_rows = 8;
        config.max_ddl_per_transaction = 1;
        config.row_history_slots = 8;
        config.row_overlay_slots = 8;
        config.max_catalog_versions_per_object = 1;
        config.catalog_history_bytes = 4096;
        config.max_spill_generations_per_table = 1;
        config.definition_image_slots = 8;
        config.table_metadata_bytes = 64 << 10;
        config.table_metadata_leases = 128;
        config.execution_control_bytes = 64 << 10;
        config.execution_control_leases = 64;
        config.row_work_bytes = 128 << 10;
        config.row_work_leases = 64;
        let mut budget =
            Budget::new(config.memtable_bytes + Storage::extra_budget_bytes(&config) + (32 << 20));
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        storage.configure_collation(&config, &mut budget).unwrap();
        let mut transaction = TxnState::new_with_config(&mut budget, &config).unwrap();
        transaction.txid = 100;
        transaction.mode = TxnMode::Explicit;
        transaction.begin_command().unwrap();
        let guc = GucState::new_with_capacities(&mut budget, 1, 8, mode).unwrap();
        let original = Arena::new(&mut budget, "waiting procedure source", 4 << 20).unwrap();
        let mut resumed = Arena::new(&mut budget, "waiting procedure resumption", 4 << 20).unwrap();
        let mut send = FixedBuf::new(&mut budget, "waiting procedure output", 4096).unwrap();
        let mut responder = Responder::new(&mut send);
        let _deadline = crate::sql::query::enter_deadline_scope();
        crate::sql::query::disarm_timeout();
        let key = AdvisoryKey::from_bigint(1234);
        let prior_identity = crate::sql::execution_identity();
        crate::sql::set_execution_connection_id(2);
        storage.begin_transaction_identity(200).unwrap();
        assert_eq!(
            storage
                .acquire_advisory_lock(200, key, AdvisoryMode::Exclusive, true, false)
                .unwrap(),
            AdvisoryDecision::Acquired
        );
        crate::sql::set_execution_connection_id(1);
        storage
            .begin_transaction_identity(transaction.txid)
            .unwrap();
        let parent_command = transaction.command_id();
        let _parent_snapshot = storage.dml_expression_snapshot(parent_command);
        let mut nested_command = parent_command;
        let saved = crate::mem::guard::forbid_alloc(|| {
            let mut command = transaction.enter_trigger_sql(&storage).unwrap();
            assert!(command.command_id() > parent_command);
            nested_command = command.command_id();
            assert_eq!(command.trigger_depth(), 1);
            let (program, code) = compile_plpgsql_execution(
                "inline_code_block",
                SOURCE,
                PlpgsqlProgramKind::Anonymous,
                0,
                &storage,
                command.txid,
                &original,
                &|_| None,
            )
            .unwrap();
            let mut execution = execution_frame::ExecutionFrame::new(
                &original,
                &code,
                program.locals,
                0,
                &storage.execution_control,
                &storage.row_work,
            )
            .unwrap();
            let invocation = crate::sql::type_input_plans::with_owner(
                crate::sql::type_input_plans::Owner::Backend,
                || {
                    TriggerInvocation::anonymous(crate::sql::guc::PlpgsqlSettings {
                        variable_conflict: crate::sql::guc::VariableConflict::Error,
                        print_strict_params: false,
                    })
                },
            )
            .unwrap()
            .with_input_sites(program.input_sites);
            let _settings = crate::sql::guc::enter_eval_scope(&guc, &mut command);
            let mut context = TriggerExecContext {
                host: PlpgsqlExecHost::Atomic {
                    storage: &mut storage,
                    services: crate::sql::exec::TriggerServices::new(
                        guc.seq_session(),
                        &mut test_cursors,
                        &mut test_row_buffers,
                    ),
                },
                txn: &mut command,
                arena: &original,
                params: crate::sql::eval::NO_PARAMS,
                responder: &mut responder,
            };
            let definition = crate::storage::RelationDefinition::EMPTY;
            execution
                .initialize(
                    &mut context,
                    invocation,
                    TriggerTransition {
                        definition,
                        old: TriggerRecordValue::Empty,
                        new: TriggerRecordValue::Empty,
                    },
                    &code,
                    &[],
                )
                .unwrap();
            let error = execution
                .execute_block(
                    &mut context,
                    definition,
                    invocation,
                    &mut TriggerRows::none(),
                    None,
                    &code,
                )
                .err()
                .unwrap();
            assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT, "{mode:?}");
            assert!(execution.status.control_frames.as_ref().unwrap().active > 1);
            let execution = execution.suspend().ok().unwrap();
            (execution, command.suspend())
        });
        assert_eq!(transaction.command_id(), parent_command);
        assert_eq!(transaction.trigger_depth(), 0);
        assert_eq!(storage.read_snapshot(), parent_command);
        drop(original);
        let mut saved = Some(saved);
        for attempt in 0..2 {
            if attempt == 1 {
                crate::sql::set_execution_connection_id(3);
                storage.begin_transaction_identity(300).unwrap();
                assert_eq!(
                    storage
                        .acquire_advisory_lock(
                            300,
                            AdvisoryKey::from_bigint(41),
                            AdvisoryMode::Exclusive,
                            true,
                            true,
                        )
                        .unwrap(),
                    AdvisoryDecision::Unavailable
                );
                assert_eq!(
                    storage
                        .acquire_advisory_lock(
                            300,
                            AdvisoryKey::from_bigint(5678),
                            AdvisoryMode::Exclusive,
                            true,
                            true,
                        )
                        .unwrap(),
                    AdvisoryDecision::Acquired
                );
                storage.release_advisory_transaction_locks(300);
                storage.finish_transaction_identity(300, true);
                let waiting = storage.lock_wait_generation(1);
                storage.release_advisory_transaction_locks(200);
                assert_ne!(storage.lock_wait_generation(1), waiting);
                storage.finish_transaction_identity(200, true);
                crate::sql::set_execution_connection_id(1);
            }
            saved = crate::mem::guard::forbid_alloc(|| {
                let (saved, command) = saved.take().unwrap();
                crate::sql::set_execution_connection_id(2);
                let (command, error) = command.resume(&mut transaction).err().unwrap();
                assert_eq!(error.sqlstate, sqlstate::SERIALIZATION_FAILURE);
                assert_eq!(transaction.command_id(), parent_command);
                assert_eq!(transaction.trigger_depth(), 0);
                assert_eq!(storage.read_snapshot(), parent_command);
                crate::sql::set_execution_connection_id(1);
                let mut command = command.resume(&mut transaction).ok().unwrap();
                assert_eq!(command.trigger_depth(), 1);
                assert_eq!(command.command_id(), nested_command);
                assert_eq!(storage.read_snapshot(), crate::storage::SNAPSHOT_ALL);
                let (program, code) = saved.source().parse(&resumed, None).unwrap();
                let mut execution = saved.restore(&resumed, &code).ok().unwrap();
                let invocation = crate::sql::type_input_plans::with_owner(
                    crate::sql::type_input_plans::Owner::Backend,
                    || {
                        TriggerInvocation::anonymous(crate::sql::guc::PlpgsqlSettings {
                            variable_conflict: crate::sql::guc::VariableConflict::Error,
                            print_strict_params: false,
                        })
                    },
                )
                .unwrap()
                .with_input_sites(program.input_sites);
                let _settings = crate::sql::guc::enter_eval_scope(&guc, &mut command);
                let mut context = TriggerExecContext {
                    host: PlpgsqlExecHost::Atomic {
                        storage: &mut storage,
                        services: crate::sql::exec::TriggerServices::new(
                            guc.seq_session(),
                            &mut test_cursors,
                            &mut test_row_buffers,
                        ),
                    },
                    txn: &mut command,
                    arena: &resumed,
                    params: crate::sql::eval::NO_PARAMS,
                    responder: &mut responder,
                };
                let definition = crate::storage::RelationDefinition::EMPTY;
                let outcome = execution.execute_block(
                    &mut context,
                    definition,
                    invocation,
                    &mut TriggerRows::none(),
                    None,
                    &code,
                );
                if attempt == 0 {
                    assert_eq!(
                        outcome.err().unwrap().sqlstate,
                        sqlstate::INTERNAL_LOCK_WAIT,
                        "{mode:?}"
                    );
                    let execution = execution.suspend().ok().unwrap();
                    return Some((execution, command.suspend()));
                }
                assert!(outcome.unwrap().is_none());
                assert_eq!(execution.values[0], Datum::Int4(3));
                assert_eq!(execution.values[1], Datum::Text("seed12"));
                assert_eq!(execution.status.found, Some(true));
                assert!(!context.txn.has_savepoints());
                assert!(matches!(
                    execution.status.control_frames.as_ref().unwrap().progress,
                    ExecutionProgress::Complete
                ));
                let error = execution
                    .execute_block(
                        &mut context,
                        definition,
                        invocation,
                        &mut TriggerRows::none(),
                        None,
                        &code,
                    )
                    .err()
                    .unwrap();
                assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
                assert!(error.message.as_str().contains("cannot run again"));
                None
            });
            assert_eq!(transaction.command_id(), parent_command);
            assert_eq!(transaction.trigger_depth(), 0);
            assert_eq!(storage.read_snapshot(), parent_command);
            resumed.reset();
        }
        assert!(saved.is_none());
        let prefix = AdvisoryKey::from_bigint(41);
        assert!(
            storage
                .unlock_advisory_session(prefix, AdvisoryMode::Exclusive)
                .unwrap()
        );
        assert!(
            !storage
                .unlock_advisory_session(prefix, AdvisoryMode::Exclusive)
                .unwrap()
        );
        storage.release_advisory_transaction_locks(100);
        storage.finish_transaction_identity(100, true);
        crate::sql::publish_execution_identity(prior_identity);
    }
}
