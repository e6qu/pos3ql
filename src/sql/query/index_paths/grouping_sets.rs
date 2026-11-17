//! Grouping sets as PostgreSQL 18's planner arranges and costs them:
//! `preprocess_grouping_sets` chains the sets into rollups
//! (`extract_rollup_sets`, a maximum matching of each set to a superset,
//! `BipartiteMatch`) ordered as prefixes (`reorder_grouping_sets`), and
//! `consider_groupingsets_paths` weighs hashing every set (`MixedAggregate`
//! when empty sets need the sorted phase) against sorting the input for the
//! first rollup, hashing the other rollups that fit in `hash_mem` (a
//! `DiscreteKnapsack` of their table sizes) and sorting the rest
//! (`create_groupingsets_path`).
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/planner.c
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/util/pathnode.c
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/lib/bipartite_match.c
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/lib/knapsack.c

use core::cmp::Ordering;

use crate::mem::arena::Arena;
use crate::sql::eval::{SqlError, arena_full};

use super::aggregate::{AggInput, AggStrategy, AggregateCosts, cost_agg, hash_agg_entry_size};
use super::paths::tuplesort_cost;

/// The most grouping columns a set holds here.
pub(crate) const MAX_SET_COLUMNS: usize = 64;

/// One grouping set: grouping positions in the order PostgreSQL lists them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GroupingSet {
    pub(crate) columns: [u8; MAX_SET_COLUMNS],
    pub(crate) len: usize,
    /// `GroupingSetData.numGroups`.
    pub(crate) groups: f64,
}

impl GroupingSet {
    const EMPTY: Self = Self {
        columns: [0; MAX_SET_COLUMNS],
        len: 0,
        groups: 1.0,
    };

    pub(crate) fn columns(&self) -> &[u8] {
        &self.columns[..self.len]
    }

    fn mask(&self) -> u64 {
        self.columns()
            .iter()
            .fold(0, |mask, &column| mask | (1u64 << column))
    }
}

/// `RollupData`: sets one aggregation pass computes, the largest first,
/// each a prefix of `clause` when sorted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Rollup<'a> {
    /// The grouping positions in the order the pass sorts or hashes by.
    pub(crate) clause: GroupingSet,
    pub(crate) sets: &'a [GroupingSet],
    pub(crate) groups: f64,
    pub(crate) hashed: bool,
    pub(crate) hashable: bool,
}

/// The node's `AggStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetsStrategy {
    Plain,
    Sorted,
    Hashed,
    Mixed,
}

impl SetsStrategy {
    pub(crate) const fn node_name(self) -> &'static str {
        match self {
            Self::Plain => "Aggregate",
            Self::Sorted => "GroupAggregate",
            Self::Hashed => "HashAggregate",
            Self::Mixed => "MixedAggregate",
        }
    }
}

/// The grouping sets path the planner keeps.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SetsPlan<'a> {
    pub(crate) strategy: SetsStrategy,
    /// The top node's rollup first, then its chain.
    pub(crate) rollups: &'a [Rollup<'a>],
    /// The `Sort` of the input by the first rollup's order, when the path
    /// reads sorted input.
    pub(crate) input_sort: Option<(f64, f64)>,
    /// The relation's rows and width the aggregation reads.
    pub(crate) input: AggInput,
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) rows: f64,
    /// The sort of the groups for the `ORDER BY`.
    pub(crate) output_sort: Option<(f64, f64)>,
}

impl SetsPlan<'_> {
    /// Whether the chained rollup at `index` sorts its input itself
    /// (`create_groupingsets_plan`).
    pub(crate) fn sorts(&self, index: usize) -> bool {
        if index == 0 || self.rollups[index].hashed {
            return false;
        }
        let mut first_sort = self.rollups[0].hashed;
        for rollup in &self.rollups[1..index] {
            if !rollup.hashed {
                first_sort = false;
            }
        }
        !first_sort
    }
}

/// What the planner needs to know about the query.
pub(crate) struct SetsQuery<'s> {
    /// The expanded sets, as `expand_grouping_sets` builds them before
    /// sorting them by length, each with its estimated groups.
    pub(crate) sets: &'s [GroupingSet],
    /// `GROUP BY DISTINCT`.
    pub(crate) distinct: bool,
    /// The `ORDER BY` keys as grouping positions (`None` for another key)
    /// with their directions.
    pub(crate) order: &'s [Option<u8>],
    /// Whether the `ORDER BY` follows the grouping order of the first
    /// rollup when its keys are a prefix of it.
    pub(crate) aggregates: AggregateCosts,
    /// The cheapest path of the relation.
    pub(crate) input: AggInput,
    /// The width of the rows the aggregation returns.
    pub(crate) output_width: f64,
}

/// A candidate path, as `add_path` weighs it.
#[derive(Clone, Copy)]
struct SetsPath<'a> {
    plan: SetsPlan<'a>,
    disabled: u32,
    /// Its rows are in the `ORDER BY` order (`pathkeys`).
    ordered: bool,
}

/// `expand_grouping_sets`'s length order: `list_sort` with
/// `cmp_list_len_asc`, PostgreSQL's quicksort, and `GROUP BY DISTINCT`'s
/// order by length and contents with duplicates removed.
fn sorted_sets<'a>(query: &SetsQuery<'_>, arena: &'a Arena) -> Result<&'a [GroupingSet], SqlError> {
    let sets = arena
        .alloc_slice_with(query.sets.len(), |at| query.sets[at])
        .map_err(|_| arena_full())?;
    let order = arena
        .alloc_slice_with(sets.len(), |at| at as u32)
        .map_err(|_| arena_full())?;
    if query.distinct && sets.len() >= 2 {
        for set in sets.iter_mut() {
            set.columns[..set.len].sort_unstable();
        }
        crate::sql::pg_qsort::pg_qsort(order, &mut |left, right| {
            let (left, right) = (&sets[left as usize], &sets[right as usize]);
            Ok(left
                .len
                .cmp(&right.len)
                .then(left.columns().cmp(right.columns())))
        })?;
        let mut kept = 0;
        for at in 0..order.len() {
            let set = &sets[order[at] as usize];
            if kept == 0 || sets[order[kept - 1] as usize].columns() != set.columns() {
                order[kept] = order[at];
                kept += 1;
            }
        }
        return arena
            .alloc_slice_with(kept, |at| sets[order[at] as usize])
            .map(|sets| &*sets)
            .map_err(|_| arena_full());
    }
    crate::sql::pg_qsort::pg_qsort(order, &mut |left, right| {
        Ok(sets[left as usize].len.cmp(&sets[right as usize].len))
    })?;
    arena
        .alloc_slice_with(order.len(), |at| sets[order[at] as usize])
        .map(|sets| &*sets)
        .map_err(|_| arena_full())
}

/// `BipartiteMatch`: Hopcroft-Karp over `adjacency` (1-based, each list
/// in PostgreSQL's order), returning `pair_uv` and `pair_vu`.
fn bipartite_match<'a>(
    size: usize,
    adjacency: &[&[u16]],
    arena: &'a Arena,
) -> Result<(&'a [u16], &'a [u16]), SqlError> {
    const INFINITY: u16 = u16::MAX;
    let pair_uv = arena
        .alloc_slice_with(size + 1, |_| 0u16)
        .map_err(|_| arena_full())?;
    let pair_vu = arena
        .alloc_slice_with(size + 1, |_| 0u16)
        .map_err(|_| arena_full())?;
    let distance = arena
        .alloc_slice_with(size + 1, |_| 0u16)
        .map_err(|_| arena_full())?;
    let queue = arena
        .alloc_slice_with(size + 2, |_| 0u16)
        .map_err(|_| arena_full())?;
    fn depth_search(
        u: usize,
        adjacency: &[&[u16]],
        distance: &mut [u16],
        pair_uv: &mut [u16],
        pair_vu: &mut [u16],
    ) -> bool {
        if u == 0 {
            return true;
        }
        if distance[u] == INFINITY {
            return false;
        }
        let next = distance[u] + 1;
        for &v in adjacency[u].iter().rev() {
            let v = usize::from(v);
            let paired = usize::from(pair_vu[v]);
            if distance[paired] == next
                && depth_search(paired, adjacency, distance, pair_uv, pair_vu)
            {
                pair_vu[v] = u as u16;
                pair_uv[u] = v as u16;
                return true;
            }
        }
        distance[u] = INFINITY;
        false
    }
    loop {
        // `hk_breadth_search`.
        let mut head = 0;
        let mut tail = 0;
        distance[0] = INFINITY;
        for u in 1..=size {
            if pair_uv[u] == 0 {
                distance[u] = 0;
                queue[head] = u as u16;
                head += 1;
            } else {
                distance[u] = INFINITY;
            }
        }
        while tail < head {
            let u = usize::from(queue[tail]);
            tail += 1;
            if distance[u] < distance[0] {
                for &v in adjacency[u].iter().rev() {
                    let next = usize::from(pair_vu[usize::from(v)]);
                    if distance[next] == INFINITY {
                        distance[next] = distance[u] + 1;
                        queue[head] = next as u16;
                        head += 1;
                    }
                }
            }
        }
        if distance[0] == INFINITY {
            break;
        }
        for u in 1..=size {
            if pair_uv[u] == 0 {
                depth_search(u, adjacency, distance, pair_uv, pair_vu);
            }
        }
    }
    Ok((pair_uv, pair_vu))
}

/// `extract_rollup_sets`: the length-sorted sets in chains, each set after
/// one it contains, the chains in the order of their first sets and the
/// empty sets leading the first.
fn extract_rollup_sets<'a>(
    sets: &[GroupingSet],
    arena: &'a Arena,
) -> Result<&'a [&'a [GroupingSet]], SqlError> {
    let empty = sets.iter().take_while(|set| set.len == 0).count();
    if empty == sets.len() {
        let all = arena
            .alloc_slice_with(sets.len(), |at| sets[at])
            .map_err(|_| arena_full())?;
        return arena
            .alloc_slice_with(1, |_| &*all)
            .map(|chains| &*chains)
            .map_err(|_| arena_full());
    }
    let raw = sets.len() - empty;
    // Distinct sets from 1, each with the raw sets it stands for.
    let masks = arena
        .alloc_slice_with(raw + 1, |_| 0u64)
        .map_err(|_| arena_full())?;
    let first_of = arena
        .alloc_slice_with(raw + 1, |_| 0usize)
        .map_err(|_| arena_full())?;
    let duplicate_of = arena
        .alloc_slice_with(raw, |_| 0usize)
        .map_err(|_| arena_full())?;
    let adjacency_store = arena
        .alloc_slice_with(raw * (raw + 1), |_| 0u16)
        .map_err(|_| arena_full())?;
    let adjacency_len = arena
        .alloc_slice_with(raw + 1, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut size_start = 0usize;
    let mut size = 0usize;
    let mut next = 1usize;
    for (at, set) in sets[empty..].iter().enumerate() {
        let mask = set.mask();
        let mut duplicate = 0;
        if size == set.len {
            duplicate = (size_start..next)
                .find(|&k| k > 0 && masks[k] == mask)
                .unwrap_or(0);
        } else if size < set.len {
            size = set.len;
            size_start = next;
        }
        if duplicate > 0 {
            duplicate_of[at] = duplicate;
            continue;
        }
        masks[next] = mask;
        first_of[next] = at;
        duplicate_of[at] = next;
        // Each smaller set it contains, the most recent first, as
        // PostgreSQL lists them; its searches walk the list from its end.
        let mut count = 0;
        for k in (1..size_start).rev() {
            if masks[k] & !mask == 0 {
                adjacency_store[next * raw + count] = k as u16;
                count += 1;
            }
        }
        adjacency_len[next] = count;
        next += 1;
    }
    let distinct = next - 1;
    let adjacency = arena
        .alloc_slice_with(distinct + 1, |u| {
            if u == 0 {
                &[][..]
            } else {
                &adjacency_store[u * raw..u * raw + adjacency_len[u]]
            }
        })
        .map_err(|_| arena_full())?;
    let (pair_uv, pair_vu) = bipartite_match(distinct, adjacency, arena)?;
    let chains = arena
        .alloc_slice_with(distinct + 1, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut chain_count = 0;
    for i in 1..=distinct {
        let u = usize::from(pair_vu[i]);
        let v = usize::from(pair_uv[i]);
        chains[i] = if u > 0 && u < i {
            chains[u]
        } else if v > 0 && v < i {
            chains[v]
        } else {
            chain_count += 1;
            chain_count
        };
    }
    let lengths = arena
        .alloc_slice_with(chain_count + 1, |_| 0usize)
        .map_err(|_| arena_full())?;
    lengths[1] += empty;
    for &distinct_index in duplicate_of.iter() {
        lengths[chains[distinct_index]] += 1;
    }
    let mut result = crate::mem::arena::ArenaList::new(arena);
    for (chain, &length) in lengths.iter().enumerate().skip(1) {
        let members = arena
            .alloc_slice_with(length, |_| GroupingSet::EMPTY)
            .map_err(|_| arena_full())?;
        let mut filled = 0;
        if chain == 1 {
            for set in &sets[..empty] {
                members[filled] = *set;
                filled += 1;
            }
        }
        // `orig_sets[i]` in distinct order, each with its duplicates.
        for i in 1..=distinct {
            if chains[i] != chain {
                continue;
            }
            members[filled] = sets[empty + first_of[i]];
            filled += 1;
            for (at, &of) in duplicate_of.iter().enumerate() {
                if of == i && at != first_of[i] {
                    members[filled] = sets[empty + at];
                    filled += 1;
                }
            }
        }
        result.push(&*members).map_err(|_| arena_full())?;
    }
    arena
        .alloc_slice_copy(result.as_slice())
        .map(|chains| &*chains)
        .map_err(|_| arena_full())
}

/// `reorder_grouping_sets`: the chain's sets largest first, each listing
/// its columns in the order of the sets it contains, which follows the
/// `ORDER BY` as far as it can.
fn reorder_grouping_sets<'a>(
    chain: &[GroupingSet],
    order: &[Option<u8>],
    arena: &'a Arena,
) -> Result<&'a [GroupingSet], SqlError> {
    let result = arena
        .alloc_slice_with(chain.len(), |_| GroupingSet::EMPTY)
        .map_err(|_| arena_full())?;
    let mut previous = GroupingSet::EMPTY;
    let mut order = order;
    for (at, candidate) in chain.iter().enumerate() {
        let mut fresh = [0u8; MAX_SET_COLUMNS];
        let mut fresh_len = 0;
        for &column in candidate.columns() {
            if !previous.columns().contains(&column) {
                fresh[fresh_len] = column;
                fresh_len += 1;
            }
        }
        while order.len() > previous.len && fresh_len > 0 {
            match order[previous.len] {
                Some(column) if fresh[..fresh_len].contains(&column) => {
                    previous.columns[previous.len] = column;
                    previous.len += 1;
                    let at = fresh[..fresh_len]
                        .iter()
                        .position(|&held| held == column)
                        .expect("checked above");
                    fresh.copy_within(at + 1..fresh_len, at);
                    fresh_len -= 1;
                }
                _ => {
                    order = &[];
                    break;
                }
            }
        }
        for &column in &fresh[..fresh_len] {
            previous.columns[previous.len] = column;
            previous.len += 1;
        }
        result[chain.len() - 1 - at] = GroupingSet {
            groups: candidate.groups,
            ..previous
        };
    }
    Ok(result)
}

/// `estimate_hashagg_tablesize`.
fn table_size(query: &SetsQuery<'_>, groups: f64) -> f64 {
    hash_agg_entry_size(
        query.aggregates.transitions,
        query.input.width,
        query.aggregates.transition_space,
    ) * groups
}

/// `get_hash_memory_limit` in bytes.
fn hash_memory() -> f64 {
    f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0 * 2.0
}

/// `DiscreteKnapsack` with every item worth one: the items whose weights
/// fit in `capacity` that maximize their count.
fn knapsack(capacity: usize, weights: &[usize], arena: &Arena) -> Result<u64, SqlError> {
    let values = arena
        .alloc_slice_with(capacity + 1, |_| 0.0f64)
        .map_err(|_| arena_full())?;
    let sets = arena
        .alloc_slice_with(capacity + 1, |_| 0u64)
        .map_err(|_| arena_full())?;
    for (item, &weight) in weights.iter().enumerate() {
        if weight > capacity {
            continue;
        }
        for j in (weight..=capacity).rev() {
            let other = j - weight;
            if values[j] <= values[other] + 1.0 {
                if j != other {
                    sets[j] = sets[other];
                }
                sets[j] |= 1 << item;
                values[j] = values[other] + 1.0;
            }
        }
    }
    Ok(sets[capacity])
}

/// `create_groupingsets_path`'s cost of `rollups` under `strategy`.
fn grouping_sets_path<'a>(
    query: &SetsQuery<'_>,
    strategy: SetsStrategy,
    rollups: &'a [Rollup<'a>],
    input: AggInput,
    input_sort: Option<(f64, f64)>,
) -> (SetsPlan<'a>, u32) {
    let methods = crate::sql::guc::planner_methods();
    let mut strategy = strategy;
    if strategy == SetsStrategy::Sorted && rollups.len() == 1 && rollups[0].clause.len == 0 {
        strategy = SetsStrategy::Plain;
    }
    if strategy == SetsStrategy::Mixed && rollups.len() == 1 {
        strategy = SetsStrategy::Hashed;
    }
    let agg = |strategy: SetsStrategy| match strategy {
        SetsStrategy::Plain => AggStrategy::Plain,
        SetsStrategy::Sorted => AggStrategy::Sorted,
        SetsStrategy::Hashed => AggStrategy::Hashed,
        SetsStrategy::Mixed => AggStrategy::Mixed,
    };
    let disabled_by = |strategy: SetsStrategy| {
        u32::from(
            matches!(strategy, SetsStrategy::Hashed | SetsStrategy::Mixed) && !methods.hashagg,
        )
    };
    let first = rollups[0];
    let (startup, mut total, mut rows) = cost_agg(
        agg(strategy),
        &query.aggregates,
        first.sets[0].len,
        first.groups,
        input,
    );
    let mut disabled = disabled_by(strategy) + u32::from(input_sort.is_some() && !methods.sort);
    let mut first_sort = !first.hashed;
    for rollup in &rollups[1..] {
        let columns = rollup.sets[0].len;
        let (_, rollup_total, rollup_rows) = if rollup.hashed || !first_sort {
            let strategy = if rollup.hashed {
                SetsStrategy::Hashed
            } else {
                SetsStrategy::Sorted
            };
            if !rollup.hashed {
                first_sort = true;
            }
            disabled += disabled_by(strategy);
            cost_agg(
                agg(strategy),
                &query.aggregates,
                columns,
                rollup.groups,
                AggInput {
                    startup: 0.0,
                    total: 0.0,
                    ..input
                },
            )
        } else {
            let (sort_startup, sort_run) =
                tuplesort_cost(input.rows, input.width, None, super::Costs::current());
            disabled += u32::from(!methods.sort);
            cost_agg(
                AggStrategy::Sorted,
                &query.aggregates,
                columns,
                rollup.groups,
                AggInput {
                    startup: sort_startup,
                    total: sort_startup + sort_run,
                    ..input
                },
            )
        };
        total += rollup_total;
        rows += rollup_rows;
    }
    (
        SetsPlan {
            strategy,
            rollups,
            input_sort,
            input,
            startup,
            total,
            rows,
            output_sort: None,
        },
        disabled,
    )
}

/// A rollup of one set, hashed (`consider_groupingsets_paths`).
fn hashed_rollup<'a>(set: GroupingSet, arena: &'a Arena) -> Result<Rollup<'a>, SqlError> {
    let sets = arena.alloc_slice_copy(&[set]).map_err(|_| arena_full())?;
    Ok(Rollup {
        clause: set,
        sets,
        groups: set.groups,
        hashed: true,
        hashable: true,
    })
}

/// The grouping sets path PostgreSQL keeps for `query`, with the sort of
/// its groups for the `ORDER BY` when it needs one.
pub(crate) fn plan_grouping_sets<'a>(
    query: &SetsQuery<'_>,
    arena: &'a Arena,
) -> Result<SetsPlan<'a>, SqlError> {
    let methods = crate::sql::guc::planner_methods();
    let sets = sorted_sets(query, arena)?;
    let chains = extract_rollup_sets(sets, arena)?;
    let mut rollups = crate::mem::arena::ArenaList::new(arena);
    let mut any_hashable = false;
    for chain in chains {
        let order = if chains.len() == 1 { query.order } else { &[] };
        let ordered = reorder_grouping_sets(chain, order, arena)?;
        let first = ordered[0];
        let hashable = first.len > 0;
        any_hashable |= hashable;
        rollups
            .push(Rollup {
                clause: first,
                sets: ordered,
                groups: ordered.iter().map(|set| set.groups).sum(),
                hashed: false,
                hashable,
            })
            .map_err(|_| arena_full())?;
    }
    let rollups: &'a [Rollup<'a>] = arena
        .alloc_slice_copy(rollups.as_slice())
        .map_err(|_| arena_full())?;
    let all_groups: f64 = rollups.iter().map(|rollup| rollup.groups).sum();
    let memory = hash_memory();
    let input = query.input;
    let mut candidates = crate::mem::arena::ArenaList::new(arena);
    // Unsorted input: hash every set, the empty ones in the sorted phase.
    if table_size(query, all_groups) <= memory || rollups.is_empty() {
        let mut hashed = crate::mem::arena::ArenaList::new(arena);
        let mut empties = crate::mem::arena::ArenaList::new(arena);
        let mut usable = true;
        for rollup in rollups {
            if !rollup.hashable {
                // A rollup of only empty sets is not hashable, but its sets
                // are handled below.
                if rollup.sets.iter().any(|set| set.len > 0) {
                    usable = false;
                    break;
                }
            }
            for &set in rollup.sets {
                if set.len == 0 {
                    empties.push(set).map_err(|_| arena_full())?;
                } else {
                    hashed
                        .push(hashed_rollup(set, arena)?)
                        .map_err(|_| arena_full())?;
                }
            }
        }
        if usable && !hashed.as_slice().is_empty() {
            let mut strategy = SetsStrategy::Hashed;
            if !empties.as_slice().is_empty() {
                let sets = arena
                    .alloc_slice_copy(empties.as_slice())
                    .map_err(|_| arena_full())?;
                hashed
                    .push(Rollup {
                        clause: GroupingSet::EMPTY,
                        sets,
                        groups: sets.len() as f64,
                        hashed: false,
                        hashable: false,
                    })
                    .map_err(|_| arena_full())?;
                strategy = SetsStrategy::Mixed;
            }
            let list = arena
                .alloc_slice_copy(hashed.as_slice())
                .map_err(|_| arena_full())?;
            let (plan, disabled) = grouping_sets_path(query, strategy, list, input, None);
            candidates
                .push(SetsPath {
                    plan,
                    disabled,
                    ordered: false,
                })
                .map_err(|_| arena_full())?;
        }
    }
    // Input sorted for the first rollup.
    if !rollups.is_empty() {
        let sorted_input = if rollups[0].clause.len > 0 {
            let (sort_startup, sort_run) =
                tuplesort_cost(input.rows, input.width, None, super::Costs::current());
            Some((
                input.total + sort_startup,
                input.total + sort_startup + sort_run,
            ))
        } else {
            None
        };
        let sorted = match sorted_input {
            Some((startup, total)) => AggInput {
                startup,
                total,
                ..input
            },
            None => input,
        };
        if any_hashable {
            let available = memory - table_size(query, 0.0);
            if available > 0.0 && rollups.len() > 1 {
                let scale = (available / (20.0 * rollups.len() as f64)).max(1.0);
                let capacity = (available / scale).floor() as usize;
                let mut weights = [0usize; 64];
                let mut items = 0;
                for rollup in &rollups[1..] {
                    if rollup.hashable {
                        if items == weights.len() {
                            return Err(crate::sql_err!(
                                crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "EXPLAIN of more than 64 hashable grouping set rollups is not supported"
                            ));
                        }
                        let size = table_size(query, rollup.groups);
                        weights[items] = (size / scale).floor().min(capacity as f64 + 1.0) as usize;
                        items += 1;
                    }
                }
                let chosen = if items > 0 {
                    knapsack(capacity, &weights[..items], arena)?
                } else {
                    0
                };
                if chosen != 0 {
                    let mut kept = crate::mem::arena::ArenaList::new(arena);
                    let mut hash_sets = crate::mem::arena::ArenaList::new(arena);
                    kept.push(rollups[0]).map_err(|_| arena_full())?;
                    let mut item = 0;
                    for rollup in &rollups[1..] {
                        if rollup.hashable {
                            if chosen & (1 << item) != 0 {
                                for &set in rollup.sets {
                                    hash_sets.push(set).map_err(|_| arena_full())?;
                                }
                            } else {
                                kept.push(*rollup).map_err(|_| arena_full())?;
                            }
                            item += 1;
                        } else {
                            kept.push(*rollup).map_err(|_| arena_full())?;
                        }
                    }
                    // Each hashed set goes to the front, so they run in
                    // reverse order of the list.
                    let hash_sets = hash_sets.as_slice();
                    let list = arena
                        .alloc_slice_with(hash_sets.len() + kept.as_slice().len(), |_| rollups[0])
                        .map_err(|_| arena_full())?;
                    for (at, &set) in hash_sets.iter().rev().enumerate() {
                        list[at] = hashed_rollup(set, arena)?;
                    }
                    list[hash_sets.len()..].copy_from_slice(kept.as_slice());
                    let (plan, disabled) =
                        grouping_sets_path(query, SetsStrategy::Mixed, list, sorted, sorted_input);
                    candidates
                        .push(SetsPath {
                            plan,
                            disabled,
                            ordered: false,
                        })
                        .map_err(|_| arena_full())?;
                }
            }
        }
        let (plan, disabled) =
            grouping_sets_path(query, SetsStrategy::Sorted, rollups, sorted, sorted_input);
        // A sorted path of one rollup returns its rows in its order.
        let ordered = plan.strategy == SetsStrategy::Sorted && rollups.len() == 1;
        candidates
            .push(SetsPath {
                plan,
                disabled,
                ordered,
            })
            .map_err(|_| arena_full())?;
    }
    let candidates = candidates.as_slice();
    // `add_path` then `create_ordered_paths`.
    let ordering_matters = !query.order.is_empty();
    let satisfied = |path: &SetsPath<'_>| {
        !ordering_matters
            || (path.ordered
                && query.order.iter().enumerate().all(|(at, key)| {
                    key.is_some_and(|key| {
                        path.plan.rollups[0].clause.columns().get(at) == Some(&key)
                    })
                }))
    };
    let finish = |path: &SetsPath<'a>| {
        let mut path = *path;
        if !satisfied(&path) {
            let (sort_startup, sort_run) = tuplesort_cost(
                path.plan.rows,
                query.output_width,
                None,
                super::Costs::current(),
            );
            path.plan.output_sort = Some((
                path.plan.total + sort_startup,
                path.plan.total + sort_startup + sort_run,
            ));
            path.disabled += u32::from(!methods.sort);
        }
        path
    };
    let cost = |path: &SetsPath<'_>| match path.plan.output_sort {
        Some((startup, total)) => (startup, total),
        None => (path.plan.startup, path.plan.total),
    };
    let cheapest = candidates.iter().copied().reduce(|best, path| {
        let order = path
            .disabled
            .cmp(&best.disabled)
            .then(path.plan.total.total_cmp(&best.plan.total))
            .then(path.plan.startup.total_cmp(&best.plan.startup));
        if order == Ordering::Less { path } else { best }
    });
    let best = candidates
        .iter()
        .filter(|path| {
            satisfied(path)
                || cheapest.is_some_and(|cheapest| {
                    cheapest.plan.total == path.plan.total
                        && cheapest.plan.startup == path.plan.startup
                        && cheapest.disabled == path.disabled
                })
        })
        .map(finish)
        .reduce(|best, path| {
            let order = path
                .disabled
                .cmp(&best.disabled)
                .then(cost(&path).1.total_cmp(&cost(&best).1))
                .then(cost(&path).0.total_cmp(&cost(&best).0));
            if order == Ordering::Less { path } else { best }
        })
        .ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "grouping sets have no path"
            )
        })?;
    Ok(best.plan)
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;
    use crate::mem::budget::Budget;

    fn set(columns: &[u8]) -> GroupingSet {
        let mut set = GroupingSet::EMPTY;
        set.columns[..columns.len()].copy_from_slice(columns);
        set.len = columns.len();
        set
    }

    #[test]
    fn cube_chains_into_postgresql_rollups() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "grouping sets", 1 << 20).unwrap();
        // `CUBE (a, b)` sorted by length: (), (a), (b), (a, b).
        let sets = [set(&[]), set(&[0]), set(&[1]), set(&[0, 1])];
        let chains = extract_rollup_sets(&sets, &arena).unwrap();
        let shapes: Vec<Vec<Vec<u8>>> = chains
            .iter()
            .map(|chain| chain.iter().map(|set| set.columns().to_vec()).collect())
            .collect();
        assert_eq!(
            shapes,
            vec![vec![vec![], vec![0], vec![0, 1]], vec![vec![1]]]
        );
        let ordered = reorder_grouping_sets(chains[0], &[], &arena).unwrap();
        let ordered: Vec<Vec<u8>> = ordered.iter().map(|set| set.columns().to_vec()).collect();
        assert_eq!(ordered, vec![vec![0, 1], vec![0], vec![]]);
    }

    #[test]
    fn reordering_follows_the_order_by_then_the_sets() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "grouping sets", 1 << 20).unwrap();
        let chain = [set(&[]), set(&[1]), set(&[0, 1])];
        let ordered = reorder_grouping_sets(&chain, &[Some(1), Some(0)], &arena).unwrap();
        assert_eq!(ordered[0].columns(), &[1, 0]);
        let ordered = reorder_grouping_sets(&chain, &[Some(0)], &arena).unwrap();
        assert_eq!(ordered[0].columns(), &[1, 0]);
    }

    #[test]
    fn knapsack_keeps_the_most_items_that_fit() {
        let mut budget = Budget::new(1 << 20);
        let arena = Arena::new(&mut budget, "grouping sets", 1 << 20).unwrap();
        assert_eq!(knapsack(10, &[6, 5, 5], &arena).unwrap(), 0b110);
        assert_eq!(knapsack(4, &[6, 5], &arena).unwrap(), 0);
    }
}
