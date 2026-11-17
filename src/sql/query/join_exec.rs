//! Runs the join plan PostgreSQL's join search chose
//! ([`super::index_paths::join_query`]), so its rows come back in
//! PostgreSQL's order: a nested loop reads its inner input again (or its
//! `Materialize` copy) for each outer row; a hash join builds its table on
//! the first outer row, stops at an empty one, and returns each outer row's
//! matches newest first, as `ExecHashJoin` walks a bucket's chain; a merge
//! join reads its sorted outer input first (its inner one only when the
//! outer has rows) and returns each outer row's run of inner matches in
//! inner order. Sorts order as PostgreSQL's `pg_qsort` does.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeHashjoin.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeMergejoin.c>

use core::cmp::Ordering;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::Collation;
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError};
use crate::sql::types::Datum;
use crate::storage::Storage;

use super::index_paths::join_query::ModeledJoin;
use super::index_paths::joins::{ClauseList, Keys, MAX_JOIN_RELS, PathKind};
use super::scan::{JoinRow, JoinRowValues, PaxFullRowReason, PaxReadDemand};
use super::{QueryScope, arena_full};

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

/// One base row: its values and row identity.
#[derive(Clone, Copy)]
struct BaseRow<'a> {
    values: &'a [Datum<'a>],
    rowid: Option<u64>,
}

/// A hash table or `Materialize` store a node keeps across rescans.
#[derive(Clone, Copy)]
struct Built<'a> {
    rows: &'a [Bound<'a>],
    index: &'a [u32],
}

/// A row a batched hash table holds: its hash value and its size as
/// `spaceUsed` counts it (`HJTUPLE_OVERHEAD` and the minimal tuple).
#[derive(Clone, Copy)]
struct HashedRow<'a> {
    row: Bound<'a>,
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
    outer_files: ArenaList<'a, (u32, Bound<'a>)>,
    /// `memory` ordered by hash, newest prepend first, once loaded.
    index: &'a [u32],
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
        Ok(())
    }

    /// Skew bucket `bucket`'s rows, chain head (newest) first.
    fn skew_chain(
        &self,
        bucket: usize,
        out: &mut ArenaList<'a, HashedRow<'a>>,
    ) -> Result<(), SqlError> {
        for &(at, hashed, _) in self.skew_rows.borrowed_slice().iter().rev() {
            if at as usize == bucket {
                out.push(hashed).map_err(|_| arena_full())?;
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

/// A built table's main rows, ordered for probing.
#[derive(Clone, Copy)]
struct Frozen<'a> {
    memory: &'a [MemoryRow<'a>],
    /// `memory` ordered by hash, newest prepend first.
    index: &'a [u32],
}

impl<'a> Frozen<'a> {
    /// The main table's rows of `hash`, chain head first.
    fn chain_of(&self, hash: u32, out: &mut ArenaList<'a, HashedRow<'a>>) -> Result<(), SqlError> {
        let rows = self.memory;
        let start = self
            .index
            .partition_point(|&at| rows[at as usize].hashed.hash < hash);
        for &at in &self.index[start..] {
            let row = rows[at as usize];
            if row.hashed.hash != hash {
                break;
            }
            out.push(row.hashed).map_err(|_| arena_full())?;
        }
        Ok(())
    }
}

/// Whether [`datum_hash`] hashes values of `ctype`.
fn datum_type_hashable(ctype: crate::sql::types::ColType) -> bool {
    use crate::sql::types::ColType;
    matches!(
        ctype,
        ColType::Int2
            | ColType::Int4
            | ColType::Int8
            | ColType::Date
            | ColType::Bool
            | ColType::Text
    )
}

/// The value hashes `ExecHashGetHashValue` combines: each type's hash
/// support function (`hashint4`, `hashint8`, `hashtext` under a
/// deterministic collation, `hashchar` for `bool`).
fn datum_hash(value: &Datum<'_>) -> Option<u32> {
    use crate::sql::identity::{hash_bytea, hash_int64_input, hash_uint32};
    Some(match value {
        Datum::Int2(value) => hash_uint32(*value as i32 as u32),
        Datum::Int4(value) | Datum::Date(value) => hash_uint32(*value as u32),
        Datum::Int8(value) => hash_uint32(hash_int64_input(*value)),
        Datum::Bool(value) => hash_uint32(u32::from(*value)),
        Datum::Text(value) => hash_bytea(value.as_bytes()) as u32,
        _ => return None,
    })
}

/// Each key clause's (outer, inner) column; unused entries hold `usize::MAX`.
type KeySides = [((usize, usize), (usize, usize)); 16];

/// The rows bound so far, by FROM position.
type Bound<'a> = [Option<BaseRow<'a>>; MAX_JOIN_RELS];

struct Runner<'r, 'a> {
    /// Each path's kept hash table or store, which a rescan reads again
    /// (`ExecReScanHashJoin`, `ExecReScanMaterial`).
    built: &'a [core::cell::Cell<Option<Built<'a>>>],
    /// Each hash join's one-batch table, which a rescan probes again.
    frozen: &'a [core::cell::Cell<Option<Frozen<'a>>>],
    /// The block the join's nodes count in.
    block: super::instrument::BlockId,
    storage: &'a Storage,
    scope: &'r QueryScope<'a>,
    txid: u32,
    model: &'r ModeledJoin<'a>,
    arena: &'a Arena,
    params: &'r [Datum<'a>],
    hooks: &'r EvalHooks<'r, 'a>,
}

/// Scans `statement`'s FROM clause: by the plan PostgreSQL's join search
/// chose when it plans the query, otherwise as `scan_source` does.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_statement_source<'a>(
    statement: &'a crate::sql::ast::Select<'a>,
    recycle_rows: bool,
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a crate::sql::ast::Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    pax_demand: PaxReadDemand,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    if outer.is_none()
        && let Some(model) = super::index_paths::join_query::modeled_join(
            storage, txid, statement, scope, params, arena,
        )?
    {
        return run(storage, scope, txid, &model, arena, params, hooks, visit);
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

/// Runs `model`'s plan, passing each joined row to `visit` until it
/// returns false.
#[allow(clippy::too_many_arguments)]
fn run<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    model: &ModeledJoin<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    visit: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let n = scope.n;
    let offsets = arena
        .alloc_slice_with(n, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut width = 0;
    for (table, offset) in offsets.iter_mut().enumerate() {
        *offset = width;
        width += scope.stored_width(table);
    }
    let offsets: &[usize] = offsets;
    let values = arena
        .alloc_slice_with(width, |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let rowids = arena
        .alloc_slice_with(n, |_| None::<u64>)
        .map_err(|_| arena_full())?;
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
    let runner = Runner {
        built,
        frozen,
        block,
        storage,
        scope,
        txid,
        model,
        arena,
        params,
        hooks,
    };
    let mut bound: Bound<'a> = [None; MAX_JOIN_RELS];
    runner.exec(model.plan.top, &mut bound, &mut |bound| {
        for table in 0..n {
            let row = bound[table].expect("the top join binds every relation");
            let start = offsets[table];
            values[start..start + row.values.len()].copy_from_slice(row.values);
            rowids[table] = row.rowid;
        }
        let _block = super::instrument::enter_block(block);
        visit(&JoinRow {
            scope,
            values: JoinRowValues::Flat { values, offsets },
            rowids,
            visible: n,
        })
    })?;
    Ok(())
}

/// The rows a materialized input holds: each one's base rows, by FROM
/// position.
struct Held<'a> {
    rows: ArenaList<'a, Bound<'a>>,
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
            PathKind::Scan { rel } => self.scan(usize::from(rel), bound, sink),
            PathKind::Material { input } => self.exec(input, bound, sink),
            PathKind::NestLoop {
                outer,
                inner,
                clauses,
            } => {
                let join = node(super::instrument::Role::Join, id);
                let _join = join.start();
                let material = match self.model.plan.path(inner).kind {
                    PathKind::Material { input } => Some(input),
                    _ => None,
                };
                let material_node = node(super::instrument::Role::Materialize, inner);
                let inner_relids = self.model.plan.path(inner).relids;
                self.exec(outer, bound, &mut |bound| {
                    let mut pass = |bound: &mut Bound<'a>| -> Result<bool, SqlError> {
                        if !self.equal(bound, &clauses)? {
                            join.join_filtered();
                            return Ok(true);
                        }
                        join.row();
                        sink(bound)
                    };
                    let Some(input) = material else {
                        return self.exec(inner, bound, &mut pass);
                    };
                    let _material = material_node.start();
                    let store = match self.built[inner as usize].get() {
                        Some(store) => store,
                        None => {
                            let store = Built {
                                rows: self.hold(input, bound)?.rows.as_slice(),
                                index: &[],
                            };
                            // The tuplestore `EXPLAIN ANALYZE` reports.
                            for row in store.rows {
                                material_node.store_row(self.tuple_len(row, inner).ok());
                            }
                            self.built[inner as usize].set(Some(store));
                            store
                        }
                    };
                    for row in store.rows {
                        material_node.row();
                        self.bind(bound, row, inner_relids);
                        if !pass(bound)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })
            }
            PathKind::HashJoin {
                outer,
                inner,
                clauses,
                hash,
                buckets,
                batches,
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let inner_relids = self.model.plan.path(inner).relids;
                let keys = self.sides(&hash, outer_relids);
                let join = node(super::instrument::Role::Join, id);
                let hash_node = node(super::instrument::Role::Hash, id);
                let _join = join.start();
                if self.hashable(&keys) {
                    return self.batched_hash_join(
                        id, outer, inner, &clauses, &keys, join, hash_node, bound, sink,
                    );
                }
                debug_assert_eq!(batches, 1, "planned batches need hashable keys");
                let build = |bound: &mut Bound<'a>| -> Result<Built<'a>, SqlError> {
                    let _hash = hash_node.start();
                    let held = self.hold(inner, bound)?;
                    let (index, bytes) = self.hash_index(&held, &keys, inner, buckets)?;
                    // A table that outgrows its memory splits into batches,
                    // which rows of these key types cannot be assigned to.
                    let inner_path = self.model.plan.path(inner);
                    let sizing = super::index_paths::joins::choose_hash_table_size(
                        inner_path.rows,
                        inner_path.width,
                        self.model.plan.settings,
                    );
                    let mut final_buckets = buckets as usize;
                    while index.len() > final_buckets + 1 {
                        final_buckets *= 2;
                    }
                    if bytes + final_buckets * 8 > sizing.space_allowed as usize {
                        return Err(crate::sql_err!(
                            crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                            "a hash join whose table outgrows its memory over keys of this type is not supported"
                        ));
                    }
                    hash_node.rows(index.len() as u64);
                    hash_node.hash_table(super::instrument::HashTable {
                        tuples: index.len() as u64,
                        tuple_bytes: Some(bytes),
                        own_bytes: 0,
                        batches: None,
                    });
                    let built = Built {
                        rows: held.rows.as_slice(),
                        index,
                    };
                    self.built[id as usize].set(Some(built));
                    Ok(built)
                };
                // `ExecHashJoin` reads the first outer row before building
                // its table when the outer plan starts cheaper than the
                // table costs, and ends at an empty table; a rescan reuses
                // the table and reads every outer row.
                let mut table = self.built[id as usize].get();
                if table.is_none()
                    && self.model.plan.path(outer).startup >= self.model.plan.path(inner).total
                {
                    let built = build(bound)?;
                    if built.index.is_empty() {
                        return Ok(true);
                    }
                    table = Some(built);
                }
                let mut ended = false;
                let kept = self.exec(outer, bound, &mut |bound| {
                    if table.is_none() {
                        let built = build(bound)?;
                        if built.index.is_empty() {
                            ended = true;
                            return Ok(false);
                        }
                        table = Some(built);
                    }
                    let Built { rows, index } = table.expect("built");
                    let Some(start) = self.first_match(bound, rows, index, &keys)? else {
                        return Ok(true);
                    };
                    for &at in &index[start..] {
                        let row = &rows[at as usize];
                        if self.compare_sides(bound, row, &keys)? != Ordering::Equal {
                            break;
                        }
                        self.bind(bound, row, inner_relids);
                        if !self.equal(bound, &clauses)? {
                            join.join_filtered();
                            continue;
                        }
                        join.row();
                        if !sink(bound)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })?;
                Ok(kept || ended)
            }
            PathKind::MergeJoin {
                outer,
                inner,
                merge,
                outer_sort,
                inner_sort,
                clauses,
                ..
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let inner_relids = self.model.plan.path(inner).relids;
                let keys = self.sides(&merge, outer_relids);
                let outer_rows = self.hold(outer, bound)?;
                if outer_rows.rows.is_empty() {
                    return Ok(true);
                }
                let outer_order = self.sorted(&outer_rows, &outer_sort, outer_relids)?;
                let inner_rows = self.hold(inner, bound)?;
                let inner_order = self.sorted(&inner_rows, &inner_sort, inner_relids)?;
                let (outer_held, inner_held) = (
                    outer_rows.rows.borrowed_slice(),
                    inner_rows.rows.borrowed_slice(),
                );
                let mut mark = 0usize;
                for &o in outer_order {
                    let outer_row = &outer_held[o as usize];
                    if self.has_null(outer_row, &keys, true) {
                        continue;
                    }
                    self.bind(bound, outer_row, outer_relids);
                    while mark < inner_order.len() {
                        let inner_row = &inner_held[inner_order[mark] as usize];
                        if self.has_null(inner_row, &keys, false)
                            || self.compare_sides(bound, inner_row, &keys)? == Ordering::Greater
                        {
                            mark += 1;
                        } else {
                            break;
                        }
                    }
                    for &i in &inner_order[mark..] {
                        let inner_row = &inner_held[i as usize];
                        if self.has_null(inner_row, &keys, false)
                            || self.compare_sides(bound, inner_row, &keys)? != Ordering::Equal
                        {
                            break;
                        }
                        self.bind(bound, inner_row, inner_relids);
                        if self.equal(bound, &clauses)? && !sink(bound)? {
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            }
        }
    }

    /// A hash join PostgreSQL plans with several batches
    /// (`ExecHashJoin` with `ExecHashTableInsert`, `ExecHashIncreaseNumBatches`,
    /// the skew buckets of `ExecHashBuildSkewHash` and
    /// `ExecHashJoinNewBatch`): batch 0's outer rows probe as they are read,
    /// the others are saved to their batch, and each later batch is loaded
    /// and probed in turn.
    #[allow(clippy::too_many_arguments)]
    /// Whether every key's type has a hash function here.
    fn hashable(&self, keys: &KeySides) -> bool {
        Self::keys(keys).all(|&(outer, inner)| {
            [outer, inner].into_iter().all(|(table, column)| {
                datum_type_hashable(
                    self.scope
                        .defs
                        .stored(table)
                        .expect("a plain table")
                        .columns()[column]
                        .ctype,
                )
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn batched_hash_join(
        &self,
        id: u32,
        outer: u32,
        inner: u32,
        clauses: &ClauseList,
        keys: &KeySides,
        join: super::instrument::Node,
        hash_node: super::instrument::Node,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let inner_path = *self.model.plan.path(inner);
        let inner_relids = inner_path.relids;
        let sizing = super::index_paths::joins::choose_hash_table_size(
            inner_path.rows,
            inner_path.width,
            self.model.plan.settings,
        );
        // A rescan reads a one-batch table again (`ExecReScanHashJoin`).
        if let Some(frozen) = self.frozen[id as usize].get() {
            return self.exec(outer, bound, &mut |bound| {
                let Some(hash) = self.key_hash(bound, keys, true)? else {
                    return Ok(true);
                };
                self.probe_batch(
                    frozen,
                    None,
                    hash,
                    keys,
                    clauses,
                    inner_relids,
                    join,
                    bound,
                    sink,
                )
            });
        }
        let mut table: Option<BatchTable<'a>> = None;
        let built_first = self.model.plan.path(outer).startup >= inner_path.total;
        if built_first {
            let built = self.build_batches(inner, keys, sizing, hash_node, bound)?;
            if built.total == 0 {
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
                let built = self.build_batches(inner, keys, sizing, hash_node, bound)?;
                if built.total == 0 {
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
            let Some(hash) = self.key_hash(bound, keys, true)? else {
                return Ok(true);
            };
            let skew = table.skew_bucket(hash);
            if skew.is_none() && table.batch_of(hash) != 0 {
                let batch = table.batch_of(hash);
                table
                    .outer_files
                    .push((batch, *bound))
                    .map_err(|_| arena_full())?;
                return Ok(true);
            }
            let skew = skew.map(|at| (&*table, at));
            self.probe_batch(
                table.frozen(),
                skew,
                hash,
                keys,
                clauses,
                inner_relids,
                join,
                bound,
                sink,
            )
        })?;
        if !kept && !ended {
            return Ok(false);
        }
        let Some(mut table) = table else {
            return Ok(true);
        };
        if table.batches == 1 {
            self.frozen[id as usize].set(Some(table.frozen()));
            return Ok(true);
        }
        // `ExecHashJoinNewBatch`.
        table.skew_enabled = false;
        let outer_start = table.batches;
        let mut current = 0u32;
        loop {
            current += 1;
            let has = |files: &[(u32, HashedRow<'a>)], batch: u32| {
                files.iter().any(|(at, _)| *at == batch)
            };
            let has_outer =
                |files: &[(u32, Bound<'a>)], batch: u32| files.iter().any(|(at, _)| *at == batch);
            while current < table.batches
                && (!has_outer(table.outer_files.borrowed_slice(), current)
                    || !has(table.inner_files.borrowed_slice(), current))
            {
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
            let files = table.inner_files.as_slice();
            let mut rest = ArenaList::new(self.arena);
            let mut loaded = ArenaList::new(self.arena);
            for &(batch, hashed) in files {
                if batch == current {
                    loaded.push(hashed).map_err(|_| arena_full())?;
                } else {
                    rest.push((batch, hashed)).map_err(|_| arena_full())?;
                }
            }
            table.inner_files = rest;
            for &hashed in loaded.as_slice() {
                table.insert(hashed, self.arena)?;
            }
            let files = table.outer_files.as_slice();
            let mut rest = ArenaList::new(self.arena);
            let mut probes = ArenaList::new(self.arena);
            for &(batch, row) in files {
                if batch == current {
                    probes.push(row).map_err(|_| arena_full())?;
                } else {
                    rest.push((batch, row)).map_err(|_| arena_full())?;
                }
            }
            table.outer_files = rest;
            table.ready(self, keys)?;
            for outer_row in probes.as_slice() {
                let mut row = *outer_row;
                let hash = self
                    .key_hash(&row, keys, true)?
                    .expect("saved rows have keys");
                let batch = table.batch_of(hash);
                if batch != table.current {
                    table
                        .outer_files
                        .push((batch, row))
                        .map_err(|_| arena_full())?;
                    continue;
                }
                if !self.probe_batch(
                    table.frozen(),
                    None,
                    hash,
                    keys,
                    clauses,
                    inner_relids,
                    join,
                    &mut row,
                    sink,
                )? {
                    return Ok(false);
                }
            }
            hash_node.hash_table(table.figures(id));
        }
    }

    /// The combined hash of a row's join keys (`ExecHashGetHashValue`):
    /// `None` when a key is NULL.
    fn key_hash(
        &self,
        row: &Bound<'a>,
        keys: &KeySides,
        outer: bool,
    ) -> Result<Option<u32>, SqlError> {
        let mut hash = 0u32;
        for (at, &(o, i)) in Self::keys(keys).enumerate() {
            let value = Self::value(row, if outer { o } else { i });
            if value.is_null() {
                return Ok(None);
            }
            let one = datum_hash(&value).ok_or_else(|| {
                crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash join of several batches over keys of this type is not supported"
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

    /// Builds the table of path `inner`'s rows (`MultiExecPrivateHash`).
    fn build_batches(
        &self,
        inner: u32,
        keys: &KeySides,
        sizing: super::index_paths::joins::HashSizing,
        hash_node: super::instrument::Node,
        bound: &mut Bound<'a>,
    ) -> Result<BatchTable<'a>, SqlError> {
        let _hash = hash_node.start();
        let held = self.hold(inner, bound)?;
        let mut table = BatchTable::new(self.arena, sizing)?;
        // `ExecHashBuildSkewHash`: the outer key's most common values, when
        // the join hashes one key of a stored column into several batches.
        if sizing.batches > 1 && Self::keys(keys).count() == 1 {
            self.build_skew(&mut table, keys[0].0, sizing)?;
        }
        for row in held.rows.borrowed_slice() {
            let Some(hash) = self.key_hash(row, keys, false)? else {
                continue;
            };
            let size = 16 + self.tuple_len(row, inner)?;
            table.insert(
                HashedRow {
                    row: *row,
                    hash,
                    size,
                },
                self.arena,
            )?;
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

    fn build_skew(
        &self,
        table: &mut BatchTable<'a>,
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
        table.skew_enabled = true;
        table.skew_length = length;
        table.note_space(length as usize * 8 + count * 4);
        table.skew_used += length as usize * 8 + count * 4;
        for index in 0..count {
            let value = distribution.mcv_value(index, ctype)?;
            let Some(hash) = datum_hash(&value) else {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "a hash join of several batches over keys of this type is not supported"
                ));
            };
            // Open addressing in the skew array; a repeated hash is skipped.
            let mut bucket = hash & (length - 1);
            loop {
                match table
                    .skew
                    .borrowed_slice()
                    .iter()
                    .find(|held| held.position == bucket)
                {
                    Some(held) if held.hash == hash => break,
                    Some(_) => bucket = (bucket + 1) & (length - 1),
                    None => {
                        table
                            .skew
                            .push(SkewSlot {
                                position: bucket,
                                hash,
                                removed: false,
                            })
                            .map_err(|_| arena_full())?;
                        table.note_space(16);
                        table.skew_used += 16;
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Probes the current batch (or skew bucket `skew`) for the bound outer
    /// row of hash `hash`, passing each match.
    #[allow(clippy::too_many_arguments)]
    fn probe_batch(
        &self,
        frozen: Frozen<'a>,
        skew: Option<(&BatchTable<'a>, usize)>,
        hash: u32,
        keys: &KeySides,
        clauses: &ClauseList,
        inner_relids: u32,
        join: super::instrument::Node,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let mut chain = ArenaList::new(self.arena);
        match skew {
            Some((table, at)) => table.skew_chain(at, &mut chain)?,
            None => frozen.chain_of(hash, &mut chain)?,
        }
        for hashed in chain.as_slice() {
            if hashed.hash != hash
                || self.compare_sides(bound, &hashed.row, keys)? != Ordering::Equal
            {
                continue;
            }
            self.bind(bound, &hashed.row, inner_relids);
            if !self.equal(bound, clauses)? {
                join.join_filtered();
                continue;
            }
            join.row();
            if !sink(bound)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn scan(
        &self,
        rel: usize,
        bound: &mut Bound<'a>,
        sink: &mut dyn FnMut(&mut Bound<'a>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        let modeled = self.model.rels[rel];
        let single = QueryScope::resolve_exec(
            self.storage,
            modeled.from,
            modeled.restriction,
            self.txid,
            self.arena,
            self.params,
            self.hooks.sequences,
        )?;
        let mut keep = true;
        let _block = super::instrument::enter_block(self.block.child(scan_segment(rel)));
        super::scan::scan_source_with_pax_columns(
            self.storage,
            &single,
            modeled.from,
            self.txid,
            modeled.restriction,
            self.arena,
            self.params,
            self.hooks,
            None::<&dyn ColumnLookup<'a>>,
            PaxReadDemand::full_row(PaxFullRowReason::WildcardProjection),
            &mut |row| {
                let values = row.table_values(0).unwrap_or(&[]);
                let values = self
                    .arena
                    .alloc_slice_copy(values)
                    .map_err(|_| arena_full())?;
                bound[rel] = Some(BaseRow {
                    values,
                    rowid: row.rowids.first().copied().flatten(),
                });
                keep = sink(bound)?;
                Ok(keep)
            },
        )?;
        Ok(keep)
    }

    /// Every row of path `id`, in its order.
    fn hold(&self, id: u32, bound: &mut Bound<'a>) -> Result<Held<'a>, SqlError> {
        let mut rows = ArenaList::new(self.arena);
        let saved = *bound;
        self.exec(id, bound, &mut |bound| {
            rows.push(*bound).map_err(|_| arena_full())?;
            Ok(true)
        })?;
        *bound = saved;
        Ok(Held { rows })
    }

    fn bind(&self, bound: &mut Bound<'a>, row: &Bound<'a>, relids: u32) {
        for table in 0..self.scope.n {
            if relids & 1 << table != 0 {
                bound[table] = row[table];
            }
        }
    }

    fn value(row: &Bound<'a>, (table, column): (usize, usize)) -> Datum<'a> {
        row[table].map_or(Datum::Null, |row| row.values[column])
    }

    fn collation(&self, (table, column): (usize, usize)) -> Collation {
        self.scope
            .defs
            .stored(table)
            .expect("a plain table")
            .columns()[column]
            .collation
    }

    /// Each clause's (outer, inner) columns.
    fn sides(&self, clauses: &ClauseList, outer: u32) -> KeySides {
        let mut sides = [((usize::MAX, 0), (usize::MAX, 0)); 16];
        for (at, &id) in clauses.as_slice().iter().enumerate() {
            let (left, right) = self
                .model
                .clause_columns(&self.model.plan.clauses[usize::from(id)]);
            sides[at] = if outer & 1 << left.0 != 0 {
                (left, right)
            } else {
                (right, left)
            };
        }
        sides
    }

    fn keys(keys: &KeySides) -> impl Iterator<Item = &((usize, usize), (usize, usize))> {
        keys.iter().take_while(|key| key.0.0 != usize::MAX)
    }

    /// Whether every clause of `clauses` holds for the bound rows.
    fn equal(&self, bound: &Bound<'a>, clauses: &ClauseList) -> Result<bool, SqlError> {
        for &id in clauses.as_slice() {
            let (left, right) = self
                .model
                .clause_columns(&self.model.plan.clauses[usize::from(id)]);
            let (one, two) = (Self::value(bound, left), Self::value(bound, right));
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

    fn has_null(&self, row: &Bound<'a>, keys: &KeySides, outer: bool) -> bool {
        Self::keys(keys).any(|(o, i)| Self::value(row, if outer { *o } else { *i }).is_null())
    }

    /// The bound outer row's keys against `inner`'s keys.
    fn compare_sides(
        &self,
        bound: &Bound<'a>,
        inner: &Bound<'a>,
        keys: &KeySides,
    ) -> Result<Ordering, SqlError> {
        for &(o, i) in Self::keys(keys) {
            let order = crate::sql::eval::compare_datums_collated(
                self.storage,
                self.txid,
                self.collation(o),
                &Self::value(bound, o),
                &Self::value(inner, i),
            )?;
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    /// The `t_len` of the minimal tuple of path `id`'s target list for
    /// the bound row.
    fn tuple_len(&self, bound: &Bound<'a>, id: u32) -> Result<usize, SqlError> {
        let mut values = [Datum::Null; 128];
        let mut width = 0;
        let mut fits = true;
        self.model
            .plan
            .target_list(self.model.plan.path(id).relids, &mut |column| {
                if width < values.len() {
                    values[width] =
                        Self::value(bound, (usize::from(column.rel), usize::from(column.column)));
                    width += 1;
                } else {
                    fits = false;
                }
            });
        fits.then(|| {
            super::postgres_memory::minimal_tuple_len_with_tids(values[..width].iter().copied(), 0)
        })
        .flatten()
        .ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a join whose rows have no PostgreSQL size here is not supported"
            )
        })
    }

    /// The held inner rows without NULL keys, ordered by key and, among
    /// equal keys, in the order a probe walks their bucket's chain: newest
    /// first as `ExecHashTableInsert` prepends them, unless the table held
    /// more rows than its planned buckets, when `ExecHashIncreaseNumBuckets`
    /// prepends them again walking its memory chunks, newest chunk first.
    fn hash_index(
        &self,
        held: &Held<'a>,
        keys: &KeySides,
        inner: u32,
        buckets: u32,
    ) -> Result<(&'a [u32], usize), SqlError> {
        /// `HASH_CHUNK_SIZE` and `HASH_CHUNK_THRESHOLD`.
        const CHUNK: usize = 32 * 1024;
        const THRESHOLD: usize = CHUNK / 4;
        let rows = held.rows.borrowed_slice();
        let mut count = 0;
        let index = self
            .arena
            .alloc_slice_with(rows.len(), |_| 0u32)
            .map_err(|_| arena_full())?;
        for (at, row) in rows.iter().enumerate() {
            if !self.has_null(row, keys, false) {
                index[count] = at as u32;
                count += 1;
            }
        }
        let index = &mut index[..count];
        // Each inserted row's place in its chain: lower walks first.
        let rank = self
            .arena
            .alloc_slice_with(rows.len(), |_| 0i64)
            .map_err(|_| arena_full())?;
        // Each inserted row's minimal tuple length.
        let lengths = self
            .arena
            .alloc_slice_with(count, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut bytes = 0usize;
        for (at, &row) in index.iter().enumerate() {
            lengths[at] = self.tuple_len(&rows[row as usize], inner)?;
            // `HJTUPLE_OVERHEAD` and the tuple, as `spaceUsed` counts them.
            bytes += 16 + lengths[at];
        }
        if count > buckets as usize + 1 {
            // The chunk list as `dense_alloc` links it: a new chunk heads
            // it, an oversized row's own chunk goes after the head.
            let chunk_of = self
                .arena
                .alloc_slice_with(count, |_| 0u32)
                .map_err(|_| arena_full())?;
            let list = self
                .arena
                .alloc_slice_with(count, |_| 0u32)
                .map_err(|_| arena_full())?;
            // (maxlen, used) per chunk.
            let space = self
                .arena
                .alloc_slice_with(count, |_| (0usize, 0usize))
                .map_err(|_| arena_full())?;
            let mut chunks = 0usize;
            for at in 0..count {
                let length = lengths[at];
                let size = (16 + length).next_multiple_of(8);
                let id = chunks;
                if size > THRESHOLD {
                    space[id] = (size, size);
                    let position = usize::from(chunks > 0);
                    list.copy_within(position..chunks, position + 1);
                    list[position] = id as u32;
                    chunks += 1;
                    chunk_of[at] = id as u32;
                } else if chunks == 0 || {
                    let (maxlen, used) = space[list[0] as usize];
                    maxlen - used < size
                } {
                    space[id] = (CHUNK, size);
                    list.copy_within(0..chunks, 1);
                    list[0] = id as u32;
                    chunks += 1;
                    chunk_of[at] = id as u32;
                } else {
                    let head = list[0] as usize;
                    space[head].1 += size;
                    chunk_of[at] = head as u32;
                }
            }
            let place = self
                .arena
                .alloc_slice_with(chunks, |_| 0usize)
                .map_err(|_| arena_full())?;
            for (position, &id) in list[..chunks].iter().enumerate() {
                place[id as usize] = position;
            }
            // The walk: chunks in list order, each one's rows in insertion
            // order; prepending them in that order puts the last walked
            // first.
            let walk = self
                .arena
                .alloc_slice_with(count, |at| (place[chunk_of[at] as usize], at, index[at]))
                .map_err(|_| arena_full())?;
            walk.sort_unstable();
            let total = walk.len() as i64;
            for (walked, &(_, _, row)) in walk.iter().enumerate() {
                rank[row as usize] = total - walked as i64;
            }
        } else {
            for &row in index.iter() {
                rank[row as usize] = -i64::from(row);
            }
        }
        let rank: &[i64] = rank;
        crate::mem::arena::try_stable_sort_via(
            self.arena,
            index,
            |&a, &b| {
                for &(o, i) in Self::keys(keys) {
                    let order = crate::sql::eval::compare_datums_collated(
                        self.storage,
                        self.txid,
                        self.collation(o),
                        &Self::value(&rows[a as usize], i),
                        &Self::value(&rows[b as usize], i),
                    )?;
                    if order != Ordering::Equal {
                        return Ok(order);
                    }
                }
                Ok(rank[a as usize].cmp(&rank[b as usize]))
            },
            |_| arena_full(),
        )?;
        Ok((index, bytes))
    }

    /// The first position of `index` whose keys are not below the bound
    /// outer row's, when that row's keys have no NULL.
    fn first_match(
        &self,
        bound: &Bound<'a>,
        rows: &[Bound<'a>],
        index: &[u32],
        keys: &KeySides,
    ) -> Result<Option<usize>, SqlError> {
        if self.has_null(bound, keys, true) {
            return Ok(None);
        }
        let (mut low, mut high) = (0usize, index.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.compare_sides(bound, &rows[index[middle] as usize], keys)? == Ordering::Greater
            {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(Some(low))
    }

    /// The held rows' order after a sort by `keys` (`pg_qsort`, NULLs
    /// last), or as held without keys.
    fn sorted(&self, held: &Held<'a>, keys: &Keys, relids: u32) -> Result<&'a [u32], SqlError> {
        let rows = held.rows.borrowed_slice();
        let order = self
            .arena
            .alloc_slice_with(rows.len(), |at| at as u32)
            .map_err(|_| arena_full())?;
        if keys.as_slice().is_empty() {
            return Ok(order);
        }
        let mut columns = [(0usize, 0usize); 8];
        for (at, &class) in keys.as_slice().iter().enumerate() {
            let members = self.model.plan.classes[usize::from(class)].members();
            let member = members
                .iter()
                .position(|member| relids & 1 << member.rel != 0)
                .expect("a sort key reads its input");
            columns[at] = self.model.member(class, member as u8);
        }
        let columns = &columns[..keys.as_slice().len()];
        crate::sql::pg_qsort::pg_qsort(order, &mut |a, b| {
            for &column in columns {
                let (one, two) = (
                    Self::value(&rows[a as usize], column),
                    Self::value(&rows[b as usize], column),
                );
                let order = match (one.is_null(), two.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => crate::sql::eval::compare_datums_collated(
                        self.storage,
                        self.txid,
                        self.collation(column),
                        &one,
                        &two,
                    )?,
                };
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(Ordering::Equal)
        })?;
        Ok(order)
    }
}
