//! Retained nested-loop joins: one retained source per FROM item, read one
//! joined row at a time.
//!
//! The levels follow the general executor's execution order (FROM order, or
//! its cost order for a cross join). An inner level rewinds its source for
//! each row of the level above it, as PostgreSQL's nested loop rescans its
//! inner plan. Every level's position, current row and match flag live in the
//! reader, so a storage wait at any level returns to the caller and the same
//! row is read again on resume.

use super::*;
use crate::mem::arena::OwnedArenaSlice;
use crate::mem::lease_pool::{LeaseError, MutableSlice};
use crate::sql::ast::{Collation, JoinKind};

/// Most FROM items a retained join reads; a larger join runs in the general
/// executor and its large output queues in the result spool.
pub(crate) const MAX_RETAINED_SOURCES: usize = 8;

/// What one FROM item of a retained join reads.
#[derive(Clone, Copy)]
pub(super) enum ItemShape<'a> {
    Table,
    Literals(usize),
    Generated(&'a [crate::sql::query::generator::GeneratorKind]),
}

/// One FROM item of a retained join at its execution depth.
pub(super) struct JoinLevel<'a> {
    /// The scope index this level reads.
    table: usize,
    source: RetainedSource<'a>,
    /// The level's current row; [`RetainedRow::Null`] is a LEFT JOIN's
    /// NULL-extended row.
    pending: Option<PendingRow<'a>>,
    /// A LEFT JOIN keeps an unmatched row of the levels above it.
    left: bool,
    /// The bound ON condition, including USING/NATURAL equalities.
    on: Option<&'a Expr<'a>>,
    /// Error-safe WHERE conjuncts whose tables are all bound at this level.
    pushdown: &'a [&'a Expr<'a>],
    /// The current row of the levels above found a partner here.
    matched: bool,
    /// The pending row passed ON and the pushed-down conjuncts.
    checked: bool,
    /// The source has not been read since the levels above moved.
    restart: bool,
    decoded_columns: Option<&'a [bool; MAX_COLUMNS]>,
    /// A hash join's build side: this level's source is read once into the
    /// table, which each row above probes.
    hash: Option<HashTable>,
}

/// The equi-keys of a hash join the general executor would choose.
#[derive(Clone, Copy)]
pub(super) struct HashShape {
    /// (probe column, build column) pairs.
    keys: [(usize, usize); 8],
    collations: [Collation; 8],
    count: usize,
    /// The planned build row count; it sizes the bucket array.
    capacity: usize,
}

#[derive(Clone, Copy)]
struct HashEntry {
    hash: u64,
    next: u32,
    start: u32,
    len: u32,
}

/// A hash join's build rows, in `plan_build_bytes` leases owned by the level.
struct HashTable {
    shape: HashShape,
    buckets: MutableSlice<u32>,
    entries: MutableSlice<HashEntry>,
    rows: MutableSlice<u8>,
    started: bool,
    built: bool,
    /// The next entry of the current probe's bucket chain.
    next: u32,
    probe: u64,
}

const NO_ENTRY: u32 = u32::MAX;

fn build_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "retained hash join build exceeds plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

impl HashTable {
    fn new(storage: &Storage, shape: HashShape) -> Result<Self, SqlError> {
        let pool = storage.plan_build();
        let buckets_len = shape
            .capacity
            .max(1)
            .checked_mul(2)
            .and_then(usize::checked_next_power_of_two)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained hash join build estimate overflows"
                )
                .plan_build_exhausted()
            })?
            .max(16);
        let mut buckets = pool.lease::<u32>(buckets_len).map_err(build_full)?;
        for _ in 0..buckets_len {
            buckets.push_reserved(NO_ENTRY).map_err(build_full)?;
        }
        Ok(Self {
            shape,
            buckets,
            entries: pool
                .lease::<HashEntry>(shape.capacity.max(1))
                .map_err(build_full)?,
            rows: pool.lease::<u8>(0).map_err(build_full)?,
            started: false,
            built: false,
            next: NO_ENTRY,
            probe: 0,
        })
    }

    fn hash(
        &self,
        storage: &Storage,
        txid: u32,
        values: &[Datum<'_>],
        build: bool,
    ) -> Result<Option<u64>, SqlError> {
        let mut keys = [Datum::Null; 8];
        for (key, &(probe, built)) in keys.iter_mut().zip(&self.shape.keys[..self.shape.count]) {
            *key = values[if build { built } else { probe }];
            if key.is_null() {
                return Ok(None);
            }
        }
        const COLUMNS: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
        crate::sql::eval::hash_key_collated_catalog(
            storage,
            txid,
            &keys[..self.shape.count],
            &COLUMNS[..self.shape.count],
            &self.shape.collations[..self.shape.count],
        )
        .map(Some)
    }

    /// Adds one build row; a row with a NULL key can never match.
    fn insert(
        &mut self,
        storage: &Storage,
        txid: u32,
        values: &[Datum<'_>],
    ) -> Result<(), SqlError> {
        let Some(hash) = self.hash(storage, txid, values, true)? else {
            return Ok(());
        };
        let length = crate::sql::exec::projected_row_len(values)?;
        let start = self.rows.len();
        let offset = |value: usize| {
            u32::try_from(value).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained hash join build exceeds 4 GiB of rows"
                )
                .plan_build_exhausted()
            })
        };
        let (start32, len32) = (offset(start)?, offset(length)?);
        offset(start + length)?;
        let index = offset(self.entries.len())?;
        let required = start + length;
        if required > self.rows.capacity_entries() {
            // Grow geometrically: each move copies the rows built so far.
            let grown = self
                .rows
                .capacity_entries()
                .saturating_mul(2)
                .min(self.rows.max_entries())
                .max(required);
            self.rows.reserve_exact(grown).map_err(build_full)?;
        }
        for _ in 0..length {
            self.rows.push_reserved(0).map_err(build_full)?;
        }
        crate::sql::exec::encode_projected_into(
            values,
            &mut self.rows.as_mut_slice()[start..start + length],
        )?;
        let bucket = (hash as usize) & (self.buckets.len() - 1);
        let next = self.buckets.as_slice()[bucket];
        self.entries
            .push(HashEntry {
                hash,
                next,
                start: start32,
                len: len32,
            })
            .map_err(build_full)?;
        self.buckets.as_mut_slice()[bucket] = index;
        Ok(())
    }

    /// The next entry in the current chain whose hash matches the probe.
    fn next_candidate(&mut self) -> Option<&[u8]> {
        while self.next != NO_ENTRY {
            let entry = self.entries.as_slice()[self.next as usize];
            self.next = entry.next;
            if entry.hash == self.probe {
                let start = entry.start as usize;
                return Some(&self.rows.as_slice()[start..start + entry.len as usize]);
            }
        }
        None
    }
}

/// The levels of a retained join and the depth being read.
pub(super) struct JoinState<'a> {
    /// Execution order: the scope index read at each depth.
    order: &'a [usize],
    levels: OwnedArenaSlice<'a, JoinLevel<'a>>,
    depth: usize,
}

fn source_ref<'a>(from: &'a FromClause<'a>, table: usize) -> &'a TableRef<'a> {
    if table == 0 {
        &from.base
    } else {
        &from.joins[table - 1].table
    }
}

/// Classifies one FROM item of a join before its scope is resolved: a table,
/// a literal VALUES list or builtin generator calls with constant arguments.
pub(super) fn item_shape<'a>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Option<ItemShape<'a>> {
    if tref.lateral || tref.sample.is_some() || tref.cte.is_some() {
        return None;
    }
    if let Some(query) = tref.subquery {
        return literal_source(query).map(ItemShape::Literals);
    }
    if tref.is_function_source() {
        return retained_function_source(tref, storage, txid, arena).map(ItemShape::Generated);
    }
    matches!(
        storage.resolve_relation(tref.schema, tref.table, txid),
        Some(crate::storage::ResolvedRelation::Table(_))
    )
    .then_some(ItemShape::Table)
}

/// Whether a physical table can be read by a retained level: a local,
/// unpartitioned table without descendants or row security for this role.
pub(super) fn retained_table(
    storage: &Storage,
    scope: &QueryScope<'_>,
    tref: &TableRef<'_>,
    table: usize,
    txid: u32,
) -> bool {
    if scope.derived[table].is_some() || scope.slots[table] == usize::MAX {
        return false;
    }
    let slot = scope.slots[table];
    let Some(definition) = scope.defs.stored(table) else {
        return false;
    };
    if definition.kind != crate::storage::TableKind::Local
        || definition.partition.is_partitioned()
        || (tref.inheritance == crate::sql::ast::RelationInheritance::Descendants
            && storage.relation_has_descendants(slot, txid))
    {
        return false;
    }
    let Some(role) = storage.current_role_slot(txid) else {
        return false;
    };
    let role = scope.authorization_roles[table].map_or(role, usize::from);
    !storage.row_security_applies(slot, role, txid)
}

/// The execution order and per-depth pushed-down WHERE conjuncts of a join
/// the retained reader holds, or `None` when the general executor would read
/// it another way (a hash join or index lookups per outer row) or a join
/// condition cannot be evaluated inside the reader.
pub(super) struct JoinShape<'a> {
    pub(super) order: &'a [usize],
    pub(super) items: [Option<ItemShape<'a>>; MAX_RETAINED_SOURCES],
    pub(super) pushdown: &'a [&'a [&'a Expr<'a>]],
    /// The second level's hash build, when the general executor hashes.
    pub(super) hash: Option<HashShape>,
}

/// Decides whether a joined FROM clause can be read by retained levels. The
/// result decides both a stream's eligibility and the reader that opens it,
/// so the two cannot disagree.
pub(super) fn join_shape<'a>(
    storage: &'a Storage,
    txid: u32,
    from: &'a FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<Option<JoinShape<'a>>, SqlError> {
    if from.joins.is_empty() || scope.n > MAX_RETAINED_SOURCES || scope.n != from.joins.len() + 1 {
        return Ok(None);
    }
    let mut items = [None; MAX_RETAINED_SOURCES];
    for (table, item) in items.iter_mut().enumerate().take(scope.n) {
        let tref = source_ref(from, table);
        let Some(shape) = item_shape(tref, storage, txid, arena) else {
            return Ok(None);
        };
        if matches!(shape, ItemShape::Table) && !retained_table(storage, scope, tref, table, txid) {
            return Ok(None);
        }
        *item = Some(shape);
    }
    if !from.joins.iter().all(|join| {
        matches!(
            join.kind,
            JoinKind::Inner | JoinKind::Cross | JoinKind::Comma | JoinKind::Left
        )
    }) {
        return Ok(None);
    }
    let catalog = storage_catalog(storage, arena, txid);
    let columns = CatalogScopeCols {
        scope,
        outer_scope: None,
        storage,
        txid,
        arena,
    };
    for (index, join) in from.joins.iter().enumerate() {
        let Some(on) = join.on.or(scope.join_on[index]) else {
            continue;
        };
        // A USING or NATURAL condition is a chain of one equality per merged
        // column: the reader checks and evaluates its equalities one by one,
        // as the general executor's flattened join does, up to a WHERE
        // clause's conjunct count; a wider one stays with that executor.
        let mut conjuncts = [on; crate::sql::query::plan::MAX_CONJUNCTS];
        let mut count = 0;
        let conjuncts: &[&Expr] = if join.on.is_some() {
            core::slice::from_ref(&on)
        } else if crate::sql::query::plan::flatten_and(on, &mut conjuncts, &mut count) {
            &conjuncts[..count]
        } else {
            return Ok(None);
        };
        for &on in conjuncts {
            if !retained_expression(on, &catalog, Some((storage, txid)))
                || !retained_scalar_calls(on, &columns)?
                || crate::sql::eval::routine_input_handoff(on, Some(&catalog))
                || crate::sql::query::predicate_safety::expression_properties(
                    on,
                    &columns,
                    storage,
                    txid,
                    crate::sql::query::predicate_safety::AnalysisStage::RuntimeProjection,
                )?
                .volatile
            {
                return Ok(None);
            }
        }
    }
    let all_cross = from.joins.iter().all(|join| join.kind.is_cross());
    let order = if all_cross {
        crate::sql::query::plan::join_order(storage, scope, where_clause, arena)?
    } else {
        arena
            .alloc_slice_with(scope.n, |index| index)
            .map_err(|_| arena_full())?
    };
    // The general executor's physical choice for the same statement: a hash
    // join is read as one, building the same side, and per-row index lookups
    // keep the join there, so a retained reader never turns an indexed or
    // hashed join into a full nested loop.
    let mut hash = None;
    if let Some(plan) = crate::sql::query::scan::select_hash_join_plan(
        storage,
        scope,
        from,
        where_clause,
        order,
        txid,
        arena,
    )? {
        let layout = plan.key_layout();
        if layout.probe_table != order[0]
            || layout.build_table != order[1]
            || !matches!(items[layout.build_table], Some(ItemShape::Table))
        {
            return Ok(None);
        }
        let count = layout.keys.len();
        let mut shape = HashShape {
            keys: [(0, 0); 8],
            collations: [Collation::None; 8],
            count,
            capacity: layout.capacity,
        };
        shape.keys[..count].copy_from_slice(layout.keys);
        shape.collations[..count].copy_from_slice(layout.collations);
        hash = Some(shape);
    }
    for depth in 1..scope.n {
        if crate::sql::query::scan::parameterized_index_access_plan(
            storage,
            scope,
            from,
            where_clause,
            order,
            depth,
            txid,
            false,
            arena,
        )?
        .is_some()
        {
            return Ok(None);
        }
    }
    // As the general nested loop does for inner and cross joins: each
    // error-safe WHERE conjunct prunes at the depth where its tables are
    // bound. The complete WHERE still runs on every joined row.
    let pushdown_buffers = arena
        .alloc_slice_with(scope.n, |_| {
            [&Expr::Null; crate::sql::query::plan::MAX_CONJUNCTS]
        })
        .map_err(|_| arena_full())?;
    let mut counts = [0usize; MAX_RETAINED_SOURCES];
    let all_inner = from.joins.iter().all(|join| {
        matches!(
            join.kind,
            JoinKind::Inner | JoinKind::Cross | JoinKind::Comma
        )
    });
    if all_inner && let Some(predicate) = where_clause {
        let mut conjuncts = [predicate; crate::sql::query::plan::MAX_CONJUNCTS];
        let mut count = 0;
        let conjuncts: &[&Expr] =
            if crate::sql::query::plan::flatten_and(predicate, &mut conjuncts, &mut count) {
                &conjuncts[..count]
            } else {
                core::slice::from_ref(&predicate)
            };
        for &conjunct in conjuncts {
            if !crate::sql::query::plan::is_error_safe(conjunct) {
                continue;
            }
            let Some(mask) = crate::sql::query::plan::expr_tables(conjunct, scope) else {
                continue;
            };
            let depth = order
                .iter()
                .rposition(|table| mask & (1 << table) != 0)
                .unwrap_or(0);
            pushdown_buffers[depth][counts[depth]] = conjunct;
            counts[depth] += 1;
        }
    }
    let pushdown = arena
        .alloc_slice_with(scope.n, |depth| &pushdown_buffers[depth][..counts[depth]])
        .map_err(|_| arena_full())?;
    Ok(Some(JoinShape {
        order,
        items,
        pushdown,
        hash,
    }))
}

/// Builds the retained levels of an admitted join. Literal sources are
/// already in `scope.derived`; physical sources capture their positions.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_levels<'a>(
    storage: &'a Storage,
    txid: u32,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    shape: &JoinShape<'a>,
    demand: PaxReadDemand,
    columns: &CatalogScopeCols<'_, 'a, '_, '_>,
    eliminated: bool,
    arena: &'a Arena,
) -> Result<JoinState<'a>, SqlError> {
    let mut on = [None; MAX_RETAINED_SOURCES];
    let mut pushdown: [&[&Expr]; MAX_RETAINED_SOURCES] = [&[]; MAX_RETAINED_SOURCES];
    let mut masks: [Option<&'a [bool; MAX_COLUMNS]>; MAX_RETAINED_SOURCES] =
        [None; MAX_RETAINED_SOURCES];
    for (depth, &table) in shape.order.iter().enumerate() {
        // A USING or NATURAL condition runs as its separate equalities,
        // checked like the pushed-down conjuncts after them.
        let mut synthesized = [&Expr::Null; crate::sql::query::plan::MAX_CONJUNCTS];
        let mut synthesized_count = 0;
        if table > 0 {
            let join = &from.joins[table - 1];
            match (join.on, scope.join_on[table - 1]) {
                (Some(condition), _) => {
                    on[depth] = Some(bind_expression(condition, arena, Some(columns), None)?);
                }
                (None, Some(condition)) => {
                    if !crate::sql::query::plan::flatten_and(
                        condition,
                        &mut synthesized,
                        &mut synthesized_count,
                    ) {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "retained join condition changed after admission"
                        ));
                    }
                }
                (None, None) => {}
            }
        }
        let bound = arena
            .alloc_slice_with(synthesized_count + shape.pushdown[depth].len(), |_| {
                &Expr::Null
            })
            .map_err(|_| arena_full())?;
        for (target, conjunct) in bound.iter_mut().zip(
            synthesized[..synthesized_count]
                .iter()
                .chain(shape.pushdown[depth]),
        ) {
            *target = bind_expression(conjunct, arena, Some(columns), None)?;
        }
        pushdown[depth] = bound;
        if let Some(mask) = demand.selected_mask(table) {
            let selected: [bool; MAX_COLUMNS] = core::array::from_fn(|column| {
                column < u64::BITS as usize && mask & (1 << column) != 0
            });
            masks[depth] = Some(arena.alloc(selected).map_err(|_| arena_full())?);
        }
    }
    let mut hash = match shape.hash {
        Some(hash) if !eliminated => Some(HashTable::new(storage, hash)?),
        _ => None,
    };
    let mut failure = None;
    let levels = arena
        .alloc_owned_slice_with(scope.n, |depth| {
            let table = shape.order[depth];
            let source = match shape.items[table].expect("admitted join item") {
                ItemShape::Table => {
                    let mut cursor = RowCursor::closed();
                    if !eliminated
                        && failure.is_none()
                        && let Err(error) = cursor.open(storage, scope.slots[table], txid, arena)
                    {
                        failure = Some(error);
                    }
                    RetainedSource::Physical(cursor)
                }
                ItemShape::Literals(_) => RetainedSource::Literals {
                    rows: scope.derived[table].expect("literal join item owns encoded rows"),
                    next: 0,
                    scrollable: true,
                },
                ItemShape::Generated(kinds) => RetainedSource::Generated {
                    function: source_ref(from, table),
                    kinds,
                    generator: None,
                },
            };
            JoinLevel {
                table,
                source,
                pending: None,
                left: table > 0 && from.joins[table - 1].kind == JoinKind::Left,
                on: on[depth],
                pushdown: pushdown[depth],
                matched: false,
                checked: false,
                restart: true,
                decoded_columns: masks[depth],
                hash: if depth == 1 { hash.take() } else { None },
            }
        })
        .map_err(|_| arena_full())?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(JoinState {
        order: shape.order,
        levels,
        depth: 0,
    })
}

impl<'a> JoinState<'a> {
    pub(super) fn has_pending(&self) -> bool {
        self.levels.iter().any(|level| level.pending.is_some())
    }

    /// Whether every level holds a checked row: a joined row is ready.
    fn complete_row(&self) -> bool {
        self.depth + 1 == self.levels.len()
            && self
                .levels
                .last()
                .is_some_and(|level| level.pending.is_some() && level.checked)
    }

    pub(super) fn tables(&self) -> impl Iterator<Item = (usize, &RetainedSource<'a>)> {
        self.levels.iter().map(|level| (level.table, &level.source))
    }

    pub(super) fn close(&mut self) {
        for level in self.levels.iter_mut() {
            level.source.close();
        }
    }

    /// Returns every level to its first row under the same snapshot. No row
    /// may be pending.
    pub(super) fn rewind(&mut self, state_arena: &Arena) -> Result<(), SqlError> {
        for level in self.levels.iter_mut() {
            if level.hash.is_none() {
                level.source.rewind(state_arena)?;
            }
            level.matched = false;
            level.checked = false;
            level.restart = true;
        }
        self.depth = 0;
        Ok(())
    }

    /// Starts generator levels: their constant arguments are evaluated once,
    /// when the reader first runs.
    pub(super) fn start_generators(
        &mut self,
        storage: &Storage,
        txid: u32,
        state_arena: &'a Arena,
        params: &[Datum<'a>],
    ) -> Result<(), SqlError> {
        for level in self.levels.iter_mut() {
            start_generator_source(&mut level.source, storage, txid, state_arena, params)?;
        }
        Ok(())
    }

    /// Drops the deepest row after the caller finished with it; the next
    /// call reads that level's next row.
    pub(super) fn finish_row(&mut self, state_arena: &Arena) {
        let level = &mut self.levels[self.depth];
        if let Some(pending) = level.pending.take() {
            // SAFETY: deeper levels hold no row; the caller dropped every
            // evaluation value of the finished row.
            unsafe { state_arena.rewind_to(pending.mark) };
        }
        level.checked = false;
    }

    /// Reads until every level holds a row that passed its join condition.
    /// `Ok(false)` is the end of the join. A wait leaves each level's
    /// position and row in place; the same call resumes it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn prepare_row(
        &mut self,
        scope: &QueryScope<'a>,
        storage: &Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &Arena,
        params: &[Datum<'_>],
    ) -> Result<bool, SqlError> {
        loop {
            check_timeout()?;
            if self.complete_row() {
                return Ok(true);
            }
            let depth = self.depth;
            if self.levels[depth].restart {
                if self.levels[depth].hash.is_some() {
                    self.build_hash(
                        depth,
                        scope,
                        storage,
                        txid,
                        observed_at,
                        state_arena,
                        buffers,
                        work,
                    )?;
                    self.start_probe(scope, storage, txid, work)?;
                } else {
                    let level = &mut self.levels[depth];
                    if depth > 0 {
                        level.source.rewind(state_arena)?;
                    }
                    if matches!(level.source, RetainedSource::Physical(_)) {
                        storage.record_relation_scan(
                            txid,
                            scope.slots[level.table],
                            None,
                            0,
                            observed_at,
                        )?;
                    }
                }
                let level = &mut self.levels[depth];
                level.restart = false;
                level.matched = false;
            }
            let level = &mut self.levels[depth];
            if level.pending.is_none() {
                let mark = state_arena.mark();
                let next = match &mut level.hash {
                    Some(table) => table
                        .next_candidate()
                        .map(|bytes| {
                            state_arena
                                .alloc_slice_copy(bytes)
                                .map(|values| RetainedRow::Saved {
                                    rowid: None,
                                    version: None,
                                    values,
                                })
                                .map_err(|_| arena_full())
                        })
                        .transpose(),
                    None => next_source_row(
                        &mut level.source,
                        state_arena,
                        buffers,
                        level.decoded_columns,
                    ),
                };
                let row = match next {
                    Ok(row) => row,
                    Err(error) => {
                        // Failed reads retain navigation state, not front scratch.
                        unsafe { state_arena.rewind_to(mark) };
                        return Err(error);
                    }
                };
                match row {
                    Some(row) => {
                        if matches!(row, RetainedRow::Physical(_)) {
                            storage.record_relation_tuple_read(
                                txid,
                                scope.slots[level.table],
                                None,
                            )?;
                        }
                        level.pending = Some(PendingRow {
                            row,
                            mark,
                            counted: true,
                            saved: None,
                        });
                        level.checked = false;
                    }
                    None if level.left && !level.matched => {
                        // No partner: the levels above join a NULL row here.
                        level.matched = true;
                        level.pending = Some(PendingRow {
                            row: RetainedRow::Null,
                            mark,
                            counted: true,
                            saved: None,
                        });
                        level.checked = true;
                    }
                    None => {
                        unsafe { state_arena.rewind_to(mark) };
                        if depth == 0 {
                            return Ok(false);
                        }
                        // This level is exhausted for the row above it.
                        level.restart = true;
                        self.depth -= 1;
                        self.finish_row(state_arena);
                        continue;
                    }
                }
            }
            if !self.levels[depth].checked {
                let passes = self.row_passes(scope, storage, txid, work, params)?;
                let level = &mut self.levels[depth];
                if !passes {
                    let pending = level.pending.take().expect("checked join row");
                    unsafe { state_arena.rewind_to(pending.mark) };
                    continue;
                }
                level.checked = true;
                level.matched = true;
            }
            if depth + 1 < self.levels.len() {
                self.depth += 1;
                self.levels[self.depth].restart = true;
            }
        }
    }

    /// Builds every hash table before the first row, so a declared cursor
    /// learns at `DECLARE` whether its build fits `plan_build_bytes`. A
    /// build reads only its table: it runs no expression and has no effect.
    /// A wait keeps the rows inserted so far.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_hashes(
        &mut self,
        scope: &QueryScope<'a>,
        storage: &Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &Arena,
    ) -> Result<(), SqlError> {
        for depth in 0..self.levels.len() {
            if self.levels[depth].hash.is_some() {
                self.build_hash(
                    depth,
                    scope,
                    storage,
                    txid,
                    observed_at,
                    state_arena,
                    buffers,
                    work,
                )?;
            }
        }
        Ok(())
    }

    /// Reads level `depth`'s whole source into its hash table once. A wait
    /// keeps the rows inserted so far and the source position.
    #[allow(clippy::too_many_arguments)]
    fn build_hash(
        &mut self,
        depth: usize,
        scope: &QueryScope<'a>,
        storage: &Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &Arena,
    ) -> Result<(), SqlError> {
        let level = &mut self.levels[depth];
        let table = level.hash.as_mut().expect("hash level");
        if table.built {
            return Ok(());
        }
        if !table.started {
            storage.record_relation_scan(txid, scope.slots[level.table], None, 0, observed_at)?;
            table.started = true;
        }
        loop {
            check_timeout()?;
            let mark = state_arena.mark();
            let row = next_source_row(
                &mut level.source,
                state_arena,
                buffers,
                level.decoded_columns,
            );
            let row = match row {
                Ok(Some(row)) => row,
                Ok(None) => {
                    unsafe { state_arena.rewind_to(mark) };
                    table.built = true;
                    level.source.close();
                    return Ok(());
                }
                Err(error) => {
                    unsafe { state_arena.rewind_to(mark) };
                    return Err(error);
                }
            };
            let inserted = (|| {
                storage.record_relation_tuple_read(txid, scope.slots[level.table], None)?;
                let pending = PendingRow {
                    row,
                    mark,
                    counted: true,
                    saved: None,
                };
                let work_mark = work.mark();
                let values = work
                    .alloc_slice_with(scope.row_width(level.table), |_| Datum::Null)
                    .map_err(|_| arena_full());
                let result = values.and_then(|values| {
                    pending.decode_table(scope, level.table, values)?;
                    let width = scope
                        .defs
                        .get(level.table)
                        .expect("build definition")
                        .n_columns;
                    table.insert(storage, txid, &values[..width])
                });
                // SAFETY: the decoded values were consumed by the insert.
                unsafe { work.rewind_to(work_mark) };
                result
            })();
            // SAFETY: the row was copied into the hash table or rejected.
            unsafe { state_arena.rewind_to(mark) };
            inserted?;
        }
    }

    /// Hashes the rows above the current hash level and starts its bucket
    /// chain; a NULL key matches nothing.
    fn start_probe(
        &mut self,
        scope: &QueryScope<'a>,
        storage: &Storage,
        txid: u32,
        work: &Arena,
    ) -> Result<(), SqlError> {
        let depth = self.depth;
        let probe_table = self.levels[0].table;
        let mark = work.mark();
        let table = self.levels[depth].hash.as_ref().expect("hash level");
        let probe = self.with_row(scope, work, depth, |row| {
            table.hash(
                storage,
                txid,
                row.table_values(probe_table).expect("probe row is bound"),
                false,
            )
        });
        // SAFETY: the probe's decoded values were dropped with the row.
        unsafe { work.rewind_to(mark) };
        let probe = probe?;
        let table = self.levels[depth].hash.as_mut().expect("hash level");
        match probe {
            Some(hash) => {
                table.probe = hash;
                table.next = table.buckets.as_slice()[(hash as usize) & (table.buckets.len() - 1)];
            }
            None => table.next = NO_ENTRY,
        }
        Ok(())
    }

    /// Evaluates the ON condition and pushed-down conjuncts of the deepest
    /// row read so far against the rows above it.
    fn row_passes(
        &self,
        scope: &QueryScope<'a>,
        storage: &Storage,
        txid: u32,
        work: &Arena,
        params: &[Datum<'_>],
    ) -> Result<bool, SqlError> {
        let level = &self.levels[self.depth];
        if level.on.is_none() && level.pushdown.is_empty() && level.hash.is_none() {
            return Ok(true);
        }
        let mark = work.mark();
        let result = self.with_row(scope, work, self.depth + 1, |row| {
            if let Some(table) = &level.hash {
                // A bucket holds colliding hashes too: compare the keys.
                let probe = row
                    .table_values(self.levels[0].table)
                    .expect("probe row is bound");
                let build = row.table_values(level.table).expect("build row is bound");
                for (index, &(probe_column, build_column)) in
                    table.shape.keys[..table.shape.count].iter().enumerate()
                {
                    if !crate::sql::eval::compare_datums_collated(
                        storage,
                        txid,
                        table.shape.collations[index],
                        &build[build_column],
                        &probe[probe_column],
                    )?
                    .is_eq()
                    {
                        return Ok(false);
                    }
                }
            }
            let catalog = storage_catalog(storage, work, txid);
            let hooks = EvalHooks {
                catalog: Some(&catalog),
                ..crate::sql::eval::NO_HOOKS
            };
            if let Some(on) = level.on {
                match eval_full(on, work, params, row, &hooks)? {
                    Datum::Bool(true) => {}
                    Datum::Bool(false) | Datum::Null => return Ok(false),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "argument of JOIN/ON must be type boolean"
                        ));
                    }
                }
            }
            for conjunct in level.pushdown {
                if !crate::sql::query::plan::conjunct_passes(conjunct, work, params, row, &hooks)? {
                    return Ok(false);
                }
            }
            Ok(true)
        });
        // SAFETY: evaluation values were dropped with the row above.
        unsafe { work.rewind_to(mark) };
        result
    }

    /// Decodes the rows of the first `count` levels into `work` and passes
    /// them as one joined row.
    pub(super) fn with_row<'work, R>(
        &'work self,
        scope: &'work QueryScope<'a>,
        work: &'work Arena,
        count: usize,
        each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let bound = work
            .alloc_slice_with(scope.n, |_| None::<scan::BoundRow<'work>>)
            .map_err(|_| arena_full())?;
        let rowids = work
            .alloc_slice_with(scope.n, |_| None::<u64>)
            .map_err(|_| arena_full())?;
        for level in &self.levels[..count] {
            let pending = level.pending.as_ref().ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "retained join level has no current row"
                )
            })?;
            let (values, rowid) = match &pending.row {
                RetainedRow::Null => continue,
                RetainedRow::Physical(row) => (
                    work.alloc_slice_with(scope.row_width(level.table), |_| Datum::Null)
                        .map_err(|_| arena_full())?,
                    Some(row.rowid),
                ),
                _ => (
                    work.alloc_slice_with(scope.row_width(level.table), |_| Datum::Null)
                        .map_err(|_| arena_full())?,
                    None,
                ),
            };
            pending.decode_table(scope, level.table, values)?;
            bound[level.table] = Some(scan::BoundRow::Values(values));
            rowids[level.table] = rowid;
        }
        each(&scan::JoinRow {
            scope,
            values: scan::JoinRowValues::Bound {
                bound,
                order: self.order,
                count,
                buffers: &[],
                buffer_offsets: &[],
            },
            rowids,
        })
    }
}
