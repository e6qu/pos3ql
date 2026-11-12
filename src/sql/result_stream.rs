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
use cursor::{FetchStream, ResultStream, StreamProtocol, StreamState};

/// Scope, description and plan headroom of a bound stream query beyond its
/// captured row positions.
const STREAM_PLAN_HEADROOM: usize = 32 * 1024;

/// How a top-level statement's output may resume as a result stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamChoice {
    /// A `SELECT` reopens as a stream if its output first overflows.
    Promote,
    /// The statement opens its stream before it runs. A `UNION ALL`
    /// statement always does: the general executor's attempt would materialize every query
    /// before its first row could overflow. In object-store mode a join,
    /// grouping or sort does too: its attempt would read a whole input
    /// before its first row, and again after each storage wait, so a cold
    /// input larger than the block cache would never finish.
    Open,
    /// A FETCH streams its cursor's rows from the start: they are evaluated
    /// as the client reads them and never saved in `cursor_bytes`.
    Fetch(FetchStream),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamStep {
    /// CommandComplete is queued and the stream is released.
    Complete,
    /// The Execute's row limit is reached: PortalSuspended is queued and
    /// the stream holds its position until the portal's next Execute.
    Suspended,
    /// Delivered rows are final; the stream continues after this wait.
    Wait(StatementWait),
    /// An ErrorResponse follows the delivered rows; the stream is released
    /// and the transaction rolled back (implicit) or failed (explicit).
    Failed,
    /// The stream's sort or hash table did not fit before anything of it
    /// was queued: the stream is released and the statement must run again
    /// with its output spooled for `Capacity`.
    Respool,
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

/// Whether a stream that failed to open may run its statement spooled
/// instead: it queued nothing, and only a capacity of the stream itself
/// (`result_stream_bytes`, its opening messages, or a build in
/// `plan_build_bytes`) stopped it, which the spooled result does not need.
pub(crate) fn stream_open_respools(error: &SqlError) -> bool {
    error.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED
}

/// Why row delivery stopped without an error.
enum DeliveryStop {
    End,
    PageFull,
    Wait(StatementWait),
    /// See [`StreamStep::Respool`].
    Respool,
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
    fn result_stream_choice(
        &self,
        statement: &Stmt<'_>,
        typed_inputs: bool,
        txid: u32,
        source_len: usize,
        cursors: &cursor::CursorPool,
    ) -> Result<StreamChoice, SpoolReason> {
        if let Stmt::FetchCursor {
            name,
            motion,
            move_only,
        } = statement
        {
            let slot = cursors
                .stream_fetch_slot(name, *motion, *move_only)
                .ok_or(SpoolReason::Shape)?;
            let limit = match motion {
                cursor::FetchMotion::Count(count) => Some(*count as u64),
                _ => None,
            };
            return Ok(StreamChoice::Fetch(FetchStream { slot, limit }));
        }
        if typed_inputs {
            return Err(SpoolReason::Shape);
        }
        if self.result_stream_bytes == 0 {
            return Err(SpoolReason::Capacity);
        }
        let mark = self.work.mark();
        // A UNION ALL statement, as opposed to a UNION ALL in FROM.
        let mut set_query = false;
        let (source, source_len) = match statement {
            Stmt::Select(select) => (
                query::stream_query(select, &self.storage, txid, &self.work).and_then(|stream| {
                    set_query = select.set_body.is_some();
                    stream.map_or(Ok(None), |select| {
                        query::RetainedSelect::stream_source(
                            &self.storage,
                            txid,
                            select,
                            &self.work,
                        )
                    })
                }),
                source_len,
            ),
            Stmt::SetQuery(set) => (
                query::set_query_select(set, &self.work)
                    .and_then(|select| query::stream_query(select, &self.storage, txid, &self.work))
                    .and_then(|select| {
                        set_query = true;
                        select.map_or(Ok(None), |select| {
                            query::RetainedSelect::stream_source(
                                &self.storage,
                                txid,
                                select,
                                &self.work,
                            )
                        })
                    }),
                source_len,
            ),
            // COPY TO STDOUT streams the query it reads.
            Stmt::Copy(copy) if copy.to => {
                match self
                    .copy_query_text(copy, txid, &self.work)
                    .and_then(|text| {
                        let select = crate::sql::parser::parse_query(text, &self.work)?;
                        set_query = select.set_body.is_some();
                        let Some(select) =
                            query::stream_query(select, &self.storage, txid, &self.work)?
                        else {
                            return Ok((None, text.len()));
                        };
                        query::RetainedSelect::stream_source(
                            &self.storage,
                            txid,
                            select,
                            &self.work,
                        )
                        .map(|source| (source, text.len()))
                    }) {
                    Ok((source, len)) => (Ok(source), source_len.max(len)),
                    Err(error) => (Err(error), source_len),
                }
            }
            _ => (Ok(None), source_len),
        };
        // The probe resolved names in scratch memory only.
        unsafe { self.work.rewind_to(mark) };
        let opens = match source {
            Ok(Some(query::StreamSource::Appended(_))) => set_query,
            Ok(Some(query::StreamSource::Table { builds, .. })) => {
                builds && self.storage.spill_attached()
            }
            Ok(Some(query::StreamSource::Join(_))) => self.storage.spill_attached(),
            _ => false,
        };
        let positions = match source {
            // Conservative: every resident entry and every saved generation.
            Ok(Some(query::StreamSource::Table { table, .. })) => {
                crate::storage::RowCursor::position_bytes(
                    self.storage.resident_row_entries(table),
                    self.max_spill_generations_per_table,
                )
            }
            // A generator retains only its next value.
            Ok(Some(query::StreamSource::Generated)) => Some(0),
            Ok(Some(query::StreamSource::Join(tables) | query::StreamSource::Appended(tables))) => {
                tables.iter().flatten().try_fold(0usize, |total, &table| {
                    crate::storage::RowCursor::position_bytes(
                        self.storage.resident_row_entries(table),
                        self.max_spill_generations_per_table,
                    )
                    .and_then(|bytes| total.checked_add(bytes))
                })
            }
            _ => return Err(SpoolReason::Shape),
        };
        // Plus the copied text, its parse tree and the bound scope.
        let estimate = positions
            .and_then(|positions| positions.checked_add(source_len.saturating_mul(16)))
            .and_then(|bytes| bytes.checked_add(STREAM_PLAN_HEADROOM));
        match estimate {
            Some(bytes) if bytes <= ResultStream::query_bytes(self.result_stream_bytes) => {
                Ok(if opens {
                    StreamChoice::Open
                } else {
                    StreamChoice::Promote
                })
            }
            _ => Err(SpoolReason::Capacity),
        }
    }

    /// Arms the statement about to run for promotion, requests its FETCH
    /// stream, or declines with the reason its output would spool. A
    /// statement re-run because its stream could not build spools for
    /// `Capacity`.
    pub(super) fn arm_result_stream(
        &self,
        statement: &Stmt<'_>,
        typed_inputs: bool,
        txid: u32,
        source_len: usize,
        cursors: &cursor::CursorPool,
        responder: &mut Responder,
    ) -> Option<FetchStream> {
        if !responder.promotion_allowed() {
            return None;
        }
        if responder.take_respool() {
            responder.decline_promotion(SpoolReason::Capacity);
            return None;
        }
        match self.result_stream_choice(statement, typed_inputs, txid, source_len, cursors) {
            Ok(StreamChoice::Promote) => {
                if !responder.request_page_stream() {
                    responder.arm_promotion();
                }
                None
            }
            Ok(StreamChoice::Open) => {
                if !responder.request_page_stream() && !responder.request_stream() {
                    responder.arm_promotion();
                }
                None
            }
            Ok(StreamChoice::Fetch(fetch)) => responder.request_stream().then_some(fetch),
            Err(reason) => {
                responder.decline_promotion(reason);
                None
            }
        }
    }

    /// The query a `COPY ... TO STDOUT` reads: its own, or its table's
    /// target columns from only that table. Table checks run first.
    fn copy_query_text<'a>(
        &self,
        copy: &ast::CopyStmt<'a>,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<&'a str, SqlError> {
        if let Some(sql) = copy.query {
            return Ok(sql);
        }
        let setup = exec::copy_begin(&self.storage, copy, txid)?;
        exec::copy_select_text(&self.storage, &setup, txid, arena)
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
    /// sends its RowDescription (extended Execute suppresses it). A paged
    /// portal passes its Execute's row limit.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open_result_stream(
        &mut self,
        stream: &mut ResultStream,
        protocol: StreamProtocol,
        page_limit: Option<u64>,
        fetch: Option<FetchStream>,
        copy: bool,
        sql: &str,
        params: &[Datum],
        parameter_types: &[i32],
        owner: type_input_plans::Owner,
        deadline: i64,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        conn_id: i32,
    ) -> Result<(), SqlError> {
        stream.release(cursors);
        if let Some(fetch) = fetch {
            return self.open_fetch_stream(
                stream, protocol, fetch, deadline, txn, cursors, responder, conn_id,
            );
        }
        let txid = txn.txid;
        let source_snapshot = txn.command_id().saturating_add(1);
        let _guc_eval_scope = guc::enter_eval_scope(guc, txn);
        let work_mark = self.work.mark();
        let query = &mut stream.query;
        let header = &mut stream.header;
        header.clear();
        let opened = exec::with_bound_parameter_types(parameter_types, || {
            // A COPY streams the query it reads, formatted as COPY data.
            let mut copy_format = None;
            let query_text = if copy {
                let mut parser = crate::sql::parser::Parser::new(sql, &self.work)
                    .map_err(|error| parse_error_to_sql(&error))?;
                let Some(Stmt::Copy(statement)) = parser
                    .next_stmt()
                    .map_err(|error| parse_error_to_sql(&error))?
                else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "result stream COPY statement changed"
                    ));
                };
                self.storage.set_read_snapshot(source_snapshot);
                let text = self.copy_query_text(&statement, txid, &self.work)?;
                let mut columns = [types::ColDesc::new("", 0, 0); MAX_PROJ];
                let count =
                    query::describe_query(text, &self.storage, txid, &self.work, &mut columns)?;
                let mut names = [""; MAX_PROJ];
                for (name, column) in names.iter_mut().zip(&columns[..count]) {
                    *name = column.name;
                }
                let format = exec::copy_fmt_for_columns(&names[..count], &statement.options)?;
                copy_format = Some((format, names, count));
                text
            } else {
                sql
            };
            query
                .bind(query_text, params, &[], &[], owner, |parser| {
                    type_inputs::validate_restored(parser, &self.storage, txid)
                })
                .map_err(stream_capacity)?;
            self.storage.set_read_snapshot(source_snapshot);
            responder.set_render(guc.render());
            // The opening messages wait in the header until the first row is
            // ready, so a sort or hash table that cannot be built leaves
            // nothing queued and the statement can still spool.
            responder.capture(header, |capture| {
                let Some((format, names, count)) = copy_format else {
                    return query
                        .prepare_source(&self.storage, txid, capture, query::RetainedUse::Stream)
                        .map(|ready| (ready, None))
                        .map_err(stream_capacity);
                };
                let ready = capture
                    .without_row_description(|capture| {
                        query.prepare_source(
                            &self.storage,
                            txid,
                            capture,
                            query::RetainedUse::Stream,
                        )
                    })
                    .map_err(stream_capacity)?;
                exec::begin_copy_out(&format, &names[..count], capture).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "result stream opening messages exceed {} bytes",
                        cursor::STREAM_HEADER_BYTES
                    )
                })?;
                Ok((ready, Some(format)))
            })
        });
        // The COPY statement and its query text were scratch.
        unsafe { self.work.rewind_to(work_mark) };
        let copy_format = match opened {
            Ok((true, format)) => format,
            Ok((false, _)) => {
                stream.release(cursors);
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "result stream source no longer retains its read position"
                ));
            }
            Err(error) => {
                stream.release(cursors);
                return Err(error);
            }
        };
        stream.state = Some(StreamState {
            connection: conn_id,
            txid,
            command: txn.command_id(),
            protocol,
            source_snapshot,
            deadline,
            emitted: 0,
            page_limit,
            suspended: false,
            fetch: None,
            copy: copy_format,
            resumed: false,
            header_pending: true,
        });
        cursors.hold_stream_tables(stream.index(), stream.source_tables());
        Ok(())
    }

    /// Opens a FETCH stream over its cursor's retained query: sends the
    /// cursor's RowDescription (extended Execute suppresses it) and delivers
    /// rows from the cursor's current position.
    #[allow(clippy::too_many_arguments)]
    fn open_fetch_stream(
        &mut self,
        stream: &mut ResultStream,
        protocol: StreamProtocol,
        fetch: FetchStream,
        deadline: i64,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        responder: &mut Responder,
        conn_id: i32,
    ) -> Result<(), SqlError> {
        let name = SqlName::parse(cursors.slot_name(fetch.slot))
            .map_err(|_| sql_err!(sqlstate::INTERNAL_ERROR, "streamed cursor name is invalid"))?;
        let (description, formats) = cursors
            .fetch_description(name.as_str(), responder.result_formats())
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_CURSOR,
                    "cursor \"{}\" does not exist",
                    name.as_str()
                )
            })?;
        let mut type_oids = [0i32; crate::pg::respond::MAX_RESULT_COLS];
        let columns = cursor::description_type_oids(description, &mut type_oids)?;
        for (index, oid) in type_oids[..columns].iter().enumerate() {
            if !formats.is_binary(index) {
                continue;
            }
            match exec::binary_output_capability(&self.storage, *oid, txn.txid) {
                exec::BinaryOutputCapability::Supported
                | exec::BinaryOutputCapability::MissingAclItemElementSend => {}
                exec::BinaryOutputCapability::MissingAclItemSend => {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "no binary output function available for type aclitem"
                    ));
                }
                exec::BinaryOutputCapability::Unsupported => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "binary output is not supported for type oid {}",
                        oid
                    ));
                }
            }
        }
        let mut formatted = responder.result_format_scope(formats);
        formatted
            .cursor_row_description(description, formats)
            .map_err(|_| super::cursor_description_too_large())?;
        drop(formatted);
        cursors.begin_streamed_fetch(fetch.slot);
        stream.state = Some(StreamState {
            connection: conn_id,
            txid: txn.txid,
            command: txn.command_id(),
            protocol,
            source_snapshot: self.storage.read_snapshot(),
            deadline,
            emitted: 0,
            page_limit: fetch.limit,
            suspended: false,
            fetch: Some(fetch),
            copy: None,
            resumed: false,
            header_pending: false,
        });
        self.result_streams_opened = self.result_streams_opened.saturating_add(1);
        Ok(())
    }

    /// Starts the next Execute of a suspended portal: a new statement with
    /// its own row limit and deadline over the portal's read position and
    /// snapshot. Returns an error (and keeps the stream) when the portal
    /// cannot run, as PostgreSQL does in a failed transaction.
    pub(crate) fn resume_portal_stream(
        &mut self,
        stream: &mut ResultStream,
        page_limit: Option<u64>,
        text: &str,
        txn: &TxnState,
        guc: &GucState,
        conn_id: i32,
    ) -> Result<(), SqlError> {
        let state = stream
            .state
            .as_mut()
            .filter(|state| state.suspended)
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "portal stream is not suspended"))?;
        if txn.failed {
            return Err(sql_err!(
                sqlstate::IN_FAILED_SQL_TRANSACTION,
                "current transaction is aborted, commands ignored until end of transaction block"
            ));
        }
        if state.connection != conn_id || state.txid != txn.txid {
            return Err(sql_err!(
                sqlstate::SERIALIZATION_FAILURE,
                "result stream resumed outside its transaction"
            ));
        }
        txn.begin_statement_clock();
        state.command = txn.command_id();
        state.deadline = query::statement_deadline();
        state.emitted = 0;
        state.page_limit = page_limit;
        state.suspended = false;
        state.resumed = false;
        let application_name = guc.get_owned("application_name").unwrap_or_default();
        let backend_xid = self
            .storage
            .assigned_transaction_identity(txn.txid)
            .map(|xid| xid as u32);
        self.storage.begin_backend_statement(
            conn_id,
            text,
            application_name.as_str(),
            datetime::transaction_micros(),
            backend_xid,
        );
        Ok(())
    }

    /// Delivers rows until the client is behind, a row waits for storage or
    /// a lock, the result ends, or it fails.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn continue_result_stream(
        &mut self,
        stream: &mut ResultStream,
        arena: &mut Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        conn_id: i32,
        lock_timeout_expired: bool,
    ) -> Result<StreamStep, WireFull> {
        let Some(mut state) = stream.state.filter(|state| !state.suspended) else {
            return self.fail_result_stream(
                Some(stream),
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
                Some(stream),
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
            if let Some(fetch) = state.fetch {
                // A FETCH's rows take its cursor's types and formats.
                let formats = {
                    let name = cursors.slot_name(fetch.slot);
                    let Some((description, formats)) =
                        cursors.fetch_description(name, responder.result_formats())
                    else {
                        return Ok(Err(sql_err!(
                            sqlstate::UNDEFINED_CURSOR,
                            "cursor \"{}\" does not exist",
                            name
                        )));
                    };
                    let mut types = [0i32; crate::pg::respond::MAX_RESULT_COLS];
                    match cursor::description_type_oids(description, &mut types) {
                        Ok(columns) => responder.restore_result_types(&types[..columns]),
                        Err(error) => return Ok(Err(error)),
                    }
                    formats
                };
                let owner = match cursors.plan_owner(fetch.slot) {
                    Ok(owner) => owner,
                    Err(error) => return Ok(Err(error)),
                };
                let mut formatted = responder.result_format_scope(formats);
                return type_input_plans::with_owner(owner, || {
                    self.deliver_stream_rows(
                        stream,
                        &mut state,
                        arena,
                        txn,
                        sqlprep,
                        cursors,
                        guc,
                        &mut formatted,
                        lock_timeout_expired,
                    )
                });
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
        let outcome = match (outcome, state.fetch) {
            // A FETCH's count ends it like the end of its rows, but leaves
            // the cursor on its last row.
            (Ok(DeliveryStop::PageFull), Some(_)) => Ok(DeliveryStop::End),
            (Ok(DeliveryStop::End), Some(fetch)) => {
                cursors.finish_streamed_fetch(fetch.slot);
                Ok(DeliveryStop::End)
            }
            (outcome, _) => outcome,
        };
        match outcome {
            Ok(DeliveryStop::End) => {
                let tag = if state.fetch.is_some() {
                    stack_format!(48, "FETCH {}", state.emitted)
                } else if state.copy.is_some() {
                    stack_format!(48, "COPY {}", state.emitted)
                } else {
                    stack_format!(48, "SELECT {}", state.emitted)
                };
                stream.release(cursors);
                txn.release_suspended_statement();
                if let Some(format) = state.copy {
                    exec::finish_copy_out(&format, responder)?;
                }
                responder.command_complete(tag.as_str())?;
                Ok(StreamStep::Complete)
            }
            Ok(DeliveryStop::PageFull) => {
                // No statement runs between Executes: the portal keeps only
                // its read position and snapshot.
                state.suspended = true;
                state.resumed = false;
                stream.state = Some(state);
                txn.release_suspended_statement();
                responder.portal_suspended()?;
                Ok(StreamStep::Suspended)
            }
            Ok(DeliveryStop::Respool) => {
                stream.release(cursors);
                txn.release_suspended_statement();
                sequence::finish_statement(guc.seq_session());
                Ok(StreamStep::Respool)
            }
            Ok(DeliveryStop::Wait(wait)) => {
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
            Err(error) => {
                if let Some(fetch) = state.fetch {
                    // PostgreSQL marks a portal that failed during a FETCH.
                    let name = SqlName::parse(cursors.slot_name(fetch.slot))
                        .expect("an open cursor's name is valid");
                    cursors.fail(name.as_str());
                }
                self.fail_result_stream(Some(stream), error, txn, cursors, guc, responder)
            }
        }
    }

    /// Ends a stream with an error after its delivered rows.
    pub(crate) fn fail_result_stream(
        &mut self,
        stream: Option<&mut ResultStream>,
        error: SqlError,
        txn: &mut TxnState,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<StreamStep, WireFull> {
        if let Some(stream) = stream {
            stream.release(cursors);
        }
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

    /// Delivers rows until the result ends, the Execute's row limit is
    /// reached, or a wait stops it.
    #[allow(clippy::too_many_arguments)]
    fn deliver_stream_rows(
        &mut self,
        stream: &mut ResultStream,
        state: &mut StreamState,
        arena: &mut Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
        lock_timeout_expired: bool,
    ) -> Result<Result<DeliveryStop, SqlError>, WireFull> {
        let mut row_attempted = state.resumed;
        loop {
            if let Err(error) = query::check_timeout() {
                return Ok(Err(error));
            }
            // A paged portal evaluates no row beyond its Execute's limit.
            if state.page_limit.is_some_and(|limit| state.emitted >= limit) {
                return Ok(Ok(DeliveryStop::PageFull));
            }
            // Rows already queued stay final; evaluate another only once the
            // client has room for it.
            if responder.output_backlogged() {
                return Ok(Ok(DeliveryStop::Wait(StatementWait::ClientOutput)));
            }
            // A delivered row's receipts are never replayed: each new row
            // starts with empty statement memory, so a long stream cannot
            // accumulate them. Resetting also invalidates every handle into
            // the finished row.
            if !row_attempted {
                arena.reset();
            }
            let arena = &*arena;
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
            let row = match state.fetch {
                Some(fetch) => self.advance_fetch_row(
                    fetch.slot,
                    state.source_snapshot,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )?,
                None => self.advance_stream_row(
                    &mut stream.query,
                    &stream.header,
                    &mut state.header_pending,
                    state.copy.as_ref(),
                    state.source_snapshot,
                    arena,
                    txn,
                    sqlprep,
                    cursors,
                    guc,
                    responder,
                )?,
            };
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
                    return Ok(Ok(DeliveryStop::End));
                }
                Err(error) => {
                    let Some(wait) = wait_kind(&error) else {
                        sequence_scope.discard(guc.seq_session());
                        if state.header_pending {
                            // Only the source's builds ran: no row, effect or
                            // byte of this stream exists yet.
                            if error.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED {
                                self.rollback_waiting_statement(txn, row_mark);
                                return Ok(Ok(DeliveryStop::Respool));
                            }
                            // PostgreSQL describes the result before its error.
                            responder.raw(stream.header.readable())?;
                            state.header_pending = false;
                        }
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
                    return Ok(Ok(DeliveryStop::Wait(wait)));
                }
            }
        }
    }

    /// Evaluates the next row of a streamed FETCH from its cursor's retained
    /// query, as [`Self::advance_stream_row`] does for a stream's own query.
    #[allow(clippy::too_many_arguments)]
    fn advance_fetch_row(
        &mut self,
        slot: usize,
        source_snapshot: u32,
        arena: &Arena,
        txn: &mut TxnState,
        sqlprep: &mut SqlPreparedPool,
        cursors: &mut cursor::CursorPool,
        guc: &GucState,
        responder: &mut Responder,
    ) -> Result<Result<Option<bool>, SqlError>, WireFull> {
        let row = (|| {
            let owner = cursors.plan_owner(slot)?;
            self.storage.set_read_snapshot(source_snapshot);
            // An ordered cursor sorts every qualifying row before its first.
            self.sort_cursor_inputs(slot, arena, txn, sqlprep, cursors, guc, responder)?;
            self.storage.set_read_snapshot(source_snapshot);
            let sorted = cursors.sorted_source(slot);
            let ready = if sorted {
                cursors.prepare_sorted_row(slot)?
            } else {
                let (work, buffers) = self.work.cursor_parts();
                let (query, _, _) = cursors.execution_parts(slot);
                query.prepare_row(
                    &self.storage,
                    txn.txid,
                    datetime::now_micros(),
                    work,
                    buffers,
                )?
            };
            if !ready {
                return Ok(None);
            }
            let emitted = self.evaluate_cursor_row(
                slot,
                !sorted,
                CursorRowSink::Emit,
                owner,
                source_snapshot,
                arena,
                txn,
                sqlprep,
                cursors,
                guc,
                responder,
            )?;
            cursors.execution_parts(slot).0.finish_row(emitted);
            if sorted {
                cursors.finish_sorted_row(slot);
            }
            if emitted {
                cursors.record_streamed_row(slot)?;
            }
            Ok(Some(emitted))
        })();
        Ok(row)
    }

    /// Evaluates the next row of a stream. `Ok(Some(true))` emitted a row,
    /// `Ok(Some(false))` consumed a row the WHERE clause or OFFSET skipped,
    /// and `Ok(None)` is the end of the result.
    #[allow(clippy::too_many_arguments)]
    fn advance_stream_row(
        &mut self,
        stream: &mut cursor::CursorQuery,
        header: &crate::mem::buffer::FixedBuf,
        header_pending: &mut bool,
        copy: Option<&exec::CopyFmt>,
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
        let ready = match ready {
            Ok(ready) => ready,
            Err(error) => return Ok(Err(error)),
        };
        if *header_pending {
            // The source's sort and hash table are built: nothing can make
            // this result spool any more.
            responder.raw(header.readable())?;
            *header_pending = false;
            self.result_streams_opened = self.result_streams_opened.saturating_add(1);
        }
        if !ready {
            return Ok(Ok(None));
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
            let emitted = match copy {
                Some(format) => exec::emit_copy_row(
                    &self.storage,
                    txid,
                    format,
                    frame,
                    responder,
                    projected.as_slice(),
                )?,
                None => query::emit_data_row(
                    &self.storage,
                    txid,
                    frame,
                    responder,
                    projected.as_slice(),
                )?,
            };
            match emitted {
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
