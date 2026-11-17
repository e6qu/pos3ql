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

/// Each key clause's (outer, inner) column; unused entries hold `usize::MAX`.
type KeySides = [((usize, usize), (usize, usize)); 16];

/// The rows bound so far, by FROM position.
type Bound<'a> = [Option<BaseRow<'a>>; MAX_JOIN_RELS];

struct Runner<'r, 'a> {
    /// Each path's kept hash table or store, which a rescan reads again
    /// (`ExecReScanHashJoin`, `ExecReScanMaterial`).
    built: &'a [core::cell::Cell<Option<Built<'a>>>],
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
    let runner = Runner {
        built,
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
                ..
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let inner_relids = self.model.plan.path(inner).relids;
                let keys = self.sides(&hash, outer_relids);
                let join = node(super::instrument::Role::Join, id);
                let hash_node = node(super::instrument::Role::Hash, id);
                let _join = join.start();
                let build = |bound: &mut Bound<'a>| -> Result<Built<'a>, SqlError> {
                    let _hash = hash_node.start();
                    let held = self.hold(inner, bound)?;
                    let (index, bytes) = self.hash_index(&held, &keys, inner, buckets)?;
                    hash_node.rows(index.len() as u64);
                    hash_node.hash_table(super::instrument::HashTable {
                        tuples: index.len() as u64,
                        tuple_bytes: Some(bytes),
                        own_bytes: 0,
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
        let path_relids = self.model.plan.path(inner).relids;
        // Each inserted row's minimal tuple length.
        let lengths = self
            .arena
            .alloc_slice_with(count, |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut bytes = 0usize;
        for (at, &row) in index.iter().enumerate() {
            let bound = &rows[row as usize];
            let mut values = [Datum::Null; 128];
            let mut width = 0;
            let mut fits = true;
            self.model.plan.target_list(path_relids, &mut |column| {
                if width < values.len() {
                    values[width] =
                        Self::value(bound, (usize::from(column.rel), usize::from(column.column)));
                    width += 1;
                } else {
                    fits = false;
                }
            });
            lengths[at] = fits
                .then(|| {
                    super::postgres_memory::minimal_tuple_len_with_tids(
                        values[..width].iter().copied(),
                        0,
                    )
                })
                .flatten()
                .ok_or_else(|| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "a hash join whose rows have no PostgreSQL size here is not supported"
                    )
                })?;
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
