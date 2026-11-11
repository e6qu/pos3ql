use super::*;
use crate::mem::budget::Budget;

fn layout_locals<'a>(
    program: &ParsedTriggerProgram<'_>,
    arena: &'a Arena,
) -> &'a [TriggerLocalDecl] {
    // Capacity uses names only; native execution supplies resolved types.
    arena
        .alloc_slice_with(program.locals.len(), |index| TriggerLocalDecl {
            label: None,
            name: program.locals[index].name,
            ..empty_plpgsql_local()
        })
        .unwrap()
}

#[test]
fn numeric_scope_owners_outlive_layout_and_syntax_without_borrowing_their_memory() {
    let mut budget = Budget::new(64 << 10);
    let parsing = Arena::new(&mut budget, "owned scope syntax", 16 << 10).unwrap();
    let layout = Arena::new(&mut budget, "owned scope layout", 4096).unwrap();
    let source = Arena::new(&mut budget, "owned scope source values", 16 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        8,
    )
    .unwrap();
    let mut frames = crate::mem::guard::forbid_alloc(|| {
        let program = parse_plpgsql_program(
            "BEGIN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &parsing,
            PlpgsqlProgramKind::Anonymous,
        )
        .unwrap();
        let frames = NumericLoopFrames::new(&layout, program.body, &[], &pool, &pool).unwrap();
        assert_eq!(layout.used(), 0);
        frames
    });
    drop(parsing);
    drop(layout);
    crate::mem::guard::forbid_alloc(|| {
        let mut outer = frames.take().unwrap();
        let mut inner = frames.take().unwrap();
        outer.values[0] = Datum::Text(source.alloc_str("outer remains selected").unwrap());
        inner.values[0] = Datum::Int4(7);
        inner.values[1] = Datum::Text(source.alloc_str("inner remains selected").unwrap());
        drop(frames);
        let moved = outer;
        assert_eq!(moved.values[0], Datum::Text("outer remains selected"));
        assert_eq!(inner.values[1], Datum::Text("inner remains selected"));
        drop(inner);
        assert!(pool.lease::<u8>(16 << 10).is_err());
        assert_eq!(moved.values[0], Datum::Text("outer remains selected"));
        drop(moved);
        assert_eq!(pool.lease::<u8>(16 << 10).unwrap().capacity(), 16 << 10);
    });
}

#[test]
fn numeric_control_admission_preserves_live_owners_and_releases_partial_reservations() {
    let mut budget = Budget::new(64 << 10);
    let parse = Arena::new(&mut budget, "numeric control syntax", 16 << 10).unwrap();
    let arena = Arena::new(&mut budget, "numeric control values", 16 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        5,
    )
    .unwrap();
    let program = parse_plpgsql_program(
        "DECLARE x integer; BEGIN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
        &parse,
        PlpgsqlProgramKind::Anonymous,
    ).unwrap();
    let locals = layout_locals(&program, &parse);
    crate::mem::guard::forbid_alloc(|| {
        let held = pool.lease::<u8>(64).unwrap();
        let error = NumericLoopFrames::new(&arena, program.body, locals, &pool, &pool)
            .err()
            .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_leases"));
        drop(held);
        assert_eq!(pool.lease::<u8>(8192).unwrap().capacity(), 8192);

        let mut frames =
            NumericLoopFrames::new(&arena, program.body, locals, &pool, &pool).unwrap();
        let mut outer = frames.take().unwrap();
        let inner = frames.take().unwrap();
        outer.locals[0] = locals[0];
        outer.values[0] = Datum::Text(arena.alloc_str("live outer value").unwrap());
        drop(frames);
        assert!(pool.lease::<u8>(8192).is_err());
        drop(inner);
        assert_eq!(outer.locals[0].name, locals[0].name);
        assert_eq!(outer.values[0], Datum::Text("live outer value"));
        drop(outer);
        assert_eq!(pool.lease::<u8>(8192).unwrap().capacity(), 8192);
    });
}

#[test]
fn nested_frames_reuse_backing_and_release_references_before_scratch_rewinds() {
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
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
        let mut frames = NumericLoopFrames::new(
            &arena,
            program.body,
            layout_locals(&program, &parse),
            &pool,
            &pool,
        )
        .unwrap();
        let mark = arena.mark();
        let mut prior_high_water = None;
        for _ in 0..256 {
            {
                let mut outer = frames.take().unwrap();
                outer.values[0] = Datum::Int4(41);
                let mut inner = frames.take().unwrap();
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
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
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
        // A counter is a new slot even when it shadows a variable.
        (
            "DECLARE i integer; BEGIN FOR i IN 1..2 LOOP FOR i IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[2, 3],
        ),
        (
            "DECLARE i integer; BEGIN FOR i IN 1..2 LOOP FOR j IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[2, 3],
        ),
        (
            "DECLARE i integer; BEGIN FOR j IN 1..2 LOOP FOR i IN 1..2 LOOP NULL; END LOOP; END LOOP; END",
            &[2, 3],
        ),
        // A nested block's declarations take one level for all of them.
        (
            "BEGIN DECLARE a int; b int; BEGIN FOR i IN 1..2 LOOP NULL; END LOOP; END; DECLARE c int; BEGIN NULL; END; END",
            &[2, 3],
        ),
    ] {
        parse.reset();
        arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            let program =
                parse_plpgsql_program(source, &parse, PlpgsqlProgramKind::Anonymous).unwrap();
            let mut frames = NumericLoopFrames::new(
                &arena,
                program.body,
                layout_locals(&program, &parse),
                &pool,
                &pool,
            )
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
    let mut control_budget = Budget::new(32 << 10);
    let pool = LeasePool::new(
        &mut control_budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        16 << 10,
        32,
    )
    .unwrap();
    let mut budget = Budget::new(1 << 17);
    let parse = Arena::new(&mut budget, "numeric reservation program", 1 << 16).unwrap();
    let arena = Arena::new(&mut budget, "numeric reservation bytes", 1).unwrap();
    let program = parse_plpgsql_program(
        "DECLARE x integer; y integer; BEGIN FOR i IN 1..2 LOOP NULL; END LOOP; END",
        &parse,
        PlpgsqlProgramKind::Anonymous,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let empty = NumericLoopFrames::new(
            &arena,
            TriggerBlock {
                statements: &[],
                sources: &[],
            },
            &[],
            &pool,
            &pool,
        )
        .unwrap();
        assert_eq!(empty.active, 0);
        assert_eq!(arena.used(), 0);
        drop(empty);
        let error = NumericLoopFrames::new(
            &arena,
            program.body,
            layout_locals(&program, &parse),
            &pool,
            &pool,
        )
        .err()
        .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(
            error.message.as_str(),
            "PL/pgSQL numeric loop frames exceed statement memory"
        );
    });
}
