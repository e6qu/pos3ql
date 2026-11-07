use super::*;
use crate::mem::Budget;

#[test]
fn pending_errors_own_metadata_before_cleanup_and_release_it_after_handler_capture() {
    let mut budget = Budget::new(16 << 10);
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        8,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let _ = crate::sql::eval::take_diagnostic();
        crate::sql::eval::stash_diagnostic(
            Some(StackStr::from_str("original é detail")),
            Some(StackStr::from_str("")),
        );
        let error = PendingError::capture(sql_err!("PZ003", "original error"), &pool);
        assert!(crate::sql::eval::take_diagnostic().is_none());
        crate::sql::eval::stash_diagnostic(
            Some(StackStr::from_str("cleanup detail")),
            Some(StackStr::from_str("cleanup hint")),
        );
        let mut handlers = DiagnosticBuffers::new(&pool, 1).unwrap();
        error.catch(&mut handlers, "original call").unwrap();
        let caught = handlers.current().unwrap();
        assert_eq!(caught.sqlstate, "PZ003");
        assert!(caught.sqlstate.internal_control().is_none());
        assert_eq!(caught.field(Field::Message), Some("original error"));
        assert_eq!(caught.field(Field::Detail), Some("original é detail"));
        assert_eq!(caught.field(Field::Hint), Some(""));
        assert_eq!(caught.field(Field::Context), Some("original call"));
        drop(pool);
        let caught = handlers.current().unwrap();
        assert_eq!(caught.field(Field::Detail), Some("original é detail"));
        assert_eq!(caught.field(Field::Hint), Some(""));
        let _ = crate::sql::eval::take_diagnostic();
    });
}

#[test]
fn publishing_pending_errors_restores_their_metadata_and_clears_cleanup_metadata() {
    let mut budget = Budget::new(4096);
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        1024,
        4,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let _ = crate::sql::eval::take_diagnostic();
        crate::sql::eval::stash_diagnostic(
            Some(StackStr::from_str("saved detail")),
            Some(StackStr::from_str("saved hint")),
        );
        let error = PendingError::capture(sql_err!("22012", "original error"), &pool);
        crate::sql::eval::stash_diagnostic(Some(StackStr::from_str("other work")), None);
        let error = error.publish();
        assert_eq!(error.sqlstate, "22012");
        let diagnostic = crate::sql::eval::take_diagnostic().unwrap();
        assert_eq!(diagnostic.detail.unwrap().as_str(), "saved detail");
        assert_eq!(diagnostic.hint.unwrap().as_str(), "saved hint");
        assert_eq!(pool.lease::<u8>(1024).unwrap().capacity(), 1024);

        let error = PendingError::capture(sql_err!("22003", "without metadata"), &pool);
        crate::sql::eval::stash_diagnostic(Some(StackStr::from_str("stale detail")), None);
        assert_eq!(error.publish().sqlstate, "22003");
        assert!(crate::sql::eval::take_diagnostic().is_none());
        assert_eq!(pool.lease::<u8>(1024).unwrap().capacity(), 1024);
    });
}

#[test]
fn pending_error_capture_reports_exhaustion_and_returns_capacity_for_reuse() {
    let mut budget = Budget::new(4096);
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        1024,
        4,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let _ = crate::sql::eval::take_diagnostic();
        let held = pool.lease::<u8>(1024).unwrap();
        crate::sql::eval::stash_diagnostic(Some(StackStr::from_str("original detail")), None);
        let pending = PendingError::capture(sql_err!("22012", "original error"), &pool);
        assert_eq!(pending.error.sqlstate, "22012");
        let error = pending.publish();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert!(crate::sql::eval::take_diagnostic().is_none());
        drop(held);
        crate::sql::eval::stash_diagnostic(None, Some(StackStr::from_str("")));
        let error = PendingError::capture(sql_err!("22012", "retry"), &pool);
        let error = error.publish();
        assert_eq!(error.sqlstate, "22012");
        let diagnostic = crate::sql::eval::take_diagnostic().unwrap();
        assert!(diagnostic.detail.is_none());
        assert_eq!(diagnostic.hint.unwrap().as_str(), "");
        assert_eq!(pool.lease::<u8>(1024).unwrap().capacity(), 1024);
    });
}

#[test]
fn repeated_diagnostics_reuse_owned_capacity_and_keep_independent_copies() {
    let mut budget = Budget::new(1 << 18);
    let output = Arena::new(&mut budget, "saved error text", 4096).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        32768,
        4,
    )
    .unwrap();
    let mut detail = StackStr::from_str(&"x".repeat(8192));
    let error = sql_err!(sqlstate::RAISE_EXCEPTION, "first error");
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&pool, 1).unwrap();
        assert!(buffers.current().is_none());
        buffers
            .capture(
                &error,
                Some(&crate::sql::eval::Diagnostic {
                    detail: Some(detail),
                    hint: None,
                }),
                "outer",
            )
            .unwrap();
        detail = StackStr::from_str("changed source");
        let first = buffers.current().unwrap();
        assert_eq!(
            first
                .stack::<8192>(Field::Detail)
                .unwrap()
                .unwrap()
                .as_str()
                .len(),
            8192
        );
        assert_eq!(
            first.stack::<8>(Field::Detail).unwrap_err().sqlstate,
            "54000"
        );
        let saved = first.variables(&output).unwrap();
        assert_eq!(first.text(Field::Hint, &output).unwrap(), None);
        let capacity = buffers.slots[0].bytes.capacity();
        buffers.release();
        assert!(buffers.current().is_none());
        for _ in 0..64 {
            buffers
                .capture(
                    &sql_err!(sqlstate::DIVISION_BY_ZERO, "later"),
                    Some(&crate::sql::eval::Diagnostic {
                        detail: Some(detail),
                        hint: Some(StackStr::from_str("")),
                    }),
                    "inner",
                )
                .unwrap();
            let next = buffers.current().unwrap();
            assert_eq!(
                next.stack::<32>(Field::Detail).unwrap().unwrap().as_str(),
                "changed source"
            );
            assert_eq!(next.text(Field::Hint, &output).unwrap(), Some(""));
            assert_eq!(next.sqlstate, "22012");
            buffers.release();
            assert_eq!(buffers.slots[0].bytes.capacity(), capacity);
        }
        drop(buffers);
        assert_eq!(saved.sqlstate_text, "P0001");
        assert_eq!(saved.message, "first error");
        assert_eq!(pool.lease::<u8>(32768).unwrap().capacity(), 32768);
    });
}

#[test]
fn nested_diagnostic_failure_preserves_the_outer_handler_and_reuses_the_inner_slot() {
    let mut budget = Budget::new(1 << 16);
    let output = Arena::new(&mut budget, "copied nested diagnostic", 1024).unwrap();
    let tiny = Arena::new(&mut budget, "exhausted diagnostic output", 1).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        2048,
        8,
    )
    .unwrap();
    let too_large = crate::sql::eval::Diagnostic {
        detail: Some(StackStr::from_str(&"x".repeat(4096))),
        hint: None,
    };
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&pool, 2).unwrap();
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "outer"),
                None,
                "outer context",
            )
            .unwrap();
        let saved = buffers.current().unwrap().variables(&output).unwrap();
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .text(Field::Message, &tiny)
                .unwrap_err()
                .sqlstate,
            "54000"
        );
        let error = buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "large"),
                Some(&too_large),
                "nested",
            )
            .unwrap_err();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("execution_control_bytes"));
        assert_eq!(buffers.active, 1);
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .text(Field::Context, &output)
                .unwrap(),
            Some("outer context")
        );
        buffers
            .capture(
                &sql_err!(sqlstate::DIVISION_BY_ZERO, "inner"),
                None,
                "nested",
            )
            .unwrap();
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .text(Field::Message, &output)
                .unwrap(),
            Some("inner")
        );
        assert_eq!(
            buffers
                .capture(
                    &sql_err!(sqlstate::RAISE_EXCEPTION, "too deep"),
                    None,
                    "nested"
                )
                .unwrap_err()
                .sqlstate,
            "54000"
        );
        assert_eq!(buffers.active, 2);
        buffers.release();
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .text(Field::Message, &output)
                .unwrap(),
            Some("outer")
        );
        buffers.release();
        assert!(buffers.current().is_none());
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "reused"),
                None,
                "later",
            )
            .unwrap();
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .text(Field::Message, &output)
                .unwrap(),
            Some("reused")
        );
        buffers.release();
        assert_eq!(saved.message, "outer");
    });
}

#[test]
fn diagnostic_owners_survive_source_destruction_and_copies_survive_owner_drop() {
    let mut budget = Budget::new(1 << 16);
    let mut source = Arena::new(&mut budget, "handler context source", 8192).unwrap();
    let output = Arena::new(&mut budget, "saved handler variables", 1024).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        8192,
        8,
    )
    .unwrap();
    let mut buffers = DiagnosticBuffers::new(&pool, 2).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "outer"),
                None,
                source.alloc_str("original context").unwrap(),
            )
            .unwrap();
        buffers
            .capture(
                &sql_err!(sqlstate::DIVISION_BY_ZERO, "inner"),
                None,
                "inner context",
            )
            .unwrap();
    });
    source.reset();
    let mut owner = buffers;
    crate::mem::guard::forbid_alloc(|| {
        source.alloc_slice_with(1024, |_| 0xa5u8).unwrap();
        drop(source);
        drop(pool);
        let saved_inner = owner.current().unwrap().variables(&output).unwrap();
        owner.release();
        assert_eq!(
            owner
                .current()
                .unwrap()
                .text(Field::Context, &output)
                .unwrap(),
            Some("original context")
        );
        let saved_outer = owner.current().unwrap().variables(&output).unwrap();
        drop(owner);
        assert_eq!(saved_inner.message, "inner");
        assert_eq!(saved_inner.sqlstate_text, "22012");
        assert_eq!(saved_outer.message, "outer");
        assert_eq!(saved_outer.sqlstate_text, "P0001");
    });
}

#[test]
fn diagnostic_admission_and_descriptor_failure_return_all_ranges_after_owner_drop() {
    let mut budget = Budget::new(1 << 16);
    let output = Arena::new(&mut budget, "diagnostic failure copies", 1024).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        1024,
        2,
    )
    .unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let held = pool.lease::<u8>(960).unwrap();
        assert_eq!(
            DiagnosticBuffers::new(&pool, 2).err().unwrap().sqlstate,
            "54000"
        );
        drop(held);
        let one = pool.lease::<u8>(64).unwrap();
        let two = pool.lease::<u8>(64).unwrap();
        let error = DiagnosticBuffers::new(&pool, 1).err().unwrap();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("0 available leases"));
        let mut empty = DiagnosticBuffers::new(&pool, 0).unwrap();
        assert!(empty.current().is_none());
        assert_eq!(
            empty
                .capture(&sql_err!(sqlstate::RAISE_EXCEPTION, "no handler"), None, "")
                .unwrap_err()
                .sqlstate,
            "54000"
        );
        drop((one, two, empty));
        let mut buffers = DiagnosticBuffers::new(&pool, 2).unwrap();
        buffers
            .capture(&sql_err!(sqlstate::RAISE_EXCEPTION, "outer"), None, "outer")
            .unwrap();
        let error = buffers
            .capture(&sql_err!(sqlstate::RAISE_EXCEPTION, "inner"), None, "inner")
            .unwrap_err();
        assert_eq!(error.sqlstate, "54000");
        assert!(error.message.as_str().contains("0 available leases"));
        assert_eq!(buffers.active, 1);
        let saved = buffers.current().unwrap().variables(&output).unwrap();
        drop(buffers);
        assert_eq!(pool.lease::<u8>(1024).unwrap().capacity(), 1024);
        assert_eq!(saved.message, "outer");
    });
}

#[test]
fn diagnostic_growth_moves_bytes_and_recovers_after_fragmented_exhaustion() {
    let mut budget = Budget::new(1 << 16);
    let output = Arena::new(&mut budget, "diagnostic growth copies", 1024).unwrap();
    let pool = LeasePool::new(
        &mut budget,
        crate::mem::lease_pool::Purpose::EXECUTION_CONTROL,
        2048,
        8,
    )
    .unwrap();
    let medium = crate::sql::eval::Diagnostic {
        detail: Some(StackStr::from_str(&"m".repeat(300))),
        hint: None,
    };
    let large = crate::sql::eval::Diagnostic {
        detail: Some(StackStr::from_str(&"l".repeat(700))),
        hint: None,
    };
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&pool, 1).unwrap();
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "first"),
                None,
                "context",
            )
            .unwrap();
        let first = buffers.current().unwrap().variables(&output).unwrap();
        let blocker = pool.lease::<u8>(64).unwrap();
        buffers.release();
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "second"),
                Some(&medium),
                "context",
            )
            .unwrap();
        let second = buffers.current().unwrap().variables(&output).unwrap();
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .stack::<300>(Field::Detail)
                .unwrap()
                .unwrap()
                .as_str(),
            medium.detail.as_ref().unwrap().as_str()
        );
        let later_blocker = pool.lease::<u8>(1024).unwrap();
        buffers.release();
        assert_eq!(
            buffers
                .capture(
                    &sql_err!(sqlstate::RAISE_EXCEPTION, "large"),
                    Some(&large),
                    "context"
                )
                .unwrap_err()
                .sqlstate,
            "54000"
        );
        assert!(buffers.current().is_none());
        drop(later_blocker);
        buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "recovered"),
                None,
                "context",
            )
            .unwrap();
        assert_eq!(
            buffers
                .current()
                .unwrap()
                .stack::<32>(Field::Message)
                .unwrap()
                .unwrap()
                .as_str(),
            "recovered"
        );
        buffers.release();
        drop((blocker, buffers));
        assert_eq!(first.message, "first");
        assert_eq!(second.message, "second");
        assert_eq!(pool.lease::<u8>(2048).unwrap().capacity(), 2048);
    });
}
