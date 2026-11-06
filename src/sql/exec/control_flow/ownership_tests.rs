use super::*;
use crate::mem::budget::Budget;

#[test]
fn lexical_frame_capacity_covers_every_branch_and_fails_before_execution() {
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
        let program = parse_plpgsql_program(source, &parse, PlpgsqlProgramKind::Anonymous).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let frames = ControlFrames::new(&arena, program.body).unwrap();
            assert_eq!(frames.slots.len(), depth, "{source}");
            assert_eq!(frames.active, 0);
            assert!(frames.slots.iter().all(Option::is_none));
        });
    }
    let small = Arena::new(&mut budget, "procedural exhausted positions", 1).unwrap();
    let program = parse_plpgsql_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parse,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let error = ControlFrames::new(&small, program.body).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("PL/pgSQL execution frames"));
    });
}

#[test]
fn numeric_positions_restore_parent_values_and_release_all_scope_references() {
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "procedural numeric positions parsing", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "procedural numeric positions", 1 << 16).unwrap();
    let program = parse_plpgsql_program(
        "BEGIN FOR x IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
        &parse,
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
        let mut status = TriggerExecutionStatus::new(&arena, program.body, &locals, 0).unwrap();
        let mut frames = status.control_frames.take().unwrap();
        let mut values = [Datum::Text("outside"), Datum::Int4(0)];
        for success in [false, true] {
            frames.start(program.body);
            let Step::Enter(frame) =
                start_numeric_loop(outer, 1, 2, 1, false, &locals, &values, &mut status).unwrap()
            else {
                panic!("numeric frame")
            };
            frames.enter(frame);
            let (scope, current) = frames.scope(&locals, &mut values);
            assert_eq!(current[0], Datum::Int4(1));
            assert_eq!(scope[1].name.as_str(), "n");
            let Step::Enter(frame) =
                start_numeric_loop(inner, 1, 2, 1, false, scope, current, &mut status).unwrap()
            else {
                panic!("nested frame")
            };
            frames.enter(frame);
            let (scope, current) = frames.scope(&locals, &mut values);
            assert_eq!(scope.len(), 3);
            assert_eq!(scope[2].name.as_str(), "j");
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
            assert!(frames.slots.iter().all(Option::is_none));
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
        status.control_frames = Some(frames);
    });
}

#[test]
fn handler_diagnostics_and_statement_positions_survive_nested_frame_changes() {
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "procedural handler positions parsing", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "procedural handler positions", 1 << 16).unwrap();
    let program = parse_plpgsql_program(
        "BEGIN BEGIN NULL; EXCEPTION WHEN OTHERS THEN IF true THEN NULL; END IF; END; END",
        &parse,
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
        let mut frames = ControlFrames::new(&arena, program.body).unwrap();
        frames.start(program.body);
        frames.last_mut().next = 1;
        let error = sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero");
        let diagnostic = frames
            .diagnostics
            .capture(&arena, &error, None, "handler")
            .unwrap();
        frames.push(
            FrameEntry::new(handler, Continuation::Handler(diagnostic)),
            FrameSource::Handler,
        );
        frames.enter(FrameEntry::new(
            branches.branches[0].block,
            Continuation::Once,
        ));
        assert_eq!(
            frames
                .diagnostic()
                .unwrap()
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
        assert_eq!(frames.diagnostic().unwrap().sqlstate, "22012");
        let handler = frames.pop();
        let Continuation::Handler(diagnostic) = handler.continuation else {
            panic!("handler");
        };
        frames.diagnostics.release(&arena, diagnostic);
        assert!(frames.diagnostic().is_none());
        assert_eq!(frames.last().next, 1);
        frames.pop();
        assert!(frames.slots.iter().all(Option::is_none));
    });
}

#[test]
fn positions_resolve_the_same_source_after_original_parse_storage_is_released() {
    const SOURCE: &str = "BEGIN WHILE true LOOP NULL; END LOOP; END";
    let saved = {
        let mut budget = Budget::new(1 << 18);
        let parse = Arena::new(&mut budget, "original procedural source", 1 << 16).unwrap();
        let arena = Arena::new(&mut budget, "original procedural positions", 1 << 16).unwrap();
        let program = parse_plpgsql_program(SOURCE, &parse, PlpgsqlProgramKind::Anonymous).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let mut frames = ControlFrames::new(&arena, program.body).unwrap();
            let TriggerStatement::While(while_program) = program.body.statements[0] else {
                panic!("while program")
            };
            frames.start(program.body);
            frames.enter(FrameEntry::new(while_program.block, Continuation::While));
            frames.last_mut().next = 1;
            let frame = frames.pop();
            let position = (frame.block, frame.next, frame.source);
            frames.pop();
            position
        })
    };
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "restored procedural source", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "restored procedural positions", 1 << 16).unwrap();
    let program = parse_plpgsql_program(SOURCE, &parse, PlpgsqlProgramKind::Anonymous).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let frames = ControlFrames::new(&arena, program.body).unwrap();
        assert_eq!(saved.1, frames.block(saved.0).statements.len());
        assert!(matches!(
            frames.block(saved.0).statements[0],
            TriggerStatement::Null
        ));
        let TriggerStatement::While(program) = frames.source_statement(saved.2) else {
            panic!("restored while position")
        };
        assert!(matches!(program.condition, Expr::Bool(true)));
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
        let bound = bind_exception_variables(invocation, Some(&caught), statement, &arena).unwrap();
        let variables = bound
            .exception
            .expect("lowering preserves handler name dependencies");
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
        for (source, expected) in [
            ("MixedCase", "mixedcase"),
            ("\"MixedCase\"", "MixedCase"),
            ("\"spaced name\"", "spaced name"),
            ("\"doubled\"\"quote\"", "doubled\"quote"),
            ("/* before */ MiXeD -- after", "mixed"),
        ] {
            let before = arena.used();
            assert_eq!(
                parse_plpgsql_identifier(source, &arena).unwrap().as_str(),
                expected
            );
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
