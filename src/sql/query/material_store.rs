//! The tuplestore of a `Materialize` under a merge join, as `EXPLAIN
//! ANALYZE` reports its `Storage`.
//!
//! The join marks its inner input at each run of equal keys and restores it
//! for the next outer row of the run (`ExecMaterialMarkPos`,
//! `ExecMaterialRestrPos`), so the tuplestore trims the rows before the mark
//! (`tuplestore_trim`) and its memory follows the runs. This replays the
//! node's reads, marks and restores over its rows' sizes. Sources
//! (REL_18_6): `executor/nodeMaterial.c`, `utils/sort/tuplestore.c`.

use super::postgres_memory::Tuplestore;
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;

#[derive(Clone, Copy)]
struct Pointer {
    current: usize,
    eof: bool,
}

/// A merge join's materialized inner rows. Each call passes `lengths`, every
/// inner row's `t_len` in the order the input returns them.
pub(crate) struct MaterialStore {
    store: Tuplestore,
    /// The read pointer (0) and the mark (1).
    pointers: [Pointer; 2],
    deleted: usize,
    base: usize,
    appended: usize,
}

impl MaterialStore {
    pub(crate) fn new() -> Self {
        Self {
            store: Tuplestore::new(),
            pointers: [Pointer {
                current: 0,
                eof: false,
            }; 2],
            deleted: 0,
            base: 0,
            appended: 0,
        }
    }

    /// `ExecMaterial`: the next row, from the tuplestore when the read
    /// pointer is not at its end, else appended from the input.
    pub(crate) fn next(&mut self, lengths: &[usize]) -> Result<(), SqlError> {
        if !self.pointers[0].eof {
            if self.pointers[0].current < self.store.count() {
                self.pointers[0].current += 1;
                return Ok(());
            }
            self.pointers[0].eof = true;
        }
        let Some(&length) = lengths.get(self.appended) else {
            return Ok(());
        };
        // `tuplestore_puttupleslot` moves a mark at the end past the row.
        if self.pointers[1].eof {
            self.pointers[1] = Pointer {
                current: self.store.count(),
                eof: false,
            };
        }
        self.store.put(length);
        if self.store.spilled() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE of a merge join whose inner rows outgrow work_mem is not supported"
            ));
        }
        self.appended += 1;
        Ok(())
    }

    /// `ExecMaterialMarkPos`.
    pub(crate) fn mark(&mut self, lengths: &[usize]) {
        self.pointers[1] = self.pointers[0];
        self.trim(lengths);
    }

    /// `ExecMaterialRestrPos`.
    pub(crate) fn restore(&mut self) {
        self.pointers[0] = self.pointers[1];
    }

    /// `tuplestore_trim`.
    fn trim(&mut self, lengths: &[usize]) {
        let count = self.store.count();
        let oldest = self
            .pointers
            .iter()
            .filter(|pointer| !pointer.eof)
            .fold(count, |oldest, pointer| oldest.min(pointer.current));
        let removed = oldest.saturating_sub(1);
        if removed == 0 {
            return;
        }
        self.store.update_most();
        for row in self.deleted..removed {
            self.store.release(lengths[self.base + row]);
        }
        self.deleted = removed;
        if removed < count / 8 {
            return;
        }
        self.store.slide(removed);
        self.base += removed;
        self.deleted = 0;
        for pointer in &mut self.pointers {
            if !pointer.eof {
                pointer.current -= removed;
            }
        }
    }

    /// The most bytes the tuplestore held (`tuplestore_get_stats`).
    pub(crate) fn finish(self) -> usize {
        self.store.finish().1
    }
}
