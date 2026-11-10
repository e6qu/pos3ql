//! PostgreSQL's FETCH/MOVE motion for a scrollable cursor whose rows are
//! evaluated again when revisited.
//!
//! PostgreSQL runs a scrollable cursor's plan backward instead of returning
//! saved rows when the plan supports it, so a revisited row's filter and
//! projection run again (a `nextval` call returns a new value). This module
//! reproduces `DoPortalRunFetch`, `PortalRunSelect`, `DoPortalRewind` and the
//! Limit node's state machine over saved input rows, step by step: the caller
//! evaluates one input row per [`ScrollAction::Visit`] and reports the
//! outcome, so a row that waits for storage, a lock or a routine resumes
//! without repeating completed steps.
//! References: <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/tcop/pquery.c>
//! and <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeLimit.c>.

use super::FetchMotion;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

/// `FETCH_ALL`: run until the input ends.
const ALL: u64 = u64::MAX;

/// How a scrollable retained cursor treats rows it returns to again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Revisit {
    /// Saved output rows are returned again: PostgreSQL materializes the
    /// plan, or evaluating the row again cannot be observed.
    SavedOutput,
    /// Saved input rows that passed the filter are projected again.
    Reproject,
    /// Every scanned input row is filtered and projected again.
    Rescan,
}

/// One input row the caller must evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollInput {
    /// The next row from the source; a row kept by the outcome becomes the
    /// next saved input.
    Source,
    /// Saved input `index` (zero-based).
    Saved(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollAction {
    /// Evaluate `input`: run the filter when `filter`, then the projection,
    /// and save the projected row when `keep`.
    Visit {
        input: ScrollInput,
        filter: bool,
        keep: bool,
    },
    /// LIMIT and OFFSET must be evaluated before the first row.
    Limits,
    /// The command is complete and reports this row count.
    Done(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisitOutcome {
    /// The row passed; `output` is its saved projected row, if kept.
    Passed { output: Option<u32> },
    /// The filter rejected the row.
    Rejected,
    /// The source has no more rows.
    SourceEnd,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortalOp {
    Rewind,
    Run {
        forward: bool,
        count: u64,
        emit: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LimitState {
    Rescan,
    Empty,
    InWindow,
    SubplanEof,
    WindowEnd,
    WindowStart,
}

#[derive(Clone, Copy, Debug)]
struct Limit {
    offset: u64,
    /// `ALL` when only OFFSET applies.
    count: u64,
    position: u64,
    state: LimitState,
    /// The saved output of the last row the Limit node pulled.
    slot: Option<u32>,
}

/// Which plan node consumes the outstanding visit's row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Caller {
    /// No Limit node: the row is the step's result.
    Plain,
    /// The Limit node is reading its window's first row.
    Fill,
    /// The Limit node is moving inside its window.
    Window,
    /// The Limit node is backing up from the input's end.
    Eof,
}

#[derive(Clone, Copy, Debug)]
struct Outstanding {
    forward: bool,
    caller: Caller,
    input: ScrollInput,
    target: u64,
}

/// A row produced by the input: its saved output when kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Row(Option<u32>);

/// The command's remaining work, kept across waits.
#[derive(Clone, Copy, Debug)]
struct Command {
    ops: [PortalOp; 3],
    len: u8,
    next: u8,
    /// Rows processed by the running `Run`, once it has decided to move.
    processed: u64,
    moving: Option<bool>,
    /// A fixed MOVE result; otherwise the last `Run`'s rows.
    fixed: Option<u64>,
    last: u64,
    /// Saving a held cursor leaves its portal position unchanged.
    hold: bool,
}

impl Command {
    fn new(ops: [PortalOp; 3], len: u8, fixed: Option<u64>, hold: bool) -> Self {
        Self {
            ops,
            len,
            next: 0,
            processed: 0,
            moving: None,
            fixed,
            last: 0,
            hold,
        }
    }

    fn running(&self) -> Option<(bool, u64, bool)> {
        match self.ops.get(self.next as usize) {
            Some(PortalOp::Run {
                forward,
                count,
                emit,
            }) if self.next < self.len => Some((*forward, *count, *emit)),
            _ => None,
        }
    }
}

pub(crate) struct ScrollScan {
    revisit: Revisit,
    position: i64,
    at_start: bool,
    at_end: bool,
    /// Input position: 0 before the first saved input, `n` on saved input
    /// `n` (one-based), `saved + 1` past the end once the source is done.
    scan: u64,
    saved: u64,
    input_done: bool,
    /// `None` until LIMIT and OFFSET are evaluated; then whether a Limit
    /// node exists.
    limit: Option<Option<Limit>>,
    command: Option<Command>,
    outstanding: Option<Outstanding>,
}

/// Receives each output row the command returns, in order.
pub(crate) type Emit<'a> = &'a mut dyn FnMut(u32) -> Result<(), SqlError>;

fn internal(message: &'static str) -> SqlError {
    sql_err!(sqlstate::INTERNAL_ERROR, "{}", message)
}

impl ScrollScan {
    pub(crate) fn new(revisit: Revisit) -> Self {
        Self {
            revisit,
            position: 0,
            at_start: true,
            at_end: false,
            scan: 0,
            saved: 0,
            input_done: false,
            limit: None,
            command: None,
            outstanding: None,
        }
    }

    pub(crate) fn revisit(&self) -> Revisit {
        self.revisit
    }

    /// PostgreSQL's portal position and end flags.
    pub(crate) fn portal(&self) -> (i64, bool, bool) {
        (self.position, self.at_start, self.at_end)
    }

    /// The saved output the Limit node may return again without evaluation.
    pub(crate) fn limit_slot(&self) -> Option<u32> {
        self.limit.flatten().and_then(|limit| limit.slot)
    }

    /// Moves the Limit slot when a new command compacts its outputs.
    pub(crate) fn relocate_limit_slot(&mut self, slot: Option<u32>) {
        if let Some(Some(limit)) = self.limit.as_mut() {
            limit.slot = slot;
        }
    }

    /// Records evaluated `(offset, count)`, or `None` when the plan has no
    /// Limit node.
    pub(crate) fn set_limits(&mut self, limits: Option<(u64, u64)>) {
        self.limit = Some(limits.map(|(offset, count)| Limit {
            offset,
            count,
            position: 0,
            state: LimitState::Rescan,
            slot: None,
        }));
    }

    /// Plans one FETCH or MOVE exactly as `DoPortalRunFetch` decomposes it.
    pub(crate) fn begin(&mut self, motion: FetchMotion, move_only: bool) {
        let mut ops = [PortalOp::Rewind; 3];
        let mut len = 0u8;
        let mut push = |op| {
            ops[len as usize] = op;
            len += 1;
        };
        let emit = !move_only;
        let run = |forward, count: u64, emit| PortalOp::Run {
            forward,
            count,
            emit,
        };
        let mut fixed = None;
        match motion {
            FetchMotion::Count(0) | FetchMotion::Relative(0) | FetchMotion::BackwardZero => {
                let on_row = !self.at_start && !self.at_end;
                if move_only {
                    fixed = Some(u64::from(on_row));
                } else if on_row {
                    // Back up one row so the current row is fetched again.
                    push(run(false, 1, false));
                    push(run(true, 1, true));
                } else {
                    push(run(true, 0, true));
                }
            }
            FetchMotion::Count(count) if count > 0 => push(run(true, count as u64, emit)),
            FetchMotion::Count(count) => push(run(false, u64::from(count.unsigned_abs()), emit)),
            FetchMotion::All => push(run(true, ALL, emit)),
            FetchMotion::BackwardAll if move_only => {
                // MOVE BACKWARD ALL is a rewind.
                let mut result = self.position.max(0) as u64;
                if result > 0 && !self.at_end {
                    result -= 1;
                }
                fixed = Some(result);
                push(PortalOp::Rewind);
            }
            FetchMotion::BackwardAll => push(run(false, ALL, emit)),
            FetchMotion::Absolute(count) if count > 0 => {
                let count = count as u64;
                let position = self.position.max(0) as u64;
                // A target less than halfway back is reached by rewinding.
                if count - 1 <= position / 2 {
                    push(PortalOp::Rewind);
                    if count > 1 {
                        push(run(true, count - 1, false));
                    }
                } else {
                    let position = position + u64::from(self.at_end);
                    if count <= position {
                        push(run(false, position - count + 1, false));
                    } else if count > position + 1 {
                        push(run(true, count - position - 1, false));
                    }
                }
                push(run(true, 1, emit));
            }
            FetchMotion::Absolute(count) if count < 0 => {
                push(run(true, ALL, false));
                if count < -1 {
                    push(run(false, u64::from(count.unsigned_abs()) - 1, false));
                }
                push(run(false, 1, emit));
            }
            FetchMotion::Absolute(_) => {
                push(PortalOp::Rewind);
                push(run(true, 0, emit));
            }
            FetchMotion::Relative(count) if count > 0 => {
                if count > 1 {
                    push(run(true, count as u64 - 1, false));
                }
                push(run(true, 1, emit));
            }
            FetchMotion::Relative(count) => {
                if count < -1 {
                    push(run(false, u64::from(count.unsigned_abs()) - 1, false));
                }
                push(run(false, 1, emit));
            }
        }
        self.command = Some(Command::new(ops, len, fixed, false));
        self.outstanding = None;
    }

    /// Plans saving a held cursor at commit: PostgreSQL rewinds a scrollable
    /// portal and stores every row; the portal keeps its position.
    pub(crate) fn begin_hold(&mut self) {
        let mut ops = [PortalOp::Rewind; 3];
        ops[0] = PortalOp::Run {
            forward: true,
            count: ALL,
            emit: true,
        };
        self.rewind_plan();
        self.command = Some(Command::new(ops, 1, None, true));
        self.outstanding = None;
    }

    fn rewind_plan(&mut self) {
        self.scan = 0;
        if let Some(Some(limit)) = self.limit.as_mut() {
            limit.position = 0;
            limit.state = LimitState::Rescan;
        }
    }

    /// The next action for the current command. Transitions that need no
    /// evaluation are applied here; an outstanding visit is repeated.
    pub(crate) fn next(&mut self, emit: Emit<'_>) -> Result<ScrollAction, SqlError> {
        loop {
            if let Some(outstanding) = self.outstanding {
                return Ok(self.visit_action(outstanding));
            }
            let command = self
                .command
                .ok_or_else(|| internal("scroll cursor has no planned command"))?;
            if command.next >= command.len {
                self.command = None;
                return Ok(ScrollAction::Done(command.fixed.unwrap_or(command.last)));
            }
            let Some((forward, count, _)) = command.running() else {
                // DoPortalRewind: a portal that has not moved needs nothing.
                if !(self.at_start && !self.at_end) {
                    self.rewind_plan();
                    self.position = 0;
                    self.at_start = true;
                    self.at_end = false;
                }
                let command = self.command.as_mut().expect("planned command");
                command.next += 1;
                continue;
            };
            let moving = match command.moving {
                Some(moving) => moving,
                None => {
                    let stopped = if command.hold {
                        false
                    } else if forward {
                        self.at_end
                    } else {
                        self.at_start
                    };
                    let moving = !stopped && count != 0;
                    let command = self.command.as_mut().expect("planned command");
                    command.moving = Some(moving);
                    command.processed = 0;
                    moving
                }
            };
            if !moving || command.processed == count {
                self.finish_run();
                continue;
            }
            if self.limit.is_none() {
                return Ok(ScrollAction::Limits);
            }
            if let Some(result) = self.node(forward)? {
                self.node_result(result, emit)?;
            }
        }
    }

    fn finish_run(&mut self) {
        let command = self.command.expect("planned command");
        let (forward, count, _) = command.running().expect("running portal step");
        let processed = command.processed;
        if command.moving == Some(true) && !command.hold {
            if forward {
                if processed > 0 {
                    self.at_start = false;
                }
                if count == ALL || processed < count {
                    self.at_end = true;
                }
                self.position = self.position.saturating_add(processed as i64);
            } else {
                if processed > 0 && self.at_end {
                    self.at_end = false;
                    self.position += 1;
                }
                if count == ALL || processed < count {
                    self.at_start = true;
                    self.position = 0;
                } else {
                    self.position -= processed as i64;
                }
            }
        }
        let command = self.command.as_mut().expect("planned command");
        command.last = processed;
        command.next += 1;
        command.moving = None;
        command.processed = 0;
    }

    /// A Limit-node (or plain plan) step result: a row, or the end.
    fn node_result(&mut self, row: Option<Row>, sink: Emit<'_>) -> Result<(), SqlError> {
        let Some(Row(output)) = row else {
            self.finish_run();
            return Ok(());
        };
        let command = self.command.as_mut().expect("planned command");
        command.processed += 1;
        let (_, _, emit) = command.running().expect("running portal step");
        if emit {
            let output = output.ok_or_else(|| internal("scroll cursor emitted an unsaved row"))?;
            sink(output)?;
        }
        Ok(())
    }

    /// Whether the current command must keep the visited row's output.
    fn keep(&self) -> bool {
        matches!(self.limit, Some(Some(_)))
            || self
                .command
                .and_then(|command| command.running())
                .is_some_and(|(_, _, emit)| emit)
    }

    fn visit_action(&self, outstanding: Outstanding) -> ScrollAction {
        ScrollAction::Visit {
            input: outstanding.input,
            filter: matches!(outstanding.input, ScrollInput::Source)
                || self.revisit == Revisit::Rescan,
            keep: self.keep(),
        }
    }

    /// One Limit-node (or plain plan) step. `Ok(None)` means a visit is now
    /// outstanding; `Ok(Some(result))` is the node's immediate result.
    fn node(&mut self, forward: bool) -> Result<Option<Option<Row>>, SqlError> {
        let Some(Some(mut limit)) = self.limit else {
            return self.request(forward, Caller::Plain);
        };
        let result = match limit.state {
            LimitState::Rescan if !forward => None,
            LimitState::Rescan if limit.count == 0 => {
                limit.state = LimitState::Empty;
                None
            }
            LimitState::Rescan => return self.request(true, Caller::Fill),
            LimitState::Empty => None,
            LimitState::InWindow if forward => {
                if limit.count != ALL && limit.position - limit.offset >= limit.count {
                    limit.state = LimitState::WindowEnd;
                    None
                } else {
                    return self.request(true, Caller::Window);
                }
            }
            LimitState::InWindow => {
                if limit.position <= limit.offset + 1 {
                    limit.state = LimitState::WindowStart;
                    None
                } else {
                    return self.request(false, Caller::Window);
                }
            }
            LimitState::SubplanEof if forward => None,
            LimitState::SubplanEof => return self.request(false, Caller::Eof),
            LimitState::WindowEnd if forward => None,
            LimitState::WindowStart if !forward => None,
            LimitState::WindowEnd | LimitState::WindowStart => {
                // The last pulled row is returned again without evaluation.
                limit.state = LimitState::InWindow;
                Some(Row(Some(limit.slot.ok_or_else(|| {
                    internal("LIMIT window has no saved row to return")
                })?)))
            }
        };
        self.limit = Some(Some(limit));
        Ok(Some(result))
    }

    /// Starts one input step. A step that needs no evaluation completes at
    /// once; otherwise its visit becomes outstanding and `Ok(None)` returns.
    fn request(&mut self, forward: bool, caller: Caller) -> Result<Option<Option<Row>>, SqlError> {
        let (input, target) = if forward {
            let target = self.scan + 1;
            if target <= self.saved {
                (ScrollInput::Saved(target as usize - 1), target)
            } else if self.input_done {
                self.scan = self.saved + 1;
                return self.sub_result(None, forward, caller);
            } else {
                (ScrollInput::Source, target)
            }
        } else if self.scan <= 1 {
            self.scan = 0;
            return self.sub_result(None, forward, caller);
        } else {
            let target = self.scan - 1;
            (ScrollInput::Saved(target as usize - 1), target)
        };
        self.outstanding = Some(Outstanding {
            forward,
            caller,
            input,
            target,
        });
        Ok(None)
    }

    /// Applies an input step's row to the node that requested it. Returns
    /// the node's result, or `Ok(None)` when the node needs another step.
    fn sub_result(
        &mut self,
        row: Option<Row>,
        forward: bool,
        caller: Caller,
    ) -> Result<Option<Option<Row>>, SqlError> {
        let Some(Some(mut limit)) = self.limit.filter(|_| caller != Caller::Plain) else {
            return Ok(Some(row));
        };
        let result = match (caller, row) {
            (Caller::Fill, None) => {
                limit.state = LimitState::Empty;
                Some(None)
            }
            (Caller::Fill, Some(row)) => {
                limit.slot = row.0;
                limit.position += 1;
                if limit.position > limit.offset {
                    limit.state = LimitState::InWindow;
                    Some(Some(row))
                } else {
                    None
                }
            }
            (Caller::Window, None) if forward => {
                limit.state = LimitState::SubplanEof;
                Some(None)
            }
            (Caller::Window, Some(row)) => {
                limit.slot = row.0;
                if forward {
                    limit.position += 1;
                } else {
                    limit.position -= 1;
                }
                Some(Some(row))
            }
            (Caller::Eof, Some(row)) => {
                limit.slot = row.0;
                limit.state = LimitState::InWindow;
                Some(Some(row))
            }
            (Caller::Window | Caller::Eof, None) => {
                return Err(internal("LIMIT subplan failed to run backwards"));
            }
            (Caller::Plain, _) => unreachable!("plain steps return above"),
        };
        self.limit = Some(Some(limit));
        Ok(result)
    }

    /// Records the outcome of the outstanding visit.
    pub(crate) fn complete(
        &mut self,
        outcome: VisitOutcome,
        emit: Emit<'_>,
    ) -> Result<(), SqlError> {
        let outstanding = self
            .outstanding
            .take()
            .ok_or_else(|| internal("scroll cursor completed a visit it did not request"))?;
        let source = outstanding.input == ScrollInput::Source;
        let row = match outcome {
            VisitOutcome::SourceEnd if source => {
                self.input_done = true;
                self.scan = self.saved + 1;
                None
            }
            VisitOutcome::SourceEnd => return Err(internal("saved cursor input ended")),
            VisitOutcome::Rejected => {
                if !source || self.revisit == Revisit::Rescan {
                    self.saved += u64::from(source);
                    self.scan = outstanding.target;
                }
                // The node repeats its step in the same direction.
                return Ok(());
            }
            VisitOutcome::Passed { output } => {
                if output.is_none() && self.keep() {
                    return Err(internal("scroll cursor did not keep a required row"));
                }
                self.saved += u64::from(source);
                self.scan = outstanding.target;
                Some(Row(output))
            }
        };
        if let Some(result) = self.sub_result(row, outstanding.forward, outstanding.caller)? {
            self.node_result(result, emit)?;
        }
        Ok(())
    }

    /// Drops the current command after an error; the cursor then fails.
    pub(crate) fn abandon(&mut self) {
        self.command = None;
        self.outstanding = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Evaluates a scan over input values. A passing row "calls nextval"
    /// once per evaluation; the filter may also call it.
    struct Model {
        rows: Vec<i64>,
        saved: Vec<i64>,
        filter: fn(i64, &mut i64) -> bool,
        limits: Option<(u64, u64)>,
        sequence: i64,
        outputs: Vec<(i64, i64)>,
    }

    impl Model {
        fn new(
            rows: &[i64],
            filter: fn(i64, &mut i64) -> bool,
            limits: Option<(u64, u64)>,
        ) -> Self {
            Self {
                rows: rows.to_vec(),
                saved: Vec::new(),
                filter,
                limits,
                sequence: 0,
                outputs: Vec::new(),
            }
        }

        fn drive(&mut self, scan: &mut ScrollScan) -> (u64, Vec<(i64, i64)>) {
            let mut emitted = Vec::new();
            loop {
                let outputs = &self.outputs;
                let action = scan
                    .next(&mut |index| {
                        emitted.push(outputs[index as usize]);
                        Ok(())
                    })
                    .unwrap();
                let outcome = match action {
                    ScrollAction::Done(count) => return (count, emitted),
                    ScrollAction::Limits => {
                        scan.set_limits(self.limits);
                        continue;
                    }
                    ScrollAction::Visit {
                        input,
                        filter,
                        keep,
                    } => {
                        let value = match input {
                            ScrollInput::Source => self.rows.get(self.saved.len()).copied(),
                            ScrollInput::Saved(index) => Some(self.saved[index]),
                        };
                        match value {
                            None => VisitOutcome::SourceEnd,
                            Some(value) => {
                                let passed = !filter || (self.filter)(value, &mut self.sequence);
                                if input == ScrollInput::Source
                                    && (passed || scan.revisit() == Revisit::Rescan)
                                {
                                    self.saved.push(value);
                                }
                                if passed {
                                    self.sequence += 1;
                                    let output = keep.then(|| {
                                        self.outputs.push((value, self.sequence));
                                        (self.outputs.len() - 1) as u32
                                    });
                                    VisitOutcome::Passed { output }
                                } else {
                                    VisitOutcome::Rejected
                                }
                            }
                        }
                    }
                };
                let outputs = &self.outputs;
                scan.complete(outcome, &mut |index| {
                    emitted.push(outputs[index as usize]);
                    Ok(())
                })
                .unwrap();
            }
        }

        fn run(
            &mut self,
            scan: &mut ScrollScan,
            motion: FetchMotion,
            move_only: bool,
        ) -> (u64, Vec<(i64, i64)>) {
            scan.begin(motion, move_only);
            self.drive(scan)
        }
    }

    /// A command, whether it is MOVE, its reported count and its rows.
    type Step<'a> = (FetchMotion, bool, u64, &'a [(i64, i64)]);

    fn replay(model: &mut Model, scan: &mut ScrollScan, steps: &[Step<'_>]) {
        for (index, (motion, move_only, count, expected)) in steps.iter().enumerate() {
            let (actual_count, rows) = model.run(scan, *motion, *move_only);
            assert_eq!(
                (actual_count, rows.as_slice()),
                (*count, *expected),
                "step {index}: {motion:?}"
            );
        }
    }

    // Every expectation below is PostgreSQL 18.6 output for
    // `DECLARE c SCROLL CURSOR FOR SELECT id, nextval('s') FROM t ...`
    // over ids 1..=4 with a fresh sequence.

    #[test]
    fn revisited_rows_are_projected_again_in_postgresql_step_order() {
        let mut model = Model::new(&[1, 2, 3, 4], |_, _| true, None);
        let mut scan = ScrollScan::new(Revisit::Reproject);
        replay(
            &mut model,
            &mut scan,
            &[
                (FetchMotion::Count(1), false, 1, &[(1, 1)]),
                (FetchMotion::Count(1), false, 1, &[(2, 2)]),
                (FetchMotion::Count(-1), false, 1, &[(1, 3)]),
                (FetchMotion::Count(1), false, 1, &[(2, 4)]),
                (FetchMotion::Absolute(1), false, 1, &[(1, 5)]),
                (FetchMotion::Relative(0), false, 1, &[(1, 6)]),
                (FetchMotion::Count(0), false, 1, &[(1, 7)]),
                (FetchMotion::Count(2), true, 2, &[]),
                (FetchMotion::Absolute(2), false, 1, &[(2, 11)]),
                (FetchMotion::Absolute(-1), false, 1, &[(4, 14)]),
                (FetchMotion::Absolute(-2), false, 1, &[(3, 16)]),
                (FetchMotion::Count(1), false, 1, &[(4, 17)]),
                (FetchMotion::Count(1), false, 0, &[]),
                (FetchMotion::Count(-1), false, 1, &[(4, 18)]),
                (FetchMotion::BackwardAll, true, 3, &[]),
            ],
        );
        assert_eq!(model.sequence, 18);
    }

    #[test]
    fn a_volatile_filter_runs_again_for_every_revisited_input() {
        // WHERE id % 2 = 0 OR nextval('s') < 0
        let mut model = Model::new(
            &[1, 2, 3, 4],
            |value, sequence| {
                value % 2 == 0 || {
                    *sequence += 1;
                    false
                }
            },
            None,
        );
        let mut scan = ScrollScan::new(Revisit::Rescan);
        replay(
            &mut model,
            &mut scan,
            &[
                (FetchMotion::Count(1), false, 1, &[(2, 2)]),
                (FetchMotion::Count(1), false, 1, &[(4, 4)]),
                (FetchMotion::Count(-1), false, 1, &[(2, 6)]),
            ],
        );
    }

    #[test]
    fn limit_windows_return_their_edge_row_again_without_evaluation() {
        // LIMIT 2 OFFSET 1
        let mut model = Model::new(&[1, 2, 3, 4], |_, _| true, Some((1, 2)));
        let mut scan = ScrollScan::new(Revisit::Reproject);
        replay(
            &mut model,
            &mut scan,
            &[
                (FetchMotion::All, false, 2, &[(2, 2), (3, 3)]),
                (FetchMotion::Count(-1), false, 1, &[(3, 3)]),
                (FetchMotion::Count(-1), false, 1, &[(2, 4)]),
                (FetchMotion::Count(-1), false, 0, &[]),
                (FetchMotion::Count(1), false, 1, &[(2, 4)]),
            ],
        );
        assert_eq!(model.sequence, 4);
    }

    #[test]
    fn holding_rewinds_and_evaluates_every_row_without_moving_the_portal() {
        let mut model = Model::new(&[1, 2, 3, 4], |_, _| true, None);
        let mut scan = ScrollScan::new(Revisit::Reproject);
        replay(
            &mut model,
            &mut scan,
            &[
                (FetchMotion::Count(1), false, 1, &[(1, 1)]),
                (FetchMotion::Count(1), false, 1, &[(2, 2)]),
            ],
        );
        scan.begin_hold();
        let (_, rows) = model.drive(&mut scan);
        assert_eq!(rows, [(1, 3), (2, 4), (3, 5), (4, 6)]);
        assert_eq!(scan.portal(), (2, false, false));
    }
}
