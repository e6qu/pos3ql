//! A retained grouping: `GROUP BY`, `HAVING` and aggregates computed by the
//! reader itself in `plan_build_bytes`.
//!
//! At the first row the reader reads its whole input (one table, generator
//! calls, a literal list or a retained join) and filters it. How it keeps
//! that input is decided before any row, from the aggregates alone:
//!
//! - When every aggregate's transition state has a fixed size or holds one
//!   value (`count`, `sum`/`avg` over integers, floats and intervals,
//!   `min`/`max`, `bool_and`/`bool_or`, bitwise and floating-point
//!   statistical aggregates, without `DISTINCT` or an aggregate `ORDER BY`),
//!   it keeps one record per group: the group's key, its states and the
//!   values `min`/`max` hold, found through a balanced tree ordered by key.
//!   Each input row folds into its group's states as it is read, so memory
//!   grows with groups, not rows, and a group's results are computed as it
//!   is returned.
//! - Otherwise it saves each passing row's grouping keys and the input
//!   columns its aggregates read, orders them by key (equal keys keep input
//!   order), and folds each group over its rows in input order.
//!
//! Either way a storage wait keeps what was saved and the input position, so
//! input is never read twice, and a group folds its rows in input order with
//! the general executor's aggregate states. Results, the order of groups
//! (keys ascending, NULLs last) and the key a group reports (its first input
//! row's, as PostgreSQL's hashed grouping keeps) match the general executor.
//! Folding calls no routine body. `HAVING` and the output list run per group
//! as its row is returned, so a routine call there hands off on its row.

use super::*;
use crate::mem::lease_pool::{LeaseError, MutableSlice};
use crate::sql::ast::{Collation, GroupingSet};
use crate::sql::query::aggregate::{AggState, FixedState};

/// Aggregate calls by address in the output list, `HAVING` and `ORDER BY`.
pub(super) type AggregateNodes<'a> = &'a [(*const Expr<'a>, &'a Expr<'a>)];

/// How a retained reader treats a statement's grouping.
pub(super) enum Grouping<'a> {
    /// No `GROUP BY`, `HAVING` or aggregate call.
    Plain,
    /// The reader groups and aggregates itself: the statement with its
    /// `GROUP BY` positions resolved, and its aggregate calls.
    Grouped {
        statement: &'a Select<'a>,
        aggregates: AggregateNodes<'a>,
    },
}

fn group_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "retained grouping exceeds plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

/// Whether `expression` may run while the input is read: as a sort key,
/// it is non-volatile, hands no call off to a routine and casts only
/// through builtin paths.
fn before_input_end(
    expression: &Expr<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
) -> Result<bool, SqlError> {
    let storage = columns.storage;
    let mut safe_casts = true;
    validate_statement_casts(expression, columns, &mut safe_casts)?;
    Ok(safe_casts
        && retained_expression(expression, catalog, Some((storage, columns.txid)))
        && retained_scalar_calls(expression, columns)?
        && !crate::sql::eval::routine_input_handoff(expression, Some(catalog))
        && !super::predicate_safety::expression_properties(
            expression,
            columns,
            storage,
            columns.txid,
            super::predicate_safety::AnalysisStage::RuntimeProjection,
        )?
        .volatile)
}

/// Whether an aggregate's state folds without a stored routine body: a
/// builtin aggregate, or a custom one whose support functions are builtins.
/// Object-store mode combines partial custom states the general executor's
/// way, which only it reproduces.
fn aggregate_folds_here<'a>(
    node: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> bool {
    let mark = arena.mark();
    let mut state = AggState::default();
    let admitted = state
        .init(node, storage, txid, &super::ScopeCols(scope), arena)
        .is_ok()
        && state
            .custom_support_oids()
            .all(|oid| storage.routine_slot_by_oid(oid, txid).is_none())
        && !(storage.spill_attached() && state.supports_partial());
    // SAFETY: the probe state and its allocations end here.
    unsafe { arena.rewind_to(mark) };
    admitted
}

/// Decides, before the statement runs, whether a retained reader computes
/// its grouping itself. `None` leaves the statement to the general executor,
/// which also reports any error in it.
pub(super) fn admit_grouping<'a>(
    statement: &'a Select<'a>,
    columns: &CatalogScopeCols<'_, 'a, '_, '_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
    arena: &'a Arena,
) -> Result<Option<Grouping<'a>>, SqlError> {
    let (scope, storage, txid) = (columns.scope, columns.storage, columns.txid);
    let mut aggregates = crate::mem::arena::ArenaList::new(arena);
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                Some(*expression)
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => None,
        })
        .chain(statement.having)
        .chain(statement.order_by.iter().map(|order| order.expression))
    {
        if super::collect_aggs(expression, &mut aggregates, storage, txid, columns).is_err() {
            return Ok(None);
        }
    }
    let plain =
        aggregates.is_empty() && statement.group_by.is_empty() && statement.having.is_none();
    if plain && !statement.distinct {
        return Ok(Some(Grouping::Plain));
    }
    // Plain DISTINCT groups by its output list, as the general executor
    // deduplicates the output sorted by it.
    if !statement.grouping_sets.is_empty()
        || statement.distinct && !plain
        || !statement.distinct_on.is_empty()
        || statement.with_ties
        || !statement.locking.is_empty()
        || statement.group_by.len() > crate::sql::parser::MAX_GROUP_TERMS
        || statement
            .items
            .iter()
            .any(|item| !matches!(item, SelectItem::Expr { .. }))
    {
        return Ok(None);
    }
    let Ok(statement) = super::resolve_group_ordinals(statement, Some(scope), arena, storage, txid)
    else {
        return Ok(None);
    };
    let statement = if statement.distinct {
        let keys = arena
            .alloc_slice_with(statement.items.len(), |index| {
                match statement.items[index] {
                    SelectItem::Expr { expression, .. } => expression,
                    _ => unreachable!("only expression items are admitted"),
                }
            })
            .map_err(|_| arena_full())?;
        &*arena
            .alloc(Select {
                group_by: keys,
                distinct: false,
                ..*statement
            })
            .map_err(|_| arena_full())?
    } else {
        statement
    };
    if super::check_key_types(statement, scope, arena, storage, txid).is_err() {
        return Ok(None);
    }
    let aggregates = aggregates.as_slice();
    let mut pointers = [core::ptr::null::<Expr<'a>>(); MAX_PROJ];
    if aggregates.len() > pointers.len() {
        return Ok(None);
    }
    for (pointer, (node, _)) in pointers.iter_mut().zip(aggregates) {
        *pointer = *node;
    }
    let pointers = &pointers[..aggregates.len()];
    // The grouped output: every column reference grouped, and only
    // expressions the reader evaluates per group.
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(statement.having)
    {
        let mut safe_casts = true;
        validate_statement_casts(expression, columns, &mut safe_casts)?;
        if !safe_casts
            || crate::sql::query::group::ungrouped_column(
                expression,
                statement.group_by,
                scope,
                aggregates,
            )
            .is_some()
            || !retained_expression_with(
                expression,
                catalog,
                Some((storage, txid)),
                Leaves::Aggregates(pointers),
            )
            || !retained_scalar_calls_except(expression, columns, pointers)?
        {
            return Ok(None);
        }
    }
    // WHERE is checked conjunct by conjunct, as the general executor
    // flattens it.
    let mut conjuncts = [&Expr::Null; crate::sql::query::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(predicate) = statement.where_clause
        && !crate::sql::query::plan::flatten_and(predicate, &mut conjuncts, &mut count)
    {
        return Ok(None);
    }
    for expression in statement
        .group_by
        .iter()
        .copied()
        .chain(conjuncts[..count].iter().copied())
    {
        if !before_input_end(expression, columns, catalog)? {
            return Ok(None);
        }
    }
    for (_, node) in aggregates {
        let Expr::Call {
            args,
            order_by,
            filter,
            over: None,
            ..
        } = node
        else {
            return Ok(None);
        };
        for expression in args
            .iter()
            .copied()
            .chain(order_by.iter().map(|order| order.expression))
            .chain(*filter)
        {
            if !before_input_end(expression, columns, catalog)? {
                return Ok(None);
            }
        }
        if !aggregate_folds_here(node, storage, txid, scope, arena) {
            return Ok(None);
        }
    }
    // ORDER BY over groups: only a prefix of the keys, ascending with NULLs
    // last, which is the order groups are formed in.
    for (index, order) in statement.order_by.iter().enumerate() {
        let Ok(target) = super::resolve_order_target(
            order.expression,
            statement.items,
            scope,
            arena,
            storage,
            txid,
        ) else {
            return Ok(None);
        };
        let key = statement.group_by.get(index);
        if order.descending
            || order.nulls_first
            || !key.is_some_and(|key| {
                **key == *target || crate::sql::query::group::same_scope_column(scope, key, target)
            })
        {
            return Ok(None);
        }
    }
    Ok(Some(Grouping::Grouped {
        statement,
        aggregates,
    }))
}

/// The source columns a grouped reader reads: its output list, filter,
/// `HAVING`, keys and `ORDER BY`.
pub(super) fn input_demand<'a>(
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<PaxReadDemand, SqlError> {
    let mut expressions = crate::mem::arena::ArenaList::new(arena);
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(statement.where_clause)
        .chain(statement.having)
        .chain(statement.group_by.iter().copied())
        .chain(statement.order_by.iter().map(|order| order.expression))
    {
        expressions.push(expression).map_err(|_| arena_full())?;
    }
    Ok(pax_column_demand(scope, from, expressions.as_slice()))
}

#[derive(Clone, Copy)]
enum Phase {
    /// Reading input: saved rows grow.
    Filling,
    /// Input is ordered; groups before entry `next` are folded.
    Folding { next: usize },
    /// Groups are folded; group `next` is returned next.
    Output { next: usize },
}

/// How a grouping keeps its input, chosen before any row is read.
#[derive(Clone, Copy)]
enum Method<'a> {
    /// Every aggregate's state has a fixed size, or holds one value: per
    /// group, its key in `inputs`, a tree node, and `stride` bytes of states.
    /// A [`FixedState::Reference`] state is followed by its value's slot in
    /// `values`.
    States {
        layouts: &'a [FixedState],
        stride: usize,
    },
    /// Some state grows: every input row's keys and aggregated columns.
    Inputs,
}

/// A node of the groups' AVL tree; its index is its group's.
#[derive(Clone, Copy)]
struct TreeNode {
    left: u32,
    right: u32,
    height: u32,
}

const NO_NODE: u32 = u32::MAX;

/// A held value's slot: offset, length (`u32::MAX` for none) and capacity.
const SLOT_BYTES: usize = 12;
const NO_VALUE: u32 = u32::MAX;

/// Bytes one aggregate's state takes in a group's states.
fn state_bytes(layout: FixedState) -> usize {
    layout.bytes()
        + if layout == FixedState::Reference {
            SLOT_BYTES
        } else {
            0
        }
}

fn slot_field(slot: &[u8], index: usize) -> u32 {
    u32::from_le_bytes(
        slot[index * 4..index * 4 + 4]
            .try_into()
            .expect("slot field"),
    )
}

/// The value a slot holds, copied into `work`.
fn load_value<'w>(
    values: &[u8],
    slot: &[u8],
    work: &'w Arena,
) -> Result<Option<Datum<'w>>, SqlError> {
    let (offset, length) = (slot_field(slot, 0) as usize, slot_field(slot, 1));
    if length == NO_VALUE {
        return Ok(None);
    }
    let bytes = work
        .alloc_slice_copy(&values[offset..offset + length as usize])
        .map_err(|_| arena_full())?;
    Ok(Some(crate::sql::exec::decode_projected_pub(bytes, 0)))
}

/// Saves `value` in its slot: in place when it fits, else appended.
fn store_value(
    values: &mut MutableSlice<u8>,
    slot: &mut [u8],
    value: Option<Datum<'_>>,
    work: &Arena,
) -> Result<(), SqlError> {
    let Some(value) = value else {
        slot[4..8].copy_from_slice(&NO_VALUE.to_le_bytes());
        return Ok(());
    };
    let encoded = encode_values(&[value], work)?;
    let too_large = || {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "retained grouping exceeds 4 GiB of values"
        )
        .plan_build_exhausted()
    };
    let length = u32::try_from(encoded.len())
        .ok()
        .filter(|&length| length != NO_VALUE)
        .ok_or_else(too_large)?;
    let (mut offset, capacity) = (slot_field(slot, 0), slot_field(slot, 2));
    if length > capacity {
        let start = values.len();
        offset = u32::try_from(start).map_err(|_| too_large())?;
        reserve(values, start + encoded.len())?;
        values.extend_from_slice(encoded).map_err(group_full)?;
        slot[8..12].copy_from_slice(&length.to_le_bytes());
    } else {
        values.as_mut_slice()[offset as usize..offset as usize + encoded.len()]
            .copy_from_slice(encoded);
    }
    slot[..4].copy_from_slice(&offset.to_le_bytes());
    slot[4..8].copy_from_slice(&length.to_le_bytes());
    Ok(())
}

/// An AVL tree of `u32::MAX` nodes is at most 46 levels deep.
const MAX_TREE_DEPTH: usize = 64;

pub(super) struct GroupState<'a> {
    method: Method<'a>,
    /// Each aggregate's state as initialized before any row.
    templates: &'a [AggState<'a>],
    keys: &'a [&'a Expr<'a>],
    collations: &'a [Collation],
    set: GroupingSet<'a>,
    aggregates: AggregateNodes<'a>,
    pointers: &'a [*const Expr<'a>],
    having: Option<&'a Expr<'a>>,
    /// The input columns the aggregates read; other columns save as NULL.
    demand: PaxReadDemand,
    /// Saved input: with fixed states, one key record per group; otherwise
    /// one record per input row: its keys, then per source a presence flag
    /// and its row's columns.
    inputs: MutableSlice<(u32, u32)>,
    input_bytes: MutableSlice<u8>,
    /// With fixed states: the groups' tree, its root and their states.
    tree: MutableSlice<TreeNode>,
    root: u32,
    states: MutableSlice<u8>,
    /// Values held by [`FixedState::Reference`] states.
    values: MutableSlice<u8>,
    /// With fixed states, once input ends: groups in key order, whose
    /// results are computed as each is returned.
    order: MutableSlice<u32>,
    /// Without fixed states: one record per group, its keys, then its
    /// aggregate results.
    groups: MutableSlice<(u32, u32)>,
    group_bytes: MutableSlice<u8>,
    phase: Phase,
    current: Option<usize>,
}

fn push_record(
    entries: &mut MutableSlice<(u32, u32)>,
    bytes: &mut MutableSlice<u8>,
    record: &[u8],
) -> Result<(), SqlError> {
    let start = bytes.len();
    let end = start + record.len();
    let (Ok(start32), Ok(length32), Ok(_)) = (
        u32::try_from(start),
        u32::try_from(record.len()),
        u32::try_from(end),
    ) else {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "retained grouping exceeds 4 GiB of rows"
        )
        .plan_build_exhausted());
    };
    if end > bytes.capacity_entries() {
        // Grow geometrically: each move copies the records saved so far.
        let grown = bytes
            .capacity_entries()
            .saturating_mul(2)
            .min(bytes.max_entries())
            .max(end);
        bytes.reserve_exact(grown).map_err(group_full)?;
    }
    bytes.extend_from_slice(record).map_err(group_full)?;
    if let Err(error) = entries.push((start32, length32)) {
        bytes.truncate(start);
        return Err(group_full(error));
    }
    Ok(())
}

/// Reserves room for `required` entries, growing geometrically: each move
/// copies the entries saved so far.
fn reserve<T>(slice: &mut MutableSlice<T>, required: usize) -> Result<(), SqlError> {
    if required > slice.capacity_entries() {
        let grown = slice
            .capacity_entries()
            .saturating_mul(2)
            .min(slice.max_entries())
            .max(required);
        slice.reserve_exact(grown).map_err(group_full)?;
    }
    Ok(())
}

fn height(tree: &[TreeNode], node: u32) -> u32 {
    if node == NO_NODE {
        0
    } else {
        tree[node as usize].height
    }
}

fn update_height(tree: &mut [TreeNode], node: u32) {
    let (left, right) = (tree[node as usize].left, tree[node as usize].right);
    tree[node as usize].height = 1 + height(tree, left).max(height(tree, right));
}

fn rotate_right(tree: &mut [TreeNode], node: u32) -> u32 {
    let left = tree[node as usize].left;
    tree[node as usize].left = tree[left as usize].right;
    tree[left as usize].right = node;
    update_height(tree, node);
    update_height(tree, left);
    left
}

fn rotate_left(tree: &mut [TreeNode], node: u32) -> u32 {
    let right = tree[node as usize].right;
    tree[node as usize].right = tree[right as usize].left;
    tree[right as usize].left = node;
    update_height(tree, node);
    update_height(tree, right);
    right
}

/// Restores the AVL balance at `node`; returns the subtree's new root.
fn rebalance(tree: &mut [TreeNode], node: u32) -> u32 {
    update_height(tree, node);
    let (left, right) = (tree[node as usize].left, tree[node as usize].right);
    let (left_height, right_height) = (height(tree, left), height(tree, right));
    if left_height > right_height + 1 {
        let inner = tree[left as usize];
        if height(tree, inner.left) < height(tree, inner.right) {
            tree[node as usize].left = rotate_left(tree, left);
        }
        return rotate_right(tree, node);
    }
    if right_height > left_height + 1 {
        let inner = tree[right as usize];
        if height(tree, inner.right) < height(tree, inner.left) {
            tree[node as usize].right = rotate_right(tree, right);
        }
        return rotate_left(tree, node);
    }
    node
}

fn record<'b>(entries: &MutableSlice<(u32, u32)>, bytes: &'b [u8], index: usize) -> &'b [u8] {
    let (start, length) = entries.as_slice()[index];
    &bytes[start as usize..(start + length) as usize]
}

fn compare_keys(
    storage: &Storage,
    txid: u32,
    collations: &[Collation],
    left: &[u8],
    right: &[u8],
) -> Result<core::cmp::Ordering, SqlError> {
    for (column, &collation) in collations.iter().enumerate() {
        let left = crate::sql::exec::decode_projected_pub(left, column);
        let right = crate::sql::exec::decode_projected_pub(right, column);
        let ordering = match (left.is_null(), right.is_null()) {
            (true, true) => core::cmp::Ordering::Equal,
            (true, false) => core::cmp::Ordering::Greater,
            (false, true) => core::cmp::Ordering::Less,
            (false, false) => {
                crate::sql::eval::compare_datums_collated(storage, txid, collation, &left, &right)?
            }
        };
        if !ordering.is_eq() {
            return Ok(ordering);
        }
    }
    Ok(core::cmp::Ordering::Equal)
}

impl<'a> GroupState<'a> {
    pub(super) fn new(
        storage: &Storage,
        txid: u32,
        statement: &'a Select<'a>,
        from: &'a FromClause<'a>,
        aggregates: AggregateNodes<'a>,
        scope: &QueryScope<'a>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let keys = statement.group_by;
        let collations = arena
            .alloc_slice_with(keys.len(), |_| Collation::None)
            .map_err(|_| arena_full())?;
        for (collation, key) in collations.iter_mut().zip(keys) {
            *collation = scope.expression_collation(storage, arena, key)?;
        }
        let words = keys.len().div_ceil(u64::BITS as usize);
        let set = GroupingSet {
            words: arena
                .alloc_slice_with(words, |index| {
                    let tail = keys.len() % u64::BITS as usize;
                    if index + 1 == words && tail != 0 {
                        (1u64 << tail) - 1
                    } else {
                        u64::MAX
                    }
                })
                .map_err(|_| arena_full())?,
        };
        let pointers = arena
            .alloc_slice_with(aggregates.len(), |index| aggregates[index].0)
            .map_err(|_| arena_full())?;
        let expressions = arena
            .alloc_slice_with(aggregates.len(), |index| aggregates[index].1)
            .map_err(|_| arena_full())?;
        let demand = pax_column_demand(scope, from, expressions);
        let templates = arena
            .alloc_slice_with(aggregates.len(), |_| AggState::default())
            .map_err(|_| arena_full())?;
        let layouts = arena
            .alloc_slice_with(aggregates.len(), |_| FixedState::Count)
            .map_err(|_| arena_full())?;
        let mut fixed = true;
        let mut stride = 0;
        for ((template, layout), (_, node)) in
            templates.iter_mut().zip(layouts.iter_mut()).zip(aggregates)
        {
            template.init(node, storage, txid, &super::ScopeCols(scope), arena)?;
            match template.fixed_state(node, &super::ScopeCols(scope)) {
                Some(state) => {
                    *layout = state;
                    stride += state_bytes(state);
                }
                None => fixed = false,
            }
        }
        let method = if fixed {
            Method::States { layouts, stride }
        } else {
            Method::Inputs
        };
        let pool = storage.plan_build();
        Ok(Self {
            method,
            templates,
            keys,
            collations,
            set,
            aggregates,
            pointers,
            having: statement.having,
            demand,
            inputs: pool.lease(0).map_err(group_full)?,
            input_bytes: pool.lease(0).map_err(group_full)?,
            tree: pool.lease(0).map_err(group_full)?,
            root: NO_NODE,
            states: pool.lease(0).map_err(group_full)?,
            values: pool.lease(0).map_err(group_full)?,
            order: pool.lease(0).map_err(group_full)?,
            groups: pool.lease(0).map_err(group_full)?,
            group_bytes: pool.lease(0).map_err(group_full)?,
            phase: Phase::Filling,
            current: None,
        })
    }

    pub(super) fn filling(&self) -> bool {
        matches!(self.phase, Phase::Filling)
    }

    pub(super) fn has_current(&self) -> bool {
        self.current.is_some()
    }

    pub(super) fn finish_row(&mut self) {
        self.current = None;
    }

    /// Takes one passing input row: folds it into its group's fixed states,
    /// or saves its keys and the columns its aggregates read.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn accept<'work>(
        &mut self,
        scope: &QueryScope<'_>,
        storage: &Storage,
        txid: u32,
        row: &scan::JoinRow<'_, 'work, '_>,
        work: &'work Arena,
        params: &[Datum<'work>],
        hooks: &EvalHooks<'_, 'work>,
    ) -> Result<(), SqlError>
    where
        'a: 'work,
    {
        let Method::States { layouts, stride } = self.method else {
            let record = self.encode_input(scope, storage, row, work, params, hooks)?;
            return push_record(&mut self.inputs, &mut self.input_bytes, record);
        };
        let mut keys = crate::mem::arena::ArenaList::new(work);
        self.push_keys(&mut keys, storage, row, work, params, hooks)?;
        let key = encode_values(keys.as_slice(), work)?;
        let group = self.find_or_insert(storage, txid, key)?;
        let mut offset = group * stride;
        for ((template, &layout), (_, node)) in
            self.templates.iter().zip(layouts).zip(self.aggregates)
        {
            let saved = &mut self.states.as_mut_slice()[offset..offset + state_bytes(layout)];
            let (state, slot) = saved.split_at_mut(layout.bytes());
            let held = if layout == FixedState::Reference {
                load_value(self.values.as_slice(), slot, work)?
            } else {
                None
            };
            let mut value = held;
            template.update_fixed(layout, state, &mut value, node, work, params, row, hooks)?;
            if layout == FixedState::Reference && value != held {
                store_value(&mut self.values, slot, value, work)?;
            }
            offset += state_bytes(layout);
        }
        Ok(())
    }

    fn push_keys<'work>(
        &self,
        values: &mut crate::mem::arena::ArenaList<'work, Datum<'work>>,
        storage: &Storage,
        row: &scan::JoinRow<'_, 'work, '_>,
        work: &'work Arena,
        params: &[Datum<'work>],
        hooks: &EvalHooks<'_, 'work>,
    ) -> Result<(), SqlError>
    where
        'a: 'work,
    {
        for (key, &collation) in self.keys.iter().zip(self.collations) {
            let value = crate::sql::eval::text_view(eval_full(key, work, params, row, hooks)?);
            storage.validate_text_collation(collation, &value)?;
            values.push(value).map_err(|_| arena_full())?;
        }
        Ok(())
    }

    /// The group whose key equals `key`, created with fresh states when it
    /// is new.
    fn find_or_insert(
        &mut self,
        storage: &Storage,
        txid: u32,
        key: &[u8],
    ) -> Result<usize, SqlError> {
        let Method::States { layouts, stride } = self.method else {
            unreachable!("only fixed states keep a tree");
        };
        let mut path = [(NO_NODE, false); MAX_TREE_DEPTH];
        let mut depth = 0;
        let mut node = self.root;
        while node != NO_NODE {
            let existing = record(&self.inputs, self.input_bytes.as_slice(), node as usize);
            let left = match compare_keys(storage, txid, self.collations, key, existing)? {
                core::cmp::Ordering::Equal => return Ok(node as usize),
                ordering => ordering.is_lt(),
            };
            let entry = self.tree.as_slice()[node as usize];
            path[depth] = (node, left);
            depth += 1;
            node = if left { entry.left } else { entry.right };
        }
        let index = self.tree.len();
        let added = u32::try_from(index)
            .ok()
            .filter(|&index| index != NO_NODE)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained grouping exceeds {} groups",
                    NO_NODE
                )
                .plan_build_exhausted()
            })?;
        reserve(&mut self.states, (index + 1) * stride)?;
        reserve(&mut self.tree, index + 1)?;
        push_record(&mut self.inputs, &mut self.input_bytes, key)?;
        let start = self.states.len();
        for _ in 0..stride {
            self.states.push_reserved(0).map_err(group_full)?;
        }
        let mut offset = start;
        for (template, &layout) in self.templates.iter().zip(layouts) {
            let saved = &mut self.states.as_mut_slice()[offset..offset + state_bytes(layout)];
            let (state, slot) = saved.split_at_mut(layout.bytes());
            template.initial_fixed(layout, state)?;
            if layout == FixedState::Reference {
                slot[4..8].copy_from_slice(&NO_VALUE.to_le_bytes());
            }
            offset += state_bytes(layout);
        }
        self.tree
            .push_reserved(TreeNode {
                left: NO_NODE,
                right: NO_NODE,
                height: 1,
            })
            .map_err(group_full)?;
        let tree = self.tree.as_mut_slice();
        let mut child = added;
        for &(parent, left) in path[..depth].iter().rev() {
            if left {
                tree[parent as usize].left = child;
            } else {
                tree[parent as usize].right = child;
            }
            let before = tree[parent as usize].height;
            child = rebalance(tree, parent);
            if child == parent && tree[parent as usize].height == before {
                // Nothing above this subtree changed.
                return Ok(index);
            }
        }
        self.root = child;
        Ok(index)
    }

    /// Encodes one passing input row: its keys, then its sources' columns
    /// the aggregates read.
    fn encode_input<'work>(
        &self,
        scope: &QueryScope<'_>,
        storage: &Storage,
        row: &scan::JoinRow<'_, 'work, '_>,
        work: &'work Arena,
        params: &[Datum<'work>],
        hooks: &EvalHooks<'_, 'work>,
    ) -> Result<&'work [u8], SqlError>
    where
        'a: 'work,
    {
        let mut values = crate::mem::arena::ArenaList::new(work);
        self.push_keys(&mut values, storage, row, work, params, hooks)?;
        for table in 0..scope.n {
            let width = scope.row_width(table);
            let saved = row
                .table_values(table)
                .filter(|_| row.table_present(table) == Some(true));
            values
                .push(Datum::Bool(saved.is_some()))
                .map_err(|_| arena_full())?;
            let n_columns = scope.defs.get(table).expect("resolved").n_columns;
            for column in 0..width {
                let value = saved
                    .filter(|_| self.demand.references_column(table, column, n_columns))
                    .and_then(|columns| columns.get(column).copied())
                    .unwrap_or(Datum::Null);
                values.push(value).map_err(|_| arena_full())?;
            }
        }
        encode_values(values.as_slice(), work)
    }

    /// Ends the input: orders the groups by key, or the saved input rows by
    /// key with equal keys in input order.
    pub(super) fn finish_input(
        &mut self,
        storage: &Storage,
        txid: u32,
        work: &Arena,
    ) -> Result<(), SqlError> {
        if let Method::States { .. } = self.method {
            // A query without GROUP BY has one group, even over no rows.
            if self.keys.is_empty() && self.tree.is_empty() {
                let mark = work.mark();
                let created = encode_values(&[], work)
                    .and_then(|key| self.find_or_insert(storage, txid, key));
                // SAFETY: the key was copied into the grouping's lease.
                unsafe { work.rewind_to(mark) };
                created?;
            }
            reserve(&mut self.order, self.tree.len())?;
            let tree = self.tree.as_slice();
            let mut stack = [NO_NODE; MAX_TREE_DEPTH];
            let mut depth = 0;
            let mut node = self.root;
            while node != NO_NODE || depth > 0 {
                while node != NO_NODE {
                    stack[depth] = node;
                    depth += 1;
                    node = tree[node as usize].left;
                }
                depth -= 1;
                let visited = stack[depth];
                self.order.push_reserved(visited).map_err(group_full)?;
                node = tree[visited as usize].right;
            }
            self.phase = Phase::Folding { next: 0 };
            return Ok(());
        }
        let collations = self.collations;
        let bytes = self.input_bytes.as_slice();
        heap_sort(self.inputs.as_mut_slice(), |left, right| {
            let left_row = &bytes[left.0 as usize..(left.0 + left.1) as usize];
            let right_row = &bytes[right.0 as usize..(right.0 + right.1) as usize];
            Ok(
                match compare_keys(storage, txid, collations, left_row, right_row)? {
                    core::cmp::Ordering::Equal => left.0 < right.0,
                    ordering => ordering.is_lt(),
                },
            )
        })?;
        self.phase = Phase::Folding { next: 0 };
        Ok(())
    }

    /// Folds every group not yet folded; a failure keeps the groups folded
    /// so far. The saved input is released once every group is folded.
    pub(super) fn fold(
        &mut self,
        scope: &QueryScope<'_>,
        storage: &Storage,
        txid: u32,
        work: &Arena,
        params: &[Datum<'_>],
    ) -> Result<(), SqlError> {
        let Phase::Folding { mut next } = self.phase else {
            return Ok(());
        };
        if let Method::States { .. } = self.method {
            // Each group's results are computed as it is returned.
            self.phase = Phase::Output { next: 0 };
            return Ok(());
        }
        let total = self.inputs.len();
        // A query without GROUP BY has one group, even over no rows.
        let global = self.keys.is_empty();
        while next < total || global && self.groups.is_empty() {
            check_timeout()?;
            let bytes = self.input_bytes.as_slice();
            let mut end = next;
            while end < total {
                if end > next
                    && !compare_keys(
                        storage,
                        txid,
                        self.collations,
                        record(&self.inputs, bytes, next),
                        record(&self.inputs, bytes, end),
                    )?
                    .is_eq()
                {
                    break;
                }
                end += 1;
            }
            let mark = work.mark();
            let folded = fold_group(
                self.keys,
                self.set,
                self.templates,
                self.aggregates,
                scope,
                storage,
                txid,
                (next..end).map(|index| record(&self.inputs, bytes, index)),
                work,
                params,
            )
            .and_then(|group| push_record(&mut self.groups, &mut self.group_bytes, group));
            // SAFETY: the group's record was copied into its lease.
            unsafe { work.rewind_to(mark) };
            folded?;
            next = end;
            self.phase = Phase::Folding { next };
        }
        let pool = storage.plan_build();
        self.inputs = pool.lease(0).map_err(group_full)?;
        self.input_bytes = pool.lease(0).map_err(group_full)?;
        self.phase = Phase::Output { next: 0 };
        Ok(())
    }

    /// Makes the next group current; `false` at the end.
    pub(super) fn advance(&mut self) -> bool {
        let Phase::Output { next } = &mut self.phase else {
            return false;
        };
        let total = match self.method {
            Method::States { .. } => self.order.len(),
            Method::Inputs => self.groups.len(),
        };
        if *next < total {
            self.current = Some(match self.method {
                Method::States { .. } => self.order.as_slice()[*next] as usize,
                Method::Inputs => *next,
            });
            *next += 1;
            true
        } else {
            self.current = None;
            false
        }
    }

    /// Runs `each` over the current group: its key values and aggregate
    /// results bound as `HAVING` and the output list read them.
    pub(super) fn with_group<'work, R>(
        &'work self,
        scope: &QueryScope<'_>,
        work: &'work Arena,
        hooks: &EvalHooks<'_, 'work>,
        each: impl FnOnce(
            &crate::sql::query::materialize::ScopeSchema<'_, '_>,
            &EvalHooks<'_, 'work>,
        ) -> Result<R, SqlError>,
    ) -> Result<R, SqlError>
    where
        'a: 'work,
    {
        let index = self.current.ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained grouping has no current group"
            )
        })?;
        let values = work
            .alloc_slice_with(self.keys.len() + self.aggregates.len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        match self.method {
            Method::States { layouts, stride } => {
                let (keys, results) = values.split_at_mut(self.keys.len());
                let key = record(&self.inputs, self.input_bytes.as_slice(), index);
                crate::sql::exec::decode_projected_row_into(key, work, keys)?;
                let mut offset = index * stride;
                for ((result, template), &layout) in
                    results.iter_mut().zip(self.templates).zip(layouts)
                {
                    let saved = &self.states.as_slice()[offset..offset + state_bytes(layout)];
                    let (state, slot) = saved.split_at(layout.bytes());
                    let value = if layout == FixedState::Reference {
                        load_value(self.values.as_slice(), slot, work)?
                    } else {
                        None
                    };
                    *result = template.finish_fixed(layout, state, value, work, hooks.catalog)?;
                    offset += state_bytes(layout);
                }
            }
            Method::Inputs => {
                let group = record(&self.groups, self.group_bytes.as_slice(), index);
                crate::sql::exec::decode_projected_row_into(group, work, values)?;
            }
        }
        let (keys, results) = values.split_at(self.keys.len());
        let group_hooks = EvalHooks {
            group: Some((self.keys, keys, self.set)),
            aggs: Some((self.pointers, results)),
            ..*hooks
        };
        each(
            &crate::sql::query::materialize::ScopeSchema(scope),
            &group_hooks,
        )
    }

    /// Whether the current group passes `HAVING`.
    pub(super) fn having_passes<'work>(
        &'work self,
        scope: &QueryScope<'_>,
        work: &'work Arena,
        params: &[Datum<'work>],
        hooks: &EvalHooks<'_, 'work>,
    ) -> Result<bool, SqlError>
    where
        'a: 'work,
    {
        let Some(having) = self.having else {
            return Ok(true);
        };
        self.with_group(scope, work, hooks, |row, hooks| {
            match eval_full(having, work, params, row, hooks)? {
                Datum::Bool(true) => Ok(true),
                Datum::Bool(false) | Datum::Null => Ok(false),
                _ => Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "argument of HAVING must be type boolean"
                )),
            }
        })
    }
}

fn encode_values<'w>(values: &[Datum<'_>], work: &'w Arena) -> Result<&'w [u8], SqlError> {
    let length = crate::sql::exec::projected_row_len(values)?;
    let bytes = work
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| arena_full())?;
    crate::sql::exec::encode_projected_into(values, bytes)?;
    Ok(bytes)
}

/// Folds one group's aggregates over its saved input rows, in input order,
/// and encodes its keys (its first row's) and results.
#[allow(clippy::too_many_arguments)]
fn fold_group<'t: 'w, 'w>(
    keys: &'w [&'w Expr<'w>],
    set: GroupingSet<'w>,
    templates: &[AggState<'t>],
    aggregates: AggregateNodes<'w>,
    scope: &QueryScope<'_>,
    storage: &Storage,
    txid: u32,
    mut rows: impl Iterator<Item = &'w [u8]>,
    work: &'w Arena,
    params: &[Datum<'w>],
) -> Result<&'w [u8], SqlError> {
    let catalog = StorageCatalog {
        storage,
        routine_workspace: work,
        txid,
        invocations: None,
        statement_arena: None,
    };
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    let states = work
        .alloc_slice_with(aggregates.len(), |_| AggState::default())
        .map_err(|_| arena_full())?;
    for (state, template) in states.iter_mut().zip(templates) {
        *state = template.fresh_copy(work)?;
    }
    let mut offsets = [0usize; MAX_RETAINED_SOURCES];
    let mut width = keys.len();
    for (table, offset) in offsets.iter_mut().enumerate().take(scope.n) {
        *offset = width + 1;
        width += 1 + scope.row_width(table);
    }
    let input = work
        .alloc_slice_with(width, |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let output = work
        .alloc_slice_with(keys.len() + aggregates.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let mut first = true;
    for row in rows.by_ref() {
        crate::sql::exec::decode_projected_row_into(row, work, input)?;
        if first {
            output[..keys.len()].copy_from_slice(&input[..keys.len()]);
            first = false;
        }
        let row = scan::JoinRow {
            scope,
            values: scan::JoinRowValues::Flat {
                values: input,
                offsets: &offsets[..scope.n],
            },
            rowids: &[None; MAX_RETAINED_SOURCES][..scope.n],
        };
        for (state, (_, node)) in states.iter_mut().zip(aggregates) {
            state.update(node, work, params, &row, &hooks)?;
        }
    }
    let (key_values, results) = output.split_at_mut(keys.len());
    let direct_hooks = EvalHooks {
        group: Some((keys, &*key_values, set)),
        ..hooks
    };
    for (result, (state, (_, node))) in results.iter_mut().zip(states.iter_mut().zip(aggregates)) {
        state.initialize_direct_inputs(
            node,
            work,
            params,
            &crate::sql::query::materialize::ScopeSchema(scope),
            &direct_hooks,
        )?;
        *result = state.finish(work, hooks.catalog)?;
    }
    let length = crate::sql::exec::projected_row_len(output)?;
    let bytes = work
        .alloc_slice_with(length, |_| 0u8)
        .map_err(|_| arena_full())?;
    crate::sql::exec::encode_projected_into(output, bytes)?;
    Ok(bytes)
}
