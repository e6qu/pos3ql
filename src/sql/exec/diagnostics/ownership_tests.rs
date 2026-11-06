use super::*;
use crate::mem::Budget;

#[test]
fn reused_diagnostics_reject_old_views_and_keep_copied_messages() {
    let mut budget = Budget::new(1 << 18);
    let arena = Arena::new(&mut budget, "caught diagnostics", 1 << 16).unwrap();
    let output = Arena::new(&mut budget, "saved error text", 1 << 12).unwrap();
    let mut detail = StackStr::from_str(&"x".repeat(8192));
    let error = sql_err!(sqlstate::RAISE_EXCEPTION, "first error");
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&arena, 1).unwrap();
        let first = buffers
            .capture(
                &arena,
                &error,
                Some(&super::super::super::eval::Diagnostic {
                    detail: Some(detail),
                    hint: None,
                }),
                "outer",
            )
            .unwrap();
        detail = StackStr::from_str("changed source");
        assert_eq!(
            first
                .stack::<8192>(&arena, Field::Detail)
                .unwrap()
                .unwrap()
                .as_str()
                .len(),
            8192
        );
        let saved = first.variables(&arena, &output).unwrap();
        assert_eq!(first.text(&arena, Field::Hint, &output).unwrap(), None);
        let used = arena.used();
        buffers.release(&arena, first);
        assert_eq!(
            first
                .text(&arena, Field::Message, &output)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        for _ in 0..64 {
            let next = buffers
                .capture(
                    &arena,
                    &sql_err!(sqlstate::DIVISION_BY_ZERO, "later"),
                    Some(&super::super::super::eval::Diagnostic {
                        detail: Some(detail),
                        hint: Some(StackStr::from_str("")),
                    }),
                    "inner",
                )
                .unwrap();
            assert_eq!(
                next.stack::<32>(&arena, Field::Detail)
                    .unwrap()
                    .unwrap()
                    .as_str(),
                "changed source"
            );
            assert_eq!(next.text(&arena, Field::Hint, &output).unwrap(), Some(""));
            assert_eq!(
                first
                    .text(&arena, Field::Message, &output)
                    .unwrap_err()
                    .sqlstate,
                "XX000"
            );
            buffers.release(&arena, next);
            assert_eq!(arena.used(), used);
        }
        assert_eq!(saved.sqlstate_text, "P0001");
        assert_eq!(saved.message, "first error");
    });
}

#[test]
fn nested_diagnostics_preserve_outer_errors_and_recover_after_capacity_failure() {
    let mut budget = Budget::new(1 << 16);
    let arena = Arena::new(&mut budget, "nested caught diagnostics", 2048).unwrap();
    let output = Arena::new(&mut budget, "copied nested diagnostic", 1024).unwrap();
    let too_large = super::super::super::eval::Diagnostic {
        detail: Some(StackStr::from_str(&"x".repeat(4096))),
        hint: None,
    };
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&arena, 2).unwrap();
        let outer = buffers
            .capture(
                &arena,
                &sql_err!(sqlstate::RAISE_EXCEPTION, "outer"),
                None,
                "outer context",
            )
            .unwrap();
        let error = buffers.capture(
            &arena,
            &sql_err!(sqlstate::RAISE_EXCEPTION, "large"),
            Some(&too_large),
            "nested",
        );
        assert_eq!(error.err().unwrap().sqlstate, "54000");
        assert_eq!(buffers.active, 1);
        let inner = buffers
            .capture(
                &arena,
                &sql_err!(sqlstate::DIVISION_BY_ZERO, "inner"),
                None,
                "nested",
            )
            .unwrap();
        assert_eq!(
            outer.text(&arena, Field::Message, &output).unwrap(),
            Some("outer")
        );
        assert_eq!(
            inner.text(&arena, Field::Message, &output).unwrap(),
            Some("inner")
        );
        buffers.release(&arena, inner);
        assert_eq!(
            inner
                .text(&arena, Field::Message, &output)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(
            outer.text(&arena, Field::Context, &output).unwrap(),
            Some("outer context")
        );
        buffers.release(&arena, outer);
        let reused = buffers
            .capture(
                &arena,
                &sql_err!(sqlstate::RAISE_EXCEPTION, "reused"),
                None,
                "later",
            )
            .unwrap();
        assert_eq!(
            reused.text(&arena, Field::Message, &output).unwrap(),
            Some("reused")
        );
        assert_eq!(
            outer
                .text(&arena, Field::Message, &output)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        buffers.release(&arena, reused);
    });
}

#[test]
fn diagnostic_owners_survive_arena_moves_and_front_recycling_but_reject_reset() {
    let mut budget = Budget::new(1 << 16);
    let arena = Arena::new(&mut budget, "retained handler state", 8192).unwrap();
    let output = Arena::new(&mut budget, "saved handler variables", 1024).unwrap();
    let small = Arena::new(&mut budget, "no handlers", 1).unwrap();
    crate::mem::guard::forbid_alloc(|| {
        let mut buffers = DiagnosticBuffers::new(&arena, 2).unwrap();
        let context = arena.alloc_str("original context").unwrap();
        let outer = buffers
            .capture(
                &arena,
                &sql_err!(sqlstate::RAISE_EXCEPTION, "outer"),
                None,
                context,
            )
            .unwrap();
        let inner = buffers
            .capture(
                &arena,
                &sql_err!(sqlstate::DIVISION_BY_ZERO, "inner"),
                None,
                "inner context",
            )
            .unwrap();
        let saved = outer.variables(&arena, &output).unwrap();
        let (mut moved, mut owner) = (arena, buffers);
        moved.recycle_front();
        moved.alloc_slice_with(1024, |_| 0xa5u8).unwrap();
        assert_eq!(
            outer.text(&moved, Field::Context, &output).unwrap(),
            Some("original context")
        );
        assert_eq!(
            inner
                .stack::<32>(&moved, Field::Message)
                .unwrap()
                .unwrap()
                .as_str(),
            "inner"
        );
        assert_eq!(
            outer
                .text(&output, Field::Message, &output)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        owner.release(&moved, inner);
        owner.release(&moved, outer);
        let next = owner
            .capture(
                &moved,
                &sql_err!(sqlstate::RAISE_EXCEPTION, "next"),
                None,
                "next context",
            )
            .unwrap();
        assert_eq!(
            outer
                .text(&moved, Field::Message, &output)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(
            next.text(&moved, Field::Message, &output).unwrap(),
            Some("next")
        );
        owner.release(&moved, next);
        moved.reset();
        let mut replacement = DiagnosticBuffers::new(&moved, 2).unwrap();
        let current = replacement
            .capture(
                &moved,
                &sql_err!(sqlstate::RAISE_EXCEPTION, "replacement"),
                None,
                "replacement context",
            )
            .unwrap();
        assert_eq!(
            next.text(&moved, Field::Message, &output)
                .unwrap_err()
                .sqlstate,
            "XX000"
        );
        assert_eq!(
            owner
                .capture(
                    &moved,
                    &sql_err!(sqlstate::RAISE_EXCEPTION, "stale"),
                    None,
                    "stale"
                )
                .err()
                .unwrap()
                .sqlstate,
            "XX000"
        );
        assert_eq!(
            current.text(&moved, Field::Message, &output).unwrap(),
            Some("replacement")
        );
        replacement.release(&moved, current);
        assert_eq!(saved.message, "outer");
        assert_eq!(saved.sqlstate_text, "P0001");
        let before = small.used();
        let _empty = DiagnosticBuffers::new(&small, 0).unwrap();
        assert_eq!(small.used(), before);
    });
}
