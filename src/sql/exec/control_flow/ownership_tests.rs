use super::*;
use crate::mem::budget::Budget;

fn parse_owned_program<'a>(
    text: &str,
    parsing: &'a Arena,
    source: &'a Arena,
    kind: PlpgsqlProgramKind,
) -> Result<(ParsedTriggerProgram<'a>, CompiledProgram<'a>), SqlError> {
    ProgramSource::capture(text, kind, source)?.parse(source, parsing)
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
        3,
    )
    .unwrap();
    let (_, code) = parse_owned_program(
        "BEGIN NULL; END",
        &parsing,
        &workspace,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let locals = [TriggerLocalDecl {
        name: SqlName::parse("value").unwrap(),
        ctype: ColType::Text,
        ..empty_plpgsql_local()
    }];
    crate::mem::guard::forbid_alloc(|| {
        let error = TriggerExecutionStatus::new(&workspace, &parsing, code, &locals, 1, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "40001");
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
        let held = pool.lease::<u8>(64).unwrap();
        let error = TriggerExecutionStatus::new(&workspace, &workspace, code, &locals, 1, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_leases"));
        drop(held);
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);

        let mut status =
            TriggerExecutionStatus::new(&workspace, &workspace, code, &locals, 1, &pool).unwrap();
        status.set_output_locals(&[0], locals.len());
        status
            .shadow_output_local(0, Datum::Text("owned output"), &workspace)
            .unwrap();
        assert_eq!(pool.lease::<u8>(1).err().unwrap().available_leases, 0);
        assert_eq!(
            status.outputs[0].value.read(&workspace, &parsing).unwrap(),
            Datum::Text("owned output")
        );
        drop(status);
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}

#[test]
fn shared_control_capacity_preserves_live_positions_and_returns_after_owner_drop() {
    let mut budget = Budget::new(64 << 10);
    let source = Arena::new(&mut budget, "shared control source", 16 << 10).unwrap();
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
        &source,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut retained = ControlFrames::new(&arena, code.layout(), &source, &pool).unwrap();

        {
            let mut frames = retained.execution(&source, &arena, code).unwrap();
            frames.start(program.body);
            frames.last_mut().next = 1;
        }
        let free = pool.lease::<u8>(4096).err().unwrap().free_bytes;
        let held = pool.lease::<u8>(free).unwrap();
        let error = ControlFrames::new(&arena, code.layout(), &source, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert!(error.message.as_str().contains("execution_control_leases"));
        assert_eq!(retained.active, 1);
        assert_eq!(retained.slots[0].as_ref().unwrap().next, 1);
        drop(held);
        let second = ControlFrames::new(&arena, code.layout(), &source, &pool).unwrap();
        assert_eq!(second.active, 0);
        assert_eq!(second.slots.len(), 2);
        drop(second);
        let mut frames = retained.execution(&source, &arena, code).unwrap();
        assert_eq!(frames.last().next, 1);
        frames.pop();
        drop(retained);
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
            parse_owned_program(source, &parse, &arena, PlpgsqlProgramKind::Anonymous).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let frames = ControlFrames::new(&arena, code.layout(), &arena, &pool).unwrap();
            assert_eq!(frames.slots.len(), depth, "{source}");
            assert_eq!(frames.active, 0);
            assert!(frames.slots.iter().all(Option::is_none));
        });
    }
    let small = Arena::new(&mut budget, "procedural empty payload workspace", 1).unwrap();
    let (_, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parse,
        &arena,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let frames = ControlFrames::new(&small, code.layout(), &arena, &pool).unwrap();
        assert_eq!(frames.resources.len(), 2);
        assert_eq!(small.used(), 0);
    });
}

#[test]
fn output_bindings_own_outer_values_through_nested_shadows_and_source_reuse() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let mut budget = Budget::new(128 << 10);
    let source = Arena::new(&mut budget, "output binding source", 16 << 10).unwrap();
    let parsing = Arena::new(&mut budget, "output binding syntax", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "output binding state", 16 << 10).unwrap();
    let mut temporary = Arena::new(&mut budget, "output binding value", 16 << 10).unwrap();
    let output = Arena::new(&mut budget, "output binding copies", 16 << 10).unwrap();
    let (_, code) = parse_owned_program(
        "BEGIN NULL; END",
        &parsing,
        &workspace,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let locals = [TriggerLocalDecl {
        name: SqlName::parse("value").unwrap(),
        ctype: ColType::Text,
        ..empty_plpgsql_local()
    }];
    let mut status =
        TriggerExecutionStatus::new(&workspace, &workspace, code, &locals, 1, &pool).unwrap();
    status.set_output_locals(&[0], locals.len());
    crate::mem::guard::forbid_alloc(|| {
        let text = temporary.alloc_str("original output").unwrap();
        status
            .shadow_output_local(0, Datum::Text(text), &workspace)
            .unwrap();
        let used = workspace.used();
        status
            .shadow_output_local(0, Datum::Int4(7), &workspace)
            .unwrap();
        assert_eq!(workspace.used(), used);
        assert_eq!(status.outputs[0].shadow_depth, 2);
        let foreign_used = source.used();
        assert_eq!(
            status
                .shadow_output_local(0, Datum::Int4(9), &source)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(status.outputs[0].shadow_depth, 2);
        assert_eq!(source.used(), foreign_used);
    });
    temporary.reset();
    crate::mem::guard::forbid_alloc(|| {
        temporary.alloc_slice_with(2048, |_| 0xffu8).unwrap();
        let first = status.outputs[0].value.read(&workspace, &output).unwrap();
        assert_eq!(first, Datum::Text("original output"));
        status.unshadow_output_local(0);
        assert_eq!(status.outputs[0].shadow_depth, 1);
        assert_eq!(
            status.outputs[0].value.read(&workspace, &output).unwrap(),
            first
        );
        status.unshadow_output_local(0);
        let error = status
            .shadow_output_local(0, Datum::Null, &source)
            .unwrap_err();
        assert_eq!(error.sqlstate, "XX000");
        assert_eq!(status.outputs[0].shadow_depth, 0);
        let used = workspace.used();
        for _ in 0..u8::MAX {
            status
                .shadow_output_local(0, Datum::Null, &workspace)
                .unwrap();
        }
        assert_eq!(
            status
                .shadow_output_local(0, Datum::Int4(9), &workspace)
                .unwrap_err()
                .sqlstate,
            "54000"
        );
        assert_eq!(status.outputs[0].shadow_depth, u8::MAX);
        assert!(
            status.outputs[0]
                .value
                .read(&workspace, &output)
                .unwrap()
                .is_null()
        );
        assert_eq!(workspace.used(), used);
        assert_eq!(first, Datum::Text("original output"));
        for _ in 0..u8::MAX {
            status.unshadow_output_local(0);
        }
        assert_eq!(status.outputs[0].shadow_depth, 0);
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
        &arena,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    let locals = [
        TriggerLocalDecl {
            name: SqlName::parse("x").unwrap(),
            ctype: ColType::Text,
            ..empty_plpgsql_local()
        },
        TriggerLocalDecl {
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
            TriggerExecutionStatus::new(&arena, &arena, code, &locals, 0, &pool).unwrap();
        let mut retained = status.control_frames.take().unwrap();

        let mut frames = retained.execution(&arena, &arena, code).unwrap();
        let mut values = [Datum::Text("outside"), Datum::Int4(0)];
        for success in [false, true] {
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
                &arena,
            )
            .unwrap() else {
                panic!("numeric frame")
            };
            frames.enter(frame);
            let (scope, current) = frames.scope(&locals, &mut values);
            assert_eq!(current[0], Datum::Int4(1));
            assert_eq!(scope[1].name.as_str(), "n");
            assert_eq!(scope[0].scope, PlpgsqlScope::frame(2));
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
                &arena,
            )
            .unwrap() else {
                panic!("nested frame")
            };
            frames.enter(frame);
            let (scope, current) = frames.scope(&locals, &mut values);
            assert_eq!(scope.len(), 3);
            assert_eq!(scope[2].name.as_str(), "j");
            assert_eq!(scope[2].scope, PlpgsqlScope::frame(3));
            current[1] = Datum::Int4(41);
            status.set_found(false);
            let inner = frames.pop();
            frames.finish(&arena, inner, &locals, &mut values, &mut status, success);
            let (_, current) = frames.scope(&locals, &mut values);
            assert_eq!(current[0], Datum::Int4(1));
            assert_eq!(current[1], Datum::Int4(41));
            let outer = frames.pop();
            frames.finish(&arena, outer, &locals, &mut values, &mut status, success);
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
        status.control_frames = Some(retained);
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
            name: SqlName::parse("x").unwrap(),
            ctype: ColType::Text,
            ..empty_plpgsql_local()
        },
        TriggerLocalDecl {
            name: SqlName::parse("n").unwrap(),
            ctype: ColType::Text,
            ..empty_plpgsql_local()
        },
    ];
    let source = ProgramSource::capture(SOURCE, PlpgsqlProgramKind::Anonymous, &original).unwrap();
    let (mut status, retained, mut values) = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = source.parse(&original, &parsing).unwrap();
        let mut status =
            TriggerExecutionStatus::new(&workspace, &original, code, &locals, 0, &pool).unwrap();
        let mut retained = status.control_frames.take().unwrap();

        let mut values = [Datum::Text("outside counter"), Datum::Text("before")];
        {
            let mut frames = retained.execution(&original, &workspace, code).unwrap();
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
                &workspace,
            )
            .unwrap() else {
                panic!("numeric loop entry")
            };
            frames.enter(entry);
            let (_, current) = frames.scope(&locals, &mut values);
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
        moved_source.recycle_front();
        parsing.alloc_slice_with(1024, |_| 0xffu8).unwrap();
        let (_, code) = source.parse(&moved_source, &parsing).unwrap();
        assert_eq!(
            moved
                .execution(&moved_source, &foreign, code)
                .err()
                .unwrap()
                .sqlstate,
            "40001"
        );
        let mut frames = moved.execution(&moved_source, &workspace, code).unwrap();
        assert_eq!(frames.state.active, 3);
        assert_eq!(frames.last().next, 1);
        let TriggerStatement::While(program) = frames.source_statement(frames.last().source) else {
            panic!("rebuilt while loop")
        };
        assert!(matches!(program.condition, Expr::Bool(true)));
        let (_, current) = frames.scope(&locals, &mut values);
        assert_eq!(
            current,
            &[Datum::Int4(1), Datum::Text("changed once before waiting")]
        );
        let child = frames.pop();
        frames.finish(&workspace, child, &locals, &mut values, &mut status, true);
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
        frames.finish(&workspace, numeric, &locals, &mut values, &mut status, true);
        assert_eq!(
            values,
            [
                Datum::Text("outside counter"),
                Datum::Text("changed once before waiting")
            ]
        );
        let root = frames.pop();
        frames.finish(&workspace, root, &locals, &mut values, &mut status, true);
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
    let (_, code) = source.parse(&moved_source, &parsing).unwrap();
    let mut reset = ControlFrames::new(&owner, code.layout(), &moved_source, &pool).unwrap();
    reset.execution(&moved_source, &owner, code).unwrap();
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
    let source = Arena::new(&mut budget, "reservation source", 16 << 10).unwrap();
    let parsing = Arena::new(&mut budget, "reservation syntax", 16 << 10).unwrap();
    let arena = Arena::new(&mut budget, "reservation positions", 4 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        5,
    )
    .unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parsing,
        &source,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut retained = ControlFrames::new(&arena, code.layout(), &source, &pool).unwrap();
        {
            let mut frames = retained.execution(&source, &arena, code).unwrap();
            frames.start(program.body);
            frames.last_mut().next = 1;
        }
        let first = pool.lease::<u8>(64).unwrap();
        let second = pool.lease::<u8>(64).unwrap();
        let error = ControlFrames::new(&arena, code.layout(), &source, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_leases"));
        assert_eq!(pool.lease::<u8>(4096).err().unwrap().available_leases, 1);
        assert_eq!(retained.active, 1);
        assert_eq!(retained.slots[0].as_ref().unwrap().next, 1);
        drop(second);
        let retry = ControlFrames::new(&arena, code.layout(), &source, &pool).unwrap();
        assert_eq!(retry.active, 0);
        drop(retry);
        drop(first);
        retained.execution(&source, &arena, code).unwrap().pop();
        drop(retained);
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
    let source = Arena::new(&mut budget, "row position source", 32 << 10).unwrap();
    let parsing = Arena::new(&mut budget, "row position syntax", 32 << 10).unwrap();
    let mut workspace = Arena::new(&mut budget, "row position resources", 32 << 10).unwrap();
    let row_pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, 4096, 4).unwrap();
    let (program, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parsing,
        &source,
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
                ControlFrames::new(&workspace, code.layout(), &source, &control_pool).unwrap();

            {
                let mut frames = retained.execution(&source, &workspace, code).unwrap();
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
                        target: SqlName::parse("x").unwrap(),
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
        &arena,
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
        let mut retained = ControlFrames::new(&arena, code.layout(), &arena, &pool).unwrap();

        let mut frames = retained.execution(&arena, &arena, code).unwrap();
        frames.start(program.body);
        frames.last_mut().next = 1;
        let error = sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero");
        let diagnostic = frames
            .state
            .diagnostics
            .capture(&arena, &error, None, "handler")
            .unwrap();
        frames.push(
            FrameEntry::new(handler, EntryState::Handler(diagnostic)),
            FrameSource::Handler,
        );
        frames.enter(FrameEntry::new(
            branches.branches[0].block,
            EntryState::Once,
        ));
        assert_eq!(
            frames
                .diagnostic()
                .unwrap()
                .0
                .variables(&arena, &arena)
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
        assert_eq!(frames.diagnostic().unwrap().0.sqlstate, "22012");
        let handler = frames.pop();
        let Continuation::Handler(diagnostic) = handler.continuation else {
            panic!("handler");
        };
        frames.state.diagnostics.release(&arena, diagnostic);
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
    let original = Arena::new(&mut budget, "owned procedural source", 1 << 16).unwrap();
    let mut parsing = Arena::new(&mut budget, "recycled procedural syntax", 1 << 16).unwrap();
    let workspace = Arena::new(&mut budget, "procedural positions", 1 << 16).unwrap();
    let replacement = Arena::new(&mut budget, "replacement procedural source", 1 << 14).unwrap();
    let replacement_parsing = Arena::new(&mut budget, "replacement syntax", 1 << 14).unwrap();
    let source = crate::mem::guard::forbid_alloc(|| {
        let text = parsing.alloc_str(SOURCE).unwrap();
        ProgramSource::capture(text, PlpgsqlProgramKind::Anonymous, &original).unwrap()
    });
    parsing.reset();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = source.parse(&original, &parsing).unwrap();
        let mut retained = ControlFrames::new(&workspace, code.layout(), &original, &pool).unwrap();

        let mut frames = retained.execution(&original, &workspace, code).unwrap();
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
    let mut moved = original;
    crate::mem::guard::forbid_alloc(|| {
        moved.recycle_front();
        moved.alloc_str("replacement front source").unwrap();
        parsing.alloc_str("replacement parsing scratch").unwrap();
        let (_, code) = source.parse(&moved, &parsing).unwrap();
        let replaced = ProgramSource::capture(
            "BEGIN WHILE false LOOP NULL; END LOOP; END",
            PlpgsqlProgramKind::Anonymous,
            &replacement,
        )
        .unwrap();
        let (_, new_code) = replaced.parse(&replacement, &replacement_parsing).unwrap();
        let mut retained = ControlFrames::new(&workspace, saved.0, &moved, &pool).unwrap();

        assert_eq!(
            retained
                .execution(&replacement, &workspace, new_code)
                .err()
                .unwrap()
                .sqlstate,
            "40001"
        );
        assert_eq!(retained.active, 0);
        assert!(retained.slots.iter().all(Option::is_none));
        let frames = retained.execution(&moved, &workspace, code).unwrap();
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
fn frame_layouts_outlive_syntax_and_reject_foreign_or_reset_source_images() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let mut budget = Budget::new(64 << 10);
    let mut tiny_budget = Budget::new(1);
    let tiny = Arena::new(&mut tiny_budget, "exhausted procedural source", 1).unwrap();
    let original = Arena::new(&mut budget, "independent procedural source", 16 << 10).unwrap();
    let mut parsing = Arena::new(&mut budget, "independent procedural syntax", 16 << 10).unwrap();
    let workspace = Arena::new(&mut budget, "independent procedural frames", 16 << 10).unwrap();
    let other = Arena::new(&mut budget, "foreign procedural source", 16 << 10).unwrap();
    let source = ProgramSource::capture(
        "BEGIN LOOP NULL; END LOOP; END",
        PlpgsqlProgramKind::Anonymous,
        &original,
    )
    .unwrap();
    let retained = {
        let (_, code) = source.parse(&original, &parsing).unwrap();
        ControlFrames::new(&workspace, code.layout(), &original, &pool).unwrap()
    };
    parsing.reset();
    let mut moved = original;
    crate::mem::guard::forbid_alloc(|| {
        moved.recycle_front();
        let error = ProgramSource::capture("BEGIN NULL; END", PlpgsqlProgramKind::Anonymous, &tiny)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(
            error.message.as_str(),
            "PL/pgSQL source image exceeds statement memory"
        );
        retained.require_source(&moved, source).unwrap();
        assert_eq!(
            retained
                .require_source(&other, source)
                .unwrap_err()
                .sqlstate,
            "40001"
        );
        assert_eq!(retained.active, 0);
        moved.reset();
        assert_eq!(
            retained
                .require_source(&moved, source)
                .unwrap_err()
                .sqlstate,
            "40001"
        );
        assert_eq!(
            source.parse(&moved, &parsing).err().unwrap().sqlstate,
            "40001"
        );
        assert_eq!(retained.active, 0);
        assert!(retained.slots.iter().all(Option::is_none));
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
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&arena, 1).unwrap();
        let caught = buffers
            .capture(
                &arena,
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
            TriggerInvocation::anonymous,
        )
        .unwrap()
        .with_input_sites(program.input_sites);
        let bound = bind_exception_variables(
            invocation,
            Some((caught, PlpgsqlScope::frame(2))),
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
        buffers.release(&arena, caught);
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
                    assert_eq!(open.variable.as_str(), "cursor \" name");
                    for source in [
                        "FETCH NEXT FROM \"cursor \"\" name\" INTO \"output value\"",
                        "MOVE NEXT FROM \"cursor \"\" name\"",
                    ] {
                        let motion = parse_plpgsql_cursor_motion(source, scratch)
                            .unwrap()
                            .unwrap();
                        assert_eq!(motion.variable, open.variable);
                        if !motion.move_only {
                            assert_eq!(motion.targets[0].as_str(), "output value");
                        }
                    }
                    assert_eq!(
                        parse_plpgsql_cursor_close("CLOSE \"cursor \"\" name\"", scratch).unwrap(),
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
    let mut invocation =
        super::super::super::type_input_plans::with_owner(owner, TriggerInvocation::anonymous)
            .unwrap();
    invocation.program_kind = PlpgsqlProgramKind::Trigger;
    invocation.name = "actual trigger";
    let mut locals = ["tg_name", "found", "sqlstate", "sqlerrm"].map(|name| TriggerLocalDecl {
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
                name: same_name,
                ctype: ColType::Int4,
                ..empty_plpgsql_local()
            },
            TriggerLocalDecl {
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
        assert_eq!(trigger_local_index(&declarations, same_name).unwrap(), 1);
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
                    // This pass uses names and mutability, independently of catalog types.
                    let locals = scratch
                        .alloc_slice_with(parsed.locals.len(), |index| TriggerLocalDecl {
                            name: parsed.locals[index].name,
                            constant: parsed.locals[index].constant,
                            ..empty_plpgsql_local()
                        })
                        .unwrap();
                    let initializers = scratch
                        .alloc_slice_with(parsed.locals.len(), |index| parsed.locals[index].initial)
                        .unwrap();
                    let program = TriggerProgram {
                        locals,
                        initializers,
                        body: parsed.body,
                        input_sites: parsed.input_sites,
                    };
                    let result = validate_plpgsql_constant_assignments(&program, &|_| false);
                    match expected {
                        Some(state) => assert_eq!(result.unwrap_err().sqlstate, state, "{source}"),
                        None => result.unwrap(),
                    }
                })
                .unwrap();
        }
    });
}
