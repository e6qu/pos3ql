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
        let mut status = TriggerExecutionStatus::new(&arena, program.body, &locals).unwrap();
        let mut frames = status.control_frames.take().unwrap();
        let mut values = [Datum::Text("outside"), Datum::Int4(0)];
        for success in [false, true] {
            frames.push(Frame::new(program.body, Continuation::Once));
            let Step::Enter(frame) =
                start_numeric_loop(outer, 1, 2, 1, false, &locals, &values, &mut status).unwrap()
            else {
                panic!("numeric frame")
            };
            frames.push(frame);
            let (scope, current) = frames.scope(&locals, &mut values);
            assert_eq!(current[0], Datum::Int4(1));
            let Step::Enter(frame) =
                start_numeric_loop(inner, 1, 2, 1, false, scope, current, &mut status).unwrap()
            else {
                panic!("nested frame")
            };
            frames.push(frame);
            let (scope, current) = frames.scope(&locals, &mut values);
            assert_eq!(scope.len(), 3);
            assert_eq!(scope[2].name.as_str(), "j");
            current[1] = Datum::Int4(41);
            status.set_found(false);
            let inner = frames.pop();
            frames.finish(inner, &locals, &mut values, &mut status, success);
            let (_, current) = frames.scope(&locals, &mut values);
            assert_eq!(current[0], Datum::Int4(1));
            assert_eq!(current[1], Datum::Int4(41));
            let outer = frames.pop();
            frames.finish(outer, &locals, &mut values, &mut status, success);
            assert_eq!(values, [Datum::Text("outside"), Datum::Int4(41)]);
            assert_eq!(status.found, Some(success));
            assert!(matches!(frames.pop().continuation, Continuation::Once));
            assert!(frames.slots.iter().all(Option::is_none));
            let workspace = status.numeric_frames.take().unwrap();
            assert!(workspace.locals.iter().all(|local| local.initial.is_none()));
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
    let diagnostic = TriggerExceptionDiagnostic {
        sqlstate: sqlstate::DIVISION_BY_ZERO.into(),
        sqlstate_text: "22012",
        message: "division by zero",
        detail: None,
        hint: None,
        context: "handler",
    };
    crate::mem::guard::forbid_alloc(|| {
        let mut frames = ControlFrames::new(&arena, program.body).unwrap();
        frames.push(Frame::new(program.body, Continuation::Once));
        frames.last_mut().next = 1;
        frames.push(Frame::new(program.body, Continuation::Handler(diagnostic)));
        frames.last_mut().next = 2;
        frames.push(Frame::new(program.body, Continuation::Once));
        assert_eq!(frames.diagnostic().unwrap().message, "division by zero");
        frames.pop();
        assert_eq!(frames.last().next, 2);
        assert_eq!(frames.diagnostic().unwrap().sqlstate, "22012");
        frames.pop();
        assert!(frames.diagnostic().is_none());
        assert_eq!(frames.last().next, 1);
        frames.pop();
        assert!(frames.slots.iter().all(Option::is_none));
    });
}
