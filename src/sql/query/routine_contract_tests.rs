use super::*;
use crate::mem::budget::Budget;
use crate::storage::{RoutineArgumentDef, RoutineDef, RoutineKind, RoutineResult};

#[test]
fn bounded_consumer_stops_before_another_row_effect_and_keeps_unowned_errors() {
    crate::mem::guard::forbid_alloc(|| {
        for limit in [1, 2] {
            let mut evaluated = 0;
            let mut emitted = 0;
            consume_query_rows(
                core::num::NonZeroU64::new(limit).unwrap(),
                |emit| {
                    for value in 0..4 {
                        evaluated += 1;
                        emit(&[Datum::Int4(value)])?;
                    }
                    Ok(())
                },
                &mut |_| {
                    emitted += 1;
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(evaluated, limit);
            assert_eq!(emitted, limit);
        }
        let error = consume_query_rows(
            core::num::NonZeroU64::MIN,
            |_| {
                Err(sql_err!(
                    sqlstate::INTERNAL_QUERY_CONSUMER_COMPLETE,
                    "unowned source error"
                ))
            },
            &mut |_| panic!("source failed before any row"),
        )
        .unwrap_err();
        assert_eq!(error.message.as_str(), "unowned source error");
    });
}

#[test]
fn pending_routine_contract_survives_front_rewind_and_rejects_a_reused_identity() {
    let mut budget = Budget::new(4096);
    let arena = Arena::new(&mut budget, "routine contract", 4096).unwrap();
    let mut routine = Box::new(RoutineDef::EMPTY);
    routine.created_at = 1;
    routine.argument_count = 1;
    routine.arguments[0] = RoutineArgumentDef {
        name: crate::storage::SqlName::parse("value").unwrap(),
        ctype: ColType::Int4,
        user_type: None,
    };
    routine.parameter_count = 1;
    routine.parameters[0].ctype = ColType::Int4;
    routine.kind = RoutineKind::Function {
        result: RoutineResult::builtin(ColType::Int4),
    };
    let mark = arena.mark();
    crate::mem::guard::forbid_alloc(|| {
        let contract = OwnedRoutineCallContract::capture(&routine, &arena).unwrap();
        arena.alloc_str("temporary parser state").unwrap();
        routine.arguments[0].ctype = ColType::Text;
        routine.parameters[0].ctype = ColType::Text;
        routine.kind = RoutineKind::Function {
            result: RoutineResult::TEXT,
        };
        unsafe { arena.rewind_to(mark) };
        contract.bind(&mut routine, &arena).unwrap();
        assert_eq!(routine.arguments[0].ctype, ColType::Int4);
        assert_eq!(routine.parameters[0].ctype, ColType::Int4);
        assert_eq!(routine.kind.function_result(), Some(ColType::Int4));
        routine.created_at = 2;
        assert_eq!(
            contract.bind(&mut routine, &arena).unwrap_err().sqlstate,
            "XX000"
        );
    });
}

#[test]
fn pending_routine_contract_reports_fixed_capacity_exhaustion() {
    let mut budget = Budget::new(1);
    let arena = Arena::new(&mut budget, "routine contract", 1).unwrap();
    let mut routine = Box::new(RoutineDef::EMPTY);
    routine.argument_count = 1;
    crate::mem::guard::forbid_alloc(|| {
        let error = match OwnedRoutineCallContract::capture(&routine, &arena) {
            Ok(_) => panic!("one byte cannot retain a routine argument"),
            Err(error) => error,
        };
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(
            error.message.as_str(),
            "pending routine type contract exceeds statement memory"
        );
    });
}

#[test]
fn pending_calls_own_nested_arguments_and_types_after_scratch_release_and_arena_move() {
    let mut budget = Budget::new(32 << 10);
    let original = Arena::new(&mut budget, "pending call owner", 16 << 10).unwrap();
    let mut output = Arena::new(&mut budget, "pending call output", 8 << 10).unwrap();
    let wrong = Arena::new(&mut budget, "other pending call", 8 << 10).unwrap();
    let mut routine = Box::new(RoutineDef::EMPTY);
    routine.created_at = 1;
    routine.argument_count = 1;
    routine.arguments[0].ctype = ColType::Record;
    routine.parameter_count = 1;
    routine.parameters[0].ctype = ColType::Record;
    routine.kind = RoutineKind::Function {
        result: RoutineResult::builtin(ColType::Int4),
    };
    let pending = crate::mem::guard::forbid_alloc(|| {
        original
            .with_remaining_region(|scratch| {
                let name = scratch.alloc_str("payload").unwrap();
                let text = scratch.alloc_str("saved nested value").unwrap();
                let fields = scratch
                    .alloc_slice_copy(&[super::super::types::RecordField {
                        name,
                        type_oid: super::super::types::oid::TEXT,
                        value: Datum::Text(text),
                    }])
                    .unwrap();
                PendingRoutineInvocation {
                    target: PendingRoutineTarget::Stored(
                        original
                            .retain(OwnedRoutineCallContract::capture(&routine, &original).unwrap())
                            .unwrap(),
                    ),
                    arguments: PendingRoutineInvocation::capture_arguments(
                        &[Datum::Record(fields)],
                        &original,
                    )
                    .unwrap(),
                    argument_count: 1,
                    source_position: Some(InvocationSite::Expression(17)),
                    input_collation: super::super::ast::Collation::None,
                    access: super::super::eval::RoutineCallAccess::Caller,
                    plan_owner: super::super::type_input_plans::Owner::Transient {
                        generation: 1,
                        statement: 0,
                    },
                }
            })
            .unwrap()
    });
    let mut moved = original;
    crate::mem::guard::forbid_alloc(|| {
        for _ in 0..8 {
            moved.recycle_front();
            output.reset();
            moved.alloc_str("replacement parsing scratch").unwrap();
            let encoded = pending.arguments(&moved, &output).unwrap();
            let Datum::Record(fields) =
                super::super::exec::decode_projected_col_record(encoded, 0, &output).unwrap()
            else {
                panic!("record argument");
            };
            assert_eq!(fields[0].name, "payload");
            assert_eq!(fields[0].type_oid, super::super::types::oid::TEXT);
            assert_eq!(fields[0].value, Datum::Text("saved nested value"));
            routine.arguments[0].ctype = ColType::Text;
            routine.parameters[0].ctype = ColType::Text;
            let PendingRoutineTarget::Stored(contract) = pending.target else {
                panic!("stored routine contract");
            };
            moved
                .retained(contract)
                .unwrap()
                .bind(&mut routine, &moved)
                .unwrap();
            assert_eq!(routine.arguments[0].ctype, ColType::Record);
            assert_eq!(routine.parameters[0].ctype, ColType::Record);
            assert_eq!(
                pending.source_position,
                Some(InvocationSite::Expression(17))
            );
        }
        assert_eq!(
            pending.arguments(&wrong, &output).unwrap_err().sqlstate,
            "XX000"
        );
        moved.reset();
        assert_eq!(
            pending.arguments(&moved, &output).unwrap_err().sqlstate,
            "XX000"
        );
        let PendingRoutineTarget::Stored(contract) = pending.target else {
            unreachable!();
        };
        assert!(moved.retained(contract).is_none());
    });
}

#[test]
fn pending_call_argument_exhaustion_and_borrow_failures_preserve_the_owner() {
    let mut budget = Budget::new(4097);
    let source = Arena::new(&mut budget, "pending arguments", 2048).unwrap();
    let mut output = Arena::new(&mut budget, "pending argument view", 2048).unwrap();
    let tiny = Arena::new(&mut budget, "tiny pending argument view", 1).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let pending = PendingRoutineInvocation {
            target: PendingRoutineTarget::Intrinsic(42),
            arguments: PendingRoutineInvocation::capture_arguments(
                &[Datum::Text("saved argument")],
                &source,
            )
            .unwrap(),
            argument_count: 1,
            source_position: None,
            input_collation: super::super::ast::Collation::None,
            access: super::super::eval::RoutineCallAccess::Caller,
            plan_owner: super::super::type_input_plans::Owner::Transient {
                generation: 1,
                statement: 0,
            },
        };
        assert_eq!(
            pending.arguments(&source, &tiny).unwrap_err().sqlstate,
            "54000"
        );
        let borrowed = source.retained_slice_mut(pending.arguments).unwrap();
        assert_eq!(
            pending.arguments(&source, &output).unwrap_err().sqlstate,
            "XX000"
        );
        drop(borrowed);
        output.reset();
        let encoded = pending.arguments(&source, &output).unwrap();
        assert_eq!(
            super::super::exec::decode_projected_col_record(encoded, 0, &output).unwrap(),
            Datum::Text("saved argument")
        );
        assert_eq!(
            PendingRoutineInvocation::capture_arguments(&[Datum::Text("x")], &tiny)
                .err()
                .unwrap()
                .sqlstate,
            "54000"
        );
    });
}

#[test]
fn completed_invocations_keep_values_after_the_body_source_is_released() {
    let mut budget = Budget::new(32 << 10);
    let statement = Arena::new(&mut budget, "completed invocation log", 16 << 10).unwrap();
    let mut work = Arena::new(&mut budget, "completed invocation body", 16 << 10).unwrap();
    let invocations = RoutineInvocationState::new(&statement);
    crate::mem::guard::forbid_alloc(|| {
        work.with_remaining_region(|body| {
            let name = body.alloc_str("body_value").unwrap();
            let text = body.alloc_str("only the saved result owns this").unwrap();
            let fields = body
                .alloc_slice_copy(&[super::super::types::RecordField {
                    name,
                    type_oid: super::super::types::oid::TEXT,
                    value: Datum::Text(text),
                }])
                .unwrap();
            invocations
                .complete(Datum::Record(fields), &statement)
                .unwrap();
        })
        .unwrap();
        for _ in 0..8 {
            work.reset();
            work.alloc_str("replacement body and parsing bytes")
                .unwrap();
            invocations.begin_attempt();
            let Datum::Record(fields) = invocations
                .resolve_intrinsic(42, &[], &statement, &work)
                .unwrap()
            else {
                panic!("retained routine record");
            };
            assert_eq!(fields[0].name, "body_value");
            assert_eq!(fields[0].type_oid, super::super::types::oid::TEXT);
            assert_eq!(
                fields[0].value,
                Datum::Text("only the saved result owns this")
            );
        }
    });
}
