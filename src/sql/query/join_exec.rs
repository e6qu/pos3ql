//! Runs the join plan PostgreSQL's join search chose
//! ([`super::index_paths::join_query`]), so its rows come back in
//! PostgreSQL's order: a nested loop reads its inner input again (or its
//! `Materialize` copy) for each outer row; a hash join builds its table on
//! the first outer row, stops at an empty one, and returns each outer row's
//! matches newest first, as `ExecHashJoin` walks a bucket's chain; a merge
//! join reads its sorted outer input first (its inner one only when the
//! outer has rows) and returns each outer row's run of inner matches in
//! inner order. An outer join returns each unmatched row of a side it keeps
//! with NULLs for the other, as each executor node does. Sorts order as
//! PostgreSQL's `pg_qsort` does.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHashjoin.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeMergejoin.c>

use core::cmp::Ordering;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::Collation;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError};
use crate::sql::types::Datum;
use crate::storage::{Storage, StoredForm};

use super::index_paths::join_query::{ModeledJoin, PlannedJoin};
use super::index_paths::joins::{
    ClauseList, JoinType, KEY_CLASS, KEY_DESCENDING, KEY_NULLS_FIRST, Keys, Member, PathKind,
    RELATION_NUMBERS, Relids,
};
use super::scan::{JoinRow, JoinRowValues, PaxReadDemand};
use super::{QueryScope, arena_full};

/// A flat outer row: its values, each table's offset in them and each
/// value's stored form.
type OuterValues<'a> = (&'a [Datum<'a>], &'a [usize], &'a [StoredForm]);

mod pull;
pub(crate) use pull::{ModeledPull, pulls};

/// The plan node numbers of a join's nodes under `EXPLAIN ANALYZE`, past
/// the numbers the block's own nodes use.
pub(crate) fn node_number(path: u32) -> Option<u16> {
    u16::try_from(path).ok()?.checked_add(1000)
}

/// The block a base relation's scan counts in: its own, so each scan's
/// nodes are those of a query of that one relation.
pub(crate) fn scan_segment(rel: usize) -> super::instrument::Segment {
    super::instrument::Segment::Derived(u16::try_from(rel).unwrap_or(u16::MAX))
}

fn node(role: impl FnOnce(u16) -> super::instrument::Role, path: u32) -> super::instrument::Node {
    node_number(path).map_or(super::instrument::Node::NONE, |number| {
        super::instrument::node(role(number))
    })
}

/// Moves `files`' entries of `batch` to `out`, replacing its contents, and
/// keeps the others in place in their order (`BufFile` reads of one batch).
fn take_batch<'a, T: Copy + 'a>(
    files: &mut ArenaList<'a, (u32, T)>,
    batch: u32,
    out: &mut ArenaList<'a, T>,
) -> Result<(), SqlError> {
    out.truncate(0);
    let entries = files.as_mut_slice().map_err(|_| arena_full())?;
    let mut kept = 0;
    for at in 0..entries.len() {
        let (at_batch, entry) = entries[at];
        if at_batch == batch {
            out.push(entry).map_err(|_| arena_full())?;
        } else {
            entries[kept] = (at_batch, entry);
            kept += 1;
        }
    }
    files.truncate(kept);
    Ok(())
}

/// One base row: its values and row identity.
#[derive(Clone, Copy)]
struct BaseRow<'a> {
    values: &'a [Datum<'a>],
    /// The stored forms of the relation's columns; empty when every one is
    /// as `heap_form_tuple` forms it.
    forms: &'a [StoredForm],
    rowid: Option<u64>,
}

/// A `Materialize` store a node keeps across rescans.
#[derive(Clone, Copy)]
struct Built<'a> {
    rows: &'a [Row<'a>],
}

/// A row a batched hash table holds: its hash value and its size as
/// `spaceUsed` counts it (`HJTUPLE_OVERHEAD` and the minimal tuple).
#[derive(Clone, Copy)]
struct HashedRow<'a> {
    row: Row<'a>,
    hash: u32,
    size: usize,
}

/// A skew bucket (`HashSkewBucket`): its slot in the skew array.
#[derive(Clone, Copy)]
struct SkewSlot {
    position: u32,
    hash: u32,
    removed: bool,
}

/// A memory chunk (`HashMemoryChunk`): its space and used bytes.
#[derive(Clone, Copy)]
struct Chunk {
    id: u32,
    maxlen: usize,
    used: usize,
}

/// A row in memory: its chunk, its place in the chunk, and when it was
/// last prepended to its chain.
#[derive(Clone, Copy)]
struct MemoryRow<'a> {
    chunk: u32,
    place: u32,
    prepended: u64,
    hashed: HashedRow<'a>,
}

/// PostgreSQL's hash table of a join of several batches, as
/// `nodeHash.c` keeps it.
struct BatchTable<'a> {
    buckets: u32,
    original_buckets: u32,
    /// `nbuckets_optimal`, which a one-batch build grows.
    optimal_buckets: u32,
    batches: u32,
    original_batches: u32,
    current: u32,
    space_allowed: usize,
    space_used: usize,
    peak: usize,
    grow: bool,
    total: usize,
    skew_total: usize,
    skew_enabled: bool,
    skew_length: u32,
    skew_used: usize,
    skew_allowed: usize,
    skew: ArenaList<'a, SkewSlot>,
    /// Skew rows by their bucket's index in `skew`, in insertion order.
    skew_rows: ArenaList<'a, (u32, HashedRow<'a>, u64)>,
    /// The chunk list, head first.
    chunks: ArenaList<'a, Chunk>,
    next_chunk: u32,
    memory: ArenaList<'a, MemoryRow<'a>>,
    prepends: u64,
    inner_files: ArenaList<'a, (u32, HashedRow<'a>)>,
    /// Saved outer rows by batch: each the query's relations, kept as
    /// [`Runner::keep`] keeps them.
    outer_files: ArenaList<'a, (u32, Row<'a>)>,
    /// `memory` ordered by hash, newest prepend first, once loaded.
    index: &'a [u32],
    /// Each row's match flag (`HeapTupleHeaderSetMatch`), in memory and in
    /// the skew buckets, once loaded.
    matched: &'a [core::cell::Cell<bool>],
    skew_matched: &'a [core::cell::Cell<bool>],
    arena: &'a Arena,
}

impl<'a> BatchTable<'a> {
    fn new(
        arena: &'a Arena,
        sizing: super::index_paths::joins::HashSizing,
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
            skew: ArenaList::new(arena),
            skew_rows: ArenaList::new(arena),
            chunks: ArenaList::new(arena),
            next_chunk: 0,
            memory: ArenaList::new(arena),
            prepends: 0,
            inner_files: ArenaList::new(arena),
            outer_files: ArenaList::new(arena),
            index: &[],
            matched: &[],
            skew_matched: &[],
            arena,
        })
    }

    /// `ExecHashGetBucketAndBatch`'s batch.
    fn batch_of(&self, hash: u32) -> u32 {
        hash.rotate_right(self.buckets.trailing_zeros()) & (self.batches - 1)
    }

    /// `ExecHashGetSkewBucket`: probing stops at an empty slot.
    fn skew_bucket(&self, hash: u32) -> Option<usize> {
        if !self.skew_enabled {
            return None;
        }
        let slots = self.skew.borrowed_slice();
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

    /// `dense_alloc`: the chunk the row's bytes go to.
    fn dense_alloc(&mut self, size: usize) -> Result<(u32, u32), SqlError> {
        const CHUNK: usize = 32 * 1024;
        let size = size.next_multiple_of(8);
        let chunks = self.chunks.borrowed_slice();
        let mut list: [Chunk; 0] = [];
        let _ = &mut list;
        let head = chunks.first().copied();
        let place = |chunk: u32, memory: &[MemoryRow<'_>]| {
            memory.iter().filter(|row| row.chunk == chunk).count() as u32
        };
        if size > CHUNK / 4 {
            let chunk = Chunk {
                id: self.next_chunk,
                maxlen: size,
                used: size,
            };
            self.next_chunk += 1;
            self.relink(chunk, usize::from(head.is_some()))?;
            return Ok((chunk.id, 0));
        }
        match head {
            Some(head) if head.maxlen - head.used >= size => {
                let mut rebuilt = ArenaList::new(self.arena);
                for (at, chunk) in self.chunks.borrowed_slice().iter().enumerate() {
                    let mut chunk = *chunk;
                    if at == 0 {
                        chunk.used += size;
                    }
                    rebuilt.push(chunk).map_err(|_| arena_full())?;
                }
                self.chunks = rebuilt;
                Ok((head.id, place(head.id, self.memory.borrowed_slice())))
            }
            _ => {
                let chunk = Chunk {
                    id: self.next_chunk,
                    maxlen: CHUNK,
                    used: size,
                };
                self.next_chunk += 1;
                self.relink(chunk, 0)?;
                Ok((chunk.id, 0))
            }
        }
    }

    /// Links `chunk` into the list at `position`.
    fn relink(&mut self, chunk: Chunk, position: usize) -> Result<(), SqlError> {
        let mut rebuilt = ArenaList::new(self.arena);
        let held = self.chunks.borrowed_slice();
        for (at, old) in held.iter().enumerate() {
            if at == position {
                rebuilt.push(chunk).map_err(|_| arena_full())?;
            }
            rebuilt.push(*old).map_err(|_| arena_full())?;
        }
        if position >= held.len() {
            rebuilt.push(chunk).map_err(|_| arena_full())?;
        }
        self.chunks = rebuilt;
        Ok(())
    }

    fn note_space(&mut self, size: usize) {
        self.space_used += size;
        self.peak = self.peak.max(self.space_used);
    }

    /// Puts a row into the main table in memory, at its chain's head.
    fn keep(&mut self, hashed: HashedRow<'a>) -> Result<(), SqlError> {
        let (chunk, place) = self.dense_alloc(hashed.size)?;
        self.prepends += 1;
        self.memory
            .push(MemoryRow {
                chunk,
                place,
                prepended: self.prepends,
                hashed,
            })
            .map_err(|_| arena_full())
    }

    /// `MultiExecPrivateHash`'s insert: into a skew bucket, else
    /// `ExecHashTableInsert`.
    fn insert(&mut self, hashed: HashedRow<'a>, _arena: &'a Arena) -> Result<(), SqlError> {
        if let Some(bucket) = self.skew_bucket(hashed.hash) {
            self.prepends += 1;
            self.skew_rows
                .push((bucket as u32, hashed, self.prepends))
                .map_err(|_| arena_full())?;
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
            let batch = self.batch_of(hashed.hash);
            self.inner_files
                .push((batch, hashed))
                .map_err(|_| arena_full())?;
        }
        Ok(())
    }

    /// `ExecHashRemoveNextSkewBucket`: the last skew bucket's rows, head of
    /// its chain first, move to the main table or their batch.
    fn remove_next_skew_bucket(&mut self) -> Result<(), SqlError> {
        let Some(bucket) = self
            .skew
            .borrowed_slice()
            .iter()
            .rposition(|slot| !slot.removed)
        else {
            self.skew_used = 0;
            return Ok(());
        };
        let hash = self.skew.borrowed_slice()[bucket].hash;
        let batch = self.batch_of(hash);
        let rows = self.skew_rows.as_slice();
        let mut chain = ArenaList::new(self.arena);
        for &(at, hashed, prepended) in rows {
            if at as usize == bucket {
                chain.push((prepended, hashed)).map_err(|_| arena_full())?;
            }
        }
        let chain = chain.as_slice();
        for &(_, hashed) in chain.iter().rev() {
            if batch == self.current {
                self.keep(hashed)?;
                self.skew_used -= hashed.size;
            } else {
                self.inner_files
                    .push((batch, hashed))
                    .map_err(|_| arena_full())?;
                self.space_used -= hashed.size;
                self.skew_used -= hashed.size;
            }
        }
        let mut slots = ArenaList::new(self.arena);
        for (at, slot) in self.skew.borrowed_slice().iter().enumerate() {
            slots
                .push(SkewSlot {
                    removed: slot.removed || at == bucket,
                    ..*slot
                })
                .map_err(|_| arena_full())?;
        }
        self.skew = slots;
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
        let walk = self.walk()?;
        self.chunks = ArenaList::new(self.arena);
        self.memory = ArenaList::new(self.arena);
        let (mut in_memory, mut freed) = (0usize, 0usize);
        for &row in walk {
            in_memory += 1;
            let batch = self.batch_of(row.hashed.hash);
            if batch == self.current {
                self.keep(row.hashed)?;
            } else {
                self.inner_files
                    .push((batch, row.hashed))
                    .map_err(|_| arena_full())?;
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
    fn increase_buckets(&mut self) -> Result<(), SqlError> {
        self.buckets = self.optimal_buckets;
        let walk = self.walk()?;
        let mut memory = ArenaList::new(self.arena);
        for &row in walk {
            self.prepends += 1;
            memory
                .push(MemoryRow {
                    prepended: self.prepends,
                    ..row
                })
                .map_err(|_| arena_full())?;
        }
        self.memory = memory;
        Ok(())
    }

    /// The table's main rows as a probe reads them.
    fn frozen(&self) -> Frozen<'a> {
        Frozen {
            memory: self.memory.as_slice(),
            index: self.index,
            matched: self.matched,
            buckets: self.buckets,
        }
    }

    /// The rows in memory in chunk-list order, each chunk's in place order.
    fn walk(&self) -> Result<&'a [MemoryRow<'a>], SqlError> {
        let chunks = self.chunks.borrowed_slice();
        let rows = self.memory.borrowed_slice();
        let ordered = self
            .arena
            .alloc_slice_with(rows.len(), |at| rows[at])
            .map_err(|_| arena_full())?;
        let position = |chunk: u32| {
            chunks
                .iter()
                .position(|held| held.id == chunk)
                .unwrap_or(usize::MAX)
        };
        ordered.sort_unstable_by_key(|row| (position(row.chunk), row.place));
        Ok(ordered)
    }

    /// `ExecHashTableReset` for the next batch.
    fn reset(&mut self) {
        self.chunks = ArenaList::new(self.arena);
        self.memory = ArenaList::new(self.arena);
        self.space_used = 0;
        self.index = &[];
        self.matched = &[];
    }

    /// Orders the rows in memory for probing: by hash, newest prepend
    /// first.
    fn ready(&mut self, _runner: &Runner<'_, 'a>, _keys: &KeySides) -> Result<(), SqlError> {
        let rows = self.memory.borrowed_slice();
        let index = self
            .arena
            .alloc_slice_with(rows.len(), |at| at as u32)
            .map_err(|_| arena_full())?;
        index.sort_unstable_by_key(|&at| {
            let row = rows[at as usize];
            (row.hashed.hash, core::cmp::Reverse(row.prepended))
        });
        self.index = index;
        self.matched = core::cell::Cell::from_mut(
            self.arena
                .alloc_slice_with(rows.len(), |_| false)
                .map_err(|_| arena_full())?,
        )
        .as_slice_of_cells();
        if self.skew_matched.len() != self.skew_rows.len() {
            self.skew_matched = core::cell::Cell::from_mut(
                self.arena
                    .alloc_slice_with(self.skew_rows.len(), |_| false)
                    .map_err(|_| arena_full())?,
            )
            .as_slice_of_cells();
        }
        Ok(())
    }

    /// Skew bucket `bucket`'s rows, chain head (newest) first, by their
    /// place among the skew rows.
    fn skew_chain(&self, bucket: usize, out: &mut ArenaList<'a, u32>) -> Result<(), SqlError> {
        let rows = self.skew_rows.borrowed_slice();
        for (place, &(at, _, _)) in rows.iter().enumerate().rev() {
            if at as usize == bucket {
                out.push(place as u32).map_err(|_| arena_full())?;
            }
        }
        Ok(())
    }

    /// The rows no outer row matched, as `ExecScanHashTableForUnmatched`
    /// walks them: each bucket's chain in bucket order, then each skew
    /// bucket's.
    fn unmatched(&self, out: &mut ArenaList<'a, Row<'a>>) -> Result<(), SqlError> {
        let rows = self.memory.borrowed_slice();
        let order = self
            .arena
            .alloc_slice_with(rows.len(), |at| at as u32)
            .map_err(|_| arena_full())?;
        let mask = self.buckets - 1;
        order.sort_unstable_by_key(|&at| {
            let row = rows[at as usize];
            (row.hashed.hash & mask, core::cmp::Reverse(row.prepended))
        });
        for &at in order.iter() {
            if !self.matched[at as usize].get() {
                out.push(rows[at as usize].hashed.row)
                    .map_err(|_| arena_full())?;
            }
        }
        // Only batch 0 has skew buckets.
        if self.current != 0 {
            return Ok(());
        }
        let skew_rows = self.skew_rows.borrowed_slice();
        for (bucket, slot) in self.skew.borrowed_slice().iter().enumerate() {
            if slot.removed {
                continue;
            }
            for (place, &(at, hashed, _)) in skew_rows.iter().enumerate().rev() {
                if at as usize == bucket && !self.skew_matched[place].get() {
                    out.push(hashed.row).map_err(|_| arena_full())?;
                }
            }
        }
        Ok(())
    }

    fn figures(&self, _id: u32) -> super::instrument::HashTable {
        super::instrument::HashTable {
            tuples: self.total as u64,
            tuple_bytes: None,
            own_bytes: 0,
            batches: Some(super::instrument::HashBatches {
                buckets: self.buckets,
                original_buckets: self.original_buckets,
                batches: self.batches,
                original_batches: self.original_batches,
                peak: self.peak,
            }),
        }
    }
}

/// The skew buckets `ExecHashBuildSkewHash` fills.
trait SkewBuckets {
    /// Enables `length` skew slots, charging `space` bytes.
    fn enable_skew(&mut self, length: u32, space: usize);
    /// The hash of the skew slot at `position`, when one is taken there.
    fn skew_slot(&self, position: u32) -> Option<u32>;
    /// Takes a skew slot (`HashSkewBucket`), charging its space.
    fn take_skew_slot(&mut self, slot: SkewSlot) -> Result<(), SqlError>;
}

impl SkewBuckets for BatchTable<'_> {
    fn enable_skew(&mut self, length: u32, space: usize) {
        self.skew_enabled = true;
        self.skew_length = length;
        self.note_space(space);
        self.skew_used += space;
    }

    fn skew_slot(&self, position: u32) -> Option<u32> {
        self.skew
            .borrowed_slice()
            .iter()
            .find(|held| held.position == position)
            .map(|held| held.hash)
    }

    fn take_skew_slot(&mut self, slot: SkewSlot) -> Result<(), SqlError> {
        self.skew.push(slot).map_err(|_| arena_full())?;
        self.note_space(16);
        self.skew_used += 16;
        Ok(())
    }
}

/// A built table's main rows, ordered for probing.
#[derive(Clone, Copy)]
struct Frozen<'a> {
    memory: &'a [MemoryRow<'a>],
    /// `memory` ordered by hash, newest prepend first.
    index: &'a [u32],
    matched: &'a [core::cell::Cell<bool>],
    buckets: u32,
}

impl<'a> Frozen<'a> {
    /// The main table's rows of `hash`, chain head first, by their place in
    /// memory.
    fn chain_of(&self, hash: u32, out: &mut ArenaList<'a, u32>) -> Result<(), SqlError> {
        let rows = self.memory;
        let start = self
            .index
            .partition_point(|&at| rows[at as usize].hashed.hash < hash);
        for &at in &self.index[start..] {
            if rows[at as usize].hashed.hash != hash {
                break;
            }
            out.push(at).map_err(|_| arena_full())?;
        }
        Ok(())
    }

    /// `ExecHashTableResetMatchFlags`.
    fn reset_matches(&self) {
        for flag in self.matched {
            flag.set(false);
        }
    }

    /// The rows no outer row matched, in bucket order.
    fn unmatched(
        &self,
        arena: &'a Arena,
        out: &mut ArenaList<'a, Row<'a>>,
    ) -> Result<(), SqlError> {
        let order = arena
            .alloc_slice_with(self.memory.len(), |at| at as u32)
            .map_err(|_| arena_full())?;
        let mask = self.buckets - 1;
        order.sort_unstable_by_key(|&at| {
            let row = self.memory[at as usize];
            (row.hashed.hash & mask, core::cmp::Reverse(row.prepended))
        });
        for &at in order.iter() {
            if !self.matched[at as usize].get() {
                out.push(self.memory[at as usize].hashed.row)
                    .map_err(|_| arena_full())?;
            }
        }
        Ok(())
    }
}

/// Whether [`datum_hash`] hashes values of `ctype`: the types whose default
/// hash operator class it computes.
pub(crate) fn datum_type_hashable(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    matches!(
        ctype,
        ColType::Int2
            | ColType::Int4
            | ColType::Int8
            | ColType::Oid
            | ColType::Cid
            | ColType::Date
            | ColType::Bool
            | ColType::Char
            | ColType::Text
            | ColType::Varchar
            | ColType::Name
            | ColType::Bpchar
            | ColType::Bytea
            | ColType::Uuid
            | ColType::Numeric
            | ColType::Float4
            | ColType::Float8
            | ColType::Timestamp
            | ColType::Timestamptz
            | ColType::Time
            | ColType::Timetz
            | ColType::Interval
            | ColType::PgLsn
            | ColType::Inet
            | ColType::Cidr
            | ColType::Macaddr
            | ColType::Macaddr8
    )
}

/// The value hashes `ExecHashGetHashValue` and `TupleHashTableHash`
/// combine: each type's default hash support function, `hashtext` and
/// `hashbpchar` under a deterministic collation.
/// https://github.com/postgres/postgres/blob/REL_18_6/src/backend/access/hash/hashfunc.c
pub(crate) fn datum_hash(value: &Datum<'_>) -> Option<u32> {
    use crate::sql::identity::{hash_bytea, hash_int64_input, hash_uint32};
    let bytes = |value: &[u8]| hash_bytea(value) as u32;
    Some(match value {
        Datum::Int2(value) => hash_uint32(*value as i32 as u32),
        Datum::Int4(value) | Datum::Date(value) => hash_uint32(*value as u32),
        Datum::Oid(value) | Datum::Cid(value) => hash_uint32(*value),
        Datum::Int8(value)
        | Datum::Timestamp(value)
        | Datum::Timestamptz(value)
        | Datum::Time(value) => hash_uint32(hash_int64_input(*value)),
        Datum::PgLsn(value) | Datum::Xid8(value) => hash_uint32(hash_int64_input(*value as i64)),
        Datum::Bool(value) => hash_uint32(u32::from(*value)),
        Datum::Char(value) => hash_uint32((*value as i8 as i32) as u32),
        Datum::Text(value) => bytes(value.as_bytes()),
        // `hashbpchar` hashes the value without its trailing blanks.
        Datum::Bpchar(value) => bytes(value.trim_end_matches(' ').as_bytes()),
        Datum::Bytea(value) => bytes(value),
        Datum::Uuid(value) => bytes(value),
        Datum::Macaddr(value) => bytes(value),
        Datum::Macaddr8(value) => bytes(value),
        Datum::Numeric(value) => numeric_hash(value),
        // `hashfloat4` hashes its value as a `float8`.
        Datum::Float4(value) => float8_hash(f64::from(*value)),
        Datum::Float8(value) => float8_hash(*value),
        // `timetz_hash`: the time's `hashint8` mixed with its zone, which
        // PostgreSQL stores as seconds west of UTC.
        Datum::Timetz(micros, offset) => {
            hash_uint32(hash_int64_input(*micros)) ^ hash_uint32(offset.wrapping_neg() as u32)
        }
        // `interval_hash`: the low 64 bits of its 30-day-month span.
        Datum::Interval(interval) => {
            let span = i128::from(interval.micros)
                + i128::from(interval.days) * 86_400_000_000
                + i128::from(interval.months) * 30 * 86_400_000_000;
            hash_uint32(hash_int64_input(span as i64))
        }
        // `hashinet`: the family (`PGSQL_AF_INET` or `PGSQL_AF_INET6`), the
        // mask length and the address bytes.
        Datum::Inet(address) | Datum::Cidr(address) => {
            let mut held = [0u8; 18];
            held[0] = if address.family() == 4 { 2 } else { 3 };
            held[1] = address.bits();
            let length = address.addr_len();
            held[2..2 + length].copy_from_slice(&address.addr()[..length]);
            bytes(&held[..2 + length])
        }
        _ => return None,
    })
}

/// [`datum_hash`] of a grouping or distinct key, which also hashes an enum
/// value (`hashenum`: `hash_uint32` of its label's `pg_enum` OID), an array
/// (`hash_array`), a record (`hash_record`), a range (`hash_range`), a
/// multirange (`hash_multirange`) and a `jsonb` document (`jsonb_hash`).
/// `arena` holds a parsed number or decoded string while it is hashed.
pub(crate) fn key_hash(
    value: &Datum<'_>,
    storage: &Storage,
    txid: u32,
    arena: &crate::mem::arena::Arena,
) -> Result<Option<u32>, SqlError> {
    // `hash_array` and `hash_record` hash a null member as zero and combine
    // the members' hashes as `result * 31 + hash`.
    let combine = |result: u32, member: &Datum<'_>| -> Result<Option<u32>, SqlError> {
        let hashed = if matches!(member, Datum::Null) {
            0
        } else {
            match key_hash(member, storage, txid, arena)? {
                Some(hashed) => hashed,
                None => return Ok(None),
            }
        };
        Ok(Some(result.wrapping_mul(31).wrapping_add(hashed)))
    };
    Ok(match value {
        Datum::Enum { .. } => {
            let oid = crate::sql::enum_values::member_oid(value, storage, txid)?;
            Some(crate::sql::identity::hash_uint32(oid))
        }
        Datum::Array { element, raw } => {
            let mut result = 1u32;
            for member in crate::sql::array::structured_elements(raw, *element, arena) {
                match combine(result, &member?)? {
                    Some(next) => result = next,
                    None => return Ok(None),
                }
            }
            Some(result)
        }
        Datum::CompositeText {
            slot,
            physical_fields,
            text,
        } => {
            let decoded = crate::sql::exec::decode_stored_composite_text(
                *text,
                *slot,
                *physical_fields,
                storage,
                txid,
                arena,
            )?;
            return key_hash(&decoded, storage, txid, arena);
        }
        Datum::Record(fields) | Datum::Composite { fields, .. } => {
            let mut result = 0u32;
            for field in *fields {
                match combine(result, &field.value)? {
                    Some(next) => result = next,
                    None => return Ok(None),
                }
            }
            Some(result)
        }
        Datum::Range { text, kind } => {
            Some(crate::sql::range::hash_range(text, *kind, None, arena)? as u32)
        }
        Datum::Multirange { text, kind } => {
            Some(crate::sql::range::hash_multirange(text, *kind, None, arena)? as u32)
        }
        Datum::Json { text, jsonb: true } => {
            Some(crate::sql::json::compare::hash_jsonb(text, arena)?)
        }
        _ => datum_hash(value),
    })
}

/// Whether [`key_hash`] hashes a key of type `ctype`.
pub(crate) fn key_type_hashable(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    match ctype {
        ColType::Enum(_) | ColType::Jsonb => true,
        ColType::Array(element) => key_type_hashable(element.to_coltype()),
        ColType::Range(kind) | ColType::Multirange(kind) => key_type_hashable(kind.elem_type()),
        _ => datum_type_hashable(ctype),
    }
}

/// Whether PostgreSQL has no hash operator class for a key of type `ctype`,
/// so it always sorts to group it: `tsvector`, `tsquery`, `money`, bit
/// strings, an anonymous record (whose field types the type cache cannot
/// vouch for), and arrays, ranges and multiranges of such elements.
pub(crate) fn key_type_sorts_only(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    match ctype {
        ColType::TsVector
        | ColType::TsQuery
        | ColType::Money
        | ColType::Bit { .. }
        | ColType::Record => true,
        ColType::Array(element) => key_type_sorts_only(element.to_coltype()),
        ColType::Range(kind) | ColType::Multirange(kind) => key_type_sorts_only(kind.elem_type()),
        _ => false,
    }
}

/// Whether two values equal by their type's equality have the same stored
/// image (`datum_image_eq`).
fn same_image(left: &Datum<'_>, right: &Datum<'_>) -> bool {
    match (left, right) {
        (Datum::Numeric(one), Datum::Numeric(two)) => one.dscale == two.dscale,
        (Datum::Float4(one), Datum::Float4(two)) => one.to_bits() == two.to_bits(),
        (Datum::Float8(one), Datum::Float8(two)) => one.to_bits() == two.to_bits(),
        (Datum::Interval(one), Datum::Interval(two)) => {
            (one.months, one.days, one.micros) == (two.months, two.days, two.micros)
        }
        (Datum::Timetz(one, one_zone), Datum::Timetz(two, two_zone)) => {
            (one, one_zone) == (two, two_zone)
        }
        (Datum::Bpchar(one), Datum::Bpchar(two)) => one == two,
        _ => true,
    }
}

/// `hash_numeric`: the base-10000 digits between the first and last
/// nonzero ones, in native byte order, the weight mixed in by XOR; NaN and
/// the infinities hash to zero and zero to all ones.
pub(crate) fn numeric_hash(value: &crate::sql::numeric::Numeric<'_>) -> u32 {
    use crate::sql::numeric::Sign;
    if matches!(value.sign, Sign::NaN | Sign::PosInf | Sign::NegInf) {
        return 0;
    }
    let count = value.ndigits();
    let Some(first) = (0..count).find(|&at| value.digit(at) != 0) else {
        return u32::MAX;
    };
    let last = (0..count)
        .rev()
        .find(|&at| value.digit(at) != 0)
        .expect("a nonzero digit exists");
    let mut held = [0u8; 2 * crate::sql::numeric::MAX_NDIGITS];
    for (at, index) in (first..=last).enumerate() {
        held[2 * at..2 * at + 2].copy_from_slice(&value.digit(index).to_ne_bytes());
    }
    let weight = i32::from(value.weight) - first as i32;
    (crate::sql::identity::hash_bytea(&held[..2 * (last - first + 1)]) as u32) ^ weight as u32
}

/// `hashfloat8`: zero of either sign hashes to zero, and every NaN alike.
fn float8_hash(value: f64) -> u32 {
    if value == 0.0 {
        return 0;
    }
    let value = if value.is_nan() { f64::NAN } else { value };
    crate::sql::identity::hash_bytea(&value.to_ne_bytes()) as u32
}

/// What `MJEvalOuterValues` and `MJEvalInnerValues` make of a row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeEval {
    Matchable,
    NonMatchable,
    EndOfJoin,
}

/// `ExecMergeJoin`'s states of an inner join.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MergeState {
    InitializeOuter,
    InitializeInner,
    JoinTuples,
    NextInner,
    NextOuter,
    TestOuter,
    SkipTest,
    SkipOuterAdvance,
    SkipInnerAdvance,
    EndOuter,
    EndInner,
}

/// A merge join's input rows in merge order, and the nodes counting what
/// the join fetched: its `Sort`, or the scan it read in part.
struct MergeInput<'a> {
    rows: &'a [Row<'a>],
    sort: super::instrument::Node,
    lazy: Option<super::instrument::Node>,
    /// The pages the scan read in part reads, when `BUFFERS` counts them.
    pages: Option<LazyPages<'a>>,
}

/// What an index scan a merge join reads in part reads per row: an index
/// entry, and the heap page of relation `rel`'s row.
#[derive(Clone, Copy)]
struct LazyPages<'a> {
    rel: usize,
    model: super::instrument::PageModel,
    /// `(row identity, heap page)` sorted by identity.
    heap: &'a [(u64, u64)],
    /// An index-only scan's all-visible pages, which precede the others:
    /// a row on a later page is a heap fetch.
    visible: Option<u64>,
    /// `BUFFERS` counts the pages.
    buffers: bool,
}

impl MergeInput<'_> {
    /// Counts the `fetched` rows its sort, or its scan read in part,
    /// returned.
    fn counted(&self, fetched: usize) {
        self.sort.rows(fetched as u64);
        if let Some(lazy) = self.lazy {
            let _scan = lazy.start();
            lazy.rows(fetched as u64);
            if let Some(pages) = self.pages {
                if pages.buffers {
                    lazy.page_model(pages.model);
                }
                for row in &self.rows[..fetched.min(self.rows.len())] {
                    if pages.buffers {
                        lazy.index_entry();
                    }
                    let Some(rowid) = row[pages.rel].and_then(|row| row.rowid) else {
                        continue;
                    };
                    let Ok(at) = pages.heap.binary_search_by_key(&rowid, |&(id, _)| id) else {
                        continue;
                    };
                    let page = pages.heap[at].1;
                    match pages.visible {
                        Some(visible) if page < visible => {}
                        Some(_) => {
                            lazy.heap_fetch();
                            if pages.buffers {
                                lazy.heap_page(page);
                            }
                        }
                        None if pages.buffers => lazy.heap_page(page),
                        None => {}
                    }
                }
            }
        }
    }
}

/// A `Memoize` entry: its key, its rows' place in the cache's row list and
/// whether its input was read to the end.
#[derive(Clone, Copy)]
struct MemoEntry<'a> {
    key: [Datum<'a>; super::index_paths::joins::MAX_KEYS],
    start: usize,
    len: usize,
    complete: bool,
}

/// A `Memoize` node's cache, kept across the nested loop's rescans.
struct MemoCache<'a> {
    entries: ArenaList<'a, MemoEntry<'a>>,
    rows: ArenaList<'a, Row<'a>>,
    stats: super::instrument::MemoizeStats,
    /// `mem_used`.
    used: usize,
}

/// What a hash join's probe reads: its keys, its join conditions and
/// filters, its join type and inner relations, and its node.
struct Probe<'k> {
    keys: &'k KeySides,
    quals: (&'k ClauseList, &'k ClauseList),
    jointype: JoinType,
    inner_relids: Relids,
    join: super::instrument::Node,
}

/// Each key clause's (outer, inner) column; unused entries hold `usize::MAX`.
type KeySides = [((usize, usize), (usize, usize)); super::index_paths::joins::MAX_JOIN_CLAUSES];

/// Each row's sort or compared key values, and its sizes under a sort.
type KeyedRows<'a> = (&'a [&'a [Datum<'a>]], &'a [(usize, usize)]);

/// The rows bound so far, by FROM position.
type Bound<'a> = [Option<BaseRow<'a>>; RELATION_NUMBERS];

/// A kept row: the query's relations of a [`Bound`], so a store of rows
/// grows with the relations the query joins, not the most it may.
type Row<'a> = &'a [Option<BaseRow<'a>>];

struct Runner<'r, 'a> {
    cx: JoinContext<'r, 'a>,
    /// The statement's storage, which the scan nodes it makes borrow.
    storage: &'r Storage,
    /// Each path's kept hash table or store, which a rescan reads again
    /// (`ExecReScanHashJoin`, `ExecReScanMaterial`).
    built: &'a [core::cell::Cell<Option<Built<'a>>>],
    /// Each hash join's one-batch table, which a rescan probes again.
    frozen: &'a [core::cell::Cell<Option<Frozen<'a>>>],
    /// Each base scan's node, made at its first execution and rescanned
    /// for each later one.
    scan_nodes: &'r [core::cell::RefCell<Option<ScanRecord<'a>>>],
    /// Each `Memoize` node's cache.
    memos: &'r [core::cell::Cell<Option<MemoCache<'a>>>],
    /// The block the join's nodes count in.
    block: super::instrument::BlockId,
}

impl<'r, 'a> core::ops::Deref for Runner<'r, 'a> {
    type Target = JoinContext<'r, 'a>;

    fn deref(&self) -> &JoinContext<'r, 'a> {
        &self.cx
    }
}

/// The flat row layout of a join's relations, which every node of its plan
/// evaluates conditions over, made once per plan.
pub(super) struct JoinLayout<'a> {
    /// Each relation's values' start in a flat row, and a row of NULLs per
    /// relation, which an outer join binds to the side it null-extends.
    offsets: &'a [usize],
    width: usize,
    nulls: &'a [&'a [Datum<'a>]],
    /// The first error an expression member of several relations raised
    /// while the join compared rows by it; the query fails with it before
    /// another row leaves the join.
    failed: core::cell::Cell<Option<SqlError>>,
    /// The flat row an expression of several relations is evaluated over,
    /// allocated once, and its values' stored forms.
    scratch: core::cell::RefCell<&'a mut [Datum<'a>]>,
    form_scratch: core::cell::RefCell<&'a mut [StoredForm]>,
}

impl<'a> JoinLayout<'a> {
    /// The layout of `model`'s relations, resolved in `scope`.
    pub(super) fn new(
        model: &ModeledJoin<'a>,
        scope: &QueryScope<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let n = scope.n;
        let offsets = arena
            .alloc_slice_with(n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut width = 0;
        for (table, offset) in offsets.iter_mut().enumerate() {
            *offset = width;
            width += scope.stored_width(table);
        }
        // A row of NULLs per relation: its columns, then a NULL for each
        // expression member.
        let mut nulls = [&[][..]; RELATION_NUMBERS];
        for (table, held) in nulls.iter_mut().enumerate().take(n) {
            *held = arena
                .alloc_slice_with(scope.stored_width(table) + model.expressions.len(), |_| {
                    Datum::Null
                })
                .map_err(|_| arena_full())?;
        }
        Ok(Self {
            offsets,
            width,
            nulls: arena
                .alloc_slice_copy(&nulls[..n])
                .map_err(|_| arena_full())?,
            failed: core::cell::Cell::new(None),
            scratch: core::cell::RefCell::new(
                arena
                    .alloc_slice_with(width, |_| Datum::Null)
                    .map_err(|_| arena_full())?,
            ),
            form_scratch: core::cell::RefCell::new(
                arena
                    .alloc_slice_with(width, |_| StoredForm::Inline)
                    .map_err(|_| arena_full())?,
            ),
        })
    }
}

/// What every node of a join's plan reads to compare and bind rows: the
/// plan, its relations' layout and the statement's evaluation context.
#[derive(Clone, Copy)]
pub(super) struct JoinContext<'r, 'a> {
    storage: &'r Storage,
    scope: &'r QueryScope<'a>,
    txid: u32,
    model: &'r ModeledJoin<'a>,
    arena: &'a Arena,
    params: &'r [Datum<'a>],
    hooks: &'r EvalHooks<'r, 'a>,
    offsets: &'a [usize],
    width: usize,
    nulls: &'a [&'a [Datum<'a>]],
    failed: &'r core::cell::Cell<Option<SqlError>>,
    scratch: &'r core::cell::RefCell<&'a mut [Datum<'a>]>,
    form_scratch: &'r core::cell::RefCell<&'a mut [StoredForm]>,
}

impl<'r, 'a> JoinContext<'r, 'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        storage: &'r Storage,
        scope: &'r QueryScope<'a>,
        txid: u32,
        model: &'r ModeledJoin<'a>,
        arena: &'a Arena,
        params: &'r [Datum<'a>],
        hooks: &'r EvalHooks<'r, 'a>,
        layout: &'r JoinLayout<'a>,
    ) -> Self {
        Self {
            storage,
            scope,
            txid,
            model,
            arena,
            params,
            hooks,
            offsets: layout.offsets,
            width: layout.width,
            nulls: layout.nulls,
            failed: &layout.failed,
            scratch: &layout.scratch,
            form_scratch: &layout.form_scratch,
        }
    }

    /// `bound`'s relations of the query, kept in the arena.
    fn keep(&self, bound: &Bound<'a>) -> Result<Row<'a>, SqlError> {
        let row = self
            .arena
            .alloc_slice_with(self.scope.n, |table| bound[table])
            .map_err(|_| arena_full())?;
        Ok(row)
    }

    fn bind(&self, bound: &mut [Option<BaseRow<'a>>], row: &[Option<BaseRow<'a>>], relids: Relids) {
        for table in 0..self.scope.n {
            if relids.contains(table as u32) {
                bound[table] = row[table];
            }
        }
    }

    /// Fails with the first error an expression of several relations
    /// raised.
    fn check(&self) -> Result<(), SqlError> {
        match self.failed.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// A member's value in the bound row: a column, an expression member
    /// its scan computed after the columns, or an expression of several
    /// relations evaluated over their bound rows.
    fn value(&self, row: &[Option<BaseRow<'a>>], (table, column): (usize, usize)) -> Datum<'a> {
        if let Some(id) = Member::plain(0, column as u16).expression_id()
            && self.model.expressions[id].reads != Relids::EMPTY
        {
            return match self.evaluate(row, self.model.expressions[id].expression) {
                Ok(value) => value,
                Err(error) => {
                    if let Some(first) = self.failed.take() {
                        self.failed.set(Some(first));
                    } else {
                        self.failed.set(Some(error));
                    }
                    Datum::Null
                }
            };
        }
        row[table].map_or(Datum::Null, |row| {
            row.values[self.value_index(row.values.len(), column)]
        })
    }

    /// Where a member's value sits in a base row of `len` values.
    fn value_index(&self, len: usize, column: usize) -> usize {
        match Member::plain(0, column as u16).expression_id() {
            Some(id) => len - self.model.expressions.len() + id,
            None => column,
        }
    }

    /// The columns of a base row, without its expression members' values.
    fn columns_of<'v>(&self, values: &'v [Datum<'a>]) -> &'v [Datum<'a>] {
        &values[..values.len() - self.model.expressions.len()]
    }

    fn collation(&self, (table, column): (usize, usize)) -> Collation {
        if let Some(id) = Member::plain(0, column as u16).expression_id() {
            return self.model.expressions[id].collation;
        }
        self.scope
            .defs
            .stored(table)
            .expect("a plain table")
            .columns()[column]
            .collation
    }

    /// Each clause's (outer, inner) columns.
    fn sides(&self, clauses: &ClauseList, outer: Relids) -> KeySides {
        let mut sides =
            [((usize::MAX, 0), (usize::MAX, 0)); super::index_paths::joins::MAX_JOIN_CLAUSES];
        for (at, &id) in clauses.as_slice().iter().enumerate() {
            let (left, right) = self
                .model
                .clause_columns(&self.model.plan.clauses[usize::from(id)])
                .expect("a hash or merge clause equates columns");
            sides[at] = if outer.contains(left.0 as u32) {
                (left, right)
            } else {
                (right, left)
            };
        }
        sides
    }

    /// A join's clauses as its plan evaluates them
    /// (`extract_actual_join_clauses`, `order_qual_clauses`): an outer
    /// join's matching conditions apart from the filters pushed down to it,
    /// each list cheapest first, without the clauses its hash or merge keys
    /// apply.
    fn split(
        &self,
        clauses: &ClauseList,
        relids: Relids,
        jointype: JoinType,
        keyed: &ClauseList,
    ) -> (ClauseList, ClauseList) {
        crate::sql::query::index_paths::joins::split_join_clauses(
            self.model.plan.clauses,
            clauses,
            relids,
            jointype,
            keyed,
        )
    }

    /// Binds a row of NULLs to the relations of `relids`.
    fn null_extend(&self, bound: &mut [Option<BaseRow<'a>>], relids: Relids) {
        for (table, (slot, nulls)) in bound.iter_mut().zip(self.nulls).enumerate() {
            if relids.contains(table as u32) {
                *slot = Some(BaseRow {
                    values: nulls,
                    forms: &[],
                    rowid: None,
                });
            }
        }
    }

    /// The stored forms of the bound rows of `relids` (all when `None`) at
    /// their places in a flat row, into `forms`; every other entry `Inline`.
    fn flat_forms(
        &self,
        bound: &[Option<BaseRow<'a>>],
        relids: Option<Relids>,
        forms: &mut [StoredForm],
    ) {
        forms.fill(StoredForm::Inline);
        for (table, row) in bound.iter().enumerate().take(self.scope.n) {
            if let Some(row) = row
                && relids.is_none_or(|relids| relids.contains(table as u32))
            {
                let start = self.offsets[table];
                forms[start..start + row.forms.len()].copy_from_slice(row.forms);
            }
        }
    }

    /// `expression` over the bound rows, its value kept in the arena.
    fn evaluate(
        &self,
        bound: &[Option<BaseRow<'a>>],
        expression: &crate::sql::ast::Expr<'a>,
    ) -> Result<Datum<'a>, SqlError> {
        let mut scratch = self.scratch.borrow_mut();
        let values: &mut [Datum<'a>] = &mut scratch;
        values.fill(Datum::Null);
        for (table, row) in bound.iter().enumerate().take(self.scope.n) {
            if let Some(row) = row {
                let start = self.offsets[table];
                let columns = self.columns_of(row.values);
                values[start..start + columns.len()].copy_from_slice(columns);
            }
        }
        let mut forms = self.form_scratch.borrow_mut();
        self.flat_forms(bound, None, &mut forms);
        let row = JoinRow {
            scope: self.scope,
            values: JoinRowValues::Flat {
                values,
                offsets: self.offsets,
                forms: Some(&forms),
            },
            rowids: &[],
            visible: self.scope.n,
        };
        crate::sql::eval::eval_full(expression, self.arena, self.params, &row, self.hooks)
    }

    /// Whether a condition no equality models holds for the bound rows.
    fn condition(
        &self,
        bound: &[Option<BaseRow<'a>>],
        expression: &crate::sql::ast::Expr<'a>,
    ) -> Result<bool, SqlError> {
        let mark = self.arena.mark();
        let values = self
            .arena
            .alloc_slice_with(self.width, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (table, row) in bound.iter().enumerate().take(self.scope.n) {
            if let Some(row) = row {
                let start = self.offsets[table];
                let columns = self.columns_of(row.values);
                values[start..start + columns.len()].copy_from_slice(columns);
            }
        }
        let forms = self
            .arena
            .alloc_slice_with(self.width, |_| StoredForm::Inline)
            .map_err(|_| arena_full())?;
        self.flat_forms(bound, None, forms);
        let row = JoinRow {
            scope: self.scope,
            values: JoinRowValues::Flat {
                values,
                offsets: self.offsets,
                forms: Some(forms),
            },
            rowids: &[],
            visible: self.scope.n,
        };
        let value =
            crate::sql::eval::eval_full(expression, self.arena, self.params, &row, self.hooks)?;
        let holds = matches!(value, Datum::Bool(true));
        // SAFETY: nothing the evaluation allocated outlives it: the result
        // was read as a flag.
        unsafe { self.arena.rewind_to(mark) };
        Ok(holds)
    }

    fn keys(keys: &KeySides) -> impl Iterator<Item = &((usize, usize), (usize, usize))> {
        keys.iter().take_while(|key| key.0.0 != usize::MAX)
    }

    /// Whether every clause of `clauses` holds for the bound rows.
    fn equal(&self, bound: &[Option<BaseRow<'a>>], clauses: &ClauseList) -> Result<bool, SqlError> {
        for &id in clauses.as_slice() {
            let clause = &self.model.plan.clauses[usize::from(id)];
            let Some((left, right)) = self.model.clause_columns(clause) else {
                let expression = self.model.condition(clause).expect("a written condition");
                if !self.condition(bound, expression)? {
                    return Ok(false);
                }
                continue;
            };
            let (one, two) = (self.value(bound, left), self.value(bound, right));
            if one.is_null()
                || two.is_null()
                || crate::sql::eval::compare_datums_collated(
                    self.storage,
                    self.txid,
                    self.collation(left),
                    &one,
                    &two,
                )? != Ordering::Equal
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn has_null(&self, row: &[Option<BaseRow<'a>>], keys: &KeySides, outer: bool) -> bool {
        Self::keys(keys).any(|(o, i)| self.value(row, if outer { *o } else { *i }).is_null())
    }

    /// The bound outer row's keys against `inner`'s keys.
    fn compare_sides(
        &self,
        bound: &[Option<BaseRow<'a>>],
        inner: &[Option<BaseRow<'a>>],
        keys: &KeySides,
    ) -> Result<Ordering, SqlError> {
        for &(o, i) in Self::keys(keys) {
            let order = crate::sql::eval::compare_datums_collated(
                self.storage,
                self.txid,
                self.collation(o),
                &self.value(bound, o),
                &self.value(inner, i),
            )?;
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    /// `create_hashjoin_plan`'s skew table: the join hashes one key whose
    /// outer side is a column.
    fn skewed(&self, keys: &KeySides) -> bool {
        JoinContext::keys(keys).count() == 1
            && Member::plain(0, keys[0].0.1 as u16)
                .expression_id()
                .is_none()
    }

    /// The combined hash of a row's join keys (`ExecHashGetHashValue`):
    /// `None` when a key is NULL.
    fn key_hash(
        &self,
        row: &[Option<BaseRow<'a>>],
        keys: &KeySides,
        outer: bool,
        keep_nulls: bool,
    ) -> Result<Option<u32>, SqlError> {
        let mut hash = 0u32;
        for (at, &(o, i)) in JoinContext::keys(keys).enumerate() {
            let value = self.value(row, if outer { o } else { i });
            if value.is_null() {
                // A join returning the side's unmatched rows hashes a NULL
                // key as nothing (`EEOP_HASHDATUM_FIRST`, `NEXT32`).
                if !keep_nulls {
                    return Ok(None);
                }
                if at != 0 {
                    hash = hash.rotate_left(1);
                }
                continue;
            }
            let one = datum_hash(&value).ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash join over keys of this type is not supported"
                )
            })?;
            hash = if at == 0 {
                one
            } else {
                hash.rotate_left(1) ^ one
            };
        }
        Ok(Some(hash))
    }

    fn build_skew(
        &self,
        table: &mut dyn SkewBuckets,
        (rel, column): (usize, usize),
        sizing: super::index_paths::joins::HashSizing,
    ) -> Result<(), SqlError> {
        let wanted = sizing.skew_mcvs as usize;
        if wanted == 0 {
            return Ok(());
        }
        let slot = self.scope.slots[rel];
        let definition = self.scope.defs.stored(rel).expect("a plain table");
        let statistics = self.storage.table_statistics(slot, self.txid);
        let Some(distribution) = definition
            .attributes
            .number(column)
            .and_then(|attribute| statistics.index_distribution(0, attribute))
        else {
            return Ok(());
        };
        let ctype = definition.columns()[column].ctype;
        let count = wanted.min(distribution.mcv_len());
        if count == 0 {
            return Ok(());
        }
        let fraction: f64 = (0..count)
            .map(|index| f64::from(distribution.mcv_frequency(index)))
            .sum();
        // `SKEW_MIN_OUTER_FRACTION`.
        if fraction < 0.01 {
            return Ok(());
        }
        let length = ((count + 1) as u32).next_power_of_two() << 2;
        table.enable_skew(length, length as usize * 8 + count * 4);
        for index in 0..count {
            let value = distribution.mcv_value(index, ctype)?;
            let Some(hash) = datum_hash(&value) else {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash join over keys of this type is not supported"
                ));
            };
            // Open addressing in the skew array; a repeated hash is skipped.
            let mut bucket = hash & (length - 1);
            loop {
                match table.skew_slot(bucket) {
                    Some(held) if held == hash => break,
                    Some(_) => bucket = (bucket + 1) & (length - 1),
                    None => {
                        table.take_skew_slot(SkewSlot {
                            position: bucket,
                            hash,
                            removed: false,
                        })?;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// The `t_len` of the minimal tuple of path `id`'s target list for
    /// the bound row.
    fn tuple_len(&self, bound: &[Option<BaseRow<'a>>], id: u32) -> Result<usize, SqlError> {
        self.tuple_sizes(bound, id, &[]).map(|(length, _)| length)
    }

    /// The columns of path `id`'s target list, then each of `extra` it
    /// lacks: a sort's key expressions, which `make_sort_from_pathkeys`
    /// adds to its input's target list as resjunk columns.
    fn sort_columns(
        &self,
        id: u32,
        extra: &[(usize, usize)],
        each: &mut dyn FnMut((usize, usize)),
    ) {
        let mut listed = [(usize::MAX, 0usize); super::index_paths::joins::MAX_KEYS];
        self.model
            .plan
            .target_list(self.model.plan.path(id).relids, &mut |column| {
                let member = (usize::from(column.rel), usize::from(column.column));
                for (slot, &key) in listed.iter_mut().zip(extra) {
                    if key == member {
                        *slot = key;
                    }
                }
                each(member);
            });
        for (at, &key) in extra.iter().enumerate() {
            if listed.get(at) != Some(&key) {
                each(key);
            }
        }
    }

    /// The `t_len` of a row's tuple under the `id` path, with the sort key
    /// columns `extra` it lacks, and what a sort writes of it to a tape
    /// (`writetup_heap`, or `writetup_datum` for a sort of one column).
    fn tuple_sizes(
        &self,
        bound: &[Option<BaseRow<'a>>],
        id: u32,
        extra: &[(usize, usize)],
    ) -> Result<(usize, usize), SqlError> {
        let mut values = [Datum::Null; 128];
        let mut width = 0;
        let mut fits = true;
        self.sort_columns(id, extra, &mut |member| {
            if width < values.len() {
                values[width] = self.value(bound, member);
                width += 1;
            } else {
                fits = false;
            }
        });
        let datum = !extra.is_empty() && width == 1;
        let sizes = fits
            .then(|| {
                super::postgres_memory::SizeSource {
                    storage: self.storage,
                    txid: self.txid,
                }
                .with(|sizing| {
                    let length = super::postgres_memory::minimal_tuple_len_with_tids(
                        Some(sizing),
                        values[..width].iter().copied(),
                        0,
                    )?;
                    let written = if datum {
                        4 + super::postgres_memory::value_written_size(
                            Some(sizing),
                            &values[0],
                            false,
                        )?
                    } else {
                        length - 10 + 4
                    };
                    Some((length, written))
                })
            })
            .flatten();
        sizes.ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a join whose rows have no PostgreSQL size here is not supported"
            )
        })
    }
}

/// Scans `statement`'s FROM clause: by the plan PostgreSQL's join search
/// chose when it plans the query (`planned`, the statement's plan), otherwise
/// as `scan_source` does.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_statement_source<'a>(
    statement: &'a crate::sql::ast::Select<'a>,
    recycle_rows: bool,
    storage: &Storage,
    scope: &QueryScope<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a crate::sql::ast::Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    pax_demand: PaxReadDemand,
    planned: &PlannedJoin<'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    if outer.is_none()
        && let Some(model) = planned.get(storage, txid, statement, scope, params, arena)?
    {
        // A `GroupAggregate` over the join's rows sorted by its keys reads
        // them in its sort's order.
        if let (Some(group), Some(chosen)) = (model.group, model.plan.upper.group)
            && !group.distinct
            && let Some((_, _, presorted)) = chosen.input_sort
        {
            return run_modeled_sorted(
                storage, scope, txid, model, statement, presorted, arena, params, hooks, visit,
            );
        }
        return run_modeled(storage, scope, txid, model, arena, params, hooks, visit);
    }
    if recycle_rows {
        super::scan::scan_source_recycling_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            where_clause,
            arena,
            params,
            hooks,
            outer,
            pax_demand,
            visit,
        )
    } else {
        super::scan::scan_source_with_pax_columns(
            storage,
            scope,
            from,
            txid,
            where_clause,
            arena,
            params,
            hooks,
            outer,
            pax_demand,
            visit,
        )
    }
}

/// Runs `model`'s plan and passes its rows to `visit` in the order the
/// `Sort` below its `GroupAggregate` returns them: PostgreSQL's tuplesort
/// by the grouping keys, or its incremental sort after the first
/// `presorted` of them, so each group's aggregates read their rows in
/// PostgreSQL's order.
#[allow(clippy::too_many_arguments)]
fn run_modeled_sorted<'a>(
    storage: &Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    model: &ModeledJoin<'a>,
    statement: &'a crate::sql::ast::Select<'a>,
    presorted: u8,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let group = model.group.expect("a sorted grouping");
    // Each grouping key's column (`None` for an expression, which each row
    // evaluates), collation and direction.
    let mut keys =
        [(None::<(usize, usize)>, Collation::None, 0u8); super::index_paths::joins::MAX_KEYS];
    for (slot, &(position, flags)) in keys.iter_mut().zip(group.keys) {
        let expression = statement.group_by[position];
        let column = super::index_paths::join_query::column_of(scope, expression);
        let collation = scope.expression_collation(storage, arena, expression)?;
        *slot = (column, collation, flags);
    }
    let keys = &keys[..group.keys.len()];
    let mut held = crate::mem::arena::ArenaList::new(arena);
    let mut offsets: &[usize] = &[];
    run_modeled(
        storage,
        scope,
        txid,
        model,
        arena,
        params,
        hooks,
        &mut |row| {
            let JoinRowValues::Flat {
                values,
                offsets: row_offsets,
                forms: Some(row_forms),
            } = row.values
            else {
                unreachable!("a modeled join's rows are flat, with their forms")
            };
            offsets = arena
                .alloc_slice_copy(row_offsets)
                .map_err(|_| arena_full())?;
            let key_values = arena
                .alloc_slice_with(keys.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for ((value, &(column, ..)), &(position, _)) in
                key_values.iter_mut().zip(keys).zip(group.keys)
            {
                *value = match column {
                    Some((table, column)) => values[row_offsets[table] + column],
                    None => crate::sql::eval::eval_full(
                        statement.group_by[position],
                        arena,
                        params,
                        row,
                        hooks,
                    )?,
                };
            }
            let values = arena.alloc_slice_copy(values).map_err(|_| arena_full())?;
            let forms = arena
                .alloc_slice_copy(row_forms)
                .map_err(|_| arena_full())?;
            let rowids = arena
                .alloc_slice_copy(row.rowids)
                .map_err(|_| arena_full())?;
            held.push((&*values, &*forms, &*rowids, &*key_values))
                .map_err(|_| arena_full())?;
            Ok(true)
        },
    )?;
    let rows = held.as_slice();
    let compare_key = |at: usize, left: u32, right: u32| {
        let (_, collation, flags) = keys[at];
        let (left, right) = (&rows[left as usize].3[at], &rows[right as usize].3[at]);
        let nulls_first = flags & super::index_paths::joins::KEY_NULLS_FIRST != 0;
        Ok::<_, SqlError>(match (left.is_null(), right.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) if nulls_first => Ordering::Less,
            (true, false) => Ordering::Greater,
            (false, true) if nulls_first => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                let compared = crate::sql::eval::compare_datums_collated(
                    storage, txid, collation, left, right,
                )?;
                if flags & super::index_paths::joins::KEY_DESCENDING != 0 {
                    compared.reverse()
                } else {
                    compared
                }
            }
        })
    };
    let compare = &mut |left: u32, right: u32| -> Result<Ordering, SqlError> {
        for key in 0..keys.len() {
            let ordering = compare_key(key, left, right)?;
            if !ordering.is_eq() {
                return Ok(ordering);
            }
        }
        Ok(Ordering::Equal)
    };
    let input = arena
        .alloc_slice_with(rows.len(), |at| at as u32)
        .map_err(|_| arena_full())?;
    let order: &[u32] = if presorted == 0 {
        crate::sql::pg_qsort::pg_tuplesort(input, None, compare)?;
        input
    } else {
        let prefix = usize::from(presorted);
        let scratch = arena
            .alloc_slice_with(rows.len() * 2, |_| 0u32)
            .map_err(|_| arena_full())?;
        let out = arena
            .alloc_slice_with(rows.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        crate::sql::pg_qsort::pg_incremental_sort(
            input,
            scratch,
            out,
            None,
            &mut |left, right| {
                for key in 0..prefix {
                    if !compare_key(key, left, right)?.is_eq() {
                        return Ok(false);
                    }
                }
                Ok(true)
            },
            compare,
            &mut |_, _, _| Ok(()),
        )?;
        out
    };
    for &at in order {
        let (values, forms, rowids, _) = rows[at as usize];
        if !visit(&JoinRow {
            scope,
            values: JoinRowValues::Flat {
                values,
                offsets,
                forms: Some(forms),
            },
            rowids,
            visible: scope.n,
        })? {
            break;
        }
    }
    Ok(())
}

/// Runs `model`'s plan, passing each joined row to `visit` until it
/// returns false.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_modeled<'a>(
    storage: &Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    model: &ModeledJoin<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let written = scope;
    let scope = model.scope(written);
    let mut output = OutputRow::new(model, written, arena)?;
    let block = super::instrument::current_block();
    let built = core::cell::Cell::from_mut(
        arena
            .alloc_slice_with(model.plan.paths.len(), |_| None::<Built<'a>>)
            .map_err(|_| arena_full())?,
    )
    .as_slice_of_cells();
    let frozen = core::cell::Cell::from_mut(
        arena
            .alloc_slice_with(model.plan.paths.len(), |_| None::<Frozen<'a>>)
            .map_err(|_| arena_full())?,
    )
    .as_slice_of_cells();
    // The plan's node records, in one `plan_build_bytes` lease.
    let mut scan_nodes = storage
        .plan_build()
        .lease::<core::cell::RefCell<Option<ScanRecord<'a>>>>(model.plan.paths.len())
        .map_err(node_pool_full)?;
    for _ in 0..model.plan.paths.len() {
        scan_nodes
            .push_reserved(core::cell::RefCell::new(None))
            .map_err(node_pool_full)?;
    }
    let memos = arena
        .alloc_owned_slice_with(model.plan.paths.len(), |_| core::cell::Cell::new(None))
        .map_err(|_| arena_full())?;
    let layout = JoinLayout::new(model, scope, arena)?;
    let runner = Runner {
        cx: JoinContext::new(storage, scope, txid, model, arena, params, hooks, &layout),
        storage,
        built,
        frozen,
        scan_nodes: &scan_nodes,
        memos: &memos,
        block,
    };
    let mut bound: Bound<'a> = [None; RELATION_NUMBERS];
    runner.exec(model.plan.top, &mut bound, &mut |bound| {
        runner.check()?;
        output.fill(model, written, layout.nulls, bound);
        let _block = super::instrument::enter_block(block);
        visit(&output.row(model, written))
    })?;
    runner.check()
}

/// The joined row a plan returns, as the query reads it: each relation's
/// bound row, a removed self-join's relation reading the row of the scan
/// kept in its place and a removed join's relation reading NULLs; with
/// parenthesized joins, each written entry's row built from its members'.
/// Its buffers are allocated once and refilled for each row.
pub(super) struct OutputRow<'a> {
    offsets: &'a [usize],
    widths: &'a [usize],
    values: &'a mut [Datum<'a>],
    forms: &'a mut [StoredForm],
    rowids: &'a mut [Option<u64>],
    written_offsets: &'a [usize],
    written_values: &'a mut [Datum<'a>],
    written_forms: &'a mut [StoredForm],
    written_rowids: &'a mut [Option<u64>],
}

impl<'a> OutputRow<'a> {
    /// The buffers of `model`'s rows over the query's `written` scope.
    pub(super) fn new(
        model: &ModeledJoin<'a>,
        written: &QueryScope<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        // The plan's relations; a parenthesized join's members fill its
        // written entry's columns in order, so one flat row serves both.
        let scope = model.scope(written);
        let n = scope.n;
        let written_offsets = arena
            .alloc_slice_with(written.n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut written_width = 0;
        for (table, offset) in written_offsets.iter_mut().enumerate() {
            *offset = written_width;
            written_width += written.stored_width(table);
        }
        let offsets = arena
            .alloc_slice_with(n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut width = 0;
        for (table, offset) in offsets.iter_mut().enumerate() {
            *offset = width;
            width += scope.stored_width(table);
        }
        Ok(Self {
            offsets,
            widths: arena
                .alloc_slice_with(n, |table| scope.stored_width(table))
                .map_err(|_| arena_full())?,
            values: arena
                .alloc_slice_with(width, |_| Datum::Null)
                .map_err(|_| arena_full())?,
            forms: arena
                .alloc_slice_with(width, |_| StoredForm::Inline)
                .map_err(|_| arena_full())?,
            rowids: arena
                .alloc_slice_with(n, |_| None::<u64>)
                .map_err(|_| arena_full())?,
            written_offsets,
            written_values: arena
                .alloc_slice_with(written_width, |_| Datum::Null)
                .map_err(|_| arena_full())?,
            written_forms: arena
                .alloc_slice_with(written_width, |_| StoredForm::Inline)
                .map_err(|_| arena_full())?,
            written_rowids: arena
                .alloc_slice_with(written.n, |_| None::<u64>)
                .map_err(|_| arena_full())?,
        })
    }

    /// Copies the top join's bound rows in.
    fn fill(
        &mut self,
        model: &ModeledJoin<'a>,
        written: &QueryScope<'a>,
        nulls: &[&'a [Datum<'a>]],
        bound: &[Option<BaseRow<'a>>],
    ) {
        let Self {
            offsets,
            widths,
            values,
            forms,
            rowids,
            written_offsets,
            written_values,
            written_forms,
            written_rowids,
        } = self;
        let n = offsets.len();
        forms.fill(StoredForm::Inline);
        for table in 0..n {
            let start = offsets[table];
            let columns = widths[table];
            // A removed self-join's relation reads the row of the scan kept
            // in its place.
            if let Some(kept) = model.kept_for[table] {
                let row = bound[usize::from(kept)].expect("the top join binds every relation");
                values[start..start + columns].copy_from_slice(&row.values[..columns]);
                forms[start..start + row.forms.len()].copy_from_slice(row.forms);
                rowids[table] = row.rowid;
                continue;
            }
            // A removed join's relation reads as NULLs, which nothing reads.
            if model.plan.removed & Relids::of(table) != Relids::EMPTY {
                values[start..start + columns].copy_from_slice(&nulls[table][..columns]);
                rowids[table] = None;
                continue;
            }
            let row = bound[table].expect("the top join binds every relation");
            values[start..start + columns].copy_from_slice(&row.values[..columns]);
            forms[start..start + row.forms.len()].copy_from_slice(row.forms);
            rowids[table] = row.rowid;
        }
        let Some(nested) = model.nested.as_ref() else {
            return;
        };
        // Each written entry's row from its members' rows; a parenthesized
        // join's is a joined row of no one row.
        let mut leaf = 0;
        for table in 0..written.n {
            let count = (leaf..n)
                .take_while(|&member| usize::from(nested.entries[member]) == table)
                .count();
            let mut at = written_offsets[table];
            let members = leaf..leaf + count;
            leaf += count;
            if !nested.joined.contains(table as u32) {
                let width = widths[members.start];
                written_values[at..at + width]
                    .copy_from_slice(&values[offsets[members.start]..][..width]);
                written_forms[at..at + width]
                    .copy_from_slice(&forms[offsets[members.start]..][..width]);
                written_rowids[table] = rowids[members.start];
                continue;
            }
            written_rowids[table] = None;
            for _ in 0..2 {
                for member in members.clone() {
                    let width = widths[member];
                    written_values[at..at + width]
                        .copy_from_slice(&values[offsets[member]..][..width]);
                    written_forms[at..at + width]
                        .copy_from_slice(&forms[offsets[member]..][..width]);
                    at += width;
                }
            }
            for member in members {
                let nulled =
                    bound[member].is_none_or(|row| core::ptr::eq(row.values, nulls[member]));
                written_values[at] = Datum::Int4(i32::from(nulled));
                written_forms[at] = StoredForm::Inline;
                at += 1;
            }
        }
    }

    /// The row last filled, over the plan's relations or, with
    /// parenthesized joins, over `written`.
    pub(super) fn row<'s>(
        &'s self,
        model: &'s ModeledJoin<'a>,
        written: &'s QueryScope<'a>,
    ) -> JoinRow<'s, 'a, 'a> {
        match model.nested.as_ref() {
            None => JoinRow {
                scope: written,
                values: JoinRowValues::Flat {
                    values: self.values,
                    offsets: self.offsets,
                    forms: Some(self.forms),
                },
                rowids: self.rowids,
                visible: written.n,
            },
            Some(_) => JoinRow {
                scope: written,
                values: JoinRowValues::Flat {
                    values: self.written_values,
                    offsets: self.written_offsets,
                    forms: Some(self.written_forms),
                },
                rowids: self.written_rowids,
                visible: written.n,
            },
        }
    }
}

/// A base relation's scan node and the outer row its condition reads.
struct ScanRecord<'a> {
    node: super::scan::PathScan<'a>,
    /// The relations whose values the scan's condition reads.
    param: Relids,
    /// Their bound values, flat by the query's FROM positions, and those
    /// values' stored forms; the other relations' values are NULL.
    outer: &'a mut [Datum<'a>],
    outer_forms: &'a mut [StoredForm],
}

fn node_pool_full(error: crate::mem::lease_pool::LeaseError) -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "a join's plan nodes exceed plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

/// The rows a materialized input holds: each one's base rows, by FROM
/// position.
struct Held<'a> {
    rows: ArenaList<'a, Row<'a>>,
}

impl<'r, 'a> Runner<'r, 'a> {
    /// Runs path `id`, binding its relations' rows into `bound` for each of
    /// its rows. Returns false once `sink` stopped.
    fn exec(
        &self,
        id: u32,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        super::check_timeout()?;
        // A join reached from a scan's row counts in the join's block.
        let _block = super::instrument::enter_block(self.block);
        let path = *self.model.plan.path(id);
        match path.kind {
            PathKind::Dummy => Ok(true),
            PathKind::Scan { rel, path } => self.scan(id, usize::from(rel), path, bound, sink),
            PathKind::Material { input } => self.exec(input, bound, sink),
            PathKind::Unique {
                input,
                keys,
                key_count,
                method,
            } => match method {
                super::index_paths::joins::UniqueMethod::Noop => self.exec(input, bound, sink),
                super::index_paths::joins::UniqueMethod::Sort => self.unique(
                    input,
                    self.model.plan.keys(keys, key_count),
                    None,
                    bound,
                    sink,
                ),
                super::index_paths::joins::UniqueMethod::Hash => self.unique(
                    input,
                    self.model.plan.keys(keys, key_count),
                    Some(path.rows),
                    bound,
                    sink,
                ),
            },
            PathKind::Memoize {
                input,
                keys,
                key_count,
                single_row,
                binary,
            } => self.memoize(
                id,
                input,
                self.model.plan.keys(keys, key_count),
                (single_row, binary),
                bound,
                sink,
            ),
            PathKind::NestLoop {
                jointype,
                outer,
                inner,
                clauses,
                inner_unique,
            } => {
                let join = node(super::instrument::Role::Join, id);
                let _join = join.start();
                let material = match self.model.plan.path(inner).kind {
                    PathKind::Material { input } => Some(input),
                    _ => None,
                };
                let material_node = node(super::instrument::Role::Materialize, inner);
                let inner_relids = self.model.plan.path(inner).relids;
                let (join_quals, other_quals) =
                    self.split(&clauses, path.relids, jointype, &ClauseList::EMPTY);
                // `ExecNestLoop`: a semi join, or a unique inner relation,
                // ends each outer row's scan at its first match, an anti
                // join at its first match without returning it; a left or
                // anti join returns an unmatched outer row with NULLs.
                let single_match = inner_unique || jointype == JoinType::Semi;
                // A loop that stops at its first match reads its inner
                // `Materialize` only as far as some loop needed
                // (`ExecMaterial` fetching on demand): under `EXPLAIN
                // ANALYZE` the inner scan counts those rows, as one read.
                let lazy = match material {
                    Some(input)
                        if super::instrument::active()
                            && (single_match || jointype == JoinType::Anti) =>
                    {
                        let PathKind::Scan {
                            rel,
                            path: scan_path,
                        } = self.model.plan.path(input).kind
                        else {
                            return Err(crate::sql_err!(
                                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                                "EXPLAIN ANALYZE of a materialized join read in part is not supported"
                            ));
                        };
                        let rel = usize::from(rel);
                        let modeled = self.model.rels[rel];
                        let scan = modeled.scan(
                            self.storage,
                            self.txid,
                            scan_path,
                            self.params,
                            self.arena,
                        )?;
                        if modeled.restriction.is_some()
                            && !(scan.path.kind == super::index_paths::ScanKind::Index
                                && self.counts_lazily(rel, scan_path)?)
                        {
                            return Err(crate::sql_err!(
                                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                                "EXPLAIN ANALYZE of a materialized join read in part is not supported"
                            ));
                        }
                        let _block =
                            super::instrument::enter_block(self.block.child(scan_segment(rel)));
                        Some(super::instrument::node(super::instrument::Role::Scan(0)))
                    }
                    _ => None,
                };
                let reached = core::cell::Cell::new(0usize);
                let kept = self.exec(outer, bound, &mut |bound| {
                    let mut matched = false;
                    let mut stopped = false;
                    let mut pass = |bound: &mut Bound<'a>| -> Result<bool, SqlError> {
                        if !self.equal(bound, &join_quals)? {
                            join.join_filtered();
                            return Ok(true);
                        }
                        matched = true;
                        if jointype == JoinType::Anti {
                            return Ok(false);
                        }
                        if self.equal(bound, &other_quals)? {
                            join.row();
                            if !sink(bound)? {
                                stopped = true;
                                return Ok(false);
                            }
                        } else {
                            join.filtered();
                        }
                        Ok(!single_match)
                    };
                    match material {
                        None => {
                            self.exec(inner, bound, &mut pass)?;
                        }
                        Some(input) => {
                            let _material = material_node.start();
                            let store = match self.built[inner as usize].get() {
                                Some(store) => store,
                                None => {
                                    let rows = if lazy.is_some() {
                                        let _suspended = super::instrument::suspend();
                                        self.hold(input, bound)?.rows.as_slice()
                                    } else {
                                        self.hold(input, bound)?.rows.as_slice()
                                    };
                                    let store = Built { rows };
                                    // The tuplestore `EXPLAIN ANALYZE` reports.
                                    if lazy.is_none() {
                                        material_node.store_opened();
                                        for row in store.rows {
                                            material_node
                                                .store_row(self.tuple_len(row, inner).ok());
                                        }
                                    }
                                    self.built[inner as usize].set(Some(store));
                                    store
                                }
                            };
                            for (at, row) in store.rows.iter().enumerate() {
                                material_node.row();
                                reached.set(reached.get().max(at + 1));
                                self.bind(bound, row, inner_relids);
                                if !pass(bound)? {
                                    break;
                                }
                            }
                        }
                    }
                    if stopped {
                        return Ok(false);
                    }
                    if !matched && jointype.fills_outer() {
                        self.null_extend(bound, inner_relids);
                        if self.equal(bound, &other_quals)? {
                            join.row();
                            if !sink(bound)? {
                                return Ok(false);
                            }
                        } else {
                            join.filtered();
                        }
                    }
                    Ok(true)
                })?;
                if let Some(lazy) = lazy {
                    let rows = self.built[inner as usize]
                        .get()
                        .map_or(&[][..], |store| store.rows);
                    let reached = reached.get().min(rows.len());
                    {
                        let _scan = lazy.start();
                        lazy.rows(reached as u64);
                    }
                    material_node.store_opened();
                    for row in &rows[..reached] {
                        material_node.store_row(self.tuple_len(row, inner).ok());
                    }
                }
                Ok(kept)
            }
            PathKind::HashJoin {
                jointype,
                outer,
                inner,
                clauses,
                hash,
                ..
            } => {
                // An inner input reading the values of an outer row above
                // changes at each rescan, which builds the table again
                // (`ExecReScanHashJoin` with the inner plan's `chgParam`).
                if self.model.plan.path(inner).param != Relids::EMPTY {
                    self.frozen[id as usize].set(None);
                }
                let outer_relids = self.model.plan.path(outer).relids;
                let keys = self.sides(&hash, outer_relids);
                let quals = self.split(&clauses, path.relids, jointype, &hash);
                let join = node(super::instrument::Role::Join, id);
                let hash_node = node(super::instrument::Role::Hash, id);
                let _join = join.start();
                self.batched_hash_join(
                    id, jointype, outer, inner, quals, &keys, join, hash_node, bound, sink,
                )
            }
            PathKind::MergeJoin {
                jointype,
                outer,
                inner,
                merge,
                outer_sort,
                inner_sort,
                clauses,
                materialize_inner,
                inner_unique,
                descending,
                nulls_first,
            } => self.merge_join(
                id,
                jointype,
                path.relids,
                (outer, outer_sort),
                (inner, inner_sort),
                &merge,
                &clauses,
                materialize_inner,
                inner_unique,
                (descending, nulls_first),
                bound,
                sink,
            ),
        }
    }

    /// A merge join as `ExecMergeJoin` runs it: it reads its outer input,
    /// then its inner one only for a joinable outer row (or to return
    /// unmatched inner rows), marks the inner input at each run of equal
    /// keys and restores it for the next outer row of the same keys, returns
    /// each unmatched row of a side it fills with NULLs for the other, and
    /// ends when an input it does not fill ends. A sorted input is read
    /// whole by its `Sort` when first fetched; an input already in order is
    /// read only as far as the join fetches it.
    #[allow(clippy::too_many_arguments)]
    fn merge_join(
        &self,
        id: u32,
        jointype: JoinType,
        relids: Relids,
        (outer, outer_sort): (u32, Keys),
        (inner, inner_sort): (u32, Keys),
        merge: &ClauseList,
        clauses: &ClauseList,
        materialize_inner: bool,
        inner_unique: bool,
        (descending, nulls_first): (u32, u32),
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let join = node(super::instrument::Role::Join, id);
        let _join = join.start();
        let outer_relids = self.model.plan.path(outer).relids;
        let inner_relids = self.model.plan.path(inner).relids;
        let keys = self.sides(merge, outer_relids);
        let (join_quals, other_quals) = self.split(clauses, relids, jointype, merge);
        let fill_outer = jointype.fills_outer();
        let fill_inner = jointype.fills_inner();
        let single_match = inner_unique || jointype == JoinType::Semi;
        let skip_mark_restore = (inner_unique
            || matches!(jointype, JoinType::Semi | JoinType::Anti))
            && clauses.as_slice().len() == merge.as_slice().len();
        let outer_input = self.merge_input(id, outer, &outer_sort, 0, bound)?;
        let outer_rows = outer_input.rows;
        // `MJEvalOuterValues` and `MJEvalInnerValues`: a NULL first key
        // sorted last ends the join unless the join returns that side's
        // unmatched rows.
        let eval = |row: &[Option<BaseRow<'a>>], outer_side: bool| -> MergeEval {
            let fills = if outer_side { fill_outer } else { fill_inner };
            let mut result = MergeEval::Matchable;
            for (at, &(o, i)) in JoinContext::keys(&keys).enumerate() {
                if self.value(row, if outer_side { o } else { i }).is_null() {
                    if at == 0 && nulls_first & 1 == 0 && !fills {
                        return MergeEval::EndOfJoin;
                    }
                    result = MergeEval::NonMatchable;
                }
            }
            result
        };
        let mut outer_fetches = 0usize;
        let mut inner_fetches = 0usize;
        let mut inner_reached = 0usize;
        let mut inner_input: Option<MergeInput<'a>> = None;
        let mut outer_at: Option<usize> = None;
        let mut inner_at: Option<usize> = None;
        let mut inner_next = 0usize;
        let mut marked: Option<usize> = None;
        // The tuplestore of a `Materialize` over the inner input.
        let mut material_store: Option<super::material_store::MaterialStore<'_>> = None;
        let mut matched_outer = false;
        let mut matched_inner = false;
        let mut state = MergeState::InitializeOuter;
        let mut stopped = false;
        macro_rules! next_outer {
            () => {{
                if outer_fetches < outer_rows.len() {
                    outer_fetches += 1;
                    outer_at = Some(outer_fetches - 1);
                } else {
                    outer_at = None;
                }
            }};
        }
        macro_rules! fetch_outer {
            () => {{
                next_outer!();
                match outer_at {
                    Some(at) => eval(&outer_rows[at], true),
                    None => MergeEval::EndOfJoin,
                }
            }};
        }
        macro_rules! next_inner {
            () => {{
                if inner_input.is_none() {
                    inner_input = Some(self.merge_input(id, inner, &inner_sort, 1, bound)?);
                }
                let rows = inner_input.as_ref().expect("read").rows;
                if inner_next < rows.len() {
                    inner_at = Some(inner_next);
                    inner_next += 1;
                    inner_fetches += 1;
                    inner_reached = inner_reached.max(inner_next);
                } else {
                    inner_at = None;
                }
                if materialize_inner && super::instrument::active() {
                    if material_store.is_none() {
                        let lengths = self
                            .arena
                            .alloc_slice_with(rows.len(), |_| 0usize)
                            .map_err(|_| arena_full())?;
                        for (length, row) in lengths.iter_mut().zip(rows) {
                            *length = self.tuple_len(row, inner)?;
                        }
                        material_store = Some(super::material_store::MaterialStore::new(lengths));
                    }
                    material_store.as_mut().expect("opened").next()?;
                }
            }};
        }
        macro_rules! fetch_inner {
            () => {{
                next_inner!();
                let rows = inner_input.as_ref().expect("read").rows;
                match inner_at {
                    Some(at) => eval(&rows[at], false),
                    None => MergeEval::EndOfJoin,
                }
            }};
        }
        // `MJFillOuter` and `MJFillInner`: the current row with NULLs for
        // the other side, when the join's filters pass it.
        macro_rules! fill {
            ($outer_side:expr) => {{
                let inner_rows = inner_input.as_ref().map_or(&[][..], |input| input.rows);
                if $outer_side {
                    self.bind(bound, &outer_rows[outer_at.expect("outer")], outer_relids);
                    self.null_extend(bound, inner_relids);
                } else {
                    self.bind(bound, &inner_rows[inner_at.expect("inner")], inner_relids);
                    self.null_extend(bound, outer_relids);
                }
                if self.equal(bound, &other_quals)? {
                    join.row();
                    if !sink(bound)? {
                        stopped = true;
                        break;
                    }
                } else {
                    join.filtered();
                }
            }};
        }
        // `MJCompare`: each key compared as its sort orders it
        // (`ApplySortComparator`), NULL equal to NULL only to order the
        // rows, never to join them.
        let compare = |outer_row: &[Option<BaseRow<'a>>],
                       inner_row: &[Option<BaseRow<'a>>]|
         -> Result<Ordering, SqlError> {
            let mut null_equals_null = false;
            for (at, &(o, i)) in JoinContext::keys(&keys).enumerate() {
                let (one, two) = (self.value(outer_row, o), self.value(inner_row, i));
                let first = nulls_first >> at & 1 != 0;
                let order = match (one.is_null(), two.is_null()) {
                    (true, true) => {
                        null_equals_null = true;
                        continue;
                    }
                    (true, false) if first => Ordering::Less,
                    (true, false) => Ordering::Greater,
                    (false, true) if first => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => {
                        let order = crate::sql::eval::compare_datums_collated(
                            self.storage,
                            self.txid,
                            self.collation(o),
                            &one,
                            &two,
                        )?;
                        if descending >> at & 1 != 0 {
                            order.reverse()
                        } else {
                            order
                        }
                    }
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(if null_equals_null {
                Ordering::Greater
            } else {
                Ordering::Equal
            })
        };
        let out_of_order = || {
            crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "mergejoin input data is out of order"
            )
        };
        loop {
            super::check_timeout()?;
            match state {
                MergeState::InitializeOuter => match fetch_outer!() {
                    MergeEval::Matchable => state = MergeState::InitializeInner,
                    MergeEval::NonMatchable => {
                        if fill_outer {
                            fill!(true);
                        }
                    }
                    MergeEval::EndOfJoin => {
                        if !fill_inner {
                            break;
                        }
                        state = MergeState::EndOuter;
                        matched_inner = true;
                    }
                },
                MergeState::InitializeInner => match fetch_inner!() {
                    MergeEval::Matchable => state = MergeState::SkipTest,
                    MergeEval::NonMatchable => {
                        if fill_inner {
                            fill!(false);
                        }
                    }
                    MergeEval::EndOfJoin => {
                        if !fill_outer {
                            break;
                        }
                        state = MergeState::EndInner;
                        matched_outer = false;
                    }
                },
                MergeState::JoinTuples => {
                    state = MergeState::NextInner;
                    let inner_rows = inner_input.as_ref().expect("read").rows;
                    let (o, i) = (outer_at.expect("outer"), inner_at.expect("inner"));
                    self.bind(bound, outer_rows[o], outer_relids);
                    self.bind(bound, inner_rows[i], inner_relids);
                    if !self.equal(bound, &join_quals)? {
                        join.join_filtered();
                        continue;
                    }
                    matched_outer = true;
                    matched_inner = true;
                    if jointype == JoinType::Anti {
                        state = MergeState::NextOuter;
                        continue;
                    }
                    if single_match {
                        state = MergeState::NextOuter;
                    }
                    if jointype == JoinType::RightAnti {
                        continue;
                    }
                    if self.equal(bound, &other_quals)? {
                        join.row();
                        if !sink(bound)? {
                            stopped = true;
                            break;
                        }
                    } else {
                        join.filtered();
                    }
                }
                MergeState::NextInner => {
                    if fill_inner && !matched_inner {
                        fill!(false);
                    }
                    let fetched = fetch_inner!();
                    matched_inner = false;
                    state = match fetched {
                        MergeEval::Matchable => {
                            let inner_rows = inner_input.as_ref().expect("read").rows;
                            match compare(
                                outer_rows[outer_at.expect("outer")],
                                inner_rows[inner_at.expect("inner")],
                            )? {
                                Ordering::Equal => MergeState::JoinTuples,
                                Ordering::Less => MergeState::NextOuter,
                                Ordering::Greater => return Err(out_of_order()),
                            }
                        }
                        MergeEval::NonMatchable => MergeState::NextOuter,
                        MergeEval::EndOfJoin => {
                            inner_at = None;
                            MergeState::NextOuter
                        }
                    };
                }
                MergeState::NextOuter => {
                    if fill_outer && !matched_outer {
                        fill!(true);
                    }
                    let fetched = fetch_outer!();
                    matched_outer = false;
                    match fetched {
                        MergeEval::Matchable => state = MergeState::TestOuter,
                        MergeEval::NonMatchable => {}
                        MergeEval::EndOfJoin => {
                            if fill_inner && inner_at.is_some() {
                                state = MergeState::EndOuter;
                            } else {
                                break;
                            }
                        }
                    }
                }
                MergeState::TestOuter => {
                    let inner_rows = inner_input.as_ref().expect("read").rows;
                    let mark = marked.expect("a marked inner row");
                    match compare(outer_rows[outer_at.expect("outer")], inner_rows[mark])? {
                        Ordering::Equal => {
                            if !skip_mark_restore {
                                // `ExecRestrPos`: the next fetch follows the mark.
                                inner_at = Some(mark);
                                inner_next = mark + 1;
                                if let Some(store) = material_store.as_mut() {
                                    store.restore();
                                }
                            }
                            state = MergeState::JoinTuples;
                        }
                        Ordering::Greater => {
                            let current = match inner_at {
                                None => MergeEval::EndOfJoin,
                                Some(at) => eval(inner_rows[at], false),
                            };
                            match current {
                                MergeEval::Matchable => state = MergeState::SkipTest,
                                MergeEval::NonMatchable => state = MergeState::SkipInnerAdvance,
                                MergeEval::EndOfJoin => {
                                    if !fill_outer {
                                        break;
                                    }
                                    state = MergeState::EndInner;
                                }
                            }
                        }
                        Ordering::Less => return Err(out_of_order()),
                    }
                }
                MergeState::SkipTest => {
                    let inner_rows = inner_input.as_ref().expect("read").rows;
                    state = match compare(
                        outer_rows[outer_at.expect("outer")],
                        inner_rows[inner_at.expect("inner")],
                    )? {
                        Ordering::Equal => {
                            marked = inner_at;
                            if !skip_mark_restore && let Some(store) = material_store.as_mut() {
                                store.mark();
                            }
                            MergeState::JoinTuples
                        }
                        Ordering::Less => MergeState::SkipOuterAdvance,
                        Ordering::Greater => MergeState::SkipInnerAdvance,
                    };
                }
                MergeState::SkipOuterAdvance => {
                    if fill_outer && !matched_outer {
                        fill!(true);
                    }
                    let fetched = fetch_outer!();
                    matched_outer = false;
                    match fetched {
                        MergeEval::Matchable => state = MergeState::SkipTest,
                        MergeEval::NonMatchable => {}
                        MergeEval::EndOfJoin => {
                            if fill_inner && inner_at.is_some() {
                                state = MergeState::EndOuter;
                            } else {
                                break;
                            }
                        }
                    }
                }
                MergeState::SkipInnerAdvance => {
                    if fill_inner && !matched_inner {
                        fill!(false);
                    }
                    let fetched = fetch_inner!();
                    matched_inner = false;
                    match fetched {
                        MergeEval::Matchable => state = MergeState::SkipTest,
                        MergeEval::NonMatchable => {}
                        MergeEval::EndOfJoin => {
                            if fill_outer && outer_at.is_some() {
                                state = MergeState::EndInner;
                            } else {
                                break;
                            }
                        }
                    }
                }
                MergeState::EndOuter => {
                    if !matched_inner {
                        fill!(false);
                    }
                    next_inner!();
                    matched_inner = false;
                    if inner_at.is_none() {
                        break;
                    }
                }
                MergeState::EndInner => {
                    if !matched_outer {
                        fill!(true);
                    }
                    next_outer!();
                    matched_outer = false;
                    if outer_at.is_none() {
                        break;
                    }
                }
            }
        }
        // The inputs' counts: a sort returns each row the join fetched, a
        // `Materialize` too while its input returns each row once.
        outer_input.counted(outer_fetches);
        if let Some(input) = inner_input {
            if materialize_inner {
                let material = node(super::instrument::Role::Materialize, id);
                let _material = material.start();
                material.rows(inner_fetches as u64);
                // Its tuplestore holds the rows it read from its input that
                // the mark still needs.
                material.store_opened();
                if let Some(store) = material_store.take() {
                    material.stored_space(false, store.finish());
                }
                input.counted(inner_reached);
            } else {
                input.counted(inner_fetches);
            }
        }
        Ok(!stopped)
    }

    /// A merge join input: sorted, read whole by its `Sort` when the join
    /// first fetches from it, or in order already, read as far as fetched.
    fn merge_input(
        &self,
        join: u32,
        id: u32,
        keys: &Keys,
        side: u8,
        bound: &mut Bound<'a>,
    ) -> Result<MergeInput<'a>, SqlError> {
        let relids = self.model.plan.path(id).relids;
        if !keys.as_slice().is_empty() {
            let sort = node(
                |number| super::instrument::Role::MergeSort(number, side),
                join,
            );
            let _sort = sort.start();
            let held = self.hold(id, bound)?;
            let rows = held.rows.borrowed_slice();
            let (members, flags, count) = self.sort_members(keys, relids);
            let (key_rows, sizes) = self.keyed(rows, &members[..count], id)?;
            let (order, space) =
                self.sort_order(id, key_rows, sizes, (&members[..count], &flags[..count]))?;
            // The planner puts a `Materialize` above an inner sort the join
            // marks, so both sorts are ordinary tuplesorts.
            if super::instrument::active() {
                sort.sorted(space);
            }
            let sorted = self
                .arena
                .alloc_slice_with(order.len(), |at| rows[order[at] as usize])
                .map_err(|_| arena_full())?;
            return Ok(MergeInput {
                rows: sorted,
                sort,
                lazy: None,
                pages: None,
            });
        }
        // An input in index order: read under no count, then counted as far
        // as the join fetched it.
        let mut pages = None;
        let lazy = match self.model.plan.path(id).kind {
            PathKind::Scan { rel, path } if self.counts_lazily(usize::from(rel), path)? => {
                let rel = usize::from(rel);
                let _block = super::instrument::enter_block(self.block.child(scan_segment(rel)));
                let buffers = super::instrument::buffers_counted();
                let lazy_pages = self.lazy_pages(rel, path, buffers)?;
                if buffers || lazy_pages.visible.is_some() {
                    pages = Some(lazy_pages);
                }
                Some(super::instrument::node(super::instrument::Role::Scan(0)))
            }
            _ if super::instrument::active() => {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN ANALYZE of a merge join over an input in order without a sort is not supported"
                ));
            }
            _ => None,
        };
        let held = {
            let _suspended = super::instrument::suspend();
            self.hold(id, bound)?
        };
        Ok(MergeInput {
            rows: held.rows.as_slice(),
            sort: super::instrument::Node::NONE,
            lazy,
            pages,
        })
    }

    /// How the index scan of `rel` by its path `path`, read in part by a
    /// merge join, counts its pages.
    fn lazy_pages(&self, rel: usize, path: u16, buffers: bool) -> Result<LazyPages<'a>, SqlError> {
        let modeled = self.model.rels[rel];
        let scan = modeled.scan(self.storage, self.txid, path, self.params, self.arena)?;
        let slot = self.scope.slots[rel];
        let index_only = scan.path.kind == super::index_paths::ScanKind::IndexOnly;
        if !buffers && !index_only {
            return Ok(LazyPages {
                rel,
                model: super::instrument::PageModel {
                    local: false,
                    relation_pages: 0,
                    rows: 0,
                    index: None,
                    sequential: false,
                    visibility_map: false,
                },
                heap: &[],
                visible: None,
                buffers,
            });
        }
        let (heap, relation_pages) =
            super::instrument::heap_pages(self.storage, slot, self.txid, self.arena)?;
        let visible =
            index_only.then(|| super::instrument::all_visible_pages(self.storage, slot, self.txid));
        Ok(LazyPages {
            rel,
            model: super::instrument::PageModel {
                local: self.storage.table_def(slot, self.txid).persistence
                    == crate::storage::RelationPersistence::Temporary,
                relation_pages,
                rows: heap.len() as u64,
                index: Some(scan.path.index_pages),
                sequential: false,
                visibility_map: visible.is_some_and(|visible| visible > 0),
            },
            heap,
            visible,
            buffers,
        })
    }

    /// Whether the join can count an index scan of `rel` read in part: an
    /// index or index-only scan without a filter, whose rows are each one
    /// fetch.
    fn counts_lazily(&self, rel: usize, path: u16) -> Result<bool, SqlError> {
        let modeled = self.model.rels[rel];
        let scan = modeled.scan(self.storage, self.txid, path, self.params, self.arena)?;
        let conjuncts = modeled.restriction.map_or(0, |restriction| {
            let mut parts = [&crate::sql::ast::Expr::Null; super::plan::MAX_CONJUNCTS];
            let mut count = 0;
            super::plan::flatten_and(restriction, &mut parts, &mut count);
            count
        });
        Ok(matches!(
            scan.path.kind,
            super::index_paths::ScanKind::Index | super::index_paths::ScanKind::IndexOnly
        ) && scan.path.absorbed.len() == conjuncts)
    }

    /// A `Memoize` node (`ExecMemoize`): the rows of its input for the bound
    /// outer row's key, from its cache when an earlier call read them all,
    /// counting hits, misses and its entries' memory as PostgreSQL does.
    fn memoize(
        &self,
        id: u32,
        input: u32,
        keys: &[super::index_paths::joins::Member],
        (single_row, binary): (bool, bool),
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let memoize = node(super::instrument::Role::Memoize, id);
        let _memoize = memoize.start();
        let inner_relids = self.model.plan.path(input).relids;
        let mut key = [Datum::Null; super::index_paths::joins::MAX_KEYS];
        for (at, member) in keys.iter().enumerate() {
            key[at] = self.value(bound, (usize::from(member.rel), usize::from(member.column)));
        }
        let key = &key[..keys.len()];
        let mut cache = self.memos[id as usize].take().unwrap_or_else(|| MemoCache {
            entries: ArenaList::new(self.arena),
            rows: ArenaList::new(self.arena),
            stats: super::instrument::MemoizeStats::default(),
            used: 0,
        });
        let mut found = None;
        for (at, entry) in cache.entries.borrowed_slice().iter().enumerate() {
            if self.same_key(&entry.key[..keys.len()], key, keys, binary)? {
                found = Some(at);
                break;
            }
        }
        if let Some(at) = found
            && cache.entries.borrowed_slice()[at].complete
        {
            cache.stats.hits += 1;
            let entry = cache.entries.borrowed_slice()[at];
            let mut keep = true;
            for position in entry.start..entry.start + entry.len {
                let row = cache.rows.borrowed_slice()[position];
                self.bind(bound, row, inner_relids);
                memoize.row();
                if !sink(bound)? {
                    keep = false;
                    break;
                }
            }
            memoize.memoize(cache.stats);
            self.memos[id as usize].set(Some(cache));
            return Ok(keep);
        }
        if found.is_some() {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a Memoize entry its consumer left incomplete is not supported"
            ));
        }
        cache.stats.misses += 1;
        let mut stored = [Datum::Null; super::index_paths::joins::MAX_KEYS];
        stored[..key.len()].copy_from_slice(key);
        let key_len =
            super::postgres_memory::minimal_tuple_len_with_tids(None, key.iter().copied(), 0)
                .ok_or_else(|| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "a Memoize key with no PostgreSQL size here is not supported"
                    )
                })?;
        // `EMPTY_ENTRY_MEMORY_BYTES`: the entry, its key and the key's tuple.
        cache.used += 24 + 24 + key_len;
        cache.stats.peak = cache.stats.peak.max(cache.used);
        let start = cache.rows.len();
        let entry_at = cache.entries.len();
        cache
            .entries
            .push(MemoEntry {
                key: stored,
                start,
                len: 0,
                complete: false,
            })
            .map_err(|_| arena_full())?;
        let limit = self.model.plan.settings.hash_mem_bytes as usize;
        let mut len = 0usize;
        let mut complete = true;
        let mut stopped = false;
        let saved = *bound;
        let kept = self.exec(input, bound, &mut |bound| {
            // `CACHE_TUPLE_BYTES`: the cached tuple and its list cell.
            cache.used += 16 + self.tuple_len(bound, input)?;
            cache.stats.peak = cache.stats.peak.max(cache.used);
            if cache.used > limit && super::instrument::active() {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN ANALYZE of a Memoize node that evicts cache entries is not supported"
                ));
            }
            cache
                .rows
                .push(self.keep(bound)?)
                .map_err(|_| arena_full())?;
            len += 1;
            memoize.row();
            if !sink(bound)? {
                stopped = true;
                complete = single_row;
                return Ok(false);
            }
            // A unique inner relation's entry is complete at its first row.
            Ok(!single_row)
        })?;
        let _ = kept;
        *bound = saved;
        let entry = &mut cache.entries.as_mut_slice().map_err(|_| arena_full())?[entry_at];
        entry.len = len;
        entry.complete = complete;
        memoize.memoize(cache.stats);
        self.memos[id as usize].set(Some(cache));
        Ok(!stopped)
    }

    /// A hash join PostgreSQL plans with several batches
    /// (`ExecHashJoin` with `ExecHashTableInsert`, `ExecHashIncreaseNumBatches`,
    /// the skew buckets of `ExecHashBuildSkewHash` and
    /// `ExecHashJoinNewBatch`): batch 0's outer rows probe as they are read,
    /// the others are saved to their batch, and each later batch is loaded
    /// and probed in turn.
    #[allow(clippy::too_many_arguments)]
    fn batched_hash_join(
        &self,
        id: u32,
        jointype: JoinType,
        outer: u32,
        inner: u32,
        quals: (ClauseList, ClauseList),
        keys: &KeySides,
        join: super::instrument::Node,
        hash_node: super::instrument::Node,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let inner_path = *self.model.plan.path(inner);
        let inner_relids = inner_path.relids;
        let outer_relids = self.model.plan.path(outer).relids;
        let (fill_outer, fill_inner) = (jointype.fills_outer(), jointype.fills_inner());
        let sizing = super::index_paths::joins::choose_hash_table_size(
            inner_path.rows,
            inner_path.width,
            self.skewed(keys),
            self.model.plan.settings,
        );
        let probe = Probe {
            keys,
            quals: (&quals.0, &quals.1),
            jointype,
            inner_relids,
            join,
        };
        // A rescan reads a one-batch table again (`ExecReScanHashJoin`),
        // its match flags cleared.
        if let Some(frozen) = self.frozen[id as usize].get() {
            frozen.reset_matches();
            if frozen.memory.is_empty() && !fill_outer {
                return Ok(true);
            }
            let kept = self.exec(outer, bound, &mut |bound| {
                let hash = self.key_hash(bound, keys, true, fill_outer)?;
                self.probe_batch(frozen, None, hash, &probe, bound, sink)
            })?;
            if !kept || !fill_inner {
                return Ok(kept);
            }
            let mut rows = ArenaList::new(self.arena);
            frozen.unmatched(self.arena, &mut rows)?;
            return self.fill_inner_rows(rows.as_slice(), &probe, outer_relids, bound, sink);
        }
        let mut table: Option<BatchTable<'a>> = None;
        let built_first =
            fill_inner || (!fill_outer && self.model.plan.path(outer).startup >= inner_path.total);
        if built_first {
            let built = self.build_batches(inner, keys, sizing, hash_node, fill_inner, bound)?;
            if built.total == 0 && !fill_outer {
                if built.batches == 1 {
                    self.frozen[id as usize].set(Some(built.frozen()));
                }
                return Ok(true);
            }
            table = Some(built);
        }
        let mut ended = false;
        // Batch 0: probe as the outer rows are read; save the rest.
        let kept = self.exec(outer, bound, &mut |bound| {
            if table.is_none() {
                let built =
                    self.build_batches(inner, keys, sizing, hash_node, fill_inner, bound)?;
                if built.total == 0 && !fill_outer {
                    // A rescan probes the empty table with every outer row.
                    if built.batches == 1 {
                        self.frozen[id as usize].set(Some(built.frozen()));
                    }
                    ended = true;
                    return Ok(false);
                }
                table = Some(built);
            }
            let table = table.as_mut().expect("built");
            let table = &mut *table;
            let hash = self.key_hash(bound, keys, true, fill_outer)?;
            let Some(value) = hash else {
                return Ok(true);
            };
            let skew = table.skew_bucket(value);
            if skew.is_none() && table.batch_of(value) != 0 {
                let batch = table.batch_of(value);
                let saved = self.keep(bound)?;
                table
                    .outer_files
                    .push((batch, saved))
                    .map_err(|_| arena_full())?;
                return Ok(true);
            }
            let skew = skew.map(|at| (&*table, at));
            self.probe_batch(table.frozen(), skew, hash, &probe, bound, sink)
        })?;
        if !kept && !ended {
            return Ok(false);
        }
        let Some(mut table) = table else {
            return Ok(true);
        };
        if table.batches == 1 {
            self.frozen[id as usize].set(Some(table.frozen()));
            if fill_inner && !ended {
                let mut rows = ArenaList::new(self.arena);
                table.unmatched(&mut rows)?;
                return self.fill_inner_rows(rows.as_slice(), &probe, outer_relids, bound, sink);
            }
            return Ok(true);
        }
        // Each batch returns its unmatched inner rows once its outer rows
        // are done (`HJ_FILL_INNER_TUPLES`), batch 0's skew buckets too.
        if fill_inner && !ended {
            let mut rows = ArenaList::new(self.arena);
            table.unmatched(&mut rows)?;
            if !self.fill_inner_rows(rows.as_slice(), &probe, outer_relids, bound, sink)? {
                return Ok(false);
            }
        }
        // `ExecHashJoinNewBatch`.
        table.skew_enabled = false;
        let outer_start = table.batches;
        let mut current = 0u32;
        // The current batch's saved rows, read in the order they were saved.
        let mut loaded = ArenaList::new(self.arena);
        let mut probes = ArenaList::new(self.arena);
        loop {
            current += 1;
            let has = |files: &[(u32, HashedRow<'a>)], batch: u32| {
                files.iter().any(|(at, _)| *at == batch)
            };
            let has_outer =
                |files: &[(u32, Row<'a>)], batch: u32| files.iter().any(|(at, _)| *at == batch);
            // A batch lacking outer or inner rows is skipped unless the join
            // returns the unmatched rows it has, or batches grew since.
            while current < table.batches
                && (!has_outer(table.outer_files.borrowed_slice(), current)
                    || !has(table.inner_files.borrowed_slice(), current))
            {
                if (has_outer(table.outer_files.borrowed_slice(), current) && fill_outer)
                    || (has(table.inner_files.borrowed_slice(), current) && fill_inner)
                {
                    break;
                }
                if has(table.inner_files.borrowed_slice(), current)
                    && table.batches != table.original_batches
                {
                    break;
                }
                if has_outer(table.outer_files.borrowed_slice(), current)
                    && table.batches != outer_start
                {
                    break;
                }
                current += 1;
            }
            if current >= table.batches {
                return Ok(true);
            }
            table.current = current;
            table.reset();
            take_batch(&mut table.inner_files, current, &mut loaded)?;
            for at in 0..loaded.len() {
                table.insert(loaded.borrowed_slice()[at], self.arena)?;
            }
            take_batch(&mut table.outer_files, current, &mut probes)?;
            table.ready(self, keys)?;
            for at in 0..probes.len() {
                let saved = probes.borrowed_slice()[at];
                let mut row = *bound;
                row[..saved.len()].copy_from_slice(saved);
                let hash = self.key_hash(&row, keys, true, fill_outer)?;
                let value = hash.expect("saved rows have hash values");
                let batch = table.batch_of(value);
                if batch != table.current {
                    table
                        .outer_files
                        .push((batch, saved))
                        .map_err(|_| arena_full())?;
                    continue;
                }
                if !self.probe_batch(table.frozen(), None, hash, &probe, &mut row, sink)? {
                    return Ok(false);
                }
            }
            if fill_inner {
                let mut rows = ArenaList::new(self.arena);
                table.unmatched(&mut rows)?;
                if !self.fill_inner_rows(rows.as_slice(), &probe, outer_relids, bound, sink)? {
                    return Ok(false);
                }
            }
            hash_node.hash_table(table.figures(id));
        }
    }

    /// Returns each unmatched inner row with NULLs for the outer relation
    /// when the join's filters pass it (`HJ_FILL_INNER_TUPLES`).
    fn fill_inner_rows(
        &self,
        rows: &[Row<'a>],
        probe: &Probe<'_>,
        outer_relids: Relids,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        for row in rows {
            self.bind(bound, row, probe.inner_relids);
            self.null_extend(bound, outer_relids);
            if self.equal(bound, probe.quals.1)? {
                probe.join.row();
                if !sink(bound)? {
                    return Ok(false);
                }
            } else {
                probe.join.filtered();
            }
        }
        Ok(true)
    }

    /// Builds the table of path `inner`'s rows (`MultiExecPrivateHash`).
    fn build_batches(
        &self,
        inner: u32,
        keys: &KeySides,
        sizing: super::index_paths::joins::HashSizing,
        hash_node: super::instrument::Node,
        keep_nulls: bool,
        bound: &mut Bound<'a>,
    ) -> Result<BatchTable<'a>, SqlError> {
        let _hash = hash_node.start();
        let held = self.hold(inner, bound)?;
        let mut table = BatchTable::new(self.arena, sizing)?;
        // `ExecHashBuildSkewHash`: the outer key's most common values, when
        // the join hashes one key of a stored column into several batches.
        if sizing.batches > 1 && self.skewed(keys) {
            self.build_skew(&mut table, keys[0].0, sizing)?;
        }
        for row in held.rows.borrowed_slice() {
            let Some(hash) = self.key_hash(row, keys, false, keep_nulls)? else {
                continue;
            };
            let size = 16 + self.tuple_len(row, inner)?;
            table.insert(HashedRow { row, hash, size }, self.arena)?;
            table.total += 1;
        }
        if table.buckets != table.optimal_buckets {
            table.increase_buckets()?;
        }
        table.space_used += table.buckets as usize * 8;
        table.peak = table.peak.max(table.space_used);
        table.ready(self, keys)?;
        hash_node.rows(table.total as u64);
        hash_node.hash_table(table.figures(inner));
        Ok(table)
    }

    /// Probes the current batch (or skew bucket `skew`) for the bound outer
    /// row of hash `hash`, passing each match (`HJ_SCAN_BUCKET`), then an
    /// unmatched outer row with NULLs when the join returns those
    /// (`HJ_FILL_OUTER_TUPLE`). A NULL key (`hash` `None`) matches nothing.
    fn probe_batch(
        &self,
        frozen: Frozen<'a>,
        skew: Option<(&BatchTable<'a>, usize)>,
        hash: Option<u32>,
        probe: &Probe<'_>,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let mut chain = ArenaList::new(self.arena);
        if let Some(hash) = hash {
            match skew {
                Some((table, at)) => table.skew_chain(at, &mut chain)?,
                None => frozen.chain_of(hash, &mut chain)?,
            }
        }
        let jointype = probe.jointype;
        let mut matched = false;
        // A NULL key matches nothing (the hash clauses are strict).
        let outer_null = self.has_null(bound, probe.keys, true);
        for &place in chain.as_slice() {
            let (row, flag) = match skew {
                Some((table, _)) => {
                    let (_, hashed, _) = table.skew_rows.borrowed_slice()[place as usize];
                    (hashed, &table.skew_matched[place as usize])
                }
                None => (
                    frozen.memory[place as usize].hashed,
                    &frozen.matched[place as usize],
                ),
            };
            if Some(row.hash) != hash
                || outer_null
                || self.has_null(row.row, probe.keys, false)
                || self.compare_sides(bound, row.row, probe.keys)? != Ordering::Equal
            {
                continue;
            }
            self.bind(bound, row.row, probe.inner_relids);
            if !self.equal(bound, probe.quals.0)? {
                probe.join.join_filtered();
                continue;
            }
            matched = true;
            // A right semi join returns each inner row at its first match.
            if flag.replace(true) && jointype == JoinType::RightSemi {
                continue;
            }
            if jointype == JoinType::Anti {
                break;
            }
            if jointype == JoinType::RightAnti {
                continue;
            }
            if self.equal(bound, probe.quals.1)? {
                probe.join.row();
                if !sink(bound)? {
                    return Ok(false);
                }
            } else {
                probe.join.filtered();
            }
            if jointype == JoinType::Semi {
                break;
            }
        }
        if !matched && jointype.fills_outer() {
            self.null_extend(bound, probe.inner_relids);
            if self.equal(bound, probe.quals.1)? {
                probe.join.row();
                if !sink(bound)? {
                    return Ok(false);
                }
            } else {
                probe.join.filtered();
            }
        }
        Ok(true)
    }

    /// The bound rows of `relids`, flat by the query's FROM positions: the
    /// outer row a parameterized scan or a `LATERAL` subquery reads.
    fn outer_values(&self, bound: &Bound<'a>, relids: Relids) -> Result<OuterValues<'a>, SqlError> {
        let n = self.scope.n;
        let offsets = self
            .arena
            .alloc_slice_with(n, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut width = 0;
        for (table, offset) in offsets.iter_mut().enumerate() {
            *offset = width;
            width += self.scope.stored_width(table);
        }
        let values = self
            .arena
            .alloc_slice_with(width, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for table in 0..n {
            if let Some(row) = bound[table]
                && relids.contains(table as u32)
            {
                let columns = self.columns_of(row.values);
                values[offsets[table]..offsets[table] + columns.len()].copy_from_slice(columns);
            }
        }
        let forms = self
            .arena
            .alloc_slice_with(width, |_| StoredForm::Inline)
            .map_err(|_| arena_full())?;
        self.flat_forms(bound, Some(relids), forms);
        Ok((values, offsets, forms))
    }

    /// A `LATERAL` subquery's rows for the bound outer row: its query run
    /// with the outer columns it reads bound, its nodes counted in the
    /// relation's block as `EXPLAIN` plans them.
    fn lateral_scan(
        &self,
        rel: usize,
        lateral: &super::index_paths::join_query::LateralRel<'a>,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let (values, offsets, forms) = self.outer_values(bound, lateral.relids)?;
        let outer_row = JoinRow {
            scope: self.scope,
            values: JoinRowValues::Flat {
                values,
                offsets,
                forms: Some(forms),
            },
            rowids: &[],
            visible: self.scope.n,
        };
        let mut rows = ArenaList::new(self.arena);
        {
            let _block = super::instrument::enter_block(self.block.child(scan_segment(rel)));
            super::select_into_rows(
                self.storage,
                self.txid,
                lateral.select,
                self.arena,
                self.params,
                Some(&outer_row),
                self.hooks.sequences,
                &mut |values, forms| {
                    let encoded =
                        crate::sql::exec::encode_projected_formed(values, forms, self.arena)?;
                    rows.push(encoded).map_err(|_| arena_full())
                },
            )?;
        }
        let columns = lateral.widths.len();
        let rows = rows.into_mut_slice().map_err(|_| arena_full())?;
        for (at, encoded) in rows.iter().enumerate() {
            let values = self
                .arena
                .alloc_slice_with(columns + self.model.expressions.len(), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (column, value) in values.iter_mut().take(columns).enumerate() {
                *value =
                    crate::sql::exec::decode_projected_col_record(encoded, column, self.arena)?;
            }
            let forms = self
                .arena
                .alloc_slice_with(columns, |column| {
                    crate::sql::exec::projected_stored_form(encoded, column)
                })
                .map_err(|_| arena_full())?;
            bound[rel] = Some(BaseRow {
                values,
                forms,
                rowid: None,
            });
            if !sink(bound)? {
                // The subquery ran to its end: its nodes' counts would
                // exceed PostgreSQL's, which stops reading it here.
                if at + 1 < rows.len() && super::instrument::active() {
                    return Err(crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN ANALYZE of a LATERAL subquery its join stops reading is not supported"
                    ));
                }
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Runs path `path` of relation `rel` (search path `id`) as planned: the
    /// scan's own planning describes that path again, and a parameterized
    /// path reads the bound outer rows' values.
    fn scan(
        &self,
        id: u32,
        rel: usize,
        path: u16,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let modeled = self.model.rels[rel];
        if let Some(lateral) = modeled.lateral.as_ref() {
            return self.lateral_scan(rel, lateral, bound, sink);
        }
        let mut record = self.scan_nodes[id as usize].try_borrow_mut().map_err(|_| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "a join's scan ran inside itself"
            )
        })?;
        // The scan's nodes and the rows it returns count in its relation's
        // block.
        let _block = super::instrument::enter_block(self.block.child(scan_segment(rel)));
        if record.is_none() {
            let scan = modeled.scan(self.storage, self.txid, path, self.params, self.arena)?;
            let node = super::scan::PathScan::new(
                self.storage,
                self.txid,
                modeled.from,
                scan.condition,
                scan.path,
                self.arena,
                self.params,
                self.hooks,
            )?;
            let outer = self
                .arena
                .alloc_slice_with(self.width, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            let outer_forms = self
                .arena
                .alloc_slice_with(self.width, |_| StoredForm::Inline)
                .map_err(|_| arena_full())?;
            *record = Some(ScanRecord {
                node,
                param: scan.param,
                outer,
                outer_forms,
            });
        }
        let record = record.as_mut().expect("the scan's node was made");
        let ScanRecord {
            node,
            param,
            outer: outer_values,
            outer_forms,
        } = record;
        // The bound rows of the relations the scan's join clauses read.
        let outer_row;
        let outer: Option<&dyn ColumnLookup<'a>> = if *param != Relids::EMPTY {
            for table in 0..self.scope.n {
                let start = self.offsets[table];
                let width = self.scope.stored_width(table);
                let slots = &mut outer_values[start..start + width];
                match bound[table] {
                    Some(row) if param.contains(table as u32) => {
                        let columns = self.columns_of(row.values);
                        slots[..columns.len()].copy_from_slice(columns);
                        slots[columns.len()..].fill(Datum::Null);
                    }
                    _ => slots.fill(Datum::Null),
                }
            }
            self.flat_forms(bound, Some(*param), outer_forms);
            outer_row = JoinRow {
                scope: self.scope,
                values: JoinRowValues::Flat {
                    values: outer_values,
                    offsets: self.offsets,
                    forms: Some(outer_forms),
                },
                rowids: &[],
                visible: self.scope.n,
            };
            Some(&outer_row)
        } else {
            None
        };
        node.rescan();
        while node.next(
            self.storage,
            crate::storage::CursorScratch::Statement,
            self.arena,
            self.params,
            self.hooks,
            outer,
        )? {
            let columns = node.values();
            let values = self
                .arena
                .alloc_slice_with(columns.len() + self.model.expressions.len(), |_| {
                    Datum::Null
                })
                .map_err(|_| arena_full())?;
            values[..columns.len()].copy_from_slice(columns);
            // The relation's expression members, computed by its scan.
            let row = node.row();
            for (id, expression) in self.model.expressions.iter().enumerate() {
                if expression.of(rel) {
                    values[columns.len() + id] = crate::sql::eval::eval_full(
                        expression.expression,
                        self.arena,
                        self.params,
                        &row,
                        self.hooks,
                    )?;
                }
            }
            bound[rel] = Some(BaseRow {
                values,
                forms: node.stored_forms(self.arena)?,
                rowid: node.rowid(),
            });
            if !sink(bound)? {
                node.stop();
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Every row of path `id`, in its order.
    fn hold(&self, id: u32, bound: &mut Bound<'a>) -> Result<Held<'a>, SqlError> {
        let mut rows = ArenaList::new(self.arena);
        let saved = *bound;
        self.exec(id, bound, &mut |bound| {
            rows.push(self.keep(bound)?).map_err(|_| arena_full())?;
            Ok(true)
        })?;
        *bound = saved;
        Ok(Held { rows })
    }

    /// A `Unique` node: the first input row of each run or group of equal
    /// compared columns, in the order its sort or hash table returns them.
    fn unique(
        &self,
        input: u32,
        keys: &[super::index_paths::joins::Member],
        hashed: Option<f64>,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let held = self.hold(input, bound)?;
        let rows = held.rows.borrowed_slice();
        let relids = self.model.plan.path(input).relids;
        let (members, count) = JoinContext::unique_members(keys);
        let members = &members[..count];
        let (key_rows, sizes) = self.keyed(rows, members, input)?;
        let order = match hashed {
            Some(groups) => self.unique_hashed_order(input, key_rows, members, groups)?,
            None => self.unique_order(input, key_rows, sizes, members)?,
        };
        for &at in order {
            self.bind(bound, rows[at as usize], relids);
            if !sink(bound)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// The orders the nodes read stored rows in, each over the rows decoded
/// into the arena.
impl<'r, 'a> JoinContext<'r, 'a> {
    /// Whether two Memoize keys are equal: NULLs equal each other.
    fn same_key(
        &self,
        held: &[Datum<'a>],
        key: &[Datum<'a>],
        members: &[super::index_paths::joins::Member],
        binary: bool,
    ) -> Result<bool, SqlError> {
        for ((left, right), member) in held.iter().zip(key).zip(members) {
            match (left.is_null(), right.is_null()) {
                (true, true) => continue,
                (true, false) | (false, true) => return Ok(false),
                (false, false) => {}
            }
            let collation = self.collation((usize::from(member.rel), usize::from(member.column)));
            if crate::sql::eval::compare_datums_collated(
                self.storage,
                self.txid,
                collation,
                left,
                right,
            )? != Ordering::Equal
            {
                return Ok(false);
            }
            // Binary mode (`datum_image_eq`) tells apart equal values whose
            // stored images differ: numerics of different scales, zeros of
            // either sign, intervals of different units and times in
            // different zones.
            if binary && !same_image(left, right) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `ExecInitSort`: a sort by `keys` of path `id`'s rows whose tuple is
    /// a single column sorts datums.
    fn sorts_datums(&self, id: u32, keys: &[(usize, usize)]) -> bool {
        let mut columns = 0;
        self.sort_columns(id, keys, &mut |_| columns += 1);
        columns == 1
    }

    /// The sizes a tuplesort by `keys` of path `id`'s rows charges for the
    /// bound row: its tuple's `t_len` and what a run writes of it.
    fn sort_sizes(
        &self,
        row: &[Option<BaseRow<'a>>],
        id: u32,
        keys: &[(usize, usize)],
    ) -> Result<(usize, usize), SqlError> {
        self.tuple_sizes(row, id, keys)
    }

    /// The values of `members` in each of `rows`, and each row's sizes under
    /// path `id`, as the orders below read them.
    fn keyed(
        &self,
        rows: &[Row<'a>],
        members: &[(usize, usize)],
        id: u32,
    ) -> Result<KeyedRows<'a>, SqlError> {
        let keys = self
            .arena
            .alloc_slice_with(rows.len(), |_| -> &'a [Datum<'a>] { &[] })
            .map_err(|_| arena_full())?;
        let sizes = self
            .arena
            .alloc_slice_with(rows.len(), |_| (0usize, 0usize))
            .map_err(|_| arena_full())?;
        for ((key, size), row) in keys.iter_mut().zip(sizes.iter_mut()).zip(rows) {
            *key = self
                .arena
                .alloc_slice_with(members.len(), |at| self.value(row, members[at]))
                .map_err(|_| arena_full())?;
            *size = self.sort_sizes(row, id, members)?;
        }
        Ok((keys, sizes))
    }

    /// The members a sort by `keys` of a path of `relids` compares, each
    /// with its key's flags.
    fn sort_members(&self, keys: &Keys, relids: Relids) -> ([(usize, usize); 8], [u8; 8], usize) {
        let mut members = [(0usize, 0usize); 8];
        let mut flags = [0u8; 8];
        for (at, &key) in keys.as_slice().iter().enumerate() {
            let class = key & KEY_CLASS;
            let member = self.model.plan.classes[usize::from(class)]
                .sort_member(relids)
                .expect("a sort key reads its input");
            members[at] = self.model.member(class, member as u8);
            flags[at] = key;
        }
        (members, flags, keys.as_slice().len())
    }

    /// Each compared member's column, for its collation.
    fn unique_members(
        keys: &[super::index_paths::joins::Member],
    ) -> ([(usize, usize); super::index_paths::joins::MAX_KEYS], usize) {
        let mut members = [(0usize, 0usize); super::index_paths::joins::MAX_KEYS];
        for (slot, member) in members.iter_mut().zip(keys) {
            *slot = (usize::from(member.rel), usize::from(member.column));
        }
        (members, keys.len())
    }

    /// A `Unique` over a `Sort` of a semi join's compared columns, whose
    /// values each row of `keys` holds: the rows' positions sorted by them
    /// (`pg_qsort`, NULLs last), the first of each run of equal values,
    /// NULLs equal (`execTuplesUnique`).
    fn unique_order(
        &self,
        input: u32,
        keys: &[&[Datum<'a>]],
        sizes: &[(usize, usize)],
        members: &[(usize, usize)],
    ) -> Result<&'a [u32], SqlError> {
        let compare = |a: u32, b: u32| -> Result<Ordering, SqlError> {
            for (at, &member) in members.iter().enumerate() {
                let (one, two) = (&keys[a as usize][at], &keys[b as usize][at]);
                let order = match (one.is_null(), two.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => crate::sql::eval::compare_datums_collated(
                        self.storage,
                        self.txid,
                        self.collation(member),
                        one,
                        two,
                    )?,
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(Ordering::Equal)
        };
        let (order, _) = self.tuplesort((input, members), sizes, &mut |a, b| compare(a, b))?;
        let kept = self
            .arena
            .alloc_slice_with(order.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        let mut count = 0;
        for &at in order {
            if count > 0 && compare(kept[count - 1], at)? == Ordering::Equal {
                continue;
            }
            kept[count] = at;
            count += 1;
        }
        Ok(&kept[..count])
    }

    /// A `HashAggregate` making a semi join's inner relation unique: the
    /// positions of the first row of each group of equal compared columns,
    /// whose values each row of `keys` holds (NULLs equal), in the order
    /// `ExecAgg` walks its `simplehash` table (`TupleHashTableHash`'s
    /// `murmurhash32` of the combined hashes, robin hood insertion, growth
    /// by doubling, `SH_START_ITERATE` from the first empty bucket
    /// backwards).
    fn unique_hashed_order(
        &self,
        input: u32,
        keys: &[&[Datum<'a>]],
        members: &[(usize, usize)],
        groups: f64,
    ) -> Result<&'a [u32], SqlError> {
        let width = self.model.plan.path(input).width;
        let row_hash = |row: usize| -> Result<u32, SqlError> {
            let mut hash = 0u32;
            for (at, value) in keys[row].iter().enumerate() {
                if at != 0 {
                    hash = hash.rotate_left(1);
                }
                if value.is_null() {
                    continue;
                }
                hash ^= datum_hash(value).ok_or_else(|| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "a hashed unique-ified relation over keys of this type is not supported"
                    )
                })?;
            }
            Ok(super::simplehash::murmur32(hash))
        };
        let same = |a: usize, b: usize| -> Result<bool, SqlError> {
            for (at, &member) in members.iter().enumerate() {
                let (one, two) = (&keys[a][at], &keys[b][at]);
                let equal = match (one.is_null(), two.is_null()) {
                    (true, true) => true,
                    (false, false) => {
                        crate::sql::eval::compare_datums_collated(
                            self.storage,
                            self.txid,
                            self.collation(member),
                            one,
                            two,
                        )? == Ordering::Equal
                    }
                    _ => false,
                };
                if !equal {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let sizing = super::index_paths::plain_hash_sizing(width, groups, members.len())
            .ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hashed unique-ified relation over {} columns is not supported",
                    members.len()
                )
            })?;
        let hashes = self
            .arena
            .alloc_slice_with(keys.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        let lens = self
            .arena
            .alloc_slice_with(keys.len(), |_| 0usize)
            .map_err(|_| arena_full())?;
        for row in 0..keys.len() {
            hashes[row] = row_hash(row)?;
            lens[row] = super::postgres_memory::minimal_tuple_len_with_tids(
                None,
                keys[row].iter().copied(),
                0,
            )
            .ok_or_else(super::instrument::unsized_value)?;
        }
        let order = self
            .arena
            .alloc_slice_with(keys.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        let (count, _) = super::hashagg::hashed_row_order(
            self.storage.plan_build(),
            sizing,
            hashes,
            lens,
            same,
            order,
        )?;
        Ok(&order[..count])
    }

    /// The order of rows whose sort key values each row of `keys` holds
    /// after a sort by `members` with `flags` (PostgreSQL's tuplesort,
    /// NULLs last unless a key puts them first), and the space the sort
    /// reports.
    fn sort_order(
        &self,
        id: u32,
        keys: &[&[Datum<'a>]],
        sizes: &[(usize, usize)],
        (members, flags): (&[(usize, usize)], &[u8]),
    ) -> Result<(&'a [u32], super::instrument::SortSpace), SqlError> {
        self.tuplesort((id, members), sizes, &mut |a, b| {
            for (at, (&member, &key)) in members.iter().zip(flags).enumerate() {
                let (one, two) = (&keys[a as usize][at], &keys[b as usize][at]);
                let first = key & KEY_NULLS_FIRST != 0;
                let order = match (one.is_null(), two.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) if first => Ordering::Less,
                    (true, false) => Ordering::Greater,
                    (false, true) if first => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => {
                        let order = crate::sql::eval::compare_datums_collated(
                            self.storage,
                            self.txid,
                            self.collation(member),
                            one,
                            two,
                        )?;
                        if key & KEY_DESCENDING != 0 {
                            order.reverse()
                        } else {
                            order
                        }
                    }
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(Ordering::Equal)
        })
    }

    /// PostgreSQL's tuplesort of the `id` path's rows, of `sizes`, in arrival order, as
    /// `tuplesort.c` runs it: each tuple (the path's target list) is charged
    /// as it arrives, a sort that outgrows `work_mem` forms its runs where
    /// `dumptuples` does and merges them as `mergeruns` does, any other is
    /// one `pg_qsort`. Rows `compare` ranks equal come out in that order.
    fn tuplesort(
        &self,
        (id, keys): (u32, &[(usize, usize)]),
        sizes: &[(usize, usize)],
        compare: &mut dyn FnMut(u32, u32) -> Result<Ordering, SqlError>,
    ) -> Result<(&'a [u32], super::instrument::SortSpace), SqlError> {
        use super::postgres_memory::{SortTuples, TuplesortModel};
        let mut model = TuplesortModel::new(
            if self.sorts_datums(id, keys) {
                SortTuples::Datum
            } else {
                SortTuples::Bump
            },
            None,
        );
        // The row counts of the runs `dumptuples` wrote.
        let runs = self
            .arena
            .alloc_slice_with(sizes.len() + 1, |_| 0u32)
            .map_err(|_| arena_full())?;
        let mut run_count = 0usize;
        for &(length, written) in sizes {
            if let Some(run) = model.push(length, written) {
                runs[run_count] = run as u32;
                run_count += 1;
            }
        }
        if let Some(run) = model.finish_run() {
            runs[run_count] = run as u32;
            run_count += 1;
        }
        let order = self
            .arena
            .alloc_slice_with(sizes.len(), |at| at as u32)
            .map_err(|_| arena_full())?;
        if let Some(merge_order) = model.merge_order() {
            let scratch = crate::sql::pg_qsort::RunScratch {
                moved: self
                    .arena
                    .alloc_slice_with(sizes.len(), |_| 0u32)
                    .map_err(|_| arena_full())?,
                bounds: self
                    .arena
                    .alloc_slice_with(2 * (run_count + 1), |_| 0u32)
                    .map_err(|_| arena_full())?,
                heap: self
                    .arena
                    .alloc_slice_with(merge_order, |_| 0u32)
                    .map_err(|_| arena_full())?,
                cursor: self
                    .arena
                    .alloc_slice_with(merge_order, |_| 0u32)
                    .map_err(|_| arena_full())?,
            };
            crate::sql::pg_qsort::pg_tuplesort_runs(
                order,
                &runs[..run_count],
                merge_order,
                scratch,
                compare,
            )?;
        } else {
            crate::sql::pg_qsort::pg_tuplesort(order, None, compare)?;
        }
        Ok((
            order,
            super::instrument::SortSpace::from_postgres(model.space(None), false),
        ))
    }
}
