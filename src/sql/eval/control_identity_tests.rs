use super::{InternalControl, SqlError, SqlState};
use crate::mem::arena::Arena;

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
            }
            .input_expression_failure();
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

struct InputCatalog(SqlState);

impl super::CatalogAccess for InputCatalog {
    fn type_schema_exists(&self, _: &str) -> bool {
        unreachable!("input validation does not inspect schemas")
    }
    fn relation_is_visible(&self, _: i32) -> Option<bool> {
        unreachable!("input validation does not inspect relation visibility")
    }
    fn type_is_visible(&self, _: i32) -> Option<bool> {
        unreachable!("input validation does not inspect type visibility")
    }
    fn function_is_visible(&self, _: i32) -> Option<bool> {
        unreachable!("input validation does not inspect function visibility")
    }
    fn collation_is_visible(&self, _: i32) -> Option<bool> {
        unreachable!("input validation does not inspect collation visibility")
    }
    fn relation_is_publishable(&self, _: i32) -> Option<bool> {
        unreachable!("input validation does not inspect publication membership")
    }
    fn index_def<'a>(
        &self,
        _: i32,
        _: i32,
        _: bool,
        _: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        unreachable!("input validation does not inspect indexes")
    }
    fn constraint_def<'a>(&self, _: i32, _: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        unreachable!("input validation does not inspect table constraints")
    }
    fn partition_key_def<'a>(&self, _: i32, _: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        unreachable!("input validation does not inspect partitions")
    }
    fn relname<'a>(&self, _: i32, _: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        unreachable!("input validation does not inspect relation names")
    }
    fn reloid(&self, _: &str) -> Result<Option<i32>, SqlError> {
        unreachable!("input validation does not inspect relation identifiers")
    }
    fn comment<'a>(
        &self,
        _: super::DescriptionTarget<'_>,
        _: u32,
        _: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        unreachable!("input validation does not inspect comments")
    }
    fn cast_user_type<'a>(
        &self,
        _: &str,
        _: super::Datum<'a>,
        _: &'a Arena,
        _: Option<&dyn super::SequenceAccess>,
    ) -> Result<Option<super::Datum<'a>>, SqlError> {
        Err(crate::sql_err!(self.0, "input conversion outcome"))
    }
}

#[test]
fn input_validation_propagates_controls_and_hard_errors_without_losing_provenance() {
    use crate::mem::budget::Budget;
    let mut budget = Budget::new(32768);
    let mut arena = Arena::new(&mut budget, "input failure ownership", 16384).unwrap();
    for control in [
        InternalControl::LockWait,
        InternalControl::IoWait,
        InternalControl::RoutineInvocation,
        InternalControl::QueryConsumerComplete,
    ] {
        arena.reset();
        crate::mem::guard::forbid_alloc(|| {
            let catalog = InputCatalog(SqlState::known(control));
            let hooks = super::EvalHooks {
                catalog: Some(&catalog),
                ..super::NO_HOOKS
            };
            let error = super::input_error("1", "application_type", &arena, &hooks)
                .err()
                .expect("engine control must escape input reporting");
            assert_eq!(error.sqlstate, control);
        });
    }
    for (code, reportable) in [
        ("22P02", true),
        ("22003", true),
        ("23502", true),
        ("23514", true),
        ("PZ001", false),
        ("57014", false),
        ("54000", false),
        ("58030", false),
        ("0A000", false),
    ] {
        for expression_error in [false, true] {
            arena.reset();
            crate::mem::guard::forbid_alloc(|| {
                let original = crate::sql_err!(code, "input error");
                let original = if expression_error {
                    original.input_expression_failure()
                } else {
                    original
                };
                let catalog = InputCatalog(SqlState::known(original.sqlstate));
                let hooks = super::EvalHooks {
                    catalog: Some(&catalog),
                    ..super::NO_HOOKS
                };
                match super::input_error("1", "application_type", &arena, &hooks) {
                    Ok(Some(error)) => {
                        assert!(reportable && !expression_error);
                        assert_eq!(error.sqlstate, SqlState::parse(code).unwrap());
                    }
                    Err(error) => {
                        assert!(!reportable || expression_error);
                        assert_eq!(error.sqlstate, SqlState::parse(code).unwrap());
                        assert_eq!(
                            error.sqlstate.can_report_input_failure(),
                            reportable && !expression_error
                        );
                    }
                    Ok(None) => panic!("fixture conversion must fail"),
                }
            });
        }
    }
}
