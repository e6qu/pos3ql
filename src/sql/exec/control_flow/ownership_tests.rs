use super::*;
use crate::mem::budget::Budget;

#[test]
fn terminal_errors_keep_original_details_without_an_unneeded_control_image() {
    let mut budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        8,
    )
    .unwrap();
    let parsing = Arena::new(&mut budget, "terminal error syntax", 8192).unwrap();
    let workspace = Arena::new(&mut budget, "terminal error workspace", 1).unwrap();
    let detail = "z".repeat(8192);
    crate::mem::guard::forbid_alloc(|| {
        let (program, code) = parse_owned_program(
            "BEGIN NULL; END",
            &parsing,
            &pool,
            PlpgsqlProgramKind::Anonymous,
        )
        .unwrap();
        let mut retained = ControlFrames::new(&workspace, code.layout(), &pool).unwrap();
        let mut frames = retained.execution(&workspace, &code).unwrap();
        frames.start(program.body);
        let free = pool.lease::<u8>(8192).err().unwrap().free_bytes;
        let _ = crate::sql::eval::take_diagnostic();
        crate::sql::eval::stash_diagnostic(Some(StackStr::from_str(&detail)), None);
        let error = frames
            .begin_error(sql_err!(sqlstate::RAISE_EXCEPTION, "original error"), &pool)
            .unwrap_err();
        assert_eq!(error.sqlstate, "P0001");
        assert_eq!(error.message.as_str(), "original error");
        let diagnostic = crate::sql::eval::take_diagnostic().unwrap();
        assert_eq!(diagnostic.detail.unwrap().as_str(), detail);
        assert!(diagnostic.hint.is_none());
        assert_eq!(pool.lease::<u8>(8192).err().unwrap().free_bytes, free);
    });
}

#[test]
fn execution_waits_require_private_provenance_and_exclude_consumer_completion() {
    use crate::sql::eval::InternalControl;
    crate::mem::guard::forbid_alloc(|| {
        for (control, waiting) in [
            (InternalControl::LockWait, true),
            (InternalControl::IoWait, true),
            (InternalControl::RoutineInvocation, true),
            (InternalControl::QueryConsumerComplete, false),
        ] {
            assert_eq!(is_execution_wait(&sql_err!(control, "control")), waiting);
            assert!(!is_execution_wait(&sql_err!(
                control.diagnostic_code(),
                "user error"
            )));
        }
        assert!(!is_execution_wait(&sql_err!(
            sqlstate::QUERY_CANCELED,
            "user cancellation"
        )));
    });
}

#[test]
fn suspended_error_cleanup_keeps_its_original_diagnostic_after_worker_reuse() {
    use super::super::execution_frame::ExecutionFrame;
    let mut budget = Budget::new(128 << 10);
    let controls = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        16,
    )
    .unwrap();
    let rows = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        2048,
        8,
    )
    .unwrap();
    let parsing = Arena::new(&mut budget, "pending error syntax", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "pending error original", 8192).unwrap();
    let restored_syntax = Arena::new(&mut budget, "pending error reparse", 16 << 10).unwrap();
    let output = Arena::new(&mut budget, "pending error restored", 64).unwrap();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = parse_owned_program(
            "BEGIN NULL; END",
            &parsing,
            &controls,
            PlpgsqlProgramKind::Anonymous,
        )
        .unwrap();
        let mut execution =
            ExecutionFrame::new(&workspace, &code, &[], 0, &controls, &rows).unwrap();
        execution.initialization = execution_frame::Initialization::Complete;
        let frames = execution.status.control_frames.as_mut().unwrap();
        frames
            .execution(&workspace, &code)
            .unwrap()
            .start(program.body);
        let _ = crate::sql::eval::take_diagnostic();
        crate::sql::eval::stash_diagnostic(
            Some(StackStr::from_str("original detail")),
            Some(StackStr::from_str("original hint")),
        );
        frames.progress = ExecutionProgress::Error(PendingError::capture(
            sql_err!(
                crate::sql::eval::InternalControl::IoWait.diagnostic_code(),
                "original error"
            ),
            &controls,
        ));
        execution.suspend().ok().unwrap()
    });
    drop((parsing, workspace));
    crate::mem::guard::forbid_alloc(|| {
        crate::sql::eval::stash_diagnostic(Some(StackStr::from_str("another connection")), None);
        let (_, code) = saved.source().parse(&restored_syntax, None).unwrap();
        let mut execution = saved.restore(&output, &code).ok().unwrap();
        let frames = execution.status.control_frames.as_mut().unwrap();
        assert_eq!(frames.active, 1);
        assert_eq!(frames.slots[0].as_ref().unwrap().next, 0);
        let ExecutionProgress::Error(error) =
            core::mem::replace(&mut frames.progress, ExecutionProgress::Complete)
        else {
            panic!("unfinished error cleanup")
        };
        let error = error.publish();
        assert_eq!(error.sqlstate, "PZ002");
        assert!(error.sqlstate.internal_control().is_none());
        assert_eq!(error.message.as_str(), "original error");
        let diagnostic = crate::sql::eval::take_diagnostic().unwrap();
        assert_eq!(diagnostic.detail.unwrap().as_str(), "original detail");
        assert_eq!(diagnostic.hint.unwrap().as_str(), "original hint");
        drop((execution, code));
        assert_eq!(controls.lease::<u8>(8192).unwrap().capacity(), 8192);
        assert_eq!(rows.lease::<u8>(2048).unwrap().capacity(), 2048);
    });
}

#[test]
fn unfinished_returns_preserve_values_after_failed_capture_and_workspace_destruction() {
    use super::super::execution_frame::ExecutionFrame;
    let mut budget = Budget::new(128 << 10);
    let controls = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        16,
    )
    .unwrap();
    let rows = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        2048,
        8,
    )
    .unwrap();
    let parsing = Arena::new(&mut budget, "pending return syntax", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "pending return original", 8192).unwrap();
    let tiny = Arena::new(&mut budget, "pending return failed restore", 64).unwrap();
    let output = Arena::new(&mut budget, "pending return output", 8192).unwrap();
    let restored_syntax = Arena::new(&mut budget, "pending return reparse", 16 << 10).unwrap();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = parse_owned_program(
            "BEGIN RETURN 'original'; END",
            &parsing,
            &controls,
            PlpgsqlProgramKind::Function,
        )
        .unwrap();
        let mut execution =
            ExecutionFrame::new(&workspace, &code, &[], 0, &controls, &rows).unwrap();
        execution.initialization = execution_frame::Initialization::Complete;
        let frames = execution.status.control_frames.as_mut().unwrap();
        frames
            .execution(&workspace, &code)
            .unwrap()
            .start(program.body);
        let text = workspace.alloc_slice_with(1024, |_| b'r').unwrap();
        frames.progress = ExecutionProgress::Flow(TriggerFlow::Return(TriggerReturnValue::Value(
            Datum::Text(core::str::from_utf8(text).unwrap()),
        )));
        let held = rows.lease::<u8>(1024).unwrap();
        let (execution, error) = execution.suspend().err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        let frames = execution.status.control_frames.as_ref().unwrap();
        assert_eq!(frames.active, 1);
        assert!(
            matches!(frames.progress.return_value(), Some(Datum::Text(value)) if value.len() == 1024)
        );
        assert!(frames.saved_return.is_none());
        drop(held);
        execution.suspend().ok().unwrap()
    });
    drop((parsing, workspace));
    crate::mem::guard::forbid_alloc(|| {
        let (_, code) = saved.source().parse(&restored_syntax, None).unwrap();
        let (saved, error) = saved.restore(&tiny, &code).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(tiny.used(), 0);
        let execution = saved.restore(&output, &code).ok().unwrap();
        let frames = execution.status.control_frames.as_ref().unwrap();
        assert_eq!(frames.active, 1);
        assert!(frames.saved_return.is_none());
        let Some(Datum::Text(value)) = frames.progress.return_value() else {
            panic!("pending return value")
        };
        assert_eq!(value.len(), 1024);
        assert!(value.bytes().all(|byte| byte == b'r'));
        let saved = execution.suspend().ok().unwrap();
        drop((saved, code));
        assert_eq!(rows.lease::<u8>(2048).unwrap().capacity(), 2048);
        assert_eq!(controls.lease::<u8>(8192).unwrap().capacity(), 8192);
    });
}

#[test]
fn suspended_query_attachment_keeps_receipts_after_rejecting_a_foreign_arena() {
    use super::super::execution_frame::ExecutionFrame;
    let mut budget = Budget::new(128 << 10);
    let controls = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        16,
    )
    .unwrap();
    let rows = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        2048,
        8,
    )
    .unwrap();
    let parsing = Arena::new(&mut budget, "query attachment syntax", 16 << 10).unwrap();
    let mut owner = Arena::new(&mut budget, "query receipt owner", 16 << 10).unwrap();
    let foreign = Arena::new(&mut budget, "foreign query attachment", 16 << 10).unwrap();
    let reparsing = Arena::new(&mut budget, "query attachment reparse", 16 << 10).unwrap();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = parse_owned_program(
            "BEGIN NULL; END",
            &parsing,
            &controls,
            PlpgsqlProgramKind::Anonymous,
        )
        .unwrap();
        let mut execution = ExecutionFrame::new(&owner, &code, &[], 0, &controls, &rows).unwrap();
        execution.initialization = execution_frame::Initialization::Complete;
        execution
            .status
            .control_frames
            .as_mut()
            .unwrap()
            .execution(&owner, &code)
            .unwrap()
            .start(program.body);
        let calls = crate::sql::query::RoutineInvocationState::new(
            &owner,
            crate::sql::query::RoutineExecution::StatementReplay,
        );
        calls
            .complete(
                crate::sql::query::OwnedRoutineResult::capture_scalar(Datum::Int4(73), &owner)
                    .unwrap(),
                &owner,
                &owner,
            )
            .unwrap();
        execution.status.pending_query = Some(calls);
        execution.suspend().ok().unwrap()
    });
    drop(parsing);
    crate::mem::guard::forbid_alloc(|| {
        owner.recycle_front();
        let (_, code) = saved.source().parse(&reparsing, None).unwrap();
        let (saved, error) = saved.restore(&foreign, &code).err().unwrap();
        assert_eq!(error.sqlstate, "XX000");
        assert_eq!(foreign.used(), 0);
        let mut execution = saved.restore(&owner, &code).ok().unwrap();
        let calls = execution.status.pending_query.take().unwrap();
        calls.begin_attempt();
        assert_eq!(
            calls.resolve_intrinsic(42, &[], &owner, &foreign).unwrap(),
            Datum::Int4(73)
        );
        drop((execution, code));
        assert_eq!(controls.lease::<u8>(8192).unwrap().capacity(), 8192);
        assert_eq!(rows.lease::<u8>(2048).unwrap().capacity(), 2048);
    });
}

fn parse_owned_program<'a>(
    text: &str,
    parsing: &'a Arena,
    pool: &std::sync::Arc<LeasePool>,
    kind: PlpgsqlProgramKind,
) -> Result<(ParsedTriggerProgram<'a>, CompiledProgram<'a>), SqlError> {
    ProgramSource::capture(text, kind, pool)?.parse(parsing, None)
}

#[test]
fn output_binding_admission_returns_control_leases_after_failure_and_owner_drop() {
    let mut budget = Budget::new(64 << 10);
    let parsing = Arena::new(&mut budget, "output admission syntax", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "output admission workspace", 16 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        4,
    )
    .unwrap();
    let (_, code) = parse_owned_program(
        "BEGIN NULL; END",
        &parsing,
        &pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let locals = [TriggerLocalDecl {
        label: None,
        name: SqlName::parse("value").unwrap(),
        ctype: ColType::Text,
        ..empty_plpgsql_local()
    }];
    let free = pool.lease::<u8>(4096).err().unwrap().free_bytes;
    crate::mem::guard::forbid_alloc(|| {
        let held = pool.lease::<u8>(64).unwrap();
        let error = TriggerExecutionStatus::new(&workspace, &code, &locals, 1, &pool, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_leases"));
        drop(held);
        assert_eq!(pool.lease::<u8>(free).unwrap().capacity(), free);

        let mut status =
            TriggerExecutionStatus::new(&workspace, &code, &locals, 1, &pool, &pool).unwrap();
        status.set_output_locals(&[0], locals.len());
        assert_eq!(status.outputs[0].local, 0);
        assert_eq!(pool.lease::<u8>(1).err().unwrap().available_leases, 0);
        drop(status);
        assert_eq!(pool.lease::<u8>(free).unwrap().capacity(), free);
        drop(code);
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}

#[test]
fn shared_control_capacity_preserves_live_positions_and_returns_after_owner_drop() {
    let mut budget = Budget::new(64 << 10);
    let parsing = Arena::new(&mut budget, "shared control syntax", 16 << 10).unwrap();
    let arena = Arena::new(&mut budget, "shared control workspace", 16 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        8,
    )
    .unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parsing,
        &pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let source_free = pool.lease::<u8>(4096).err().unwrap().free_bytes;
    crate::mem::guard::forbid_alloc(|| {
        let mut retained = ControlFrames::new(&arena, code.layout(), &pool).unwrap();

        {
            let mut frames = retained.execution(&arena, &code).unwrap();
            frames.start(program.body);
            frames.last_mut().next = 1;
        }
        let free = pool.lease::<u8>(4096).err().unwrap().free_bytes;
        let held = pool.lease::<u8>(free).unwrap();
        let error = ControlFrames::new(&arena, code.layout(), &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert!(error.message.as_str().contains("execution_control_leases"));
        assert_eq!(retained.active, 1);
        assert_eq!(retained.slots[0].as_ref().unwrap().next, 1);
        drop(held);
        let second = ControlFrames::new(&arena, code.layout(), &pool).unwrap();
        assert_eq!(second.active, 0);
        assert_eq!(second.slots.len(), 2);
        drop(second);
        let mut frames = retained.execution(&arena, &code).unwrap();
        assert_eq!(frames.last().next, 1);
        frames.pop();
        drop(retained);
        assert_eq!(
            pool.lease::<u8>(source_free).unwrap().capacity(),
            source_free
        );
        drop(code);
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}

#[test]
fn lexical_frame_capacity_covers_every_branch_and_fails_before_execution() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let mut budget = Budget::new(1 << 18);
    let mut parse = Arena::new(&mut budget, "procedural positions parsing", 1 << 16).unwrap();
    let mut arena = Arena::new(&mut budget, "procedural positions", 1 << 16).unwrap();
    for (source, depth) in [
        ("BEGIN NULL; END", 1),
        ("BEGIN LOOP IF true THEN NULL; END IF; END LOOP; END", 3),
        (
            "BEGIN CASE WHEN true THEN NULL; ELSE WHILE false LOOP NULL; END LOOP; END CASE; END",
            3,
        ),
        (
            "BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END; END",
            4,
        ),
        (
            "DECLARE x integer; BEGIN FOREACH x IN ARRAY ARRAY[1,2] LOOP NULL; END LOOP; FOR x IN SELECT 1 LOOP NULL; END LOOP; FOR x IN EXECUTE 'SELECT 2' LOOP NULL; END LOOP; END",
            2,
        ),
    ] {
        parse.reset();
        arena.reset();
        let (_, code) =
            parse_owned_program(source, &parse, &pool, PlpgsqlProgramKind::Anonymous).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let frames = ControlFrames::new(&arena, code.layout(), &pool).unwrap();
            assert_eq!(frames.slots.len(), depth, "{source}");
            assert_eq!(frames.active, 0);
            assert!(frames.slots.iter().all(Option::is_none));
        });
    }
    let small = Arena::new(&mut budget, "procedural empty payload workspace", 1).unwrap();
    let (_, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parse,
        &pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let frames = ControlFrames::new(&small, code.layout(), &pool).unwrap();
        assert_eq!(frames.resources.len(), 2);
        assert_eq!(small.used(), 0);
    });
}

#[test]
fn numeric_positions_restore_parent_values_and_release_all_scope_references() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "procedural numeric positions parsing", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "procedural numeric positions", 1 << 16).unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN FOR x IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
        &parse,
        &pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let locals = [
        TriggerLocalDecl {
            label: None,
            name: SqlName::parse("x").unwrap(),
            ctype: ColType::Text,
            ..empty_plpgsql_local()
        },
        TriggerLocalDecl {
            label: None,
            name: SqlName::parse("n").unwrap(),
            ctype: ColType::Int4,
            ..empty_plpgsql_local()
        },
    ];
    let TriggerStatement::For(outer) = program.body.statements[0] else {
        panic!("outer loop")
    };
    let TriggerStatement::For(inner) = outer.block.statements[0] else {
        panic!("inner loop")
    };
    crate::mem::guard::forbid_alloc(|| {
        let mut status =
            TriggerExecutionStatus::new(&arena, &code, &locals, 0, &pool, &pool).unwrap();
        drop(status.control_frames.take().unwrap());
        let mut values = [Datum::Text("outside"), Datum::Int4(0)];
        for success in [false, true] {
            // Calls share the reusable numeric workspace, while each call has
            // its own control stack and completion position.
            let mut retained = ControlFrames::new(&arena, code.layout(), &pool).unwrap();
            let mut frames = retained.execution(&arena, &code).unwrap();
            frames.start(program.body);
            let Step::Enter(frame) = start_numeric_loop(
                outer,
                1,
                2,
                1,
                false,
                PlpgsqlScope::frame(2),
                &locals,
                &values,
                &mut status,
            )
            .unwrap() else {
                panic!("numeric frame")
            };
            frames.enter(frame);
            // The counter shadows `x` in a new slot after the variables.
            let (scope, current, _) = frames.scope(&locals, &mut values);
            assert_eq!(current[0], Datum::Text("outside"));
            assert_eq!(current[2], Datum::Int4(1));
            assert_eq!(scope[1].name.as_str(), "n");
            assert_eq!(scope[2].name.as_str(), "x");
            assert_eq!(scope[2].scope, PlpgsqlScope::frame(2));
            let Step::Enter(frame) = start_numeric_loop(
                inner,
                1,
                2,
                1,
                false,
                PlpgsqlScope::frame(3),
                scope,
                current,
                &mut status,
            )
            .unwrap() else {
                panic!("nested frame")
            };
            frames.enter(frame);
            let (scope, current, _) = frames.scope(&locals, &mut values);
            assert_eq!(scope.len(), 4);
            assert_eq!(scope[3].name.as_str(), "j");
            assert_eq!(scope[3].scope, PlpgsqlScope::frame(3));
            current[1] = Datum::Int4(41);
            status.set_found(false);
            let inner = frames.pop();
            frames.finish(inner, &locals, &mut values, &mut status, success);
            let (_, current, _) = frames.scope(&locals, &mut values);
            assert_eq!(current[2], Datum::Int4(1));
            assert_eq!(current[1], Datum::Int4(41));
            let outer = frames.pop();
            frames.finish(outer, &locals, &mut values, &mut status, success);
            assert_eq!(values, [Datum::Text("outside"), Datum::Int4(41)]);
            assert_eq!(status.found, Some(success));
            assert!(matches!(frames.pop().continuation, Continuation::Once));
            assert!(frames.state.slots.iter().all(Option::is_none));
            let workspace = status.numeric_frames.take().unwrap();
            assert!(
                workspace
                    .locals
                    .iter()
                    .all(|local| local.name == SqlName::EMPTY)
            );
            assert!(workspace.values.iter().all(Datum::is_null));
            status.numeric_frames.restore(workspace);
        }
    });
}

#[test]
fn active_numeric_positions_and_values_rebind_after_syntax_storage_is_replaced() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    const SOURCE: &str = "BEGIN FOR x IN 1..3 LOOP WHILE true LOOP NULL; END LOOP; END LOOP; END";
    let mut budget = Budget::new(256 << 10);
    let original = Arena::new(&mut budget, "active procedural source", 64 << 10).unwrap();
    let mut parsing = Arena::new(&mut budget, "active procedural syntax", 64 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "active procedural values", 64 << 10).unwrap();
    let foreign = Arena::new(&mut budget, "foreign procedural positions", 64 << 10).unwrap();
    let locals = [
        TriggerLocalDecl {
            label: None,
            name: SqlName::parse("x").unwrap(),
            ctype: ColType::Text,
            ..empty_plpgsql_local()
        },
        TriggerLocalDecl {
            label: None,
            name: SqlName::parse("n").unwrap(),
            ctype: ColType::Text,
            ..empty_plpgsql_local()
        },
    ];
    let source = ProgramSource::capture(
        original.alloc_str(SOURCE).unwrap(),
        PlpgsqlProgramKind::Anonymous,
        &pool,
    )
    .unwrap();
    let (mut status, retained, mut values) = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = source.parse(&parsing, None).unwrap();
        let mut status =
            TriggerExecutionStatus::new(&workspace, &code, &locals, 0, &pool, &pool).unwrap();
        let mut retained = status.control_frames.take().unwrap();

        let mut values = [Datum::Text("outside counter"), Datum::Text("before")];
        {
            let mut frames = retained.execution(&workspace, &code).unwrap();
            frames.start(program.body);
            let TriggerStatement::For(program) = program.body.statements[0] else {
                panic!("numeric loop")
            };
            let Step::Enter(entry) = start_numeric_loop(
                program,
                1,
                3,
                1,
                false,
                PlpgsqlScope::frame(2),
                &locals,
                &values,
                &mut status,
            )
            .unwrap() else {
                panic!("numeric loop entry")
            };
            frames.enter(entry);
            let (_, current, _) = frames.scope(&locals, &mut values);
            current[1] = Datum::Text(workspace.alloc_str("changed once before waiting").unwrap());
            let TriggerStatement::While(program) = program.block.statements[0] else {
                panic!("while loop")
            };
            frames.enter(FrameEntry::new(program.block, EntryState::While));
            frames.last_mut().next = 1;
            assert_eq!(frames.state.active, 3);
            assert!(pool.lease::<u8>(16 << 10).is_err());
        }
        (status, retained, values)
    });
    // No syntax reference remains in either the control stack or loop values.
    parsing.reset();
    let mut moved_source = original;
    let mut moved = retained;
    crate::mem::guard::forbid_alloc(|| {
        moved_source.reset();
        moved_source.alloc_slice_with(1024, |_| 0xffu8).unwrap();
        drop(moved_source);
        parsing.alloc_slice_with(1024, |_| 0xffu8).unwrap();
        let (_, code) = source.parse(&parsing, None).unwrap();
        assert_eq!(
            moved.execution(&foreign, &code).err().unwrap().sqlstate,
            "40001"
        );
        let mut frames = moved.execution(&workspace, &code).unwrap();
        assert_eq!(frames.state.active, 3);
        assert_eq!(frames.last().next, 1);
        let TriggerStatement::While(program) = frames.source_statement(frames.last().source) else {
            panic!("rebuilt while loop")
        };
        assert!(matches!(program.condition, Expr::Bool(true)));
        let (_, current, _) = frames.scope(&locals, &mut values);
        assert_eq!(
            current,
            &[
                Datum::Text("outside counter"),
                Datum::Text("changed once before waiting"),
                Datum::Int4(1)
            ]
        );
        let child = frames.pop();
        frames.finish(child, &locals, &mut values, &mut status, true);
        assert!(matches!(
            frames.last().continuation,
            Continuation::Numeric(NumericLoop {
                value: 1,
                upper: 3,
                step: 1,
                reverse: false,
                ..
            })
        ));
        let numeric = frames.pop();
        frames.finish(numeric, &locals, &mut values, &mut status, true);
        assert_eq!(
            values,
            [
                Datum::Text("outside counter"),
                Datum::Text("changed once before waiting")
            ]
        );
        let root = frames.pop();
        frames.finish(root, &locals, &mut values, &mut status, true);
        assert!(frames.state.slots.iter().all(Option::is_none));
        assert!(
            frames
                .state
                .resources
                .iter()
                .all(|resource| matches!(resource, FrameResource::Empty))
        );
    });
    // Resetting the workspace rejects later bindings while the source lives.
    let mut owner_budget = Budget::new(8192);
    let mut owner = Arena::new(&mut owner_budget, "reset procedural metadata", 8192).unwrap();
    let (_, code) = source.parse(&parsing, None).unwrap();
    let mut reset = ControlFrames::new(&owner, code.layout(), &pool).unwrap();
    reset.execution(&owner, &code).unwrap();
    owner.reset();
    assert_eq!(
        reset.require_workspace(&owner).unwrap_err().sqlstate,
        "40001"
    );
    // Preserve the unique paired owners just as normal execution does.
    status.control_frames = Some(moved);
}

#[test]
fn resource_reservation_exhaustion_preserves_positions_and_allows_retry() {
    let mut budget = Budget::new(64 << 10);
    let parsing = Arena::new(&mut budget, "reservation syntax", 16 << 10).unwrap();
    let arena = Arena::new(&mut budget, "reservation positions", 4 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        6,
    )
    .unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parsing,
        &pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let source_free = pool.lease::<u8>(4096).err().unwrap().free_bytes;
    crate::mem::guard::forbid_alloc(|| {
        let mut retained = ControlFrames::new(&arena, code.layout(), &pool).unwrap();
        {
            let mut frames = retained.execution(&arena, &code).unwrap();
            frames.start(program.body);
            frames.last_mut().next = 1;
        }
        let first = pool.lease::<u8>(64).unwrap();
        let second = pool.lease::<u8>(64).unwrap();
        let error = ControlFrames::new(&arena, code.layout(), &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_leases"));
        assert_eq!(pool.lease::<u8>(4096).err().unwrap().available_leases, 1);
        assert_eq!(retained.active, 1);
        assert_eq!(retained.slots[0].as_ref().unwrap().next, 1);
        drop(second);
        let retry = ControlFrames::new(&arena, code.layout(), &pool).unwrap();
        assert_eq!(retry.active, 0);
        drop(retry);
        drop(first);
        retained.execution(&arena, &code).unwrap().pop();
        drop(retained);
        assert_eq!(
            pool.lease::<u8>(source_free).unwrap().capacity(),
            source_free
        );
        drop(code);
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}

#[test]
fn row_loop_resources_release_once_on_pop_and_owner_drop() {
    let mut control_budget = Budget::new(32 << 10);
    let control_pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    use crate::mem::lease_pool::Purpose;

    let mut budget = Budget::new(128 << 10);
    let parsing = Arena::new(&mut budget, "row position syntax", 32 << 10).unwrap();
    let mut workspace = Arena::new(&mut budget, "row position resources", 32 << 10).unwrap();
    let row_pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, 4096, 4).unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parsing,
        &control_pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let TriggerStatement::Loop(block) = program.body.statements[0] else {
        panic!("loop body")
    };
    for pop in [true, false] {
        workspace.reset();
        crate::mem::guard::forbid_alloc(|| {
            let mut retained =
                ControlFrames::new(&workspace, code.layout(), &control_pool).unwrap();

            {
                let mut frames = retained.execution(&workspace, &code).unwrap();
                frames.start(program.body);
                let rows = super::super::loop_rows::LoopRows::capture(
                    &row_pool,
                    TriggerForRows {
                        values: &[Datum::Text("owned row")],
                        empty: Datum::Null,
                    },
                )
                .unwrap();
                frames.enter(FrameEntry::new(
                    block,
                    EntryState::Rows {
                        target: PlpgsqlVariableName::plain(SqlName::parse("x").unwrap()),
                        index: 0,
                        values: rows,
                    },
                ));
                assert!(row_pool.lease::<u8>(4096).is_err());
                if pop {
                    let exited = frames.pop();
                    assert!(matches!(frames.state.resources[1], FrameResource::Empty));
                    assert!(row_pool.lease::<u8>(4096).is_err());
                    drop(exited);
                    assert_eq!(row_pool.lease::<u8>(4096).unwrap().capacity(), 4096);
                    frames.pop();
                }
            }
            drop(retained);
            assert_eq!(row_pool.lease::<u8>(4096).unwrap().capacity(), 4096);
        });
    }
}

#[test]
fn handler_diagnostics_and_statement_positions_survive_nested_frame_changes() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "procedural handler positions parsing", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "procedural handler positions", 1 << 16).unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN IF true THEN NULL; END IF; END; END",
        &parse,
        &pool,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let TriggerStatement::Exception(protected) = program.body.statements[0] else {
        panic!("protected block")
    };
    let handler = protected.handlers[0].block;
    let TriggerStatement::If(branches) = handler.statements[0] else {
        panic!("handler branch")
    };
    crate::mem::guard::forbid_alloc(|| {
        let mut retained = ControlFrames::new(&arena, code.layout(), &pool).unwrap();

        let mut frames = retained.execution(&arena, &code).unwrap();
        frames.start(program.body);
        frames.last_mut().next = 1;
        let error = sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero");
        frames
            .state
            .diagnostics
            .capture(&error, None, "handler")
            .unwrap();
        frames.push(
            FrameEntry::new(handler, EntryState::Handler),
            FrameSource::Handler,
        );
        frames.enter(FrameEntry::new(
            branches.branches[0].block,
            EntryState::Once,
        ));
        assert_eq!(
            frames
                .state
                .diagnostics
                .current()
                .unwrap()
                .variables(&arena)
                .unwrap()
                .message,
            "division by zero"
        );
        let child = frames.pop();
        assert!(matches!(
            frames.source_statement(child.source),
            TriggerStatement::If(_)
        ));
        assert_eq!(frames.last().next, 1);
        assert_eq!(
            frames.state.diagnostics.current().unwrap().sqlstate,
            "22012"
        );
        let handler = frames.pop();
        let Continuation::Handler = handler.continuation else {
            panic!("handler");
        };
        frames.state.diagnostics.release();
        assert!(frames.diagnostic().is_none());
        assert_eq!(frames.last().next, 1);
        frames.pop();
        assert!(frames.state.slots.iter().all(Option::is_none));
    });
}

#[test]
fn positions_resolve_the_same_source_after_original_parse_storage_is_released() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    const SOURCE: &str = "BEGIN WHILE true LOOP NULL; END LOOP; END";
    let mut budget = Budget::new(1 << 18);
    let mut parsing = Arena::new(&mut budget, "recycled procedural syntax", 1 << 16).unwrap();
    let workspace = Arena::new(&mut budget, "procedural positions", 1 << 16).unwrap();
    let replacement_parsing = Arena::new(&mut budget, "replacement syntax", 1 << 14).unwrap();
    let source = crate::mem::guard::forbid_alloc(|| {
        let text = parsing.alloc_str(SOURCE).unwrap();
        ProgramSource::capture(text, PlpgsqlProgramKind::Anonymous, &pool).unwrap()
    });
    parsing.reset();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = source.parse(&parsing, None).unwrap();
        let mut retained = ControlFrames::new(&workspace, code.layout(), &pool).unwrap();

        let mut frames = retained.execution(&workspace, &code).unwrap();
        let TriggerStatement::While(while_program) = program.body.statements[0] else {
            panic!("while program")
        };
        frames.start(program.body);
        frames.enter(FrameEntry::new(while_program.block, EntryState::While));
        frames.last_mut().next = 1;
        let frame = frames.pop();
        let position = (code.layout(), frame.block, frame.next, frame.source);
        frames.pop();
        position
    });
    parsing.reset();
    crate::mem::guard::forbid_alloc(|| {
        parsing.alloc_str("replacement parsing scratch").unwrap();
        let (_, code) = source.parse(&parsing, None).unwrap();
        let replaced = ProgramSource::capture(
            "BEGIN WHILE false LOOP NULL; END LOOP; END",
            PlpgsqlProgramKind::Anonymous,
            &pool,
        )
        .unwrap();
        let (_, new_code) = replaced.parse(&replacement_parsing, None).unwrap();
        let mut retained = ControlFrames::new(&workspace, saved.0, &pool).unwrap();

        assert_eq!(
            retained
                .execution(&workspace, &new_code)
                .err()
                .unwrap()
                .sqlstate,
            "40001"
        );
        assert_eq!(retained.active, 0);
        assert!(retained.slots.iter().all(Option::is_none));
        let frames = retained.execution(&workspace, &code).unwrap();
        assert_eq!(saved.2, frames.program.block(saved.1).statements.len());
        assert!(matches!(
            frames.program.block(saved.1).statements[0],
            TriggerStatement::Null
        ));
        let TriggerStatement::While(program) = frames.source_statement(saved.3) else {
            panic!("restored while position")
        };
        assert!(matches!(program.condition, Expr::Bool(true)));
    });
}

#[test]
fn source_owners_outlive_input_and_syntax_arenas_but_reject_replacement_images() {
    let mut budget = Budget::new(128 << 10);
    let mut input = Arena::new(&mut budget, "original source input", 8192).unwrap();
    let mut parsing = Arena::new(&mut budget, "reused source parsing", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "retained source positions", 8192).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        16,
    )
    .unwrap();
    const SOURCE: &str = "BEGIN LOOP NULL; END LOOP; END";
    let source = ProgramSource::capture(
        input.alloc_str(SOURCE).unwrap(),
        PlpgsqlProgramKind::Anonymous,
        &pool,
    )
    .unwrap();
    let mut retained = {
        let (program, code) = source.parse(&parsing, None).unwrap();
        let mut retained = ControlFrames::new(&workspace, code.layout(), &pool).unwrap();
        {
            let mut frames = retained.execution(&workspace, &code).unwrap();
            frames.start(program.body);
            frames.last_mut().next = 1;
        }
        retained
    };
    parsing.reset();
    input.reset();
    crate::mem::guard::forbid_alloc(|| {
        input.alloc_slice_with(1024, |_| 0xffu8).unwrap();
        drop(input);
        let other = ProgramSource::capture(SOURCE, PlpgsqlProgramKind::Anonymous, &pool).unwrap();
        let other_kind =
            ProgramSource::capture(SOURCE, PlpgsqlProgramKind::Trigger, &pool).unwrap();
        for replacement in [&other, &other_kind] {
            assert_eq!(
                retained.require_source(replacement).unwrap_err().sqlstate,
                "40001"
            );
            assert_eq!(retained.active, 1);
            assert_eq!(retained.slots[0].as_ref().unwrap().next, 1);
        }
        drop((other, other_kind));
        let (_, code) = retained.source.parse(&parsing, None).unwrap();
        drop(source);
        drop(pool);
        let mut frames = retained.execution(&workspace, &code).unwrap();
        assert_eq!(frames.last().next, 1);
        assert!(matches!(
            frames.program.root().statements[0],
            TriggerStatement::Loop(_)
        ));
        frames.pop();
    });
}

#[test]
fn source_capture_and_parse_failures_preserve_live_frames_and_return_capacity() {
    let mut budget = Budget::new(64 << 10);
    let parsing = Arena::new(&mut budget, "source failure syntax", 16 << 10).unwrap();
    let tiny = Arena::new(&mut budget, "source parsing exhaustion", 1).unwrap();
    let workspace = Arena::new(&mut budget, "source failure positions", 8192).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        4,
    )
    .unwrap();
    let small = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        64,
        2,
    )
    .unwrap();
    let large = [b'x'; 128];
    crate::mem::guard::forbid_alloc(|| {
        let error = ProgramSource::capture(
            core::str::from_utf8(&large).unwrap(),
            PlpgsqlProgramKind::Anonymous,
            &small,
        )
        .err()
        .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert_eq!(small.lease::<u8>(64).unwrap().capacity(), 64);
        let source = ProgramSource::capture(
            "BEGIN LOOP NULL; END LOOP; END",
            PlpgsqlProgramKind::Anonymous,
            &pool,
        )
        .unwrap();
        let (program, code) = source.parse(&parsing, None).unwrap();
        let mut retained = ControlFrames::new(&workspace, code.layout(), &pool).unwrap();
        {
            let mut frames = retained.execution(&workspace, &code).unwrap();
            frames.start(program.body);
            frames.last_mut().next = 1;
        }
        let free = pool.lease::<u8>(4096).err().unwrap().free_bytes;
        let held = pool.lease::<u8>(64).unwrap();
        let error = ProgramSource::capture("BEGIN NULL; END", PlpgsqlProgramKind::Anonymous, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("0 available leases"));
        drop(held);
        assert_eq!(pool.lease::<u8>(4096).err().unwrap().free_bytes, free);
        let error = source.parse(&tiny, None).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("statement memory"));
        assert_eq!(retained.active, 1);
        assert_eq!(retained.slots[0].as_ref().unwrap().next, 1);
        for text in ["BEGIN", "", "DECLARE", "DECLARE value integer"] {
            let invalid =
                ProgramSource::capture(text, PlpgsqlProgramKind::Anonymous, &pool).unwrap();
            assert_eq!(
                invalid.parse(&parsing, None).err().unwrap().sqlstate,
                "42601"
            );
            drop(invalid);
            assert_eq!(pool.lease::<u8>(4096).err().unwrap().free_bytes, free);
        }
        retained.require_source(&source).unwrap();
        retained.execution(&workspace, &code).unwrap().pop();
        drop((retained, code, source));
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}
#[test]
fn lowered_assignment_inputs_bind_handler_variables_from_the_original_source() {
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "handler assignment parsing", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "handler assignment values", 1 << 16).unwrap();
    let program = parse_plpgsql_program(
        "DECLARE document jsonb := '{}'; BEGIN BEGIN RAISE EXCEPTION 'caught'; EXCEPTION WHEN OTHERS THEN document[SQLERRM] := to_jsonb(SQLSTATE); END; END",
        &parse, PlpgsqlProgramKind::Anonymous,
    ).unwrap();
    let TriggerStatement::Exception(block) = program.body.statements[0] else {
        panic!("exception");
    };
    let statement = block.handlers[0].block.statements[0];
    let TriggerStatement::LocalAssign(assignment) = statement else {
        panic!("assignment");
    };
    assert!(assignment.exception_variables);
    assert!(matches!(
        assignment.expression,
        Expr::Call {
            name: "__jsonb_subscript_set",
            ..
        }
    ));
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        4,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&pool, 1).unwrap();
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "caught"),
                None,
                "handler",
            )
            .unwrap();
        let invocation = crate::sql::type_input_plans::with_owner(
            crate::sql::type_input_plans::Owner::Transient {
                generation: 1,
                statement: 0,
            },
            || {
                TriggerInvocation::anonymous(crate::sql::guc::PlpgsqlSettings {
                    variable_conflict: crate::sql::guc::VariableConflict::Error,
                    print_strict_params: false,
                })
            },
        )
        .unwrap()
        .with_input_sites(program.input_sites);
        let bound = bind_exception_variables(
            invocation,
            Some((buffers.current().unwrap(), PlpgsqlScope::frame(2))),
            statement,
            &arena,
        )
        .unwrap();
        let variables = bound
            .exception
            .expect("lowering preserves handler name dependencies")
            .values;
        assert_eq!(variables.message, "caught");
        assert_eq!(variables.sqlstate_text, "P0001");
        buffers.release();
        assert_eq!(variables.message, "caught");
    });
}

#[test]
fn procedural_identifier_sources_keep_quoted_names_without_retaining_lexer_scratch() {
    let mut budget = Budget::new(1 << 16);
    let arena = Arena::new(&mut budget, "procedural name tokens", 1 << 16).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for (source, expected, quoted, spelling) in [
            ("MixedCase", "mixedcase", false, "MixedCase"),
            ("\"MixedCase\"", "MixedCase", true, "\"MixedCase\""),
            ("\"spaced name\"", "spaced name", true, "\"spaced name\""),
            (
                "\"doubled\"\"quote\"",
                "doubled\"quote",
                true,
                "\"doubled\"\"quote\"",
            ),
            ("/* before */ MiXeD -- after", "mixed", false, "MiXeD"),
            (
                "/* before */ \"SQLSTATE\" -- after",
                "SQLSTATE",
                true,
                "\"SQLSTATE\"",
            ),
        ] {
            let before = arena.used();
            let parsed = parse_plpgsql_identifier(source, &arena).unwrap();
            assert_eq!(parsed.name.as_str(), expected);
            assert_eq!(parsed.quoted, quoted);
            assert_eq!(parsed.spelling, spelling);
            assert_eq!(arena.used(), before);
        }
        for source in ["a b", "name.field", "1", "\"unterminated"] {
            let before = arena.used();
            assert_eq!(
                parse_plpgsql_identifier(source, &arena)
                    .unwrap_err()
                    .sqlstate,
                "42601"
            );
            assert_eq!(arena.used(), before);
        }
    });
}

#[test]
fn quoted_cursor_names_use_tokens_in_open_fetch_move_and_close() {
    let mut budget = Budget::new(1 << 16);
    let arena = Arena::new(&mut budget, "quoted cursor tokens", 1 << 16).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for _ in 0..32 {
            let before = arena.used();
            arena
                .with_remaining_region(|scratch| {
                    let open = parse_plpgsql_cursor_open(
                        "OPEN \"cursor \"\" name\" FOR SELECT 1",
                        scratch,
                    )
                    .unwrap()
                    .unwrap();
                    assert_eq!(open.variable.name.as_str(), "cursor \" name");
                    for source in [
                        "FETCH NEXT FROM \"cursor \"\" name\" INTO \"output value\"",
                        "MOVE NEXT FROM \"cursor \"\" name\"",
                    ] {
                        let motion = parse_plpgsql_cursor_motion(source, scratch)
                            .unwrap()
                            .unwrap();
                        assert_eq!(motion.variable, open.variable);
                        if !motion.move_only {
                            assert_eq!(motion.targets[0].name.as_str(), "output value");
                        }
                    }
                    assert_eq!(
                        parse_plpgsql_cursor_close("CLOSE \"cursor \"\" name\"", scratch)
                            .unwrap()
                            .map(|close| close.variable),
                        Some(open.variable),
                    );
                })
                .unwrap();
            assert_eq!(arena.used(), before);
        }
    });
}

#[test]
fn lexical_variable_values_types_and_domains_select_one_binding() {
    let owner = super::super::super::type_input_plans::Owner::Transient {
        generation: 1,
        statement: 0,
    };
    let mut invocation = super::super::super::type_input_plans::with_owner(owner, || {
        TriggerInvocation::anonymous(crate::sql::guc::PlpgsqlSettings {
            variable_conflict: crate::sql::guc::VariableConflict::Error,
            print_strict_params: false,
        })
    })
    .unwrap();
    invocation.program_kind = PlpgsqlProgramKind::Trigger;
    invocation.name = "actual trigger";
    let mut locals = ["tg_name", "found", "sqlstate", "sqlerrm"].map(|name| TriggerLocalDecl {
        label: None,
        name: SqlName::parse(name).unwrap(),
        ctype: ColType::Int4,
        ..empty_plpgsql_local()
    });
    locals[2].user_type = Some(crate::storage::UserTypeName::declaration(
        SqlName::parse("public").unwrap(),
        SqlName::parse("state_domain").unwrap(),
    ));
    let values = [
        Datum::Int4(1),
        Datum::Int4(42),
        Datum::Int4(7),
        Datum::Int4(8),
    ];
    crate::mem::guard::forbid_alloc(|| {
        for (index, local) in locals.iter().enumerate() {
            let selected = plpgsql_variable(
                &locals,
                &values,
                invocation,
                Some(false),
                local.name.as_str(),
            )
            .unwrap();
            assert_eq!(selected.value, values[index]);
            assert_eq!(selected.ctype, ColType::Int4);
            assert_eq!(selected.user_type.is_some(), index == 2);
            if let Some(domain) = selected.user_type {
                assert_eq!(domain.schema.as_str(), "public");
                assert_eq!(domain.name.as_str(), "state_domain");
            }
        }
        invocation = invocation.with_exception(
            TriggerExceptionVariables {
                sqlstate_text: "P0001",
                message: "caught",
            },
            PlpgsqlScope::frame(2),
        );
        for (name, expected) in [("sqlstate", "P0001"), ("sqlerrm", "caught")] {
            let selected =
                plpgsql_variable(&locals, &values, invocation, Some(false), name).unwrap();
            assert_eq!(selected.value, Datum::Text(expected));
            assert_eq!(selected.ctype, ColType::Text);
            assert!(selected.user_type.is_none());
        }
        // A counter inside the handler shadows its implicit variables; a
        // handler inside that counter restores the implicit text binding.
        locals[2].scope = PlpgsqlScope::frame(3);
        locals[2].user_type = None;
        let counter =
            plpgsql_variable(&locals, &values, invocation, Some(false), "sqlstate").unwrap();
        assert_eq!(counter.value, Datum::Int4(7));
        assert_eq!(counter.ctype, ColType::Int4);
        invocation.exception.as_mut().unwrap().scope = PlpgsqlScope::frame(4);
        let handler =
            plpgsql_variable(&locals, &values, invocation, Some(false), "sqlstate").unwrap();
        assert_eq!(handler.value, Datum::Text("P0001"));
        assert_eq!(handler.ctype, ColType::Text);
        assert!(plpgsql_variable(&locals, &values, invocation, Some(false), "TG_NAME").is_none());
        let selected = plpgsql_variable(&[], &[], invocation, Some(false), "tg_name").unwrap();
        assert_eq!(selected.value, Datum::Text("actual trigger"));
        assert_eq!(selected.ctype, ColType::Name);

        let same_name = SqlName::parse("value").unwrap();
        let declarations = [
            TriggerLocalDecl {
                label: None,
                name: same_name,
                ctype: ColType::Int4,
                ..empty_plpgsql_local()
            },
            TriggerLocalDecl {
                label: None,
                name: same_name,
                ctype: ColType::Text,
                ..empty_plpgsql_local()
            },
        ];
        let selected = plpgsql_variable(
            &declarations,
            &[Datum::Int4(7), Datum::Text("body local")],
            invocation,
            Some(false),
            "value",
        )
        .unwrap();
        assert_eq!(selected.value, Datum::Text("body local"));
        assert_eq!(selected.ctype, ColType::Text);
        assert_eq!(
            trigger_local_index(&declarations, PlpgsqlVariableName::plain(same_name)).unwrap(),
            1
        );
    });
}

#[test]
fn constant_validation_uses_the_innermost_handler_or_counter_binding() {
    let mut budget = Budget::new(1 << 18);
    let arena = Arena::new(&mut budget, "lexical constant checks", 1 << 17).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        for (source, expected) in [
            (
                "DECLARE sqlstate integer; BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN sqlstate := 1; END; END",
                Some("22005"),
            ),
            (
                "DECLARE c refcursor; BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN FETCH c INTO sqlstate; END; END",
                Some("22005"),
            ),
            (
                "DECLARE x constant integer := 1; c refcursor; BEGIN FETCH c INTO x; END",
                Some("22005"),
            ),
            (
                "DECLARE sqlstate integer; BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN FOR sqlstate IN 1..2 LOOP sqlstate := 1; END LOOP; END; END",
                None,
            ),
            (
                "DECLARE sqlstate integer; BEGIN FOR sqlstate IN 1..2 LOOP BEGIN NULL; EXCEPTION WHEN OTHERS THEN sqlstate := 1; END; END LOOP; END",
                Some("22005"),
            ),
            (
                "BEGIN FOR SQLSTATE IN 1..2 LOOP NULL; END LOOP; END",
                Some("42601"),
            ),
            (
                "BEGIN FOR \"sqlstate\" IN 1..2 LOOP NULL; END LOOP; END",
                None,
            ),
            (
                "DECLARE c constant refcursor := 'c'; BEGIN OPEN c FOR SELECT 1; CLOSE c; END",
                None,
            ),
        ] {
            arena
                .with_remaining_region(|scratch| {
                    let parsed =
                        parse_plpgsql_program(source, scratch, PlpgsqlProgramKind::Anonymous)
                            .unwrap();
                    // This pass uses names and mutability, and whether a
                    // cursor variable is a refcursor; `c` is the only one.
                    let locals = scratch
                        .alloc_slice_with(parsed.locals.len(), |index| TriggerLocalDecl {
                            label: None,
                            name: parsed.locals[index].name,
                            constant: parsed.locals[index].constant,
                            ctype: if parsed.locals[index].name.as_str() == "c" {
                                ColType::Refcursor
                            } else {
                                empty_plpgsql_local().ctype
                            },
                            ..empty_plpgsql_local()
                        })
                        .unwrap();
                    let program = TriggerProgram {
                        locals,
                        body: parsed.body,
                        input_sites: parsed.input_sites,
                        variable_conflict: parsed.variable_conflict,
                        print_strict_params: parsed.print_strict_params,
                    };
                    let result =
                        validate_plpgsql_variable_uses(&program, false, &|_| None, scratch);
                    match expected {
                        Some(state) => assert_eq!(result.unwrap_err().sqlstate, state, "{source}"),
                        None => result.unwrap(),
                    }
                })
                .unwrap();
        }
    });
}

#[test]
fn procedural_statement_boundaries_use_sql_quoting_and_discard_scan_scratch() {
    let mut budget = Budget::new(1 << 16);
    let arena = Arena::new(&mut budget, "quoted procedural boundaries", 1 << 12).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let segments = split_trigger_statements(
            r#"EXECUTE $body$SELECT ';', 'then', 'loop'$body$; PERFORM E'it\'s; intact'; PERFORM 1 /* ; /* ; */ ; */; PERFORM 2 -- ;
; RETURN NEW; END"#,
            &arena,
        )
        .unwrap();
        assert_eq!(
            segments,
            [
                "EXECUTE $body$SELECT ';', 'then', 'loop'$body$",
                r"PERFORM E'it\'s; intact'",
                "PERFORM 1 /* ; /* ; */ ; */",
                "PERFORM 2 -- ;",
                "RETURN NEW",
                "END",
            ]
        );
        assert!(arena.used() < 1024);
    });
}

#[test]
fn procedural_lexical_exhaustion_is_a_resource_error_and_releases_scan_memory() {
    let mut budget = Budget::new(4096);
    let tiny = Arena::new(&mut budget, "lexical exhaustion", 1).unwrap();
    let scan = Arena::new(&mut budget, "failed scan scratch", 256).unwrap();
    let quoted = format!("PERFORM E'{}';", "x".repeat(512));
    crate::mem::guard::forbid_alloc(|| {
        for sql in ["SELECT", "'it''s'", r"E'\n'", "\"a\"\"b\""] {
            let mut lexer = crate::sql::lexer::Lexer::new(sql, &tiny);
            let error = lexer.next_token().unwrap_err();
            assert_eq!(error.kind.sqlstate(), "54000");
            let error = super::super::super::parse_error_to_sql(
                &crate::sql::parser::ParseError::lexical(error, sql),
            );
            assert_eq!(error.sqlstate, "54000");
            assert_eq!(error.message.as_str(), "statement too large for SQL arena");
        }
        let error = split_trigger_statements(&quoted, &scan).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(scan.used(), 0);
        assert_eq!(split_trigger_statements("NULL", &scan).unwrap(), ["NULL"]);
    });
}

#[test]
fn suspended_execution_preserves_scopes_and_failed_restore_after_workspace_destruction() {
    use super::super::execution_frame::ExecutionFrame;
    let mut budget = Budget::new(512 << 10);
    let controls = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        32 << 10,
        64,
    )
    .unwrap();
    let rows = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        64 << 10,
        16,
    )
    .unwrap();
    let mut original = Arena::new(&mut budget, "suspended original syntax", 64 << 10).unwrap();
    let mut workspace = Arena::new(&mut budget, "suspended original values", 32 << 10).unwrap();
    let mut parsing = Arena::new(&mut budget, "restored syntax", 64 << 10).unwrap();
    let partial = Arena::new(&mut budget, "partial value restoration", 1024).unwrap();
    let prefix = partial.alloc_str("preserve").unwrap();
    let output = Arena::new(&mut budget, "restored execution values", 32 << 10).unwrap();
    const SOURCE: &str =
        "BEGIN FOR x IN 1..3 LOOP FOR y IN 1..2 LOOP NULL; END LOOP; END LOOP; END";
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) =
            parse_owned_program(SOURCE, &original, &controls, PlpgsqlProgramKind::Anonymous)
                .unwrap();
        let locals = [
            TriggerLocalDecl {
                label: None,
                name: SqlName::parse("x").unwrap(),
                ctype: ColType::Text,
                constant: true,
                ..empty_plpgsql_local()
            },
            TriggerLocalDecl {
                label: None,
                name: SqlName::parse("marker").unwrap(),
                ctype: ColType::Text,
                not_null: true,
                type_mod: 4128,
                ..empty_plpgsql_local()
            },
        ];
        let mut execution =
            ExecutionFrame::new(&workspace, &code, &locals, 1, &controls, &rows).unwrap();
        execution.status.set_output_locals(&[0], locals.len());
        execution.values[0] = Datum::Text(workspace.alloc_str("outside counter").unwrap());
        let large = workspace.alloc_slice_with(4096, |_| b'z').unwrap();
        execution.values[1] = Datum::Text(core::str::from_utf8(large).unwrap());
        execution.status.found = Some(true);
        execution.status.row_count = 37;
        execution.initialization = execution_frame::Initialization::Complete;
        let mut retained = execution.status.control_frames.take().unwrap();
        {
            let mut frames = retained.execution(&workspace, &code).unwrap();
            frames.start(program.body);
            let TriggerStatement::For(outer) = program.body.statements[0] else {
                panic!("outer numeric loop")
            };
            let Step::Enter(entry) = start_numeric_loop(
                outer,
                1,
                3,
                1,
                false,
                PlpgsqlScope::frame(2),
                &execution.locals,
                &execution.values,
                &mut execution.status,
            )
            .unwrap() else {
                panic!("outer entry")
            };
            frames.enter(entry);
            let TriggerStatement::For(inner) = outer.block.statements[0] else {
                panic!("inner numeric loop")
            };
            let (locals, values, _) = frames.scope(&execution.locals, &mut execution.values);
            let changed = workspace.alloc_slice_with(128, |_| b'a').unwrap();
            values[1] = Datum::Text(core::str::from_utf8(changed).unwrap());
            let Step::Enter(entry) = start_numeric_loop(
                inner,
                1,
                2,
                1,
                false,
                PlpgsqlScope::frame(3),
                locals,
                values,
                &mut execution.status,
            )
            .unwrap() else {
                panic!("inner entry")
            };
            frames.enter(entry);
            let (_, values, _) = frames.scope(&execution.locals, &mut execution.values);
            let changed = workspace.alloc_slice_with(256, |_| b'b').unwrap();
            values[1] = Datum::Text(core::str::from_utf8(changed).unwrap());
            frames.last_mut().next = 1;
        }
        execution.status.control_frames = Some(retained);
        execution.suspend().ok().unwrap()
    });
    original.reset();
    workspace.reset();
    original.alloc_slice_with(4096, |_| 0xffu8).unwrap();
    workspace.alloc_slice_with(8192, |_| 0xffu8).unwrap();
    drop((original, workspace));
    let saved = crate::mem::guard::forbid_alloc(|| {
        // An identical replacement image still cannot change the selected body.
        let (_, replacement) =
            parse_owned_program(SOURCE, &parsing, &controls, PlpgsqlProgramKind::Anonymous)
                .unwrap();
        let before = partial.used();
        let (unchanged, error) = saved.restore(&partial, &replacement).err().unwrap();
        assert_eq!(error.sqlstate, "40001");
        assert_eq!(partial.used(), before);
        drop(replacement);
        unchanged
    });
    parsing.reset();
    let (saved, source) = crate::mem::guard::forbid_alloc(|| {
        let (_, code) = saved.source().parse(&parsing, None).unwrap();
        let saved = partial
            .with_remaining_region(|child| {
                let held = child.alloc_slice_with(64, |_| 0xa5_u8).unwrap();
                let before = partial.used();
                let (saved, error) = saved.restore(&partial, &code).err().unwrap();
                assert_eq!(error.sqlstate, "54000");
                assert_eq!(partial.used(), before);
                assert!(held.iter().all(|byte| *byte == 0xa5));
                saved
            })
            .unwrap();
        assert_eq!(partial.used(), prefix.len());
        let (saved, error) = saved.restore(&partial, &code).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        // Both numeric images fitted before the ordinary 4096-byte local failed.
        assert!(partial.high_water() > 256);
        assert_eq!(partial.used(), prefix.len());
        assert_eq!(prefix, "preserve");
        let source = saved.source().clone();
        (saved, source)
    });
    parsing.reset();
    crate::mem::guard::forbid_alloc(|| {
        parsing.alloc_slice_with(1024, |_| 0xaau8).unwrap();
        let (_, code) = source.parse(&parsing, None).unwrap();
        let mut execution = saved.restore(&output, &code).ok().unwrap();
        assert!(execution.locals[0].constant);
        assert!(execution.locals[1].not_null);
        assert_eq!(execution.locals[1].type_mod, 4128);
        assert_eq!(execution.values[0], Datum::Text("outside counter"));
        assert!(
            matches!(execution.values[1], Datum::Text(value) if value.len() == 4096 && value.bytes().all(|byte| byte == b'z'))
        );
        assert_eq!(
            execution.initialization,
            execution_frame::Initialization::Complete
        );
        assert_eq!(execution.status.found, Some(true));
        assert_eq!(execution.status.row_count, 37);
        // Restored values borrow the destination, not the released saved images.
        let mut overwritten = rows.lease::<u8>(64 << 10).unwrap();
        overwritten.extend_from_slice(&[0xff; 4096]).unwrap();
        drop(overwritten);
        let mut retained = execution.status.control_frames.take().unwrap();
        let mut frames = retained.execution(&output, &code).unwrap();
        assert_eq!(frames.state.active, 3);
        assert_eq!(frames.last().next, 1);
        let (_, current, _) = frames.scope(&execution.locals, &mut execution.values);
        assert_eq!(current[0], Datum::Text("outside counter"));
        assert_eq!(current[2], Datum::Int4(1));
        assert_eq!(current[3], Datum::Int4(1));
        assert!(
            matches!(current[1], Datum::Text(value) if value.len() == 256 && value.bytes().all(|byte| byte == b'b'))
        );
        let inner = frames.pop();
        frames.finish(
            inner,
            &execution.locals,
            &mut execution.values,
            &mut execution.status,
            true,
        );
        let outer = frames.pop();
        frames.finish(
            outer,
            &execution.locals,
            &mut execution.values,
            &mut execution.status,
            true,
        );
        assert_eq!(execution.values[0], Datum::Text("outside counter"));
        assert!(
            matches!(execution.values[1], Datum::Text(value) if value.len() == 256 && value.bytes().all(|byte| byte == b'b'))
        );
        let root = frames.pop();
        frames.finish(
            root,
            &execution.locals,
            &mut execution.values,
            &mut execution.status,
            true,
        );
        execution.status.control_frames = Some(retained);
        drop((execution, code, source));
        assert_eq!(controls.lease::<u8>(32 << 10).unwrap().capacity(), 32 << 10);
        assert_eq!(rows.lease::<u8>(64 << 10).unwrap().capacity(), 64 << 10);
    });
}

#[test]
fn failed_frame_capture_preserves_live_scopes_and_drop_returns_paused_capacity() {
    use super::super::execution_frame::ExecutionFrame;
    let mut budget = Budget::new(256 << 10);
    let controls = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let rows = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        2048,
        8,
    )
    .unwrap();
    let parsing = Arena::new(&mut budget, "failed capture syntax", 64 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "failed capture values", 16 << 10).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let (program, code) = parse_owned_program(
            "BEGIN FOR x IN 1..2 LOOP NULL; END LOOP; END",
            &parsing,
            &controls,
            PlpgsqlProgramKind::Anonymous,
        )
        .unwrap();
        let locals = [
            TriggerLocalDecl {
                label: None,
                name: SqlName::parse("x").unwrap(),
                ctype: ColType::Text,
                ..empty_plpgsql_local()
            },
            TriggerLocalDecl {
                label: None,
                name: SqlName::parse("marker").unwrap(),
                ctype: ColType::Text,
                ..empty_plpgsql_local()
            },
        ];
        let mut execution =
            ExecutionFrame::new(&workspace, &code, &locals, 0, &controls, &rows).unwrap();
        execution.values[0] = Datum::Text("outside counter");
        execution.values[1] = Datum::Text("ordinary value");
        execution.initialization = execution_frame::Initialization::Complete;
        let mut retained = execution.status.control_frames.take().unwrap();
        {
            let mut frames = retained.execution(&workspace, &code).unwrap();
            frames.start(program.body);
            let TriggerStatement::For(program) = program.body.statements[0] else {
                panic!("numeric loop")
            };
            let Step::Enter(entry) = start_numeric_loop(
                program,
                1,
                2,
                1,
                false,
                PlpgsqlScope::frame(2),
                &execution.locals,
                &execution.values,
                &mut execution.status,
            )
            .unwrap() else {
                panic!("numeric entry")
            };
            frames.enter(entry);
            let (_, values, _) = frames.scope(&execution.locals, &mut execution.values);
            let marker = workspace.alloc_slice_with(512, |_| b'q').unwrap();
            values[1] = Datum::Text(core::str::from_utf8(marker).unwrap());
        }
        execution.status.control_frames = Some(retained);
        let held = rows.lease::<u8>(1536).unwrap();
        let (mut execution, error) = execution.suspend().err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("row_work_bytes"));
        assert_eq!(rows.lease::<u8>(512).unwrap().capacity(), 512);
        assert_eq!(execution.values[0], Datum::Text("outside counter"));
        assert_eq!(execution.values[1], Datum::Text("ordinary value"));
        let mut retained = execution.status.control_frames.take().unwrap();
        {
            let mut frames = retained.execution(&workspace, &code).unwrap();
            assert_eq!(frames.state.active, 2);
            let (_, values, _) = frames.scope(&execution.locals, &mut execution.values);
            assert_eq!(values[0], Datum::Text("outside counter"));
            assert_eq!(values[2], Datum::Int4(1));
            assert!(
                matches!(values[1], Datum::Text(value) if value.len() == 512 && value.bytes().all(|byte| byte == b'q'))
            );
        }
        execution.status.control_frames = Some(retained);
        drop(held);
        let saved = execution.suspend().ok().unwrap();
        assert!(rows.lease::<u8>(2048).is_err());
        drop((saved, code));
        assert_eq!(controls.lease::<u8>(16 << 10).unwrap().capacity(), 16 << 10);
        assert_eq!(rows.lease::<u8>(2048).unwrap().capacity(), 2048);
    });
}

#[test]
fn declaration_capture_is_failure_preserving_and_repeated_pauses_share_metadata() {
    use super::super::execution_frame::ExecutionFrame;
    let mut budget = Budget::new(128 << 10);
    let controls = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        8,
    )
    .unwrap();
    let rows = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        4096,
        8,
    )
    .unwrap();
    let parsing = Arena::new(&mut budget, "declaration capture syntax", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "declaration capture values", 8192).unwrap();
    let output = Arena::new(&mut budget, "declaration capture output", 8192).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let (_, code) = parse_owned_program(
            "BEGIN NULL; END",
            &parsing,
            &controls,
            PlpgsqlProgramKind::Anonymous,
        )
        .unwrap();
        let declarations = [TriggerLocalDecl {
            label: None,
            name: SqlName::parse("selected").unwrap(),
            ctype: ColType::Text,
            constant: true,
            ..empty_plpgsql_local()
        }];
        let mut execution =
            ExecutionFrame::new(&workspace, &code, &declarations, 0, &controls, &rows).unwrap();
        execution.values[0] = Datum::Text(workspace.alloc_str("live value").unwrap());
        execution.initialization = execution_frame::Initialization::Coerce(0);
        let free = controls.lease::<u8>(4096).err().unwrap().free_bytes;
        let held = controls.lease::<u8>(free).unwrap();
        let (execution, error) = execution.suspend().err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert_eq!(execution.values[0], Datum::Text("live value"));
        assert_eq!(rows.lease::<u8>(4096).unwrap().capacity(), 4096);
        drop(held);
        let saved = execution.suspend().ok().unwrap();
        let execution = saved.restore(&output, &code).ok().unwrap();
        assert!(execution.locals[0].constant);
        assert_eq!(
            execution.initialization,
            execution_frame::Initialization::Coerce(0)
        );
        assert_eq!(execution.values[0], Datum::Text("live value"));
        let free = controls.lease::<u8>(4096).err().unwrap().free_bytes;
        let saved = execution.suspend().ok().unwrap();
        assert_eq!(controls.lease::<u8>(4096).err().unwrap().free_bytes, free);
        drop((saved, code));
        assert_eq!(controls.lease::<u8>(4096).unwrap().capacity(), 4096);
        assert_eq!(rows.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}
