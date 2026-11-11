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
//!
//! A statement made of several steps (the sub-statements of a data-modifying
//! `WITH` and its main statement; a rule's original statement and actions)
//! also keeps its finished steps: the next attempt skips them, using their
//! saved output rows and row counts, and runs the unfinished step at its
//! row, or from the step's start when no row was unfinished.
//!
//! A top-level write also keeps its rows across storage and lock waits: its
//! transaction holds the progress in a [`RetainedWrite`] while the statement
//! waits, each row's reads yield until the row's own first effect, and a
//! wait rolls back only the unfinished row.

use super::*;
use crate::sql::query::{self, InvocationPosition};
use crate::sql::sequence::{self, StatementReplayStart};
use crate::sql::txn::{StatementMark, SuspendedTriggerSql, TransactionEffectMark};

/// A top-level write's progress while its statement waits for storage or a
/// lock. The transaction keeps it until the statement runs again, ends, or
/// is canceled; the next run continues at the unfinished row with the same
/// invocation log and sequence replay.
pub(crate) struct RetainedWrite {
    pub(crate) progress: RowProgress,
    pub(crate) invocations: query::RoutineInvocationState,
    pub(crate) replay: Option<StatementReplayStart>,
    /// Where the statement's first run started: its `AFTER` triggers belong
    /// there, and a run from the start rolls back to it.
    pub(crate) start: Option<(StatementMark, crate::pg::overflow::OutputMark)>,
}

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
    transitions: [Option<transition_rows::SavedTransitionRows>; CAPTURES],
    /// The changed row's old and new values, for its `RETURNING` stage.
    returning: Option<transition_rows::SavedTransitionRows>,
    /// Statement-wide row state the write keeps besides its counters.
    state: Option<transition_rows::SavedTransitionRows>,
}

/// A write's transition captures: one per operation it performs.
pub(super) const CAPTURES: usize = 3;

/// Rows each of a write's transition captures (its own operation, the
/// update of `INSERT ... ON CONFLICT`, or `MERGE`'s three) held when a row
/// started.
pub(super) type CaptureLengths = [(usize, usize); CAPTURES];

pub(super) fn capture_lengths(
    captures: [Option<&TransitionCapture<'_>>; CAPTURES],
) -> CaptureLengths {
    captures.map(|capture| capture.map_or((0, 0), TransitionCapture::lengths))
}

/// Where a resumed attempt continues.
pub(super) struct ResumedRows<'p> {
    pub(super) first: usize,
    pub(super) changed: u64,
    pub(super) transitions: [Option<&'p transition_rows::SavedTransitionRows>; CAPTURES],
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

/// Where the unfinished step of a multi-step statement started.
#[derive(Clone, Copy)]
struct StepStart {
    mark: StatementMark,
    output: crate::pg::overflow::OutputMark,
    point: ReplayPoint,
    effects: TransactionEffectMark,
    /// The command the step runs as; a resumed step runs as it again so its
    /// rows are selected under the same command.
    command: u32,
}

/// The finished steps of a multi-step statement.
struct StepProgress {
    /// Whether this attempt runs a step at this level, so a multi-step
    /// statement inside it keeps its steps one level deeper.
    running: bool,
    finished: usize,
    start: Option<StepStart>,
    /// The finished steps' output rows and row counts, then the unfinished
    /// step's captured output rows: rows in `old`, one count row per step
    /// in `new`.
    saved: Option<transition_rows::SavedTransitionRows>,
}

impl StepProgress {
    const EMPTY: Self = Self {
        running: false,
        finished: 0,
        start: None,
        saved: None,
    };
}

/// A finished step's output, as a multi-step statement keeps it.
#[derive(Clone, Copy)]
pub(crate) struct StepRecord<'r> {
    pub(crate) rows: &'r [&'r [u8]],
    pub(crate) affected: u64,
}

/// The steps an interrupted attempt kept, decoded in the statement arena.
pub(crate) struct ResumedSteps<'a> {
    pub(crate) finished: &'a [StepRecord<'a>],
    /// Output rows the unfinished step had captured from its kept rows.
    pub(crate) captured: &'a [&'a [u8]],
}

pub(crate) struct RowProgress {
    journal: Option<(usize, u64)>,
    selection: Option<ReplayPoint>,
    running: Option<RowStart>,
    resume: Option<RowResume>,
    command: Option<SuspendedTriggerSql>,
    returning: Option<transition_rows::SavedTransitionRows>,
    state: Option<transition_rows::SavedTransitionRows>,
    started: bool,
    unrestartable: bool,
    /// One record per level at which multi-step statements nest inside one
    /// another's steps (a rule action on a table with rules, a rule on a
    /// `WITH` target) and still keep their progress: `write_step_levels`,
    /// leased when a multi-step statement first claims a level.
    steps: crate::mem::lease_pool::MutableSlice<StepProgress>,
    step_pool: std::sync::Arc<LeasePool>,
    step_levels: usize,
    /// The level a data-modifying `WITH` claimed in this attempt.
    implicit_level: usize,
    /// A multi-step statement of this attempt nested deeper than the step
    /// levels and runs its steps without progress.
    steps_exhausted: bool,
    /// The statement keeps this progress across storage and lock waits.
    retained: bool,
    /// The unreplayable-effect epoch when the attempt started.
    attempt_effects: u64,
    /// This attempt's rows can resume after a wait.
    rows_resumable: bool,
}

impl RowProgress {
    /// Progress whose step records `execution_control_bytes` supplies.
    pub(crate) fn new(storage: &crate::storage::Storage) -> Result<Self, SqlError> {
        let step_pool = std::sync::Arc::clone(&storage.execution_control);
        Ok(Self {
            steps: step_pool.lease(0).map_err(step_records_full)?,
            step_pool,
            step_levels: storage.write_step_levels,
            journal: None,
            selection: None,
            running: None,
            resume: None,
            command: None,
            returning: None,
            state: None,
            started: false,
            unrestartable: false,
            implicit_level: 0,
            steps_exhausted: false,
            retained: false,
            attempt_effects: 0,
            rows_resumable: false,
        })
    }

    /// Progress of a statement whose transaction keeps it across waits.
    pub(crate) fn retained(storage: &crate::storage::Storage) -> Result<Self, SqlError> {
        let mut progress = Self::new(storage)?;
        progress.retained = true;
        Ok(progress)
    }

    /// Records the journal and lock positions of the attempt about to run;
    /// row rollback marks combine them with the transaction's own positions.
    pub(crate) fn begin_attempt(&mut self, wal: usize, lock: u64) {
        self.journal = Some((wal, lock));
        self.running = None;
        self.steps_exhausted = false;
        self.attempt_effects = query::unreplayable_effect_epoch();
        self.rows_resumable = false;
        for level in self.steps.iter_mut() {
            level.running = false;
        }
    }

    /// The error for a routine call that running after a rollback would
    /// hide this attempt's changed rows from: the step nesting capacity when
    /// a statement nested past it, else the missing row progress.
    pub(crate) fn progress_required(&self) -> SqlError {
        if self.steps_exhausted {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "multi-step statements nest deeper than the {} levels write progress keeps \
                 (write_step_levels)",
                self.step_levels
            )
        } else {
            super::write_attempts::retained_progress_required()
        }
    }

    /// Claims the step level of a multi-step statement: one deeper than the
    /// innermost step this attempt runs. Levels follow the statements'
    /// nesting, so each attempt claims the same level for the same
    /// statement. `None` beyond the deepest level: the statement then runs
    /// its steps without progress, and a call after its first change fails
    /// loudly instead of hiding that change.
    pub(crate) fn claim_step_level(&mut self) -> Result<Option<usize>, SqlError> {
        if self.steps.is_empty() {
            let mut steps = self
                .step_pool
                .lease(self.step_levels)
                .map_err(step_records_full)?;
            for _ in 0..self.step_levels {
                steps
                    .push_reserved(StepProgress::EMPTY)
                    .map_err(step_records_full)?;
            }
            self.steps = steps;
        }
        let level = self
            .steps
            .iter()
            .rposition(|level| level.running)
            .map_or(0, |running| running + 1);
        self.steps_exhausted |= level >= self.steps.len();
        Ok((level < self.steps.len()).then_some(level))
    }

    /// [`Self::claim_step_level`] for a data-modifying `WITH`, whose steps
    /// then use the level implicitly.
    pub(crate) fn claim_steps(&mut self) -> Result<bool, SqlError> {
        Ok(match self.claim_step_level()? {
            Some(level) => {
                self.implicit_level = level;
                true
            }
            None => false,
        })
    }

    /// Before step `index` of the claimed statement: `false` when an earlier
    /// attempt finished it. An unfinished step resumed by this attempt
    /// replays its calls from its start; a new step records its start. With
    /// `separate`, each step after the first is its own command, which sees
    /// the earlier steps' changes (a rule's actions); otherwise all steps
    /// share the statement's snapshot (data-modifying `WITH`).
    pub(crate) fn begin_step(
        &mut self,
        index: usize,
        separate: bool,
        txn: &mut TxnState,
        responder: &Responder<'_>,
    ) -> Result<bool, SqlError> {
        self.begin_step_at(self.implicit_level, index, separate, txn, responder)
    }

    /// [`Self::begin_step`] at a claimed `level`.
    pub(crate) fn begin_step_at(
        &mut self,
        level: usize,
        index: usize,
        separate: bool,
        txn: &mut TxnState,
        responder: &Responder<'_>,
    ) -> Result<bool, SqlError> {
        let steps = &mut self.steps[level];
        if index < steps.finished {
            return Ok(false);
        }
        if index != steps.finished {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a write step started before the previous one finished"
            ));
        }
        steps.running = true;
        match &mut steps.start {
            Some(start) => {
                txn.resume_command(start.command);
                start.point.seek()?;
                // The calls between attempts ran after the step's rollback:
                // a later rollback of the step keeps their effects and output.
                let (wal, lock) = self.journal.ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "write step progress has no attempt journal position"
                    )
                })?;
                start.mark = txn.statement_mark(wal, lock);
                start.output = responder.mark();
                start.effects = txn.transaction_effect_mark();
            }
            None => {
                // A new step keeps nothing of statements nested in earlier ones.
                for deeper in &mut self.steps[level + 1..] {
                    *deeper = StepProgress::EMPTY;
                }
                if separate && index != 0 {
                    txn.begin_command()?;
                }
                let (wal, lock) = self.journal.ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "write step progress has no attempt journal position"
                    )
                })?;
                self.steps[level].start = Some(StepStart {
                    mark: txn.statement_mark(wal, lock),
                    output: responder.mark(),
                    point: ReplayPoint::capture()?,
                    effects: txn.transaction_effect_mark(),
                    command: txn.command_id(),
                });
            }
        }
        Ok(true)
    }

    /// The running step finished; the next one starts with no row progress.
    pub(crate) fn finish_step(&mut self) {
        self.finish_step_at(self.implicit_level);
    }

    /// [`Self::finish_step`] at a claimed `level`.
    pub(crate) fn finish_step_at(&mut self, level: usize) {
        let steps = &mut self.steps[level];
        steps.finished += 1;
        steps.start = None;
        steps.running = false;
        for deeper in &mut self.steps[level + 1..] {
            *deeper = StepProgress::EMPTY;
        }
        self.selection = None;
        self.running = None;
        self.resume = None;
        self.returning = None;
        self.state = None;
    }

    /// Saves the finished steps' outputs, and the rows the unfinished step
    /// captured, when an attempt stops for a routine call.
    pub(crate) fn keep_steps(
        &mut self,
        finished: &[StepRecord<'_>],
        captured: &[&[u8]],
        arena: &Arena,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        self.keep_steps_at(self.implicit_level, finished, captured, arena, pool)
    }

    /// [`Self::keep_steps`] at a claimed `level`.
    pub(crate) fn keep_steps_at(
        &mut self,
        level: usize,
        finished: &[StepRecord<'_>],
        captured: &[&[u8]],
        arena: &Arena,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        if finished.len() != self.steps[level].finished {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a write kept a different number of finished steps"
            ));
        }
        // Captured rows belong to rows the unfinished step keeps; a step that
        // runs again from its start captures them again. Rows a deeper
        // statement keeps belong to that statement's own step.
        let deepest = self.steps[level + 1..]
            .iter()
            .all(|deeper| deeper.start.is_none());
        let captured = if self.resume.is_some() && deepest {
            captured
        } else {
            &[]
        };
        let mark = arena.mark();
        let saved = (|| {
            let full = |_| crate::sql::query::arena_full_pub();
            let total = finished
                .iter()
                .map(|step| step.rows.len())
                .sum::<usize>()
                .checked_add(captured.len())
                .ok_or_else(crate::sql::query::arena_full_pub)?;
            let mut rows = arena.alloc_slice_with(total, |_| &[][..]).map_err(full)?;
            let mut at = 0;
            for step in finished
                .iter()
                .map(|step| step.rows)
                .chain(core::iter::once(captured))
            {
                rows[at..at + step.len()].copy_from_slice(step);
                at += step.len();
            }
            let counts = arena
                .alloc_slice_with(finished.len() + 1, |_| &[][..])
                .map_err(full)?;
            for (count, step) in
                counts
                    .iter_mut()
                    .zip(finished.iter().copied().chain(core::iter::once(StepRecord {
                        rows: captured,
                        affected: 0,
                    })))
            {
                *count = encode_projected_pub(
                    &[
                        Datum::Int8(
                            i64::try_from(step.rows.len())
                                .map_err(|_| crate::sql::query::arena_full_pub())?,
                        ),
                        Datum::Int8(i64::try_from(step.affected).unwrap_or(i64::MAX)),
                    ],
                    arena,
                )?;
            }
            let rows: &[&[u8]] = &*core::mem::take(&mut rows);
            transition_rows::SavedTransitionRows::capture(
                TransitionRows {
                    old: rows,
                    new: counts,
                },
                pool,
            )
        })();
        // SAFETY: the rows and counts were copied into the saved owner.
        unsafe { arena.rewind_to(mark) };
        self.steps[level].saved = Some(saved?);
        Ok(())
    }

    /// The steps an interrupted attempt kept, copied into `arena`; `None`
    /// before any attempt stopped after a finished step or kept rows.
    pub(crate) fn resumed_steps<'a>(
        &self,
        arena: &'a Arena,
    ) -> Result<Option<ResumedSteps<'a>>, SqlError> {
        self.resumed_steps_at(self.implicit_level, arena)
    }

    /// [`Self::resumed_steps`] at a claimed `level`.
    pub(crate) fn resumed_steps_at<'a>(
        &self,
        level: usize,
        arena: &'a Arena,
    ) -> Result<Option<ResumedSteps<'a>>, SqlError> {
        let Some(saved) = &self.steps[level].saved else {
            return Ok(None);
        };
        let full = |_| crate::sql::query::arena_full_pub();
        let rows = saved.restore(arena)?;
        let copied = arena
            .alloc_slice_with(rows.old.len(), |_| &[][..])
            .map_err(full)?;
        for (copy, row) in copied.iter_mut().zip(rows.old) {
            *copy = &*arena.alloc_slice_copy(row).map_err(full)?;
        }
        let copied: &'a [&'a [u8]] = copied;
        let count = |row: &[u8], column| -> Result<usize, SqlError> {
            match decode_projected_col_record(row, column, arena)? {
                Datum::Int8(value) => usize::try_from(value).map_err(|_| changed_steps()),
                _ => Err(changed_steps()),
            }
        };
        let (captured_counts, finished_counts) = rows.new.split_last().ok_or_else(changed_steps)?;
        let records = arena
            .alloc_slice_with(finished_counts.len(), |_| StepRecord {
                rows: &[],
                affected: 0,
            })
            .map_err(full)?;
        let mut at = 0;
        for (record, counts) in records.iter_mut().zip(finished_counts) {
            let length = count(counts, 0)?;
            record.rows = copied.get(at..at + length).ok_or_else(changed_steps)?;
            record.affected = count(counts, 1)? as u64;
            at += length;
        }
        let captured_length = count(captured_counts, 0)?;
        let captured = copied
            .get(at..at + captured_length)
            .ok_or_else(changed_steps)?;
        if at + captured_length != copied.len() || records.len() > self.steps[level].finished {
            return Err(changed_steps());
        }
        Ok(Some(ResumedSteps {
            finished: records,
            captured,
        }))
    }

    /// Whether this attempt continues rows an earlier attempt changed.
    pub(crate) fn resuming(&self) -> bool {
        self.resume.is_some()
    }

    /// Whether the attempt stops for `error` keeping its rows: a routine
    /// call that needs the engine, or a storage or lock wait in resumable
    /// rows.
    pub(crate) fn stops_for(&self, error: &SqlError) -> bool {
        error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION
            || (self.rows_resumable && is_wait(error))
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
                transitions: [None; CAPTURES],
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
            transitions: resume.transitions.each_ref().map(Option::as_ref),
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

    /// Saves statement-wide row state (as text) with the unfinished row, for
    /// a write whose per-row loop carries more than counters, such as
    /// `MERGE`'s matched target rows. A resumed row state is idempotent: the
    /// unfinished row runs again and sets what it already set.
    pub(super) fn keep_state(
        &mut self,
        state: &str,
        arena: &Arena,
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        if self.running.is_none() {
            return Ok(());
        }
        let mark = arena.mark();
        let saved = encode_projected_pub(&[Datum::Text(state)], arena).and_then(|row| {
            transition_rows::SavedTransitionRows::capture(
                TransitionRows {
                    old: core::slice::from_ref(&row),
                    new: &[],
                },
                pool,
            )
        });
        // SAFETY: the encoded row was copied into the saved owner.
        unsafe { arena.rewind_to(mark) };
        self.state = Some(saved?);
        Ok(())
    }

    /// The row state an interrupted attempt kept, decoded in `arena`.
    pub(super) fn resumed_state<'out>(
        &'out self,
        arena: &'out Arena,
    ) -> Result<Option<&'out str>, SqlError> {
        let Some(saved) = self
            .resume
            .as_ref()
            .and_then(|resume| resume.state.as_ref())
        else {
            return Ok(None);
        };
        let rows = saved.restore(arena)?;
        let row = rows.old.first().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a resumed write lost its row state"
            )
        })?;
        match decode_projected_col_record(row, 0, arena)? {
            Datum::Text(text) => Ok(Some(text)),
            _ => Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a resumed write's row state is not text"
            )),
        }
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
        // The row runs its RETURNING stage again from the same start, so a
        // second stop inside it saves the same values again.
        let start = resume.start;
        self.resume = None;
        self.running = Some(start);
        let saved = &*self.returning.insert(saved);
        if self.rows_resumable {
            query::reopen_statement_reads();
        }
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
        self.state = None;
        self.started = true;
        if self.rows_resumable {
            query::reopen_statement_reads();
        }
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

    /// The row phase finished; a later wait or call is not inside a row,
    /// so the statement's remaining reads block until another row starts.
    pub(super) fn finish_rows(&mut self) {
        self.running = None;
        if self.rows_resumable {
            query::set_resumable_row(false);
            query::protect_statement_effects();
        }
    }

    /// Whether an attempt has started changing rows, so running the write
    /// again from its start would repeat them.
    pub(crate) fn changed_rows(&self) -> bool {
        self.started || self.steps.iter().any(|level| level.finished != 0)
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
        captures: [Option<&TransitionCapture<'_>>; CAPTURES],
        pool: &std::sync::Arc<LeasePool>,
    ) -> Result<(), SqlError> {
        let Some(start) = self.running.take() else {
            return Ok(());
        };
        let mut transitions = [None, None, None];
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
            state: self.state.take(),
        });
        Ok(())
    }

    /// For the caller after an attempt ended with a routine call: the row to
    /// roll back while earlier rows stay changed, the start of the unfinished
    /// step when no row was unfinished, or `None` to roll back the whole
    /// attempt. `Err` when that rollback could not undo remote rows, or would
    /// hide changes the unfinished step made without row progress.
    pub(crate) fn unfinished_row(&self, txn: &TxnState) -> Result<Option<UnfinishedRow>, SqlError> {
        // The innermost unfinished step holds the rows the attempt stopped in.
        let step = self.steps.iter().rev().find_map(|level| level.start);
        match (&self.resume, step) {
            (Some(resume), _) => Ok(Some(UnfinishedRow {
                mark: resume.start.mark,
                output: resume.start.output,
            })),
            _ if self.unrestartable => Err(self.progress_required()),
            (None, Some(start)) if txn.transaction_effect_mark() != start.effects => {
                Err(self.progress_required())
            }
            (None, Some(start)) => Ok(Some(UnfinishedRow {
                mark: start.mark,
                output: start.output,
            })),
            (None, None) => Ok(None),
        }
    }
}

/// Starts the main statement of a `WITH` whose `steps` sub-statements
/// modify data, as the step after them.
pub(crate) fn begin_main_step<'p>(
    progress: &'p mut RowProgress,
    steps: usize,
    txn: &mut TxnState,
    responder: &Responder<'_>,
) -> Result<Option<&'p mut RowProgress>, SqlError> {
    if steps != 0 && !progress.begin_step(steps, false, txn, responder)? {
        return Err(changed_steps());
    }
    Ok(Some(progress))
}

/// After the main statement of a `WITH`: a stop for a routine call keeps
/// the sub-statements' output rows for the next attempt.
pub(crate) fn settle_main_step<'a, T>(
    progress: Option<&mut RowProgress>,
    outcome: Result<Result<(), SqlError>, T>,
    materialized: &[(&str, &'a crate::sql::ast::MaterializedCte<'a>)],
    arena: &'a Arena,
    pool: &std::sync::Arc<LeasePool>,
) -> Result<Result<(), SqlError>, T> {
    if let Ok(Err(error)) = &outcome
        && !materialized.is_empty()
        && let Some(progress) = progress
        && progress.stops_for(error)
    {
        let kept = cte_step_records(materialized, arena)
            .and_then(|records| progress.keep_steps(records, &[], arena, pool));
        if let Err(error) = kept {
            return Ok(Err(error));
        }
    }
    outcome
}

/// The finished data-modifying `WITH` sub-statements, as steps.
pub(crate) fn cte_step_records<'a>(
    materialized: &[(&str, &'a crate::sql::ast::MaterializedCte<'a>)],
    arena: &'a Arena,
) -> Result<&'a [StepRecord<'a>], SqlError> {
    let records = arena
        .alloc_slice_with(materialized.len(), |_| StepRecord {
            rows: &[],
            affected: 0,
        })
        .map_err(|_| crate::sql::query::arena_full_pub())?;
    for (record, (_, cte)) in records.iter_mut().zip(materialized) {
        let crate::sql::ast::MaterializedCteSource::Inline(rows) = cte.source else {
            return Err(changed_steps());
        };
        record.rows = rows;
    }
    Ok(records)
}

fn step_records_full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "write step progress: {}",
        error
    )
}

fn changed_steps() -> SqlError {
    sql_err!(
        sqlstate::INTERNAL_ERROR,
        "a resumed write lost its finished steps"
    )
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

/// Where a write's row changes start. The statement's remaining reads block,
/// unless its transaction keeps its progress across waits and nothing
/// before the rows would repeat differently when they are selected again:
/// then each row's reads yield until the row's own first effect, and a wait
/// rolls back only that row.
pub(super) fn enter_rows(progress: Option<&mut RowProgress>) {
    match progress {
        Some(progress)
            if progress.retained
                && query::unreplayable_effect_epoch() == progress.attempt_effects =>
        {
            progress.rows_resumable = true;
        }
        // A nested write inside a resumable row rolls back with that row.
        progress => {
            if let Some(progress) = progress {
                progress.rows_resumable = false;
            }
            query::protect_replayable_effect();
        }
    }
}

/// A storage or lock wait.
pub(crate) fn is_wait(error: &SqlError) -> bool {
    matches!(
        error.sqlstate.internal_control(),
        Some(
            crate::sql::eval::InternalControl::LockWait | crate::sql::eval::InternalControl::IoWait
        )
    )
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
    captures: [Option<&TransitionCapture<'_>>; CAPTURES],
    pool: &std::sync::Arc<LeasePool>,
) -> Result<(), SqlError> {
    let Some(progress) = progress else {
        return Ok(());
    };
    match outcome {
        Ok(Err(error)) if progress.stops_for(error) && txn.pending_write_completion.is_none() => {
            progress.interrupt(selected, captures, pool)
        }
        _ => {
            progress.finish_rows();
            Ok(())
        }
    }
}
