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
            |_| Err(sql_err!("PZ004", "unowned source error")),
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
        let contract = RoutineCallContract::capture(&routine, &arena).unwrap();
        arena.alloc_str("temporary parser state").unwrap();
        routine.arguments[0].ctype = ColType::Text;
        routine.parameters[0].ctype = ColType::Text;
        routine.kind = RoutineKind::Function {
            result: RoutineResult::TEXT,
        };
        unsafe { arena.rewind_to(mark) };
        contract.bind(&mut routine).unwrap();
        assert_eq!(routine.arguments[0].ctype, ColType::Int4);
        assert_eq!(routine.parameters[0].ctype, ColType::Int4);
        assert_eq!(routine.kind.function_result(), Some(ColType::Int4));
        routine.created_at = 2;
        assert_eq!(contract.bind(&mut routine).unwrap_err().sqlstate, "XX000");
    });
}

#[test]
fn pending_routine_contract_reports_fixed_capacity_exhaustion() {
    let mut budget = Budget::new(1);
    let arena = Arena::new(&mut budget, "routine contract", 1).unwrap();
    let mut routine = Box::new(RoutineDef::EMPTY);
    routine.argument_count = 1;
    crate::mem::guard::forbid_alloc(|| {
        let error = match RoutineCallContract::capture(&routine, &arena) {
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
