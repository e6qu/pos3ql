//! A large result that resumes row by row while its client reads.
//!
//! A top-level `SELECT` whose output first overflows the send buffer while it
//! can still restart is reopened as a retained read: its query, parameters
//! and read position move into this connection-owned allocation
//! (`result_stream_bytes`), and the statement continues across reactor turns.
//! It yields when the client is behind, when a row waits for object storage,
//! and when a row waits for a lock. Rows already delivered are final; a wait
//! inside a row re-evaluates only that row.

use super::*;

/// Which protocol message owns the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamProtocol {
    /// Statement `statement` of the simple Query at the front of `recv`.
    Simple { statement: usize },
    /// The unnamed or unpaged portal executed by the Execute at the front of `recv`.
    Extended { portal: usize },
}

#[derive(Clone, Copy)]
pub(crate) struct StreamState {
    pub(crate) connection: i32,
    pub(crate) txid: u32,
    pub(crate) command: u32,
    pub(crate) protocol: StreamProtocol,
    pub(crate) source_snapshot: u32,
    pub(crate) deadline: i64,
    pub(crate) emitted: u64,
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

    pub(crate) fn is_active(&self) -> bool {
        self.state.is_some()
    }

    pub(crate) fn protocol(&self) -> Option<StreamProtocol> {
        self.state.map(|state| state.protocol)
    }

    /// Drops the query, its reader leases and position.
    pub(crate) fn release(&mut self) {
        self.state = None;
        self.query.clear();
    }
}
