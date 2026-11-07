use super::*;
use crate::mem::budget::Budget;
use crate::mem::lease_pool::Purpose;

#[test]
fn saved_rows_keep_prefix_after_errors_and_overwrite_unpublished_bytes() {
    let mut budget = Budget::new(1 << 18);
    let pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, 4096, 8).unwrap();
    let scratch = Arena::new(&mut budget, "set row conversion", 1 << 16).unwrap();
    let output = Arena::new(&mut budget, "set row output", 1 << 16).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut result = PlpgsqlSetResult {
            contract: Contract::Scalar(crate::storage::RoutineResult {
                ctype: ColType::Int4,
                user_type: None,
            }),
            bytes: pool.lease(0).unwrap(),
            ends: pool.lease(0).unwrap(),
        };
        result.append(&[Datum::Int4(1)], &scratch).unwrap();
        assert_eq!(scratch.used(), 0);
        let error = result
            .append(&[Datum::Text("not an integer")], &scratch)
            .unwrap_err();
        assert_eq!(error.sqlstate, "22P02");
        assert_eq!(scratch.used(), 0);
        // A cancelled writer can have initialized, but not published, bytes.
        result.bytes.extend_from_slice(&[0xde; 32]).unwrap();
        result.append(&[Datum::Int4(9)], &scratch).unwrap();
        let rows = result.rows(&output).unwrap();
        assert_eq!(rows.len(), 2);
        drop(result);
        let mut reused = pool.lease::<u8>(4096).unwrap();
        for _ in 0..4096 {
            reused.push_reserved(0x5a).unwrap();
        }
        assert_eq!(
            decode_projected_col_record(rows[0], 0, &output).unwrap(),
            Datum::Int4(1)
        );
        assert_eq!(
            decode_projected_col_record(rows[1], 0, &output).unwrap(),
            Datum::Int4(9)
        );
    });
}

#[test]
fn set_result_exhaustion_preserves_rows_and_admits_a_later_small_append() {
    let mut budget = Budget::new(1 << 18);
    let pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, 128, 4).unwrap();
    let scratch = Arena::new(&mut budget, "set capacity conversion", 1 << 16).unwrap();
    let output = Arena::new(&mut budget, "set capacity output", 1 << 16).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut result = PlpgsqlSetResult {
            contract: Contract::Scalar(crate::storage::RoutineResult::TEXT),
            bytes: pool.lease(0).unwrap(),
            ends: pool.lease(0).unwrap(),
        };
        result.append(&[Datum::Text("first")], &scratch).unwrap();
        let error = result.append(&[Datum::Text(
            "this intentionally oversized value cannot coexist with its existing prefix inside the configured fixed shared row-work pool",
        )], &scratch).unwrap_err();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("row_work_bytes"));
        assert_eq!(scratch.used(), 0);
        result.append(&[Datum::Text("last")], &scratch).unwrap();
        let rows = result.rows(&output).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            decode_projected_col_record(rows[0], 0, &output).unwrap(),
            Datum::Text("first")
        );
        assert_eq!(
            decode_projected_col_record(rows[1], 0, &output).unwrap(),
            Datum::Text("last")
        );
        drop(result);
        assert_eq!(pool.lease::<u8>(128).unwrap().capacity(), 128);
    });
}

#[test]
fn output_contract_owns_only_actual_columns_and_survives_definition_reuse() {
    let mut budget = Budget::new(1 << 18);
    let scratch = Arena::new(&mut budget, "set contract conversion", 1 << 16).unwrap();
    let output = Arena::new(&mut budget, "set contract output", 1 << 16).unwrap();
    let mut routine = crate::storage::RoutineDef::EMPTY;
    routine.kind = crate::storage::RoutineKind::TableFunction;
    routine.result_column_count = 2;
    routine.result_columns[0] = RoutineArgumentDef {
        name: SqlName::parse("number").unwrap(),
        ctype: ColType::Int4,
        user_type: None,
    };
    routine.result_columns[1] = RoutineArgumentDef {
        name: SqlName::parse("label").unwrap(),
        ctype: ColType::Text,
        user_type: None,
    };
    // Metadata and the two nonempty result buffers each occupy whole pool
    // alignment units. Charge actual metadata width on every target.
    let pool_bytes =
        core::mem::size_of_val(&routine.result_columns[..2]).next_multiple_of(64) + 2 * 64;
    assert!(pool_bytes < core::mem::size_of::<crate::storage::RoutineDef>());
    let pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, pool_bytes, 4).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut result = PlpgsqlSetResult::for_routine(&routine, &pool).unwrap();
        routine.result_columns.fill(RoutineArgumentDef::EMPTY);
        let Contract::Columns(columns) = &result.contract else {
            panic!("table function contract")
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name.as_str(), "number");
        assert_eq!(columns[1].name.as_str(), "label");
        result
            .append(&[Datum::Text("7"), Datum::Text("retained")], &scratch)
            .unwrap();
        assert_eq!(scratch.used(), 0);
        let rows = result.rows(&output).unwrap();
        drop(result);
        assert_eq!(pool.lease::<u8>(pool_bytes).unwrap().capacity(), pool_bytes);
        assert_eq!(
            decode_projected_col_record(rows[0], 0, &output).unwrap(),
            Datum::Int4(7)
        );
        assert_eq!(
            decode_projected_col_record(rows[0], 1, &output).unwrap(),
            Datum::Text("retained")
        );
    });
}
