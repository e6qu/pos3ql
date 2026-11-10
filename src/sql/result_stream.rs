//! Large results that resume row by row while their client reads.
//!
//! A top-level `SELECT` is armed before it runs when its shape can resume
//! from a retained read position and that position fits
//! `result_stream_bytes`. If its output first outgrows the send buffer while
//! the statement can still restart (no byte sent, no effect started), the
//! attempt is discarded and the statement reopens as a [`ResultStream`]. Every
//! other overflowing result queues in the result spool; the choice is made
//! once, before any row of the statement reaches the client.

use super::*;
use crate::pg::respond::SpoolReason;
use cursor::{ResultStream, StreamProtocol, StreamState};

/// Scope, description and plan headroom of a bound stream query beyond its
/// captured row positions.
const STREAM_PLAN_HEADROOM: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamStep {
    /// CommandComplete is queued and the stream is released.
    Complete,
    /// Delivered rows are final; the stream continues after this wait.
    Wait(StatementWait),
    /// An ErrorResponse follows the delivered rows; the stream is released
    /// and the transaction rolled back (implicit) or failed (explicit).
    Failed,
}

/// Counters for the operations metrics endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResultStreamMetrics {
    pub(crate) opened: u64,
    pub(crate) spooled_shape: u64,
    pub(crate) spooled_capacity: u64,
    pub(crate) spooled_effects: u64,
}

fn stream_capacity(error: SqlError) -> SqlError {
    if error.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "result stream exceeds result_stream_bytes: {}",
            error.message.as_str()
        )
    } else {
        error
    }
}

fn wait_kind(error: &SqlError) -> Option<StatementWait> {
    if error.sqlstate == sqlstate::INTERNAL_IO_WAIT {
        Some(StatementWait::ObjectIo)
    } else if error.sqlstate == sqlstate::INTERNAL_LOCK_WAIT {
        Some(StatementWait::Lock)
    } else {
        None
    }
}

impl Engine {
    /// Decides before a top-level statement runs whether its output may
    /// resume as a result stream.
    pub(super) fn result_stream_choice(
        &self,
        statement: &Stmt<'_>,
        typed_inputs: bool,
        txid: u32,
        source_len: usize,
    ) -> Result<(), SpoolReason> {
        let Stmt::Select(select) = statement else {
            return Err(SpoolReason::Shape);
        };
        if typed_inputs {
            return Err(SpoolReason::Shape);
        }
        if self.result_stream_bytes == 0 {
            return Err(SpoolReason::Capacity);
        }
        let mark = self.work.mark();
        let table =
            query::RetainedSelect::stream_source_table(&self.storage, txid, select, &self.work);
        // The probe resolved names in scratch memory only.
        unsafe { self.work.rewind_to(mark) };
        let Ok(Some(table)) = table else {
            return Err(SpoolReason::Shape);
        };
        // Conservative: every resident entry and every saved generation, plus
        // the copied text, its parse tree and the bound scope.
        let estimate = crate::storage::RowCursor::position_bytes(
            self.storage.resident_row_entries(table),
            self.max_spill_generations_per_table,
        )
        .and_then(|positions| positions.checked_add(source_len.saturating_mul(16)))
        .and_then(|bytes| bytes.checked_add(STREAM_PLAN_HEADROOM));
        match estimate {
            Some(bytes) if bytes <= self.result_stream_bytes => Ok(()),
            _ => Err(SpoolReason::Capacity),
        }
    }

    pub(super) fn record_result_spool(&mut self, reason: SpoolReason) {
        let counter = match reason {
            SpoolReason::Shape => 0,
            SpoolReason::Capacity => 1,
            SpoolReason::Effects => 2,
        };
        self.result_streams_spooled[counter] =
            self.result_streams_spooled[counter].saturating_add(1);
    }

    pub(crate) fn result_stream_metrics(&self) -> ResultStreamMetrics {
        ResultStreamMetrics {
            opened: self.result_streams_opened,
            spooled_shape: self.result_streams_spooled[0],
            spooled_capacity: self.result_streams_spooled[1],
            spooled_effects: self.result_streams_spooled[2],
        }
    }

    /// Reopens a promoted statement as a stream: copies its text and
    /// parameters into the stream allocation, captures the read position and
    /// sends its RowDescription (extended Execute suppresses it).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_result_stream(
        &mut self,
        stream: &mut ResultStream,
        protocol: StreamProtocol,
        sql: &str,
        params: &[Datum],
        parameter_types: &[i32],
        owner: type_input_plans::Owner,
        deadline: i64,
        txn: &mut TxnState,
        guc: &GucState,
        responder: &mut Responder,
        conn_id: i32,
    ) -> Result<(), SqlError> {
        stream.release();
        let txid = txn.txid;
        let source_snapshot = txn.command_id().saturating_add(1);
        let _guc_eval_scope = guc::enter_eval_scope(guc, txn);
        let opened = exec::with_bound_parameter_types(parameter_types, || {
            stream
                .query
                .bind(sql, params, &[], &[], owner, |parser| {
                    type_inputs::validate_restored(parser, &self.storage, txid)
                })
                .map_err(stream_capacity)?;
            self.storage.set_read_snapshot(source_snapshot);
            responder.set_render(guc.render());
            stream
                .query
                .prepare_source(&self.storage, txid, responder)
                .map_err(stream_capacity)
        });
        match opened {
            Ok(true) => {}
            Ok(false) => {
                stream.release();
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "result stream source no longer retains its read position"
                ));
            }
            Err(error) => {
                stream.release();
                return Err(error);
            }
        }
        stream.state = Some(StreamState {
            connection: conn_id,
            txid,
            command: txn.command_id(),
            protocol,
            source_snapshot,
            deadline,
            emitted: 0,
            resumed: false,
        });
        self.result_streams_opened = self.result_streams_opened.saturating_add(1);
        Ok(())
    }

    /// Delivers rows until the client is behind, a row waits for storage or
    /// a lock, the result ends, or it fails.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn continue_result_stream(
        &mut self,
        stream: &mut ResultStream,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        conn_id: i32,
        lock_timeout_expired: bool,
    ) -> Result<StreamStep, WireFull> {
        let Some(mut state) = stream.state else {
            return self.fail_result_stream(
                stream,
                sql_err!(sqlstate::INTERNAL_ERROR, "no result stream is open"),
                txn,
                cursors,
                guc,
                responder,
            );
        };
        if state.connection != conn_id
            || state.txid != txn.txid
            || state.command != txn.command_id()
        {
            return self.fail_result_stream(
                stream,
                sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "result stream resumed outside its statement"
                ),
                txn,
                cursors,
                guc,
                responder,
            );
        }
        txn.begin_statement_clock();
        txn.restore_statement_deadline(state.deadline);
        let outcome = (|| {
            let _configuration_reload_scope = ConfigurationReloadScope::new(self, guc);
            let _guc_eval_scope = guc::enter_eval_scope(guc, txn);
            if let Err(error) = self.enter_stream_environment(guc, txn, responder) {
                return Ok(Err(error));
            }
            match stream.query.result_type_oids() {
                Ok(Some(types)) => responder.restore_result_types(types),
                Ok(None) => {}
                Err(error) => return Ok(Err(error)),
            }
            let owner = stream.query.owner();
            type_input_plans::with_owner(owner, || {
                self.deliver_stream_rows(
                    stream,
                    &mut state,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                    lock_timeout_expired,
                )
            })
        })()?;
        match outcome {
            Ok(None) => {
                let tag = stack_format!(48, "SELECT {}", state.emitted);
                stream.release();
                txn.release_suspended_statement();
                responder.command_complete(tag.as_str())?;
                Ok(StreamStep::Complete)
            }
            Ok(Some(wait)) => {
                state.resumed = true;
                stream.state = Some(state);
                if txn.suspended_statement_context().is_none() {
                    txn.suspend_statement(
                        None,
                        datetime::statement_clock(),
                        state.deadline,
                        exec::PlpgsqlTransactionContext::Atomic,
                    );
                }
                Ok(StreamStep::Wait(wait))
            }
            Err(error) => self.fail_result_stream(stream, error, txn, cursors, guc, responder),
        }
    }

    /// Ends a stream with an error after its delivered rows.
    pub(crate) fn fail_result_stream(
        &mut self,
        stream: &mut ResultStream,
        error: SqlError,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<StreamStep, WireFull> {
        stream.release();
        txn.release_suspended_statement();
        sequence::finish_statement(guc.seq_session());
        if error.sqlstate == sqlstate::DEADLOCK_DETECTED {
            self.storage.record_deadlock();
        }
        if txn.is_explicit() && error.sqlstate == sqlstate::DEADLOCK_DETECTED {
            self.abort_explicit_txn(txn, guc);
        } else if txn.is_explicit() {
            txn.failed = true;
        } else {
            cursors.on_rollback();
            self.rollback_txn(txn, guc);
        }
        responder.error(error.sqlstate, error.message.as_str())?;
        Ok(StreamStep::Failed)
    }

    /// Installs the session environment a statement sees, as statement
    /// execution does, for rows evaluated on a later reactor turn.
    fn enter_stream_environment(
        &mut self,
        guc: &GucState,
        txn: &TxnState,
        responder: &mut Responder,
    ) -> Result<(), SqlError> {
        self.refresh_prepared_transaction_catalog()?;
        let _ = eval::take_diagnostic();
        exec::reset_record_shapes();
        for (_, composite) in self.storage.composites_with_slots_visible_to(txn.txid) {
            exec::register_named_composite_shape(
                Some(composite.creation()),
                composite.name.as_str(),
                composite.fields(),
                &self.storage,
                txn.txid,
            )?;
        }
        let database = self
            .storage
            .database_slot_by_oid(self.storage.current_database_oid(), txn.txid)
            .expect("selected database remains visible");
        let database_name = self.storage.database_definition(database, txn.txid).name;
        eval::funcs::system::set_current_database(database_name.as_str());
        guc.refresh_identities(&self.storage, txn.txid);
        eval::funcs::system::set_session_user(guc.session_identity());
        let current_role = guc.current_role();
        eval::funcs::system::set_current_user(guc.current_identity());
        self.publish_session_context(guc, txn, current_role.as_str())?;
        timezone::set_session(guc.timezone());
        responder.set_render(guc.render());
        Ok(())
    }

    /// Returns `None` at the end of the result, or the wait that stopped it.
    #[allow(clippy::too_many_arguments)]
    fn deliver_stream_rows(
        &mut self,
        stream: &mut ResultStream,
        state: &mut StreamState,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        lock_timeout_expired: bool,
    ) -> Result<Result<Option<StatementWait>, SqlError>, WireFull> {
        let mut row_attempted = state.resumed;
        loop {
            if let Err(error) = query::check_timeout() {
                return Ok(Err(error));
            }
            // Rows already queued stay final; evaluate another only once the
            // client has room for it.
            if responder.output_backlogged() {
                return Ok(Ok(Some(StatementWait::ClientOutput)));
            }
            // Effects end with their row: a later row's reads yield again.
            let row_mark =
                txn.statement_mark(self.wal.stage_mark(txn.txid), self.storage.lock_mark());
            let effect_reads = self
                .ckpt
                .as_ref()
                .map(Checkpointer::deferred_atomic_block_read_scope);
            let _row_effects = query::enter_statement_read_effect_scope(effect_reads.as_ref());
            let sequence_scope = sequence::enter_statement(
                arena,
                guc.seq_session(),
                row_attempted,
                Some(self.storage.advisory_journal()),
            );
            if let Err(error) = self.storage.begin_advisory_statement() {
                return Ok(Err(error));
            }
            let row = self.advance_stream_row(
                &mut stream.query,
                state.source_snapshot,
                arena,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
            )?;
            match row {
                Ok(Some(emitted)) => {
                    if let Err(error) = sequence_scope.finish(guc.seq_session()) {
                        return Ok(Err(error));
                    }
                    row_attempted = false;
                    state.emitted += u64::from(emitted);
                }
                Ok(None) => {
                    if let Err(error) = sequence_scope.finish(guc.seq_session()) {
                        return Ok(Err(error));
                    }
                    return Ok(Ok(None));
                }
                Err(error) => {
                    let Some(wait) = wait_kind(&error) else {
                        sequence_scope.discard(guc.seq_session());
                        return Ok(Err(error));
                    };
                    if wait == StatementWait::Lock && lock_timeout_expired {
                        sequence_scope.discard(guc.seq_session());
                        self.storage.rollback_locks_to(txn.txid, row_mark.lock);
                        return Ok(Err(sql_err!(
                            sqlstate::LOCK_NOT_AVAILABLE,
                            "canceling statement due to lock timeout"
                        )));
                    }
                    if let Err(error) = sequence::ensure_statement_effects_replayable() {
                        sequence_scope.discard(guc.seq_session());
                        return Ok(Err(error));
                    }
                    // Only this row's partial work rolls back; its receipts
                    // replay when the row is evaluated again.
                    self.storage.preserve_advisory_statement();
                    self.rollback_waiting_statement(txn, row_mark);
                    state.resumed = true;
                    return Ok(Ok(Some(wait)));
                }
            }
        }
    }

    /// Evaluates the next row of a stream. `Ok(Some(true))` emitted a row,
    /// `Ok(Some(false))` consumed a row the WHERE clause or OFFSET skipped,
    /// and `Ok(None)` is the end of the result.
    #[allow(clippy::too_many_arguments)]
    fn advance_stream_row(
        &mut self,
        stream: &mut cursor::CursorQuery,
        source_snapshot: u32,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<Option<bool>, SqlError>, WireFull> {
        self.storage.set_read_snapshot(source_snapshot);
        let ready = {
            let (work, buffers) = self.work.cursor_parts();
            stream.prepare_row(
                &self.storage,
                txn.txid,
                datetime::now_micros(),
                work,
                buffers,
            )
        };
        match ready {
            Ok(true) => {}
            Ok(false) => return Ok(Ok(None)),
            Err(error) => return Ok(Err(error)),
        }
        let txid = txn.txid;
        let row = arena.with_remaining_region(|frame| {
            let source = &*stream;
            let passes = self.execute_retained_expression(
                frame,
                None,
                source_snapshot,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
                |storage, work, sequences, invocations, _| {
                    source.evaluate_predicate(
                        storage,
                        txid,
                        work,
                        Some(sequences),
                        invocations,
                        frame,
                    )
                },
            )?;
            match passes {
                Ok(true) => {}
                Ok(false) => return Ok(Ok(false)),
                Err(error) => return Ok(Err(error)),
            }
            let mut projected = crate::mem::arena::ArenaList::new(frame);
            for index in 0..source.items_len() {
                let values = self.execute_retained_expression(
                    frame,
                    None,
                    source_snapshot,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                    |storage, work, sequences, invocations, _| {
                        source.evaluate_item(
                            index,
                            storage,
                            txid,
                            work,
                            Some(sequences),
                            invocations,
                            frame,
                        )
                    },
                )?;
                let values = match values {
                    Ok(values) => values,
                    Err(error) => return Ok(Err(error)),
                };
                for value in values {
                    if projected.len() == MAX_PROJ {
                        return Ok(Err(sql_err!(
                            sqlstate::TOO_MANY_COLUMNS,
                            "target lists can have at most 1664 entries"
                        )));
                    }
                    if projected.push(*value).is_err() {
                        return Ok(Err(eval::arena_full()));
                    }
                }
            }
            if stream.skip_row() {
                return Ok(Ok(false));
            }
            self.storage.set_read_snapshot(source_snapshot);
            // The row reaches send or the private reserve whole; a wait can
            // no longer re-evaluate it.
            match query::emit_data_row(&self.storage, txid, frame, responder, projected.as_slice())?
            {
                Ok(()) => Ok(Ok(true)),
                Err(error) => Ok(Err(error)),
            }
        });
        let emitted = match row {
            Ok(result) => result?,
            Err(error) => {
                return Ok(Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "result stream row exceeds statement memory: {}",
                    error
                )));
            }
        };
        let emitted = match emitted {
            Ok(emitted) => emitted,
            Err(error) => return Ok(Err(error)),
        };
        stream.finish_row(emitted);
        Ok(Ok(Some(emitted)))
    }
}
