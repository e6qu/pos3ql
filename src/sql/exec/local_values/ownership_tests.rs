use super::*;
use crate::mem::budget::Budget;

#[test]
fn local_slot_owners_preserve_values_on_failed_admission_and_return_capacity_on_drop() {
    let mut budget = Budget::new(32 << 10);
    let source = Arena::new(&mut budget, "owned local payloads", 16 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        2,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut values = allocate_plpgsql_local_values(&pool, 3).unwrap();
        values[0] = Datum::Text(source.alloc_str("selected value").unwrap());
        values[1] = Datum::Int4(73);
        assert!(values[2].is_null());
        let held = pool.lease::<u8>(64).unwrap();
        let error = allocate_plpgsql_local_values(&pool, 1).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_leases"));
        let moved = values;
        drop(held);
        assert_eq!(moved[0], Datum::Text("selected value"));
        assert_eq!(moved[1], Datum::Int4(73));
        assert!(pool.lease::<u8>(4096).is_err());
        drop(moved);
        assert_eq!(pool.lease::<u8>(4096).unwrap().capacity(), 4096);
    });
}

#[test]
fn saved_values_keep_typed_nested_data_after_sources_and_owners_are_recycled() {
    let mut budget = Budget::new(32 << 10);
    let mut source = Arena::new(&mut budget, "saved value source", 8 << 10).unwrap();
    let original = Arena::new(&mut budget, "saved value owner", 8 << 10).unwrap();
    let output = Arena::new(&mut budget, "saved value output", 16 << 10).unwrap();
    let mut saved = SavedValue::new();
    crate::mem::guard::forbid_alloc(|| {
        let text = source.alloc_str("independent text").unwrap();
        let array = crate::sql::array::build_shaped(
            &[Datum::Int4(7), Datum::Null],
            crate::sql::array::Shape::new(&[2], &[-2]).unwrap(),
            &source,
        )
        .unwrap();
        let fields = source
            .alloc_slice_copy(&[
                RecordField {
                    name: source.alloc_str("payload").unwrap(),
                    type_oid: 25,
                    value: Datum::Text(text),
                },
                RecordField {
                    name: source.alloc_str("elements").unwrap(),
                    type_oid: 1007,
                    value: Datum::Array {
                        element: ArrElem::Int4,
                        raw: array,
                    },
                },
                RecordField {
                    name: "missing",
                    type_oid: 23,
                    value: Datum::Null,
                },
            ])
            .unwrap();
        saved.replace(Datum::Record(fields), &original).unwrap();
    });
    source.reset();
    let mut moved = original;
    let value = crate::mem::guard::forbid_alloc(|| {
        moved.recycle_front();
        moved.alloc_slice_with(2048, |_| 0xffu8).unwrap();
        source.alloc_slice_with(2048, |_| 0xaau8).unwrap();
        saved.read(&moved, &output).unwrap()
    });
    moved.reset();
    crate::mem::guard::forbid_alloc(|| {
        let used = moved.used();
        assert_eq!(
            saved
                .replace(Datum::Bytea(&[0; 2048]), &moved)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(moved.used(), used);
        let Datum::Record(fields) = value else {
            panic!("saved record")
        };
        assert_eq!(fields[0].name, "payload");
        assert_eq!(fields[0].type_oid, 25);
        assert_eq!(fields[0].value, Datum::Text("independent text"));
        assert_eq!(fields[1].name, "elements");
        assert_eq!(fields[1].type_oid, 1007);
        let Datum::Array { element, raw } = fields[1].value else {
            panic!("saved array")
        };
        assert_eq!(element, ArrElem::Int4);
        let shape = crate::sql::array::shape(raw).unwrap();
        assert_eq!(shape.dimension(0), Some(2));
        assert_eq!(shape.lower_bound(0), Some(-2));
        assert_eq!(
            crate::sql::array::elements(raw, element).next(),
            Some(Datum::Int4(7))
        );
        assert_eq!(fields[2].type_oid, 23);
        assert!(fields[2].value.is_null());
        assert_eq!(saved.read(&moved, &output).unwrap_err().sqlstate, "XX000");
    });
}

#[test]
fn saved_values_reuse_capacity_and_preserve_prior_values_after_failed_changes() {
    let mut budget = Budget::new(192 << 10);
    let source = Arena::new(&mut budget, "saved value input", 128 << 10).unwrap();
    let arena = Arena::new(&mut budget, "saved value reuse", 8 << 10).unwrap();
    let small = Arena::new(&mut budget, "saved value exhaustion", 128).unwrap();
    let foreign = Arena::new(&mut budget, "foreign saved values", 128).unwrap();
    let output = Arena::new(&mut budget, "saved value decoding", 8 << 10).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut saved = SavedValue::new();
        saved.replace(Datum::Bytea(&[0; 512]), &arena).unwrap();
        let used = arena.used();
        for turn in 0..64u8 {
            saved.replace(Datum::Bytea(&[turn; 512]), &arena).unwrap();
            output
                .with_remaining_region(|scratch| {
                    assert_eq!(
                        saved.read(&arena, scratch).unwrap(),
                        Datum::Bytea(&[turn; 512])
                    );
                })
                .unwrap();
            saved.replace(Datum::Int4(i32::from(turn)), &arena).unwrap();
            output
                .with_remaining_region(|scratch| {
                    assert_eq!(
                        saved.read(&arena, scratch).unwrap(),
                        Datum::Int4(i32::from(turn))
                    );
                })
                .unwrap();
            assert_eq!(arena.used(), used);
        }
        let mut kept = SavedValue::new();
        kept.replace(Datum::Text("kept"), &small).unwrap();
        let before = small.used();
        let large = source.alloc_slice_with(1024, |_| b'x').unwrap();
        let text = core::str::from_utf8(large).unwrap();
        let foreign_used = foreign.used();
        assert_eq!(
            kept.replace(Datum::Text(text), &foreign)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(foreign.used(), foreign_used);
        let error = kept.replace(Datum::Text(text), &small).unwrap_err();
        assert_eq!(error.sqlstate, "54000");
        assert_eq!(
            error.message.as_str(),
            "PL/pgSQL saved values exceed statement memory"
        );
        assert_eq!(small.used(), before);
        assert_eq!(kept.read(&small, &output).unwrap(), Datum::Text("kept"));
        let invalid_name = source.alloc_slice_with(1 << 16, |_| b'x').unwrap();
        let fields = [RecordField {
            name: core::str::from_utf8(invalid_name).unwrap(),
            type_oid: 23,
            value: Datum::Int4(1),
        }];
        assert_eq!(
            kept.replace(Datum::Record(&fields), &small)
                .unwrap_err()
                .sqlstate,
            "54000"
        );
        assert_eq!(small.used(), before);
        let guard = small.retained_slice_mut(kept.bytes.unwrap()).unwrap();
        assert_eq!(
            kept.replace(Datum::Text(text), &small)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(small.used(), before);
        assert_eq!(
            kept.replace(Datum::Int4(4), &small).unwrap_err().sqlstate,
            "XX000"
        );
        drop(guard);
        assert_eq!(kept.read(&small, &output).unwrap(), Datum::Text("kept"));
        assert_eq!(kept.read(&foreign, &output).unwrap_err().sqlstate, "XX000");
        let previous = kept.read(&small, &output).unwrap();
        kept.replace(Datum::Null, &small).unwrap();
        assert_eq!(previous, Datum::Text("kept"));
        assert!(kept.read(&small, &output).unwrap().is_null());
        assert_eq!(small.used(), before);
    });
}
