//! A large result that resumes row by row while its client reads.
//!
//! A top-level `SELECT` whose output first overflows the send buffer while it
//! can still restart is reopened as a retained read: its query, parameters
//! and read position move into this connection-owned allocation
//! (`result_stream_bytes`), and the statement continues across reactor turns.
//! It yields when the client is behind, when a row waits for object storage,
//! and when a row waits for a lock. Rows already delivered are final; a wait
//! inside a row re-evaluates only that row.
//!
//! A paged portal (`Execute` with a row limit) opens its stream before its
//! first row and evaluates rows as each Execute asks for them. Between
//! Executes the stream is suspended: it holds its read position, but no
//! statement is running, so other statements of the transaction may run.
//!
//! A connection owns `result_stream_slots` streams: at most one runs at a
//! time, and every other may hold a suspended portal.

use super::*;
use crate::mem::buffer::FixedBuf;

/// Bytes of each stream's `result_stream_bytes` that hold its opening
/// messages (RowDescription, or CopyOutResponse and a COPY header) until
/// its first row is ready. A larger opening spools the result instead.
pub(crate) const STREAM_HEADER_BYTES: usize = 16 * 1024;

/// Which protocol message owns the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamProtocol {
    /// Statement `statement` of the simple Query at the front of `recv`.
    Simple { statement: usize },
    /// The portal executed by the Execute at the front of `recv` while
    /// running, or the suspended portal between Executes.
    Extended { portal: usize },
}

/// A FETCH whose rows stream from its cursor's retained query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FetchStream {
    pub(crate) slot: usize,
    /// `FETCH n` delivers at most `n` rows; `FETCH ALL` has no limit.
    pub(crate) limit: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) struct StreamState {
    pub(crate) connection: i32,
    pub(crate) txid: u32,
    pub(crate) command: u32,
    pub(crate) protocol: StreamProtocol,
    pub(crate) source_snapshot: u32,
    pub(crate) deadline: i64,
    /// Rows delivered by this statement or Execute: its command tag count.
    pub(crate) emitted: u64,
    /// The current Execute's row limit; reaching it suspends the portal.
    pub(crate) page_limit: Option<u64>,
    /// A paged portal between Executes.
    pub(crate) suspended: bool,
    /// Rows come from this cursor instead of the stream's own query.
    pub(crate) fetch: Option<FetchStream>,
    /// A `COPY ... TO STDOUT` sends its query's rows as COPY data.
    pub(crate) copy: Option<crate::sql::exec::CopyFmt>,
    /// Whether a previous turn already entered the statement's effect scope.
    pub(crate) resumed: bool,
    /// The opening messages in [`ResultStream::header`] still wait for the
    /// first row: the stream's sort or hash table is not built yet, so a
    /// build that does not fit can still re-run the statement spooled.
    pub(crate) header_pending: bool,
}

pub(crate) struct ResultStream {
    pub(crate) query: CursorQuery,
    /// Opening messages held back until the first row is ready.
    pub(crate) header: FixedBuf,
    pub(crate) state: Option<StreamState>,
    /// This stream's slot among its connection's streams.
    index: usize,
}

impl ResultStream {
    fn new(config: &Config, budget: &mut Budget, index: usize) -> Result<Self, BudgetError> {
        Ok(Self {
            header: FixedBuf::new(
                budget,
                "result_stream_bytes",
                STREAM_HEADER_BYTES.min(config.result_stream_bytes),
            )?,
            query: CursorQuery::new(budget, Self::query_bytes(config.result_stream_bytes))?,
            state: None,
            index,
        })
    }

    /// The part of `result_stream_bytes` that holds the query, parameters
    /// and read positions.
    pub(crate) fn query_bytes(result_stream_bytes: usize) -> usize {
        result_stream_bytes.saturating_sub(STREAM_HEADER_BYTES)
    }

    pub(crate) fn index(&self) -> usize {
        self.index
    }

    /// A statement is running in the stream.
    pub(crate) fn is_active(&self) -> bool {
        self.state.is_some_and(|state| !state.suspended)
    }

    /// The stream allocation is in use, running or suspended.
    pub(crate) fn is_held(&self) -> bool {
        self.state.is_some()
    }

    /// The protocol owner of a running stream.
    pub(crate) fn protocol(&self) -> Option<StreamProtocol> {
        self.state
            .filter(|state| !state.suspended)
            .map(|state| state.protocol)
    }

    /// The portal of a suspended stream.
    pub(crate) fn suspended_portal(&self) -> Option<usize> {
        match self.state {
            Some(StreamState {
                suspended: true,
                protocol: StreamProtocol::Extended { portal },
                ..
            }) => Some(portal),
            _ => None,
        }
    }

    /// The tables a held stream reads; a FETCH stream's cursor holds its own.
    pub(crate) fn source_tables(&self) -> StreamTables {
        match (&self.state, self.query.retained.as_ref()) {
            (Some(_), Some(query)) => query.source_tables(),
            _ => NO_STREAM_TABLES,
        }
    }

    /// Drops the query, its reader leases and position, and its table hold.
    pub(crate) fn release(&mut self, cursors: &mut CursorPool) {
        self.state = None;
        self.query.clear();
        self.header.clear();
        cursors.hold_stream_tables(self.index, NO_STREAM_TABLES);
    }
}

/// A connection's startup-reserved result streams.
pub(crate) struct ResultStreams {
    slots: Box<[ResultStream]>,
}

impl ResultStreams {
    /// Startup bytes for the streams' control records and query arenas.
    pub(crate) fn budget_bytes(config: &Config) -> usize {
        config.result_stream_slots.saturating_mul(
            core::mem::size_of::<ResultStream>().saturating_add(config.result_stream_bytes),
        )
    }

    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        budget.draw_array(
            config.result_stream_slots,
            core::mem::size_of::<ResultStream>(),
            "result_stream_slots",
        )?;
        let mut slots = Vec::with_capacity(config.result_stream_slots);
        for index in 0..config.result_stream_slots {
            slots.push(ResultStream::new(config, budget, index)?);
        }
        Ok(Self {
            slots: slots.into_boxed_slice(),
        })
    }

    pub(crate) fn get(&self, index: usize) -> &ResultStream {
        &self.slots[index]
    }

    pub(crate) fn get_mut(&mut self, index: usize) -> &mut ResultStream {
        &mut self.slots[index]
    }

    /// The running stream, if any.
    pub(crate) fn active(&self) -> Option<&ResultStream> {
        self.slots.iter().find(|stream| stream.is_active())
    }

    pub(crate) fn active_mut(&mut self) -> Option<&mut ResultStream> {
        self.slots.iter_mut().find(|stream| stream.is_active())
    }

    /// A statement is running in one of the streams.
    pub(crate) fn is_active(&self) -> bool {
        self.active().is_some()
    }

    /// The protocol owner of the running stream.
    pub(crate) fn protocol(&self) -> Option<StreamProtocol> {
        self.active().and_then(ResultStream::protocol)
    }

    /// Some stream is held, running or suspended.
    #[cfg(test)]
    pub(crate) fn is_held(&self) -> bool {
        self.slots.iter().any(ResultStream::is_held)
    }

    /// A stream that no statement or portal holds.
    pub(crate) fn free(&self) -> Option<usize> {
        self.slots.iter().position(|stream| !stream.is_held())
    }

    /// The stream a portal holds, running or suspended.
    pub(crate) fn portal(&self, portal: usize) -> Option<usize> {
        self.slots.iter().position(|stream| {
            stream
                .state
                .is_some_and(|state| state.protocol == StreamProtocol::Extended { portal })
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Releases the running stream; suspended portals keep theirs.
    pub(crate) fn release_active(&mut self, cursors: &mut CursorPool) {
        if let Some(stream) = self.active_mut() {
            stream.release(cursors);
        }
    }

    pub(crate) fn release_all(&mut self, cursors: &mut CursorPool) {
        for stream in self.slots.iter_mut() {
            stream.release(cursors);
        }
    }
}
