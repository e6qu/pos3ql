//! Completed calls own encoded values and replay receipts, without arena borrows.

use super::*;
use crate::mem::arena::FrozenArenaSliceHandle;

#[derive(Clone, Copy)]
pub(crate) struct ResultBytes(FrozenArenaSliceHandle<u8>);

impl ResultBytes {
    #[cfg(test)]
    pub(super) fn capture_scalar(value: Datum<'_>, arena: &Arena) -> Result<Self, SqlError> {
        let length = super::super::exec::projected_row_len(&[value])?;
        let handle = arena
            .retain_slice_with(length, |_| 0u8)
            .map_err(|_| full())?;
        let mut bytes = arena.retained_slice_mut(handle).map_err(|_| inactive())?;
        super::super::exec::encode_projected_into(&[value], &mut bytes)?;
        drop(bytes);
        Ok(Self(
            arena
                .freeze_retained_slice(handle)
                .map_err(|_| inactive())?,
        ))
    }

    #[cfg(test)]
    pub(super) fn capture_rows(rows: &[&[u8]], arena: &Arena) -> Result<Self, SqlError> {
        let length = super::super::exec::projected_rows_len(rows)?;
        let handle = arena
            .retain_slice_with(length, |_| 0u8)
            .map_err(|_| full())?;
        let mut bytes = arena.retained_slice_mut(handle).map_err(|_| inactive())?;
        super::super::exec::pack_projected_rows_into(rows, &mut bytes)?;
        drop(bytes);
        Ok(Self(
            arena
                .freeze_retained_slice(handle)
                .map_err(|_| inactive())?,
        ))
    }

    pub(super) fn copy<'out>(
        self,
        source: &Arena,
        output: &'out Arena,
    ) -> Result<&'out [u8], SqlError> {
        let bytes = source.retained_slice(self.0).map_err(|_| inactive())?;
        output
            .alloc_slice_copy(bytes)
            .map(|bytes| &*bytes)
            .map_err(|_| full())
    }

    pub(super) fn scalar<'out>(
        self,
        source: &Arena,
        output: &'out Arena,
    ) -> Result<Datum<'out>, SqlError> {
        let bytes = self.copy(source, output)?;
        super::super::exec::decode_projected_col_record(bytes, 0, output)
    }

    pub(super) fn rows<'out>(
        self,
        source: &'out Arena,
        output: &'out Arena,
    ) -> Result<&'out [&'out [u8]], SqlError> {
        let bytes = source.retained_slice(self.0).map_err(|_| inactive())?;
        super::super::exec::unpack_projected_rows(bytes, output)
    }

    pub(super) fn copied_rows<'out>(
        self,
        source: &Arena,
        output: &'out Arena,
    ) -> Result<&'out [&'out [u8]], SqlError> {
        let bytes = self.copy(source, output)?;
        super::super::exec::unpack_projected_rows(bytes, output)
    }
}

/// Completed calls preserve result shape independently of the stored bytes.
#[derive(Clone, Copy)]
pub(crate) enum OwnedRoutineResult {
    Scalar(ResultBytes),
    Rows(ResultBytes),
    /// A row trigger run on the engine: the firing it answers and its
    /// result row (whether the row is kept, then the returned `NEW`).
    Trigger(super::TriggerCall, ResultBytes),
}

impl OwnedRoutineResult {
    pub(super) fn relocate(self, source: &Arena, target: &Arena) -> Result<Self, SqlError> {
        let (Self::Scalar(bytes) | Self::Rows(bytes) | Self::Trigger(_, bytes)) = self;
        let bytes = source.retained_slice(bytes.0).map_err(|_| inactive())?;
        let copied = target
            .retain_slice_with(bytes.len(), |index| bytes[index])
            .map_err(|_| full())?;
        let copied = ResultBytes(
            target
                .freeze_retained_slice(copied)
                .map_err(|_| inactive())?,
        );
        Ok(match self {
            Self::Scalar(_) => Self::Scalar(copied),
            Self::Rows(_) => Self::Rows(copied),
            Self::Trigger(call, _) => Self::Trigger(call, copied),
        })
    }

    pub(crate) fn from_scalar_image(image: FrozenArenaSliceHandle<u8>) -> Self {
        Self::Scalar(ResultBytes(image))
    }

    pub(crate) fn from_row_image(image: FrozenArenaSliceHandle<u8>) -> Self {
        Self::Rows(ResultBytes(image))
    }

    #[cfg(test)]
    pub(crate) fn capture_scalar(value: Datum<'_>, arena: &Arena) -> Result<Self, SqlError> {
        ResultBytes::capture_scalar(value, arena).map(Self::Scalar)
    }

    pub(crate) fn from_trigger_image(
        call: super::TriggerCall,
        image: FrozenArenaSliceHandle<u8>,
    ) -> Self {
        Self::Trigger(call, ResultBytes(image))
    }

    pub(super) fn validate(self, arena: &Arena) -> Result<(), SqlError> {
        let (Self::Scalar(bytes) | Self::Rows(bytes) | Self::Trigger(_, bytes)) = self;
        arena
            .retained_slice(bytes.0)
            .map(|_| ())
            .map_err(|_| inactive())
    }
}

#[derive(Clone, Copy)]
pub(super) struct RoutineInvocationResult {
    pub(super) value: OwnedRoutineResult,
    pub(super) replay: super::super::sequence::StatementReplayCursor,
}

pub(super) type RoutineResults = crate::mem::arena::RetainedList<RoutineInvocationResult>;

pub(super) fn directory_error(error: crate::mem::arena::RetainedListError) -> SqlError {
    match error {
        crate::mem::arena::RetainedListError::Capacity(_) => full(),
        crate::mem::arena::RetainedListError::Storage(_) => inactive(),
    }
}

pub(super) fn result_at(
    results: &RoutineResults,
    arena: &Arena,
    index: usize,
) -> Result<RoutineInvocationResult, SqlError> {
    results
        .get(arena, index)
        .map_err(|_| inactive())?
        .ok_or_else(inactive)
}

fn full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "mutable routine replay exceeds statement memory"
    )
}

fn inactive() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "completed routine result storage is inactive"
    )
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::mem::Budget;

    #[test]
    fn callback_result_copies_remain_readable_after_both_source_owners_are_dropped() {
        let mut budget = Budget::new(16 << 10);
        let source = Arena::new(&mut budget, "callback source", 4096).unwrap();
        let owner = Arena::new(&mut budget, "callback result owner", 4096).unwrap();
        let output = Arena::new(&mut budget, "independent callback result", 8192).unwrap();
        let invocations = RoutineInvocationState::new(
            &owner,
            crate::sql::query::RoutineExecution::StatementReplay,
        );
        let (scalar, rows) = crate::mem::guard::forbid_alloc(|| {
            let value = Datum::Text(source.alloc_str("independent text").unwrap());
            let scalar = ResultBytes::capture_scalar(value, &owner).unwrap();
            let encoded =
                super::super::super::exec::encode_projected_pub(&[value], &source).unwrap();
            let rows = ResultBytes::capture_rows(&[encoded], &owner).unwrap();
            let copied = with_routine_invocation_scope(
                Some(RoutineInvocationContext::new(&invocations, &owner)),
                || {
                    with_active_routine_invocations(|context| {
                        let (calls, arena) = context.unwrap();
                        calls.require_arena(arena).unwrap();
                        (
                            scalar.scalar(arena, &output).unwrap(),
                            rows.copied_rows(arena, &output).unwrap(),
                        )
                    })
                },
            );
            drop(source);
            drop(owner);
            copied
        });
        crate::mem::guard::forbid_alloc(|| {
            assert_eq!(scalar, Datum::Text("independent text"));
            assert_eq!(rows.len(), 1);
            assert_eq!(
                super::super::super::exec::decode_projected_col_record(rows[0], 0, &output)
                    .unwrap(),
                scalar
            );
            with_active_routine_invocations(|context| assert!(context.is_none()));
        });
    }

    #[test]
    fn table_result_views_reserve_only_the_row_directory_and_share_frozen_payload() {
        let mut budget = Budget::new(64 << 10);
        let source = Arena::new(&mut budget, "table result source", 4096).unwrap();
        let row = super::super::super::exec::encode_projected_pub(
            &[Datum::Text("saved table payload")],
            &source,
        )
        .unwrap();
        let rows = [row; 64];
        let packed = super::super::super::exec::projected_rows_len(&rows).unwrap();
        let directory = rows.len() * core::mem::size_of::<&[u8]>();
        let capacity = Arena::retained_slice_capacity::<u8>(packed).unwrap() + directory;
        let owner = Arena::new(&mut budget, "one retained table payload", capacity).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let value = ResultBytes::capture_rows(&rows, &owner).unwrap();
            let retained = owner.used();
            let bytes = owner.retained_slice(value.0).unwrap();
            let restored = value.rows(&owner, &owner).unwrap();
            assert_eq!(owner.used() - retained, directory);
            assert_eq!(restored.len(), 64);
            assert!(restored[0].as_ptr() >= bytes.as_ptr());
            assert!(restored[63].as_ptr() < bytes.as_ptr().wrapping_add(bytes.len()));
            assert_eq!(
                super::super::super::exec::decode_projected_col_record(restored[63], 0, &owner)
                    .unwrap(),
                Datum::Text("saved table payload")
            );
        });
    }

    #[test]
    fn completed_results_own_nested_values_and_rows_after_source_release_and_arena_move() {
        let mut budget = Budget::new(64 << 10);
        let original = Arena::new(&mut budget, "completed calls", 32 << 10).unwrap();
        let mut output = Arena::new(&mut budget, "completed call view", 16 << 10).unwrap();
        let wrong = Arena::new(&mut budget, "other completed calls", 16 << 10).unwrap();
        let mut results = RoutineResults::new();
        crate::mem::guard::forbid_alloc(|| {
            original
                .with_remaining_region(|scratch| {
                    let field = scratch.alloc_str("saved field").unwrap();
                    let text = scratch.alloc_str("saved text").unwrap();
                    let fields = scratch
                        .alloc_slice_copy(&[super::super::super::types::RecordField {
                            name: field,
                            type_oid: super::super::super::types::oid::TEXT,
                            value: Datum::Text(text),
                        }])
                        .unwrap();
                    results
                        .push(
                            &original,
                            RoutineInvocationResult {
                                value: OwnedRoutineResult::Scalar(
                                    ResultBytes::capture_scalar(Datum::Record(fields), &original)
                                        .unwrap(),
                                ),
                                replay: super::super::super::sequence::StatementReplayCursor::EMPTY,
                            },
                        )
                        .unwrap();
                    let first = super::super::super::exec::encode_projected_pub(
                        &[Datum::Record(fields)],
                        scratch,
                    )
                    .unwrap();
                    let second =
                        super::super::super::exec::encode_projected_pub(&[Datum::Null], scratch)
                            .unwrap();
                    for rows in [&[first, second][..], &[][..]] {
                        results
                            .push(
                                &original,
                                RoutineInvocationResult {
                                    value: OwnedRoutineResult::Rows(
                                        ResultBytes::capture_rows(rows, &original).unwrap(),
                                    ),
                                    replay:
                                        super::super::super::sequence::StatementReplayCursor::EMPTY,
                                },
                            )
                            .unwrap();
                    }
                    // Force directory growth, retaining random-access ordinals used
                    // when an input continuation skips completed nested calls.
                    for number in 0..12 {
                        results
                            .push(
                                &original,
                                RoutineInvocationResult {
                                    value: OwnedRoutineResult::Scalar(
                                        ResultBytes::capture_scalar(Datum::Int4(number), &original)
                                            .unwrap(),
                                    ),
                                    replay:
                                        super::super::super::sequence::StatementReplayCursor::EMPTY,
                                },
                            )
                            .unwrap();
                    }
                })
                .unwrap();
        });
        let mut moved = original;
        crate::mem::guard::forbid_alloc(|| {
            moved.recycle_front();
            for index in [14, 0, 9, 2, 1, 3] {
                output.reset();
                match result_at(&results, &moved, index).unwrap().value {
                    OwnedRoutineResult::Scalar(value) => {
                        let value = value.scalar(&moved, &output).unwrap();
                        if index == 0 {
                            let Datum::Record(fields) = value else {
                                panic!("saved record")
                            };
                            assert_eq!(fields[0].name, "saved field");
                            assert_eq!(fields[0].value, Datum::Text("saved text"));
                        } else {
                            assert_eq!(value, Datum::Int4(index as i32 - 3));
                        }
                    }
                    OwnedRoutineResult::Rows(value) => {
                        let rows = value.rows(&moved, &output).unwrap();
                        assert_eq!(rows.len(), if index == 1 { 2 } else { 0 });
                        if index == 1 {
                            let Datum::Record(fields) =
                                super::super::super::exec::decode_projected_col_record(
                                    rows[0], 0, &output,
                                )
                                .unwrap()
                            else {
                                panic!("saved row record")
                            };
                            assert_eq!(fields[0].type_oid, super::super::super::types::oid::TEXT);
                            assert_eq!(fields[0].value, Datum::Text("saved text"));
                            assert!(
                                super::super::super::exec::decode_projected_col_record(
                                    rows[1], 0, &output
                                )
                                .unwrap()
                                .is_null()
                            );
                        }
                    }
                    OwnedRoutineResult::Trigger(..) => panic!("no trigger result was saved"),
                }
            }
            assert!(results.get(&wrong, 0).is_err());
            assert!(result_at(&results, &moved, 15).is_err());
            moved.reset();
            assert!(results.get(&moved, 0).is_err());
        });
    }

    #[test]
    fn failed_result_directory_growth_preserves_every_published_entry() {
        let capacity = Arena::retained_slice_capacity::<RoutineInvocationResult>(4).unwrap()
            + Arena::retained_slice_capacity::<u8>(
                super::super::super::exec::projected_row_len(&[Datum::Int4(42)]).unwrap(),
            )
            .unwrap();
        let mut budget = Budget::new(capacity);
        let arena = Arena::new(&mut budget, "completed result capacity", capacity).unwrap();
        let mut results = RoutineResults::new();
        crate::mem::guard::forbid_alloc(|| {
            let value = RoutineInvocationResult {
                value: OwnedRoutineResult::Scalar(
                    ResultBytes::capture_scalar(Datum::Int4(42), &arena).unwrap(),
                ),
                replay: super::super::super::sequence::StatementReplayCursor::EMPTY,
            };
            while results.push(&arena, value).is_ok() {}
            let before = results.len();
            assert_eq!(before, 4);
            assert_eq!(
                directory_error(results.push(&arena, value).unwrap_err()).sqlstate,
                "54000"
            );
            assert_eq!(results.len(), before);
            for index in 0..before {
                assert!(matches!(
                    result_at(&results, &arena, index).unwrap(),
                    RoutineInvocationResult {
                        value: OwnedRoutineResult::Scalar(_),
                        ..
                    }
                ));
            }
        });
    }
}
