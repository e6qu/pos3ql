use super::*;
use crate::mem::budget::Budget;
use crate::mem::lease_pool::Purpose;

#[test]
fn loop_values_keep_names_types_arrays_and_null_rows_after_source_reset() {
    let mut budget = Budget::new(1 << 18);
    let pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, 4096, 4).unwrap();
    let mut source = Arena::new(&mut budget, "loop value source", 1 << 16).unwrap();
    let output = Arena::new(&mut budget, "loop value output", 1 << 16).unwrap();
    let text = source.alloc_str("retained text").unwrap();
    let fields = source
        .alloc_slice_copy(&[RecordField {
            name: source.alloc_str("amount").unwrap(),
            type_oid: crate::sql::types::oid::INT4,
            type_mod: -1,
            value: Datum::Null,
        }])
        .unwrap();
    let raw = crate::sql::array::build_shaped(
        &[Datum::Int4(7), Datum::Null],
        crate::sql::array::Shape::new(&[2], &[-2]).unwrap(),
        &source,
    )
    .unwrap();
    let rows = crate::mem::guard::forbid_alloc(|| {
        LoopRows::capture(
            &pool,
            TriggerForRows {
                values: &[
                    Datum::Null,
                    Datum::Text(text),
                    Datum::Record(fields),
                    Datum::Array {
                        element: ArrElem::Int4,
                        raw,
                    },
                ],
                empty: Datum::Record(fields),
                width: 1,
            },
        )
        .unwrap()
    });
    source.reset();
    crate::mem::guard::forbid_alloc(|| {
        let retained_text = rows.value(1, &output).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows.value(0, &output).unwrap(), Datum::Null);
        assert_eq!(
            rows.value(1, &output).unwrap(),
            Datum::Text("retained text")
        );
        for value in [
            rows.empty_query_row(&output).unwrap(),
            rows.value(2, &output).unwrap(),
        ] {
            let Datum::Record(fields) = value else {
                panic!("typed record")
            };
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "amount");
            assert_eq!(fields[0].type_oid, 23);
            assert!(fields[0].value.is_null());
        }
        let Datum::Array { element, raw } = rows.value(3, &output).unwrap() else {
            panic!("array")
        };
        assert_eq!(element, ArrElem::Int4);
        let shape = crate::sql::array::shape(raw).unwrap();
        assert_eq!(shape.dimension(0), Some(2));
        assert_eq!(shape.lower_bound(0), Some(-2));
        let mut elements = crate::sql::array::elements(raw, element);
        assert_eq!(elements.next(), Some(Datum::Int4(7)));
        assert_eq!(elements.next(), Some(Datum::Null));
        assert_eq!(elements.next(), None);
        drop(rows);
        // Both the payload and its lease are returned when the loop ends.
        let mut reused = pool.lease::<u8>(4096).unwrap();
        assert_eq!(reused.capacity(), 4096);
        for _ in 0..4096 {
            reused.push_reserved(0x5a).unwrap();
        }
        assert_eq!(retained_text, Datum::Text("retained text"));
    });
}

#[test]
fn loop_value_capacity_errors_name_the_shared_pool_and_release_reservations() {
    let mut budget = Budget::new(1 << 16);
    let pool = LeasePool::new(&mut budget, Purpose::ROW_WORK, 64, 2).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let error = LoopRows::capture(
            &pool,
            TriggerForRows {
                values: &[Datum::Text(
                    "a value which cannot fit inside the sixty-four byte pool",
                )],
                empty: Datum::Null,
                width: 1,
            },
        )
        .err()
        .unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("row_work_bytes"));
        assert!(error.message.as_str().contains("row_work_leases"));
        assert_eq!(pool.lease::<u8>(64).unwrap().capacity(), 64);
    });
}
