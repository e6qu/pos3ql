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
                .stack::<8192>(Field::Detail)
                .unwrap()
                .unwrap()
                .as_str()
                .len(),
            8192
        );
        let saved = first.variables(&output).unwrap();
        assert_eq!(first.text(Field::Hint, &output).unwrap(), None);
        let used = arena.used();
        buffers.release(first);
        assert_eq!(
            first.text(Field::Message, &output).unwrap_err().sqlstate,
            "XX000"
        );
        for _ in 0..64 {
            let next = buffers
                .capture(
                    &sql_err!(sqlstate::DIVISION_BY_ZERO, "later"),
                    Some(&super::super::super::eval::Diagnostic {
                        detail: Some(detail),
                        hint: Some(StackStr::from_str("")),
                    }),
                    "inner",
                )
                .unwrap();
            assert_eq!(
                next.stack::<32>(Field::Detail).unwrap().unwrap().as_str(),
                "changed source"
            );
            assert_eq!(next.text(Field::Hint, &output).unwrap(), Some(""));
            assert_eq!(
                first.text(Field::Message, &output).unwrap_err().sqlstate,
                "XX000"
            );
            buffers.release(next);
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
                &sql_err!(sqlstate::RAISE_EXCEPTION, "outer"),
                None,
                "outer context",
            )
            .unwrap();
        let error = buffers.capture(
            &sql_err!(sqlstate::RAISE_EXCEPTION, "large"),
            Some(&too_large),
            "nested",
        );
        assert_eq!(error.err().unwrap().sqlstate, "54000");
        assert_eq!(buffers.active, 1);
        let inner = buffers
            .capture(
                &sql_err!(sqlstate::DIVISION_BY_ZERO, "inner"),
                None,
                "nested",
            )
            .unwrap();
        assert_eq!(outer.text(Field::Message, &output).unwrap(), Some("outer"));
        assert_eq!(inner.text(Field::Message, &output).unwrap(), Some("inner"));
        buffers.release(inner);
        assert_eq!(
            inner.text(Field::Message, &output).unwrap_err().sqlstate,
            "XX000"
        );
        assert_eq!(
            outer.text(Field::Context, &output).unwrap(),
            Some("outer context")
        );
        buffers.release(outer);
        let reused = buffers
            .capture(
                &sql_err!(sqlstate::RAISE_EXCEPTION, "reused"),
                None,
                "later",
            )
            .unwrap();
        assert_eq!(
            reused.text(Field::Message, &output).unwrap(),
            Some("reused")
        );
        assert_eq!(
            outer.text(Field::Message, &output).unwrap_err().sqlstate,
            "XX000"
        );
        buffers.release(reused);
    });
}
