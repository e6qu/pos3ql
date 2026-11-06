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
        let (_, code) =
            parse_owned_program(source, &parse, &arena, PlpgsqlProgramKind::Anonymous).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let frames = ControlFrames::new(&arena, code.layout(), &arena).unwrap();
            assert_eq!(frames.slots.len(), depth, "{source}");
            assert_eq!(frames.active, 0);
            assert!(frames.slots.iter().all(Option::is_none));
        });
    }
    let small = Arena::new(&mut budget, "procedural exhausted positions", 1).unwrap();
    let (_, code) = parse_owned_program(
        "BEGIN LOOP NULL; END LOOP; END",
        &parse,
        &arena,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let error = ControlFrames::new(&small, code.layout(), &arena)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("PL/pgSQL execution frames"));
    });
}

#[test]
fn numeric_positions_restore_parent_values_and_release_all_scope_references() {
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
        let mut status = TriggerExecutionStatus::new(&arena, code, &locals, 0).unwrap();
        let mut retained = status.control_frames.take().unwrap();
        let mut frames = retained.execution(&arena, code).unwrap();
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
fn handler_diagnostics_and_statement_positions_survive_nested_frame_changes() {
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
        let mut retained = ControlFrames::new(&arena, code.layout(), &arena).unwrap();
        let mut frames = retained.execution(&arena, code).unwrap();
        frames.start(program.body);
        frames.last_mut().next = 1;
        let error = sql_err!(sqlstate::DIVISION_BY_ZERO, "division by zero");
        let diagnostic = frames
            .state
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
    const SOURCE: &str = "BEGIN WHILE true LOOP NULL; END LOOP; END";
    let mut budget = Budget::new(1 << 18);
    let original = Arena::new(&mut budget, "owned procedural source", 1 << 16).unwrap();
    let mut parsing = Arena::new(&mut budget, "recycled procedural syntax", 1 << 16).unwrap();
    let workspace = Arena::new(&mut budget, "procedural positions", 1 << 16).unwrap();
    let replacement = Arena::new(&mut budget, "replacement procedural source", 1 << 16).unwrap();
    let replacement_parsing = Arena::new(&mut budget, "replacement syntax", 1 << 16).unwrap();
    let source = crate::mem::guard::forbid_alloc(|| {
        let text = parsing.alloc_str(SOURCE).unwrap();
        ProgramSource::capture(text, PlpgsqlProgramKind::Anonymous, &original).unwrap()
    });
    parsing.reset();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let (program, code) = source.parse(&original, &parsing).unwrap();
        let mut retained = ControlFrames::new(&workspace, code.layout(), &original).unwrap();
        let mut frames = retained.execution(&original, code).unwrap();
        let TriggerStatement::While(while_program) = program.body.statements[0] else {
            panic!("while program")
        };
        frames.start(program.body);
        frames.enter(FrameEntry::new(while_program.block, Continuation::While));
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
        let mut retained = ControlFrames::new(&workspace, saved.0, &moved).unwrap();
        assert_eq!(
            retained
                .execution(&replacement, new_code)
                .err()
                .unwrap()
                .sqlstate,
            "40001"
        );
        assert_eq!(retained.active, 0);
        assert!(retained.slots.iter().all(Option::is_none));
        let frames = retained.execution(&moved, code).unwrap();
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
        ControlFrames::new(&workspace, code.layout(), &original).unwrap()
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
