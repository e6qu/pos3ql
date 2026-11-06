//! Completed calls own encoded values and replay receipts, without arena borrows.

use super::*;
use crate::mem::arena::ArenaSliceHandle;

#[derive(Clone, Copy)]
pub(super) struct ResultBytes(ArenaSliceHandle<u8>);

impl ResultBytes {
    pub(super) fn capture_scalar(value: Datum<'_>, arena: &Arena) -> Result<Self, SqlError> {
        let length = super::super::exec::projected_row_len(&[value])?;
        let handle = arena
            .retain_slice_with(length, |_| 0u8)
            .map_err(|_| full())?;
        let mut bytes = arena.retained_slice_mut(handle).map_err(|_| inactive())?;
        super::super::exec::encode_projected_into(&[value], &mut bytes)?;
        Ok(Self(handle))
    }

    pub(super) fn capture_rows(rows: &[&[u8]], arena: &Arena) -> Result<Self, SqlError> {
        let length = super::super::exec::projected_rows_len(rows)?;
        let handle = arena
            .retain_slice_with(length, |_| 0u8)
            .map_err(|_| full())?;
        let mut bytes = arena.retained_slice_mut(handle).map_err(|_| inactive())?;
        super::super::exec::pack_projected_rows_into(rows, &mut bytes)?;
        Ok(Self(handle))
    }

    fn copy<'out>(self, source: &Arena, output: &'out Arena) -> Result<&'out [u8], SqlError> {
        let bytes = source.retained_slice_mut(self.0).map_err(|_| inactive())?;
        output
            .alloc_slice_copy(&bytes)
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
        source: &Arena,
        output: &'out Arena,
    ) -> Result<&'out [&'out [u8]], SqlError> {
        let bytes = self.copy(source, output)?;
        super::super::exec::unpack_projected_rows(bytes, output)
    }
}

/// Scalar and table-source results keep distinct contracts even after encoding.
#[derive(Clone, Copy)]
pub(super) enum RoutineInvocationResult {
    Scalar(ResultBytes, super::super::sequence::StatementReplayCursor),
    Rows(ResultBytes, super::super::sequence::StatementReplayCursor),
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
                            RoutineInvocationResult::Scalar(
                                ResultBytes::capture_scalar(Datum::Record(fields), &original)
                                    .unwrap(),
                                super::super::super::sequence::StatementReplayCursor::EMPTY,
                            ),
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
                                RoutineInvocationResult::Rows(
                                    ResultBytes::capture_rows(rows, &original).unwrap(),
                                    super::super::super::sequence::StatementReplayCursor::EMPTY,
                                ),
                            )
                            .unwrap();
                    }
                    // Force directory growth, retaining random-access ordinals used
                    // when an input continuation skips completed nested calls.
                    for number in 0..12 {
                        results
                            .push(
                                &original,
                                RoutineInvocationResult::Scalar(
                                    ResultBytes::capture_scalar(Datum::Int4(number), &original)
                                        .unwrap(),
                                    super::super::super::sequence::StatementReplayCursor::EMPTY,
                                ),
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
                match result_at(&results, &moved, index).unwrap() {
                    RoutineInvocationResult::Scalar(value, _) => {
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
                    RoutineInvocationResult::Rows(value, _) => {
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
            let value = RoutineInvocationResult::Scalar(
                ResultBytes::capture_scalar(Datum::Int4(42), &arena).unwrap(),
                super::super::super::sequence::StatementReplayCursor::EMPTY,
            );
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
                    RoutineInvocationResult::Scalar(_, _)
                ));
            }
        });
    }
}
