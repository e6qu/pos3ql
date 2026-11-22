//! A batched hash join's table in `plan_build_bytes` leases, as
//! `nodeHash.c` keeps it: the general executor's [`BatchTable`] with each
//! list in a lease and each row a handle to its encoding in the node's
//! store, so a pull node keeps the table between calls without an arena.
//! Its buckets and batches grow exactly as the general executor's do.

use super::*;

/// A row the table holds: its encoding's first relation in the node's
/// store, its hash value, and its size as `spaceUsed` counts it
/// (`HJTUPLE_OVERHEAD` and the minimal tuple).
#[derive(Clone, Copy)]
pub(super) struct Hashed {
    pub(super) row: u32,
    pub(super) hash: u32,
    pub(super) size: usize,
}

/// A row in memory: its chunk, its place there, when it was last prepended
/// to its chain, and whether an outer row matched it
/// (`HeapTupleHeaderSetMatch`).
#[derive(Clone, Copy)]
struct Kept {
    chunk: u32,
    place: u32,
    prepended: u64,
    hashed: Hashed,
    matched: bool,
}

/// A row of a skew bucket: the bucket's index among the skew slots.
#[derive(Clone, Copy)]
struct SkewRow {
    bucket: u32,
    hashed: Hashed,
    matched: bool,
}

/// A row saved to a later batch's file: an inner row, or an outer row
/// with its hash.
#[derive(Clone, Copy)]
pub(super) struct Saved {
    pub(super) batch: u32,
    pub(super) outer: bool,
    pub(super) hashed: Hashed,
}

/// A memory chunk (`HashMemoryChunk`): its space, its used bytes and the
/// rows placed in it.
#[derive(Clone, Copy)]
struct Chunk {
    id: u32,
    maxlen: usize,
    used: usize,
    rows: u32,
}

/// Where a probe of one outer row is in its chain.
#[derive(Clone, Copy)]
pub(super) enum Chain {
    /// The main table's rows of the hash: the next position in the index.
    Main { next: usize, hash: u32 },
    /// A skew bucket's rows, newest first: the skew rows before `before`
    /// are left.
    Skew {
        bucket: u32,
        before: usize,
        hash: u32,
    },
    /// No row is left (a NULL key matches none).
    Empty,
}

/// A candidate of a probe's chain.
#[derive(Clone, Copy)]
pub(super) struct Candidate {
    pub(super) hashed: Hashed,
    skew: bool,
    place: usize,
}

fn table_full(error: ListError) -> SqlError {
    error.into_sql("a join's hash table's rows")
}

/// Inserts `value` at `position`, moving the entries from there one on.
fn insert_at<T: Copy>(
    list: &mut NodeList<'_, T>,
    position: usize,
    value: T,
) -> Result<(), SqlError> {
    let position = position.min(list.len());
    list.push(value).map_err(table_full)?;
    list.as_mut_slice()[position..].rotate_right(1);
    Ok(())
}

/// PostgreSQL's hash table of a join of several batches.
pub(super) struct BatchStore<'a> {
    pub(super) buckets: u32,
    /// `nbuckets_original`, as `EXPLAIN ANALYZE` reports it.
    original_buckets: u32,
    /// `nbuckets_optimal`, which a one-batch build grows.
    optimal_buckets: u32,
    pub(super) batches: u32,
    original_batches: u32,
    pub(super) current: u32,
    space_allowed: usize,
    space_used: usize,
    peak: usize,
    grow: bool,
    pub(super) total: usize,
    skew_total: usize,
    pub(super) skew_enabled: bool,
    skew_length: u32,
    skew_used: usize,
    skew_allowed: usize,
    skew: NodeList<'a, SkewSlot>,
    skew_rows: NodeList<'a, SkewRow>,
    /// The chunk list, head first.
    chunks: NodeList<'a, Chunk>,
    next_chunk: u32,
    memory: NodeList<'a, Kept>,
    prepends: u64,
    /// The later batches' rows, each side's in the order it was saved.
    files: NodeList<'a, Saved>,
    /// `memory` ordered by hash, newest prepend first, once loaded.
    index: NodeList<'a, u32>,
}

impl<'a> BatchStore<'a> {
    pub(super) fn new(
        space: &Space<'a>,
        sizing: crate::sql::query::index_paths::joins::HashSizing,
    ) -> Result<Self, SqlError> {
        let space_allowed = sizing.space_allowed as usize;
        Ok(Self {
            buckets: sizing.buckets,
            original_buckets: sizing.buckets,
            optimal_buckets: sizing.buckets,
            batches: sizing.batches,
            original_batches: sizing.batches,
            current: 0,
            space_allowed,
            space_used: 0,
            peak: 0,
            grow: true,
            total: 0,
            skew_total: 0,
            skew_enabled: false,
            skew_length: 0,
            skew_used: 0,
            skew_allowed: space_allowed * 2 / 100,
            skew: space.list().map_err(table_full)?,
            skew_rows: space.list().map_err(table_full)?,
            chunks: space.list().map_err(table_full)?,
            next_chunk: 0,
            memory: space.list().map_err(table_full)?,
            prepends: 0,
            files: space.list().map_err(table_full)?,
            index: space.list().map_err(table_full)?,
        })
    }

    /// The table's figures `EXPLAIN ANALYZE` reports on its `Hash` node.
    pub(super) fn figures(&self) -> crate::sql::query::instrument::HashTable {
        crate::sql::query::instrument::HashTable {
            tuples: self.total as u64,
            tuple_bytes: None,
            own_bytes: 0,
            batches: Some(crate::sql::query::instrument::HashBatches {
                buckets: self.buckets,
                original_buckets: self.original_buckets,
                batches: self.batches,
                original_batches: self.original_batches,
                peak: self.peak,
            }),
        }
    }

    /// `ExecHashGetBucketAndBatch`'s batch.
    pub(super) fn batch_of(&self, hash: u32) -> u32 {
        hash.rotate_right(self.buckets.trailing_zeros()) & (self.batches - 1)
    }

    /// `ExecHashGetSkewBucket`: probing stops at an empty slot.
    pub(super) fn skew_bucket(&self, hash: u32) -> Option<usize> {
        if !self.skew_enabled {
            return None;
        }
        let slots = self.skew.as_slice();
        let mut position = hash & (self.skew_length - 1);
        loop {
            let held = slots
                .iter()
                .position(|slot| slot.position == position && !slot.removed)?;
            if slots[held].hash == hash {
                return Some(held);
            }
            position = (position + 1) & (self.skew_length - 1);
        }
    }

    /// `dense_alloc`: the chunk the row's bytes go to, and its place there.
    fn dense_alloc(&mut self, size: usize) -> Result<(u32, u32), SqlError> {
        const CHUNK: usize = 32 * 1024;
        let size = size.next_multiple_of(8);
        let head = self.chunks.as_slice().first().copied();
        if size > CHUNK / 4 {
            // An oversized row's own chunk goes after the head.
            let chunk = Chunk {
                id: self.next_chunk,
                maxlen: size,
                used: size,
                rows: 1,
            };
            self.next_chunk += 1;
            insert_at(&mut self.chunks, usize::from(head.is_some()), chunk)?;
            return Ok((chunk.id, 0));
        }
        match head {
            Some(head) if head.maxlen - head.used >= size => {
                let held = &mut self.chunks.as_mut_slice()[0];
                held.used += size;
                held.rows += 1;
                Ok((head.id, head.rows))
            }
            _ => {
                let chunk = Chunk {
                    id: self.next_chunk,
                    maxlen: CHUNK,
                    used: size,
                    rows: 1,
                };
                self.next_chunk += 1;
                insert_at(&mut self.chunks, 0, chunk)?;
                Ok((chunk.id, 0))
            }
        }
    }

    fn note_space(&mut self, size: usize) {
        self.space_used += size;
        self.peak = self.peak.max(self.space_used);
    }

    /// Puts a row into the main table in memory, at its chain's head.
    fn keep(&mut self, hashed: Hashed) -> Result<(), SqlError> {
        let (chunk, place) = self.dense_alloc(hashed.size)?;
        self.prepends += 1;
        self.memory
            .push(Kept {
                chunk,
                place,
                prepended: self.prepends,
                hashed,
                matched: false,
            })
            .map_err(table_full)
    }

    fn save(&mut self, batch: u32, outer: bool, hashed: Hashed) -> Result<(), SqlError> {
        self.files
            .push(Saved {
                batch,
                outer,
                hashed,
            })
            .map_err(table_full)
    }

    /// Saves an outer row of hash `hashed.hash` to its batch's file.
    pub(super) fn save_outer(&mut self, batch: u32, hashed: Hashed) -> Result<(), SqlError> {
        self.save(batch, true, hashed)
    }

    /// `MultiExecPrivateHash`'s insert: into a skew bucket, else
    /// `ExecHashTableInsert`.
    pub(super) fn insert(&mut self, hashed: Hashed) -> Result<(), SqlError> {
        if let Some(bucket) = self.skew_bucket(hashed.hash) {
            self.skew_rows
                .push(SkewRow {
                    bucket: bucket as u32,
                    hashed,
                    matched: false,
                })
                .map_err(table_full)?;
            self.note_space(hashed.size);
            self.skew_used += hashed.size;
            self.skew_total += 1;
            while self.skew_used > self.skew_allowed {
                self.remove_next_skew_bucket()?;
            }
            if self.space_used > self.space_allowed {
                self.increase_batches()?;
            }
            return Ok(());
        }
        if self.batch_of(hashed.hash) == self.current {
            // The rows before this one, skew rows aside, outnumbering the
            // buckets double them while the table has one batch.
            let before = self.total - self.skew_total;
            self.keep(hashed)?;
            if self.batches == 1 && before > self.optimal_buckets as usize {
                self.optimal_buckets *= 2;
            }
            self.note_space(hashed.size);
            if self.space_used + self.optimal_buckets as usize * 8 > self.space_allowed {
                self.increase_batches()?;
            }
        } else {
            self.save(self.batch_of(hashed.hash), false, hashed)?;
        }
        Ok(())
    }

    /// `ExecHashRemoveNextSkewBucket`: the last skew bucket's rows, head of
    /// its chain first, move to the main table or their batch.
    fn remove_next_skew_bucket(&mut self) -> Result<(), SqlError> {
        let Some(bucket) = self.skew.as_slice().iter().rposition(|slot| !slot.removed) else {
            self.skew_used = 0;
            return Ok(());
        };
        let hash = self.skew.as_slice()[bucket].hash;
        let batch = self.batch_of(hash);
        for at in (0..self.skew_rows.len()).rev() {
            let row = self.skew_rows.as_slice()[at];
            if row.bucket as usize != bucket {
                continue;
            }
            if batch == self.current {
                self.keep(row.hashed)?;
                self.skew_used -= row.hashed.size;
            } else {
                self.save(batch, false, row.hashed)?;
                self.space_used -= row.hashed.size;
                self.skew_used -= row.hashed.size;
            }
        }
        self.skew.as_mut_slice()[bucket].removed = true;
        Ok(())
    }

    /// Orders the rows in memory as a walk of the chunks reads them: chunks
    /// in list order, each one's rows in place order.
    fn order_walk(&mut self) -> Result<(), SqlError> {
        let mut position = self.memory.sibling::<u32>().map_err(table_full)?;
        position
            .reserve_exact(self.next_chunk as usize)
            .map_err(table_full)?;
        for _ in 0..self.next_chunk {
            position.push_reserved(u32::MAX).map_err(table_full)?;
        }
        for (at, chunk) in self.chunks.as_slice().iter().enumerate() {
            position.as_mut_slice()[chunk.id as usize] = at as u32;
        }
        let position = position.as_slice();
        self.memory
            .as_mut_slice()
            .sort_unstable_by_key(|row| (position[row.chunk as usize], row.place));
        Ok(())
    }

    /// `ExecHashIncreaseNumBatches`.
    fn increase_batches(&mut self) -> Result<(), SqlError> {
        if !self.grow {
            return Ok(());
        }
        // `ExecHashIncreaseBatchSize`.
        if self.space_allowed <= self.batches as usize * 2 * 8192 {
            self.space_allowed *= 2;
            return Ok(());
        }
        self.batches *= 2;
        self.buckets = self.optimal_buckets;
        self.order_walk()?;
        let fresh = self.memory.sibling().map_err(table_full)?;
        let walk = core::mem::replace(&mut self.memory, fresh);
        self.chunks.clear();
        let (mut in_memory, mut freed) = (0usize, 0usize);
        for &row in walk.as_slice() {
            in_memory += 1;
            let batch = self.batch_of(row.hashed.hash);
            if batch == self.current {
                self.keep(row.hashed)?;
            } else {
                self.save(batch, false, row.hashed)?;
                self.space_used -= row.hashed.size;
                freed += 1;
            }
        }
        if freed == 0 || freed == in_memory {
            self.grow = false;
        }
        Ok(())
    }

    /// `ExecHashIncreaseNumBuckets`: every row in memory prepended again,
    /// walking the chunks.
    pub(super) fn increase_buckets(&mut self) -> Result<(), SqlError> {
        self.buckets = self.optimal_buckets;
        self.order_walk()?;
        for row in self.memory.as_mut_slice() {
            self.prepends += 1;
            row.prepended = self.prepends;
        }
        Ok(())
    }

    /// Ends a build: grows the buckets to their optimal number and charges
    /// their space.
    pub(super) fn finish_build(&mut self) -> Result<(), SqlError> {
        if self.buckets != self.optimal_buckets {
            self.increase_buckets()?;
        }
        self.space_used += self.buckets as usize * 8;
        self.peak = self.peak.max(self.space_used);
        self.ready()
    }

    /// `ExecHashTableReset` for the next batch.
    fn reset(&mut self) {
        self.chunks.clear();
        self.memory.clear();
        self.space_used = 0;
        self.index.clear();
    }

    /// Orders the rows in memory for probing (by hash, newest prepend
    /// first) and clears their match flags.
    pub(super) fn ready(&mut self) -> Result<(), SqlError> {
        let count = self.memory.len();
        self.index.clear();
        self.index.reserve_exact(count).map_err(table_full)?;
        for at in 0..count {
            self.index.push_reserved(at as u32).map_err(table_full)?;
        }
        let rows = self.memory.as_slice();
        self.index.as_mut_slice().sort_unstable_by_key(|&at| {
            let row = rows[at as usize];
            (row.hashed.hash, core::cmp::Reverse(row.prepended))
        });
        for row in self.memory.as_mut_slice() {
            row.matched = false;
        }
        Ok(())
    }

    /// `ExecHashTableResetMatchFlags` of a one-batch table probed again.
    pub(super) fn reset_matches(&mut self) {
        for row in self.memory.as_mut_slice() {
            row.matched = false;
        }
    }

    /// Whether the main table holds no row.
    pub(super) fn memory_empty(&self) -> bool {
        self.memory.is_empty()
    }

    /// The chain an outer row of hash `hash` probes: skew bucket `skew`'s,
    /// or the main table's.
    pub(super) fn chain(&self, hash: Option<u32>, skew: Option<usize>) -> Chain {
        let Some(hash) = hash else {
            return Chain::Empty;
        };
        match skew {
            Some(bucket) => Chain::Skew {
                bucket: bucket as u32,
                before: self.skew_rows.len(),
                hash,
            },
            None => {
                let rows = self.memory.as_slice();
                Chain::Main {
                    next: self
                        .index
                        .as_slice()
                        .partition_point(|&at| rows[at as usize].hashed.hash < hash),
                    hash,
                }
            }
        }
    }

    /// The chain's next row, `None` at its end.
    pub(super) fn next_candidate(&self, chain: &mut Chain) -> Option<Candidate> {
        match chain {
            Chain::Empty => None,
            Chain::Main { next, hash } => {
                let &at = self.index.as_slice().get(*next)?;
                let row = self.memory.as_slice()[at as usize];
                if row.hashed.hash != *hash {
                    *chain = Chain::Empty;
                    return None;
                }
                *next += 1;
                Some(Candidate {
                    hashed: row.hashed,
                    skew: false,
                    place: at as usize,
                })
            }
            Chain::Skew {
                bucket,
                before,
                hash,
            } => {
                let rows = &self.skew_rows.as_slice()[..*before];
                let place = rows
                    .iter()
                    .rposition(|row| row.bucket == *bucket && row.hashed.hash == *hash)?;
                *before = place;
                Some(Candidate {
                    hashed: rows[place].hashed,
                    skew: true,
                    place,
                })
            }
        }
    }

    /// Sets a candidate's match flag, returning whether it was set before.
    pub(super) fn mark_matched(&mut self, candidate: Candidate) -> bool {
        let flag = if candidate.skew {
            &mut self.skew_rows.as_mut_slice()[candidate.place].matched
        } else {
            &mut self.memory.as_mut_slice()[candidate.place].matched
        };
        core::mem::replace(flag, true)
    }

    /// The rows no outer row matched, as `ExecScanHashTableForUnmatched`
    /// walks them: each bucket's chain in bucket order, then, in batch 0,
    /// each skew bucket's. Their handles are written to `out`.
    pub(super) fn unmatched(&self, out: &mut NodeList<'_, u32>) -> Result<(), SqlError> {
        out.clear();
        let rows = self.memory.as_slice();
        let mut order = self.memory.sibling::<u32>().map_err(table_full)?;
        order.reserve_exact(rows.len()).map_err(table_full)?;
        for at in 0..rows.len() {
            order.push_reserved(at as u32).map_err(table_full)?;
        }
        let mask = self.buckets - 1;
        order.as_mut_slice().sort_unstable_by_key(|&at| {
            let row = rows[at as usize];
            (row.hashed.hash & mask, core::cmp::Reverse(row.prepended))
        });
        for &at in order.as_slice() {
            let row = rows[at as usize];
            if !row.matched {
                out.push(row.hashed.row).map_err(table_full)?;
            }
        }
        // Only batch 0 has skew buckets.
        if self.current != 0 {
            return Ok(());
        }
        let skew_rows = self.skew_rows.as_slice();
        for (bucket, slot) in self.skew.as_slice().iter().enumerate() {
            if slot.removed {
                continue;
            }
            for row in skew_rows.iter().rev() {
                if row.bucket as usize == bucket && !row.matched {
                    out.push(row.hashed.row).map_err(table_full)?;
                }
            }
        }
        Ok(())
    }

    /// `ExecHashJoinNewBatch`'s choice of the next batch after `current`:
    /// one lacking outer or inner rows is skipped unless the join returns
    /// the unmatched rows it has, or batches grew since. `outer_start` is
    /// the batches batch 0 ended with.
    pub(super) fn next_batch(
        &self,
        mut current: u32,
        outer_start: u32,
        fill_outer: bool,
        fill_inner: bool,
    ) -> Option<u32> {
        let has = |batch: u32, outer: bool| {
            self.files
                .as_slice()
                .iter()
                .any(|saved| saved.batch == batch && saved.outer == outer)
        };
        current += 1;
        while current < self.batches && (!has(current, true) || !has(current, false)) {
            if (has(current, true) && fill_outer) || (has(current, false) && fill_inner) {
                break;
            }
            if has(current, false) && self.batches != self.original_batches {
                break;
            }
            if has(current, true) && self.batches != outer_start {
                break;
            }
            current += 1;
        }
        (current < self.batches).then_some(current)
    }

    /// Loads batch `batch`: its inner rows are inserted, in the order they
    /// were saved, and its outer rows moved to `outer`, theirs.
    pub(super) fn load(
        &mut self,
        batch: u32,
        outer: &mut NodeList<'_, Saved>,
    ) -> Result<(), SqlError> {
        self.current = batch;
        self.reset();
        outer.clear();
        let mut inner = self.files.sibling::<Hashed>().map_err(table_full)?;
        let mut kept = 0;
        for at in 0..self.files.len() {
            let saved = self.files.as_slice()[at];
            if saved.batch != batch {
                self.files.as_mut_slice()[kept] = saved;
                kept += 1;
            } else if saved.outer {
                outer.push(saved).map_err(table_full)?;
            } else {
                inner.push(saved.hashed).map_err(table_full)?;
            }
        }
        self.files.truncate(kept);
        for &hashed in inner.as_slice() {
            self.insert(hashed)?;
        }
        self.ready()
    }
}

impl SkewBuckets for BatchStore<'_> {
    fn enable_skew(&mut self, length: u32, space: usize) {
        self.skew_enabled = true;
        self.skew_length = length;
        self.note_space(space);
        self.skew_used += space;
    }

    fn skew_slot(&self, position: u32) -> Option<u32> {
        self.skew
            .as_slice()
            .iter()
            .find(|held| held.position == position)
            .map(|held| held.hash)
    }

    fn take_skew_slot(&mut self, slot: SkewSlot) -> Result<(), SqlError> {
        self.skew.push(slot).map_err(table_full)?;
        self.note_space(16);
        self.skew_used += 16;
        Ok(())
    }
}
