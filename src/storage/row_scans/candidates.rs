//! A row-at-a-time read of an index path's candidate rows, in the order the
//! path returns them. A storage wait leaves the position on the candidate it
//! was reading, so the same call after the wait resumes that row; rows
//! already returned are never read again.

use super::*;
use crate::storage::row_images::OrderedRowSnapshot;

/// One candidate row the cursor's snapshot sees.
pub(crate) struct CandidateRow<'a> {
    pub(crate) rowid: u64,
    /// The candidate's place in the list, counting candidates the snapshot
    /// does not see.
    pub(crate) position: usize,
    pub(crate) bytes: &'a [u8],
}

/// The candidates of one index, index-only or bitmap scan and how far it
/// has read them. Visibility is fixed when the cursor opens: the command
/// and commit snapshots are captured then, as a scan's snapshot is.
pub(crate) struct CandidateCursor<'s, 'a> {
    table: usize,
    txid: u32,
    command: u32,
    commit: u64,
    candidates: Candidates<'s, 'a>,
    next: usize,
}

enum Candidates<'s, 'a> {
    /// Row identities, each resolved under the cursor's snapshot as it is
    /// read.
    Rowids(&'a [u64]),
    /// Versions a table snapshot already selected, in scan order.
    Selected(&'a [Option<OrderedRowSnapshot<'s, 'a>>]),
}

impl<'s, 'a> CandidateCursor<'s, 'a> {
    /// Opens on `rowids`, the candidates in their scan order. Nothing is read.
    pub(crate) fn open(storage: &Storage, table: usize, txid: u32, rowids: &'a [u64]) -> Self {
        Self::with(storage, table, txid, Candidates::Rowids(rowids))
    }

    /// Opens on versions `selected` already chose, in their scan order.
    pub(crate) fn open_selected(
        storage: &Storage,
        table: usize,
        txid: u32,
        selected: &'a [Option<OrderedRowSnapshot<'s, 'a>>],
    ) -> Self {
        Self::with(storage, table, txid, Candidates::Selected(selected))
    }

    fn with(storage: &Storage, table: usize, txid: u32, candidates: Candidates<'s, 'a>) -> Self {
        Self {
            table,
            txid,
            command: storage.read_snapshot(),
            commit: storage.commit_snapshot(),
            candidates,
            next: 0,
        }
    }

    fn len(&self) -> usize {
        match self.candidates {
            Candidates::Rowids(rowids) => rowids.len(),
            Candidates::Selected(selected) => selected.len(),
        }
    }

    /// The next candidate the snapshot sees, its bytes copied into `arena`.
    /// A failed read, a storage wait among them, keeps the position and
    /// releases what the read allocated.
    pub(crate) fn next<'arena>(
        &mut self,
        storage: &Storage,
        arena: &'arena Arena,
    ) -> Result<Option<CandidateRow<'arena>>, SqlError> {
        while self.next < self.len() {
            crate::sql::query::check_timeout()?;
            let mark = arena.mark();
            match self.read(storage, arena) {
                Ok(read) => {
                    let position = self.next;
                    self.next += 1;
                    if let Some((rowid, bytes)) = read {
                        return Ok(Some(CandidateRow {
                            rowid,
                            position,
                            bytes,
                        }));
                    }
                }
                Err(error) => {
                    // SAFETY: nothing borrows the bytes of a row not returned.
                    unsafe { arena.rewind_to(mark) };
                    return Err(error);
                }
            }
        }
        Ok(None)
    }

    fn read<'arena>(
        &self,
        storage: &Storage,
        arena: &'arena Arena,
    ) -> Result<Option<(u64, &'arena [u8])>, SqlError> {
        let rowid = match self.candidates {
            Candidates::Rowids(rowids) => rowids[self.next],
            Candidates::Selected(selected) => {
                let Some(row) = selected[self.next] else {
                    return Ok(None);
                };
                let rowid = row.rowid();
                let bytes = storage.row_bytes(self.table, rowid, row.read()?, arena)?;
                return Ok(Some((rowid, bytes)));
            }
        };
        let Some(state) = storage.row_state(self.table, rowid)? else {
            return Ok(None);
        };
        let Some(home) = storage.visible_row_home_at(
            self.table,
            rowid,
            state,
            self.txid,
            self.command,
            self.commit,
        )?
        else {
            return Ok(None);
        };
        storage
            .row_bytes(self.table, rowid, home, arena)
            .map(|bytes| Some((rowid, bytes)))
    }
}
