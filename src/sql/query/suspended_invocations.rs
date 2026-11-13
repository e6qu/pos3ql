//! A real wait transfers typed invocation receipts out of temporary query memory.

use super::*;

impl OwnedRoutineCallContract {
    fn relocate(self, source: &Arena, target: &Arena) -> Result<Self, SqlError> {
        let copy = |handle| {
            let entries = source
                .retained_slice_mut(handle)
                .map_err(|_| inactive_pending_call())?;
            target
                .retain_slice_with(entries.len(), |index| entries[index])
                .map_err(|_| arena_full())
        };
        Ok(Self {
            oid: self.oid,
            arguments: copy(self.arguments)?,
            parameters: copy(self.parameters)?,
            result: self.result,
            result_columns: copy(self.result_columns)?,
        })
    }
}

impl PendingRoutineInvocation {
    fn relocate(self, source: &Arena, target: &Arena) -> Result<Self, SqlError> {
        let arguments = self.arguments(source)?;
        let arguments = target
            .retain_slice_with(arguments.len(), |index| arguments[index])
            .map_err(|_| arena_full())?;
        let arguments = target
            .freeze_retained_slice(arguments)
            .map_err(|_| inactive_pending_call())?;
        let selected = match self.target {
            PendingRoutineTarget::Stored(handle) => {
                let contract = source.retained(handle).ok_or_else(inactive_pending_call)?;
                let contract = contract.relocate(source, target)?;
                PendingRoutineTarget::Stored(target.retain(contract).map_err(|_| arena_full())?)
            }
            PendingRoutineTarget::Intrinsic(oid) => PendingRoutineTarget::Intrinsic(oid),
            PendingRoutineTarget::WriteQuery(write) => PendingRoutineTarget::WriteQuery(write),
            PendingRoutineTarget::Trigger(call) => PendingRoutineTarget::Trigger(call),
        };
        Ok(Self {
            arguments,
            target: selected,
            ..self
        })
    }
}

impl RoutineInvocationState {
    /// Source receipts remain usable if admission fails. Only actual values and
    /// selected definitions move; no arena identity or borrowed syntax is copied.
    #[expect(
        clippy::result_large_err,
        reason = "failed relocation returns the original invocation owner"
    )]
    pub(crate) fn relocate(self, source: &Arena, target: &Arena) -> Result<Self, (Self, SqlError)> {
        let copied = self.copy_receipts(source, target);
        match copied {
            Ok(mut output) => {
                output.suspended = RefCell::new(self.suspended.into_inner());
                Ok(output)
            }
            Err(error) => Err((self, error)),
        }
    }

    fn copy_receipts(&self, source: &Arena, target: &Arena) -> Result<Self, SqlError> {
        self.require_arena(source)?;
        let output = Self::new(target, self.execution);
        output.unowned_values.set(self.unowned_values.get());
        output.next.set(self.next.get());
        output.pending.set(
            self.pending
                .get()
                .map(|pending| pending.relocate(source, target))
                .transpose()?,
        );
        output.pending_replay.set(self.pending_replay.get());
        output.pending_firing.set(self.pending_firing.get());
        let results = self.results.borrow();
        for index in 0..results.len() {
            let saved = invocation_results::result_at(&results, source, index)?;
            let value = saved.value.relocate(source, target)?;
            output
                .results
                .borrow_mut()
                .push(
                    target,
                    RoutineInvocationResult {
                        value,
                        replay: saved.replay,
                    },
                )
                .map_err(invocation_results::directory_error)?;
        }
        let writes = self.writes.borrow();
        for index in 0..writes.len() {
            let write = writes
                .get(source, index)
                .map_err(|_| inactive_pending_call())?
                .ok_or_else(inactive_pending_call)?;
            output
                .writes
                .borrow_mut()
                .push(
                    target,
                    WriteQueryResult {
                        rows: write.rows.relocate(source, target)?,
                        ..write
                    },
                )
                .map_err(invocation_results::directory_error)?;
        }
        output.reads_writes.set(self.reads_writes.get());
        output.writes_root.set(self.writes_root.get());
        Ok(Self {
            id: self.id,
            inputs: self.inputs.relocate(source, target)?,
            ..output
        })
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::mem::Budget;
    use crate::sql::type_input_plans::{Owner, with_owner};

    #[test]
    fn suspended_invocations_relocate_values_and_pending_arguments_before_region_teardown() {
        let mut budget = Budget::new(128 << 10);
        let owner = Arena::new(&mut budget, "suspended call owner", 128 << 10).unwrap();
        let mut routine = Box::new(crate::storage::RoutineDef::EMPTY);
        routine.created_at = 1;
        routine.argument_count = 1;
        routine.arguments[0] = crate::storage::RoutineArgumentDef {
            name: crate::storage::SqlName::parse("input").unwrap(),
            ctype: ColType::Text,
            user_type: None,
        };
        routine.parameter_count = 1;
        routine.parameters[0].ctype = ColType::Text;
        routine.kind = crate::storage::RoutineKind::Function {
            result: crate::storage::RoutineResult::TEXT,
        };
        let calls = crate::mem::guard::forbid_alloc(|| {
            owner
                .with_remaining_region(|scratch| {
                    let calls = RoutineInvocationState::new(
                        scratch,
                        crate::sql::query::RoutineExecution::Retained,
                    );
                    let text = scratch.alloc_str("completed value").unwrap();
                    let fields = [
                        super::super::super::types::RecordField {
                            name: "nullable_domain",
                            type_oid: 123456,
                            type_mod: -1,
                            value: Datum::Null,
                        },
                        super::super::super::types::RecordField {
                            name: "payload",
                            type_oid: super::super::super::types::oid::TEXT,
                            type_mod: -1,
                            value: Datum::Text(text),
                        },
                    ];
                    let result =
                        OwnedRoutineResult::capture_scalar(Datum::Record(&fields), scratch)
                            .unwrap();
                    calls.complete(result, scratch, scratch).unwrap();
                    let row = crate::sql::exec::encode_projected_pub(&[Datum::Int4(73)], scratch)
                        .unwrap();
                    let rows =
                        invocation_results::ResultBytes::capture_rows(&[row; 8], scratch).unwrap();
                    calls
                        .complete(OwnedRoutineResult::Rows(rows), scratch, scratch)
                        .unwrap();
                    calls.next.set(2);
                    let error = with_owner(Owner::Backend, || {
                        calls.resolve(
                            &routine,
                            &[Datum::Text(text)],
                            scratch,
                            scratch,
                            None,
                            crate::sql::ast::Collation::Default,
                            crate::sql::eval::RoutineCallAccess::Caller,
                        )
                    })
                    .unwrap_err();
                    assert_eq!(
                        error.sqlstate,
                        super::super::super::eval::sqlstate::INTERNAL_ROUTINE_INVOCATION
                    );
                    calls
                        .relocate(scratch, &owner)
                        .unwrap_or_else(|(_, error)| {
                            panic!("invocation relocation failed: {error:?}")
                        })
                })
                .unwrap()
        });
        let mut moved = owner;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            moved.alloc_slice_with(4096, |_| 0xccu8).unwrap();
            calls.require_arena(&moved).unwrap();
            moved
                .with_remaining_region(|scratch| {
                    let first =
                        invocation_results::result_at(&calls.results.borrow(), &moved, 0).unwrap();
                    let OwnedRoutineResult::Scalar(first) = first.value else {
                        panic!("scalar shape changed")
                    };
                    let Datum::Record(fields) = first.scalar(&moved, scratch).unwrap() else {
                        panic!("record shape changed")
                    };
                    assert_eq!(fields[0].name, "nullable_domain");
                    assert_eq!(fields[0].type_oid, 123456);
                    assert!(fields[0].value.is_null());
                    assert_eq!(fields[1].value, Datum::Text("completed value"));
                    let second =
                        invocation_results::result_at(&calls.results.borrow(), &moved, 1).unwrap();
                    let OwnedRoutineResult::Rows(rows) = second.value else {
                        panic!("row shape changed")
                    };
                    let rows = rows.copied_rows(&moved, scratch).unwrap();
                    assert_eq!(rows.len(), 8);
                    assert_eq!(
                        crate::sql::exec::decode_projected_pub(rows[7], 0),
                        Datum::Int4(73)
                    );
                    let pending = calls.take_pending().unwrap();
                    assert_eq!(pending.plan_owner, Owner::Backend);
                    let PendingRoutineTarget::Stored(contract) = pending.target else {
                        panic!("stored call selection changed")
                    };
                    routine.arguments[0].ctype = ColType::Int4;
                    routine.parameters[0].ctype = ColType::Int4;
                    moved
                        .retained(contract)
                        .unwrap()
                        .bind(&mut routine, &moved)
                        .unwrap();
                    assert_eq!(routine.arguments[0].ctype, ColType::Text);
                    assert_eq!(routine.parameters[0].ctype, ColType::Text);
                    let arguments = pending.arguments(&moved).unwrap();
                    assert_eq!(
                        crate::sql::exec::decode_projected_col_record(arguments, 0, scratch)
                            .unwrap(),
                        Datum::Text("completed value")
                    );
                })
                .unwrap();
            moved.reset();
            assert!(calls.require_arena(&moved).is_err());
        });
    }

    #[test]
    fn suspended_invocations_failed_admission_preserves_the_original_receipts() {
        let mut budget = Budget::new(32 << 10);
        let source = Arena::new(&mut budget, "original receipts", 8192).unwrap();
        let short = Arena::new(&mut budget, "short receipt target", 16).unwrap();
        let target = Arena::new(&mut budget, "admitted receipts", 8192).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let calls =
                RoutineInvocationState::new(&source, crate::sql::query::RoutineExecution::Retained);
            calls
                .complete(
                    OwnedRoutineResult::capture_scalar(Datum::Text("kept receipt"), &source)
                        .unwrap(),
                    &source,
                    &source,
                )
                .unwrap();
            let (calls, _) = calls.relocate(&source, &short).err().unwrap();
            let (calls, _) = calls.relocate(&target, &source).err().unwrap();
            assert_eq!(calls.results.borrow().len(), 1);
            let moved = calls
                .relocate(&source, &target)
                .unwrap_or_else(|(_, error)| panic!("invocation relocation failed: {error:?}"));
            assert_eq!(moved.results.borrow().len(), 1);
            {
                let calls = &moved;
                let arena = &target;
                let saved =
                    invocation_results::result_at(&calls.results.borrow(), arena, 0).unwrap();
                let OwnedRoutineResult::Scalar(value) = saved.value else {
                    panic!("shape changed")
                };
                assert_eq!(
                    value.scalar(arena, arena).unwrap(),
                    Datum::Text("kept receipt")
                );
            }
        });
    }
}
