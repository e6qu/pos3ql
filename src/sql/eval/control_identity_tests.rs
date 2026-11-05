use super::{InternalControl, SqlError, SqlState};

#[test]
fn application_sqlstates_never_construct_private_execution_controls() {
    crate::mem::guard::forbid_alloc(|| {
        for control in [
            InternalControl::LockWait,
            InternalControl::IoWait,
            InternalControl::RoutineInvocation,
            InternalControl::QueryConsumerComplete,
        ] {
            let code = control.diagnostic_code();
            let engine = SqlState::known(control);
            let application = SqlState::parse(code).unwrap();
            let ordinary_constant = SqlState::known(code);
            assert_eq!(engine.internal_control(), Some(control));
            assert_eq!(application.internal_control(), None);
            assert_eq!(ordinary_constant, application);
            assert_ne!(engine, application);
            assert_eq!(engine, control);
            assert_eq!(control, engine);
            assert_ne!(application, control);
            assert_ne!(control, application);
            assert_eq!(engine.as_str(), code);
            assert_eq!(application.as_str(), code);
        }
        assert!(
            SqlState::parse("PZ999")
                .unwrap()
                .internal_control()
                .is_none()
        );
    });
}

#[test]
fn copied_errors_preserve_private_provenance_but_wire_codes_remain_public() {
    crate::mem::guard::forbid_alloc(|| {
        for control in [
            InternalControl::LockWait,
            InternalControl::IoWait,
            InternalControl::RoutineInvocation,
            InternalControl::QueryConsumerComplete,
        ] {
            let original = crate::sql_err!(control, "unfinished operation");
            let forwarded = SqlError {
                sqlstate: SqlState::known(original.sqlstate),
                message: original.message,
            };
            assert_eq!(forwarded.sqlstate.internal_control(), Some(control));
            let remote = SqlError {
                sqlstate: SqlState::parse(forwarded.sqlstate.as_str()).unwrap(),
                message: forwarded.message,
            };
            assert_eq!(remote.sqlstate.internal_control(), None);
            assert_ne!(remote.sqlstate, control);
        }
    });
}
