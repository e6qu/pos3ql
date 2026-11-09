use super::*;
use crate::mem::Budget;

#[test]
fn mixed_transition_tables_restore_all_operations_atomically_after_source_drop() {
    let mut budget = Budget::new(96 << 10);
    let source = Arena::new(&mut budget, "mixed transition source", 16 << 10).unwrap();
    let output = Arena::new(&mut budget, "mixed transition output", 512).unwrap();
    let small = Arena::new(&mut budget, "mixed transition failed admission", 64).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        32 << 10,
        4,
    )
    .unwrap();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let text = [b'x'; 8192];
        let old = encode_projected_pub(
            &[Datum::Text(core::str::from_utf8(&text).unwrap())],
            &source,
        )
        .unwrap();
        let inserted = encode_projected_pub(&[Datum::Int4(1)], &source).unwrap();
        let updated = encode_projected_pub(&[Datum::Int4(3)], &source).unwrap();
        let deleted = encode_projected_pub(&[Datum::Int4(4)], &source).unwrap();
        let rows = TransitionRowSets {
            inserted: Some(TransitionRows {
                old: &[],
                new: &[inserted],
            }),
            updated: Some(TransitionRows {
                old: &[old],
                new: &[updated],
            }),
            deleted: Some(TransitionRows {
                old: &[deleted],
                new: &[],
            }),
        };
        let held_a = pool.lease::<u8>(1).unwrap();
        let held_b = pool.lease::<u8>(1).unwrap();
        let error = SavedTransitionRowSets::capture(rows, &pool).err().unwrap();
        assert!(error.message.as_str().contains("row_work_leases"));
        drop((held_a, held_b));
        SavedTransitionRowSets::capture(rows, &pool).unwrap()
    });
    drop(source);
    let external = Arc::clone(&pool);
    drop(pool);
    crate::mem::guard::forbid_alloc(|| {
        let prefix = small.alloc_str("kept").unwrap();
        let before = small.used();
        for _ in 0..2 {
            assert_eq!(
                saved.restore(&small).err().unwrap().sqlstate,
                sqlstate::PROGRAM_LIMIT_EXCEEDED
            );
            assert_eq!(small.used(), before);
            assert_eq!(prefix, "kept");
        }
        small
            .with_remaining_region(|child| {
                let live = child.alloc_str("live child").unwrap();
                assert_eq!(
                    saved.restore(&small).err().unwrap().sqlstate,
                    sqlstate::PROGRAM_LIMIT_EXCEEDED
                );
                assert_eq!(live, "live child");
            })
            .unwrap();
        let restored = saved.restore(&output).unwrap();
        let inserted = restored.for_event(TriggerEvents::INSERT).unwrap();
        assert!(inserted.old.is_empty());
        assert_eq!(
            decode_projected_col_record(inserted.new[0], 0, &output).unwrap(),
            Datum::Int4(1)
        );
        let updated = restored.for_event(TriggerEvents::UPDATE).unwrap();
        let Datum::Text(text) = decode_projected_col_record(updated.old[0], 0, &output).unwrap()
        else {
            panic!("original UPDATE old value");
        };
        assert_eq!(text.len(), 8192);
        assert!(text.bytes().all(|byte| byte == b'x'));
        assert_eq!(
            decode_projected_col_record(updated.new[0], 0, &output).unwrap(),
            Datum::Int4(3)
        );
        let deleted = restored.for_event(TriggerEvents::DELETE).unwrap();
        assert!(deleted.new.is_empty());
        assert_eq!(
            decode_projected_col_record(deleted.old[0], 0, &output).unwrap(),
            Datum::Int4(4)
        );
        drop(saved);
        assert_eq!(external.lease::<u8>(32 << 10).unwrap().capacity(), 32 << 10);
        let empty = SavedTransitionRowSets::capture(
            TransitionRowSets {
                inserted: Some(TransitionRows { old: &[], new: &[] }),
                updated: None,
                deleted: None,
            },
            &external,
        )
        .unwrap();
        let restored = empty.restore(&output).unwrap();
        assert!(restored.inserted.is_some());
        assert!(restored.updated.is_none());
        assert!(restored.deleted.is_none());
    });
}

#[test]
fn published_transition_rows_own_payloads_after_collection_memory_is_recycled() {
    let mut budget = Budget::new(96 << 10);
    let mut inputs = Arena::new(&mut budget, "transition input values", 16 << 10).unwrap();
    let mut collection = Arena::new(&mut budget, "transition collection", 16 << 10).unwrap();
    let output = Arena::new(&mut budget, "transition directories", 512).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        32 << 10,
        4,
    )
    .unwrap();
    let saved = crate::mem::guard::forbid_alloc(|| {
        let text = inputs.alloc_slice_with(8192, |_| b'x').unwrap();
        let text = core::str::from_utf8(text).unwrap();
        let mut capture = TransitionCapture::new(&collection, 2, 1).unwrap();
        capture
            .push_old(&[Datum::Text(text), Datum::Null], &collection)
            .unwrap();
        capture.push_old(&[], &collection).unwrap();
        capture
            .push_new(&[Datum::Bytea(&[1, 2, 3]), Datum::Int4(73)], &collection)
            .unwrap();
        let error = capture.push_new(&[], &collection).unwrap_err();
        assert!(error.message.as_str().contains("NEW TABLE"));
        let error = capture.push_old(&[], &collection).unwrap_err();
        assert!(error.message.as_str().contains("OLD TABLE"));
        SavedTransitionRows::capture(capture.into_rows(), &pool).unwrap()
    });
    inputs.reset();
    collection.reset();
    crate::mem::guard::forbid_alloc(|| {
        inputs.alloc_slice_with(12 << 10, |_| 0xaa_u8).unwrap();
        collection.alloc_slice_with(12 << 10, |_| 0xbb_u8).unwrap();
        drop(inputs);
        drop(collection);
        drop(pool);
        let moved = saved.clone();
        drop(saved);
        let rows = moved.restore(&output).unwrap();
        assert_eq!(rows.old.len(), 2);
        assert_eq!(rows.new.len(), 1);
        let Datum::Text(text) = decode_projected_col_record(rows.old[0], 0, &output).unwrap()
        else {
            panic!("saved text");
        };
        assert_eq!(text.len(), 8192);
        assert!(text.bytes().all(|byte| byte == b'x'));
        assert!(
            decode_projected_col_record(rows.old[0], 1, &output)
                .unwrap()
                .is_null()
        );
        assert_eq!(projected_row_width(rows.old[1]), 0);
        assert_eq!(
            decode_projected_col_record(rows.new[0], 0, &output).unwrap(),
            Datum::Bytea(&[1, 2, 3])
        );
        assert_eq!(
            decode_projected_col_record(rows.new[0], 1, &output).unwrap(),
            Datum::Int4(73)
        );
        // Large payloads are borrowed from the owner, not recopied into scratch.
        assert!(output.used() < 512);
    });
}

#[test]
fn failed_transition_restore_keeps_scratch_and_both_images() {
    let mut budget = Budget::new(16 << 10);
    let source = Arena::new(&mut budget, "transition source", 1024).unwrap();
    let small = Arena::new(&mut budget, "partial transition restore", 48).unwrap();
    let output = Arena::new(&mut budget, "complete transition restore", 1024).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        1024,
        4,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let row = encode_projected_pub(&[Datum::Int4(41)], &source).unwrap();
        let saved = SavedTransitionRows::capture(
            TransitionRows {
                old: &[row, row],
                new: &[row, row, row],
            },
            &pool,
        )
        .unwrap();
        let prefix = small.alloc_str("kept").unwrap();
        let before = small.used();
        for _ in 0..2 {
            let error = saved.restore(&small).err().unwrap();
            assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
            assert_eq!(small.used(), before);
            assert_eq!(prefix, "kept");
        }
        small
            .with_remaining_region(|child| {
                let held = child.alloc_str("live child").unwrap();
                let error = saved.restore(&small).err().unwrap();
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert_eq!(held, "live child");
            })
            .unwrap();
        assert_eq!(small.used(), before);
        let rows = saved.restore(&output).unwrap();
        assert_eq!(rows.old.len(), 2);
        assert_eq!(rows.new.len(), 3);
        assert_eq!(rows.old[0], row);
        assert_eq!(rows.new[2], row);
    });
}

#[test]
fn transition_capture_exhaustion_keeps_published_rows_and_returns_shared_capacity() {
    let mut budget = Budget::new(16 << 10);
    let source = Arena::new(&mut budget, "transition capacity source", 1024).unwrap();
    let output = Arena::new(&mut budget, "transition capacity output", 1024).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::ROW_WORK,
        1024,
        2,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let row = encode_projected_pub(&[Datum::Int4(19)], &source).unwrap();
        let rows = TransitionRows {
            old: &[row],
            new: &[],
        };
        let saved = SavedTransitionRows::capture(rows, &pool).unwrap();
        let held = pool.lease::<u8>(8).unwrap();
        let error = SavedTransitionRows::capture(rows, &pool).err().unwrap();
        assert!(error.message.as_str().contains("row_work_leases"));
        drop(held);
        let oversized = [0_u8; 2048];
        let error = SavedTransitionRows::capture(
            TransitionRows {
                old: &[],
                new: &[&oversized],
            },
            &pool,
        )
        .err()
        .unwrap();
        assert!(error.message.as_str().contains("row_work_bytes"));
        let restored = saved.restore(&output).unwrap();
        assert_eq!(restored.old, &[row]);
        assert!(restored.new.is_empty());
        drop(saved);
        let saved = SavedTransitionRows::capture(
            TransitionRows {
                old: &[],
                new: &[row],
            },
            &pool,
        )
        .unwrap();
        let restored = saved.restore(&output).unwrap();
        assert!(restored.old.is_empty());
        assert_eq!(restored.new, &[row]);
        drop(saved);
        let saved =
            SavedTransitionRows::capture(TransitionRows { old: &[], new: &[] }, &pool).unwrap();
        let restored = saved.restore(&output).unwrap();
        assert!(restored.old.is_empty());
        assert!(restored.new.is_empty());
        drop(saved);
        assert_eq!(pool.lease::<u8>(1024).unwrap().capacity(), 1024);
    });
}
