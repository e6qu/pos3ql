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
}

/// `AggStrategy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggStrategy {
    Plain,
    Sorted,
    Hashed,
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
/// outside the model: a grammar-spelled or user-defined aggregate, one with
/// `DISTINCT` or `ORDER BY` inputs (which PostgreSQL may presort), or a
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
        if *distinct || !order_by.is_empty() {
            return Ok(None);
        }
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
        result.transition_space += match aggregate.space {
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

/// `get_hash_memory_limit`: work_mem times the default
/// `hash_mem_multiplier` of 2, which pos3ql does not let a session change.
fn hash_memory_limit() -> f64 {
    f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0 * 2.0
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
        AggStrategy::Sorted => {
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
    if strategy == AggStrategy::Hashed {
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
