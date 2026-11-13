//! `INSERT ... SELECT` whose source rows the engine reads one at a time.
//!
//! PostgreSQL's ModifyTable pulls one source row, writes it, and returns
//! its `RETURNING` row before it pulls the next. A data-modifying `WITH`
//! query read as it runs, and a write that reads one, need that order. The
//! engine reads the source through a retained reader: a call in the source
//! hands off on its row, and a `WITH` query the source reads runs its next
//! step between the reader's rows. Each row is then written by its own call
//! of the insert, which keeps the write's progress between rows, so a call
//! inside the row's write runs it again from the row's start.

use super::*;

/// The fixed block a driven reader, and the statement it reads for, live
/// in. A source in an arena that cannot spare it is not driven.
const DRIVEN_SOURCE_BLOCK: usize = 512 << 10;

/// An `INSERT ... SELECT` the engine writes row by row from a retained
/// reader of its source.
pub(crate) struct DrivenInsert<'a> {
    insert: &'a ast::Insert<'a>,
    reader: query::RetainedSelect<'a>,
    block: &'a Arena,
    /// The snapshot the source reads under: the write's command.
    snapshot: u32,
    /// The index of the next row to write.
    next: usize,
    /// The source ended; only the write's completion remains.
    ended: bool,
}

/// How a driven write stopped.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrivenEnd {
    /// A row returned its `RETURNING` row and the caller asked to stop there.
    Returned,
    /// The source ended and the write completed.
    Finished,
}

/// Where a driven write's statement and reader live.
#[derive(Clone, Copy)]
pub(crate) enum DrivenBlock {
    /// The statement arena's persistent tail: the reader outlives the
    /// attempt, as a data-modifying `WITH` query's steps need.
    Persistent,
    /// The statement arena's front, for one attempt.
    Front,
}

impl<'a> DrivenInsert<'a> {
    /// The block a driven write lives in, or `None` when the arena cannot
    /// spare one.
    pub(crate) fn block(arena: &'a Arena, kind: DrivenBlock) -> Option<&'a Arena> {
        match kind {
            DrivenBlock::Persistent => arena.alloc_persistent_block(DRIVEN_SOURCE_BLOCK).ok(),
            DrivenBlock::Front => {
                if arena.remaining() < 2 * DRIVEN_SOURCE_BLOCK {
                    return None;
                }
                arena.alloc_front_block(DRIVEN_SOURCE_BLOCK).ok()
            }
        }
    }

    /// Opens the driven source of `statement`, an `INSERT ... SELECT` into
    /// a table without rules, or `None` when the retained reader does not
    /// run its source. Rows before `next` were written by earlier attempts,
    /// and `progress` keeps the source values of an unfinished row `next`:
    /// the reader skips past them.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        storage: &Storage,
        txn: &TxnState,
        statement: &'a Stmt<'a>,
        block: &'a Arena,
        params: &[Datum<'a>],
        responder: &mut Responder,
        work: &Arena,
        buffers: &mut crate::storage::RowCursorBuffers,
        next: usize,
        progress: &exec::RowProgress,
    ) -> Result<Option<Self>, SqlError> {
        let skip = next + usize::from(progress.keeps_source_row(next));
        let Stmt::Insert(insert) = statement else {
            return Ok(None);
        };
        let Some(select) = insert.select else {
            return Ok(None);
        };
        let Some(crate::storage::ResolvedRelation::Table(table)) =
            storage.resolve_relation(insert.table.schema, insert.table.name, txn.txid)
        else {
            return Ok(None);
        };
        let replication_role = guc::active_session_replication_role()?;
        if storage
            .firing_rules_for(
                crate::storage::RuleTarget::Table(table as u16),
                crate::storage::RewriteEvent::Insert,
                replication_role,
                txn.txid,
            )
            .next()
            .is_some()
        {
            return Ok(None);
        }
        let snapshot = storage.read_snapshot().min(txn.command_id());
        let prior = storage.read_snapshot();
        storage.set_read_snapshot(snapshot);
        let opened = query::without_routine_invocations(|| {
            query::RetainedSelect::open_driven_source(
                storage, txn.txid, select, params, block, responder,
            )
        });
        let mut reader = match opened {
            Ok(Some(reader)) => reader,
            Ok(None) => {
                storage.set_read_snapshot(prior);
                return Ok(None);
            }
            Err(error) => {
                storage.set_read_snapshot(prior);
                return Err(error);
            }
        };
        // Its rows are written, not returned.
        reader.discard_description();
        // The rows earlier attempts wrote are read again without evaluating
        // their output; what decides which rows the source reads is
        // repeatable, so the count finds the next row.
        let skipped = (|| {
            let mut read = 0;
            while read < skip {
                let mark = work.mark();
                // The `WITH` query rows it reads again were returned already.
                if !reader.prepare_statement_row(
                    storage,
                    txn.txid,
                    datetime::now_micros(),
                    block,
                    buffers,
                    work,
                    params,
                )? {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "a resumed write selected different rows"
                    ));
                }
                let passes = query::without_routine_invocations(|| {
                    reader.evaluate_statement_predicate(
                        storage, txn.txid, work, params, None, None, work,
                    )
                })?;
                let counted = passes && !reader.skip_statement_row();
                reader.finish_statement_row(block, counted);
                // SAFETY: the skipped row's values were dropped.
                unsafe { work.rewind_to(mark) };
                read += usize::from(counted);
            }
            Ok(())
        })();
        storage.set_read_snapshot(prior);
        skipped?;
        Ok(Some(Self {
            insert,
            reader,
            block,
            snapshot,
            next,
            ended: false,
        }))
    }

    /// The source rows read so far.
    pub(crate) fn read(&self) -> usize {
        self.next
    }

    /// Detaches the reader's lifetime for a data-modifying `WITH` query that
    /// keeps it between steps.
    ///
    /// # Safety
    /// The block, the statement and the parameters the reader borrows must
    /// outlive the returned value: they live in the statement arena's
    /// persistent tail or the statement, which outlive its invocation log.
    pub(crate) unsafe fn into_kept(self) -> KeptDriven {
        KeptDriven(unsafe { core::mem::transmute::<Self, DrivenInsert<'static>>(self) })
    }
}

/// A driven reader a data-modifying `WITH` query keeps between its steps.
pub(crate) struct KeptDriven(DrivenInsert<'static>);

// SAFETY: a kept reader belongs to its statement's invocation log, which
// moves with its connection to whichever worker runs it; nothing shares it.
unsafe impl Send for KeptDriven {}

impl KeptDriven {
    /// Gives a kept reader the lifetime of the statement running it again.
    ///
    /// # Safety
    /// As for [`DrivenInsert::into_kept`].
    pub(crate) unsafe fn attach<'a>(self) -> DrivenInsert<'a> {
        unsafe { core::mem::transmute::<DrivenInsert<'static>, DrivenInsert<'a>>(self.0) }
    }
}

/// The rows one attempt of a driven row returned, beside its outcome.
type RowAttempt<'a> = (&'a [&'a [u8]], Result<(), SqlError>);

/// What a driven write does with each row it returns.
pub(crate) type ReturnedRows<'r> = dyn FnMut(&[u8]) -> Result<(), SqlError> + 'r;

impl Engine {
    /// Writes `driven`'s rows until one returns a `RETURNING` row and
    /// `stop_at_returned` asks to stop there, or until the source ends and
    /// the write completes. `writes` is the statement's invocation log:
    /// `WITH` queries the source reads run their steps under it, and each
    /// row's write takes a log of its own. Returned rows are passed to
    /// `returned` when `returning` captures them; otherwise they go to the
    /// client.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn drive_insert<'a>(
        &mut self,
        driven: &mut DrivenInsert<'a>,
        progress: &mut exec::RowProgress,
        writes: &query::RoutineInvocationState,
        mut returned: Option<&mut ReturnedRows<'_>>,
        stop_at_returned: bool,
        sub_statement: bool,
        arena: &'a Arena,
        params: &[Datum<'a>],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<DrivenEnd, SqlError>, WireFull> {
        loop {
            let row_mark = arena.mark();
            let index = driven.next;
            let values = if driven.ended {
                None
            } else {
                match progress.kept_source_row(index, arena) {
                    Ok(Some(values)) => Some(values),
                    Ok(None) => match self.read_driven_row(
                        driven, writes, arena, params, txn, sqlprep, cursors, guc, responder,
                    )? {
                        Ok(Some(values)) => Some(values),
                        Ok(None) => {
                            driven.ended = true;
                            None
                        }
                        Err(error) => {
                            if exec::is_wait(&error) {
                                // A step of a `WITH` query the source reads
                                // waited inside its own rows.
                                progress.abandon_rows();
                            }
                            return Ok(Err(error));
                        }
                    },
                    Err(error) => return Ok(Err(error)),
                }
            };
            let mut count = 0usize;
            let capture = returned.is_some();
            let outcome = self.write_driven_row(
                driven.insert,
                index,
                values,
                progress,
                writes,
                &mut |row| {
                    count += 1;
                    match returned.as_deref_mut() {
                        Some(returned) => returned(row),
                        None => Ok(()),
                    }
                },
                capture,
                sub_statement,
                arena,
                params,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
            )?;
            if let Err(error) = outcome {
                // A wait or failure inside the row: the row runs again with
                // the same source values when the write resumes.
                if let Some(values) = values
                    && let Err(kept) =
                        progress.keep_source_row(index, values, arena, &self.storage.row_work)
                {
                    return Ok(Err(kept));
                }
                return Ok(Err(error));
            }
            if values.is_none() {
                return Ok(Ok(DrivenEnd::Finished));
            }
            driven.next += 1;
            // SAFETY: the row is written and its returned rows were passed on.
            unsafe { arena.rewind_to(row_mark) };
            if stop_at_returned && count > 0 {
                return Ok(Ok(DrivenEnd::Returned));
            }
        }
    }

    /// Reads the driven source's next row: its output values, detached into
    /// `arena`, or `None` at its end. A call hands off on its row, and a
    /// data-modifying `WITH` query the source reads runs its next step.
    #[allow(clippy::too_many_arguments)]
    fn read_driven_row<'a>(
        &mut self,
        driven: &mut DrivenInsert<'a>,
        writes: &query::RoutineInvocationState,
        arena: &'a Arena,
        params: &[Datum<'a>],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<Option<&'a [Datum<'a>]>, SqlError>, WireFull> {
        let prior = self.storage.read_snapshot();
        let read = self.read_driven_row_at(
            driven, writes, arena, params, txn, sqlprep, cursors, guc, responder,
        );
        self.storage.set_read_snapshot(prior);
        read
    }

    #[allow(clippy::too_many_arguments)]
    fn read_driven_row_at<'a>(
        &mut self,
        driven: &mut DrivenInsert<'a>,
        writes: &query::RoutineInvocationState,
        arena: &'a Arena,
        params: &[Datum<'a>],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<Option<&'a [Datum<'a>]>, SqlError>, WireFull> {
        let snapshot = driven.snapshot;
        let txid = txn.txid;
        loop {
            self.storage.set_read_snapshot(snapshot);
            let next = {
                let (work, buffers) = self.work.cursor_parts();
                query::with_routine_invocation_scope(
                    Some(query::RoutineInvocationContext::new(writes, arena)),
                    || {
                        driven.reader.prepare_statement_row(
                            &self.storage,
                            txid,
                            datetime::now_micros(),
                            driven.block,
                            buffers,
                            work,
                            params,
                        )
                    },
                )
            };
            match next {
                Ok(true) => {}
                Ok(false) => return Ok(Ok(None)),
                Err(error) if error.sqlstate == sqlstate::INTERNAL_ROUTINE_INVOCATION => {
                    // A data-modifying `WITH` query's next row: its step runs
                    // and the same read runs again.
                    let Some(pending) = writes.take_pending() else {
                        return Ok(Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "a driven source yielded without a pending query step"
                        )));
                    };
                    if let Err(error) = self.complete_pending_routine(
                        pending, writes, arena, arena, txn, sqlprep, cursors, guc, responder,
                    )? {
                        return Ok(Err(error));
                    }
                    continue;
                }
                Err(error) => return Ok(Err(error)),
            }
            let reader = &driven.reader;
            let passes = self.execute_retained_expression(
                arena,
                None,
                snapshot,
                query::CatalogReads::Statement,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
                |storage, work, sequences, invocations, _cursors| {
                    reader.evaluate_statement_predicate(
                        storage,
                        txid,
                        work,
                        params,
                        Some(sequences),
                        Some(invocations),
                        arena,
                    )
                },
            )?;
            let passes = match passes {
                Ok(passes) => passes,
                Err(error) => return Ok(Err(error)),
            };
            if !passes {
                driven.reader.finish_statement_row(driven.block, false);
                continue;
            }
            let skipped = driven.reader.skip_statement_row();
            let reader = &driven.reader;
            let mut values = ArenaList::new(arena);
            for index in 0..reader.statement_items_len() {
                let item = self.execute_retained_expression(
                    arena,
                    None,
                    snapshot,
                    query::CatalogReads::Statement,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                    |storage, work, sequences, invocations, _cursors| {
                        reader.evaluate_statement_item(
                            index,
                            storage,
                            txid,
                            work,
                            params,
                            Some(sequences),
                            Some(invocations),
                            arena,
                        )
                    },
                )?;
                let item = match item {
                    Ok(item) => item,
                    Err(error) => return Ok(Err(error)),
                };
                for value in item {
                    if values.push(*value).is_err() {
                        return Ok(Err(query::arena_full_pub()));
                    }
                }
            }
            driven.reader.finish_statement_row(driven.block, !skipped);
            if skipped {
                continue;
            }
            return Ok(Ok(Some(values.as_slice())));
        }
    }

    /// Writes row `index` of a driven write with its source `values`, or
    /// completes the write when `values` is `None`: attempts of the row,
    /// each ended by a call it makes, which completes before the row runs
    /// again from its start.
    #[allow(clippy::too_many_arguments)]
    fn write_driven_row<'a>(
        &mut self,
        insert: &'a ast::Insert<'a>,
        index: usize,
        values: Option<&'a [Datum<'a>]>,
        progress: &mut exec::RowProgress,
        writes: &query::RoutineInvocationState,
        returned: &mut ReturnedRows<'_>,
        capture: bool,
        sub_statement: bool,
        arena: &'a Arena,
        params: &[Datum<'a>],
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<(), SqlError>, WireFull> {
        let start = match sequence::begin_replay_cursor() {
            Ok(start) => start,
            Err(error) => return Ok(Err(error)),
        };
        let calls = writes.write_query_child(arena);
        let snapshot = self.storage.read_snapshot();
        let attempt_mark = arena.mark();
        loop {
            // SAFETY: what an earlier attempt kept (its calls' results) lives
            // in the arena's retained tail.
            unsafe { arena.rewind_to(attempt_mark) };
            if let Err(error) = sequence::rewind_replay_cursor(start) {
                return Ok(Err(error));
            }
            calls.begin_attempt();
            let (wal, lock) = (self.wal.stage_mark(txn.txid), self.storage.lock_mark());
            let mark = txn.statement_mark(wal, lock);
            let effects = txn.transaction_effect_mark();
            progress.begin_row_attempt(wal, lock);
            progress.restart_fresh_row(mark, responder.mark());
            let (rows, outcome) = query::with_routine_invocation_scope(
                Some(query::RoutineInvocationContext::new(&calls, arena)),
                || {
                    self.insert_driven_row(
                        insert,
                        index,
                        values,
                        capture,
                        sub_statement,
                        arena,
                        params,
                        txn,
                        guc,
                        cursors,
                        responder,
                        progress,
                    )
                },
            )?;
            self.storage.set_read_snapshot(snapshot);
            let error = match outcome {
                Ok(()) => {
                    for row in rows {
                        if let Err(error) = returned(row) {
                            return Ok(Err(error));
                        }
                    }
                    return Ok(Ok(()));
                }
                Err(error) => error,
            };
            // A completion handed to the caller's attempt loop runs there.
            if error.sqlstate != sqlstate::INTERNAL_ROUTINE_INVOCATION
                || txn.pending_write_completion.is_some()
            {
                return Ok(Err(error));
            }
            if calls.take_produced() {
                for row in rows {
                    if let Err(error) = returned(row) {
                        return Ok(Err(error));
                    }
                }
                return Ok(Ok(()));
            }
            let unfinished = match progress.unfinished_row(txn) {
                Ok(unfinished) => unfinished,
                Err(error) => return Ok(Err(error)),
            };
            if unfinished.is_none() && txn.transaction_effect_mark() != effects {
                // Running the call after a rollback would hide the row's
                // earlier changes from it, unlike PostgreSQL.
                return Ok(Err(exec::retained_progress_required()));
            }
            let (rollback, output) =
                unfinished.map_or((mark, None), |row| (row.mark, Some(row.output)));
            self.rollback_waiting_statement(txn, rollback);
            if let Some(output) = output
                && let Err(error) = responder.truncate_to(output)
            {
                return Ok(Err(error));
            }
            self.dml_scratch.clear();
            let Some(pending) = calls.take_pending() else {
                return Ok(Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "a driven row yielded without a pending call"
                )));
            };
            if let Err(error) = self.complete_pending_routine(
                pending, &calls, arena, arena, txn, sqlprep, cursors, guc, responder,
            )? {
                return Ok(Err(error));
            }
        }
    }

    /// One attempt of a driven row: the insert with the row as its source,
    /// and the rows it returned when `capture` keeps them.
    #[allow(clippy::too_many_arguments)]
    fn insert_driven_row<'a>(
        &mut self,
        insert: &'a ast::Insert<'a>,
        index: usize,
        values: Option<&'a [Datum<'a>]>,
        capture: bool,
        sub_statement: bool,
        arena: &'a Arena,
        params: &[Datum<'a>],
        txn: &mut TxnState,
        guc: &GucState,
        cursors: &mut cursor::CursorPool,
        responder: &mut Responder,
        progress: &mut exec::RowProgress,
    ) -> Result<RowAttempt<'a>, WireFull> {
        let mut rows = ArenaList::new(arena);
        let mut sink =
            |_: &Storage, _: u32, _: &dyn SequenceAccess, vals: &[Datum]| -> Result<(), SqlError> {
                let encoded = exec::encode_projected_pub(vals, arena)?;
                rows.push(encoded).map_err(|_| query::arena_full_pub())
            };
        if sub_statement {
            self.storage.set_read_snapshot(txn.command_id());
        }
        // A sub-statement's foreign key checks wait for the statement's end,
        // where its main statement fires them.
        let outer = sub_statement.then(|| {
            txn.with_sub_statement
                .replace((txn.trigger_depth(), txn.command_id()))
        });
        let (work, row_buffers) = self.work.cursor_parts();
        let outcome = exec::insert(
            &mut self.storage,
            txn,
            insert,
            exec::DmlAuthorization::Invoker,
            None,
            work,
            params,
            &mut exec::TriggerServices::new(guc.seq_session(), cursors, row_buffers),
            responder,
            if capture { Some(&mut sink) } else { None },
            None,
            None,
            None,
            exec::InsertSource::Produced { index, row: values },
            Some(progress),
        );
        if let Some(outer) = outer {
            txn.with_sub_statement = outer;
        }
        let outcome = outcome?;
        Ok((rows.as_slice(), outcome))
    }
}
