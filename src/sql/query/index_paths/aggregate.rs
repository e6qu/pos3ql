//! The costs of aggregation as PostgreSQL 18's planner charges them:
//! `get_agg_clause_costs` (each transition state's function and input
//! expressions per input row, each aggregate's final function once per
//! group), `cost_agg` for the plain, sorted, hashed and mixed strategies with
//! its hash aggregate spill model (`hash_agg_entry_size`,
//! `hash_agg_set_limits`, `hash_choose_num_partitions`).
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/prep/prepagg.c
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/costsize.c
//! https://github.com/postgres/postgres/blob/REL_18_6/src/backend/executor/nodeAgg.c

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::Expr;
use crate::sql::catalog::{TransitionSpace, builtin_aggregate_cost};
use crate::sql::eval::SqlError;
use crate::storage::Storage;

use super::super::{AggregateRoutine, aggregate_routine, collect_aggs, plan};
use super::Costs;

/// `AggClauseCosts`: what the aggregates of one query level cost.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct AggregateCosts {
    pub(crate) transition_startup: f64,
    pub(crate) transition_per_tuple: f64,
    pub(crate) final_startup: f64,
    pub(crate) final_per_tuple: f64,
    /// `transitionSpace`: the bytes the states add to a hash entry.
    pub(crate) transition_space: f64,
    /// The transition states (`list_length(root->aggtransinfos)`).
    pub(crate) transitions: usize,
    /// The bytes each group's by-reference transition states allocate when
    /// a hash aggregate starts the group, in state order.
    pub(crate) per_group: [u16; MAX_SIZED_STATES],
    pub(crate) per_group_count: usize,
    /// A state allocates memory per group the hash aggregate memory model
    /// does not reproduce.
    pub(crate) unsized_states: bool,
    /// An aggregate has `DISTINCT` or `ORDER BY` inputs
    /// (`numOrderedAggs`), which forbids hashing and which PostgreSQL may
    /// presort.
    pub(crate) ordered: bool,
}

use crate::sql::query::postgres_memory::MAX_SIZED_STATES;

/// The bytes a transition state of transition function `function` allocates
/// in the aggregate's memory when its group starts, as PostgreSQL 18.6 does,
/// given that `space` is what `get_agg_clause_costs` charges for it: a copy
/// of an array initial value (`'{0,0}'` is a 40-byte `int8[]`), or the
/// 48-byte `Int128AggState` of the integer `avg`, `sum(int8)` and variance
/// states. `None` for a state that allocates otherwise (text, numeric and
/// collecting states).
fn group_allocation(function: i32, space: f64) -> Option<u16> {
    match function {
        // int2_avg_accum, int4_avg_accum: `{0,0}` as `int8[]`.
        1962 | 1963 => Some(40),
        // float4_accum, float8_accum: `{0,0,0}` as `float8[]`.
        208 | 222 => Some(48),
        // float8_regr_accum: six `float8` elements.
        2806 => Some(72),
        // int8_avg_accum, int2_accum, int4_accum.
        2746 | 1834 | 1835 => Some(48),
        // Collecting states of no planned space.
        2333 | 3173 | 3180 | 3265 | 3268 | 3535 | 3543 | 4051 | 4299 | 6225 | 6275 | 6277
        | 6278 | 6279 | 6283 | 6285 | 6286 | 6287 => None,
        _ if space == 0.0 => Some(0),
        _ => None,
    }
}

/// `AggStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggStrategy {
    Plain,
    Sorted,
    Hashed,
    /// Sorted input feeding hash tables too (`AGG_MIXED`).
    Mixed,
}

/// One transition state, which aggregates of the same inputs share.
#[derive(Clone, Copy)]
struct Transition<'a> {
    function: i32,
    initial: Option<&'static str>,
    call: &'a Expr<'a>,
}

/// The inputs of two aggregate calls are the same (`find_compatible_agg`'s
/// test for sharing a transition state): their arguments, `FILTER`,
/// `DISTINCT`, `ORDER BY` and `*`.
fn same_inputs(left: &Expr<'_>, right: &Expr<'_>) -> bool {
    match (left, right) {
        (
            Expr::Call {
                args: left_args,
                filter: left_filter,
                distinct: left_distinct,
                order_by: left_order,
                star: left_star,
                variadic: left_variadic,
                ..
            },
            Expr::Call {
                args: right_args,
                filter: right_filter,
                distinct: right_distinct,
                order_by: right_order,
                star: right_star,
                variadic: right_variadic,
                ..
            },
        ) => {
            left_args == right_args
                && left_filter == right_filter
                && left_distinct == right_distinct
                && left_order == right_order
                && left_star == right_star
                && left_variadic == right_variadic
        }
        _ => false,
    }
}

/// `get_agg_clause_costs` of the aggregates `expressions` call at this query
/// level, with their columns typed by `columns`; `None` when one of them is
/// outside the model: a grammar-spelled or user-defined aggregate, or a
/// state whose size depends on a type the model has no layout for.
pub(crate) fn aggregate_costs<'a>(
    expressions: &[&'a Expr<'a>],
    storage: &Storage,
    txid: u32,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    arena: &'a Arena,
) -> Result<Option<AggregateCosts>, SqlError> {
    let costs = Costs::current();
    let mut calls = ArenaList::new(arena);
    for expression in expressions {
        collect_aggs(expression, &mut calls, storage, txid, columns)?;
    }
    let mut result = AggregateCosts::default();
    let mut transitions = ArenaList::<Transition<'a>>::new(arena);
    let mut seen = ArenaList::<&'a Expr<'a>>::new(arena);
    let charge = |expression: &Expr<'_>| {
        plan::qual_cost(expression, columns, storage, txid, arena) / 2.0 * costs.cpu_operator
    };
    for &(_, call) in calls.as_slice() {
        // Equal calls are one aggregate (`find_compatible_agg`).
        if seen.as_slice().contains(&call) {
            continue;
        }
        seen.push(call)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let Expr::Call {
            args,
            filter,
            distinct,
            order_by,
            ..
        } = call
        else {
            return Ok(None);
        };
        result.ordered |= *distinct || !order_by.is_empty();
        let Some(AggregateRoutine::Builtin(oid)) = aggregate_routine(call, storage, txid, columns)?
        else {
            return Ok(None);
        };
        let Some(aggregate) = builtin_aggregate_cost(oid) else {
            return Ok(None);
        };
        result.final_per_tuple += aggregate.final_cost * costs.cpu_operator;
        if transitions.as_slice().iter().any(|transition| {
            transition.function == aggregate.transition
                && transition.initial == aggregate.initial
                && same_inputs(transition.call, call)
        }) {
            continue;
        }
        transitions
            .push(Transition {
                function: aggregate.transition,
                initial: aggregate.initial,
                call,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        result.transitions += 1;
        result.transition_per_tuple += aggregate.transition_cost * costs.cpu_operator;
        for argument in *args {
            result.transition_per_tuple += charge(argument);
        }
        if let Some(filter) = filter {
            result.transition_per_tuple += charge(filter);
        }
        let space = match aggregate.space {
            TransitionSpace::Fixed(bytes) => f64::from(bytes),
            TransitionSpace::Input => {
                let Some(argument) = args.first() else {
                    return Ok(None);
                };
                let Ok(oid) = crate::sql::exec::infer_routine_argument_oid(argument, columns)
                else {
                    return Ok(None);
                };
                match state_layout(storage, txid, oid) {
                    Some(bytes) => f64::from(bytes),
                    None => return Ok(None),
                }
            }
        };
        result.transition_space += space;
        match group_allocation(aggregate.transition, space) {
            Some(0) => {}
            Some(bytes) if result.per_group_count < MAX_SIZED_STATES => {
                result.per_group[result.per_group_count] = bytes;
                result.per_group_count += 1;
            }
            Some(_) | None => result.unsized_states = true,
        }
    }
    Ok(Some(result))
}

/// The bytes a polymorphic state of type `oid` adds: nothing passed by
/// value, else its `get_typavgwidth` (32 for a variable-length type) rounded
/// to `MAXALIGN` plus two pointers.
fn state_layout(storage: &Storage, txid: u32, oid: i32) -> Option<u32> {
    let (ctype, _) = crate::sql::exec::catalog_column_type(storage, txid, oid)?;
    let layout = ctype.physical_layout();
    let (by_value, length) = (layout.by_value, i32::from(layout.length));
    if by_value {
        return Some(0);
    }
    let width = if length > 0 { length as u32 } else { 32 };
    Some(width.next_multiple_of(8) + 16)
}

/// `hash_agg_entry_size`: the tuple-hash entry, the group's first tuple as
/// a minimal tuple, its per-state data and its transition values.
pub(crate) fn hash_agg_entry_size(
    transitions: usize,
    tuple_width: f64,
    transition_space: f64,
) -> f64 {
    /// `sizeof(TupleHashEntryData)`.
    const ENTRY: f64 = 16.0;
    /// `MAXALIGN(SizeofMinimalTupleHeader)`.
    const MINIMAL_HEADER: f64 = 16.0;
    /// `sizeof(AggStatePerGroupData)`.
    const PER_GROUP: f64 = 16.0;
    /// `CHUNKHDRSZ`.
    const CHUNK_HEADER: f64 = 8.0;
    // `Size` arithmetic truncates the double width.
    let tuple = ((MINIMAL_HEADER + tuple_width.trunc()) as u64).next_multiple_of(8) as f64;
    let per_group = transitions as f64 * PER_GROUP;
    let transition = if transition_space > 0.0 {
        CHUNK_HEADER + (transition_space as u64).next_power_of_two() as f64
    } else {
        0.0
    };
    ENTRY + tuple + per_group + transition
}

/// `get_hash_memory_limit`.
fn hash_memory_limit() -> f64 {
    crate::sql::guc::hash_memory_bytes()
}

/// `BLCKSZ`, the size of the buffers of a hash aggregate's spill files.
const BLOCK: f64 = 8192.0;

/// `hash_choose_num_partitions` with no hash bits used yet.
fn hash_choose_num_partitions(input_groups: f64, entry_size: f64) -> u32 {
    let memory = hash_memory_limit();
    let partition_limit = (memory * 0.25 - BLOCK) / BLOCK;
    let wanted = 1.5 * input_groups * entry_size;
    let mut partitions = 1.0 + wanted / memory;
    if partitions > partition_limit {
        partitions = partition_limit;
    }
    partitions = partitions.clamp(4.0, 1024.0);
    // `my_log2`, then a power of two.
    let count = partitions as u32;
    1 << count.next_power_of_two().trailing_zeros()
}

/// `hash_agg_set_limits`: the memory and group limits of the table and the
/// partitions it spills to (0 when it is not expected to spill).
fn hash_agg_set_limits(entry_size: f64, input_groups: f64) -> (f64, f64, u32) {
    let memory = hash_memory_limit();
    if input_groups * entry_size <= memory {
        return (memory, (memory / entry_size).trunc(), 0);
    }
    let partitions = hash_choose_num_partitions(input_groups, entry_size);
    let partition_memory = BLOCK + BLOCK * f64::from(partitions);
    let limit = if memory > 4.0 * partition_memory {
        memory - partition_memory
    } else {
        (memory * 0.75).trunc()
    };
    let groups = if limit > entry_size {
        (limit / entry_size).trunc()
    } else {
        1.0
    };
    (limit, groups, partitions)
}

/// `relation_byte_size`: the rows as heap tuples of `width` bytes.
fn relation_byte_size(tuples: f64, width: f64) -> f64 {
    const HEADER: f64 = 24.0;
    tuples * (((width as u64).next_multiple_of(8)) as f64 + HEADER)
}

/// The input `cost_agg` aggregates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AggInput {
    pub(crate) startup: f64,
    pub(crate) total: f64,
    pub(crate) rows: f64,
    pub(crate) width: f64,
}

/// The partitions `hash_agg_set_limits` plans for a hash table of `groups`
/// groups, which EXPLAIN reports when there are any.
pub(crate) fn planned_partitions(aggregates: &AggregateCosts, groups: f64, width: f64) -> u32 {
    let entry_size =
        hash_agg_entry_size(aggregates.transitions, width, aggregates.transition_space);
    hash_agg_set_limits(entry_size, groups).2
}

/// What sizes the memory of a hash aggregate of `groups` estimated groups
/// over `columns` grouping columns, reading rows of `width`; `None` when a
/// transition state's memory is outside the model.
pub(crate) fn hash_agg_sizing(
    aggregates: &AggregateCosts,
    groups: f64,
    width: f64,
    columns: usize,
) -> Option<crate::sql::query::postgres_memory::HashAggSizing> {
    if aggregates.unsized_states
        || columns == 0
        || columns > crate::sql::query::postgres_memory::HashAggSizing::MAX_COLUMNS
    {
        return None;
    }
    let entry_size =
        hash_agg_entry_size(aggregates.transitions, width, aggregates.transition_space);
    let (memory, _, _) = hash_agg_set_limits(entry_size, groups);
    Some(crate::sql::query::postgres_memory::HashAggSizing {
        buckets: crate::sql::query::postgres_memory::HashAggSizing::buckets(
            entry_size,
            groups,
            memory,
            aggregates.transitions,
        ),
        columns,
        transitions: aggregates.transitions,
        per_group: aggregates.per_group,
        per_group_count: aggregates.per_group_count,
    })
}

/// The buckets `build_hash_table` asks of a hash aggregate of `groups`
/// estimated groups over rows `width` bytes wide (`hash_choose_num_buckets`).
pub(crate) fn hash_agg_buckets(aggregates: &AggregateCosts, groups: f64, width: f64) -> usize {
    let entry_size =
        hash_agg_entry_size(aggregates.transitions, width, aggregates.transition_space);
    let (memory, _, _) = hash_agg_set_limits(entry_size, groups);
    crate::sql::query::postgres_memory::HashAggSizing::buckets(
        entry_size,
        groups,
        memory,
        aggregates.transitions,
    )
}

/// The buckets `build_hash_table` asks of a hash aggregate without
/// transition states over `groups` estimated groups of rows `width` bytes
/// wide (`hash_choose_num_buckets`).
pub(crate) fn plain_hash_buckets(width: f64, groups: f64) -> usize {
    let entry_size = hash_agg_entry_size(0, width, 0.0);
    let (memory, _, _) = hash_agg_set_limits(entry_size, groups);
    crate::sql::query::postgres_memory::HashAggSizing::buckets(entry_size, groups, memory, 0)
}

/// The cost and selectivity of a grouping's `HAVING` qualification
/// (`cost_qual_eval`, `clauselist_selectivity`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct QualCosts {
    pub(crate) startup: f64,
    pub(crate) per_tuple: f64,
    pub(crate) selectivity: f64,
}

/// The cost and selectivity PostgreSQL's planner gives a `HAVING`
/// condition that reads aggregates (`cost_qual_eval`, which costs an
/// aggregate as a column, and `clauselist_selectivity`, whose operators find
/// no statistics for an aggregate: `DEFAULT_EQ_SEL` for `=`, its complement
/// for `<>`, `DEFAULT_INEQ_SEL` for an inequality). `None` for a condition
/// outside that model, or one without an aggregate (which PostgreSQL moves to
/// `WHERE`).
pub(crate) fn having_quals(
    having: &Expr<'_>,
    storage: &Storage,
    txid: u32,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    arena: &Arena,
) -> Result<Option<QualCosts>, SqlError> {
    use crate::sql::ast::BinaryOp;
    const DEFAULT_EQ_SEL: f64 = 0.005;
    const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;
    fn selectivity(condition: &Expr<'_>) -> Option<f64> {
        match condition {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
                ..
            } => Some(selectivity(left)? * selectivity(right)?),
            Expr::Binary {
                operator: BinaryOp::Or,
                left,
                right,
                ..
            } => {
                let (left, right) = (selectivity(left)?, selectivity(right)?);
                Some(left + right - left * right)
            }
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } if (left.is_aggregate_use() && right.is_constant(false))
                || (right.is_aggregate_use() && left.is_constant(false)) =>
            {
                match operator {
                    BinaryOp::Eq => Some(DEFAULT_EQ_SEL),
                    BinaryOp::NotEq => Some(1.0 - DEFAULT_EQ_SEL),
                    BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
                        Some(DEFAULT_INEQ_SEL)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
    if having.contains_subquery() {
        return Ok(None);
    }
    let Some(selectivity) = selectivity(having) else {
        return Ok(None);
    };
    Ok(Some(QualCosts {
        startup: 0.0,
        per_tuple: crate::sql::query::qual_cost(having, columns, storage, txid, arena) / 2.0
            * crate::sql::guc::planner_value("cpu_operator_cost"),
        selectivity,
    }))
}

/// `cost_agg`'s accounting of its `HAVING` quals over the costs and rows it
/// computed without them: their startup cost, their per-row cost on every
/// group, and the groups they keep (`clamp_row_est`).
pub(crate) fn with_having(
    (startup, total, rows): (f64, f64, f64),
    quals: Option<QualCosts>,
) -> (f64, f64, f64) {
    let Some(quals) = quals else {
        return (startup, total, rows);
    };
    (
        startup + quals.startup,
        total + quals.startup + rows * quals.per_tuple,
        super::clamp_row_est(rows * quals.selectivity),
    )
}

/// `cost_agg`'s startup and total costs and its output rows, before any
/// `HAVING` qualification and the target list's own cost.
pub(crate) fn cost_agg(
    strategy: AggStrategy,
    aggregates: &AggregateCosts,
    group_columns: usize,
    groups: f64,
    input: AggInput,
) -> (f64, f64, f64) {
    let costs = Costs::current();
    let group_columns = group_columns as f64;
    let (mut startup, mut total, rows) = match strategy {
        AggStrategy::Plain => {
            let startup = input.total
                + aggregates.transition_startup
                + aggregates.transition_per_tuple * input.rows
                + aggregates.final_startup
                + aggregates.final_per_tuple;
            (startup, startup + costs.cpu_tuple, 1.0)
        }
        AggStrategy::Sorted | AggStrategy::Mixed => {
            let total = input.total
                + aggregates.transition_startup
                + aggregates.transition_per_tuple * input.rows
                + costs.cpu_operator * group_columns * input.rows
                + aggregates.final_startup
                + aggregates.final_per_tuple * groups
                + costs.cpu_tuple * groups;
            (input.startup, total, groups)
        }
        AggStrategy::Hashed => {
            let startup = input.total
                + aggregates.transition_startup
                + aggregates.transition_per_tuple * input.rows
                + costs.cpu_operator * group_columns * input.rows
                + aggregates.final_startup;
            let total = startup + aggregates.final_per_tuple * groups + costs.cpu_tuple * groups;
            (startup, total, groups)
        }
    };
    if matches!(strategy, AggStrategy::Hashed | AggStrategy::Mixed) {
        let entry_size = hash_agg_entry_size(
            aggregates.transitions,
            input.width,
            aggregates.transition_space,
        );
        let (memory_limit, groups_limit, partitions) = hash_agg_set_limits(entry_size, groups);
        let batches = ((groups * entry_size) / memory_limit)
            .max(groups / groups_limit)
            .ceil()
            .max(1.0);
        let partitions = f64::from(partitions.max(2));
        let depth = (batches.ln() / partitions.ln()).ceil();
        let pages = relation_byte_size(input.rows, input.width) / BLOCK;
        let pages_written = pages * depth * 2.0;
        let pages_read = pages * depth * 2.0;
        startup += pages_written * costs.random_page;
        total += pages_written * costs.random_page + pages_read * costs.seq_page;
        let spill = depth * input.rows * 2.0 * costs.cpu_tuple;
        startup += spill;
        total += spill;
    }
    (startup, total, rows)
}
