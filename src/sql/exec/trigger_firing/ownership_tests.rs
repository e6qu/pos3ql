use super::*;
use crate::mem::FixedBuf;
use crate::sql::guc::GucState;
use crate::sql::lock::{AdvisoryDecision, AdvisoryKey, AdvisoryMode};
use crate::sql::sequence;

#[test]
fn firing_environment_keeps_original_clocks_and_consumed_timeouts() {
    use crate::mem::lease_pool::{LeasePool, Purpose};
    use crate::sql::{datetime, query};

    let _clock = datetime::test_clock::enter(812_345_678_901_234);
    let _session = crate::sql::eval::funcs::system::retain_session_evaluation();
    let _deadline = query::enter_deadline_scope();
    let mut budget = crate::mem::Budget::new(
        TxnState::budget_bytes_with_large_objects(1, 1, 1) + LeasePool::budget_bytes(64, 1),
    );
    let mut txn = TxnState::new_with_large_objects(&mut budget, 1, 1, 1).unwrap();
    let controls = LeasePool::new(&mut budget, Purpose::EXECUTION_CONTROL, 64, 1).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        datetime::begin_statement();
        datetime::begin_transaction();
        query::restore_statement_deadline(812_345_678_902_234);
        let mut schemas = crate::sql::eval::funcs::system::session_schemas();
        schemas.names[0] = crate::util::StackStr::from_str("function_schema");
        schemas.n = 1;
        crate::sql::eval::funcs::system::set_session_schemas(schemas);
        let saved_path = crate::storage::execution_path();
        let active = CallEnvironment::new(None, None);
        let saved = SavedCallEnvironment::capture(&active, &controls).unwrap();
        drop(active);

        let _worker_clock = datetime::test_clock::enter(812_345_678_901_634);
        datetime::begin_statement();
        datetime::begin_transaction();
        query::restore_statement_deadline(812_345_678_901_233);
        schemas.names[0] = crate::util::StackStr::from_str("other_client");
        crate::sql::eval::funcs::system::set_session_schemas(schemas);
        {
            let _active = saved.enter(&txn).unwrap();
            assert_eq!(
                crate::storage::execution_path().entries(),
                saved_path.entries()
            );
            assert_eq!(
                crate::sql::eval::funcs::system::session_schemas().names[0].as_str(),
                "function_schema"
            );
            assert_eq!(datetime::statement_micros(), 812_345_678_901_234);
            assert_eq!(datetime::transaction_micros(), 812_345_678_901_234);
            assert_eq!(query::statement_deadline(), 812_345_678_902_234);
            query::check_timeout_at(812_345_678_902_233).unwrap();
            assert_eq!(
                query::check_timeout_at(812_345_678_902_234)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::QUERY_CANCELED
            );
        }
        assert_eq!(
            crate::sql::eval::funcs::system::session_schemas().names[0].as_str(),
            "other_client"
        );
        assert_eq!(datetime::statement_micros(), 812_345_678_901_634);
        assert_eq!(datetime::transaction_micros(), 812_345_678_901_634);
        assert_eq!(query::statement_deadline(), 812_345_678_901_233);

        query::restore_statement_deadline(812_345_678_902_234);
        txn.catch_query_cancellation(812_345_678_902_234);
        query::restore_statement_deadline(812_345_678_901_233);
        {
            let _active = saved.enter(&txn).unwrap();
            assert_eq!(query::statement_deadline(), 0);
            query::check_timeout_at(i64::MAX).unwrap();
        }
        assert_eq!(query::statement_deadline(), 812_345_678_901_233);

        txn.enter_statement_timeout(crate::sql::StatementTimeoutEntry::Command { timeout_ms: 1 });
        txn.catch_query_cancellation(812_345_678_901_634);
        {
            let _active = saved.enter(&txn).unwrap();
            assert_eq!(query::statement_deadline(), 812_345_678_902_234);
            assert_eq!(
                query::check_timeout_at(812_345_678_902_234)
                    .unwrap_err()
                    .sqlstate,
                sqlstate::QUERY_CANCELED
            );
        }
    });
}

#[test]
fn paused_firing_keeps_rows_arguments_schemas_and_effects_after_source_destruction() {
    let mut test_cursors = crate::sql::cursor::CursorPool::for_tests();
    let _clock = crate::sql::datetime::test_clock::enter(812_345_678_901_234);
    let mut config = crate::storage::tests::test_config();
    config.max_tables = 1;
    config.max_catalog_versions_per_object = 1;
    config.txn_rows = 16;
    config.max_ddl_per_transaction = 2;
    config.row_history_slots = 16;
    config.row_overlay_slots = 16;
    config.catalog_history_bytes = 4096;
    config.definition_image_slots = 4;
    config.table_metadata_bytes = 64 << 10;
    config.table_metadata_leases = 64;
    config.execution_control_bytes = 64 << 10;
    config.execution_control_leases = 64;
    config.row_work_bytes = 64 << 10;
    config.row_work_leases = 32;
    let mut budget = crate::storage::tests::test_budget(&config);
    let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
    let controls = std::sync::Arc::clone(&storage.execution_control);
    let rows = std::sync::Arc::clone(&storage.row_work);
    let original = Arena::new(&mut budget, "trigger firing source", 64 << 10).unwrap();
    let mut resumed = Arena::new(&mut budget, "trigger firing resumption", 64 << 10).unwrap();
    let small = Arena::new(&mut budget, "trigger firing failed admission", 64).unwrap();
    let output = Arena::new(&mut budget, "trigger firing result", 32 << 10).unwrap();
    let statement_arena = Arena::new(&mut budget, "trigger firing journal", 16 << 10).unwrap();
    let mut send = FixedBuf::new(&mut budget, "trigger firing output", 4096).unwrap();
    let mut responder = Responder::new(&mut send);
    let guc = GucState::new_with_capacities(&mut budget, 1, 4, config.transaction_mode).unwrap();
    let _session = crate::sql::eval::funcs::system::retain_session_evaluation();
    crate::sql::datetime::begin_statement();
    crate::sql::datetime::begin_transaction();
    let _caller = crate::sql::eval::funcs::system::enter_current_user(
        crate::storage::SessionIdentity::unauthenticated("caller"),
    );
    guc.begin_transaction();
    guc.set_config("application_name", Some("outer"), false)
        .unwrap();
    crate::sql::eval::funcs::system::set_session_settings(
        &["application_name"],
        &[crate::util::StackStr::from_str("outer")],
        &[crate::util::StackStr::new()],
        &["session"],
    )
    .unwrap();
    let mut transaction = TxnState::new_with_config(&mut budget, &config).unwrap();
    transaction.txid = 100;
    transaction.mode = TxnMode::Explicit;
    transaction.begin_command().unwrap();
    let prior = super::super::super::execution_identity();
    super::super::super::set_execution_connection_id(2);
    storage.begin_transaction_identity(200).unwrap();
    let blocking = AdvisoryKey::from_bigint(42);
    assert_eq!(
        storage
            .acquire_advisory_lock(200, blocking, AdvisoryMode::Exclusive, true, false,)
            .unwrap(),
        AdvisoryDecision::Acquired
    );
    super::super::super::set_execution_connection_id(1);
    storage.begin_transaction_identity(100).unwrap();
    let _deadline = super::super::super::query::enter_deadline_scope();
    super::super::super::query::restore_statement_deadline(812_345_679_901_234);
    let mut columns = [crate::storage::ColumnMeta::EMPTY; 2];
    columns[0].name = SqlName::parse("id").unwrap();
    columns[0].ctype = ColType::Int4;
    columns[1].name = SqlName::parse("note").unwrap();
    columns[1].ctype = ColType::Text;
    let effects = core::cell::Cell::new(0);
    let mut saved = crate::mem::guard::forbid_alloc(|| {
        storage.begin_advisory_statement().unwrap();
        let _statement = sequence::enter_statement(
            &statement_arena,
            guc.seq_session(),
            false,
            Some(storage.advisory_journal()),
        );
        sequence::statement_effect(
            sequence::StatementEffectKind::Cursor,
            &[Datum::Text("before firing")],
            sequence::StatementEffectRetry::Continue,
            &original,
            || {
                effects.set(effects.get() + 1);
                sequence::StatementEffectAttempt {
                    result: Ok(&[Datum::Int8(1)]),
                    rewound_state: false,
                }
            },
        )
        .unwrap();
        let definition = RelationDefinition {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse("selected_rows").unwrap(),
            n_columns: 2,
            columns: &columns,
        };
        let old_note = original.alloc_slice_with(8192, |_| b'x').unwrap();
        let mut old = [
            Datum::Int4(10),
            Datum::Text(core::str::from_utf8(old_note).unwrap()),
        ];
        let mut new = [
            Datum::Int4(11),
            Datum::Text(original.alloc_str("initial").unwrap()),
        ];
        let argv = crate::sql::array::build_shaped(
            &[Datum::Text(original.alloc_str("!").unwrap())],
            crate::sql::array::Shape::new(&[1], &[0]).unwrap(),
            &original,
        )
        .unwrap();
        let source = "DECLARE n integer := 0; marker text := 'once'; \
            anchor timestamp with time zone := statement_timestamp(); BEGIN \
            BEGIN PERFORM $1; RAISE EXCEPTION 'enclosing SQL parameters leaked'; \
                EXCEPTION WHEN SQLSTATE '42P02' THEN NULL; END; \
            n := n + 1; PERFORM set_config('application_name', 'changed', true); \
            PERFORM pg_advisory_lock(41), pg_advisory_xact_lock(42); \
            IF statement_timestamp() <> anchor OR transaction_timestamp() <> anchor THEN \
                RAISE EXCEPTION 'trigger clocks changed'; END IF; \
            NEW.note := OLD.note || (TG_ARGV[0] || marker || current_setting('application_name') || current_user); \
            NEW.id := NEW.id + n; \
            RETURN NEW; END";
        let (program, code) = compile_plpgsql_execution(
            source,
            PlpgsqlProgramKind::Trigger,
            &storage,
            100,
            &original,
            &|_| false,
        )
        .unwrap();
        let transition_row = encode_projected_pub(&new, &original).unwrap();
        let transition_rows = original.alloc_slice_copy(&[transition_row]).unwrap();
        let alias = transition_relation(
            &storage,
            100,
            SqlName::parse("inserted_rows").unwrap(),
            definition,
            transition_rows,
            &original,
        )
        .unwrap();
        let relations = original
            .alloc_slice_copy(&[("inserted_rows", alias)])
            .unwrap();
        let invocation = TriggerInvocation {
            program_kind: PlpgsqlProgramKind::Trigger,
            name: original.alloc_str("selected_trigger").unwrap(),
            routine_schema: original.alloc_str("public").unwrap(),
            routine_name: original.alloc_str("selected_body").unwrap(),
            table_schema: original.alloc_str("public").unwrap(),
            table_name: original.alloc_str("selected_rows").unwrap(),
            relid: 42,
            routine_oid: 43,
            input_origin: super::super::super::plpgsql_inputs::Origin::Routine {
                oid: 43,
                version: 7,
            },
            input_owner: super::super::super::type_input_plans::Owner::Backend,
            input_collation: crate::sql::ast::Collation::None,
            input_sites: program.input_sites,
            variable_conflict: crate::sql::guc::VariableConflict::Error,
            print_strict_params: false,
            fresh_snapshots: true,
            materialized_relations: Some(relations),
            nargs: 1,
            when: "BEFORE",
            level: "ROW",
            operation: "INSERT",
            argv: Datum::Array {
                element: ArrElem::Text,
                raw: argv,
            },
            event: "",
            tag: "",
            exception: None,
        };
        let _settings = super::super::super::guc::enter_eval_scope(&guc, &mut transaction);
        let settings = guc
            .enter_routine_configs(&[crate::storage::RoutineConfig {
                name: SqlName::parse("application_name").unwrap(),
                value: crate::util::StackStr::from_str("bound"),
            }])
            .unwrap();
        let security = crate::sql::eval::funcs::system::enter_current_user(
            crate::storage::SessionIdentity::bound(storage.capture_role_slot(0, 100).unwrap()),
        );
        let definitions = storage.table_definition_images();
        definitions.retain_relation(&definition).unwrap();
        let mut context = TriggerExecContext {
            host: PlpgsqlExecHost::Atomic {
                storage: &mut storage,
                services: crate::sql::exec::TriggerServices::new(
                    guc.seq_session(),
                    &mut test_cursors,
                ),
            },
            txn: &mut transaction,
            arena: &original,
            params: &[Datum::Int4(99)],
            responder: &mut responder,
        };
        let mut firing = TriggerFiring::new(
            &context,
            code,
            program.locals,
            FiringBinding {
                invocation,
                definition,
                rows: super::TriggerRows {
                    old: super::TriggerRecord::new(&mut old, true),
                    new: super::TriggerRecord::new(&mut new, true),
                    before: true,
                },
                caller_new: true,
                level: TriggerLevel::Row,
            },
            CallEnvironment::new(Some(settings), Some(security)),
        )
        .unwrap();
        let error = firing.execute(&mut context).err().unwrap();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_LOCK_WAIT);
        let held = rows.lease::<u8>(64 << 10).unwrap();
        let (firing, error) = firing.suspend(&context, &definitions).err().unwrap();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("row_work_bytes"));
        assert_eq!(
            guc.get_owned("application_name").unwrap().as_str(),
            "changed"
        );
        assert_eq!(
            crate::sql::eval::funcs::system::current_identity().as_str(),
            "postgres"
        );
        drop(held);
        let available = controls
            .lease::<u8>(64 << 10)
            .err()
            .unwrap()
            .available_leases;
        let mut held: [Option<MutableSlice<u8>>; 64] = std::array::from_fn(|_| None);
        for slot in &mut held[..available - 1] {
            *slot = Some(controls.lease::<u8>(1).unwrap());
        }
        let (firing, error) = firing.suspend(&context, &definitions).err().unwrap();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("execution_control_leases"));
        assert_eq!(
            controls
                .lease::<u8>(64 << 10)
                .err()
                .unwrap()
                .available_leases,
            1
        );
        drop(held);
        firing.suspend(&context, &definitions).ok().unwrap()
    });
    columns[0].name = SqlName::parse("replaced_id").unwrap();
    columns[1].ctype = ColType::Bool;
    drop(original);
    let _other_clock = crate::sql::datetime::test_clock::enter(812_345_678_961_234);
    crate::sql::datetime::begin_statement();
    crate::sql::datetime::begin_transaction();
    super::super::super::query::restore_statement_deadline(812_345_678_901_233);
    crate::mem::guard::forbid_alloc(|| {
        storage.preserve_advisory_statement();
        storage.begin_advisory_statement().unwrap();
        let _statement = sequence::enter_statement(
            &statement_arena,
            guc.seq_session(),
            true,
            Some(storage.advisory_journal()),
        );
        assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
        assert_eq!(
            crate::sql::eval::funcs::system::current_identity().as_str(),
            "caller"
        );
        assert_eq!(saved.saved.metadata.header.routine_oid, 43);
        let definition = saved.saved.metadata.definition.relation();
        assert_eq!(definition.columns()[0].name.as_str(), "id");
        assert_eq!(definition.columns()[1].ctype, ColType::Text);
        assert_ne!(definition.columns()[1].ctype, columns[1].ctype);
        let alias = saved.saved.metadata.aliases[0].as_ref().unwrap();
        assert_eq!(alias.types[0].0, 23);
        assert_eq!(alias.types[1].0, 25);
        let _settings = super::super::super::guc::enter_eval_scope(&guc, &mut transaction);
        let mut context = TriggerExecContext {
            host: PlpgsqlExecHost::Atomic {
                storage: &mut storage,
                services: crate::sql::exec::TriggerServices::new(
                    guc.seq_session(),
                    &mut test_cursors,
                ),
            },
            txn: &mut transaction,
            arena: &small,
            params: crate::sql::eval::NO_PARAMS,
            responder: &mut responder,
        };
        let advisory_mark = context.storage().advisory_statement_mark();
        let error = saved.poll(&mut context).err().unwrap();
        assert_eq!(context.storage().advisory_statement_mark(), advisory_mark);
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(small.used(), 0);
        assert!(matches!(
            sequence::expected_input().unwrap(),
            sequence::ExpectedInput::OtherEffect
        ));
        assert_eq!(
            crate::sql::datetime::statement_micros(),
            812_345_678_961_234
        );
        assert_eq!(
            crate::sql::datetime::transaction_micros(),
            812_345_678_961_234
        );
        assert_eq!(crate::sql::query::statement_deadline(), 812_345_678_901_233);
        assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
        assert_eq!(
            crate::sql::eval::funcs::system::current_identity().as_str(),
            "caller"
        );
        assert!(matches!(saved.state, FiringState::Paused(_)));
    });
    crate::mem::guard::forbid_alloc(|| {
        let metadata_bytes = resumed
            .with_remaining_region(|scratch| {
                let FiringState::Paused(frame) = &saved.state else {
                    panic!("paused firing")
                };
                let (parsed, _code) = frame.source().parse(scratch, None).unwrap();
                let values = saved.saved.values.decode(scratch).unwrap();
                let binding = saved
                    .saved
                    .metadata
                    .bind(values, parsed.input_sites, scratch)
                    .unwrap();
                let relations = binding.invocation.materialized_relations.unwrap();
                assert_eq!(relations[0].0, "inserted_rows");
                assert_eq!(relations[0].1.column_types[1].0, 25);
                scratch.used()
            })
            .unwrap();
        assert_eq!(resumed.used(), 0);
        let available = metadata_bytes.next_multiple_of(16);
        assert!(available + 16 < resumed.capacity());
        resumed
            .alloc_slice_with(resumed.capacity() - available, |_| 0xcc_u8)
            .unwrap();
        resumed
            .with_remaining_region(|scratch| {
                storage.preserve_advisory_statement();
                storage.begin_advisory_statement().unwrap();
                let _statement = sequence::enter_statement(
                    &statement_arena,
                    guc.seq_session(),
                    true,
                    Some(storage.advisory_journal()),
                );
                let _settings = super::super::super::guc::enter_eval_scope(&guc, &mut transaction);
                let mut context = TriggerExecContext {
                    host: PlpgsqlExecHost::Atomic {
                        storage: &mut storage,
                        services: crate::sql::exec::TriggerServices::new(
                            guc.seq_session(),
                            &mut test_cursors,
                        ),
                    },
                    txn: &mut transaction,
                    arena: scratch,
                    params: crate::sql::eval::NO_PARAMS,
                    responder: &mut responder,
                };
                let advisory_mark = context.storage().advisory_statement_mark();
                let error = saved.poll(&mut context).err().unwrap();
                assert_eq!(context.storage().advisory_statement_mark(), advisory_mark);
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(scratch.used(), 0);
                assert!(matches!(
                    sequence::expected_input().unwrap(),
                    sequence::ExpectedInput::OtherEffect
                ));
                assert!(matches!(saved.state, FiringState::Paused(_)));
            })
            .unwrap();
    });
    resumed.reset();
    for released in [false, true] {
        if released {
            super::super::super::set_execution_connection_id(2);
            storage.release_advisory_transaction_locks(200);
            storage.finish_transaction_identity(200, true);
            super::super::super::set_execution_connection_id(1);
        }
        crate::mem::guard::forbid_alloc(|| {
            storage.preserve_advisory_statement();
            storage.begin_advisory_statement().unwrap();
            let statement = sequence::enter_statement(
                &statement_arena,
                guc.seq_session(),
                true,
                Some(storage.advisory_journal()),
            );
            let _settings = super::super::super::guc::enter_eval_scope(&guc, &mut transaction);
            let mut context = TriggerExecContext {
                host: PlpgsqlExecHost::Atomic {
                    storage: &mut storage,
                    services: crate::sql::exec::TriggerServices::new(
                        guc.seq_session(),
                        &mut test_cursors,
                    ),
                },
                txn: &mut transaction,
                arena: &resumed,
                params: crate::sql::eval::NO_PARAMS,
                responder: &mut responder,
            };
            if !released {
                super::super::super::set_execution_connection_id(2);
                assert_eq!(
                    saved.poll(&mut context).err().unwrap().sqlstate,
                    sqlstate::SERIALIZATION_FAILURE
                );
                assert_eq!(resumed.used(), 0);
                super::super::super::set_execution_connection_id(1);
                let parent_command = context.txn.command_id();
                {
                    let mut nested = context
                        .txn
                        .enter_trigger_sql(context.host.storage())
                        .unwrap();
                    let mut nested_context = TriggerExecContext {
                        host: context.host.reborrow(),
                        txn: &mut nested,
                        arena: context.arena,
                        params: crate::sql::eval::NO_PARAMS,
                        responder: context.responder,
                    };
                    assert_eq!(
                        saved.poll(&mut nested_context).err().unwrap().sqlstate,
                        sqlstate::SERIALIZATION_FAILURE
                    );
                }
                assert_eq!(context.txn.command_id(), parent_command);
                assert_eq!(resumed.used(), 0);
            }
            assert_eq!(
                saved.poll(&mut context).unwrap(),
                if released {
                    FiringProgress::Complete { accepted: true }
                } else {
                    FiringProgress::Waiting {
                        control: sqlstate::INTERNAL_LOCK_WAIT,
                    }
                }
            );
            assert_eq!(resumed.used(), 0);
            assert_eq!(effects.get(), 1);
            assert!(matches!(
                sequence::expected_input().unwrap(),
                sequence::ExpectedInput::None
            ));
            assert_eq!(
                crate::sql::datetime::statement_micros(),
                812_345_678_961_234
            );
            assert_eq!(
                crate::sql::datetime::transaction_micros(),
                812_345_678_961_234
            );
            assert_eq!(crate::sql::query::statement_deadline(), 812_345_678_901_233);
            assert_eq!(guc.get_owned("application_name").unwrap().as_str(), "outer");
            assert_eq!(
                crate::sql::eval::funcs::system::current_identity().as_str(),
                "caller"
            );
            if released {
                assert_eq!(
                    saved.poll(&mut context).err().unwrap().sqlstate,
                    sqlstate::INTERNAL_ERROR
                );
                sequence::statement_effect(
                    sequence::StatementEffectKind::Cursor,
                    &[Datum::Text("after firing")],
                    sequence::StatementEffectRetry::Continue,
                    &output,
                    || {
                        effects.set(effects.get() + 1);
                        sequence::StatementEffectAttempt {
                            result: Ok(&[Datum::Int8(2)]),
                            rewound_state: false,
                        }
                    },
                )
                .unwrap();
                statement.finish(guc.seq_session()).unwrap();
                assert_eq!(effects.get(), 2);
            }
        });
        resumed.reset();
    }
    crate::mem::guard::forbid_alloc(|| {
        let completed = saved.new_values(&output).unwrap().unwrap();
        assert_eq!(completed[0], Datum::Int4(12));
        drop(saved);
        let mut overwritten = rows.lease::<u8>(64 << 10).unwrap();
        overwritten.extend_from_slice(&[0xff; 8192]).unwrap();
        let all_controls = controls.lease::<u8>(64 << 10).unwrap();
        let Datum::Text(note) = completed[1] else {
            panic!("restored NEW.note")
        };
        assert_eq!(note.len(), 8212);
        assert!(note[..8192].bytes().all(|byte| byte == b'x'));
        assert_eq!(&note[8192..], "!oncechangedpostgres");
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
        drop(all_controls);
    });
    super::super::super::publish_execution_identity(prior);
}
