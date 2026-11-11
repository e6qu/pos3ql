//! A write's row phase kept across the routine calls it makes.
//!
//! A routine call that needs the engine ends the write attempt. Before the
//! first row change the caller rolls the attempt back and runs it again.
//! After that, it keeps the rows already changed: it rolls back only the
//! unfinished row, runs the call, and starts another attempt of the same
//! command. That attempt selects its rows again under the original command
//! snapshot, skips the changed rows without evaluating them, and continues at
//! the unfinished row. The call therefore sees the earlier rows' changes, as
//! in PostgreSQL, and every row, trigger and `RETURNING` row happens once.

use super::*;
use crate::sql::query::{self, InvocationPosition};
use crate::sql::sequence::{self, StatementReplayStart};
use crate::sql::txn::{StatementMark, SuspendedTriggerSql};

/// Where an attempt's replay of routine results and sequence values stands.
#[derive(Clone, Copy)]
struct ReplayPoint {
    calls: Option<InvocationPosition>,
    replay: Option<StatementReplayStart>,
}

impl ReplayPoint {
    fn capture() -> Result<Self, SqlError> {
        Ok(Self {
            calls: query::active_invocation_position(),
            replay: sequence::begin_replay_cursor()?,
        })
    }

    fn seek(self) -> Result<(), SqlError> {
        query::seek_active_invocations(self.calls)?;
        sequence::rewind_replay_cursor(self.replay)
    }
}

/// Where an unfinished row stopped: before its change, or after it while
/// its `RETURNING` values were evaluated.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowStage {
    Change,
    Returning,
}

#[derive(Clone, Copy)]
struct RowStart {
    index: usize,
    stage: RowStage,
    row: Option<PhysicalRow>,
    changed: u64,
    transitions: CaptureLengths,
    mark: StatementMark,
    output: crate::pg::overflow::OutputMark,
    point: ReplayPoint,
}

struct RowResume {
    start: RowStart,
    selected: usize,
    transitions: [Option<transition_rows::SavedTransitionRows>; 2],
    /// The changed row's old and new values, for its `RETURNING` stage.
    returning: Option<transition_rows::SavedTransitionRows>,
}

/// Rows each of a write's transition captures (its own operation, and the
/// update of `INSERT ... ON CONFLICT`) held when a row started.
pub(super) type CaptureLengths = [(usize, usize); 2];

pub(super) fn capture_lengths(captures: [Option<&TransitionCapture<'_>>; 2]) -> CaptureLengths {
    captures.map(|capture| capture.map_or((0, 0), TransitionCapture::lengths))
}

/// Where a resumed attempt continues.
pub(super) struct ResumedRows<'p> {
    pub(super) first: usize,
    pub(super) changed: u64,
    pub(super) transitions: [Option<&'p transition_rows::SavedTransitionRows>; 2],
}

/// A changed row's values for its resumed `RETURNING` stage.
pub(super) struct SavedReturning<'a> {
    pub(super) old: Option<&'a [Datum<'a>]>,
    pub(super) new: Option<&'a [Datum<'a>]>,
}

/// The unfinished row and how to roll it back, handed to the caller.
pub(crate) struct UnfinishedRow {
    pub(crate) mark: StatementMark,
    pub(crate) output: crate::pg::overflow::OutputMark,
}

pub(crate) struct RowProgress {
    journal: Option<(usize, u64)>,
    selection: Option<ReplayPoint>,
    running: Option<RowStart>,
    resume: Option<RowResume>,
    command: Option<SuspendedTriggerSql>,
    returning: Option<transition_rows::SavedTransitionRows>,
    started: bool,
    unrestartable: bool,
}

impl RowProgress {
    pub(crate) const fn new() -> Self {
        Self {
            journal: None,
            selection: None,
            running: None,
            resume: None,
            command: None,
            returning: None,
            started: false,
            unrestartable: false,
        }
    }

    /// Records the journal and lock positions of the attempt about to run;
    /// row rollback marks combine them with the transaction's own positions.
    pub(crate) fn begin_attempt(&mut self, wal: usize, lock: u64) {
        self.journal = Some((wal, lock));
        self.running = None;
    }

    /// Whether this attempt continues rows an earlier attempt changed.
    pub(super) fn resuming(&self) -> bool {
        self.resume.is_some()
    }

    /// The suspended command of a nested write, kept with its rows.
    pub(super) fn take_command(&mut self) -> Option<SuspendedTriggerSql> {
        self.command.take()
    }

    pub(super) fn keep_command(&mut self, command: SuspendedTriggerSql) {
        self.command = Some(command);
    }

    /// Called where the selection starts, after statement-level triggers.
    /// A resumed attempt replays from the same position as the first one.
    pub(super) fn enter_selection(&mut self) -> Result<(), SqlError> {
        match self.selection {
            Some(point) if self.resume.is_some() => point.seek(),
            _ => {
                self.selection = Some(ReplayPoint::capture()?);
                Ok(())
            }
        }
    }

    /// After the rows are selected again: checks that the selection is the
    /// one already partly changed and returns the first row to change, the
    /// count of changed rows and their saved transition rows.
    /// `row_at` names a selected row's stored identity, when it has one.
    pub(super) fn resume_rows<'p>(
        &'p self,
        selected: usize,
        row_at: impl FnOnce(usize) -> Option<PhysicalRow>,
    ) -> Result<ResumedRows<'p>, SqlError> {
        let Some(resume) = &self.resume else {
            return Ok(ResumedRows {
                first: 0,
                changed: 0,
                transitions: [None, None],
            });
        };
        if resume.selected != selected
            || resume.start.index >= selected
            || !same_row(row_at(resume.start.index), resume.start.row)
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a resumed write selected different rows"
            ));
        }
        resume.start.point.seek()?;
        Ok(ResumedRows {
            first: resume.start.index,
            changed: resume.start.changed,
            transitions: [
                resume.transitions[0].as_ref(),
                resume.transitions[1].as_ref(),
            ],
        })
    }

    /// Called before a row's first effect.
    pub(super) fn begin_row(
        &mut self,
        index: usize,
        row: Option<PhysicalRow>,
        changed: u64,
        transitions: CaptureLengths,
        txn: &TxnState,
        responder: &Responder<'_>,
    ) -> Result<(), SqlError> {
        self.begin_stage(
            RowStage::Change,
            index,
            row,
            changed,
            transitions,
            txn,
            responder,
        )
    }

    /// Called after a row's change, before its `RETURNING` values: a call
    /// there sees the change, which the caller keeps.
    pub(super) fn begin_returning(
        &mut self,
        index: usize,
        row: Option<PhysicalRow>,
        changed: u64,
        transitions: CaptureLengths,
        txn: &TxnState,
        responder: &Responder<'_>,
    ) -> Result<(), SqlError> {
        self.begin_stage(
            RowStage::Returning,
            index,
            row,
            changed,
            transitions,
            txn,
            responder,
        )
    }

    /// Saves the changed row's values when its `RETURNING` stage stops for
    /// a routine call.
    pub(super) fn keep_returning(
        &mut self,
        old: Option<&[Datum<'_>]>,
        new: Option<&[Datum<'_>]>,
        arena: &Arena,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        let mark = arena.mark();
        let saved = (|| {
            let old = old
                .map(|old| encode_projected_pub(old, arena))
                .transpose()?;
            let new = new
                .map(|new| encode_projected_pub(new, arena))
                .transpose()?;
            transition_rows::SavedTransitionRows::capture(
                TransitionRows {
                    old: old.as_slice(),
                    new: new.as_slice(),
                },
                pool,
            )
        })();
        // SAFETY: the encoded rows were copied into the saved owner.
        unsafe { arena.rewind_to(mark) };
        self.returning = Some(saved?);
        Ok(())
    }

    /// For the row a resumed attempt continues at its `RETURNING` stage:
    /// the saved old and new values, encoded in `arena`.
    pub(super) fn take_returning<'out>(
        &mut self,
        index: usize,
        arena: &'out Arena,
    ) -> Result<Option<SavedReturning<'out>>, SqlError> {
        let Some(resume) = &mut self.resume else {
            return Ok(None);
        };
        if resume.start.index != index || resume.start.stage != RowStage::Returning {
            return Ok(None);
        }
        let saved = resume.returning.take().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a resumed RETURNING row lost its values"
            )
        })?;
        let rows = saved.restore(arena)?;
        let decode = |row: Option<&&[u8]>| -> Result<Option<&'out [Datum<'out>]>, SqlError> {
            let Some(row) = row else {
                return Ok(None);
            };
            let row = arena
                .alloc_slice_copy(row)
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            let mut values = [Datum::Null; MAX_COLUMNS];
            let width = projected::decode_projected_row_into(row, arena, &mut values)?;
            let values = arena
                .alloc_slice_copy(&values[..width])
                .map_err(|_| crate::sql::query::arena_full_pub())?;
            Ok(Some(&*values))
        };
        Ok(Some(SavedReturning {
            old: decode(rows.old.first())?,
            new: decode(rows.new.first())?,
        }))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "a row position records its selection, counters and rollback marks"
    )]
    fn begin_stage(
        &mut self,
        stage: RowStage,
        index: usize,
        row: Option<PhysicalRow>,
        changed: u64,
        transitions: CaptureLengths,
        txn: &TxnState,
        responder: &Responder<'_>,
    ) -> Result<(), SqlError> {
        let (wal, lock) = self.journal.ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "write row progress has no attempt journal position"
            )
        })?;
        self.resume = None;
        self.returning = None;
        self.started = true;
        self.running = Some(RowStart {
            index,
            stage,
            row,
            changed,
            transitions,
            mark: txn.statement_mark(wal, lock),
            output: responder.mark(),
            point: ReplayPoint::capture()?,
        });
        Ok(())
    }

    /// The row phase finished; a later wait or call is not inside a row.
    pub(super) fn finish_rows(&mut self) {
        self.running = None;
    }

    /// Whether an attempt has started changing rows, so running the write
    /// again from its start would repeat them.
    pub(crate) fn changed_rows(&self) -> bool {
        self.started
    }

    /// The write changed rows a rollback cannot undo, such as a foreign
    /// table's remote rows: it may keep them, but not run again from its start.
    pub(super) fn forbid_restart(&mut self) {
        self.unrestartable = true;
    }

    /// The attempt stopped inside its running row for a routine call. Saves
    /// the earlier rows' transition rows so the next attempt can continue.
    pub(super) fn interrupt(
        &mut self,
        selected: usize,
        captures: [Option<&TransitionCapture<'_>>; 2],
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        let Some(start) = self.running.take() else {
            return Ok(());
        };
        let mut transitions = [None, None];
        for (index, capture) in captures.into_iter().enumerate() {
            transitions[index] = capture
                .map(|capture| {
                    transition_rows::SavedTransitionRows::capture(
                        capture.prefix(start.transitions[index]),
                        pool,
                    )
                })
                .transpose()?;
        }
        let returning = self.returning.take();
        if (start.stage == RowStage::Returning) != returning.is_some() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "an unfinished RETURNING row did not save its values"
            ));
        }
        self.resume = Some(RowResume {
            start,
            selected,
            transitions,
            returning,
        });
        Ok(())
    }

    /// For the caller after an attempt ended with a routine call: the row to
    /// roll back while earlier rows stay changed, or `None` to roll back the
    /// whole attempt. `Err` when that rollback could not undo remote rows.
    pub(crate) fn unfinished_row(&self) -> Result<Option<UnfinishedRow>, SqlError> {
        match &self.resume {
            Some(resume) => Ok(Some(UnfinishedRow {
                mark: resume.start.mark,
                output: resume.start.output,
            })),
            None if self.unrestartable => Err(super::write_attempts::retained_progress_required()),
            None => Ok(None),
        }
    }
}

fn same_row(left: Option<PhysicalRow>, right: Option<PhysicalRow>) -> bool {
    match (left, right) {
        (None, None) => true,
        (
            Some(PhysicalRow::Local {
                table_index: left_table,
                rowid: left_row,
                ..
            }),
            Some(PhysicalRow::Local {
                table_index: right_table,
                rowid: right_row,
                ..
            }),
        ) => left_table == right_table && left_row == right_row,
        _ => false,
    }
}

/// Starts row `index` of a write that may keep progress. `Some` returns the
/// values of a row an earlier attempt changed, whose `RETURNING` stage
/// remains.
#[expect(
    clippy::too_many_arguments,
    reason = "a row position records its selection, counters and rollback marks"
)]
pub(super) fn start_row<'a>(
    progress: Option<&mut RowProgress>,
    index: usize,
    row: Option<PhysicalRow>,
    changed: u64,
    transitions: CaptureLengths,
    txn: &TxnState,
    responder: &Responder<'_>,
    arena: &'a Arena,
) -> Result<Option<SavedReturning<'a>>, SqlError> {
    let Some(progress) = progress else {
        return Ok(None);
    };
    if let Some(resumed) = progress.take_returning(index, arena)? {
        return Ok(Some(resumed));
    }
    progress.begin_row(index, row, changed, transitions, txn, responder)?;
    Ok(None)
}

/// After a write's row phase: an attempt that stopped for a routine call
/// saves where it stopped; any other end leaves no unfinished row.
pub(super) fn settle_rows(
    progress: Option<&mut RowProgress>,
    outcome: &Outcome,
    txn: &TxnState,
    selected: usize,
    captures: [Option<&TransitionCapture<'_>>; 2],
    pool: &std::sync::Arc<LeasePool>,
) -> Result<(), SqlError> {
    let Some(progress) = progress else {
        return Ok(());
    };
    match outcome {
        Ok(Err(error))
            if error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION
                && txn.pending_write_completion.is_none() =>
        {
            progress.interrupt(selected, captures, pool)
        }
        _ => {
            progress.finish_rows();
            Ok(())
        }
    }
}
