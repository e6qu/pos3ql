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
    BinaryOp, ExplainFormat, ExplainOptions, ExplainSerialize, Expr, Select, SelectItem, SetOp,
    SetQuery, SetTree, Stmt,
};
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql::guc::PlannerSettings;
use crate::sql::query::{self, QueryScope};
use crate::sql::types::{ColDesc, Datum, oid};
use crate::sql_err;
use crate::stack_format;
use crate::storage::Storage;
use crate::store::BlockIoStats;
use crate::util::StackStr;

const MAX_EXPLAIN_STAGES: usize = 8;

// A SELECT can expose one heap and one bitmap node per source plus every
// operator stage. Composed set trees reserve the sum of their actual leaves,
// so independently accepted widths never form a new fixed product ceiling.
fn select_plan_capacity(statement: &Select<'_>) -> usize {
    statement.from.as_ref().map_or(MAX_EXPLAIN_STAGES, |from| {
        (from.joins.len() + 1) * 2 + MAX_EXPLAIN_STAGES
    })
}

fn set_tree_plan_capacity(tree: &SetTree<'_>) -> usize {
    match tree {
        SetTree::Select(select) => select_plan_capacity(select),
        SetTree::Op { left, right, .. } => {
            1 + set_tree_plan_capacity(left) + set_tree_plan_capacity(right)
        }
    }
}

#[derive(Clone, Copy)]
struct PlanNode {
    name: StackStr<96>,
    bitmap_index: StackStr<64>,
    relation: StackStr<96>,
    output: StackStr<256>,
    depth: u8,
    startup_cost: f64,
    total_cost: f64,
    rows: u64,
    width: u32,
    object_requests: u64,
    cache_blocks: u64,
    constant_false: bool,
    qual_label: QualLabel,
    qual: StackStr<512>,
}

/// The label PostgreSQL's EXPLAIN gives a node's qualification.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QualLabel {
    None,
    Filter,
    HashCond,
    JoinFilter,
}

impl QualLabel {
    const fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Filter => "Filter",
            Self::HashCond => "Hash Cond",
            Self::JoinFilter => "Join Filter",
        }
    }
}

impl PlanNode {
    /// This node with `qual` under `label`.
    fn with_qual(mut self, label: QualLabel, qual: Option<StackStr<512>>) -> Self {
        if let Some(qual) = qual {
            self.qual_label = label;
            self.qual = qual;
        }
        self
    }

    const EMPTY: Self = Self {
        name: StackStr::new(),
        bitmap_index: StackStr::new(),
        relation: StackStr::new(),
        output: StackStr::new(),
        depth: 0,
        startup_cost: 0.0,
        total_cost: 0.0,
        rows: 0,
        width: 0,
        object_requests: 0,
        cache_blocks: 0,
        constant_false: false,
        qual_label: QualLabel::None,
        qual: StackStr::new(),
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

pub(super) struct Plan<'a> {
    nodes: &'a mut [PlanNode],
    count: usize,
    planning_micros: u64,
}

impl<'a> Plan<'a> {
    fn new(arena: &'a Arena, capacity: usize) -> Result<Self, SqlError> {
        let nodes = arena
            .alloc_slice_with(capacity, |_| PlanNode::EMPTY)
            .map_err(|_| query::arena_full_pub())?;
        Ok(Self {
            nodes,
            count: 0,
            planning_micros: 0,
        })
    }

    fn push(&mut self, node: PlanNode) -> Result<(), SqlError> {
        if self.count == self.nodes.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN plan exceeds {} nodes",
                self.nodes.len()
            ));
        }
        self.nodes[self.count] = node;
        self.count += 1;
        Ok(())
    }

    fn memory_kib(&self) -> (usize, usize) {
        let header = core::mem::size_of::<Self>();
        let node = core::mem::size_of::<PlanNode>();
        (
            header
                .saturating_add(self.count.saturating_mul(node))
                .div_ceil(1024),
            header
                .saturating_add(self.nodes.len().saturating_mul(node))
                .div_ceil(1024),
        )
    }
}

/// The most conjuncts a condition's EXPLAIN qualification is split into.
const MAX_CONJUNCTS: usize = 32;

/// `condition`'s AND conjuncts, or None when there are more than
/// [`MAX_CONJUNCTS`].
fn split_conjuncts<'e>(
    condition: &'e Expr<'e>,
    out: &mut [&'e Expr<'e>; MAX_CONJUNCTS],
    count: &mut usize,
) -> Option<()> {
    match condition {
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
            ..
        } => {
            split_conjuncts(left, out, count)?;
            split_conjuncts(right, out, count)
        }
        _ => {
            *out.get_mut(*count)? = condition;
            *count += 1;
            Some(())
        }
    }
}

/// The relations (bit per relation) an expression reads, as `resolve` maps
/// each column reference. None when it reads anything else, such as a
/// subquery or a name `resolve` cannot place.
fn relation_mask(
    expression: &Expr<'_>,
    resolve: &dyn Fn(Option<&str>, &str) -> Option<usize>,
) -> Option<u64> {
    if expression.contains_subquery() {
        return None;
    }
    let mut mask = 0u64;
    let mut placed = true;
    expression.for_each_column_reference(&mut |qualifier, name| match resolve(qualifier, name) {
        Some(relation) if relation < 64 => mask |= 1 << relation,
        _ => placed = false,
    });
    placed.then_some(mask)
}

/// Whether PostgreSQL's planner turns `conjunct` into an equivalence class
/// (an equality between columns or between a column and a constant), whose
/// clauses it regenerates after the scan's other restrictions.
fn equivalence_clause(conjunct: &Expr<'_>) -> bool {
    let operand = |side: &Expr<'_>| {
        matches!(
            side,
            Expr::Column { .. }
                | Expr::Int(_)
                | Expr::Str(_)
                | Expr::Float(_)
                | Expr::NumericLit(_)
                | Expr::Bool(_)
        )
    };
    matches!(conjunct, Expr::Binary { operator: BinaryOp::Eq, left, right, .. }
        if operand(left) && operand(right)
            && (matches!(left, Expr::Column { .. }) || matches!(right, Expr::Column { .. })))
}

/// PostgreSQL's order of a scan's restrictions: the others as written, then
/// the equivalence-class equalities in the order they were written.
fn order_restrictions(conjuncts: &mut [&Expr<'_>]) {
    let mut ordered = 0;
    for index in 0..conjuncts.len() {
        if !equivalence_clause(conjuncts[index]) {
            conjuncts[ordered..=index].rotate_right(1);
            ordered += 1;
        }
    }
}

/// PostgreSQL's EXPLAIN text of `conjuncts` combined with AND: each
/// deparsed as `ruleutils.c` prints a qualification. None when one of them
/// is outside the deparser's coverage.
fn deparse_qual(
    context: &crate::sql::catalog::deparse::Context<'_>,
    conjuncts: &[&Expr<'_>],
    arena: &Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    if conjuncts.is_empty() {
        return Ok(None);
    }
    let mark = arena.mark();
    let result = (|| {
        let mut text = StackStr::<512>::new();
        if conjuncts.len() > 1 {
            let _ = write!(text, "(");
        }
        for (index, conjunct) in conjuncts.iter().enumerate() {
            let Some(part) = crate::sql::catalog::deparse::deparse_expression(
                context,
                conjunct,
                crate::sql::catalog::deparse::Destination::Condition,
                arena,
            )?
            else {
                return Ok(None);
            };
            if index != 0 {
                let _ = write!(text, " AND ");
            }
            let _ = write!(text, "{part}");
        }
        if conjuncts.len() > 1 {
            let _ = write!(text, ")");
        }
        Ok((!text.is_truncated()).then_some(text))
    })();
    // SAFETY: the text was copied out of the arena.
    unsafe { arena.rewind_to(mark) };
    result
}

/// A comparison of two relations' columns with the outer relation's side
/// first, as PostgreSQL prints a hash join's condition.
fn equijoin<'e>(
    conjunct: &'e Expr<'e>,
    resolve: &dyn Fn(Option<&str>, &str) -> Option<usize>,
    outer: usize,
    arena: &'e Arena,
) -> Result<Option<&'e Expr<'e>>, SqlError> {
    let Expr::Binary {
        operator: BinaryOp::Eq,
        left,
        right,
        source_position,
    } = conjunct
    else {
        return Ok(None);
    };
    let side = |expression: &Expr<'_>| match expression {
        Expr::Column { qualifier, name } => resolve(*qualifier, name),
        _ => None,
    };
    match (side(left), side(right)) {
        (Some(a), Some(b)) if a == outer && b != outer => Ok(Some(conjunct)),
        (Some(a), Some(b)) if b == outer && a != outer => {
            let swapped = arena
                .alloc(Expr::Binary {
                    source_position: *source_position,
                    operator: BinaryOp::Eq,
                    left: right,
                    right: left,
                })
                .map_err(|_| arena_full())?;
            Ok(Some(&*swapped))
        }
        _ => Ok(None),
    }
}

/// The qualifications PostgreSQL attaches to a join of two relations: each
/// relation's own conjuncts as its scan's filter, and the conjuncts reading
/// both as the join's hash condition (equalities, under a hash join) or join
/// filter. None when a conjunct cannot be placed.
struct JoinQuals {
    filters: [Option<StackStr<512>>; 2],
    hash_cond: Option<StackStr<512>>,
    join_filter: Option<StackStr<512>>,
}

fn two_relation_quals<'a>(
    storage: &Storage,
    txid: u32,
    relations: [(&'a str, &'a crate::storage::TableDef); 2],
    condition: Option<&'a Expr<'a>>,
    hash: bool,
    arena: &'a Arena,
) -> Result<Option<JoinQuals>, SqlError> {
    let mut quals = JoinQuals {
        filters: [None, None],
        hash_cond: None,
        join_filter: None,
    };
    let Some(condition) = condition else {
        return Ok(Some(quals));
    };
    let resolve = |qualifier: Option<&str>, name: &str| -> Option<usize> {
        let has = |table: &crate::storage::TableDef| {
            table
                .columns()
                .iter()
                .any(|column| column.name.as_str() == name)
        };
        match qualifier {
            Some(qualifier) => relations
                .iter()
                .position(|(exposed, table)| *exposed == qualifier && has(table)),
            None => match (has(relations[0].1), has(relations[1].1)) {
                (true, false) => Some(0),
                (false, true) => Some(1),
                _ => None,
            },
        }
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none() {
        return Ok(None);
    }
    let mut own: [[&Expr<'_>; MAX_CONJUNCTS]; 2] = [[&Expr::Null; MAX_CONJUNCTS]; 2];
    let mut own_count = [0usize; 2];
    let mut hash_conds = [&Expr::Null; MAX_CONJUNCTS];
    let mut hash_count = 0;
    let mut joins = [&Expr::Null; MAX_CONJUNCTS];
    let mut join_count = 0;
    // PostgreSQL's equivalence classes: a column equal to another
    // relation's column that a constant fixes is fixed by that constant too,
    // and the join equality is then implied rather than checked.
    let constant_of = |relation: usize, column: &Expr<'_>| -> Option<&'a Expr<'a>> {
        let Expr::Column { name, .. } = column else {
            return None;
        };
        conjuncts[..count].iter().find_map(|conjunct| {
            let Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
                ..
            } = conjunct
            else {
                return None;
            };
            let same = |side: &Expr<'_>| {
                matches!(side, Expr::Column { qualifier, name: other }
                    if other == name && resolve(*qualifier, other) == Some(relation))
            };
            let constant = |side: &'a Expr<'a>| {
                matches!(
                    side,
                    Expr::Int(_)
                        | Expr::Str(_)
                        | Expr::Float(_)
                        | Expr::NumericLit(_)
                        | Expr::Bool(_)
                )
                .then_some(side)
            };
            if same(left) {
                constant(right)
            } else if same(right) {
                constant(left)
            } else {
                None
            }
        })
    };
    for conjunct in &conjuncts[..count] {
        match relation_mask(conjunct, &resolve) {
            Some(1) => {
                own[0][own_count[0]] = conjunct;
                own_count[0] += 1;
            }
            Some(2) => {
                own[1][own_count[1]] = conjunct;
                own_count[1] += 1;
            }
            Some(3)
                if let Some(Expr::Binary {
                    operator: BinaryOp::Eq,
                    left,
                    right,
                    ..
                }) = equijoin(conjunct, &resolve, 0, arena)?
                    && let Some((fixed, constant)) = constant_of(1, right)
                        .map(|constant| (0usize, constant))
                        .or_else(|| constant_of(0, left).map(|constant| (1usize, constant))) =>
            {
                let column = if fixed == 0 { *left } else { *right };
                let derived = &*arena
                    .alloc(Expr::Binary {
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                        operator: BinaryOp::Eq,
                        left: column,
                        right: constant,
                    })
                    .map_err(|_| arena_full())?;
                if own_count[fixed] == MAX_CONJUNCTS {
                    return Ok(None);
                }
                own[fixed][own_count[fixed]] = derived;
                own_count[fixed] += 1;
            }
            Some(3) => match equijoin(conjunct, &resolve, 0, arena)? {
                Some(equality) if hash => {
                    hash_conds[hash_count] = equality;
                    hash_count += 1;
                }
                _ => {
                    joins[join_count] = conjunct;
                    join_count += 1;
                }
            },
            _ => return Ok(None),
        }
    }
    for (index, (exposed, table)) in relations.iter().enumerate() {
        order_restrictions(&mut own[index][..own_count[index]]);
        let joined = [crate::sql::catalog::deparse::Joined::new(exposed, table)];
        let context = crate::sql::catalog::deparse::Context {
            storage,
            txid,
            table: Some(table),
            parameters: &[],
            joined: &joined,
            pretty: false,
            qualify: false,
        };
        quals.filters[index] = deparse_qual(&context, &own[index][..own_count[index]], arena)?;
        if own_count[index] != 0 && quals.filters[index].is_none() {
            return Ok(None);
        }
    }
    let joined = [
        crate::sql::catalog::deparse::Joined::new(relations[0].0, relations[0].1),
        crate::sql::catalog::deparse::Joined::new(relations[1].0, relations[1].1),
    ];
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined: &joined,
        pretty: false,
        qualify: true,
    };
    quals.hash_cond = deparse_qual(&context, &hash_conds[..hash_count], arena)?;
    quals.join_filter = deparse_qual(&context, &joins[..join_count], arena)?;
    if (hash_count != 0 && quals.hash_cond.is_none())
        || (join_count != 0 && quals.join_filter.is_none())
    {
        return Ok(None);
    }
    Ok(Some(quals))
}

/// The filter of a scan of one relation reading `condition`, when every
/// conjunct reads only that relation.
fn single_relation_filter<'a>(
    storage: &Storage,
    txid: u32,
    exposed: &'a str,
    table: &'a crate::storage::TableDef,
    condition: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    let Some(condition) = condition else {
        return Ok(None);
    };
    let resolve = |qualifier: Option<&str>, name: &str| -> Option<usize> {
        (qualifier.is_none_or(|qualifier| qualifier == exposed)
            && table
                .columns()
                .iter()
                .any(|column| column.name.as_str() == name))
        .then_some(0)
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none()
        || conjuncts[..count]
            .iter()
            .any(|conjunct| relation_mask(conjunct, &resolve).is_none())
    {
        return Ok(None);
    }
    order_restrictions(&mut conjuncts[..count]);
    let joined = [crate::sql::catalog::deparse::Joined::new(exposed, table)];
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: Some(table),
        parameters: &[],
        joined: &joined,
        pretty: false,
        qualify: false,
    };
    deparse_qual(&context, &conjuncts[..count], arena)
}

fn push_scan(plan: &mut Plan<'_>, scan: PlanNode) -> Result<(), SqlError> {
    let bitmap_index = scan.bitmap_index;
    let bitmap_depth = scan.depth.saturating_add(1);
    let bitmap_rows = scan.rows;
    let bitmap_cost = scan.total_cost * 0.4;
    plan.push(scan)?;
    if !bitmap_index.as_str().is_empty() {
        let mut name = StackStr::new();
        let _ = write!(name, "Bitmap Index Scan using {}", bitmap_index.as_str());
        plan.push(PlanNode {
            name,
            depth: bitmap_depth,
            total_cost: bitmap_cost,
            rows: bitmap_rows,
            ..PlanNode::EMPTY
        })?;
    }
    Ok(())
}

fn projected_shape<'a>(
    statement: &Select<'a>,
    scope: Option<&QueryScope<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(u32, StackStr<256>), SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); crate::sql::exec::MAX_PROJ];
    let count = match scope {
        Some(scope) => query::describe_scope_items(
            statement.items,
            scope,
            None,
            storage,
            txid,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Sql,
        )?,
        None => query::describe_catalog_items(
            statement.items,
            None,
            storage,
            txid,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Sql,
        )?,
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

fn has_aggregate(
    statement: &Select<'_>,
    storage: &Storage,
    txid: u32,
    columns: &dyn crate::sql::exec::ColTypeResolver,
) -> Result<bool, SqlError> {
    if !statement.group_by.is_empty() || statement.having.is_some() {
        return Ok(true);
    }
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item
            && query::expr_has_aggregate(expression, storage, txid, columns)?
        {
            return Ok(true);
        }
    }
    Ok(false)
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
        // COLLATE changes comparison semantics, not the relation/attribute
        // identity of a simple predicate.  Keeping the wrapper transparent
        // here lets the cost model see the same indexed column the executor
        // will scan.
        Expr::Collate { operand, .. } => predicate_column(operand, scope),
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
            ..
        } => predicate_column(left, scope).or_else(|| predicate_column(right, scope)),
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } if matches!(
            operator,
            BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
        ) =>
        {
            let side = |column: &Expr<'_>,
                        constant: &Expr<'_>,
                        operator: BinaryOp|
             -> Option<(usize, usize, BinaryOp)> {
                let column = match column {
                    Expr::Collate { operand, .. } => operand,
                    expression => expression,
                };
                let Expr::Column { qualifier, name } = column else {
                    return None;
                };
                if !constant.is_constant(false) || constant.contains_call() {
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
    let definition = storage.table_def(scope.slots[table], txid);
    let Some(column_statistics) = definition
        .attributes
        .number(column)
        .and_then(|attribute| statistics.column(attribute))
    else {
        return default;
    };
    if !statistics.valid || statistics.rows == 0 {
        return default;
    }
    let non_null = 1.0 - f64::from(column_statistics.null_fraction_ppm) / 1_000_000.0;
    let distinct = crate::storage::column_distinct_estimate(
        &definition.columns()[column],
        column_statistics,
        statistics.rows,
        false,
    );
    (non_null / distinct.max(1.0)).clamp(0.0, 1.0)
}

fn expression_statistics_selectivity(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    expression: &Expr<'_>,
    txid: u32,
    arena: &Arena,
) -> Option<f64> {
    let Expr::Binary {
        operator: BinaryOp::Eq,
        left,
        right,
        ..
    } = expression
    else {
        return None;
    };
    let candidate = if right.is_constant(false) && !right.contains_call() {
        *left
    } else if left.is_constant(false) && !left.contains_call() {
        *right
    } else {
        return None;
    };
    if scope.derived[table].is_some() || scope.slots[table] == usize::MAX {
        return None;
    }
    for (slot, statistics) in storage.extended_statistics_for_table(scope.slots[table], txid) {
        let data = storage.extended_statistics_data(slot, txid);
        if !data.valid || data.rows == 0 {
            continue;
        }
        for (key, definition) in statistics.keys_for(txid).iter().enumerate() {
            let crate::storage::ExtendedStatisticsKey::Expression(source) = definition else {
                continue;
            };
            let parsed = crate::sql::parser::parse_expr(source.as_str(), arena).ok()?;
            if parsed != candidate || !data.expression_statistics[key].valid {
                continue;
            }
            let column = data.expression_statistics[key];
            let non_null = 1.0 - f64::from(column.null_fraction_ppm) / 1_000_000.0;
            return Some((non_null / column.distinct_values.max(1) as f64).clamp(0.0, 1.0));
        }
    }
    None
}

/// Consumes a named PostgreSQL extended-statistics object only when the
/// conjunction supplies every plain-column key with a constant equality.
fn extended_statistics_selectivity<'a>(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    expression: &Expr<'a>,
    txid: u32,
    arena: &Arena,
) -> Option<f64> {
    fn collect<'a>(
        expression: &Expr<'a>,
        scope: &QueryScope<'_>,
        table: usize,
        arena: &'a Arena,
        columns: &mut [usize; crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
        values: &mut [Datum<'a>; crate::storage::MAX_EXTENDED_STATISTICS_KEYS],
        count: &mut usize,
    ) -> bool {
        fn column_constant<'e>(
            candidate: &Expr<'e>,
            other: &'e Expr<'e>,
            scope: &QueryScope<'_>,
            table: usize,
        ) -> Option<(usize, &'e Expr<'e>)> {
            let Expr::Column { qualifier, name } = candidate else {
                return None;
            };
            if !other.is_constant(false) || other.contains_call() {
                return None;
            }
            match scope.find_column(*qualifier, name).ok()? {
                query::ResolvedColumn::Table(owner, column) if owner == table => {
                    Some((column, other))
                }
                _ => None,
            }
        }
        match expression {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
                ..
            } => {
                collect(left, scope, table, arena, columns, values, count)
                    && collect(right, scope, table, arena, columns, values, count)
            }
            Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
                ..
            } => {
                let Some((column, constant)) = column_constant(left, right, scope, table)
                    .or_else(|| column_constant(right, left, scope, table))
                else {
                    return false;
                };
                if *count == columns.len() || columns[..*count].contains(&column) {
                    return false;
                }
                let Ok(value) = crate::sql::eval::eval(
                    constant,
                    arena,
                    crate::sql::eval::NO_PARAMS,
                    &crate::sql::eval::NoColumns,
                ) else {
                    return false;
                };
                columns[*count] = column;
                values[*count] = value;
                *count += 1;
                true
            }
            _ => false,
        }
    }

    if scope.derived[table].is_some() || scope.slots[table] == usize::MAX {
        return None;
    }
    let mut columns = [0usize; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    let mut values = [Datum::Null; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
    let mut count = 0usize;
    if !collect(
        expression,
        scope,
        table,
        arena,
        &mut columns,
        &mut values,
        &mut count,
    ) || count < 2
    {
        return None;
    }
    for (slot, statistics) in storage.extended_statistics_for_table(scope.slots[table], txid) {
        if statistics.keys_for(txid).len() != count
            || statistics.keys_for(txid).iter().any(|key| {
                let crate::storage::ExtendedStatisticsKey::Column(column) = key else {
                    return true;
                };
                let definition = storage.table_def(scope.slots[table], txid);
                definition
                    .column_index(column.as_str())
                    .is_none_or(|column| !columns[..count].contains(&column))
            })
        {
            continue;
        }
        let data = storage.extended_statistics_data(slot, txid);
        if !data.valid || data.rows == 0 {
            continue;
        }
        let mut ordered = [Datum::Null; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
        let indices = [0u16, 1, 2, 3, 4, 5, 6, 7];
        for (position, key) in statistics.keys_for(txid).iter().enumerate() {
            let crate::storage::ExtendedStatisticsKey::Column(column) = key else {
                unreachable!("expression keys were rejected above")
            };
            let column = storage
                .table_def(scope.slots[table], txid)
                .column_index(column.as_str())?;
            let source = columns[..count]
                .iter()
                .position(|candidate| *candidate == column)?;
            ordered[position] = values[source];
        }
        if statistics.kinds.mcv() {
            let hash = crate::sql::eval::hash_key_identified(&ordered[..count], &indices[..count])?;
            if let Some(entry) = data.mcv[..usize::from(data.n_mcv)]
                .iter()
                .find(|entry| entry.hash == hash)
            {
                return Some((entry.count as f64 / data.rows as f64).clamp(0.0, 1.0));
            }
        }
        if statistics.kinds.ndistinct() {
            return Some(
                (data.non_null_rows as f64 / data.rows as f64 / data.distinct_values.max(1) as f64)
                    .clamp(0.0, 1.0),
            );
        }
        if statistics.kinds.dependencies() {
            let mut selectivities = [1.0f64; crate::storage::MAX_EXTENDED_STATISTICS_KEYS];
            for (position, key) in statistics.keys_for(txid).iter().enumerate() {
                let crate::storage::ExtendedStatisticsKey::Column(column) = key else {
                    unreachable!("expression keys were rejected above")
                };
                let column = storage
                    .table_def(scope.slots[table], txid)
                    .column_index(column.as_str())?;
                selectivities[position] =
                    column_selectivity(storage, scope, table, column, txid, 0.005);
            }
            let mut estimate = selectivities.iter().take(count).product::<f64>();
            for determinant in 0..count {
                for dependent in 0..count {
                    let strength = f64::from(
                        data.dependencies_ppm[determinant
                            * crate::storage::MAX_EXTENDED_STATISTICS_KEYS
                            + dependent],
                    ) / 1_000_000.0;
                    if strength > 0.0 {
                        let independent = selectivities[determinant] * selectivities[dependent];
                        let dependent_estimate = selectivities[determinant]
                            * (strength + (1.0 - strength) * selectivities[dependent]);
                        estimate *= dependent_estimate / independent.max(f64::MIN_POSITIVE);
                        return Some(estimate.clamp(0.0, 1.0));
                    }
                }
            }
        }
    }
    None
}

fn predicate_selectivity<'a>(
    storage: &Storage,
    scope: &QueryScope<'_>,
    table: usize,
    expression: &Expr<'a>,
    txid: u32,
    arena: &'a Arena,
) -> f64 {
    match expression {
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
            ..
        } => {
            if let Some(selectivity) =
                extended_statistics_selectivity(storage, scope, table, expression, txid, arena)
            {
                return selectivity;
            }
            let left = predicate_selectivity(storage, scope, table, left, txid, arena);
            let right = predicate_selectivity(storage, scope, table, right, txid, arena);
            (left * right).clamp(0.0, 1.0)
        }
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
            ..
        } => {
            let left = predicate_selectivity(storage, scope, table, left, txid, arena);
            let right = predicate_selectivity(storage, scope, table, right, txid, arena);
            (left + right - left * right).clamp(0.0, 1.0)
        }
        Expr::Binary {
            operator:
                operator
                @ (BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq),
            ..
        } => {
            if *operator == BinaryOp::Eq
                && let Some(selectivity) = expression_statistics_selectivity(
                    storage, scope, table, expression, txid, arena,
                )
            {
                return selectivity;
            }
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
            let definition = storage.table_def(scope.slots[table], txid);
            let column_statistics = definition
                .attributes
                .number(column)
                .and_then(|attribute| statistics.column(attribute));
            let Some(column_statistics) = column_statistics.filter(|_| statistics.valid) else {
                return if *negated { 0.995 } else { 0.005 };
            };
            let fraction = f64::from(column_statistics.null_fraction_ppm) / 1_000_000.0;
            if *negated { 1.0 - fraction } else { fraction }
        }
        Expr::InList {
            operand,
            list,
            negated: false,
        } if list.iter().all(|item| item.is_constant(false)) => {
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

#[expect(
    clippy::too_many_arguments,
    reason = "physical plan inputs are explicit"
)]
fn scan_node<'a>(
    storage: &Storage,
    scope: &QueryScope<'a>,
    table: usize,
    predicate: Option<&'a Expr<'a>>,
    txid: u32,
    depth: u8,
    arena: &'a Arena,
    ordered: Option<query::OrderedIndexAccessPlan<'a>>,
    parameterized: Option<query::IndexAccessPlan<'a>>,
    index_only: bool,
) -> Result<PlanNode, SqlError> {
    let slot = scope.slots[table];
    let derived = scope.derived[table].is_some() || slot == usize::MAX;
    let rows = if derived {
        scope.derived[table].map_or(1_000, |rows| rows.len() as u64)
    } else {
        storage.planning_row_estimate(slot)
    };
    let selectivity = predicate.map_or(1.0, |predicate| {
        predicate_selectivity(storage, scope, table, predicate, txid, arena)
    });
    let predicate_rows = if rows == 0 {
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
    // Execution and EXPLAIN consume the same typed leading-key plan. A
    // complete resident equality map has no durable-read cost and therefore
    // wins even when the immutable table and index estimates tie.
    let ordered = ordered.filter(|_| table == 0);
    let fallback_index = if table == 0 {
        query::index_access_plan(storage, scope, txid, predicate, arena)?
    } else {
        None
    };
    let index_plan = ordered
        .map(|plan| plan.access())
        .or(parameterized)
        .or(fallback_index);
    let index_rows = index_plan.map_or(predicate_rows, |plan| {
        plan.expected_rows(
            storage,
            slot,
            scope.defs.stored(table).expect("base table"),
            txid,
        )
    });
    // Predicate statistics remain authoritative for rows emitted by the scan:
    // unlike the access path, they can use joint statistics and non-index
    // filters. The access estimate instead prices the index candidates that
    // execution must fetch before the complete predicate recheck.
    let output_rows = predicate_rows;
    let index_identity = ordered
        .map(|ordered| (ordered.access(), ordered.index_name()))
        .or_else(|| index_plan.map(|plan| (plan, plan.index_name())));
    let resident_index = index_identity.is_some_and(|(plan, _)| {
        !matches!(
            plan.method(),
            crate::sql::ast::IndexAccessMethod::Brin
                | crate::sql::ast::IndexAccessMethod::Gist
                | crate::sql::ast::IndexAccessMethod::Gin
                | crate::sql::ast::IndexAccessMethod::SpGist
        ) && plan.is_exact()
            && storage.value_binding_cache_complete(slot, plan.binding())
    });
    let selective_brin = index_identity.is_some_and(|(plan, _)| {
        plan.method() == crate::sql::ast::IndexAccessMethod::Brin
            && index_rows.saturating_mul(8) <= rows
    });
    let selective_gist = index_identity.is_some_and(|(plan, _)| {
        plan.method() == crate::sql::ast::IndexAccessMethod::Gist
            && index_rows.saturating_mul(4) <= rows
    });
    let selective_gin = index_identity.is_some_and(|(plan, _)| {
        plan.method() == crate::sql::ast::IndexAccessMethod::Gin
            && index_rows.saturating_mul(4) <= rows
    });
    let selective_spgist = index_identity.is_some_and(|(plan, _)| {
        plan.method() == crate::sql::ast::IndexAccessMethod::SpGist
            && index_rows.saturating_mul(4) <= rows
    });
    let use_index = ordered.is_some()
        || index_identity.is_some()
            && (resident_index
                || selective_brin
                || selective_gist
                || selective_gin
                || selective_spgist
                || (generations != 0
                    && !storage.sequential_spill_scan_is_cheaper(slot, index_rows, txid)));
    let blocks = if use_index {
        // One bounded key-generation descent per immutable generation, then
        // only the row blocks expected to survive the predicate.
        generations.saturating_mul(3).saturating_add(
            index_rows
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
    let cpu_rows = if use_index { index_rows } else { rows };
    let cpu_cost = cpu_rows as f64 * 0.01;
    let mut relation = StackStr::new();
    let _ = write!(relation, "{}", scope.names[table]);
    let mut bitmap_index = StackStr::new();
    let name = if let Some((access, index)) = index_identity.filter(|_| use_index) {
        if matches!(
            access.method(),
            crate::sql::ast::IndexAccessMethod::Brin | crate::sql::ast::IndexAccessMethod::Gin
        ) {
            bitmap_index = index;
            StackStr::from_str("Bitmap Heap Scan")
        } else {
            let mut name = StackStr::new();
            let kind = if index_only && ordered.is_some() {
                "Index Only Scan"
            } else {
                "Index Scan"
            };
            let _ = write!(name, "{kind} using {}", index.as_str());
            name
        }
    } else {
        StackStr::from_str(if derived { "Subquery Scan" } else { "Seq Scan" })
    };
    Ok(PlanNode {
        name,
        bitmap_index,
        relation,
        output: StackStr::new(),
        depth,
        startup_cost: 0.0,
        total_cost: io_cost + cpu_cost,
        rows: output_rows,
        width,
        object_requests,
        cache_blocks,
        constant_false: false,
        qual_label: QualLabel::None,
        qual: StackStr::new(),
    })
}

pub(super) fn plan_select<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    let scope = match &statement.from {
        Some(from) => Some(QueryScope::resolve_schema(
            storage, from, txid, arena, None,
        )?),
        None => None,
    };
    let capacity = scope
        .as_ref()
        .map_or(MAX_EXPLAIN_STAGES, |scope| scope.n * 2 + MAX_EXPLAIN_STAGES);
    let mut plan = Plan::new(arena, capacity)?;
    let (width, output) = projected_shape(statement, scope.as_ref(), storage, txid, arena)?;
    let scoped_columns = scope.as_ref().map(|scope| query::CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    });
    let empty_columns = crate::sql::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let columns: &dyn crate::sql::exec::ColTypeResolver = scoped_columns.as_ref().map_or(
        &empty_columns as &dyn crate::sql::exec::ColTypeResolver,
        |columns| columns,
    );
    let aggregate = has_aggregate(statement, storage, txid, columns)?;
    let ordered_index = if !statement.distinct
        && statement.distinct_on.is_empty()
        && !statement.order_by.is_empty()
        && !aggregate
        && !has_window(statement)
        && scope.as_ref().is_some_and(|scope| scope.n == 1)
        && statement
            .from
            .as_ref()
            .is_some_and(|from| from.joins.is_empty() && from.base.sample.is_none())
    {
        query::ordered_index_access_plan(
            storage,
            scope.as_ref().expect("checked"),
            txid,
            statement.from.as_ref().expect("checked").base.inheritance
                == crate::sql::ast::RelationInheritance::Descendants,
            statement.where_clause,
            statement.items,
            statement.order_by,
            arena,
        )?
    } else {
        None
    };
    let ordered_index_only = match (
        ordered_index,
        scope.as_ref(),
        statement.from.as_ref(),
        storage.current_role_slot(txid),
    ) {
        (Some(index), Some(scope), Some(from), Some(current_role)) => {
            let role = scope.authorization_roles[0].map_or(current_role, usize::from);
            statement.locking.is_empty()
                && !storage.row_security_applies(scope.slots[0], role, txid)
                && query::streaming_pax_columns(
                    scope,
                    from,
                    statement.items,
                    statement.where_clause,
                )
                .selected_mask(0)
                .is_some_and(|demanded| index.covers(demanded))
        }
        _ => false,
    };

    if statement
        .where_clause
        .and_then(|predicate| query::plan_time_bool(predicate, arena))
        == Some(false)
    {
        plan.push(PlanNode {
            name: StackStr::from_str("Result"),
            output,
            rows: 0,
            width,
            total_cost: 0.0,
            constant_false: true,
            ..PlanNode::EMPTY
        })?;
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }

    let mut estimated_rows = 1u64;
    let mut total_cost = 0.01f64;
    let mut object_requests = 0u64;
    let mut cache_blocks = 0u64;
    let mut hash_join = false;
    let table_count = scope.as_ref().map_or(0, |scope| scope.n);
    let table_order = arena
        .alloc_slice_with(table_count, |index| index)
        .map_err(|_| arena_full())?;
    let parameterized_indexes = arena
        .alloc_slice_with(table_count, |_| None)
        .map_err(|_| arena_full())?;
    if let Some(scope) = &scope {
        let reorderable = statement.from.as_ref().is_some_and(|from| {
            from.joins.iter().all(|join| join.kind.is_cross())
                && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
        });
        if reorderable {
            table_order.copy_from_slice(query::join_order(
                storage,
                scope,
                statement.where_clause,
                arena,
            )?);
        }
        if let Some(from) = statement.from.as_ref() {
            for depth in 1..scope.n {
                let table = table_order[depth];
                parameterized_indexes[table] = query::parameterized_index_access_plan(
                    storage,
                    scope,
                    from,
                    statement.where_clause,
                    &table_order[..scope.n],
                    depth,
                    txid,
                    false,
                    arena,
                )?;
            }
        }
        estimated_rows = 1;
        total_cost = 0.0;
        for &table in &table_order[..scope.n] {
            let scan = scan_node(
                storage,
                scope,
                table,
                statement.where_clause,
                txid,
                1,
                arena,
                ordered_index,
                parameterized_indexes[table],
                ordered_index_only,
            )?;
            estimated_rows = estimated_rows.saturating_mul(scan.rows.max(1));
            total_cost += scan.total_cost;
            object_requests = object_requests.saturating_add(scan.object_requests);
            cache_blocks = cache_blocks.saturating_add(scan.cache_blocks);
        }
        if let Some(from) = statement.from.as_ref() {
            hash_join = query::select_hash_join_plan(
                storage,
                scope,
                from,
                statement.where_clause,
                &table_order[..scope.n],
                txid,
                arena,
            )?
            .is_some();
        }
    }

    let mut stages = [PlanNode::EMPTY; MAX_EXPLAIN_STAGES];
    let mut stage_count = 0usize;
    fn add_stage(
        stages: &mut [PlanNode; MAX_EXPLAIN_STAGES],
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
                name: StackStr::from_str(if hash_join {
                    "Hash Join"
                } else {
                    "Nested Loop"
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
    if !statement.order_by.is_empty() && ordered_index.is_none() {
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
    // The WHERE conjuncts PostgreSQL attaches to each scan and to the join
    // of two relations, when every conjunct can be placed and printed.
    let mut filters: [Option<StackStr<512>>; 2] = [None, None];
    // Two relations, joined by a comma, CROSS JOIN or an inner join's ON
    // condition, which PostgreSQL merges with the WHERE conjuncts.
    let two_way = scope.as_ref().is_some_and(|scope| {
        scope.n == 2
            && statement.from.as_ref().is_some_and(|from| {
                from.joins.iter().all(|join| {
                    join.kind.is_cross()
                        || (join.kind == crate::sql::ast::JoinKind::Inner
                            && join.using.is_none()
                            && !join.natural)
                }) && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
            })
    });
    let mut join_condition = statement.where_clause;
    if two_way && let Some(from) = statement.from.as_ref() {
        for join in from.joins {
            join_condition = query::and_where(join_condition, join.on, arena)?;
        }
    }
    if let Some(scope) = &scope {
        let relation = |table: usize| {
            scope
                .defs
                .stored(table)
                .map(|def| (scope.names[table], def))
        };
        if scope.n == 1
            && let Some((exposed, def)) = relation(0)
        {
            filters[0] =
                single_relation_filter(storage, txid, exposed, def, statement.where_clause, arena)?;
        } else if two_way
            && let (Some(outer), Some(inner)) = (relation(table_order[0]), relation(table_order[1]))
            && let Some(quals) = two_relation_quals(
                storage,
                txid,
                [outer, inner],
                join_condition,
                hash_join,
                arena,
            )?
        {
            filters = quals.filters;
            stages[0] = stages[0]
                .with_qual(QualLabel::HashCond, quals.hash_cond)
                .with_qual(QualLabel::JoinFilter, quals.join_filter);
        }
    }
    for (depth, stage) in stages[..stage_count].iter().rev().enumerate() {
        let mut stage = *stage;
        stage.depth = depth as u8;
        stage.output = output;
        plan.push(stage)?;
    }
    if let Some(scope) = &scope {
        for (position, &table) in table_order[..scope.n].iter().enumerate() {
            let mut scan = scan_node(
                storage,
                scope,
                table,
                statement.where_clause,
                txid,
                stage_count as u8,
                arena,
                ordered_index,
                parameterized_indexes[table],
                ordered_index_only,
            )?;
            scan.output = output;
            let sequential = scan.name.as_str() == "Seq Scan";
            if sequential && position < 2 {
                scan = scan.with_qual(QualLabel::Filter, filters[position].take());
            }
            if two_way && position == 1 {
                // A hash join hashes its inner relation; a nested loop
                // rescans its inner sequential scan from a materialized copy.
                let wrapper = if hash_join {
                    Some("Hash")
                } else {
                    sequential.then_some("Materialize")
                };
                if let Some(wrapper) = wrapper {
                    plan.push(PlanNode {
                        name: StackStr::from_str(wrapper),
                        depth: scan.depth,
                        total_cost: scan.total_cost,
                        rows: scan.rows,
                        width: scan.width,
                        object_requests: scan.object_requests,
                        cache_blocks: scan.cache_blocks,
                        output: scan.output,
                        ..PlanNode::EMPTY
                    })?;
                    scan.depth += 1;
                }
            }
            push_scan(&mut plan, scan)?;
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
    index: Option<(&str, u64, crate::sql::ast::IndexAccessMethod)>,
) -> PlanNode {
    let rows = storage.planning_row_estimate(slot);
    let output_rows = index.map_or_else(
        || if filtered { rows.div_ceil(10) } else { rows },
        |(_, expected_rows, _)| expected_rows,
    );
    let statistics = storage.table_statistics(slot, txid);
    let width = if statistics.valid {
        statistics.average_row_width.max(1)
    } else {
        32
    };
    let generations = storage.spill_generation_count(slot) as u64;
    let full_scan_blocks = if generations == 0 {
        0
    } else {
        rows.saturating_mul(u64::from(width))
            .div_ceil(crate::store::MAX_PAYLOAD as u64)
            .saturating_add(generations.saturating_mul(2))
    };
    let blocks = if index.is_some() {
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
    let total_cost = object_requests as f64 * object_request_cost(storage.block_io_stats())
        + cache_blocks as f64 * (ram_probability * 0.01 + disk_probability * 0.1)
        + if index.is_some() { output_rows } else { rows } as f64 * 0.01;
    let mut bitmap_index = StackStr::new();
    let name = index.map_or_else(
        || StackStr::from_str("Seq Scan"),
        |(index_name, _, method)| {
            if matches!(
                method,
                crate::sql::ast::IndexAccessMethod::Brin | crate::sql::ast::IndexAccessMethod::Gin
            ) {
                bitmap_index = StackStr::from_str(index_name);
                return StackStr::from_str("Bitmap Heap Scan");
            }
            let mut name = StackStr::new();
            let _ = write!(name, "Index Scan using {index_name}");
            name
        },
    );
    PlanNode {
        name,
        bitmap_index,
        relation: StackStr::from_str(relation_name),
        output: StackStr::new(),
        depth,
        startup_cost: 0.0,
        total_cost,
        rows: output_rows,
        width,
        object_requests,
        cache_blocks,
        constant_false: false,
        qual_label: QualLabel::None,
        qual: StackStr::new(),
    }
}

fn push_set_tree(
    plan: &mut Plan<'_>,
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
            ..
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
                bitmap_index: StackStr::new(),
                relation: StackStr::new(),
                output: left.output,
                depth,
                startup_cost: 0.0,
                total_cost: left.total_cost + right.total_cost + rows as f64 * 0.01,
                rows,
                width: left.width.max(right.width),
                object_requests: left.object_requests.saturating_add(right.object_requests),
                cache_blocks: left.cache_blocks.saturating_add(right.cache_blocks),
                constant_false: false,
                qual_label: QualLabel::None,
                qual: StackStr::new(),
            };
            plan.nodes[at] = node;
            Ok(node)
        }
    }
}

pub(super) fn plan_set_query<'a>(
    storage: &Storage,
    txid: u32,
    query: &SetQuery<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    let wrapper_count =
        usize::from(!query.order_by.is_empty()) + usize::from(query.limit.is_some());
    let mut plan = Plan::new(
        arena,
        wrapper_count.saturating_add(set_tree_plan_capacity(query.body)),
    )?;
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

pub(super) fn plan_modification<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    let (verb, target, filtered, source, predicate, alias, joined) = match statement {
        Stmt::Insert(insert) => (
            "Insert",
            insert.table,
            false,
            insert.select,
            None,
            insert.alias,
            false,
        ),
        Stmt::Update(update) => (
            "Update",
            update.table,
            update.where_clause.is_some(),
            None,
            update.where_clause,
            update.alias,
            update.from.is_some(),
        ),
        Stmt::Delete(delete) => (
            "Delete",
            delete.table,
            delete.where_clause.is_some(),
            None,
            delete.where_clause,
            delete.alias,
            delete.using.is_some(),
        ),
        Stmt::Merge(merge) => (
            "Merge",
            merge.target,
            true,
            None,
            None,
            merge.target_alias,
            true,
        ),
        _ => {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN does not support this statement type"
            ));
        }
    };
    let joined_from = match statement {
        Stmt::Update(update) => update.from,
        Stmt::Delete(delete) => delete.using,
        _ => None,
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
    let capacity = if let Some(from) = joined_from {
        3 + (from.joins.len() + 1) * 2
    } else if let Some(select) = source {
        1 + select_plan_capacity(select)
    } else {
        3
    };
    let mut plan = Plan::new(arena, capacity)?;
    let mut name = StackStr::new();
    let _ = write!(name, "{verb}");
    let target_node = PlanNode {
        name,
        bitmap_index: StackStr::new(),
        relation: StackStr::from_str(target.name),
        output: StackStr::new(),
        depth: 0,
        startup_cost: 0.0,
        total_cost: 0.01,
        rows: 0,
        width: 0,
        object_requests: 0,
        cache_blocks: 0,
        constant_false: false,
        qual_label: QualLabel::None,
        qual: StackStr::new(),
    };
    plan.push(target_node)?;
    if let Some(from) = joined_from {
        let target_slot = relation_slot(storage, txid, target.schema, target.name)
            .expect("target existence checked above");
        let scope = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
        let order = arena
            .alloc_slice_with(scope.n, |index| index)
            .map_err(|_| arena_full())?;
        if from.joins.iter().all(|join| join.kind.is_cross())
            && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
        {
            order.copy_from_slice(query::join_order(storage, &scope, predicate, arena)?);
        }
        let parameterized = arena
            .alloc_slice_with(scope.n, |_| None)
            .map_err(|_| arena_full())?;
        for depth in 0..scope.n {
            let table = order[depth];
            parameterized[table] = query::parameterized_index_access_plan(
                storage,
                &scope,
                from,
                predicate,
                &order[..scope.n],
                depth,
                txid,
                true,
                arena,
            )?;
        }
        let target_scan =
            physical_scan_node(storage, target_slot, target.name, false, txid, 2, None);
        let mut total_cost = target_scan.total_cost;
        let mut rows = target_scan.rows.max(1);
        let mut object_requests = target_scan.object_requests;
        let mut cache_blocks = target_scan.cache_blocks;
        let source_nodes = arena
            .alloc_slice_with(scope.n, |_| PlanNode::EMPTY)
            .map_err(|_| arena_full())?;
        for &table in &order[..scope.n] {
            let node = scan_node(
                storage,
                &scope,
                table,
                predicate,
                txid,
                2,
                arena,
                None,
                parameterized[table],
                false,
            )?;
            total_cost += node.total_cost;
            rows = rows.saturating_mul(node.rows.max(1));
            object_requests = object_requests.saturating_add(node.object_requests);
            cache_blocks = cache_blocks.saturating_add(node.cache_blocks);
            source_nodes[table] = node;
        }
        let mut join = PlanNode {
            name: StackStr::from_str("Nested Loop"),
            depth: 1,
            total_cost: total_cost + rows as f64 * 0.01,
            rows,
            width: target_scan.width,
            object_requests,
            cache_blocks,
            ..PlanNode::EMPTY
        };
        let mut target_scan = target_scan;
        // One source: the target and source scans' filters and the join
        // filter, and the source rescanned from a materialized copy.
        let target_definition = storage.table_def(target_slot, txid);
        let single_source = scope.n == 1 && source_nodes[0].name.as_str() == "Seq Scan";
        if single_source
            && let Some(source) = scope.defs.stored(0)
            && let Some(quals) = two_relation_quals(
                storage,
                txid,
                [
                    (alias.unwrap_or(target.name), &target_definition),
                    (scope.names[0], source),
                ],
                predicate,
                false,
                arena,
            )?
        {
            let [target_filter, source_filter] = quals.filters;
            target_scan = target_scan.with_qual(QualLabel::Filter, target_filter);
            source_nodes[0] = source_nodes[0].with_qual(QualLabel::Filter, source_filter);
            join = join.with_qual(QualLabel::JoinFilter, quals.join_filter);
        }
        plan.push(join)?;
        push_scan(&mut plan, target_scan)?;
        for &table in &order[..scope.n] {
            let mut node = source_nodes[table];
            if single_source {
                plan.push(PlanNode {
                    name: StackStr::from_str("Materialize"),
                    depth: node.depth,
                    total_cost: node.total_cost,
                    rows: node.rows,
                    width: node.width,
                    object_requests: node.object_requests,
                    cache_blocks: node.cache_blocks,
                    ..PlanNode::EMPTY
                })?;
                node.depth += 1;
            }
            push_scan(&mut plan, node)?;
        }
        plan.nodes[0].total_cost = join.total_cost;
        plan.nodes[0].object_requests = join.object_requests;
        plan.nodes[0].cache_blocks = join.cache_blocks;
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
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
            let index = (!joined && !storage.relation_has_descendants(slot, txid))
                .then(|| {
                    let definition = storage.table_def(slot, txid);
                    query::dml_index_access_plan(
                        storage,
                        slot,
                        &definition,
                        alias,
                        txid,
                        predicate,
                        arena,
                    )
                })
                .transpose()?
                .flatten()
                .map(|plan| (plan, plan.index_name()));
            let index = index.filter(|(plan, _)| {
                let expected_rows =
                    plan.expected_rows(storage, slot, &storage.table_def(slot, txid), txid);
                let resident_exact = !matches!(
                    plan.method(),
                    crate::sql::ast::IndexAccessMethod::Brin
                        | crate::sql::ast::IndexAccessMethod::Gist
                        | crate::sql::ast::IndexAccessMethod::Gin
                        | crate::sql::ast::IndexAccessMethod::SpGist
                ) && plan.is_exact()
                    && storage.value_binding_cache_complete(slot, plan.binding());
                let selective_brin = plan.method() == crate::sql::ast::IndexAccessMethod::Brin
                    && expected_rows.saturating_mul(8) <= storage.planning_row_estimate(slot);
                let selective_gist = plan.method() == crate::sql::ast::IndexAccessMethod::Gist
                    && expected_rows.saturating_mul(4) <= storage.planning_row_estimate(slot);
                let selective_gin = plan.method() == crate::sql::ast::IndexAccessMethod::Gin
                    && expected_rows.saturating_mul(4) <= storage.planning_row_estimate(slot);
                let selective_spgist = plan.method() == crate::sql::ast::IndexAccessMethod::SpGist
                    && expected_rows.saturating_mul(4) <= storage.planning_row_estimate(slot);
                resident_exact
                    || selective_brin
                    || selective_gist
                    || selective_gin
                    || selective_spgist
                    || (storage.spill_generation_count(slot) != 0
                        && !storage.sequential_spill_scan_is_cheaper(slot, expected_rows, txid))
            });
            let index_name = index.as_ref().map(|(plan, name)| {
                (
                    name.as_str(),
                    plan.expected_rows(storage, slot, &storage.table_def(slot, txid), txid),
                    plan.method(),
                )
            });
            let scan =
                physical_scan_node(storage, slot, target.name, filtered, txid, 1, index_name);
            if scan.name.as_str() == "Seq Scan" {
                let definition = storage.table_def(slot, txid);
                let filter = single_relation_filter(
                    storage,
                    txid,
                    alias.unwrap_or(target.name),
                    &definition,
                    predicate,
                    arena,
                )?;
                scan.with_qual(QualLabel::Filter, filter)
            } else {
                scan
            }
        };
        push_scan(&mut plan, child)?;
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

/// The column a node's property lines (`Filter:`, `Output:`) start at: two
/// past the root, six past a child's `->  ` arrow.
fn property_indent(depth: u8) -> usize {
    if depth == 0 {
        2
    } else {
        6 * usize::from(depth) + 2
    }
}

fn text_line(
    node: &PlanNode,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    root: bool,
) -> StackStr<512> {
    let mut line = StackStr::new();
    // PostgreSQL indents each child's `->  ` arrow six columns past its
    // parent's, starting two columns in.
    if node.depth > 0 {
        for _ in 0..property_indent(node.depth) - 6 {
            let _ = write!(line, " ");
        }
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
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    responder: &mut Responder,
) -> Result<(), WireFull> {
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
    visit_text_rows(plan, options, actual, settings, |line| {
        responder.data_row(&[Datum::Text(line)])
    })?;
    responder.command_complete("EXPLAIN")
}

pub(super) fn visit_text_rows<E>(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    mut emit: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    for (index, node) in plan.nodes[..plan.count].iter().enumerate() {
        let line = text_line(node, options, actual, index == 0);
        emit(line.as_str())?;
        if options.verbose && !node.output.as_str().is_empty() {
            let mut output = StackStr::<512>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(output, " ");
            }
            let _ = write!(output, "Output: {}", node.output.as_str());
            emit(output.as_str())?;
        }
        if node.qual_label != QualLabel::None {
            let mut qual = StackStr::<600>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(qual, " ");
            }
            let _ = write!(qual, "{}: {}", node.qual_label.text(), node.qual.as_str());
            emit(qual.as_str())?;
        }
        if node.constant_false {
            let mut filter = StackStr::<64>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(filter, " ");
            }
            let _ = write!(filter, "One-Time Filter: false");
            emit(filter.as_str())?;
        }
        if index == 0 && options.buffers {
            let actual = actual.expect("BUFFERS requires ANALYZE");
            let hits = actual.io.ram_hits.saturating_add(actual.io.disk_hits);
            if hits != 0 || actual.io.object_gets != 0 {
                let mut buffers = StackStr::<512>::new();
                let _ = write!(buffers, "  Buffers: shared");
                if hits != 0 {
                    let _ = write!(buffers, " hit={hits}");
                }
                if actual.io.object_gets != 0 {
                    let _ = write!(buffers, " read={}", actual.io.object_gets);
                }
                emit(buffers.as_str())?;
            }
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
            emit(wal.as_str())?;
        }
    }
    if options.settings && !settings.entries().is_empty() {
        let mut line = StackStr::<512>::new();
        let _ = write!(line, "Settings: ");
        for (index, setting) in settings.entries().iter().enumerate() {
            if index != 0 {
                let _ = write!(line, ", ");
            }
            let _ = write!(line, "{} = '", setting.name());
            for character in setting.value().chars() {
                if character == '\'' {
                    let _ = write!(line, "''");
                } else {
                    let _ = write!(line, "{character}");
                }
            }
            let _ = write!(line, "'");
        }
        emit(line.as_str())?;
    }
    if options.memory {
        emit("Planning:")?;
        let (used, allocated) = plan.memory_kib();
        let mut memory = StackStr::<512>::new();
        let _ = write!(
            memory,
            "  Memory: used={}kB  allocated={}kB",
            used, allocated
        );
        emit(memory.as_str())?;
    }
    if options.summary {
        let mut planning = StackStr::<512>::new();
        let _ = write!(
            planning,
            "Planning Time: {:.3} ms",
            plan.planning_micros as f64 / 1_000.0
        );
        emit(planning.as_str())?;
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
                emit(serialization.as_str())?;
            }
            let mut execution = StackStr::<512>::new();
            let _ = write!(
                execution,
                "Execution Time: {:.3} ms",
                actual.elapsed_micros as f64 / 1_000.0
            );
            emit(execution.as_str())?;
        }
    }
    Ok(())
}

pub(super) fn plan_statement<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    match statement {
        Stmt::Select(select) => {
            let select = if select.with.iter().any(|cte| cte.dml.is_some()) {
                return plan_data_modifying_cte_select(select, arena);
            } else if select.with.is_empty() {
                select
            } else {
                query::expand_ctes(select, storage, txid, arena)?
            };
            plan_select(storage, txid, select, arena)
        }
        Stmt::SetQuery(set_query) => {
            let body = query::expand_set_tree(
                set_query.with,
                set_query.body,
                storage,
                txid,
                None,
                None,
                arena,
            )?;
            let planned = SetQuery {
                with: &[],
                body,
                ..*set_query
            };
            plan_set_query(storage, txid, &planned, arena)
        }
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => {
            plan_modification(storage, txid, statement, arena)
        }
        Stmt::With { statement, .. } => plan_modification(storage, txid, statement, arena),
        _ => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN does not support this statement type"
        )),
    }
}

/// A data-modifying CTE's result relation exists only while the statement is
/// executing.  Planning must not run that write merely to discover its rows,
/// so model the consumer and each producer from the parsed boundary; ANALYZE
/// then executes the ordinary shared CTE path to obtain the actual metrics.
fn plan_data_modifying_cte_select<'a>(
    select: &Select<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    let capacity = 1 + select.with.iter().filter(|cte| cte.dml.is_some()).count();
    let mut plan = Plan::new(arena, capacity)?;
    plan.push(PlanNode {
        name: StackStr::from_str("CTE Scan"),
        rows: 1_000,
        width: 32,
        total_cost: 10.0,
        ..PlanNode::EMPTY
    })?;
    for cte in select.with.iter().filter(|cte| cte.dml.is_some()) {
        plan.push(PlanNode {
            name: StackStr::from_str("Data-modifying CTE"),
            relation: StackStr::from_str(cte.name),
            depth: 1,
            rows: 1_000,
            width: 32,
            total_cost: 10.0,
            ..PlanNode::EMPTY
        })?;
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
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

/// PostgreSQL names an index scan's operation and index independently in
/// machine-readable EXPLAIN. Text keeps the conventional combined spelling.
fn structured_node_name(node: &PlanNode) -> (&str, Option<&str>) {
    let name = node.name.as_str();
    match name.strip_prefix("Index Scan using ") {
        Some(index) => ("Index Scan", Some(index)),
        None => (name, None),
    }
}

/// Appends PostgreSQL's runtime fields for the one executor boundary that is
/// currently instrumented.  Cache tiers map onto PostgreSQL's shared hit/read
/// vocabulary at this client-visible boundary; the unmodelled local and temp
/// counters remain explicitly zero instead of being omitted.
fn render_json_runtime(out: &mut StackStr<16_384>, options: ExplainOptions, actual: ExplainActual) {
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
            ",\"Shared Hit Blocks\":{},\"Shared Read Blocks\":{},\"Shared Dirtied Blocks\":0,\"Shared Written Blocks\":0,\"Local Hit Blocks\":0,\"Local Read Blocks\":0,\"Local Dirtied Blocks\":0,\"Local Written Blocks\":0,\"Temp Read Blocks\":0,\"Temp Written Blocks\":0",
            actual.io.ram_hits.saturating_add(actual.io.disk_hits),
            actual.io.object_gets,
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

fn render_json_node(
    plan: &Plan<'_>,
    at: usize,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    out: &mut StackStr<16_384>,
) -> usize {
    let node = &plan.nodes[at];
    let (node_type, index_name) = structured_node_name(node);
    let _ = write!(out, "{{\"Node Type\":");
    json_string(out, node_type);
    let _ = write!(out, ",\"Parallel Aware\":false,\"Async Capable\":false");
    if let Some(index_name) = index_name {
        let _ = write!(out, ",\"Index Name\":");
        json_string(out, index_name);
    }
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
    if node.qual_label != QualLabel::None {
        let _ = write!(out, ",");
        json_string(out, node.qual_label.text());
        let _ = write!(out, ":");
        json_string(out, node.qual.as_str());
    }
    if node.constant_false {
        let _ = write!(out, ",\"One-Time Filter\":\"false\"");
    }
    if at == 0
        && let Some(actual) = actual
    {
        render_json_runtime(out, options, actual);
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

fn render_xml_settings(out: &mut StackStr<16_384>, settings: &PlannerSettings) {
    let _ = write!(out, "<Settings>");
    for setting in settings.entries() {
        let _ = write!(out, "<");
        xml_text(out, setting.name());
        let _ = write!(out, ">");
        xml_text(out, setting.value());
        let _ = write!(out, "</");
        xml_text(out, setting.name());
        let _ = write!(out, ">");
    }
    let _ = write!(out, "</Settings>");
}

fn xml_field(out: &mut StackStr<16_384>, name: &str, value: &str) {
    let _ = write!(out, "<{name}>");
    xml_text(out, value);
    let _ = write!(out, "</{name}>");
}

fn render_xml_runtime(out: &mut StackStr<16_384>, options: ExplainOptions, actual: ExplainActual) {
    if options.timing {
        xml_field(out, "Actual-Startup-Time", "0.000");
        let elapsed = stack_format!(32, "{:.3}", actual.elapsed_micros as f64 / 1_000.0);
        xml_field(out, "Actual-Total-Time", elapsed.as_str());
    }
    xml_field(
        out,
        "Actual-Rows",
        stack_format!(32, "{:.2}", actual.rows as f64).as_str(),
    );
    xml_field(out, "Actual-Loops", "1");
    if options.buffers {
        let hit = actual.io.ram_hits.saturating_add(actual.io.disk_hits);
        for (name, value) in [
            ("Shared-Hit-Blocks", hit),
            ("Shared-Read-Blocks", actual.io.object_gets),
            ("Shared-Dirtied-Blocks", 0),
            ("Shared-Written-Blocks", 0),
            ("Local-Hit-Blocks", 0),
            ("Local-Read-Blocks", 0),
            ("Local-Dirtied-Blocks", 0),
            ("Local-Written-Blocks", 0),
            ("Temp-Read-Blocks", 0),
            ("Temp-Written-Blocks", 0),
        ] {
            xml_field(out, name, stack_format!(32, "{value}").as_str());
        }
    }
    if options.wal {
        xml_field(
            out,
            "WAL-Records",
            stack_format!(32, "{}", actual.wal_records).as_str(),
        );
        xml_field(out, "WAL-FPI", "0");
        xml_field(
            out,
            "WAL-Bytes",
            stack_format!(32, "{}", actual.wal_bytes).as_str(),
        );
        xml_field(out, "WAL-Buffers-Full", "0");
    }
}

/// The internal plan is preorder plus depth.  Reconstructing the tree at the
/// rendering boundary keeps XML's nested `<Plans>` contract from depending on
/// executor layout or a second heap-owned representation.
fn render_xml_node(
    plan: &Plan<'_>,
    at: usize,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    out: &mut StackStr<16_384>,
) -> usize {
    let node = &plan.nodes[at];
    let (node_type, index_name) = structured_node_name(node);
    let _ = write!(out, "<Plan>");
    xml_field(out, "Node-Type", node_type);
    xml_field(out, "Parallel-Aware", "false");
    xml_field(out, "Async-Capable", "false");
    if let Some(index_name) = index_name {
        xml_field(out, "Index-Name", index_name);
    }
    if !node.relation.as_str().is_empty() {
        xml_field(out, "Relation-Name", node.relation.as_str());
    }
    if options.costs {
        xml_field(
            out,
            "Startup-Cost",
            stack_format!(32, "{:.2}", node.startup_cost).as_str(),
        );
        xml_field(
            out,
            "Total-Cost",
            stack_format!(32, "{:.2}", node.total_cost).as_str(),
        );
        xml_field(
            out,
            "Plan-Rows",
            stack_format!(32, "{}", node.rows).as_str(),
        );
        xml_field(
            out,
            "Plan-Width",
            stack_format!(32, "{}", node.width).as_str(),
        );
    }
    if options.verbose && !node.output.as_str().is_empty() {
        let _ = write!(out, "<Output>");
        for column in node.output.as_str().split(", ") {
            xml_field(out, "Item", column);
        }
        let _ = write!(out, "</Output>");
    }
    if node.qual_label != QualLabel::None {
        let mut name = StackStr::<16>::new();
        for character in node.qual_label.text().chars() {
            let _ = write!(name, "{}", if character == ' ' { '-' } else { character });
        }
        xml_field(out, name.as_str(), node.qual.as_str());
    }
    if node.constant_false {
        xml_field(out, "One-Time-Filter", "false");
    }
    xml_field(out, "Disabled", "false");
    if at == 0
        && let Some(actual) = actual
    {
        render_xml_runtime(out, options, actual);
    }
    let mut next = at + 1;
    if next < plan.count && plan.nodes[next].depth > node.depth {
        let _ = write!(out, "<Plans>");
        while next < plan.count && plan.nodes[next].depth > node.depth {
            if plan.nodes[next].depth != node.depth + 1 {
                break;
            }
            next = render_xml_node(plan, next, options, None, out);
        }
        let _ = write!(out, "</Plans>");
    }
    let _ = write!(out, "</Plan>");
    next
}

fn yaml_indent(out: &mut StackStr<16_384>, depth: usize) {
    for _ in 0..depth {
        let _ = write!(out, " ");
    }
}

fn yaml_field(out: &mut StackStr<16_384>, indent: usize, name: &str, value: &str) {
    yaml_indent(out, indent);
    let _ = write!(out, "{name}: ");
    json_string(out, value);
    let _ = writeln!(out);
}

fn yaml_number(
    out: &mut StackStr<16_384>,
    indent: usize,
    name: &str,
    value: impl core::fmt::Display,
) {
    yaml_indent(out, indent);
    let _ = writeln!(out, "{name}: {value}");
}

fn render_yaml_runtime(
    out: &mut StackStr<16_384>,
    indent: usize,
    options: ExplainOptions,
    actual: ExplainActual,
) {
    if options.timing {
        yaml_number(out, indent, "Actual Startup Time", "0.000");
        yaml_number(
            out,
            indent,
            "Actual Total Time",
            stack_format!(32, "{:.3}", actual.elapsed_micros as f64 / 1_000.0),
        );
    }
    yaml_number(
        out,
        indent,
        "Actual Rows",
        stack_format!(32, "{:.2}", actual.rows as f64),
    );
    yaml_number(out, indent, "Actual Loops", 1);
    if options.buffers {
        let hit = actual.io.ram_hits.saturating_add(actual.io.disk_hits);
        for (name, value) in [
            ("Shared Hit Blocks", hit),
            ("Shared Read Blocks", actual.io.object_gets),
            ("Shared Dirtied Blocks", 0),
            ("Shared Written Blocks", 0),
            ("Local Hit Blocks", 0),
            ("Local Read Blocks", 0),
            ("Local Dirtied Blocks", 0),
            ("Local Written Blocks", 0),
            ("Temp Read Blocks", 0),
            ("Temp Written Blocks", 0),
        ] {
            yaml_number(out, indent, name, value);
        }
    }
    if options.wal {
        yaml_number(out, indent, "WAL Records", actual.wal_records);
        yaml_number(out, indent, "WAL FPI", 0);
        yaml_number(out, indent, "WAL Bytes", actual.wal_bytes);
        yaml_number(out, indent, "WAL Buffers Full", 0);
    }
}

fn render_yaml_node(
    plan: &Plan<'_>,
    at: usize,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    out: &mut StackStr<16_384>,
    indent: usize,
    list_entry: bool,
) -> usize {
    let node = &plan.nodes[at];
    let (node_type, index_name) = structured_node_name(node);
    let field_indent = if list_entry { indent + 2 } else { indent };
    yaml_indent(out, indent);
    if list_entry {
        let _ = write!(out, "- ");
    }
    let _ = write!(out, "Node Type: ");
    json_string(out, node_type);
    let _ = writeln!(out);
    yaml_number(out, field_indent, "Parallel Aware", "false");
    yaml_number(out, field_indent, "Async Capable", "false");
    if let Some(index_name) = index_name {
        yaml_field(out, field_indent, "Index Name", index_name);
    }
    if !node.relation.as_str().is_empty() {
        yaml_field(out, field_indent, "Relation Name", node.relation.as_str());
    }
    if options.costs {
        yaml_number(
            out,
            field_indent,
            "Startup Cost",
            stack_format!(32, "{:.2}", node.startup_cost),
        );
        yaml_number(
            out,
            field_indent,
            "Total Cost",
            stack_format!(32, "{:.2}", node.total_cost),
        );
        yaml_number(out, field_indent, "Plan Rows", node.rows);
        yaml_number(out, field_indent, "Plan Width", node.width);
    }
    if options.verbose && !node.output.as_str().is_empty() {
        yaml_indent(out, field_indent);
        let _ = writeln!(out, "Output:");
        for column in node.output.as_str().split(", ") {
            yaml_indent(out, field_indent + 2);
            let _ = write!(out, "- ");
            json_string(out, column);
            let _ = writeln!(out);
        }
    }
    if node.qual_label != QualLabel::None {
        yaml_field(
            out,
            field_indent,
            node.qual_label.text(),
            node.qual.as_str(),
        );
    }
    if node.constant_false {
        yaml_field(out, field_indent, "One-Time Filter", "false");
    }
    yaml_number(out, field_indent, "Disabled", "false");
    if at == 0
        && let Some(actual) = actual
    {
        render_yaml_runtime(out, field_indent, options, actual);
    }
    let mut next = at + 1;
    if next < plan.count && plan.nodes[next].depth > node.depth {
        yaml_indent(out, field_indent);
        let _ = writeln!(out, "Plans:");
        while next < plan.count && plan.nodes[next].depth > node.depth {
            if plan.nodes[next].depth != node.depth + 1 {
                break;
            }
            next = render_yaml_node(plan, next, options, None, out, field_indent + 2, true);
        }
    }
    next
}

fn render_document(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
) -> Result<StackStr<16_384>, SqlError> {
    let mut out = StackStr::new();
    match options.format {
        ExplainFormat::Text => unreachable!("text has a row-per-line renderer"),
        ExplainFormat::Json => {
            let _ = write!(out, "[{{\"Plan\":");
            render_json_node(plan, 0, options, actual, &mut out);
            if options.settings {
                let _ = write!(out, ",\"Settings\":{{");
                for (index, setting) in settings.entries().iter().enumerate() {
                    if index != 0 {
                        let _ = write!(out, ",");
                    }
                    json_string(&mut out, setting.name());
                    let _ = write!(out, ":");
                    json_string(&mut out, setting.value());
                }
                let _ = write!(out, "}}");
            }
            if options.memory {
                let (used, allocated) = plan.memory_kib();
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
                "<explain xmlns=\"http://www.postgresql.org/2009/explain\"><Query>"
            );
            // XML carries the same tree as JSON and YAML rather than a flat
            // sequence of synthetic `<Node>` wrappers.
            render_xml_node(plan, 0, options, actual, &mut out);
            if options.settings {
                render_xml_settings(&mut out, settings);
            }
            if options.memory {
                let (used, allocated) = plan.memory_kib();
                let _ = write!(
                    out,
                    "<Planning><Memory-Used>{}</Memory-Used><Memory-Allocated>{}</Memory-Allocated></Planning>",
                    used, allocated
                );
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
            }
            let _ = write!(out, "</Query></explain>");
        }
        ExplainFormat::Yaml => {
            let _ = writeln!(out, "- Plan:");
            render_yaml_node(plan, 0, options, actual, &mut out, 4, false);
            if options.settings {
                let _ = writeln!(out, "  Settings:");
                for setting in settings.entries() {
                    let _ = write!(out, "    {}: ", setting.name());
                    json_string(&mut out, setting.value());
                    let _ = writeln!(out);
                }
            }
            if options.memory {
                let (used, allocated) = plan.memory_kib();
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

pub(super) fn visit_plan_rows(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    mut emit: impl FnMut(&str) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if options.format == ExplainFormat::Text {
        return visit_text_rows(plan, options, actual, settings, |line| emit(line));
    }
    let document = render_document(plan, options, actual, settings)?;
    emit(document.as_str())
}

/// An expression rewrite's failure: the arena filled, or the rewrite's own
/// error.
struct RewriteFailure(SqlError);

impl From<crate::mem::arena::ArenaFull> for RewriteFailure {
    fn from(_: crate::mem::arena::ArenaFull) -> Self {
        Self(arena_full())
    }
}

/// The statements a rewrite rule turns a data-changing statement into, in
/// PostgreSQL's order: for `INSERT` the original first, for `UPDATE` and
/// `DELETE` the rule actions first. Each action reads the original
/// relation's rows under the original's condition, with `OLD` and `NEW`
/// replaced by that row and its new values. None when no rule applies.
fn rule_rewritten_statements<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a [&'a Stmt<'a>]>, SqlError> {
    use crate::storage::{RewriteEvent, RewriteMode, RuleTarget};
    let (table, alias, event, condition) = match statement {
        Stmt::Insert(insert) => (insert.table, insert.alias, RewriteEvent::Insert, None),
        Stmt::Update(update) => (
            update.table,
            update.alias,
            RewriteEvent::Update,
            update.where_clause,
        ),
        Stmt::Delete(delete) => (
            delete.table,
            delete.alias,
            RewriteEvent::Delete,
            delete.where_clause,
        ),
        _ => return Ok(None),
    };
    let Some(crate::storage::ResolvedRelation::Table(slot)) =
        storage.resolve_relation(table.schema, table.name, txid)
    else {
        return Ok(None);
    };
    let target = RuleTarget::Table(slot as u16);
    if storage
        .firing_rules_for(target, event, false, txid)
        .next()
        .is_none()
    {
        return Ok(None);
    }
    let unsupported = |what: &str| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of a statement a rewrite rule changes is not supported for {}",
            what
        )
    };
    let definition = storage.table_def(slot, txid);
    let exposed = alias.unwrap_or(table.name);
    // A relation's unqualified columns named through its FROM item, as a
    // rewritten action reads both its own target and the original relation.
    let qualify_with = |expression: &'a Expr<'a>,
                        relation: &crate::storage::TableDef,
                        exposed: &'a str|
     -> Result<&'a Expr<'a>, SqlError> {
        crate::sql::ast::rewrite_expression(expression, arena, &mut |node: &'a Expr<'a>| -> Result<
            Option<&'a Expr<'a>>,
            RewriteFailure,
        > {
            Ok(match node {
                Expr::Column {
                    qualifier: None,
                    name,
                } if relation.column_index(name).is_some() => {
                    Some(&*arena.alloc(Expr::Column {
                        qualifier: Some(exposed),
                        name,
                    })?)
                }
                _ => None,
            })
        })
        .map_err(|RewriteFailure(error)| error)
    };
    let qualify = |expression: &'a Expr<'a>| qualify_with(expression, &definition, exposed);
    // An action's own target columns, before the original relation joins it.
    let qualify_action = |expression: &'a Expr<'a>,
                          action_table: crate::sql::ast::QualName<'a>|
     -> Result<&'a Expr<'a>, SqlError> {
        match storage.resolve_relation(action_table.schema, action_table.name, txid) {
            Some(crate::storage::ResolvedRelation::Table(action_slot)) => qualify_with(
                expression,
                &storage.table_def(action_slot, txid),
                action_table.name,
            ),
            _ => Err(unsupported("an action on a view")),
        }
    };
    let old_value = |name: &'a str| -> Result<&'a Expr<'a>, SqlError> {
        Ok(&*arena
            .alloc(Expr::Column {
                qualifier: Some(exposed),
                name,
            })
            .map_err(|_| arena_full())?)
    };
    let new_value = |name: &'a str| -> Result<&'a Expr<'a>, SqlError> {
        match statement {
            Stmt::Update(update) => match update
                .assignments
                .iter()
                .find(|(assigned, _)| *assigned == name)
            {
                Some((_, value)) => qualify(value),
                None => old_value(name),
            },
            Stmt::Insert(insert) => {
                let [row] = insert.rows else {
                    return Err(unsupported("an INSERT of several or selected rows"));
                };
                let position = if insert.columns.is_empty() {
                    definition.column_index(name)
                } else {
                    insert.columns.iter().position(|column| *column == name)
                };
                match position.and_then(|position| row.get(position)) {
                    Some(Expr::DefaultMarker) => Err(unsupported("an inserted DEFAULT")),
                    Some(value) => Ok(value),
                    None => Ok(&Expr::Null),
                }
            }
            _ => Err(unsupported("NEW in a DELETE rule")),
        }
    };
    let substitute = |expression: &'a Expr<'a>| -> Result<&'a Expr<'a>, SqlError> {
        crate::sql::ast::rewrite_expression(expression, arena, &mut |node: &'a Expr<'a>| -> Result<
            Option<&'a Expr<'a>>,
            RewriteFailure,
        > {
            match node {
                Expr::Column {
                    qualifier: Some(qualifier),
                    name,
                } if qualifier.eq_ignore_ascii_case("old") => {
                    old_value(name).map(Some).map_err(RewriteFailure)
                }
                Expr::Column {
                    qualifier: Some(qualifier),
                    name,
                } if qualifier.eq_ignore_ascii_case("new") => {
                    new_value(name).map(Some).map_err(RewriteFailure)
                }
                _ => Ok(None),
            }
        })
        .map_err(|RewriteFailure(error)| error)
    };
    let source = (event != RewriteEvent::Insert).then(|| crate::sql::ast::FromClause {
        base: query::rule_relation_ref(table, alias),
        joins: &[],
    });
    let original_condition = condition.map(qualify).transpose()?;
    let mut actions = crate::mem::arena::ArenaList::new(arena);
    let mut suppressed = false;
    let mut exclusions: Option<&'a Expr<'a>> = None;
    for (_, rule) in storage.firing_rules_for(target, event, false, txid) {
        let rule = rule.definition_for(txid);
        let rule_condition = match rule.condition_sql() {
            Some(sql) => {
                let sql = arena.alloc_str(sql).map_err(|_| arena_full())?;
                Some(substitute(crate::sql::parser::parse_expr(sql, arena)?)?)
            }
            None => None,
        };
        if rule.mode == RewriteMode::Instead {
            match rule_condition {
                None => suppressed = true,
                Some(rule_condition) => {
                    let negated = arena
                        .alloc(Expr::Unary {
                            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                            operator: crate::sql::ast::UnaryOp::Not,
                            operand: rule_condition,
                        })
                        .map_err(|_| arena_full())?;
                    exclusions = query::and_where(exclusions, Some(negated), arena)?;
                }
            }
        }
        let filter = query::and_where(original_condition, rule_condition, arena)?;
        for action in rule.action_sql() {
            let action = arena.alloc_str(action).map_err(|_| arena_full())?;
            let action = crate::sql::parser::parse_stored_statement(action, arena)?;
            let rewritten: Stmt<'a> = match *action {
                Stmt::Notify { .. } => *action,
                Stmt::Insert(insert)
                    if insert.select.is_none()
                        && insert.rows.len() == 1
                        && insert.returning.is_empty() =>
                {
                    let row = insert.rows[0];
                    let items = arena
                        .alloc_slice_with(row.len(), |_| {
                            SelectItem::Wildcard(crate::sql::ast::SourcePosition::UNKNOWN)
                        })
                        .map_err(|_| arena_full())?;
                    let mut values = crate::mem::arena::ArenaList::new(arena);
                    for (item, value) in items.iter_mut().zip(row) {
                        let value = substitute(value)?;
                        *item = SelectItem::Expr {
                            expression: value,
                            alias: None,
                        };
                        values.push(value).map_err(|_| arena_full())?;
                    }
                    if source.is_none() && filter.is_none() {
                        let row = arena
                            .alloc_slice_copy(values.as_slice())
                            .map_err(|_| arena_full())?;
                        let rows = arena
                            .alloc_slice_with(1, |_| &*row)
                            .map_err(|_| arena_full())?;
                        Stmt::Insert(crate::sql::ast::Insert { rows, ..insert })
                    } else {
                        let select = arena
                            .alloc(query::rule_select(items, source, filter))
                            .map_err(|_| arena_full())?;
                        Stmt::Insert(crate::sql::ast::Insert {
                            rows: &[],
                            select: Some(select),
                            ..insert
                        })
                    }
                }
                Stmt::Update(update) if update.from.is_none() && update.returning.is_empty() => {
                    let assignments = arena
                        .alloc_slice_with(update.assignments.len(), |index| {
                            update.assignments[index]
                        })
                        .map_err(|_| arena_full())?;
                    for (_, value) in assignments.iter_mut() {
                        *value = substitute(qualify_action(value, update.table)?)?;
                    }
                    let own = update
                        .where_clause
                        .map(|own| substitute(qualify_action(own, update.table)?))
                        .transpose()?;
                    let from = match source {
                        Some(source) => Some(&*arena.alloc(source).map_err(|_| arena_full())?),
                        None => None,
                    };
                    Stmt::Update(crate::sql::ast::Update {
                        assignments,
                        from,
                        where_clause: query::and_where(own, filter, arena)?,
                        ..update
                    })
                }
                Stmt::Delete(delete) if delete.using.is_none() && delete.returning.is_empty() => {
                    let own = delete
                        .where_clause
                        .map(|own| substitute(qualify_action(own, delete.table)?))
                        .transpose()?;
                    let using = match source {
                        Some(source) => Some(&*arena.alloc(source).map_err(|_| arena_full())?),
                        None => None,
                    };
                    Stmt::Delete(crate::sql::ast::Delete {
                        using,
                        where_clause: query::and_where(own, filter, arena)?,
                        ..delete
                    })
                }
                _ => return Err(unsupported("this rule action")),
            };
            let rewritten = &*arena.alloc(rewritten).map_err(|_| arena_full())?;
            actions.push(rewritten).map_err(|_| arena_full())?;
        }
    }
    let original = if suppressed {
        None
    } else if let Some(exclusions) = exclusions {
        let rewritten = match statement {
            Stmt::Update(update) => Stmt::Update(crate::sql::ast::Update {
                where_clause: query::and_where(update.where_clause, Some(exclusions), arena)?,
                ..*update
            }),
            Stmt::Delete(delete) => Stmt::Delete(crate::sql::ast::Delete {
                where_clause: query::and_where(delete.where_clause, Some(exclusions), arena)?,
                ..*delete
            }),
            _ => return Err(unsupported("a conditional INSTEAD rule on INSERT")),
        };
        Some(&*arena.alloc(rewritten).map_err(|_| arena_full())?)
    } else {
        Some(statement)
    };
    let actions = actions.as_slice();
    let count = actions.len() + usize::from(original.is_some());
    let ordered = arena
        .alloc_slice_with(count, |_| statement)
        .map_err(|_| arena_full())?;
    let (first, rest) = if event == RewriteEvent::Insert {
        (0, usize::from(original.is_some()))
    } else {
        (actions.len(), 0)
    };
    if let Some(original) = original {
        ordered[first] = original;
    }
    ordered[rest..rest + actions.len()].copy_from_slice(actions);
    Ok(Some(ordered))
}

/// EXPLAIN of a data-changing statement a rewrite rule changes: each
/// statement the rule produces planned in turn, separated by an empty line.
/// Returns false when no rule applies.
pub(super) fn emit_rewritten_plans(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    options: ExplainOptions,
    settings: &PlannerSettings,
    arena: &Arena,
    responder: &mut Responder,
) -> Result<Result<bool, SqlError>, WireFull> {
    let statements = match rule_rewritten_statements(storage, txid, statement, arena) {
        Ok(Some(statements)) => statements,
        Ok(None) => return Ok(Ok(false)),
        Err(error) => return Ok(Err(error)),
    };
    if options.analyze || options.format != ExplainFormat::Text {
        return Ok(Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN ANALYZE or a structured format of a statement a rewrite rule changes is not supported"
        )));
    }
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
    for (index, statement) in statements.iter().enumerate() {
        if index != 0 {
            responder.data_row(&[Datum::Text("")])?;
        }
        if let Stmt::Notify { .. } = statement {
            responder.data_row(&[Datum::Text("NOTIFY")])?;
            continue;
        }
        let plan = match plan_statement(storage, txid, statement, arena) {
            Ok(plan) => plan,
            Err(error) => return Ok(Err(error)),
        };
        visit_text_rows(&plan, options, None, settings, |line| {
            responder.data_row(&[Datum::Text(line)])
        })?;
    }
    responder.command_complete("EXPLAIN")?;
    Ok(Ok(true))
}

pub(super) fn emit_plan(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    responder: &mut Responder,
) -> Result<Result<(), SqlError>, WireFull> {
    if options.format == ExplainFormat::Text {
        emit_text(plan, options, actual, settings, responder)?;
        return Ok(Ok(()));
    }
    let document = match render_document(plan, options, actual, settings) {
        Ok(document) => document,
        Err(error) => return Ok(Err(error)),
    };
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
    responder.data_row(&[Datum::Text(document.as_str())])?;
    responder.command_complete("EXPLAIN")?;
    Ok(Ok(()))
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::{PlanNode, object_request_cost, structured_node_name};
    use crate::store::BlockIoStats;
    use crate::util::StackStr;

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

    #[test]
    fn structured_index_scan_separates_operation_and_index_name() {
        let node = PlanNode {
            name: StackStr::from_str("Index Scan using accounts_pkey"),
            ..PlanNode::EMPTY
        };
        assert_eq!(
            structured_node_name(&node),
            ("Index Scan", Some("accounts_pkey"))
        );
    }
}
