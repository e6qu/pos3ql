//! Storage-aware plan construction and EXPLAIN rendering.
//!
//! Plans use only catalog metadata, ANALYZE statistics, and cumulative cache
//! telemetry. Constructing a plan never fetches a durable block: observing a
//! query must not warm its own cache or add object-store requests.

use core::fmt::Write as _;

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql::ast::{
    BinaryOp, ExplainFormat, ExplainOptions, ExplainSerialize, Expr, JoinKind, Select, SelectItem,
    SetOp, SetQuery, SetTree, Stmt,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::query::{self, QueryScope};
use crate::sql::types::{ColDesc, Datum, oid};
use crate::sql_err;
use crate::storage::Storage;
use crate::store::BlockIoStats;
use crate::util::StackStr;

const MAX_PLAN_NODES: usize = 32;

#[derive(Clone, Copy)]
struct PlanNode {
    name: StackStr<96>,
    relation: StackStr<96>,
    output: StackStr<256>,
    depth: u8,
    startup_cost: f64,
    total_cost: f64,
    rows: u64,
    width: u32,
    object_requests: u64,
    cache_blocks: u64,
}

impl PlanNode {
    const EMPTY: Self = Self {
        name: StackStr::new(),
        relation: StackStr::new(),
        output: StackStr::new(),
        depth: 0,
        startup_cost: 0.0,
        total_cost: 0.0,
        rows: 0,
        width: 0,
        object_requests: 0,
        cache_blocks: 0,
    };
}

#[derive(Clone, Copy)]
pub(super) struct ExplainActual {
    pub(super) rows: u64,
    pub(super) elapsed_micros: u64,
    pub(super) io: BlockIoStats,
    pub(super) serialized_bytes: u64,
    pub(super) serialization_micros: u64,
    pub(super) wal_records: u64,
    pub(super) wal_bytes: u64,
}

pub(super) struct Plan {
    nodes: [PlanNode; MAX_PLAN_NODES],
    count: usize,
    planning_micros: u64,
}

impl Plan {
    fn new() -> Self {
        Self {
            nodes: [PlanNode::EMPTY; MAX_PLAN_NODES],
            count: 0,
            planning_micros: 0,
        }
    }

    fn push(&mut self, node: PlanNode) -> Result<(), SqlError> {
        if self.count == self.nodes.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN plan exceeds {} nodes",
                MAX_PLAN_NODES
            ));
        }
        self.nodes[self.count] = node;
        self.count += 1;
        Ok(())
    }
}

fn projected_shape<'a>(
    statement: &Select<'a>,
    scope: Option<&QueryScope<'a>>,
    storage: &Storage,
    txid: u32,
) -> Result<(u32, StackStr<256>), SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); crate::sql::exec::MAX_PROJ];
    let count = match scope {
        Some(scope) => {
            query::describe_scope_items(statement.items, scope, storage, txid, &mut columns)?
        }
        None => query::describe_catalog_items(statement.items, None, storage, txid, &mut columns)?,
    };
    let mut width = 0u32;
    let mut output = StackStr::new();
    for column in &columns[..count] {
        width = width.saturating_add(if column.typlen > 0 {
            column.typlen as u32
        } else {
            32
        });
        if !output.as_str().is_empty() {
            let _ = write!(output, ", ");
        }
        let _ = write!(output, "{}", column.name);
    }
    if output.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "EXPLAIN output column list exceeds 256 bytes"
        ));
    }
    Ok((width, output))
}

fn has_aggregate(statement: &Select<'_>) -> bool {
    !statement.group_by.is_empty()
        || statement.having.is_some()
        || statement.items.iter().any(|item| {
            matches!(
                item,
                SelectItem::Expr { expression, .. }
                    if query::expr_has_aggregate(expression)
            )
        })
}

fn has_window(statement: &Select<'_>) -> bool {
    statement.items.iter().any(|item| {
        matches!(
            item,
            SelectItem::Expr { expression, .. } if query::expr_has_window(expression)
        )
    }) || statement
        .order_by
        .iter()
        .any(|order| query::expr_has_window(order.expression))
}

fn cache_probabilities(stats: BlockIoStats) -> (f64, f64) {
    let ram_total = stats.ram_hits.saturating_add(stats.ram_misses);
    let ram = if ram_total == 0 {
        0.0
    } else {
        stats.ram_hits as f64 / ram_total as f64
    };
    let disk_total = stats.disk_hits.saturating_add(stats.disk_misses);
    let disk = if disk_total == 0 {
        0.0
    } else {
        (1.0 - ram) * stats.disk_hits as f64 / disk_total as f64
    };
    (ram, disk)
}

/// Estimated cost of one cold durable-block request in the plan's arbitrary
/// cost units. Until one response has completed, keep the documented bootstrap
/// calibration; afterwards use the provider-neutral observed mean latency.
/// A plan never performs I/O to obtain this value.
fn object_request_cost(stats: BlockIoStats) -> f64 {
    if stats.object_read_completions == 0 {
        return 4.0;
    }
    let mean_micros = stats.object_read_micros as f64 / stats.object_read_completions as f64;
    // Cost units are milliseconds. A sub-millisecond local object store must
    // still retain a non-zero request cost so it cannot erase CPU work or make
    // an unbounded number of requests look free.
    (mean_micros / 1_000.0).max(0.01)
}

fn predicate_column(
    expression: &Expr<'_>,
    scope: &QueryScope<'_>,
) -> Option<(usize, usize, BinaryOp)> {
    fn reverse(operator: BinaryOp) -> BinaryOp {
        match operator {
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::LtEq => BinaryOp::GtEq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::GtEq => BinaryOp::LtEq,
            other => other,
        }
    }
    match expression {
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
        } => predicate_column(left, scope).or_else(|| predicate_column(right, scope)),
        Expr::Binary {
            operator,
            left,
            right,
        } if matches!(
            operator,
            BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
        ) =>
        {
            let side = |column: &Expr<'_>,
                        constant: &Expr<'_>,
                        operator: BinaryOp|
             -> Option<(usize, usize, BinaryOp)> {
                let Expr::Column { qualifier, name } = column else {
                    return None;
                };
                if !constant.is_constant() || constant.contains_call() {
                    return None;
                }
                match scope.find_column(*qualifier, name).ok()? {
                    query::ResolvedColumn::Table(table, column) => Some((table, column, operator)),
                    query::ResolvedColumn::Merged(_) => None,
                }
            };
            side(left, right, *operator).or_else(|| side(right, left, reverse(*operator)))
        }
        _ => None,
    }
}

fn column_selectivity(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    column: usize,
    txid: u32,
    default: f64,
) -> f64 {
    if scope.derived[table].is_some() || scope.slots[table] == usize::MAX {
        return default;
    }
    let statistics = storage.table_statistics(scope.slots[table], txid);
    let column_statistics = statistics.columns[column];
    if !statistics.valid || !column_statistics.valid || statistics.rows == 0 {
        return default;
    }
    let non_null = 1.0 - f64::from(column_statistics.null_fraction_ppm) / 1_000_000.0;
    (non_null / column_statistics.distinct_values.max(1) as f64).clamp(0.0, 1.0)
}

/// Returns a joint equality estimate when an AND-conjunction supplies every
/// column of a collected composite key. This deliberately recognizes only
/// constant equality arms: joins, parameters, calls, and range predicates
/// retain the conservative general estimator below.
fn multi_column_selectivity(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    expression: &Expr<'_>,
    txid: u32,
) -> Option<f64> {
    fn collect(
        expression: &Expr<'_>,
        scope: &QueryScope<'_>,
        table: usize,
        columns: &mut [usize; crate::storage::MAX_INDEX_COLS],
        count: &mut usize,
    ) -> bool {
        match expression {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
            } => {
                collect(left, scope, table, columns, count)
                    && collect(right, scope, table, columns, count)
            }
            Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
            } => {
                let column = |candidate: &Expr<'_>, other: &Expr<'_>| {
                    let Expr::Column { qualifier, name } = candidate else {
                        return None;
                    };
                    if !other.is_constant() || other.contains_call() {
                        return None;
                    }
                    match scope.find_column(*qualifier, name).ok()? {
                        query::ResolvedColumn::Table(owner, column) if owner == table => {
                            Some(column)
                        }
                        _ => None,
                    }
                };
                let Some(column) = column(left, right).or_else(|| column(right, left)) else {
                    return false;
                };
                if *count == columns.len() || columns[..*count].contains(&column) {
                    return false;
                }
                columns[*count] = column;
                *count += 1;
                true
            }
            _ => false,
        }
    }

    if scope.derived[table].is_some() || scope.slots[table] == usize::MAX {
        return None;
    }
    let mut columns = [0usize; crate::storage::MAX_INDEX_COLS];
    let mut count = 0usize;
    if !collect(expression, scope, table, &mut columns, &mut count) || count < 2 {
        return None;
    }
    let statistics = storage.table_statistics(scope.slots[table], txid);
    if !statistics.valid || statistics.rows == 0 {
        return None;
    }
    let multi = statistics
        .multi_columns
        .iter()
        .find(|statistics| statistics.covers(&columns[..count]))?;
    Some(
        (multi.non_null_rows as f64 / statistics.rows as f64 / multi.distinct_values.max(1) as f64)
            .clamp(0.0, 1.0),
    )
}

fn predicate_selectivity(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    expression: &Expr<'_>,
    txid: u32,
) -> f64 {
    match expression {
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
        } => {
            if let Some(selectivity) =
                multi_column_selectivity(storage, scope, table, expression, txid)
            {
                return selectivity;
            }
            let left = predicate_selectivity(storage, scope, table, left, txid);
            let right = predicate_selectivity(storage, scope, table, right, txid);
            (left * right).clamp(0.0, 1.0)
        }
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
        } => {
            let left = predicate_selectivity(storage, scope, table, left, txid);
            let right = predicate_selectivity(storage, scope, table, right, txid);
            (left + right - left * right).clamp(0.0, 1.0)
        }
        Expr::Binary {
            operator:
                operator
                @ (BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq),
            ..
        } => {
            let Some((owner, column, _)) = predicate_column(expression, scope) else {
                return 1.0;
            };
            if owner != table {
                return 1.0;
            }
            if *operator == BinaryOp::Eq {
                column_selectivity(storage, scope, table, column, txid, 0.005)
            } else {
                let equality = column_selectivity(storage, scope, table, column, txid, 0.005);
                (1.0 - equality).max(0.0) / 3.0
            }
        }
        Expr::IsNull { operand, negated } => {
            let Expr::Column { qualifier, name } = **operand else {
                return 1.0;
            };
            let Ok(query::ResolvedColumn::Table(owner, column)) =
                scope.find_column(qualifier, name)
            else {
                return 1.0;
            };
            if owner != table || scope.derived[table].is_some() {
                return 1.0;
            }
            let statistics = storage.table_statistics(scope.slots[table], txid);
            let column_statistics = statistics.columns[column];
            if !statistics.valid || !column_statistics.valid {
                return if *negated { 0.995 } else { 0.005 };
            }
            let fraction = f64::from(column_statistics.null_fraction_ppm) / 1_000_000.0;
            if *negated { 1.0 - fraction } else { fraction }
        }
        Expr::Between { operand, .. } => {
            let Expr::Column { qualifier, name } = **operand else {
                return 1.0;
            };
            match scope.find_column(qualifier, name) {
                Ok(query::ResolvedColumn::Table(owner, _)) if owner == table => 0.005,
                _ => 1.0,
            }
        }
        Expr::InList {
            operand,
            list,
            negated: false,
        } if list.iter().all(|item| item.is_constant()) => {
            let Expr::Column { qualifier, name } = **operand else {
                return 1.0;
            };
            let Ok(query::ResolvedColumn::Table(owner, column)) =
                scope.find_column(qualifier, name)
            else {
                return 1.0;
            };
            if owner != table {
                return 1.0;
            }
            (column_selectivity(storage, scope, table, column, txid, 0.005) * list.len() as f64)
                .min(1.0)
        }
        Expr::Like { operand, .. } | Expr::Match { operand, .. } => {
            let Expr::Column { qualifier, name } = **operand else {
                return 1.0;
            };
            match scope.find_column(qualifier, name) {
                Ok(query::ResolvedColumn::Table(owner, _)) if owner == table => 0.005,
                _ => 1.0,
            }
        }
        _ => 1.0,
    }
}

fn index_name(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    predicate: Option<&Expr<'_>>,
    txid: u32,
) -> Option<StackStr<96>> {
    let (owner, column, operator) = predicate_column(predicate?, scope)?;
    if owner != table || scope.derived[table].is_some() {
        return None;
    }
    let slot = scope.slots[table];
    let columns = [column as u16];
    let complete = if operator == BinaryOp::Eq {
        storage.value_probe_complete(slot, &columns)
    } else {
        storage.value_durable_complete(slot, &columns)
    };
    if !complete {
        return None;
    }
    let definition = scope.defs[table]?;
    let mut name = StackStr::new();
    if definition.columns()[column].primary {
        let _ = write!(name, "{}_pkey", definition.name.as_str());
    } else if definition.columns()[column].unique {
        let _ = write!(
            name,
            "{}_{}_key",
            definition.name.as_str(),
            definition.columns()[column].name.as_str()
        );
    } else {
        let index = storage
            .indexes_for(definition.schema.as_str(), definition.name.as_str(), txid)
            .find(|index| index.n_cols == 1 && index.columns[0] as usize == column)?;
        let _ = write!(name, "{}", index.name.as_str());
    }
    Some(name)
}

fn scan_node(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    predicate: Option<&Expr<'_>>,
    txid: u32,
    depth: u8,
) -> PlanNode {
    let slot = scope.slots[table];
    let derived = scope.derived[table].is_some() || slot == usize::MAX;
    let rows = if derived {
        scope.derived[table].map_or(1_000, |rows| rows.len() as u64)
    } else {
        storage.planning_row_estimate(slot)
    };
    let selectivity = predicate.map_or(1.0, |predicate| {
        predicate_selectivity(storage, scope, table, predicate, txid)
    });
    let output_rows = if rows == 0 {
        0
    } else {
        (rows as f64 * selectivity).ceil().max(1.0) as u64
    };
    let statistics = (!derived).then(|| storage.table_statistics(slot, txid));
    let width = statistics
        .filter(|statistics| statistics.valid)
        .map_or(32, |statistics| statistics.average_row_width.max(1));
    let generations = if derived {
        0
    } else {
        storage.spill_generation_count(slot) as u64
    };
    let full_scan_blocks = if generations == 0 {
        0
    } else {
        rows.saturating_mul(u64::from(width))
            .div_ceil(crate::store::MAX_PAYLOAD as u64)
            .saturating_add(generations.saturating_mul(2))
    };
    let index = index_name(storage, scope, table, predicate, txid);
    let use_index = index.is_some()
        && generations != 0
        && !storage.sequential_spill_scan_is_cheaper(slot, output_rows, txid);
    let blocks = if use_index {
        // One bounded key-generation descent per immutable generation, then
        // only the row blocks expected to survive the predicate.
        generations.saturating_mul(3).saturating_add(
            output_rows
                .saturating_mul(u64::from(width))
                .div_ceil(crate::store::MAX_PAYLOAD as u64),
        )
    } else {
        full_scan_blocks
    };
    let (ram_probability, disk_probability) = cache_probabilities(storage.block_io_stats());
    let cache_probability = (ram_probability + disk_probability).min(1.0);
    let cache_blocks = (blocks as f64 * cache_probability).round() as u64;
    let object_requests = blocks.saturating_sub(cache_blocks);
    // Object misses dominate; disk and RAM hits retain smaller but non-zero
    // costs so two equally selective plans prefer the warmer access path.
    let io_cost = object_requests as f64 * object_request_cost(storage.block_io_stats())
        + (blocks.saturating_sub(object_requests) as f64)
            * (ram_probability * 0.01 + disk_probability * 0.1);
    let cpu_rows = if use_index { output_rows } else { rows };
    let cpu_cost = cpu_rows as f64 * 0.01;
    let mut relation = StackStr::new();
    let _ = write!(relation, "{}", scope.names[table]);
    let name = if let Some(index) = index.filter(|_| use_index) {
        let mut name = StackStr::new();
        let _ = write!(name, "Index Scan using {}", index.as_str());
        name
    } else {
        StackStr::from_str(if derived { "Subquery Scan" } else { "Seq Scan" })
    };
    PlanNode {
        name,
        relation,
        output: StackStr::new(),
        depth,
        startup_cost: 0.0,
        total_cost: io_cost + cpu_cost,
        rows: output_rows,
        width,
        object_requests,
        cache_blocks,
    }
}

pub(super) fn plan_select(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    arena: &Arena,
) -> Result<Plan, SqlError> {
    let started = std::time::Instant::now();
    let mut plan = Plan::new();
    let scope = match &statement.from {
        Some(from) => Some(QueryScope::resolve_schema(storage, from, txid, arena)?),
        None => None,
    };
    let (width, output) = projected_shape(statement, scope.as_ref(), storage, txid)?;

    let mut estimated_rows = 1u64;
    let mut total_cost = 0.01f64;
    let mut object_requests = 0u64;
    let mut cache_blocks = 0u64;
    let mut table_order: [usize; query::MAX_JOIN_TABLES] = core::array::from_fn(|index| index);
    if let Some(scope) = &scope {
        let reorderable = statement.from.as_ref().is_some_and(|from| {
            from.joins
                .iter()
                .all(|join| matches!(join.kind, JoinKind::Cross))
                && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
        });
        if reorderable {
            table_order = query::join_order(storage, scope, statement.where_clause);
        }
        estimated_rows = 1;
        total_cost = 0.0;
        for &table in &table_order[..scope.n] {
            let scan = scan_node(storage, scope, table, statement.where_clause, txid, 1);
            estimated_rows = estimated_rows.saturating_mul(scan.rows.max(1));
            total_cost += scan.total_cost;
            object_requests = object_requests.saturating_add(scan.object_requests);
            cache_blocks = cache_blocks.saturating_add(scan.cache_blocks);
        }
    }

    let mut stages = [PlanNode::EMPTY; 8];
    let mut stage_count = 0usize;
    fn add_stage(
        stages: &mut [PlanNode; 8],
        count: &mut usize,
        stage: PlanNode,
    ) -> Result<(), SqlError> {
        if *count == stages.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN operator stack exceeds {} nodes",
                stages.len()
            ));
        }
        stages[*count] = stage;
        *count += 1;
        Ok(())
    }

    if scope.as_ref().is_some_and(|scope| scope.n > 1) {
        total_cost += estimated_rows as f64 * 0.01;
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Nested Loop"),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }

    let aggregate = has_aggregate(statement);
    if aggregate {
        estimated_rows = if statement.group_by.is_empty() {
            1
        } else {
            (estimated_rows as f64).sqrt().ceil().max(1.0) as u64
        };
        total_cost += estimated_rows as f64 * 0.02;
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str(if statement.group_by.is_empty() {
                    "Aggregate"
                } else {
                    "HashAggregate"
                }),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    if has_window(statement) {
        total_cost += estimated_rows as f64 * 0.02;
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("WindowAgg"),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    if !statement.order_by.is_empty() {
        total_cost += estimated_rows as f64 * (estimated_rows.max(2) as f64).log2() * 0.0025;
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Sort"),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    if statement.distinct || !statement.distinct_on.is_empty() {
        estimated_rows = estimated_rows.div_ceil(2);
        total_cost += estimated_rows as f64 * 0.01;
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Unique"),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    if statement.limit.is_some() {
        estimated_rows = estimated_rows.min(100);
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Limit"),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }

    if scope.is_none() && stage_count == 0 {
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Result"),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    for (depth, stage) in stages[..stage_count].iter().rev().enumerate() {
        let mut stage = *stage;
        stage.depth = depth as u8;
        stage.output = output;
        plan.push(stage)?;
    }
    if let Some(scope) = &scope {
        for &table in &table_order[..scope.n] {
            let mut scan = scan_node(
                storage,
                scope,
                table,
                statement.where_clause,
                txid,
                stage_count as u8,
            );
            scan.output = output;
            plan.push(scan)?;
        }
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

fn relation_slot(storage: &Storage, txid: u32, schema: Option<&str>, name: &str) -> Option<usize> {
    match storage.resolve_relation(schema, name, txid) {
        Some(crate::storage::ResolvedRelation::Table(slot)) => Some(slot),
        _ => None,
    }
}

fn physical_scan_node(
    storage: &Storage,
    slot: usize,
    relation_name: &str,
    filtered: bool,
    txid: u32,
    depth: u8,
) -> PlanNode {
    let rows = storage.planning_row_estimate(slot);
    let output_rows = if filtered { rows.div_ceil(10) } else { rows };
    let statistics = storage.table_statistics(slot, txid);
    let width = if statistics.valid {
        statistics.average_row_width.max(1)
    } else {
        32
    };
    let generations = storage.spill_generation_count(slot) as u64;
    let blocks = if generations == 0 {
        0
    } else {
        rows.saturating_mul(u64::from(width))
            .div_ceil(crate::store::MAX_PAYLOAD as u64)
            .saturating_add(generations.saturating_mul(2))
    };
    let (ram_probability, disk_probability) = cache_probabilities(storage.block_io_stats());
    let cache_probability = (ram_probability + disk_probability).min(1.0);
    let cache_blocks = (blocks as f64 * cache_probability).round() as u64;
    let object_requests = blocks.saturating_sub(cache_blocks);
    let total_cost = object_requests as f64 * object_request_cost(storage.block_io_stats())
        + cache_blocks as f64 * (ram_probability * 0.01 + disk_probability * 0.1)
        + rows as f64 * 0.01;
    PlanNode {
        name: StackStr::from_str("Seq Scan"),
        relation: StackStr::from_str(relation_name),
        output: StackStr::new(),
        depth,
        startup_cost: 0.0,
        total_cost,
        rows: output_rows,
        width,
        object_requests,
        cache_blocks,
    }
}

fn push_set_tree(
    plan: &mut Plan,
    storage: &Storage,
    txid: u32,
    tree: &SetTree<'_>,
    arena: &Arena,
    depth: u8,
) -> Result<PlanNode, SqlError> {
    match tree {
        SetTree::Select(select) => {
            let leaf = plan_select(storage, txid, select, arena)?;
            let root = leaf.nodes[0];
            for node in &leaf.nodes[..leaf.count] {
                let mut node = *node;
                node.depth = node.depth.saturating_add(depth);
                plan.push(node)?;
            }
            Ok(root)
        }
        SetTree::Op {
            operator,
            all,
            left,
            right,
        } => {
            let at = plan.count;
            plan.push(PlanNode::EMPTY)?;
            let left = push_set_tree(plan, storage, txid, left, arena, depth.saturating_add(1))?;
            let right = push_set_tree(plan, storage, txid, right, arena, depth.saturating_add(1))?;
            let rows = match (operator, all) {
                (SetOp::Union, true) => left.rows.saturating_add(right.rows),
                (SetOp::Union, false) => left.rows.saturating_add(right.rows).div_ceil(2),
                (SetOp::Intersect, true) => left.rows.min(right.rows),
                (SetOp::Intersect, false) => left.rows.min(right.rows).div_ceil(2),
                (SetOp::Except, true) => left.rows,
                (SetOp::Except, false) => left.rows.div_ceil(2),
            };
            let name = match (operator, all) {
                (SetOp::Union, true) => "Append",
                (SetOp::Union, false) => "HashAggregate",
                (SetOp::Intersect, _) => "SetOp Intersect",
                (SetOp::Except, _) => "SetOp Except",
            };
            let node = PlanNode {
                name: StackStr::from_str(name),
                relation: StackStr::new(),
                output: left.output,
                depth,
                startup_cost: 0.0,
                total_cost: left.total_cost + right.total_cost + rows as f64 * 0.01,
                rows,
                width: left.width.max(right.width),
                object_requests: left.object_requests.saturating_add(right.object_requests),
                cache_blocks: left.cache_blocks.saturating_add(right.cache_blocks),
            };
            plan.nodes[at] = node;
            Ok(node)
        }
    }
}

pub(super) fn plan_set_query(
    storage: &Storage,
    txid: u32,
    query: &SetQuery<'_>,
    arena: &Arena,
) -> Result<Plan, SqlError> {
    let started = std::time::Instant::now();
    let mut plan = Plan::new();
    let wrapper_count =
        usize::from(!query.order_by.is_empty()) + usize::from(query.limit.is_some());
    for depth in 0..wrapper_count {
        plan.push(PlanNode {
            name: StackStr::from_str(if query.limit.is_some() && depth == 0 {
                "Limit"
            } else {
                "Sort"
            }),
            depth: depth as u8,
            ..PlanNode::EMPTY
        })?;
    }
    let root = push_set_tree(
        &mut plan,
        storage,
        txid,
        query.body,
        arena,
        wrapper_count as u8,
    )?;
    let mut child = root;
    for at in (0..wrapper_count).rev() {
        let mut node = plan.nodes[at];
        node.rows = if node.name.as_str() == "Limit" {
            child.rows.min(100)
        } else {
            child.rows
        };
        node.width = child.width;
        node.total_cost = child.total_cost
            + if node.name.as_str() == "Sort" {
                child.rows as f64 * (child.rows.max(2) as f64).log2() * 0.0025
            } else {
                0.0
            };
        node.object_requests = child.object_requests;
        node.cache_blocks = child.cache_blocks;
        node.output = child.output;
        plan.nodes[at] = node;
        child = node;
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

pub(super) fn plan_modification(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    arena: &Arena,
) -> Result<Plan, SqlError> {
    let started = std::time::Instant::now();
    let (verb, target, filtered, source) = match statement {
        Stmt::Insert(insert) => ("Insert", insert.table, false, insert.select),
        Stmt::Update(update) => ("Update", update.table, update.where_clause.is_some(), None),
        Stmt::Delete(delete) => ("Delete", delete.table, delete.where_clause.is_some(), None),
        Stmt::Merge(merge) => ("Merge", merge.target, true, None),
        _ => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN does not support this statement type"
            ));
        }
    };
    if storage
        .resolve_relation(target.schema, target.name, txid)
        .is_none()
    {
        return Err(sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "relation \"{}\" does not exist",
            target.name
        ));
    }
    let mut plan = Plan::new();
    let mut name = StackStr::new();
    let _ = write!(name, "{verb}");
    let target_node = PlanNode {
        name,
        relation: StackStr::from_str(target.name),
        output: StackStr::new(),
        depth: 0,
        startup_cost: 0.0,
        total_cost: 0.01,
        rows: 0,
        width: 0,
        object_requests: 0,
        cache_blocks: 0,
    };
    plan.push(target_node)?;
    let child = if let Some(select) = source {
        let source_plan = plan_select(storage, txid, select, arena)?;
        let root = source_plan.nodes[0];
        for source_node in &source_plan.nodes[..source_plan.count] {
            let mut source_node = *source_node;
            source_node.depth = source_node.depth.saturating_add(1);
            plan.push(source_node)?;
        }
        root
    } else if let Some(slot) = relation_slot(storage, txid, target.schema, target.name) {
        let child = if matches!(statement, Stmt::Insert(_)) {
            PlanNode {
                name: StackStr::from_str("Result"),
                depth: 1,
                rows: match statement {
                    Stmt::Insert(insert) => insert.rows.len() as u64,
                    _ => 1,
                },
                width: storage
                    .table_statistics(slot, txid)
                    .average_row_width
                    .max(1),
                total_cost: 0.01,
                ..PlanNode::EMPTY
            }
        } else {
            physical_scan_node(storage, slot, target.name, filtered, txid, 1)
        };
        plan.push(child)?;
        child
    } else {
        let child = PlanNode {
            name: StackStr::from_str("Subquery Scan"),
            relation: StackStr::from_str(target.name),
            depth: 1,
            rows: 1_000,
            width: 32,
            total_cost: 10.0,
            ..PlanNode::EMPTY
        };
        plan.push(child)?;
        child
    };
    plan.nodes[0].total_cost = child.total_cost;
    plan.nodes[0].object_requests = child.object_requests;
    plan.nodes[0].cache_blocks = child.cache_blocks;
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

fn text_line(
    node: &PlanNode,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    root: bool,
) -> StackStr<512> {
    let mut line = StackStr::new();
    for _ in 0..node.depth {
        let _ = write!(line, "  ");
    }
    if node.depth > 0 {
        let _ = write!(line, "->  ");
    }
    let _ = write!(line, "{}", node.name.as_str());
    if !node.relation.as_str().is_empty() {
        let _ = write!(line, " on {}", node.relation.as_str());
    }
    if options.costs {
        let _ = write!(
            line,
            "  (cost={:.2}..{:.2} rows={} width={})",
            node.startup_cost, node.total_cost, node.rows, node.width
        );
    }
    if let Some(actual) = actual.filter(|_| root) {
        if options.timing {
            let elapsed_ms = actual.elapsed_micros as f64 / 1_000.0;
            let _ = write!(
                line,
                " (actual time=0.000..{elapsed_ms:.3} rows={:.2} loops=1)",
                actual.rows as f64
            );
        } else {
            let _ = write!(line, " (actual rows={:.2} loops=1)", actual.rows as f64);
        }
    }
    line
}

fn emit_text(
    plan: &Plan,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    responder: &mut Responder,
) -> Result<(), WireFull> {
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
    for (index, node) in plan.nodes[..plan.count].iter().enumerate() {
        let line = text_line(node, options, actual, index == 0);
        responder.data_row(&[Datum::Text(line.as_str())])?;
        if options.verbose && !node.output.as_str().is_empty() {
            let mut output = StackStr::<512>::new();
            for _ in 0..=node.depth {
                let _ = write!(output, "  ");
            }
            let _ = write!(output, "Output: {}", node.output.as_str());
            responder.data_row(&[Datum::Text(output.as_str())])?;
        }
        if index == 0 && options.buffers {
            let actual = actual.expect("BUFFERS requires ANALYZE");
            let mut buffers = StackStr::<512>::new();
            let hits = actual.io.ram_hits.saturating_add(actual.io.disk_hits);
            let _ = write!(
                buffers,
                "  Buffers: shared hit={} read={}",
                hits, actual.io.object_gets
            );
            responder.data_row(&[Datum::Text(buffers.as_str())])?;
        }
        if index == 0
            && options.wal
            && let Some(actual) = actual
            && actual.wal_records > 0
        {
            let mut wal = StackStr::<512>::new();
            let _ = write!(
                wal,
                "  WAL: records={} fpi=0 bytes={}",
                actual.wal_records, actual.wal_bytes
            );
            responder.data_row(&[Datum::Text(wal.as_str())])?;
        }
    }
    if options.memory {
        responder.data_row(&[Datum::Text("Planning:")])?;
        let used = core::mem::size_of::<Plan>()
            .saturating_sub(
                (MAX_PLAN_NODES - plan.count).saturating_mul(core::mem::size_of::<PlanNode>()),
            )
            .div_ceil(1024);
        let allocated = core::mem::size_of::<Plan>().div_ceil(1024);
        let mut memory = StackStr::<512>::new();
        let _ = write!(
            memory,
            "  Memory: used={}kB  allocated={}kB",
            used, allocated
        );
        responder.data_row(&[Datum::Text(memory.as_str())])?;
    }
    if options.summary {
        let mut planning = StackStr::<512>::new();
        let _ = write!(
            planning,
            "Planning Time: {:.3} ms",
            plan.planning_micros as f64 / 1_000.0
        );
        responder.data_row(&[Datum::Text(planning.as_str())])?;
        if let Some(actual) = actual {
            if options.serialize != ExplainSerialize::None {
                let mut serialization = StackStr::<512>::new();
                let format = match options.serialize {
                    ExplainSerialize::None => unreachable!(),
                    ExplainSerialize::Text => "text",
                    ExplainSerialize::Binary => "binary",
                };
                let _ = write!(
                    serialization,
                    "Serialization: time={:.3} ms  output={}kB  format={}",
                    actual.serialization_micros as f64 / 1_000.0,
                    actual.serialized_bytes.div_ceil(1024),
                    format
                );
                responder.data_row(&[Datum::Text(serialization.as_str())])?;
            }
            let mut execution = StackStr::<512>::new();
            let _ = write!(
                execution,
                "Execution Time: {:.3} ms",
                actual.elapsed_micros as f64 / 1_000.0
            );
            responder.data_row(&[Datum::Text(execution.as_str())])?;
        }
    }
    responder.command_complete("EXPLAIN")
}

fn json_string(out: &mut StackStr<16_384>, value: &str) {
    let _ = write!(out, "\"");
    for character in value.chars() {
        match character {
            '"' => {
                let _ = write!(out, "\\\"");
            }
            '\\' => {
                let _ = write!(out, "\\\\");
            }
            '\n' => {
                let _ = write!(out, "\\n");
            }
            '\r' => {
                let _ = write!(out, "\\r");
            }
            '\t' => {
                let _ = write!(out, "\\t");
            }
            character if character.is_control() => {
                let _ = write!(out, "\\u{:04x}", character as u32);
            }
            character => {
                let _ = write!(out, "{character}");
            }
        }
    }
    let _ = write!(out, "\"");
}

fn render_json_node(
    plan: &Plan,
    at: usize,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    out: &mut StackStr<16_384>,
) -> usize {
    let node = &plan.nodes[at];
    let _ = write!(out, "{{\"Node Type\":");
    json_string(out, node.name.as_str());
    let _ = write!(out, ",\"Parallel Aware\":false,\"Async Capable\":false");
    if !node.relation.as_str().is_empty() {
        let _ = write!(out, ",\"Relation Name\":");
        json_string(out, node.relation.as_str());
    }
    if options.costs {
        let _ = write!(
            out,
            ",\"Startup Cost\":{:.2},\"Total Cost\":{:.2},\"Plan Rows\":{},\"Plan Width\":{}",
            node.startup_cost, node.total_cost, node.rows, node.width
        );
    }
    let _ = write!(out, ",\"Disabled\":false");
    if options.verbose && !node.output.as_str().is_empty() {
        let _ = write!(out, ",\"Output\":[");
        for (index, column) in node.output.as_str().split(", ").enumerate() {
            if index != 0 {
                let _ = write!(out, ",");
            }
            json_string(out, column);
        }
        let _ = write!(out, "]");
    }
    if at == 0
        && let Some(actual) = actual
    {
        if options.timing {
            let _ = write!(
                out,
                ",\"Actual Startup Time\":0.000,\"Actual Total Time\":{:.3}",
                actual.elapsed_micros as f64 / 1_000.0
            );
        }
        let _ = write!(
            out,
            ",\"Actual Rows\":{:.2},\"Actual Loops\":1",
            actual.rows as f64
        );
        if options.buffers {
            let _ = write!(
                out,
                ",\"Shared Hit Blocks\":{},\"Shared Read Blocks\":{}",
                actual.io.ram_hits.saturating_add(actual.io.disk_hits),
                actual.io.object_gets
            );
        }
        if options.wal {
            let _ = write!(
                out,
                ",\"WAL Records\":{},\"WAL FPI\":0,\"WAL Bytes\":{},\"WAL Buffers Full\":0",
                actual.wal_records, actual.wal_bytes
            );
        }
    }
    let mut next = at + 1;
    let mut first = true;
    while next < plan.count && plan.nodes[next].depth > node.depth {
        if plan.nodes[next].depth != node.depth + 1 {
            break;
        }
        if first {
            let _ = write!(out, ",\"Plans\":[");
            first = false;
        } else {
            let _ = write!(out, ",");
        }
        next = render_json_node(plan, next, options, None, out);
    }
    if !first {
        let _ = write!(out, "]");
    }
    let _ = write!(out, "}}");
    next
}

fn xml_text(out: &mut StackStr<16_384>, value: &str) {
    for character in value.chars() {
        match character {
            '&' => {
                let _ = write!(out, "&amp;");
            }
            '<' => {
                let _ = write!(out, "&lt;");
            }
            '>' => {
                let _ = write!(out, "&gt;");
            }
            '"' => {
                let _ = write!(out, "&quot;");
            }
            '\'' => {
                let _ = write!(out, "&apos;");
            }
            character => {
                let _ = write!(out, "{character}");
            }
        }
    }
}

fn render_document(
    plan: &Plan,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
) -> Result<StackStr<16_384>, SqlError> {
    let mut out = StackStr::new();
    match options.format {
        ExplainFormat::Text => unreachable!("text has a row-per-line renderer"),
        ExplainFormat::Json => {
            let _ = write!(out, "[{{\"Plan\":");
            render_json_node(plan, 0, options, actual, &mut out);
            if options.memory {
                let used = core::mem::size_of::<Plan>()
                    .saturating_sub(
                        (MAX_PLAN_NODES - plan.count)
                            .saturating_mul(core::mem::size_of::<PlanNode>()),
                    )
                    .div_ceil(1024);
                let allocated = core::mem::size_of::<Plan>().div_ceil(1024);
                let _ = write!(
                    out,
                    ",\"Planning\":{{\"Memory Used\":{},\"Memory Allocated\":{}}}",
                    used, allocated
                );
            }
            if options.summary {
                let _ = write!(
                    out,
                    ",\"Planning Time\":{:.3}",
                    plan.planning_micros as f64 / 1_000.0
                );
                if let Some(actual) = actual {
                    if options.serialize != ExplainSerialize::None {
                        let format = match options.serialize {
                            ExplainSerialize::None => unreachable!(),
                            ExplainSerialize::Text => "text",
                            ExplainSerialize::Binary => "binary",
                        };
                        let _ = write!(
                            out,
                            ",\"Serialization\":{{\"Output Volume\":{},\"Format\":\"{}\"}}",
                            actual.serialized_bytes.div_ceil(1024),
                            format
                        );
                    }
                    let _ = write!(
                        out,
                        ",\"Execution Time\":{:.3}",
                        actual.elapsed_micros as f64 / 1_000.0
                    );
                }
            }
            let _ = write!(out, "}}]");
        }
        ExplainFormat::Xml => {
            let _ = write!(
                out,
                "<explain xmlns=\"http://www.postgresql.org/2009/explain\"><Query><Plan>"
            );
            for node in &plan.nodes[..plan.count] {
                let _ = write!(out, "<Node><Node-Type>");
                xml_text(&mut out, node.name.as_str());
                let _ = write!(
                    out,
                    "</Node-Type><Parallel-Aware>false</Parallel-Aware><Async-Capable>false</Async-Capable>"
                );
                if !node.relation.as_str().is_empty() {
                    let _ = write!(out, "<Relation-Name>");
                    xml_text(&mut out, node.relation.as_str());
                    let _ = write!(out, "</Relation-Name>");
                }
                if options.costs {
                    let _ = write!(
                        out,
                        "<Startup-Cost>{:.2}</Startup-Cost><Total-Cost>{:.2}</Total-Cost><Plan-Rows>{}</Plan-Rows><Plan-Width>{}</Plan-Width>",
                        node.startup_cost, node.total_cost, node.rows, node.width
                    );
                }
                let _ = write!(out, "<Disabled>false</Disabled>");
                if options.verbose && !node.output.as_str().is_empty() {
                    let _ = write!(out, "<Output>");
                    for column in node.output.as_str().split(", ") {
                        let _ = write!(out, "<Item>");
                        xml_text(&mut out, column);
                        let _ = write!(out, "</Item>");
                    }
                    let _ = write!(out, "</Output>");
                }
                let _ = write!(out, "</Node>");
            }
            if options.memory {
                let used = core::mem::size_of::<Plan>()
                    .saturating_sub(
                        (MAX_PLAN_NODES - plan.count)
                            .saturating_mul(core::mem::size_of::<PlanNode>()),
                    )
                    .div_ceil(1024);
                let allocated = core::mem::size_of::<Plan>().div_ceil(1024);
                let _ = write!(
                    out,
                    "</Plan><Planning><Memory-Used>{}</Memory-Used><Memory-Allocated>{}</Memory-Allocated></Planning>",
                    used, allocated
                );
            } else if options.summary {
                let _ = write!(out, "</Plan>");
            }
            if options.summary {
                let _ = write!(
                    out,
                    "<Planning-Time>{:.3}</Planning-Time>",
                    plan.planning_micros as f64 / 1_000.0
                );
                if let Some(actual) = actual {
                    if options.serialize != ExplainSerialize::None {
                        let format = match options.serialize {
                            ExplainSerialize::None => unreachable!(),
                            ExplainSerialize::Text => "text",
                            ExplainSerialize::Binary => "binary",
                        };
                        let _ = write!(
                            out,
                            "<Serialization><Output-Volume>{}</Output-Volume><Format>{}</Format></Serialization>",
                            actual.serialized_bytes.div_ceil(1024),
                            format
                        );
                    }
                    let _ = write!(
                        out,
                        "<Execution-Time>{:.3}</Execution-Time>",
                        actual.elapsed_micros as f64 / 1_000.0
                    );
                }
            } else if !options.memory {
                let _ = write!(out, "</Plan>");
            }
            let _ = write!(out, "</Query></explain>");
        }
        ExplainFormat::Yaml => {
            let _ = writeln!(out, "- Plan:");
            for node in &plan.nodes[..plan.count] {
                for _ in 0..=node.depth {
                    let _ = write!(out, "  ");
                }
                let _ = write!(out, "- Node Type: ");
                json_string(&mut out, node.name.as_str());
                let _ = writeln!(out);
                for _ in 0..=node.depth {
                    let _ = write!(out, "  ");
                }
                let _ = writeln!(out, "  Parallel Aware: false");
                for _ in 0..=node.depth {
                    let _ = write!(out, "  ");
                }
                let _ = writeln!(out, "  Async Capable: false");
                if !node.relation.as_str().is_empty() {
                    for _ in 0..=node.depth {
                        let _ = write!(out, "  ");
                    }
                    let _ = write!(out, "  Relation Name: ");
                    json_string(&mut out, node.relation.as_str());
                    let _ = writeln!(out);
                }
                if options.verbose && !node.output.as_str().is_empty() {
                    for _ in 0..=node.depth {
                        let _ = write!(out, "  ");
                    }
                    let _ = writeln!(out, "  Output:");
                    for column in node.output.as_str().split(", ") {
                        for _ in 0..=node.depth {
                            let _ = write!(out, "  ");
                        }
                        let _ = write!(out, "    - ");
                        json_string(&mut out, column);
                        let _ = writeln!(out);
                    }
                }
                for _ in 0..=node.depth {
                    let _ = write!(out, "  ");
                }
                let _ = writeln!(out, "  Disabled: false");
            }
            if options.memory {
                let used = core::mem::size_of::<Plan>()
                    .saturating_sub(
                        (MAX_PLAN_NODES - plan.count)
                            .saturating_mul(core::mem::size_of::<PlanNode>()),
                    )
                    .div_ceil(1024);
                let allocated = core::mem::size_of::<Plan>().div_ceil(1024);
                let _ = write!(
                    out,
                    "  Planning:\n    Memory Used: {}\n    Memory Allocated: {}\n",
                    used, allocated
                );
            }
            if options.summary {
                let _ = writeln!(
                    out,
                    "  Planning Time: {:.3}",
                    plan.planning_micros as f64 / 1_000.0
                );
                if let Some(actual) = actual {
                    if options.serialize != ExplainSerialize::None {
                        let format = match options.serialize {
                            ExplainSerialize::None => unreachable!(),
                            ExplainSerialize::Text => "text",
                            ExplainSerialize::Binary => "binary",
                        };
                        let _ = write!(
                            out,
                            "  Serialization:\n    Output Volume: {}\n    Format: \"{}\"\n",
                            actual.serialized_bytes.div_ceil(1024),
                            format
                        );
                    }
                    let _ = writeln!(
                        out,
                        "  Execution Time: {:.3}",
                        actual.elapsed_micros as f64 / 1_000.0
                    );
                }
            }
        }
    }
    if out.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "EXPLAIN document exceeds 16384 bytes"
        ));
    }
    Ok(out)
}

pub(super) fn emit_plan(
    plan: &Plan,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    responder: &mut Responder,
) -> Result<Result<(), SqlError>, WireFull> {
    if options.format == ExplainFormat::Text {
        emit_text(plan, options, actual, responder)?;
        return Ok(Ok(()));
    }
    let document = match render_document(plan, options, actual) {
        Ok(document) => document,
        Err(error) => return Ok(Err(error)),
    };
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
    responder.data_row(&[Datum::Text(document.as_str())])?;
    responder.command_complete("EXPLAIN")?;
    Ok(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::object_request_cost;
    use crate::store::BlockIoStats;

    #[test]
    fn object_cost_bootstraps_then_uses_completed_read_latency() {
        assert_eq!(object_request_cost(BlockIoStats::default()), 4.0);
        assert_eq!(
            object_request_cost(BlockIoStats {
                object_read_completions: 4,
                object_read_micros: 12_000,
                ..BlockIoStats::default()
            }),
            3.0
        );
    }

    #[test]
    fn object_cost_never_makes_a_completed_request_free() {
        assert_eq!(
            object_request_cost(BlockIoStats {
                object_read_completions: 1,
                object_read_micros: 0,
                ..BlockIoStats::default()
            }),
            0.01
        );
    }
}
