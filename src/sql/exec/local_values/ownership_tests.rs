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
    let output = Arena::new(&mut budget, "saved value output", 16 << 10).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        4096,
        4,
    )
    .unwrap();
    let mut saved = SavedValue::new(&pool).unwrap();
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
        saved.replace(Datum::Record(fields)).unwrap();
    });
    source.reset();
    let mut moved = saved;
    crate::mem::guard::forbid_alloc(|| {
        source.alloc_slice_with(2048, |_| 0xaau8).unwrap();
        drop(source);
        drop(pool);
        let value = moved.read(&output).unwrap();
        moved.replace(Datum::Bytea(&[9; 2048])).unwrap();
        drop(moved);
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
    });
}

#[test]
fn saved_values_reuse_capacity_and_preserve_prior_values_after_failed_changes() {
    let mut budget = Budget::new(192 << 10);
    let source = Arena::new(&mut budget, "saved value input", 128 << 10).unwrap();
    let output = Arena::new(&mut budget, "saved value decoding", 8 << 10).unwrap();
    let tiny = Arena::new(&mut budget, "saved value read exhaustion", 1).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        4,
    )
    .unwrap();
    let small = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        128,
        2,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut saved = SavedValue::new(&pool).unwrap();
        assert_eq!(saved.read(&output).unwrap_err().sqlstate, "XX000");
        saved.replace(Datum::Bytea(&[0; 512])).unwrap();
        let capacity = saved.bytes.capacity();
        for turn in 0..64u8 {
            saved.replace(Datum::Bytea(&[turn; 512])).unwrap();
            output
                .with_remaining_region(|scratch| {
                    assert_eq!(saved.read(scratch).unwrap(), Datum::Bytea(&[turn; 512]));
                })
                .unwrap();
            saved.replace(Datum::Int4(i32::from(turn))).unwrap();
            output
                .with_remaining_region(|scratch| {
                    assert_eq!(saved.read(scratch).unwrap(), Datum::Int4(i32::from(turn)));
                })
                .unwrap();
            assert_eq!(saved.bytes.capacity(), capacity);
        }
        let mut kept = SavedValue::new(&small).unwrap();
        kept.replace(Datum::Text("kept")).unwrap();
        let capacity = kept.bytes.capacity();
        let large = source.alloc_slice_with(1024, |_| b'x').unwrap();
        let text = core::str::from_utf8(large).unwrap();
        let error = kept.replace(Datum::Text(text)).unwrap_err();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert_eq!(kept.bytes.capacity(), capacity);
        assert_eq!(kept.read(&output).unwrap(), Datum::Text("kept"));
        assert_eq!(kept.read(&tiny).unwrap_err().sqlstate, "54000");
        let invalid_name = source.alloc_slice_with(1 << 16, |_| b'x').unwrap();
        let fields = [RecordField {
            name: core::str::from_utf8(invalid_name).unwrap(),
            type_oid: 23,
            value: Datum::Int4(1),
        }];
        assert_eq!(
            kept.replace(Datum::Record(&fields)).unwrap_err().sqlstate,
            "54000"
        );
        assert_eq!(kept.bytes.capacity(), capacity);
        let previous = kept.read(&output).unwrap();
        source
            .with_remaining_region(|scratch| {
                kept.replace(Datum::Text(scratch.alloc_str("next").unwrap()))
                    .unwrap();
            })
            .unwrap();
        assert_eq!(kept.read(&output).unwrap(), Datum::Text("next"));
        kept.replace(Datum::Null).unwrap();
        assert_eq!(previous, Datum::Text("kept"));
        assert!(kept.read(&output).unwrap().is_null());
        let mut unpublished = SavedValue::new(&small).unwrap();
        assert_eq!(
            unpublished.replace(Datum::Text(text)).unwrap_err().sqlstate,
            "54000"
        );
        assert!(unpublished.length.is_none());
        assert_eq!(unpublished.read(&output).unwrap_err().sqlstate, "XX000");
        drop(kept);
        assert_eq!(small.lease::<u8>(128).unwrap().capacity(), 128);
    });
}

#[test]
fn saved_value_growth_moves_owned_bytes_and_failed_growth_keeps_published_values() {
    let mut budget = Budget::new(16 << 10);
    let output = Arena::new(&mut budget, "saved growth output", 8192).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        1024,
        4,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut saved = SavedValue::new(&pool).unwrap();
        saved.replace(Datum::Text("before move")).unwrap();
        let earlier = saved.read(&output).unwrap();
        let blocker = pool.lease::<u8>(64).unwrap();
        saved.replace(Datum::Bytea(&[7; 128])).unwrap();
        assert_eq!(saved.read(&output).unwrap(), Datum::Bytea(&[7; 128]));
        let after_move = saved.read(&output).unwrap();
        let later_blocker = pool.lease::<u8>(512).unwrap();
        let error = saved.replace(Datum::Bytea(&[8; 300])).unwrap_err();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert_eq!(saved.read(&output).unwrap(), Datum::Bytea(&[7; 128]));
        assert_eq!(earlier, Datum::Text("before move"));
        assert_eq!(after_move, Datum::Bytea(&[7; 128]));
        drop(later_blocker);
        drop(blocker);
        drop(saved);
        assert_eq!(pool.lease::<u8>(1024).unwrap().capacity(), 1024);
    });
}
