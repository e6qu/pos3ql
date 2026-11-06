use super::*;
use crate::mem::budget::Budget;

fn layout_locals<'a>(
    program: &ParsedTriggerProgram<'_>,
    arena: &'a Arena,
) -> &'a [TriggerLocalDecl] {
    // Capacity uses names only; native execution supplies resolved types.
    arena
        .alloc_slice_with(program.locals.len(), |index| TriggerLocalDecl {
            name: program.locals[index].name,
            ..empty_plpgsql_local()
        })
        .unwrap()
}

#[test]
fn nested_frames_reuse_backing_and_release_references_before_scratch_rewinds() {
    let mut budget = Budget::new(1 << 18);
    let parse = Arena::new(&mut budget, "numeric loop program", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "numeric loop frames", 1 << 16).unwrap();
    let program = parse_plpgsql_program(
        "DECLARE x integer; y integer; BEGIN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
        &parse,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut frames =
            NumericLoopFrames::new(&arena, program.body, layout_locals(&program, &parse)).unwrap();
        let mark = arena.mark();
        let mut prior_high_water = None;
        for _ in 0..256 {
            {
                let outer = frames.take().unwrap();
                outer.values[0] = Datum::Int4(41);
                let inner = frames.take().unwrap();
                let temporary = arena.alloc_str("temporary loop text").unwrap();
                inner.values[0] = Datum::Text(temporary);
                inner.locals[0].name = SqlName::parse("temporary_name").unwrap();
                assert_eq!(frames.take().err().unwrap().sqlstate, "54000");
                frames.restore(inner);
                assert_eq!(outer.values[0], Datum::Int4(41));
                frames.restore(outer);
            }
            // Both frames have cleared all references into this suffix.
            unsafe { arena.rewind_to(mark) };
            let outer = frames.take().unwrap();
            assert!(outer.values.iter().all(Datum::is_null));
            assert!(
                outer
                    .locals
                    .iter()
                    .all(|local| local.name == SqlName::EMPTY)
            );
            let inner = frames.take().unwrap();
            assert!(inner.values.iter().all(Datum::is_null));
            frames.restore(inner);
            frames.restore(outer);
            if let Some(prior) = prior_high_water {
                assert_eq!(arena.high_water(), prior);
            }
            prior_high_water = Some(arena.high_water());
        }
    });
}

#[test]
fn every_procedural_branch_reserves_exact_numeric_scope_capacity() {
    let mut budget = Budget::new(1 << 18);
    let mut parse = Arena::new(&mut budget, "numeric loop parsing", 1 << 16).unwrap();
    let mut arena = Arena::new(&mut budget, "numeric loop capacity", 1 << 16).unwrap();
    for (source, expected) in [
        ("BEGIN NULL; END", &[] as &[usize]),
        (
            "BEGIN FOR i IN 1..2 LOOP NULL; END LOOP; FOR j IN 1..2 LOOP NULL; END LOOP; END",
            &[1],
        ),
        (
            "BEGIN IF false THEN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END IF; END",
            &[1, 2],
        ),
        (
            "BEGIN CASE WHEN true THEN FOR i IN 1..2 LOOP NULL; END LOOP; ELSE FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END CASE; END",
            &[1, 2],
        ),
        (
            "BEGIN BEGIN FOR i IN 1..2 LOOP NULL; END LOOP; EXCEPTION WHEN OTHERS THEN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END; END",
            &[1, 2],
        ),
        (
            "BEGIN LOOP WHILE false LOOP FOR i IN 1..2 LOOP NULL; END LOOP; END LOOP; EXIT; END LOOP; END",
            &[1],
        ),
        (
            "DECLARE n integer; BEGIN FOR n IN SELECT 1 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[2],
        ),
        (
            "DECLARE n integer; BEGIN FOREACH n IN ARRAY ARRAY[1] LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[2],
        ),
        (
            "DECLARE i integer; BEGIN FOR i IN 1..2 LOOP FOR i IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[1, 1],
        ),
        (
            "DECLARE i integer; BEGIN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[1, 2],
        ),
        (
            "DECLARE i integer; BEGIN FOR j IN 1..2 LOOP FOR i IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[2, 2],
        ),
    ] {
        parse.reset();
        arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            let program =
                parse_plpgsql_program(source, &parse, PlpgsqlProgramKind::Anonymous).unwrap();
            let mut frames =
                NumericLoopFrames::new(&arena, program.body, layout_locals(&program, &parse))
                    .unwrap();
            let mut held = [None, None];
            for (depth, frame) in held[..expected.len()].iter_mut().enumerate() {
                let next = frames.take().unwrap();
                assert_eq!(next.values.len(), expected[depth]);
                *frame = Some(next);
            }
            assert_eq!(frames.take().err().unwrap().sqlstate, "54000");
            for frame in held[..expected.len()].iter_mut().rev() {
                frames.restore(frame.take().unwrap());
            }
        });
    }
}

#[test]
fn reservation_failure_names_statement_memory_without_heap_growth() {
    let mut budget = Budget::new(1 << 17);
    let parse = Arena::new(&mut budget, "numeric reservation program", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "numeric reservation bytes", 64).unwrap();
    let program = parse_plpgsql_program(
        "DECLARE x integer; y integer; BEGIN FOR i IN 1..2 LOOP NULL; END LOOP; END",
        &parse,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let error = NumericLoopFrames::new(&arena, program.body, layout_locals(&program, &parse))
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(
            error.message.as_str(),
            "PL/pgSQL numeric loop frames exceed statement memory"
        );
    });
}
