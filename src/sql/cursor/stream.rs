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

use super::*;

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
    /// Whether a previous turn already entered the statement's effect scope.
    pub(crate) resumed: bool,
}

pub(crate) struct ResultStream {
    pub(crate) query: CursorQuery,
    pub(crate) state: Option<StreamState>,
}

impl ResultStream {
    pub(crate) fn new(config: &Config, budget: &mut Budget) -> Result<Self, BudgetError> {
        Ok(Self {
            query: CursorQuery::new(budget, config.result_stream_bytes)?,
            state: None,
        })
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

    /// The table a held stream reads; a FETCH stream's cursor holds its own.
    pub(crate) fn source_table(&self) -> Option<usize> {
        self.state?;
        self.query.retained.as_ref()?.source_table()
    }

    /// Drops the query, its reader leases and position, and its table hold.
    pub(crate) fn release(&mut self, cursors: &mut CursorPool) {
        self.state = None;
        self.query.clear();
        cursors.hold_stream_table(None);
    }
}
