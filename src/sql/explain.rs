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
use crate::sql::eval::{ColumnLookup, SqlError, arena_full, sqlstate};
use crate::sql::guc::PlannerSettings;
use crate::sql::query::instrument::{
    BlockId, BufferUsage, HashColumns, NodeCounters, NodeKey, Role, Segment, Slot,
};
use crate::sql::query::{self, QueryScope};
use crate::sql::types::{ColDesc, Datum, oid};
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnSet, Storage};
use crate::store::BlockIoStats;
use crate::util::StackStr;

mod joins;
mod rewrite;
mod targets;

const MAX_EXPLAIN_STAGES: usize = 24;

// A SELECT can expose one heap and one bitmap node per source plus every
// operator stage. Composed set trees reserve the sum of their actual leaves,
// so independently accepted widths never form a new fixed product ceiling.
fn select_plan_capacity(statement: &Select<'_>) -> usize {
    // A one-table scan's bitmap may hold a tree of bitmap scans.
    statement.from.as_ref().map_or(MAX_EXPLAIN_STAGES, |from| {
        (from.joins.len() + 1) * 2 + MAX_EXPLAIN_STAGES + if from.joins.is_empty() { 64 } else { 0 }
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
    /// The name the query gives the relation, when it differs from it.
    alias: StackStr<96>,
    output: targets::Output,
    /// The node's target list is outside the deparser's coverage:
    /// `VERBOSE` rejects the plan rather than print a wrong `Output:`.
    output_unprintable: bool,
    /// The node's estimates are pos3ql's own rather than PostgreSQL's
    /// cost model: an `EXPLAIN` showing costs rejects the plan.
    unmodeled_costs: bool,
    depth: u8,
    startup_cost: f64,
    total_cost: f64,
    rows: u64,
    width: u32,
    object_requests: u64,
    cache_blocks: u64,
    constant_false: bool,
    /// The node's qualifications in PostgreSQL's display order; unused
    /// entries are labelled `QualLabel::None`.
    quals: [(QualLabel, StackStr<512>); 3],
    keys: PlanKeys,
    /// The node's identity in execution, for the nodes whose work
    /// `EXPLAIN ANALYZE` measures.
    key: Option<NodeKey>,
    /// A `Hash` node's row values, which size its table as PostgreSQL's.
    hash_columns: Option<HashColumns>,
    /// Qualifications as `VERBOSE` prints them, where that differs: with
    /// every column qualified by its relation.
    verbose_quals: [(QualLabel, StackStr<512>); 3],
    /// The schema of `relation`, which `VERBOSE` prints.
    schema: StackStr<64>,
    /// For the root of a subplan, how its parent runs it and its name
    /// (`InitPlan 1`, `SubPlan 2`, `CTE w`).
    subplan: Option<(SubplanKind, StackStr<72>)>,
    /// The subplans this node is nested in: PostgreSQL indents each
    /// subplan two columns past its parent's children.
    shift: u8,
    /// The relations a postgres_fdw scan of a join or grouped relation
    /// covers (`Aggregate on (public.t)`), which every format prints:
    /// schema-qualified under `VERBOSE`, by name otherwise.
    relations: StackStr<256>,
    plain_relations: StackStr<256>,
    /// The query a postgres_fdw node sends, which `VERBOSE` prints.
    remote_sql: StackStr<REMOTE_SQL_BYTES>,
    /// The rows a postgres_fdw INSERT sends per remote command.
    batch_size: Option<u32>,
    /// A `Subquery Scan` that returns every row of its child unchanged,
    /// reported with its child's figures.
    passthrough: bool,
    /// For the `Append` of a partitioned table's members, how many members
    /// executor startup pruned (`Subplans Removed`).
    subplans_removed: Option<u32>,
    /// A result relation of the write above it, which PostgreSQL lists
    /// among the write's properties (`Update on pt1 pt_1`) rather than as
    /// a plan node.
    target_member: bool,
    /// An `Append` whose members PostgreSQL prunes again during each scan
    /// by a subplan's or an enclosing query's value.
    scan_pruned: bool,
    /// A joined `UPDATE` or `DELETE`'s join, whose rows the executor does
    /// not count.
    uncounted: bool,
    /// A join whose inner relation matches at most one row per outer row
    /// by a unique index on its join keys (`innerrel_is_unique`).
    inner_unique: bool,
    /// A bitmap heap scan's index scan cost and rows, when the planner
    /// costed it.
    bitmap_index_estimate: Option<(f64, u64)>,
    /// Why `EXPLAIN ANALYZE` cannot count this node as PostgreSQL does.
    analyze_unsupported: Option<&'static str>,
    /// The partitions a hash aggregate expects to spill its groups to,
    /// which text shows only when there are any.
    planned_partitions: Option<u32>,
    /// What sizes a hash aggregate's memory as PostgreSQL's.
    hash_agg: Option<query::postgres_memory::HashAggSizing>,
    /// On an InitPlan's top node: what running it once added to the costs
    /// of the node it is attached to (`SS_charge_for_initplans`).
    initplan_charge: f64,
}

/// How a node runs a subplan: once before its first row, or per row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SubplanKind {
    InitPlan,
    SubPlan,
}

impl SubplanKind {
    const fn relationship(self) -> &'static str {
        match self {
            Self::InitPlan => "InitPlan",
            Self::SubPlan => "SubPlan",
        }
    }
}

/// The longest remote query an EXPLAIN node holds.
const REMOTE_SQL_BYTES: usize = 2_048;

/// `text` as a node's `Remote SQL`, which must fit it whole.
fn remote_sql_text(text: &str) -> Result<StackStr<REMOTE_SQL_BYTES>, SqlError> {
    let held = StackStr::from_str(text);
    if held.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "remote SQL of {} bytes exceeds EXPLAIN's {}-byte node property",
            text.len(),
            REMOTE_SQL_BYTES
        ));
    }
    Ok(held)
}

/// A node's `Sort Key` or `Group Key` items, separated by
/// [`KEY_SEPARATOR`]: as printed without `VERBOSE` (columns qualified only
/// when the query reads several relations) and as printed with it.
#[derive(Clone, Copy)]
struct PlanKeys {
    label: KeyLabel,
    plain: StackStr<256>,
    verbose: StackStr<256>,
}

const KEY_SEPARATOR: char = '\u{1f}';

#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyLabel {
    None,
    Sort,
    Group,
    /// A grouping sets node's key lines, each record a tag (`B` a rollup's
    /// start, `S` its sort, `H` a hashed set, `G` a grouped set) and its
    /// items, separated by [`GROUPING_SET_SEPARATOR`].
    Sets,
}

const GROUPING_SET_SEPARATOR: char = '\u{1e}';

impl PlanKeys {
    const NONE: Self = Self {
        label: KeyLabel::None,
        plain: StackStr::new(),
        verbose: StackStr::new(),
    };

    /// The label and items to print, if any.
    fn shown(&self, verbose: bool) -> Option<(&'static str, impl Iterator<Item = &str>)> {
        let label = match self.label {
            KeyLabel::None | KeyLabel::Sets => return None,
            KeyLabel::Sort => "Sort Key",
            KeyLabel::Group => "Group Key",
        };
        let keys = if verbose { &self.verbose } else { &self.plain };
        Some((label, keys.as_str().split(KEY_SEPARATOR)))
    }
}

const NO_QUALS: [(QualLabel, StackStr<512>); 3] = [(QualLabel::None, StackStr::new()); 3];

/// The label PostgreSQL's EXPLAIN gives a node's qualification.
#[derive(Clone, Copy, PartialEq, Eq)]
enum QualLabel {
    None,
    OneTimeFilter,
    Filter,
    HashCond,
    MergeCond,
    IndexCond,
    RecheckCond,
    JoinFilter,
    /// A `WindowAgg`'s window definition.
    Window,
    /// An incremental sort's keys its input is already sorted by.
    PresortedKey,
    /// A `Function Scan`'s call, which only `VERBOSE` prints.
    FunctionCall,
    /// A `Memoize` node's parameters and how it compares them.
    CacheKey,
    CacheMode,
}

impl QualLabel {
    /// PostgreSQL's EXPLAIN shows a join's or an index's conditions before
    /// its filter.
    const fn rank(self) -> u8 {
        match self {
            Self::Window
            | Self::PresortedKey
            | Self::FunctionCall
            | Self::OneTimeFilter
            | Self::HashCond
            | Self::MergeCond
            | Self::IndexCond
            | Self::RecheckCond
            | Self::CacheKey
            | Self::CacheMode => 0,
            Self::JoinFilter => 1,
            Self::Filter => 2,
            Self::None => 3,
        }
    }

    const fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::OneTimeFilter => "One-Time Filter",
            Self::Filter => "Filter",
            Self::HashCond => "Hash Cond",
            Self::MergeCond => "Merge Cond",
            Self::IndexCond => "Index Cond",
            Self::RecheckCond => "Recheck Cond",
            Self::JoinFilter => "Join Filter",
            Self::Window => "Window",
            Self::PresortedKey => "Presorted Key",
            Self::FunctionCall => "Function Call",
            Self::CacheKey => "Cache Key",
            Self::CacheMode => "Cache Mode",
        }
    }

    /// Whether only `VERBOSE` prints the property.
    const fn verbose_only(self) -> bool {
        matches!(self, Self::FunctionCall)
    }
}

impl PlanNode {
    /// A `HashAggregate` keeping distinct rows (no transition states) of
    /// `groups` estimated groups over its key columns: its memory sizing,
    /// or `EXPLAIN ANALYZE`'s refusal past the modelled key columns.
    fn with_distinct_hash(mut self, groups: u64, width: u32) -> Self {
        if self.name.as_str() != "HashAggregate" {
            return self;
        }
        let columns = self
            .keys
            .plain
            .as_str()
            .split(KEY_SEPARATOR)
            .filter(|key| !key.is_empty())
            .count();
        self.hash_agg = query::index_paths::hash_agg_sizing(
            &query::index_paths::AggregateCosts::default(),
            groups as f64,
            f64::from(width),
            columns,
        );
        if self.hash_agg.is_none() {
            self.analyze_unsupported = Some(
                "EXPLAIN ANALYZE of a hash aggregate of more grouping columns than PostgreSQL's memory figure is modelled for is not supported",
            );
        }
        self
    }

    /// This node with `qual` under `label`.
    fn with_qual(mut self, label: QualLabel, qual: Option<StackStr<512>>) -> Self {
        if let Some(qual) = qual {
            match self.quals.iter().position(|(held, _)| *held == label) {
                Some(index) => self.quals[index].1 = qual,
                None => {
                    let last = self.quals.len() - 1;
                    self.quals[last] = (label, qual);
                    self.quals.sort_by_key(|(held, _)| held.rank());
                }
            }
        }
        self
    }

    /// Names the relation `old` of this node's query block `new` in its
    /// conditions, keys and target list.
    fn requalify(&mut self, old: &str, new: &str) {
        for (_, qual) in self.quals.iter_mut().chain(self.verbose_quals.iter_mut()) {
            *qual = requalified(qual, old, new);
        }
        self.output = requalified(&self.output, old, new);
        self.keys.plain = requalified(&self.keys.plain, old, new);
        self.keys.verbose = requalified(&self.keys.verbose, old, new);
    }

    /// The `Relations` property as printed with or without `VERBOSE`.
    fn shown_relations(&self, verbose: bool) -> &str {
        if verbose {
            self.relations.as_str()
        } else {
            self.plain_relations.as_str()
        }
    }

    /// The name the query exposes the node's relation under.
    fn exposed_name(&self) -> &str {
        if self.alias.as_str().is_empty() {
            self.relation.as_str()
        } else {
            self.alias.as_str()
        }
    }

    /// This node with `qual` under `label` as `VERBOSE` prints it.
    fn with_verbose_qual(mut self, label: QualLabel, qual: Option<StackStr<512>>) -> Self {
        if let Some(qual) = qual
            && let Some(slot) = self
                .verbose_quals
                .iter_mut()
                .find(|(held, _)| *held == label || *held == QualLabel::None)
        {
            *slot = (label, qual);
        }
        self
    }

    /// The labelled qualifications, in display order.
    fn qualifications(&self) -> impl Iterator<Item = (QualLabel, &str)> {
        self.shown_qualifications(false)
    }

    /// The labelled qualifications as printed with or without `VERBOSE`.
    fn shown_qualifications(&self, verbose: bool) -> impl Iterator<Item = (QualLabel, &str)> {
        let mut shown = [(QualLabel::None, ""); 6];
        let mut count = 0;
        for (label, qual) in self
            .quals
            .iter()
            .filter(|(label, _)| *label != QualLabel::None)
        {
            let text = self
                .verbose_quals
                .iter()
                .find(|(held, _)| verbose && held == label)
                .map_or(qual.as_str(), |(_, verbose)| verbose.as_str());
            shown[count] = (*label, text);
            count += 1;
        }
        // A property only `VERBOSE` prints (`Function Call`).
        for (label, qual) in self.verbose_quals.iter().filter(|(label, _)| {
            verbose && label.verbose_only() && !self.quals.iter().any(|(held, _)| held == label)
        }) {
            shown[count] = (*label, qual.as_str());
            count += 1;
        }
        shown[..count].sort_by_key(|(label, _)| label.rank());
        shown.into_iter().take(count)
    }

    /// Sets the node's target list; `None` (outside the deparser's
    /// coverage) leaves it unprintable.
    fn set_output(&mut self, output: Option<targets::Output>) {
        self.output_unprintable = output.is_none();
        self.output = output.unwrap_or_default();
    }

    const EMPTY: Self = Self {
        name: StackStr::new(),
        bitmap_index: StackStr::new(),
        relation: StackStr::new(),
        alias: StackStr::new(),
        output: StackStr::new(),
        depth: 0,
        startup_cost: 0.0,
        total_cost: 0.0,
        rows: 0,
        width: 0,
        object_requests: 0,
        cache_blocks: 0,
        constant_false: false,
        quals: NO_QUALS,
        keys: PlanKeys::NONE,
        key: None,
        hash_columns: None,
        verbose_quals: NO_QUALS,
        schema: StackStr::new(),
        subplan: None,
        shift: 0,
        relations: StackStr::new(),
        plain_relations: StackStr::new(),
        output_unprintable: false,
        unmodeled_costs: false,
        remote_sql: StackStr::new(),
        batch_size: None,
        passthrough: false,
        subplans_removed: None,
        target_member: false,
        scan_pruned: false,
        uncounted: false,
        inner_unique: false,
        bitmap_index_estimate: None,
        analyze_unsupported: None,
        planned_partitions: None,
        hash_agg: None,
        initplan_charge: 0.0,
    };
}

/// What `EXPLAIN ANALYZE` reports for one node.
#[derive(Clone, Copy)]
#[expect(
    clippy::large_enum_variant,
    reason = "a node's counters are copied by value, never stored in bulk"
)]
enum NodeActual {
    /// Not analyzed, or a node whose work the executor does not count.
    Unmeasured,
    NeverExecuted,
    Ran(NodeCounters),
}

impl NodeActual {
    /// The rows removed by `label`'s qualification, per loop as PostgreSQL
    /// prints them, unless the node was not measured.
    fn removed(self, label: QualLabel) -> Option<f64> {
        let counters = match self {
            Self::Unmeasured => return None,
            Self::NeverExecuted => return Some(0.0),
            Self::Ran(counters) => counters,
        };
        let removed = match label {
            QualLabel::Filter => counters.filtered,
            QualLabel::JoinFilter => counters.join_filtered,
            QualLabel::IndexCond | QualLabel::RecheckCond => 0,
            QualLabel::OneTimeFilter
            | QualLabel::HashCond
            | QualLabel::MergeCond
            | QualLabel::Window
            | QualLabel::PresortedKey
            | QualLabel::FunctionCall
            | QualLabel::CacheKey
            | QualLabel::CacheMode
            | QualLabel::None => return None,
        };
        Some(removed as f64 / counters.loops as f64)
    }
}

impl QualLabel {
    /// The label of the count of rows this qualification removed.
    const fn removed_label(self) -> Option<&'static str> {
        match self {
            Self::Filter => Some("Rows Removed by Filter"),
            Self::JoinFilter => Some("Rows Removed by Join Filter"),
            Self::IndexCond | Self::RecheckCond => Some("Rows Removed by Index Recheck"),
            Self::OneTimeFilter
            | Self::HashCond
            | Self::MergeCond
            | Self::Window
            | Self::PresortedKey
            | Self::FunctionCall
            | Self::CacheKey
            | Self::CacheMode
            | Self::None => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct ExplainActual {
    pub(super) rows: u64,
    pub(super) elapsed_micros: u64,
    pub(super) serialized_bytes: u64,
    pub(super) serialization_micros: u64,
    pub(super) wal_records: u64,
    pub(super) wal_bytes: u64,
}

pub(super) struct Plan<'a> {
    nodes: &'a mut [PlanNode],
    count: usize,
    /// Per node, what it did while `EXPLAIN ANALYZE` ran the statement.
    measured: Option<&'a [NodeCounters]>,
    planning_micros: u64,
    /// The plan methods enabled while planning, which mark nodes that had
    /// to use a disabled one.
    methods: crate::sql::guc::PlannerMethods,
    /// An `INSERT ... ON CONFLICT`'s resolution.
    conflict: Option<&'a Conflict>,
}

/// The most arbiter indexes an `ON CONFLICT` explains.
const MAX_ARBITERS: usize = 8;

/// An `INSERT ... ON CONFLICT`'s resolution (`show_modifytable_info`): its
/// action and the unique indexes that arbitrate it, in OID order.
#[derive(Clone, Copy)]
struct Conflict {
    update: bool,
    arbiters: [StackStr<64>; MAX_ARBITERS],
    count: usize,
}

impl Conflict {
    fn arbiters(&self) -> impl Iterator<Item = &str> {
        self.arbiters[..self.count].iter().map(StackStr::as_str)
    }

    /// The conflict's analyzed outcome: the rows the insertion path
    /// inserted and those that conflicted, from the rows the write's input
    /// returned.
    fn tuples(plan: &Plan<'_>) -> Option<(u64, u64)> {
        let measured = plan.measured?;
        let input = (1..plan.count).find(|&at| !plan.nodes[at].target_member)?;
        let conflicts = measured[0].conflicts;
        Some((measured[input].rows.saturating_sub(conflicts), conflicts))
    }
}

impl<'a> Plan<'a> {
    fn new(arena: &'a Arena, capacity: usize) -> Result<Self, SqlError> {
        let nodes = arena
            .alloc_slice_with(capacity, |_| PlanNode::EMPTY)
            .map_err(|_| query::arena_full_pub())?;
        Ok(Self {
            nodes,
            count: 0,
            measured: None,
            planning_micros: 0,
            methods: crate::sql::guc::planner_methods(),
            conflict: None,
        })
    }

    /// Whether `node` runs a plan method the session disabled, which
    /// PostgreSQL 18 reports as `Disabled: true`.
    fn disabled(&self, node: &PlanNode) -> bool {
        let methods = self.methods;
        let name = node.name.as_str();
        if name == "Seq Scan" {
            !methods.seqscan
        } else if name.starts_with("Index Only Scan") {
            !methods.indexonlyscan
        } else if name.starts_with("Index Scan") {
            !methods.indexscan
        } else if name == "Bitmap Heap Scan" {
            !methods.bitmapscan
        } else if name.starts_with("Nested Loop") {
            !methods.nestloop
        } else if name.starts_with("Hash") && name.ends_with("Join") {
            !methods.hashjoin
        } else if name == "Sort" {
            !methods.sort
        } else if name == "HashAggregate" {
            !methods.hashagg
        } else {
            false
        }
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

    /// Runs `execute`, the statement this plan explains, counting the work
    /// of each node that has a key; `TIMING` also measures its time. A plan
    /// [`printable`] rejects is rejected before the statement runs.
    pub(super) fn analyze<R>(
        &mut self,
        arena: &'a Arena,
        options: ExplainOptions,
        execute: impl FnOnce() -> R,
    ) -> Result<R, SqlError> {
        let timing = options.timing;
        // PostgreSQL's per-scan pruning by a subplan's value runs the subplan
        // first and skips the members it rules out; execution here reads
        // them all, so the counts would differ.
        if self.nodes[..self.count].iter().any(|node| node.scan_pruned) {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE of partition pruning by a subquery or outer query value is not supported"
            ));
        }
        if let Some(message) = self.nodes[..self.count]
            .iter()
            .find_map(|node| node.analyze_unsupported)
        {
            return Err(sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "{}", message));
        }
        if self.nodes[..self.count].iter().any(|node| node.uncounted) {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE of a joined UPDATE or DELETE is not supported"
            ));
        }
        let nodes = &self.nodes[..self.count];
        let slots = arena
            .alloc_slice_with(self.count, |index| {
                Slot::new(nodes[index].key)
                    .with_hash_columns(nodes[index].hash_columns)
                    .with_hash_agg(nodes[index].hash_agg)
            })
            .map_err(|_| query::arena_full_pub())?;
        let result = query::instrument::analyze(slots, timing, options.buffers, execute);
        let counters = arena
            .alloc_slice_with(self.count, |index| slots[index].counters())
            .map_err(|_| query::arena_full_pub())?;
        mirror_passthrough(nodes, counters);
        buffers_counted(nodes, counters, options.buffers)?;
        self.measured = Some(counters);
        if counters
            .iter()
            .any(|counters| counters.cte_stored == Some(query::instrument::StoredSpace::Unsized))
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE of a WITH query returning values without a PostgreSQL storage size is not supported"
            ));
        }
        Ok(result)
    }

    /// What the tuplestore a `CTE Scan` at `index` reads held at most
    /// (`show_ctescan_info`): its `WITH` query's rows once read, else the
    /// empty store PostgreSQL creates with the scan.
    fn cte_storage(&self, index: usize) -> Option<(bool, usize)> {
        let node = &self.nodes[index];
        if node.name.as_str() != "CTE Scan" {
            return None;
        }
        let measured = self.measured?;
        let root = self.cte_root(index);
        let stored = root.and_then(|root| {
            let depth = self.nodes[root].depth;
            (root..self.count)
                .take_while(|&at| at == root || self.nodes[at].depth > depth)
                .find_map(|at| measured[at].cte_stored)
        });
        Some(match stored {
            Some(query::instrument::StoredSpace::Sized { disk, bytes }) => (disk, bytes),
            Some(query::instrument::StoredSpace::Unsized) | None => {
                (false, query::postgres_memory::Tuplestore::new().finish().1)
            }
        })
    }

    /// What the node at `index` did under `EXPLAIN ANALYZE`, as PostgreSQL
    /// reports it.
    fn actual(&self, index: usize, actual: Option<ExplainActual>) -> NodeActual {
        let Some(actual) = actual else {
            return NodeActual::Unmeasured;
        };
        let node = &self.nodes[index];
        let counters = self
            .measured
            .filter(|_| node.key.is_some() || node.passthrough)
            .map(|measured| measured[index])
            .map(|mut counters| {
                // Execution filters a `VALUES` list's rows as it builds them:
                // its scan removed the rows of the list it did not return.
                if node.name.as_str() == "Values Scan"
                    && node
                        .qualifications()
                        .any(|(label, _)| label == QualLabel::Filter)
                {
                    counters.filtered = node
                        .rows
                        .saturating_mul(counters.loops)
                        .saturating_sub(counters.rows);
                }
                counters
            });
        if let Some(counters) = counters.filter(|counters| counters.loops != 0) {
            return NodeActual::Ran(counters);
        }
        // The root always runs once. When the path execution took does not
        // count it, its rows are the statement's.
        if index == 0 {
            return NodeActual::Ran(NodeCounters {
                loops: 1,
                rows: actual.rows,
                startup_nanos: 0,
                total_nanos: actual.elapsed_micros.saturating_mul(1_000),
                ..NodeCounters::default()
            });
        }
        // A `Result` whose filter the planner folded to false returns no
        // rows, once per row request of the node reading it.
        if node.constant_false
            && node.key.is_none()
            && let Some(parent) = self.nodes[..index]
                .iter()
                .rposition(|parent| parent.depth + 1 == node.depth)
        {
            return match self.actual(parent, Some(actual)) {
                NodeActual::Ran(counters) => NodeActual::Ran(NodeCounters {
                    loops: counters.loops,
                    ..NodeCounters::default()
                }),
                other => other,
            };
        }
        let (Some(measured), Some(_)) = (self.measured, counters) else {
            return NodeActual::Unmeasured;
        };
        // A node whose own subtree ran did run: its operator is not one the
        // executor counts on the path it took.
        let subtree_ran = self.nodes[index + 1..self.count]
            .iter()
            .zip(&measured[index + 1..])
            .take_while(|(child, _)| child.depth > node.depth)
            .any(|(child, counters)| child.key.is_some() && counters.loops != 0);
        if subtree_ran {
            NodeActual::Unmeasured
        } else {
            NodeActual::NeverExecuted
        }
    }

    /// The pages node `index` and the nodes below it accessed, as
    /// PostgreSQL's per-node buffer usage includes its inputs' and
    /// subplans'; nothing before `EXPLAIN ANALYZE` ran the plan. A `WITH`
    /// query runs inside the first `CTE Scan` that reads it, so its pages
    /// count there rather than under the node listing it, with the work
    /// files of a tuplestore that outgrew memory.
    fn buffers(&self, index: usize) -> BufferUsage {
        let Some(measured) = self.measured else {
            return BufferUsage::default();
        };
        let depth = self.nodes[index].depth;
        let mut usage = BufferUsage::default();
        let mut at = index;
        while at < self.count && (at == index || self.nodes[at].depth > depth) {
            if at != index && self.cte_reader(at).is_some() {
                at = self.subtree_end(at);
                continue;
            }
            usage = usage.plus(measured[at].buffers);
            if let Some(root) = self
                .cte_root(at)
                .filter(|&root| self.cte_reader(root) == Some(at))
            {
                usage = usage.plus(self.buffers(root));
                if let Some((true, bytes)) = self.cte_storage(at) {
                    let blocks = bytes.div_ceil(8192) as u64;
                    usage = usage.plus(BufferUsage {
                        temp_read: blocks,
                        temp_written: blocks,
                        ..BufferUsage::default()
                    });
                }
            }
            at += 1;
        }
        usage
    }

    /// The index after node `index`'s subtree.
    fn subtree_end(&self, index: usize) -> usize {
        let depth = self.nodes[index].depth;
        (index + 1..self.count)
            .find(|&at| self.nodes[at].depth <= depth)
            .unwrap_or(self.count)
    }

    /// The root of the `WITH` query the `CTE Scan` at `index` reads.
    fn cte_root(&self, index: usize) -> Option<usize> {
        let node = &self.nodes[index];
        if node.name.as_str() != "CTE Scan" {
            return None;
        }
        let named = |at: &usize| {
            self.nodes[*at].subplan.as_ref().is_some_and(|(_, name)| {
                name.as_str()
                    .strip_prefix("CTE ")
                    .is_some_and(|name| name == node.relation.as_str())
            })
        };
        (0..index)
            .rev()
            .find(named)
            .or_else(|| (index + 1..self.count).find(named))
    }

    /// The first `CTE Scan` that ran reading the `WITH` query whose root is
    /// at `root`, which runs it.
    fn cte_reader(&self, root: usize) -> Option<usize> {
        let measured = self.measured?;
        let (_, name) = self.nodes[root].subplan.as_ref()?;
        name.as_str().strip_prefix("CTE ")?;
        (0..self.count).find(|&at| measured[at].loops != 0 && self.cte_root(at) == Some(root))
    }

    /// What an analyzed `MERGE` did with the rows of its join, the node at
    /// `join`: the rows each kind of action changed, and the rest.
    fn merge_tuples(&self, join: usize) -> Option<MergeOutcome> {
        let measured = self.measured?;
        if self.nodes[0].name.as_str() != "Merge" {
            return None;
        }
        let changed = measured[0].merged;
        let total = measured.get(join)?.rows;
        Some(MergeOutcome {
            total,
            inserted: changed.inserted,
            updated: changed.updated,
            deleted: changed.deleted,
            skipped: total
                .saturating_sub(changed.inserted)
                .saturating_sub(changed.updated)
                .saturating_sub(changed.deleted),
        })
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

/// The rows of an analyzed `MERGE`'s join (`total`) and what its actions
/// did with them (`show_modifytable_info`).
struct MergeOutcome {
    total: u64,
    inserted: u64,
    updated: u64,
    deleted: u64,
    skipped: u64,
}

impl MergeOutcome {
    fn properties(&self, document: &mut impl FnMut(&'static str, u64)) {
        document("Tuples Inserted", self.inserted);
        document("Tuples Updated", self.updated);
        document("Tuples Deleted", self.deleted);
        document("Tuples Skipped", self.skipped);
    }
}

/// The most subqueries one EXPLAIN shows as subplans.
const MAX_SUBPLANS: usize = 64;

thread_local! {
    /// The subqueries the statement being explained runs as subplans, by the
    /// address of their query, and the number of the last one.
    static SUBPLANS: core::cell::RefCell<([(usize, crate::sql::catalog::deparse::SublinkPlan); MAX_SUBPLANS], usize)> =
        const {
            core::cell::RefCell::new(
                ([(0, crate::sql::catalog::deparse::SublinkPlan::InitPlan(0)); MAX_SUBPLANS], 0),
            )
        };
}

/// Forgets the subplans of the statement explained before.
fn reset_subplans() {
    SUBPLANS.with(|subplans| subplans.borrow_mut().1 = 0);
}

/// Numbers `select` as the next subplan, as PostgreSQL numbers each
/// subquery once it has planned it (and so the subqueries inside it).
fn register_subplan(
    select: &Select<'_>,
    correlated: bool,
    compared: Compared,
) -> Result<crate::sql::catalog::deparse::SublinkPlan, SqlError> {
    SUBPLANS.with(|subplans| {
        let mut subplans = subplans.borrow_mut();
        let count = subplans.1;
        if count == MAX_SUBPLANS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN subplans exceed {}",
                MAX_SUBPLANS
            ));
        }
        let number = u16::try_from(count + 1).expect("bounded by MAX_SUBPLANS");
        let plan = match (correlated, compared) {
            (false, Compared::Hashed) => {
                crate::sql::catalog::deparse::SublinkPlan::HashedSubPlan(number)
            }
            (false, Compared::No) => crate::sql::catalog::deparse::SublinkPlan::InitPlan(number),
            _ => crate::sql::catalog::deparse::SublinkPlan::SubPlan(number),
        };
        subplans.0[count] = (core::ptr::from_ref(select) as usize, plan);
        subplans.1 = count + 1;
        Ok(plan)
    })
}

/// Takes the next subplan number for a plan no expression reads by its
/// query: a `WITH` query run once.
fn reserve_subplan() -> Result<u16, SqlError> {
    SUBPLANS.with(|subplans| {
        let mut subplans = subplans.borrow_mut();
        let count = subplans.1;
        if count == MAX_SUBPLANS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN subplans exceed {}",
                MAX_SUBPLANS
            ));
        }
        subplans.0[count] = (0, crate::sql::catalog::deparse::SublinkPlan::InitPlan(0));
        subplans.1 = count + 1;
        Ok(u16::try_from(count + 1).expect("bounded by MAX_SUBPLANS"))
    })
}

/// How a subquery compares its rows with an operand: `IN` and `= ANY` hash
/// an uncorrelated subquery's rows; any other comparison scans them per row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Compared {
    No,
    Hashed,
    Scanned,
}

impl Compared {
    fn of(expression: &Expr<'_>) -> Self {
        match expression {
            Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery {
                operator: BinaryOp::Eq,
                all: false,
                ..
            } => Self::Hashed,
            Expr::QuantifiedSubquery { .. } => Self::Scanned,
            _ => Self::No,
        }
    }
}

/// The subplan the explained statement runs `select` as, if any.
fn registered_sublink(select: *const ()) -> Option<crate::sql::catalog::deparse::SublinkPlan> {
    SUBPLANS.with(|subplans| {
        let subplans = subplans.borrow();
        subplans.0[..subplans.1]
            .iter()
            .find(|(held, _)| *held == select as usize)
            .map(|(_, plan)| *plan)
    })
}

/// Whether a nested loop reads its inner sequential scan through a
/// `Materialize` node. PostgreSQL's `match_unsorted_outer` offers a
/// materialized inner path beside the plain one when `enable_material` is
/// on, and `add_path` keeps the cheaper: `cost_material` charges for the
/// first scan and saves only on rescans, so with at most one outer row,
/// which never rescans, the plain inner path wins.
fn nested_loop_materializes(outer_rows: u64, enable_material: bool) -> bool {
    enable_material && outer_rows > 1
}

/// The query of a subquery EXPLAIN can show as a subplan.
fn sublink_query<'e>(expression: &'e Expr<'e>) -> Option<&'e Select<'e>> {
    match expression {
        Expr::Subquery(select)
        | Expr::Exists(select)
        | Expr::ArraySubquery(select)
        | Expr::InSubquery { select, .. }
        | Expr::QuantifiedSubquery { select, .. } => Some(select),
        _ => None,
    }
}

/// Whether every subquery of `expression` is a registered subplan, a
/// correlated one only when `correlated` allows it.
fn sublinks_registered(expression: &Expr<'_>, correlated: bool) -> bool {
    let mut registered = true;
    visit_sublinks(expression, &mut |node| {
        registered &= sublink_query(node)
            .and_then(|select| registered_sublink(core::ptr::from_ref(select).cast()))
            .is_some_and(|plan| {
                correlated || matches!(plan, crate::sql::catalog::deparse::SublinkPlan::InitPlan(_))
            });
    });
    registered
}

/// Calls `visit` with each subquery of `expression` outside other
/// subqueries, in written order.
fn visit_sublinks<'e>(expression: &'e Expr<'e>, visit: &mut dyn FnMut(&'e Expr<'e>)) {
    if matches!(
        expression,
        Expr::Subquery(_)
            | Expr::RowSubquery { .. }
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_)
    ) {
        visit(expression);
        if let Expr::InSubquery { operand, .. } | Expr::QuantifiedSubquery { operand, .. } =
            expression
        {
            visit_sublinks(operand, visit);
        }
        return;
    }
    let _ = query::walk_children(expression, &mut |child| {
        visit_sublinks(child, visit);
        Ok(())
    });
}

/// `condition` split into its pseudoconstant conjuncts, which read no
/// column, call no volatile function and read only subplans run once, and
/// which PostgreSQL checks once, and the rest.
fn split_gating<'e>(
    storage: &Storage,
    txid: u32,
    condition: Option<&'e Expr<'e>>,
    arena: &'e Arena,
) -> Result<(Option<&'e Expr<'e>>, Option<&'e Expr<'e>>), SqlError> {
    let Some(condition) = condition else {
        return Ok((None, None));
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none() {
        return Ok((None, Some(condition)));
    }
    let (mut gating, mut rest) = (None, None);
    for conjunct in &conjuncts[..count] {
        let mut columns = false;
        conjunct.for_each_column_reference(&mut |_, _| columns = true);
        let pseudoconstant = if conjunct.contains_subquery() {
            sublinks_registered(conjunct, false)
        } else {
            !query::expression_contains_volatile(conjunct, storage, txid, false)
        };
        if !columns && pseudoconstant {
            gating = query::and_where(gating, Some(conjunct), arena)?;
        } else {
            rest = query::and_where(rest, Some(conjunct), arena)?;
        }
    }
    Ok((gating, rest))
}

/// Whether the planner folds the qualification `condition` to false or
/// NULL, which returns no rows.
fn qual_false(condition: &Expr<'_>, arena: &Arena) -> bool {
    if query::plan_time_bool(condition, arena) == Some(false) {
        return true;
    }
    match condition {
        Expr::Null => true,
        Expr::BoundLiteral {
            value: crate::sql::ast::BoundLiteralValue::Folded(encoded),
            ..
        } => {
            let mut values = [crate::sql::types::Datum::Null];
            crate::sql::exec::decode_projected_row_into(encoded, arena, &mut values).is_ok()
                && matches!(values[0], crate::sql::types::Datum::Null)
        }
        _ => false,
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
    arena: &Arena,
) -> Option<()> {
    match condition {
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
            ..
        } => {
            split_conjuncts(left, out, count, arena)?;
            split_conjuncts(right, out, count, arena)
        }
        // The planner drops a conjunct it folded to true.
        Expr::Bool(true) => Some(()),
        Expr::BoundLiteral { .. } if query::plan_time_bool(condition, arena) == Some(true) => {
            Some(())
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
    // A subplan run once reads no relation of the query; one run per row
    // reads the relations it is correlated with, which only a single
    // relation's scan places.
    if expression.contains_subquery() && !sublinks_registered(expression, false) {
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
/// clauses it regenerates after the scan's other restrictions. An
/// uncorrelated scalar subquery is an InitPlan's output parameter there,
/// which the class holds as a constant.
fn equivalence_clause<'a>(
    conjunct: &'a Expr<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let initplan = |side: &'a Expr<'a>| -> Result<bool, SqlError> {
        Ok(matches!(side, Expr::Subquery(_))
            && !query::subquery_node_correlated(side, storage, txid, arena)?)
    };
    let operand = |side: &Expr<'_>| {
        matches!(
            side,
            Expr::Column { .. }
                | Expr::Int(_)
                | Expr::Str(_)
                | Expr::Float(_)
                | Expr::NumericLit(_)
                | Expr::Bool(_)
                // A constant the planner folded, or a generic plan's
                // parameter, which an equivalence class holds as one.
                | Expr::BoundLiteral { .. }
                | Expr::Param(_)
                | Expr::Unary {
                    operator: crate::sql::ast::UnaryOp::Neg,
                    operand: Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_),
                    ..
                }
        )
    };
    let Expr::Binary {
        operator: BinaryOp::Eq,
        left,
        right,
        ..
    } = conjunct
    else {
        return Ok(false);
    };
    Ok((operand(left) || initplan(left)?)
        && (operand(right) || initplan(right)?)
        && (matches!(left, Expr::Column { .. }) || matches!(right, Expr::Column { .. })))
}

/// Whether a one-relation query's `ORDER BY` key is a column one of its
/// top-level `WHERE` equalities fixes to a constant, which makes the key's
/// pathkey redundant.
fn constant_fixed_key<'a>(
    statement: &Select<'a>,
    key: &Expr<'_>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let item = |index: usize| match statement.items.get(index) {
        Some(SelectItem::Expr { expression, .. }) => Some(*expression),
        _ => None,
    };
    let key = match key {
        Expr::Int(position) => usize::try_from(*position)
            .ok()
            .and_then(|position| position.checked_sub(1))
            .and_then(item),
        Expr::Column {
            qualifier: None,
            name,
        } => statement
            .items
            .iter()
            .position(|candidate| {
                matches!(candidate, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
            })
            .and_then(item),
        _ => None,
    }
    .unwrap_or(key);
    let Expr::Column {
        qualifier: key_qualifier,
        name: key_name,
    } = key
    else {
        return Ok(false);
    };
    let same = |side: &Expr<'_>| {
        matches!(side, Expr::Column { qualifier, name }
            if name == key_name
                && (qualifier.is_none() || key_qualifier.is_none() || qualifier == key_qualifier))
    };
    let constant = |side: &Expr<'_>| !matches!(side, Expr::Column { .. } | Expr::Bool(_));
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(condition) = statement.where_clause
        && split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none()
    {
        return Ok(false);
    }
    for conjunct in &conjuncts[..count] {
        if equivalence_clause(conjunct, storage, txid, arena)?
            && matches!(conjunct, Expr::Binary { left, right, .. }
                if same(left) && constant(right) || same(right) && constant(left))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// PostgreSQL's order of a scan's restrictions: the others as written, then
/// the equivalence-class equalities in the order they were written.
fn order_restrictions<'a>(
    conjuncts: &mut [&'a Expr<'a>],
    cost: &dyn Fn(&Expr<'_>) -> f64,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let mut ordered = 0;
    for index in 0..conjuncts.len() {
        if !equivalence_clause(conjuncts[index], storage, txid, arena)? {
            conjuncts[ordered..=index].rotate_right(1);
            ordered += 1;
        }
    }
    // `order_qual_clauses`: a stable insertion sort by per-row cost.
    let mut costs = [0.0f64; MAX_CONJUNCTS];
    for (slot, conjunct) in costs.iter_mut().zip(conjuncts.iter()) {
        *slot = cost(conjunct);
    }
    for index in 1..conjuncts.len().min(MAX_CONJUNCTS) {
        let mut at = index;
        while at > 0 && costs[at - 1] > costs[at] {
            conjuncts.swap(at - 1, at);
            costs.swap(at - 1, at);
            at -= 1;
        }
    }
    Ok(())
}

/// The cost of a conjunct over one table, for [`order_restrictions`].
fn table_qual_cost<'t>(
    storage: &'t Storage,
    txid: u32,
    exposed: &'t str,
    definition: crate::storage::RelationDefinition<'t>,
    aliases: &'t [&'t str],
    arena: &'t Arena,
) -> impl Fn(&Expr<'_>) -> f64 + 't {
    move |conjunct| {
        let columns = crate::sql::exec::AliasedDefCols {
            definition,
            alias: (exposed != definition.name.as_str()).then_some(exposed),
            output_aliases: aliases,
            remote_ctid: false,
        };
        query::qual_cost(conjunct, &columns, storage, txid, arena)
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
    // The one relation a side's columns all read; a key may be any
    // expression of it (`s1.a = (s2.x + 1)`).
    fn relations(
        expression: &Expr<'_>,
        resolve: &dyn Fn(Option<&str>, &str) -> Option<usize>,
    ) -> Option<u64> {
        if expression.contains_subquery() {
            return None;
        }
        if let Expr::Column { qualifier, name } = expression {
            return resolve(*qualifier, name).and_then(|read| 1u64.checked_shl(read as u32));
        }
        let mut mask = Some(0u64);
        let _ = query::walk_children(expression, &mut |child| {
            mask = mask
                .zip(relations(child, resolve))
                .map(|(held, read)| held | read);
            Ok(())
        });
        mask
    }
    let side = |expression: &Expr<'_>, resolve: &dyn Fn(Option<&str>, &str) -> Option<usize>| {
        relations(expression, resolve)
            .filter(|mask| mask.count_ones() == 1)
            .map(|mask| mask.trailing_zeros() as usize)
    };
    match (side(left, resolve), side(right, resolve)) {
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

/// A relation a join reads: a stored table, or a subquery in FROM that
/// PostgreSQL scans with a `Subquery Scan`.
#[derive(Clone, Copy)]
struct JoinRelation<'a> {
    /// The name the query exposes it under.
    exposed: &'a str,
    definition: crate::storage::RelationDefinition<'a>,
    table: Option<&'a dyn crate::sql::catalog::deparse::DeparseTable>,
    columns: crate::sql::catalog::deparse::Columns<'a>,
    /// A subquery's columns as its own plan outputs them. Without a filter
    /// a join input's `Subquery Scan` is trivial and `set_plan_references`
    /// removes it (`trivial_subqueryscan`), so the join's conditions print
    /// the columns its plan outputs.
    subquery: Option<&'a [crate::sql::catalog::deparse::Column<'a>]>,
    /// A partitioned table pruning left no member to scan: the plan reads
    /// none of its range table entries, so EXPLAIN names it nothing and
    /// prints its columns unqualified.
    nameless: bool,
}

impl<'a> JoinRelation<'a> {
    fn table(exposed: &'a str, table: &'a crate::storage::TableDef) -> Self {
        Self {
            exposed,
            definition: crate::storage::RelationDefinition::from_definition(table),
            table: Some(table),
            columns: crate::sql::catalog::deparse::Columns::Table(table),
            subquery: None,
            nameless: false,
        }
    }

    /// A table read under the column aliases `aliases`, when it has them.
    fn aliased(
        exposed: &'a str,
        table: &'a crate::storage::TableDef,
        aliases: Option<&'a [&'a str]>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let Some(aliases) = aliases else {
            return Ok(Self::table(exposed, table));
        };
        let aliased: &'a dyn crate::sql::catalog::deparse::DeparseTable = arena
            .alloc(crate::sql::catalog::deparse::AliasedTable { table, aliases })
            .map_err(|_| arena_full())?;
        Ok(Self {
            table: Some(aliased),
            columns: crate::sql::catalog::deparse::Columns::Table(aliased),
            ..Self::table(exposed, table)
        })
    }

    /// The relation as a deparsed condition names it; `removed` when its
    /// `Subquery Scan` has no filter of its own and so is removed.
    fn joined(&self, removed: bool) -> crate::sql::catalog::deparse::Joined<'a> {
        match (self.table, self.subquery.filter(|_| removed)) {
            (Some(table), _) => crate::sql::catalog::deparse::Joined {
                printed: if self.nameless {
                    ""
                } else {
                    crate::sql::catalog::deparse::printed_name(self.exposed)
                },
                ..crate::sql::catalog::deparse::Joined::new(self.exposed, table)
            },
            (None, outputs) => crate::sql::catalog::deparse::Joined {
                printed: crate::sql::catalog::deparse::printed_name(self.exposed),
                unqualified: !self.exposed.starts_with('\0'),
                ..crate::sql::catalog::deparse::Joined::rule_columns(
                    self.exposed,
                    outputs.map_or(self.columns, crate::sql::catalog::deparse::Columns::Derived),
                )
            },
        }
    }
}

/// `name` made unique among `used`, as `select_rtable_names_for_explain`
/// names each range-table entry: a repeated name takes the first free
/// `_1`, `_2`, ... suffix.
fn unique_rtable_name(name: &str, used: &[StackStr<96>]) -> StackStr<96> {
    let mut unique = StackStr::<96>::from_str(name);
    let mut suffix = 1usize;
    while used.iter().any(|held| held.as_str() == unique.as_str()) {
        unique = StackStr::new();
        let _ = write!(unique, "{name}_{suffix}");
        suffix += 1;
    }
    unique
}

/// The columns of a subquery in FROM as its plan outputs them: each of its
/// output columns that reads a column of its one stored relation prints as
/// that relation's column (`o1.a` for `(SELECT * FROM o1) x`).
fn subquery_outputs<'a>(
    storage: &Storage,
    txid: u32,
    body: &Select<'_>,
    definition: crate::storage::RelationDefinition<'a>,
    used: &mut crate::mem::arena::ArenaList<'a, StackStr<96>>,
    arena: &'a Arena,
) -> Result<&'a [crate::sql::catalog::deparse::Column<'a>], SqlError> {
    let columns = arena
        .alloc_slice_with(definition.n_columns, |index| {
            crate::sql::catalog::deparse::Column {
                name: definition.columns[index].name.as_str(),
                type_oid: None,
                merged: None,
            }
        })
        .map_err(|_| arena_full())?;
    let Some(from) = body.from.as_ref().filter(|from| {
        body.set_body.is_none()
            && from.joins.is_empty()
            && from.base.subquery.is_none()
            && from.base.func_args.is_none()
            && from.base.rows_from.is_none()
            && from.base.cte.is_none()
    }) else {
        return Ok(columns);
    };
    let Some(slot) = relation_slot(storage, txid, from.base.schema, from.base.table) else {
        return Ok(columns);
    };
    let inner = storage.table_def(slot, txid);
    let written = from.base.alias.unwrap_or(from.base.table);
    // The subquery's relation follows the outer query's in the range table.
    let printed = unique_rtable_name(written, used.as_slice());
    used.push(printed).map_err(|_| arena_full())?;
    let printed = arena
        .alloc_str(printed.as_str())
        .map_err(|_| arena_full())?;
    let inner_name = written;
    let mut position = 0;
    let mut output = |name: Option<&str>| -> Result<(), SqlError> {
        if let Some(column) = columns.get_mut(position) {
            if let Some(name) = name {
                column.merged = Some((
                    Some(printed),
                    arena.alloc_str(name).map_err(|_| arena_full())?,
                ));
            }
            position += 1;
        }
        Ok(())
    };
    for item in body.items {
        match item {
            SelectItem::Wildcard(_) => {
                for column in inner.columns() {
                    output(Some(column.name.as_str()))?;
                }
            }
            SelectItem::TableWildcard(name) if *name == inner_name => {
                for column in inner.columns() {
                    output(Some(column.name.as_str()))?;
                }
            }
            SelectItem::Expr {
                expression: Expr::Column { qualifier, name },
                ..
            } if qualifier.is_none_or(|qualifier| qualifier == inner_name)
                && inner.column_index(name).is_some() =>
            {
                output(Some(name))?;
            }
            _ => output(None)?,
        }
    }
    Ok(columns)
}

/// The qualifications PostgreSQL attaches to a left-deep join of relations:
/// each relation's own conjuncts as its scan's filter, and each conjunct
/// reading several relations at the join that adds the last of them, as the
/// join's hash condition (equalities, under a hash join of two relations) or
/// join filter. None when a conjunct cannot be placed.
struct JoinQuals<'a> {
    /// Per relation, in the relations' order.
    filters: &'a mut [Option<StackStr<512>>],
    /// `filters` as `VERBOSE` prints them, columns qualified.
    verbose_filters: &'a mut [Option<StackStr<512>>],
    hash_cond: Option<StackStr<512>>,
    /// Per join, indexed by the depth of the relation it adds (`0` unused).
    join_filters: &'a mut [Option<StackStr<512>>],
}

/// `relations` are named as the query exposes them; `order` lists them in
/// the order the joins add them.
#[allow(clippy::too_many_arguments)]
fn join_quals<'a>(
    storage: &'a Storage,
    txid: u32,
    relations: &[JoinRelation<'a>],
    order: &[usize],
    condition: Option<&'a Expr<'a>>,
    hash: bool,
    indexes: &[Option<query::IndexAccessPlan<'a>>],
    parameterized: &[bool],
    arena: &'a Arena,
) -> Result<Option<JoinQuals<'a>>, SqlError> {
    let count_relations = relations.len();
    if count_relations > 64 || order.len() != count_relations {
        return Ok(None);
    }
    let quals = JoinQuals {
        filters: arena
            .alloc_slice_with(count_relations, |_| None)
            .map_err(|_| arena_full())?,
        verbose_filters: arena
            .alloc_slice_with(count_relations, |_| None)
            .map_err(|_| arena_full())?,
        hash_cond: None,
        join_filters: arena
            .alloc_slice_with(count_relations, |_| None)
            .map_err(|_| arena_full())?,
    };
    let Some(condition) = condition else {
        return Ok(Some(quals));
    };
    let mut quals = quals;
    let resolve =
        |qualifier: Option<&str>, name: &str| resolve_relation(relations, qualifier, name);
    let mut position = [0usize; 64];
    for (depth, &relation) in order.iter().enumerate() {
        position[relation] = depth;
    }
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none() {
        return Ok(None);
    }
    // Each conjunct's relation (a scan filter) or join depth, in order.
    let own = arena
        .alloc_slice_with(count_relations * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let own_count = arena
        .alloc_slice_with(count_relations, |_| 0usize)
        .map_err(|_| arena_full())?;
    // Join conjuncts a parameterized index scan evaluates, per relation.
    let moved = arena
        .alloc_slice_with(count_relations * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let moved_count = arena
        .alloc_slice_with(count_relations, |_| 0usize)
        .map_err(|_| arena_full())?;
    let joins = arena
        .alloc_slice_with(count_relations * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let join_count = arena
        .alloc_slice_with(count_relations, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut hash_conds = [&Expr::Null; MAX_CONJUNCTS];
    let mut hash_count = 0;
    let column_relation = |side: &Expr<'a>| -> Option<(usize, &'a str)> {
        match side {
            Expr::Column { qualifier, name } => {
                resolve(*qualifier, name).map(|relation| (relation, *name))
            }
            _ => None,
        }
    };
    let literal = |side: &'a Expr<'a>| {
        matches!(
            side,
            Expr::Int(_) | Expr::Str(_) | Expr::Float(_) | Expr::NumericLit(_) | Expr::Bool(_)
        )
        .then_some(side)
    };
    // PostgreSQL's equivalence classes: columns equated with each other,
    // directly or through other columns, form one class, and a constant
    // equated with any member fixes every member. Each member's relation is
    // then filtered by the constant and the join equalities are implied.
    let constant_of = |start: (usize, &str)| -> Option<&'a Expr<'a>> {
        let mut members = [(0usize, ""); 2 * MAX_CONJUNCTS];
        members[0] = start;
        let mut known = 1;
        let mut grown = true;
        while grown {
            grown = false;
            for conjunct in &conjuncts[..count] {
                let Expr::Binary {
                    operator: BinaryOp::Eq,
                    left,
                    right,
                    ..
                } = conjunct
                else {
                    continue;
                };
                let (Some(left), Some(right)) = (column_relation(left), column_relation(right))
                else {
                    continue;
                };
                for (from, to) in [(left, right), (right, left)] {
                    if members[..known].contains(&from)
                        && !members[..known].contains(&to)
                        && known < members.len()
                    {
                        members[known] = to;
                        known += 1;
                        grown = true;
                    }
                }
            }
        }
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
            match (column_relation(left), column_relation(right)) {
                (Some(column), None) if members[..known].contains(&column) => literal(right),
                (None, Some(column)) if members[..known].contains(&column) => literal(left),
                _ => None,
            }
        })
    };
    // Columns a scan filter already equates with a constant.
    let mut fixed = [(0usize, ""); 2 * MAX_CONJUNCTS];
    let mut fixed_count = 0;
    for conjunct in &conjuncts[..count] {
        if let Expr::Binary {
            operator: BinaryOp::Eq,
            left,
            right,
            ..
        } = conjunct
        {
            let column = match (column_relation(left), column_relation(right)) {
                (Some(column), None) if literal(right).is_some() => Some(column),
                (None, Some(column)) if literal(left).is_some() => Some(column),
                _ => None,
            };
            if let Some(column) = column
                && fixed_count < fixed.len()
            {
                fixed[fixed_count] = column;
                fixed_count += 1;
            }
        }
    }
    let push = |list: &mut [&'a Expr<'a>], counts: &mut [usize], slot: usize, conjunct| {
        if counts[slot] == MAX_CONJUNCTS {
            return false;
        }
        list[slot * MAX_CONJUNCTS + counts[slot]] = conjunct;
        counts[slot] += 1;
        true
    };
    for &conjunct in &conjuncts[..count] {
        // A scan's index condition, printed with the scan.
        if indexes
            .iter()
            .any(|index| consumed_by(index.as_ref(), conjunct))
        {
            continue;
        }
        let Some(mask) = relation_mask(conjunct, &resolve) else {
            return Ok(None);
        };
        if mask.count_ones() == 1 {
            if !push(own, own_count, mask.trailing_zeros() as usize, conjunct) {
                return Ok(None);
            }
            continue;
        }
        if mask == 0 {
            return Ok(None);
        }
        if let Expr::Binary {
            operator: BinaryOp::Eq,
            left,
            right,
            ..
        } = conjunct
            && let (Some(left_column), Some(right_column)) =
                (column_relation(left), column_relation(right))
            && left_column.0 != right_column.0
            && let Some(constant) = constant_of(left_column)
        {
            for (column, side) in [(left_column, *left), (right_column, *right)] {
                if fixed[..fixed_count].contains(&column) {
                    continue;
                }
                let derived = &*arena
                    .alloc(Expr::Binary {
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                        operator: BinaryOp::Eq,
                        left: side,
                        right: constant,
                    })
                    .map_err(|_| arena_full())?;
                if !push(own, own_count, column.0, derived) || fixed_count == fixed.len() {
                    return Ok(None);
                }
                fixed[fixed_count] = column;
                fixed_count += 1;
            }
            continue;
        }
        let depth = (0..count_relations)
            .filter(|relation| mask & (1 << relation) != 0)
            .map(|relation| position[relation])
            .max()
            .expect("a join conjunct reads relations");
        if hash
            && depth == 1
            && let Some(equality) = equijoin(conjunct, &resolve, order[0], arena)?
        {
            hash_conds[hash_count] = equality;
            hash_count += 1;
            continue;
        }
        let relation = order[depth];
        let placed = if parameterized[relation] {
            push(moved, moved_count, relation, conjunct)
        } else {
            push(joins, join_count, depth, conjunct)
        };
        if !placed {
            return Ok(None);
        }
    }
    if !scan_filters(
        storage,
        txid,
        relations,
        own,
        own_count,
        moved,
        moved_count,
        quals.filters,
        quals.verbose_filters,
        arena,
    )? {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(count_relations, |index| {
            relations[index].joined(quals.filters[index].is_none())
        })
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        bare: None,
    };
    quals.hash_cond = deparse_qual(&context, &hash_conds[..hash_count], arena)?;
    if hash_count != 0 && quals.hash_cond.is_none() {
        return Ok(None);
    }
    for depth in 1..count_relations {
        let filters = &joins[depth * MAX_CONJUNCTS..depth * MAX_CONJUNCTS + join_count[depth]];
        quals.join_filters[depth] = deparse_qual(&context, filters, arena)?;
        if !filters.is_empty() && quals.join_filters[depth].is_none() {
            return Ok(None);
        }
    }
    Ok(Some(quals))
}

/// The qualifications of an outer hash join of two relations: the
/// conditions execution moves into each scan (`pushed`, by FROM position),
/// the equalities it hashes, the rest of its `ON` condition as its join
/// filter and the rest of the `WHERE` condition as the filter of its output.
struct OuterHashQuals {
    scan_filters: [Option<StackStr<512>>; 2],
    verbose_scan_filters: [Option<StackStr<512>>; 2],
    hash_cond: StackStr<512>,
    join_filter: Option<StackStr<512>>,
    output_filter: Option<StackStr<512>>,
}

#[allow(clippy::too_many_arguments)]
fn outer_hash_quals<'a>(
    storage: &Storage,
    txid: u32,
    relations: &[JoinRelation<'a>],
    probe: usize,
    condition: &'a Expr<'a>,
    where_clause: Option<&'a Expr<'a>>,
    pushed: [&[&'a Expr<'a>]; 2],
    arena: &'a Arena,
) -> Result<Option<OuterHashQuals>, SqlError> {
    let resolve =
        |qualifier: Option<&str>, name: &str| resolve_relation(relations, qualifier, name);
    let is_pushed = |conjunct: &Expr<'_>| {
        pushed
            .iter()
            .any(|side| side.iter().any(|held| core::ptr::eq(*held, conjunct)))
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none() {
        return Ok(None);
    }
    let mut hash_conds = [&Expr::Null; MAX_CONJUNCTS];
    let mut hash_count = 0;
    let mut filters = [&Expr::Null; MAX_CONJUNCTS];
    let mut filter_count = 0;
    for &conjunct in &conjuncts[..count] {
        if is_pushed(conjunct) {
            continue;
        }
        if let Some(equality) = equijoin(conjunct, &resolve, probe, arena)? {
            hash_conds[hash_count] = equality;
            hash_count += 1;
        } else {
            filters[filter_count] = conjunct;
            filter_count += 1;
        }
    }
    let mut rest = [&Expr::Null; MAX_CONJUNCTS];
    let mut rest_count = 0;
    if let Some(where_clause) = where_clause {
        let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
        let mut count = 0;
        if split_conjuncts(where_clause, &mut conjuncts, &mut count, arena).is_none() {
            return Ok(None);
        }
        for &conjunct in &conjuncts[..count] {
            if !is_pushed(conjunct) {
                rest[rest_count] = conjunct;
                rest_count += 1;
            }
        }
    }
    let own = arena
        .alloc_slice_with(2 * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let own_count = arena
        .alloc_slice_with(2, |index| pushed[index].len())
        .map_err(|_| arena_full())?;
    for (index, side) in pushed.iter().enumerate() {
        if side.len() > MAX_CONJUNCTS {
            return Ok(None);
        }
        own[index * MAX_CONJUNCTS..index * MAX_CONJUNCTS + side.len()].copy_from_slice(side);
    }
    let moved = arena
        .alloc_slice_with(2 * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let moved_count = arena
        .alloc_slice_with(2, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut scan_filters = [None, None];
    let mut verbose_scan_filters = [None, None];
    if !self::scan_filters(
        storage,
        txid,
        relations,
        own,
        own_count,
        moved,
        moved_count,
        &mut scan_filters,
        &mut verbose_scan_filters,
        arena,
    )? {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(relations.len(), |index| {
            relations[index].joined(scan_filters[index].is_none())
        })
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
    };
    let Some(hash_cond) = deparse_qual(&context, &hash_conds[..hash_count], arena)? else {
        return Ok(None);
    };
    let join_filter = deparse_qual(&context, &filters[..filter_count], arena)?;
    let output_filter = deparse_qual(&context, &rest[..rest_count], arena)?;
    if (filter_count != 0 && join_filter.is_none()) || (rest_count != 0 && output_filter.is_none())
    {
        return Ok(None);
    }
    Ok(Some(OuterHashQuals {
        scan_filters,
        verbose_scan_filters,
        hash_cond,
        join_filter,
        output_filter,
    }))
}

/// A FROM-less query's `WHERE` condition, which PostgreSQL checks once as
/// its `Result`'s one-time filter.
fn one_time_filter(
    storage: &Storage,
    txid: u32,
    condition: Option<&Expr<'_>>,
    arena: &Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    let Some(condition) = condition else {
        return Ok(None);
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none() {
        return Ok(None);
    }
    // The planner folds a constant conjunct; one that holds disappears.
    let mut kept = 0;
    for index in 0..count {
        if query::plan_time_bool(conjuncts[index], arena) != Some(true) {
            conjuncts[kept] = conjuncts[index];
            kept += 1;
        }
    }
    let count = kept;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined: &[],
        pretty: false,
        qualify: false,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        arena,
        dependencies: None,
    };
    deparse_qual(&context, &conjuncts[..count], arena)
}

/// Each relation's scan filter: its own conjuncts in PostgreSQL's order,
/// then the join conjuncts a parameterized index scan of it evaluates,
/// which name the other relations' columns qualified. False when one
/// cannot be printed.
#[allow(clippy::too_many_arguments)]
fn scan_filters<'a>(
    storage: &'a Storage,
    txid: u32,
    relations: &[JoinRelation<'a>],
    own: &mut [&'a Expr<'a>],
    own_count: &[usize],
    moved: &[&'a Expr<'a>],
    moved_count: &[usize],
    filters: &mut [Option<StackStr<512>>],
    verbose_filters: &mut [Option<StackStr<512>>],
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    for (index, relation) in relations.iter().enumerate() {
        let exposed = relation.exposed;
        let own = &mut own[index * MAX_CONJUNCTS..index * MAX_CONJUNCTS + own_count[index]];
        order_restrictions(
            own,
            &table_qual_cost(storage, txid, exposed, relation.definition, &[], arena),
            storage,
            txid,
            arena,
        )?;
        let moved = &moved[index * MAX_CONJUNCTS..index * MAX_CONJUNCTS + moved_count[index]];
        if moved.is_empty() {
            let joined = [relation.joined(false)];
            for (verbose, slot) in [
                (false, &mut filters[index]),
                (true, &mut verbose_filters[index]),
            ] {
                let context = crate::sql::catalog::deparse::Context {
                    storage,
                    txid,
                    table: relation
                        .table
                        .map(|table| table as &dyn crate::sql::catalog::deparse::DeparseTable),
                    parameters: &[],
                    joined: &joined,
                    pretty: false,
                    // A `Subquery Scan` qualifies its filter's columns, as
                    // `VERBOSE` does every scan's (`show_scan_qual`).
                    qualify: verbose || relation.subquery.is_some(),
                    bare: None,
                    domain_value: None,
                    sublinks: Some(&registered_sublink),
                    references: None,
                    folded: true,
                    arena,
                    dependencies: None,
                };
                *slot = deparse_qual(&context, own, arena)?;
            }
        } else {
            let all = arena
                .alloc_slice_with(own.len() + moved.len(), |position| {
                    own.get(position)
                        .copied()
                        .unwrap_or_else(|| moved[position - own.len()])
                })
                .map_err(|_| arena_full())?;
            let joined = arena
                .alloc_slice_with(relations.len(), |index| relations[index].joined(false))
                .map_err(|_| arena_full())?;
            let context = crate::sql::catalog::deparse::Context {
                storage,
                txid,
                table: None,
                parameters: &[],
                joined,
                pretty: false,
                qualify: true,
                bare: Some(exposed),
                domain_value: None,
                sublinks: Some(&registered_sublink),
                references: None,
                folded: true,
                arena,
                dependencies: None,
            };
            filters[index] = deparse_qual(&context, all, arena)?;
            let context = crate::sql::catalog::deparse::Context {
                bare: None,
                ..context
            };
            verbose_filters[index] = deparse_qual(&context, all, arena)?;
        }
        if (!own.is_empty() || !moved.is_empty()) && filters[index].is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The relation of `relations` a column reference names, when exactly one
/// has it.
fn resolve_relation(
    relations: &[JoinRelation<'_>],
    qualifier: Option<&str>,
    name: &str,
) -> Option<usize> {
    let has = |relation: &JoinRelation<'_>| relation.columns.position(name).is_some();
    match qualifier {
        Some(qualifier) => relations
            .iter()
            .position(|relation| relation.exposed == qualifier && has(relation)),
        // A pulled-up subquery's relation answers only qualified names.
        None => {
            let mut found = relations
                .iter()
                .enumerate()
                .filter(|(_, relation)| !relation.exposed.starts_with('\0') && has(relation))
                .map(|(index, _)| index);
            match (found.next(), found.next()) {
                (Some(index), None) => Some(index),
                _ => None,
            }
        }
    }
}

/// The qualifications PostgreSQL attaches to relations joined in FROM order
/// by outer joins (and inner joins with `ON`), relation `k` added by
/// `joins[k - 1]`. An `ON` conjunct reading only a join's nullable input is
/// filtered below the join; one reading only rows the join itself nulls,
/// or both inputs, stays on the join as its hash condition or join filter.
/// A `WHERE` conjunct reading a relation an outer join nulls filters the
/// join's output (`Filter`) at the join that nulls it. None when a conjunct
/// cannot be placed.
#[allow(clippy::too_many_arguments)]
fn outer_join_quals<'a>(
    storage: &'a Storage,
    txid: u32,
    relations: &[JoinRelation<'a>],
    joins: &'a [crate::sql::ast::Join<'a>],
    join_on: &[Option<&'a Expr<'a>>],
    condition: Option<&'a Expr<'a>>,
    hash: bool,
    indexes: &[Option<query::IndexAccessPlan<'a>>],
    parameterized: &[bool],
    arena: &'a Arena,
) -> Result<Option<OuterJoinQuals<'a>>, SqlError> {
    use crate::sql::ast::JoinKind;
    let count = relations.len();
    if count > 64 || joins.len() + 1 != count {
        return Ok(None);
    }
    let resolve =
        |qualifier: Option<&str>, name: &str| resolve_relation(relations, qualifier, name);
    // A relation is nulled by a later join's outer side.
    let nulls = |join: usize, relation: usize| match joins[join - 1].kind {
        JoinKind::Left => relation == join,
        JoinKind::Right => relation < join,
        JoinKind::Full => relation <= join,
        _ => false,
    };
    let nulled_below =
        |relation: usize, depth: usize| (1..depth.min(count)).any(|join| nulls(join, relation));
    let nulled_at = |relation: usize| {
        (1..count)
            .filter(|&join| nulls(join, relation))
            .max()
            .unwrap_or(0)
    };
    let own = arena
        .alloc_slice_with(count * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let own_count = arena
        .alloc_slice_with(count, |_| 0usize)
        .map_err(|_| arena_full())?;
    let moved = arena
        .alloc_slice_with(count * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let moved_count = arena
        .alloc_slice_with(count, |_| 0usize)
        .map_err(|_| arena_full())?;
    // Per join: its join filter and the filter of its output.
    let join_list = arena
        .alloc_slice_with(count * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let join_count = arena
        .alloc_slice_with(count, |_| 0usize)
        .map_err(|_| arena_full())?;
    let post_list = arena
        .alloc_slice_with(count * MAX_CONJUNCTS, |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    let post_count = arena
        .alloc_slice_with(count, |_| 0usize)
        .map_err(|_| arena_full())?;
    let mut hash_conds = [&Expr::Null; MAX_CONJUNCTS];
    let mut hash_count = 0;
    let push = |list: &mut [&'a Expr<'a>], counts: &mut [usize], slot: usize, conjunct| {
        if counts[slot] == MAX_CONJUNCTS {
            return false;
        }
        list[slot * MAX_CONJUNCTS + counts[slot]] = conjunct;
        counts[slot] += 1;
        true
    };
    // `ON` conjuncts of each join, then the `WHERE` conjuncts (depth 0).
    for depth in (1..count).chain(core::iter::once(0)) {
        // A `USING` or `NATURAL` join's condition is the one analysis
        // synthesized.
        let source = if depth == 0 {
            condition
        } else {
            joins[depth - 1].on.or(join_on[depth - 1])
        };
        let Some(source) = source else {
            continue;
        };
        let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
        let mut conjunct_count = 0;
        if split_conjuncts(source, &mut conjuncts, &mut conjunct_count, arena).is_none() {
            return Ok(None);
        }
        for &conjunct in &conjuncts[..conjunct_count] {
            if indexes
                .iter()
                .any(|index| consumed_by(index.as_ref(), conjunct))
            {
                continue;
            }
            let Some(mask) = relation_mask(conjunct, &resolve) else {
                return Ok(None);
            };
            if mask == 0 {
                return Ok(None);
            }
            let highest = 63 - mask.leading_zeros() as usize;
            let single = (mask.count_ones() == 1).then_some(highest);
            let placed = if depth == 0 {
                let nulled = (0..count)
                    .filter(|relation| mask & (1 << relation) != 0)
                    .map(nulled_at)
                    .max()
                    .unwrap_or(0);
                if nulled == 0 {
                    match single {
                        Some(relation) => push(own, own_count, relation, conjunct),
                        None if joins[highest - 1].kind.is_cross()
                            || joins[highest - 1].kind == JoinKind::Inner =>
                        {
                            push(join_list, join_count, highest, conjunct)
                        }
                        None => push(post_list, post_count, highest, conjunct),
                    }
                } else {
                    push(post_list, post_count, highest.max(nulled), conjunct)
                }
            } else {
                if highest > depth {
                    return Ok(None);
                }
                let kind = joins[depth - 1].kind;
                let pushed_down = match (kind, single) {
                    (JoinKind::Left | JoinKind::Semi | JoinKind::Anti, Some(relation)) => {
                        relation == depth
                    }
                    (JoinKind::Right | JoinKind::Inner, Some(relation)) => {
                        (kind == JoinKind::Inner || relation < depth)
                            && !nulled_below(relation, depth)
                    }
                    _ => false,
                };
                if pushed_down {
                    push(own, own_count, highest, conjunct)
                } else if hash
                    && depth == 1
                    && let Some(equality) = equijoin(conjunct, &resolve, 0, arena)?
                {
                    hash_conds[hash_count] = equality;
                    hash_count += 1;
                    true
                } else if parameterized[depth] && mask & (1 << depth) != 0 {
                    push(moved, moved_count, depth, conjunct)
                } else {
                    push(join_list, join_count, depth, conjunct)
                }
            };
            if !placed {
                return Ok(None);
            }
        }
    }
    let slots = || {
        arena
            .alloc_slice_with(count, |_| None::<StackStr<512>>)
            .map_err(|_| arena_full())
    };
    let mut quals = OuterJoinQuals {
        filters: slots()?,
        verbose_filters: slots()?,
        hash_cond: None,
        join_filters: slots()?,
        post_filters: slots()?,
    };
    if !scan_filters(
        storage,
        txid,
        relations,
        own,
        own_count,
        moved,
        moved_count,
        quals.filters,
        quals.verbose_filters,
        arena,
    )? {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(count, |index| {
            relations[index].joined(quals.filters[index].is_none())
        })
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        arena,
        dependencies: None,
    };
    quals.hash_cond = deparse_qual(&context, &hash_conds[..hash_count], arena)?;
    if hash_count != 0 && quals.hash_cond.is_none() {
        return Ok(None);
    }
    for depth in 1..count {
        for (list, counts, target) in [
            (&*join_list, &*join_count, &mut quals.join_filters[depth]),
            (&*post_list, &*post_count, &mut quals.post_filters[depth]),
        ] {
            let conjuncts = &list[depth * MAX_CONJUNCTS..depth * MAX_CONJUNCTS + counts[depth]];
            *target = deparse_qual(&context, conjuncts, arena)?;
            if !conjuncts.is_empty() && target.is_none() {
                return Ok(None);
            }
        }
    }
    Ok(Some(quals))
}

/// Each scan's index condition, with columns qualified by the relations
/// joined.
fn joined_index_conditions<'a>(
    storage: &Storage,
    txid: u32,
    relations: &[JoinRelation<'a>],
    indexes: &[Option<query::IndexAccessPlan<'a>>],
    conditions: &mut [Option<StackStr<512>>],
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let joined = arena
        .alloc_slice_with(relations.len(), |index| relations[index].joined(true))
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        arena,
        dependencies: None,
    };
    for ((index, condition), relation) in indexes.iter().zip(conditions.iter_mut()).zip(relations) {
        if let Some(index) = index {
            *condition = index_condition(&context, relation.exposed, index, false, arena)?;
        }
    }
    Ok(())
}

/// [`outer_join_quals`]' placement, per relation and per join depth.
struct OuterJoinQuals<'a> {
    filters: &'a mut [Option<StackStr<512>>],
    verbose_filters: &'a mut [Option<StackStr<512>>],
    hash_cond: Option<StackStr<512>>,
    join_filters: &'a mut [Option<StackStr<512>>],
    post_filters: &'a mut [Option<StackStr<512>>],
}

/// The index condition and filter of a scan of one relation reading
/// `condition`, when every conjunct reads only that relation; the conjuncts
/// `index` consumes form the index condition instead of the filter.
/// The `Filter` of a grouped query's aggregation: its `HAVING`, over stored
/// relations, qualified when the query reads several.
fn having_filter<'a>(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    having: Option<&'a Expr<'a>>,
    verbose: bool,
    arena: &'a Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    let Some(having) = having else {
        return Ok(None);
    };
    // A query without FROM reads no relation.
    let relations = scope.map_or(0, |scope| scope.n);
    if scope.is_some_and(|scope| (0..scope.n).any(|table| scope.defs.stored(table).is_none())) {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(relations, |table| {
            let scope = scope.expect("relations come from the scope");
            crate::sql::catalog::deparse::Joined::new(
                scope.names[table],
                scope.defs.stored(table).expect("stored relation"),
            )
        })
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        // `show_upper_qual` qualifies columns under `VERBOSE`.
        qualify: verbose || relations > 1,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        bare: None,
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(having, &mut conjuncts, &mut count, arena).is_none() {
        return Ok(None);
    }
    deparse_qual(&context, &conjuncts[..count], arena)
}

/// How PostgreSQL names a derived relation's columns in its scan's quals.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DerivedNames {
    /// A function's: bare, as the only range table entry.
    Bare,
    /// A subquery's, whose query adds range table entries of its own.
    Qualified,
    /// A pulled-up `VALUES` list's: `column1`, ...
    Values,
}

/// The `Filter` of the scan of a derived relation (a subquery, function or
/// `VALUES` list) read alone: `condition`, when it reads only that relation.
#[allow(clippy::too_many_arguments)]
fn derived_relation_filter<'a>(
    storage: &Storage,
    txid: u32,
    exposed: &'a str,
    definition: crate::storage::RelationDefinition<'_>,
    names: DerivedNames,
    condition: Option<&'a Expr<'a>>,
    verbose: bool,
    arena: &'a Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    let Some(condition) = condition else {
        return Ok(None);
    };
    let columns = arena
        .alloc_slice_with(definition.n_columns, |index| {
            crate::sql::catalog::deparse::Column {
                name: definition.columns[index].name.as_str(),
                type_oid: None,
                merged: None,
            }
        })
        .map_err(|_| arena_full())?;
    if names == DerivedNames::Values {
        for (index, column) in columns.iter_mut().enumerate() {
            let printed = arena
                .alloc_str_display(format_args!("column{}", index + 1))
                .map_err(|_| arena_full())?;
            // `VERBOSE` qualifies by the list's own name.
            column.merged = Some((verbose.then_some("*VALUES*"), printed));
        }
    }
    let resolve = |qualifier: Option<&str>, name: &str| -> Option<usize> {
        (qualifier.is_none_or(|qualifier| qualifier == exposed)
            && columns.iter().any(|column| column.name == name))
        .then_some(0)
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none()
        || conjuncts[..count]
            .iter()
            .any(|conjunct| relation_mask(conjunct, &resolve).is_none())
    {
        return Ok(None);
    }
    order_restrictions(
        &mut conjuncts[..count],
        &|conjunct| query::qual_cost(conjunct, &crate::sql::exec::NoCols, storage, txid, arena),
        storage,
        txid,
        arena,
    )?;
    let joined = [crate::sql::catalog::deparse::Joined::rule_columns(
        exposed,
        crate::sql::catalog::deparse::Columns::Derived(columns),
    )];
    let joined = [crate::sql::catalog::deparse::Joined {
        unqualified: true,
        ..joined[0]
    }];
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: None,
        parameters: &[],
        joined: &joined,
        pretty: false,
        qualify: verbose || names == DerivedNames::Qualified,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        bare: None,
    };
    deparse_qual(&context, &conjuncts[..count], arena)
}

/// The row-security quals a query reading the one relation of `scope`
/// adds to its scan, as the role the scope reads it as.
fn select_security_quals<'a>(
    storage: &Storage,
    scope: &query::QueryScope<'_>,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let Some(current) = storage.current_role_slot(txid) else {
        return Ok(&[]);
    };
    let role = scope.authorization_roles[0].map_or(current, usize::from);
    match query::plan_row_security(
        storage,
        scope.slots[0],
        role,
        crate::storage::PolicyCommandKind::Select,
        query::RowSecurityExpression::Using,
        txid,
        arena,
    )? {
        Some(plan) => plan.security_quals(arena),
        None => Ok(&[]),
    }
}

/// The row-security quals an `UPDATE` or `DELETE` adds to its target's scan:
/// its own command's policies, then, when it reads the target's columns, the
/// `SELECT` policies, as PostgreSQL's `get_row_security_policies` lists them.
fn write_security_quals<'a>(
    storage: &Storage,
    statement: &Stmt<'_>,
    slot: usize,
    definition: &crate::storage::TableDef,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let (command, alias, condition, assignments, returning) = match statement {
        Stmt::Update(update) => (
            crate::storage::PolicyCommandKind::Update,
            update.alias,
            update.where_clause,
            update.assignments,
            update.returning,
        ),
        Stmt::Delete(delete) => (
            crate::storage::PolicyCommandKind::Delete,
            delete.alias,
            delete.where_clause,
            &[][..],
            delete.returning,
        ),
        _ => return Ok(&[]),
    };
    let Some(role) = storage.current_role_slot(txid) else {
        return Ok(&[]);
    };
    let policies = |command| {
        query::plan_row_security(
            storage,
            slot,
            role,
            command,
            query::RowSecurityExpression::Using,
            txid,
            arena,
        )
    };
    let own = policies(command)?;
    let reads = crate::sql::exec::write_target_reads(
        definition,
        alias,
        condition,
        assignments,
        returning,
        storage,
        txid,
        arena,
    )?;
    let select = if reads.is_empty() {
        None
    } else {
        policies(crate::storage::PolicyCommandKind::Select)?
    };
    match query::conjoin_row_security(own, select) {
        Some(plan) => plan.security_quals(arena),
        None => Ok(&[]),
    }
}

#[allow(clippy::too_many_arguments)]
fn single_relation_quals<'a>(
    storage: &Storage,
    txid: u32,
    exposed: &'a str,
    table: &'a crate::storage::TableDef,
    aliases: &'a [&'a str],
    condition: Option<&'a Expr<'a>>,
    security: &[&'a Expr<'a>],
    index: Option<&query::IndexAccessPlan<'a>>,
    verbose: bool,
    arena: &'a Arena,
) -> Result<(Option<StackStr<512>>, Option<StackStr<512>>), SqlError> {
    if condition.is_none() && security.is_empty() {
        return Ok((None, None));
    }
    // The relation's columns under the column aliases it is read with.
    let aliased = crate::sql::catalog::deparse::AliasedTable { table, aliases };
    let resolve = |qualifier: Option<&str>, name: &str| -> Option<usize> {
        use crate::sql::catalog::deparse::DeparseTable;
        if qualifier.is_none_or(|qualifier| qualifier == exposed)
            && aliased.column_index(name).is_some()
        {
            return Some(0);
        }
        // A correlated subquery's condition reads its enclosing query's
        // relation as a parameter.
        let mut outer = false;
        outer_relations(storage, txid, |relation, definition| {
            outer |= qualifier.is_some_and(|qualifier| qualifier == relation)
                && definition
                    .columns()
                    .iter()
                    .any(|column| column.name.as_str() == name);
        });
        outer.then_some(1)
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    // The one relation's scan evaluates its subplans, the ones it runs per
    // row included.
    let placeable = |conjunct: &Expr<'_>| {
        if !conjunct.contains_subquery() {
            return relation_mask(conjunct, &resolve).is_some();
        }
        let mut columns = true;
        conjunct.for_each_column_reference(&mut |qualifier, name| {
            columns &= resolve(qualifier, name).is_some();
        });
        columns && sublinks_registered(conjunct, true)
    };
    if let Some(condition) = condition
        && (split_conjuncts(condition, &mut conjuncts, &mut count, arena).is_none()
            || !conjuncts[..count]
                .iter()
                .all(|conjunct| placeable(conjunct)))
    {
        return Ok((None, None));
    }
    let mut kept = 0;
    for position in 0..count {
        if !consumed_by(index, conjuncts[position]) {
            conjuncts[kept] = conjuncts[position];
            kept += 1;
        }
    }
    let cost = table_qual_cost(
        storage,
        txid,
        exposed,
        crate::storage::RelationDefinition::from_definition(table),
        aliases,
        arena,
    );
    order_restrictions(&mut conjuncts[..kept], &cost, storage, txid, arena)?;
    let conjuncts = if security.is_empty() {
        &conjuncts[..kept]
    } else {
        // `order_qual_clauses` under row security: each security qual has
        // its own level below the statement's conditions, except that any
        // cheap leakproof qual takes level zero; sorted stably by level,
        // then cost.
        let columns = crate::sql::exec::AliasedDefCols {
            definition: crate::storage::RelationDefinition::from_definition(table),
            alias: (exposed != table.name.as_str()).then_some(exposed),
            output_aliases: aliases,
            remote_ctid: false,
        };
        let ordered = arena
            .alloc_slice_with(security.len() + kept, |position| {
                let (level, qual) = match security.get(position) {
                    Some(qual) => (position, *qual),
                    None => (security.len(), conjuncts[position - security.len()]),
                };
                let level = if query::leakproof_at_first_security_level(
                    qual, &columns, storage, txid, arena,
                ) {
                    0
                } else {
                    level
                };
                (level, cost(qual), qual)
            })
            .map_err(|_| arena_full())?;
        for index in 1..ordered.len() {
            let mut at = index;
            while at > 0 && (ordered[at - 1].0, ordered[at - 1].1) > (ordered[at].0, ordered[at].1)
            {
                ordered.swap(at - 1, at);
                at -= 1;
            }
        }
        &*arena
            .alloc_slice_with(ordered.len(), |position| ordered[position].2)
            .map_err(|_| arena_full())?
    };
    // The enclosing query's relation, or the relations a join's
    // parameterized scan reads, whose columns print qualified.
    let (outer, outer_count) = OUTER_RELATIONS.with(|outer| {
        let outer = outer.borrow();
        let count = JOIN_OUTER_RELATIONS
            .with(core::cell::Cell::get)
            .max(1)
            .min(outer.1);
        let mut taken = [(StackStr::<64>::new(), 0usize); 8];
        for (at, held) in taken.iter_mut().enumerate().take(count) {
            *held = outer.0[outer.1 - count + at];
        }
        (taken, count)
    });
    let outer_definitions: [_; 8] =
        core::array::from_fn(|at| (at < outer_count).then(|| storage.table_def(outer[at].1, txid)));
    let mut outer_names = [""; 8];
    for (name, (held, _)) in outer_names.iter_mut().zip(&outer).take(outer_count) {
        *name = arena.alloc_str(held.as_str()).map_err(|_| arena_full())?;
    }
    let own = crate::sql::catalog::deparse::Joined::new(exposed, &aliased);
    let mut with_outer = [own; 9];
    for at in 0..outer_count {
        if let Some(definition) = outer_definitions[at].as_deref() {
            with_outer[at + 1] =
                crate::sql::catalog::deparse::Joined::new(outer_names[at], definition);
        }
    }
    let joined: &[crate::sql::catalog::deparse::Joined<'_>] = &with_outer[..=outer_count];
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: Some(&aliased),
        parameters: &[],
        joined,
        pretty: false,
        qualify: verbose,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        // Its own columns print bare, a pulled-up subquery's relation
        // under its hidden name included.
        bare: (outer_count != 0 && !verbose)
            .then_some(crate::sql::catalog::deparse::printed_name(exposed)),
    };
    let index_cond = match index {
        Some(index) => index_condition(&context, exposed, index, verbose, arena)?,
        None => None,
    };
    Ok((index_cond, deparse_qual(&context, conjuncts, arena)?))
}

/// Pushes `scan`; a bitmap heap scan rechecks the index condition its
/// bitmap index scan child evaluates.
/// The members a FROM item's partitioned table or inheritance tree scan
/// reads: those planning keeps, and the ones executor startup keeps of them.
#[derive(Clone, Copy)]
struct Expansion<'a> {
    planned: query::partition_prune::Members<'a>,
    started: &'a [usize],
    /// Each foreign member ships the scan's conditions to its server
    /// whole, leaving it no filter, except those in `foreign_local`, which
    /// ship none.
    foreign_shipped: bool,
    foreign_local: &'a [usize],
}

/// Rejects members that have an index on a column in `read`: PostgreSQL
/// scans each partition by its own indexes as their costs choose, which
/// this plan does not model.
fn reject_member_indexes(
    storage: &Storage,
    txid: u32,
    members: &[usize],
    read: &dyn Fn(&str) -> bool,
) -> Result<(), SqlError> {
    for &member in members {
        let definition = storage.table_def(member, txid);
        for binding in 0..storage.value_binding_count(member) {
            let (columns, count) = storage.value_binding_columns(member, binding);
            let leading = (count != 0)
                .then(|| definition.columns().get(usize::from(columns[0])))
                .flatten();
            if leading.is_none_or(|column| read(column.name.as_str())) {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN of a partition or inheritance child scanned by its index is not supported"
                ));
            }
        }
    }
    Ok(())
}

/// Whether `expressions` read the column `name` of `relation`: a condition,
/// key or aggregate argument an index of the relation could serve.
fn reads_column<'e>(
    expressions: impl Iterator<Item = &'e Expr<'e>>,
    relation: &dyn Fn(Option<&str>, &str) -> bool,
    name: &str,
) -> bool {
    let mut found = false;
    for expression in expressions {
        let _ = query::walk_expression_tree(expression, &mut |node| {
            if let Expr::Column {
                qualifier,
                name: column,
            } = node
                && *column == name
                && relation(*qualifier, column)
            {
                found = true;
            }
        });
    }
    found
}

/// Whether the planner's path model chooses the scan of the one relation
/// `statement` reads: no join, grouping, `DISTINCT`, window or
/// set-returning select list above it.
fn single_relation_planned(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    scope: &QueryScope<'_>,
) -> bool {
    statement.from.as_ref().is_some_and(|from| {
        scope.n == 1
            && from.joins.is_empty()
            && from.base.sample.is_none()
            && statement.group_by.is_empty()
            && !statement.distinct
            && statement.distinct_on.is_empty()
            && !has_window(statement)
            && !query::has_set_returning_items(statement.items, storage, txid)
    })
}

/// Per FROM position of `scope`, the members its scan reads when it is a
/// partitioned table or an inheritance parent with children.
fn relation_expansions<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    statement: &'a Select<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    arena: &'a Arena,
) -> Result<&'a [Option<Expansion<'a>>], SqlError> {
    let where_clause = statement.where_clause;
    use query::partition_prune::{PruneStage, ScanValues, expands, members, scan_conditions};
    let expansions = arena
        .alloc_slice_with(scope.n, |_| None)
        .map_err(|_| arena_full())?;
    let items = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
    for (table, item) in items.enumerate().take(scope.n) {
        let slot = scope.slots[table];
        if scope.derived[table].is_some()
            || slot == usize::MAX
            || item.cte.is_some()
            || item.subquery.is_some()
            || !expands(storage, slot, txid)
        {
            continue;
        }
        let partitioned = storage.table_def(slot, txid).partition.is_partitioned();
        if item.inheritance == crate::sql::ast::RelationInheritance::Only {
            // `ONLY` of a partitioned table reads no rows; of an inheritance
            // parent, its own.
            if partitioned {
                expansions[table] = Some(Expansion {
                    planned: query::partition_prune::Members {
                        leaves: &[],
                        expanded: 0,
                        scan_pruned: false,
                    },
                    started: &[],
                    foreign_shipped: false,
                    foreign_local: &[],
                });
            }
            continue;
        }
        let conditions = scan_conditions(from, where_clause, table, arena)?;
        let parameters = crate::sql::eval::generic_plan_parameters();
        let values = ScanValues {
            storage,
            txid,
            relation: query::partition_prune::ScanRelation::From { scope, table },
            arena,
            params: parameters,
            hooks: &crate::sql::eval::NO_HOOKS,
            outer: None,
        };
        let planned = members(
            storage,
            txid,
            slot,
            conditions,
            &values,
            PruneStage::Plan,
            arena,
        )?;
        // A single member replaces the `Append`, which then prunes nothing.
        let started = if planned.leaves.len() >= 2 {
            let kept = members(
                storage,
                txid,
                slot,
                conditions,
                &values,
                PruneStage::Startup,
                arena,
            )?;
            let started = arena
                .alloc_slice_with(planned.leaves.len(), |_| 0usize)
                .map_err(|_| arena_full())?;
            let mut count = 0;
            for &leaf in planned.leaves {
                if kept.leaves.contains(&leaf) {
                    started[count] = leaf;
                    count += 1;
                }
            }
            &started[..count]
        } else {
            planned.leaves
        };
        // An index serves a condition, a sort or grouping key, or an
        // aggregate's argument.
        let own = |qualifier: Option<&str>, name: &str| {
            matches!(
                scope.find_column_unsuggested(qualifier, name),
                Ok(query::ResolvedColumn::Table(found, _)) if found == table
            )
        };
        let indexed = |name: &str| {
            let mut aggregated = false;
            for item in statement.items {
                if let SelectItem::Expr { expression, .. } = item {
                    let _ = query::walk_expression_tree(expression, &mut |node| {
                        if node.is_aggregate_use()
                            && let Expr::Call { args, .. } = node
                        {
                            aggregated |= reads_column(args.iter().copied(), &own, name);
                        }
                    });
                }
            }
            let read = where_clause
                .into_iter()
                .chain(from.joins.iter().filter_map(|join| join.on))
                .chain(statement.order_by.iter().map(|key| key.expression))
                .chain(statement.group_by.iter().copied())
                .chain(statement.distinct_on.iter().copied())
                .chain(statement.having);
            aggregated || reads_column(read, &own, name)
        };
        // The planner chooses each member's path of a query reading the
        // tree alone.
        if !single_relation_planned(storage, txid, statement, scope) {
            reject_member_indexes(storage, txid, planned.leaves, &indexed)?;
        }
        // postgres_fdw plans a foreign member as a foreign table scanned on
        // its own; a condition it keeps local is outside this plan.
        let own = query::partition_prune::own_restrictions(scope, conditions, table, arena)?;
        let mut local = crate::mem::arena::ArenaList::new(arena);
        for &leaf in planned.leaves {
            if storage.table_def(leaf, txid).kind != crate::storage::TableKind::Foreign
                || own.is_empty()
            {
                continue;
            }
            let plan = crate::sql::foreign::deparse::member_scan(
                storage,
                txid,
                item,
                leaf,
                scope.names[table],
                own,
                arena,
            )?;
            let kept = match plan {
                None => own.len(),
                Some(plan) => match plan.local_where {
                    None => 0,
                    Some(kept) => crate::sql::foreign::deparse::conjuncts(Some(kept), arena)?.len(),
                },
            };
            if kept == own.len() {
                local.push(leaf).map_err(|_| arena_full())?;
            } else if kept != 0 {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN of a foreign partition or inheritance child shipping some of its conditions is not supported"
                ));
            }
        }
        if planned.expanded != 0 {
            EXPANDED_RELATIONS.with(|expanded| expanded.set(true));
        }
        expansions[table] = Some(Expansion {
            planned,
            started,
            foreign_shipped: true,
            foreign_local: local.as_slice(),
        });
    }
    Ok(expansions)
}

/// The names a statement's range table entries print under, as
/// `set_rtable_names` makes each unique by a numeric suffix.
struct RangeTableNames<'n> {
    names: &'n mut [StackStr<96>],
    count: usize,
}

impl RangeTableNames<'_> {
    /// Takes the first name `base`, `base_1`, `base_2`, ... not yet taken.
    fn take(&mut self, base: &str) -> Result<StackStr<96>, SqlError> {
        let unique = unique_rtable_name(base, &self.names[..self.count]);
        let Some(slot) = self.names.get_mut(self.count) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN range table exceeds {} relations",
                self.names.len()
            ));
        };
        *slot = unique;
        self.count += 1;
        Ok(unique)
    }
}

/// Pushes the scan of an expanded relation: its one member's scan, or an
/// `Append` of each member's scan, each with the relation's conditions and
/// target list under its own name (`pt_1`, `pt_2`, ...) from `names`;
/// `member_key` is each member's node. `projection` is the target a
/// `Result` above them computes: an `Append` cannot project
/// (`is_projection_capable_path`), so an inheritance tree's scan/join
/// target that differs from the columns its members return is computed
/// there (`create_projection_plan`), and the `Result` stays when setrefs
/// removes a one-member `Append`, naming the member's columns.
#[allow(clippy::too_many_arguments)]
fn push_members(
    plan: &mut Plan<'_>,
    storage: &Storage,
    txid: u32,
    mut scan: PlanNode,
    expansion: Expansion<'_>,
    nominal_named: bool,
    names: &mut RangeTableNames<'_>,
    member_key: &dyn Fn(usize) -> Option<NodeKey>,
    projection: Option<(targets::Output, bool)>,
    tree: Option<&ChosenTree<'_>>,
) -> Result<(), SqlError> {
    if scan.name.as_str() != "Seq Scan" || !scan.bitmap_index.as_str().is_empty() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of a partitioned table or inheritance tree read by {} is not supported",
            scan.name.as_str()
        ));
    }
    let result = match projection {
        Some((output, output_unprintable)) => {
            let (startup_cost, total_cost) = tree
                .and_then(|tree| tree.path.result_cost)
                .unwrap_or((scan.startup_cost, scan.total_cost));
            plan.push(PlanNode {
                name: StackStr::from_str("Result"),
                depth: scan.depth,
                shift: scan.shift,
                startup_cost,
                total_cost,
                rows: scan.rows,
                width: scan.width,
                output,
                output_unprintable,
                passthrough: true,
                ..PlanNode::EMPTY
            })?;
            scan.depth += 1;
            Some(plan.count - 1)
        }
        None => None,
    };
    // A relation without members is PostgreSQL's dummy relation, scanned
    // as a `Result` returning no rows.
    if expansion.planned.leaves.is_empty() {
        return plan.push(PlanNode {
            name: StackStr::from_str("Result"),
            depth: scan.depth,
            shift: scan.shift,
            key: scan.key,
            rows: 0,
            width: scan.width,
            output: scan.output,
            output_unprintable: scan.output_unprintable,
            constant_false: true,
            ..PlanNode::EMPTY
        });
    }
    let exposed = StackStr::<96>::from_str(scan.exposed_name());
    let member = |leaf: usize, alias: &str| -> Result<PlanNode, SqlError> {
        let definition = storage.table_def(leaf, txid);
        let mut node = scan;
        node.relation = StackStr::from_str(definition.name.as_str());
        node.schema = StackStr::from_str(definition.schema.as_str());
        node.alias = if alias == definition.name.as_str() {
            StackStr::new()
        } else {
            StackStr::from_str(alias)
        };
        if definition.kind == crate::storage::TableKind::Foreign {
            // A foreign member's conditions go to its server where they
            // ship, which only a query's own scan plans.
            if scan.qualifications().next().is_some() {
                if !expansion.foreign_shipped {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of a condition on a foreign partition or inheritance child is not supported"
                    ));
                }
                if !expansion.foreign_local.contains(&leaf) {
                    node.quals = NO_QUALS;
                    node.verbose_quals = NO_QUALS;
                }
            }
            node.name = StackStr::from_str("Foreign Scan");
            node.output_unprintable = true;
        }
        if alias != exposed.as_str() {
            node.requalify(exposed.as_str(), alias);
        }
        Ok(node)
    };
    if let Some(tree) = tree {
        return push_tree_members(
            plan,
            scan,
            expansion,
            nominal_named,
            names,
            member_key,
            result,
            tree,
            &member,
        );
    }
    let planned = expansion.planned.leaves;
    // One member is scanned in place of the `Append`. It takes the
    // relation's name, unless a write names the relation itself.
    if planned.len() == 1 {
        let alias = if nominal_named {
            names.take(exposed.as_str())?
        } else {
            exposed
        };
        if let Some(result) = result
            && alias != exposed
        {
            plan.nodes[result].requalify(exposed.as_str(), alias.as_str());
        }
        let mut node = member(planned[0], alias.as_str())?;
        node.key = scan.key;
        return plan.push(node);
    }
    plan.push(PlanNode {
        name: StackStr::from_str("Append"),
        depth: scan.depth,
        shift: scan.shift,
        key: scan.key,
        startup_cost: scan.startup_cost,
        total_cost: scan.total_cost,
        rows: scan.rows,
        width: scan.width,
        object_requests: scan.object_requests,
        cache_blocks: scan.cache_blocks,
        output: scan.output,
        output_unprintable: scan.output_unprintable,
        subplans_removed: Some(
            u32::try_from(planned.len() - expansion.started.len()).map_err(|_| arena_full())?,
        ),
        scan_pruned: expansion.planned.scan_pruned,
        uncounted: false,
        inner_unique: false,
        bitmap_index_estimate: None,
        analyze_unsupported: None,
        planned_partitions: None,
        hash_agg: None,
        initplan_charge: 0.0,
        ..PlanNode::EMPTY
    })?;
    for &leaf in expansion.started {
        let alias = names.take(exposed.as_str())?;
        let mut node = member(leaf, alias.as_str())?;
        node.depth = scan.depth + 1;
        node.key = member_key(leaf);
        node.rows = scan.rows.div_ceil(planned.len() as u64);
        node.total_cost = scan.total_cost / planned.len() as f64;
        plan.push(node)?;
    }
    Ok(())
}

/// The path chosen over a partition or inheritance tree, with each
/// member's conditions as its scan prints them and the `Merge Append`'s
/// sort keys.
#[derive(Clone, Copy)]
struct ChosenTree<'a> {
    path: query::index_paths::TreePath<'a>,
    /// Per member of `path`, in its order.
    quals: &'a [MemberQuals<'a>],
    keys: PlanKeys,
}

/// A member scan's conditions as printed without and with `VERBOSE`: its
/// `Index Cond` and `Filter`, a bitmap heap scan's `Recheck Cond` and the
/// conditions of each bitmap index scan of its tree.
#[derive(Clone, Copy, Default)]
struct MemberQuals<'a> {
    index_cond: LeafQuals,
    filter: LeafQuals,
    recheck: LeafQuals,
    tree: &'a [LeafQuals],
}

/// Pushes the scan of a tree whose members `tree` chose paths for: one
/// member's subtree in place of the `Append`, or an `Append` or `Merge
/// Append` of each started member's, each member under a sort when the
/// order needs one.
#[allow(clippy::too_many_arguments)]
fn push_tree_members(
    plan: &mut Plan<'_>,
    scan: PlanNode,
    expansion: Expansion<'_>,
    nominal_named: bool,
    names: &mut RangeTableNames<'_>,
    member_key: &dyn Fn(usize) -> Option<NodeKey>,
    result: Option<usize>,
    tree: &ChosenTree<'_>,
    member: &dyn Fn(usize, &str) -> Result<PlanNode, SqlError>,
) -> Result<(), SqlError> {
    use query::index_paths::{AppendKind, ScanKind};
    let exposed = StackStr::<96>::from_str(scan.exposed_name());
    let members = tree.path.members;
    // One member's scan, under its sort, at `depth`.
    let push_member = |plan: &mut Plan<'_>,
                       at: usize,
                       alias: &str,
                       depth: u8,
                       key: Option<NodeKey>|
     -> Result<(), SqlError> {
        let chosen = &members[at];
        let path = &chosen.path;
        let quals = tree.quals.get(at).copied().unwrap_or_default();
        let mut depth = depth;
        if let Some((startup, total)) = chosen.sort {
            let mut keys = tree.keys;
            keys.plain = requalified(&keys.plain, exposed.as_str(), alias);
            keys.verbose = requalified(&keys.verbose, exposed.as_str(), alias);
            plan.push(PlanNode {
                name: StackStr::from_str("Sort"),
                depth,
                shift: scan.shift,
                key: key.and_then(|key| match key.role {
                    Role::Member(position, leaf) => {
                        Some(NodeKey::new(key.block, Role::MemberSort(position, leaf)))
                    }
                    _ => None,
                }),
                startup_cost: startup,
                total_cost: total,
                rows: path.rows as u64,
                width: path.width,
                keys,
                ..PlanNode::EMPTY
            })?;
            depth += 1;
        }
        let mut node = member(chosen.leaf, alias)?;
        node.depth = depth;
        node.key = key;
        node.startup_cost = path.startup_cost;
        node.total_cost = path.total_cost;
        node.rows = path.rows as u64;
        node.width = path.width;
        let mut name = StackStr::new();
        match path.kind {
            ScanKind::Seq => {
                let _ = write!(name, "Seq Scan");
            }
            ScanKind::Index | ScanKind::IndexOnly => {
                let _ = write!(
                    name,
                    "Index {}Scan {}using {}",
                    if path.kind == ScanKind::IndexOnly {
                        "Only "
                    } else {
                        ""
                    },
                    if path.backward { "Backward " } else { "" },
                    path.index_name.as_str()
                );
            }
            ScanKind::Bitmap => {
                let _ = write!(name, "Bitmap Heap Scan");
                if let [only] = path.bitmap
                    && let query::index_paths::BitmapPlanKind::Index { index_name, .. } = only.kind
                {
                    node.bitmap_index = index_name;
                }
                node.bitmap_index_estimate = match path.bitmap {
                    [only] => Some((only.total_cost, only.rows as u64)),
                    _ => None,
                };
            }
        }
        node.name = name;
        if path.kind != ScanKind::Seq {
            let requalify = |text: Option<StackStr<512>>| {
                text.map(|text| requalified(&text, exposed.as_str(), alias))
            };
            node.quals = NO_QUALS;
            node.verbose_quals = NO_QUALS;
            node = node
                .with_qual(QualLabel::IndexCond, requalify(quals.index_cond.0))
                .with_qual(QualLabel::Filter, requalify(quals.filter.0))
                .with_verbose_qual(QualLabel::IndexCond, requalify(quals.index_cond.1))
                .with_verbose_qual(QualLabel::Filter, requalify(quals.filter.1));
            if path.kind == ScanKind::Bitmap {
                node = node
                    .with_qual(QualLabel::RecheckCond, requalify(quals.recheck.0))
                    .with_verbose_qual(QualLabel::RecheckCond, requalify(quals.recheck.1));
            }
        }
        node.analyze_unsupported = path.searches_unmodeled.then_some(
            "EXPLAIN ANALYZE of an index scan searching an array or skipping a key column over several leaf pages is not supported",
        );
        push_scan(plan, node)?;
        if path.kind == ScanKind::Bitmap && path.bitmap.len() > 1 {
            let leaf_quals = arena_requalified(quals.tree, exposed.as_str(), alias);
            push_bitmap_tree_keyed(
                plan,
                path.bitmap,
                &leaf_quals[..quals.tree.len().min(leaf_quals.len())],
                depth,
                &|at| {
                    key.and_then(|key| match key.role {
                        Role::Member(position, leaf) => Some(NodeKey::new(
                            key.block,
                            Role::MemberBitmapNode(position, leaf, at),
                        )),
                        _ => None,
                    })
                },
            )?;
        }
        Ok(())
    };
    // Members are named in the order planning lists them; started ones
    // only, as EXPLAIN names the relations its plan reads.
    let mut aliases = [StackStr::<96>::new(); 256];
    let mut alias_leaves = [usize::MAX; 256];
    let mut alias_count = 0;
    let started = |leaf: usize| expansion.started.contains(&leaf);
    if members.len() == 1 {
        let alias = if nominal_named {
            names.take(exposed.as_str())?
        } else {
            exposed
        };
        if let Some(result) = result
            && alias != exposed
        {
            plan.nodes[result].requalify(exposed.as_str(), alias.as_str());
        }
        return push_member(plan, 0, alias.as_str(), scan.depth, scan.key);
    }
    for &leaf in expansion.planned.leaves {
        if started(leaf) && members.iter().any(|chosen| chosen.leaf == leaf) {
            if alias_count == aliases.len() {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "EXPLAIN range table exceeds {} relations",
                    aliases.len()
                ));
            }
            aliases[alias_count] = names.take(exposed.as_str())?;
            alias_leaves[alias_count] = leaf;
            alias_count += 1;
        }
    }
    let started_count = members.iter().filter(|chosen| started(chosen.leaf)).count();
    let merge = tree.path.kind == AppendKind::Merge;
    plan.push(PlanNode {
        name: StackStr::from_str(if merge { "Merge Append" } else { "Append" }),
        depth: scan.depth,
        shift: scan.shift,
        key: scan.key,
        startup_cost: scan.startup_cost,
        total_cost: scan.total_cost,
        rows: scan.rows,
        width: scan.width,
        object_requests: scan.object_requests,
        cache_blocks: scan.cache_blocks,
        output: scan.output,
        output_unprintable: scan.output_unprintable,
        keys: if merge { tree.keys } else { PlanKeys::NONE },
        subplans_removed: Some(
            u32::try_from(members.len() - started_count).map_err(|_| arena_full())?,
        ),
        scan_pruned: expansion.planned.scan_pruned,
        ..PlanNode::EMPTY
    })?;
    for (at, chosen) in members.iter().enumerate() {
        if !started(chosen.leaf) {
            continue;
        }
        let alias = aliases[..alias_count]
            .iter()
            .zip(&alias_leaves[..alias_count])
            .find(|(_, held)| **held == chosen.leaf)
            .map(|(alias, _)| *alias)
            .unwrap_or(exposed);
        push_member(
            plan,
            at,
            alias.as_str(),
            scan.depth + 1,
            member_key(chosen.leaf),
        )?;
    }
    Ok(())
}

/// The conditions each member of `tree` prints, read by the parent's
/// `exposed` name: an index scan's `Index Cond` and `Filter`, a bitmap
/// heap scan's `Recheck Cond` and its bitmap index scans' conditions.
#[allow(clippy::too_many_arguments)]
fn tree_member_quals<'a>(
    storage: &Storage,
    txid: u32,
    exposed: &'a str,
    scanned: Option<&'a Expr<'a>>,
    security: &[&'a Expr<'a>],
    tree: &query::index_paths::TreePath<'a>,
    arena: &'a Arena,
) -> Result<&'a [MemberQuals<'a>], SqlError> {
    use query::index_paths::ScanKind;
    let definitions = storage.table_definition_images();
    let quals = arena
        .alloc_slice_with(tree.members.len(), |_| MemberQuals::default())
        .map_err(|_| arena_full())?;
    for (at, member) in tree.members.iter().enumerate() {
        let path = &member.path;
        if path.kind == ScanKind::Seq {
            continue;
        }
        let definition = definitions.definition(storage, member.leaf, txid)?;
        let print = |index: &query::IndexAccessPlan<'a>, verbose: bool| {
            single_relation_quals(
                storage,
                txid,
                exposed,
                definition,
                &[],
                scanned,
                security,
                Some(index),
                verbose,
                arena,
            )
        };
        let index = query::IndexAccessPlan::chosen(path);
        let plain = print(&index, false)?;
        let verbose = print(&index, true)?;
        quals[at].index_cond = (plain.0, verbose.0);
        quals[at].filter = (plain.1, verbose.1);
        if path.kind == ScanKind::Bitmap {
            let written = index.written();
            quals[at].recheck = (print(&written, false)?.0, print(&written, true)?.0);
            if path.bitmap.len() > 1 {
                let tree_quals = arena
                    .alloc_slice_with(path.bitmap.len(), |_| (None, None))
                    .map_err(|_| arena_full())?;
                for (node_at, node) in path.bitmap.iter().enumerate() {
                    let query::index_paths::BitmapPlanKind::Index { conditions, .. } = node.kind
                    else {
                        continue;
                    };
                    let leaf = query::IndexAccessPlan::chosen_conditions(query::ChosenConditions {
                        conditions,
                        recheck: &[],
                        absorbed: &[],
                    });
                    tree_quals[node_at] = (print(&leaf, false)?.0, print(&leaf, true)?.0);
                }
                quals[at].tree = tree_quals;
            }
        }
    }
    Ok(quals)
}

/// `quals` with `old` renamed `new`.
fn arena_requalified(quals: &[LeafQuals], old: &str, new: &str) -> [LeafQuals; 64] {
    let mut renamed = [(None, None); 64];
    for (at, (plain, verbose)) in quals.iter().take(64).enumerate() {
        renamed[at] = (
            plain.map(|text| requalified(&text, old, new)),
            verbose.map(|text| requalified(&text, old, new)),
        );
    }
    renamed
}

fn push_scan(plan: &mut Plan<'_>, mut scan: PlanNode) -> Result<(), SqlError> {
    let bitmap_index = scan.bitmap_index;
    let bitmap_depth = scan.depth.saturating_add(1);
    let (bitmap_cost, bitmap_rows) = scan
        .bitmap_index_estimate
        .unwrap_or((scan.total_cost * 0.4, scan.rows));
    let mut index_cond = None;
    let mut verbose_index_cond = None;
    // A heap scan that rechecks its conditions as written passes its
    // bitmap index scan the `Index Cond` it holds; so does the scan of a
    // planner-costed bitmap whose conditions are all lossy, which has no
    // recheck of its own.
    if !bitmap_index.as_str().is_empty()
        && (scan.bitmap_index_estimate.is_some()
            || scan
                .quals
                .iter()
                .any(|(label, _)| *label == QualLabel::RecheckCond))
    {
        for (quals, held) in [
            (&mut scan.quals, &mut index_cond),
            (&mut scan.verbose_quals, &mut verbose_index_cond),
        ] {
            if let Some(entry) = quals
                .iter_mut()
                .find(|(label, _)| *label == QualLabel::IndexCond)
            {
                *held = Some(entry.1);
                *entry = (QualLabel::None, StackStr::new());
            }
        }
    } else if !bitmap_index.as_str().is_empty()
        && let Some(entry) = scan
            .quals
            .iter_mut()
            .find(|(label, _)| *label == QualLabel::IndexCond)
    {
        entry.0 = QualLabel::RecheckCond;
        index_cond = Some(entry.1);
        if let Some(entry) = scan
            .verbose_quals
            .iter_mut()
            .find(|(label, _)| *label == QualLabel::IndexCond)
        {
            entry.0 = QualLabel::RecheckCond;
            verbose_index_cond = Some(entry.1);
        }
    }
    // The bitmap index scan counts under its heap scan's position.
    let bitmap_key = scan.key.and_then(|key| match key.role {
        Role::Scan(position) => Some(NodeKey::new(key.block, Role::BitmapIndex(position))),
        Role::Member(position, leaf) => Some(NodeKey::new(
            key.block,
            Role::MemberBitmapIndex(position, leaf),
        )),
        _ => None,
    });
    plan.push(scan)?;
    if !bitmap_index.as_str().is_empty() {
        let mut name = StackStr::new();
        let _ = write!(name, "Bitmap Index Scan on {}", bitmap_index.as_str());
        plan.push(
            PlanNode {
                name,
                key: bitmap_key,
                depth: bitmap_depth,
                total_cost: bitmap_cost,
                rows: bitmap_rows,
                ..PlanNode::EMPTY
            }
            .with_qual(QualLabel::IndexCond, index_cond)
            .with_verbose_qual(QualLabel::IndexCond, verbose_index_cond),
        )?;
    }
    Ok(())
}

/// The path PostgreSQL's costs choose for an `UPDATE`'s or `DELETE`'s scan
/// of its target, with the scope it reads: `None` when only a sequential
/// scan serves it, whose estimates the model leaves out, or when the target
/// is no plain table.
fn chosen_write_scan<'a>(
    storage: &'a Storage,
    txid: u32,
    target: crate::sql::ast::QualName<'a>,
    alias: Option<&'a str>,
    predicate: Option<&'a Expr<'a>>,
    assignments: &[(&str, &'a Expr<'a>)],
    arena: &'a Arena,
) -> Result<Option<(query::index_paths::ScanPath<'a>, QueryScope<'a>)>, SqlError> {
    let (scope, shape) =
        query::index_paths::write_shape(storage, txid, target, alias, assignments, arena)?;
    match query::index_paths::scan_path(storage, &scope, txid, predicate, None, &[], arena, shape)?
    {
        query::index_paths::Planned::Chosen(path) => Ok(Some((path, scope))),
        query::index_paths::Planned::Outside(shape) => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of a scan of one table with {} is not supported",
            shape
        )),
        query::index_paths::Planned::NotTable
        | query::index_paths::Planned::SeqOnly
        | query::index_paths::Planned::Members(_)
        | query::index_paths::Planned::Tree(_)
        | query::index_paths::Planned::Join(_) => Ok(None),
    }
}

/// The assignments of an `UPDATE`; none for another write.
fn write_assignments<'e>(statement: &Stmt<'e>) -> &'e [(&'e str, &'e Expr<'e>)] {
    match statement {
        Stmt::Update(update) => update.assignments,
        _ => &[],
    }
}

/// A bitmap index scan's `Index Cond`, as printed without and with
/// `VERBOSE`.
type LeafQuals = (Option<StackStr<512>>, Option<StackStr<512>>);

/// Pushes the `BitmapAnd`, `BitmapOr` and bitmap index scan nodes of a
/// chosen bitmap heap scan at `depth`, each bitmap index scan with its
/// conditions.
fn push_bitmap_tree(
    plan: &mut Plan<'_>,
    nodes: &[query::index_paths::BitmapPlanNode<'_>],
    quals: &[LeafQuals],
    depth: u8,
    block: crate::sql::query::instrument::BlockId,
) -> Result<(), SqlError> {
    push_bitmap_tree_keyed(plan, nodes, quals, depth, &|at| {
        Some(NodeKey::new(block, Role::BitmapNode(at)))
    })
}

/// [`push_bitmap_tree`] with each node's key by its position.
fn push_bitmap_tree_keyed(
    plan: &mut Plan<'_>,
    nodes: &[query::index_paths::BitmapPlanNode<'_>],
    quals: &[LeafQuals],
    depth: u8,
    key: &dyn Fn(u16) -> Option<NodeKey>,
) -> Result<(), SqlError> {
    use query::index_paths::BitmapPlanKind;
    for (at, node) in nodes.iter().enumerate() {
        let mut name = StackStr::new();
        match node.kind {
            BitmapPlanKind::Index { index_name, .. } => {
                let _ = write!(name, "Bitmap Index Scan on {}", index_name.as_str());
            }
            BitmapPlanKind::And => {
                let _ = write!(name, "BitmapAnd");
            }
            BitmapPlanKind::Or => {
                let _ = write!(name, "BitmapOr");
            }
        }
        let (plain, verbose) = quals.get(at).copied().unwrap_or((None, None));
        plan.push(
            PlanNode {
                name,
                key: key(u16::try_from(at).map_err(|_| arena_full())?),
                depth: depth.saturating_add(node.depth),
                // `cost_bitmap_and_node` and `cost_bitmap_or_node` start at
                // their total: nothing returns before every input is built.
                startup_cost: match node.kind {
                    BitmapPlanKind::Index { .. } => 0.0,
                    BitmapPlanKind::And | BitmapPlanKind::Or => node.total_cost,
                },
                total_cost: node.total_cost,
                rows: node.rows as u64,
                ..PlanNode::EMPTY
            }
            .with_qual(QualLabel::IndexCond, plain)
            .with_verbose_qual(QualLabel::IndexCond, verbose),
        )?;
    }
    Ok(())
}

/// The conditions `index` consumes, each with the indexed side first as
/// PostgreSQL prints an index scan's `Index Cond`; the scanned relation
/// `exposed` prints its columns unqualified.
fn index_condition<'a>(
    context: &crate::sql::catalog::deparse::Context<'_>,
    exposed: &str,
    index: &query::IndexAccessPlan<'a>,
    verbose: bool,
    arena: &'a Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    let mut conditions = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(chosen) = index.chosen_parts() {
        let context = crate::sql::catalog::deparse::Context {
            bare: (!verbose).then_some(crate::sql::catalog::deparse::printed_name(exposed)),
            ..*context
        };
        if index.as_written() {
            return deparse_qual(&context, chosen.recheck, arena);
        }
        for condition in chosen.conditions {
            if count == MAX_CONJUNCTS {
                return Ok(None);
            }
            conditions[count] = condition.printed;
            count += 1;
        }
        return deparse_qual(&context, &conditions[..count], arena);
    }
    for (source, key, operator, operand) in index.index_conditions() {
        if count == MAX_CONJUNCTS {
            return Ok(None);
        }
        // A null test, and a bitmap heap scan's recheck, print the
        // condition as written.
        if index.as_written() || matches!(source, Expr::IsNull { .. }) {
            conditions[count] = source;
            count += 1;
            continue;
        }
        let source_position = match source {
            Expr::Binary {
                source_position, ..
            } => *source_position,
            _ => crate::sql::ast::SourcePosition::UNKNOWN,
        };
        conditions[count] = &*arena
            .alloc(Expr::Binary {
                source_position,
                operator,
                left: key,
                right: operand,
            })
            .map_err(|_| arena_full())?;
        count += 1;
    }
    // `VERBOSE` qualifies the indexed column too.
    let context = crate::sql::catalog::deparse::Context {
        bare: (!verbose).then_some(exposed),
        ..*context
    };
    deparse_qual(&context, &conditions[..count], arena)
}

/// Whether `conjunct` is one of the conditions `index` consumes.
fn consumed_by(index: Option<&query::IndexAccessPlan<'_>>, conjunct: &Expr<'_>) -> bool {
    index.is_some_and(|index| match index.chosen_parts() {
        Some(chosen) => chosen
            .absorbed
            .iter()
            .any(|absorbed| core::ptr::eq(*absorbed, conjunct)),
        None => index
            .index_conditions()
            .any(|(source, ..)| core::ptr::eq(source, conjunct)),
    })
}

fn projected_shape<'a>(
    statement: &Select<'a>,
    scope: Option<&QueryScope<'a>>,
    outer: Option<&dyn ColumnLookup<'_>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(u32, StackStr<256>), SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); crate::sql::exec::MAX_PROJ];
    // A subquery's list may read the columns of the queries around it.
    let count = match scope {
        Some(scope) => query::describe_scope_items(
            statement.items,
            scope,
            outer,
            storage,
            txid,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Sql,
        )?,
        // A query without FROM types its subqueries' values as the
        // statement's description does.
        None if statement.set_body.is_none() && outer.is_none() => {
            let statement = arena.alloc(*statement).map_err(|_| arena_full())?;
            query::describe_select(
                statement,
                storage,
                txid,
                arena,
                &mut columns,
                crate::sql::exec::DescriptionTarget::Sql,
                None,
            )?
        }
        None => {
            let outer_types = outer.map(query::OuterColumnTypes);
            let statement = arena.alloc(*statement).map_err(|_| arena_full())?;
            let tree = arena
                .alloc(SetTree::Select(statement))
                .map_err(|_| arena_full())?;
            query::describe_set_body(
                storage,
                tree,
                txid,
                &mut columns,
                arena,
                None,
                crate::sql::exec::DescriptionTarget::Sql,
                outer_types
                    .as_ref()
                    .map(|types| types as &dyn ColumnLookup<'_>),
            )?
        }
    };
    let mut width = 0u32;
    let mut output = StackStr::new();
    for column in &columns[..count] {
        // A fixed-length type's width is its length, also where the
        // description leaves the length unset (a subquery's column).
        let length = crate::sql::types::ColType::from_oid(column.type_oid)
            .map_or(0, |ctype| ctype.physical_layout().length);
        width = width.saturating_add(if column.typlen > 0 {
            column.typlen as u32
        } else if length > 0 {
            length as u32
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

/// Whether `statement` aggregates, resolving its columns through its FROM
/// items.
fn selects_aggregate(
    statement: &Select<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let scope = match &statement.from {
        Some(from) => Some(QueryScope::resolve_schema(
            storage, from, txid, arena, None,
        )?),
        None => None,
    };
    let scoped_columns = scope.as_ref().map(|scope| query::CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    });
    let empty_columns = crate::sql::exec::CatalogCols {
        remote_ctid: false,
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
    has_aggregate(statement, storage, txid, columns)
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

/// The scan and grouping PostgreSQL's planner chooses for a query grouping
/// the rows of one table by its columns, with the keys its sort and its
/// `Group Key` list in the order the grouping uses.
#[derive(Clone, Copy)]
struct GroupedScan<'a> {
    path: query::index_paths::ScanPath<'a>,
    /// The query shape and read columns the path was planned for, which
    /// the scan's execution plans it from again.
    shape: query::index_paths::QueryShape,
    needed: Option<crate::storage::ColumnSet>,
    sort_keys: &'a [KeyItem<'a>],
    group_keys: &'a [KeyItem<'a>],
    /// The width of the rows the grouping returns, each column of the
    /// table its analyzed width.
    width: u32,
}

/// [`GroupedScan`] of `statement`, or `None` when the query is outside the
/// model: a `HAVING`, `LIMIT`, `DISTINCT`, window or set-returning list, a
/// grouping key other than a column of the table, a select list computing
/// more than aggregates, columns and constants, an `ORDER BY` sharing only
/// part of its leading keys with the grouping, or a scan the model of
/// [`query::index_paths`] leaves out.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
fn grouped_scan_path<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    output_width: u32,
    arena: &'a Arena,
) -> Result<Option<GroupedScan<'a>>, SqlError> {
    use query::index_paths::{GroupOrder, GroupingShape, MAX_ORDER_KEYS, QueryShape};
    if statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.group_by.len() > MAX_ORDER_KEYS
        || scope.n != 1
        || !from.joins.is_empty()
        || from.base.sample.is_some()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.locking.is_empty()
        || has_window(statement)
        || query::has_set_returning_items(statement.items, storage, txid)
        || scope.slots[0] == usize::MAX
        || scope.derived[0].is_some()
    {
        return Ok(None);
    }
    let Some(limit) = query::index_paths::constant_limit(statement.limit, statement.offset, arena)?
    else {
        return Ok(None);
    };
    let slot = scope.slots[0];
    if storage.table_def(slot, txid).partition.is_partitioned()
        || storage.relation_has_descendants(slot, txid)
    {
        return Ok(None);
    }
    // An ordinal names a select-list entry.
    let resolve = |expression: &'a Expr<'a>| -> &'a Expr<'a> {
        match expression {
            Expr::Int(position) if *position >= 1 => statement
                .items
                .get(*position as usize - 1)
                .and_then(|item| match item {
                    SelectItem::Expr { expression, .. } => Some(*expression),
                    _ => None,
                })
                .unwrap_or(expression),
            _ => expression,
        }
    };
    let column_of = |expression: &'a Expr<'a>| -> Option<usize> {
        match resolve(expression) {
            Expr::Column { qualifier, name } => match scope.find_column(*qualifier, name) {
                Ok(query::ResolvedColumn::Table(0, column)) => Some(column),
                _ => None,
            },
            _ => None,
        }
    };
    let mut group = [0usize; MAX_ORDER_KEYS];
    let mut group_len = 0;
    for &key in statement.group_by {
        let Some(column) = column_of(key) else {
            return Ok(None);
        };
        if !group[..group_len].contains(&column) {
            group[group_len] = column;
            group_len += 1;
        }
    }
    // `preprocess_groupclause`: the grouping takes the `ORDER BY`'s order
    // and directions when its keys lead with grouping columns.
    let mut ordered = [None::<(usize, (bool, bool))>; MAX_ORDER_KEYS];
    let mut ordered_len = 0;
    let mut order = if statement.order_by.is_empty() {
        GroupOrder::None
    } else {
        GroupOrder::Grouped
    };
    for (at, key) in statement.order_by.iter().enumerate() {
        let column = column_of(key.expression).filter(|column| group[..group_len].contains(column));
        match column {
            Some(column) if order == GroupOrder::Grouped => {
                if !ordered[..ordered_len]
                    .iter()
                    .flatten()
                    .any(|(seen, _)| *seen == column)
                {
                    ordered[ordered_len] = Some((column, (key.descending, key.nulls_first)));
                    ordered_len += 1;
                }
            }
            // The groups are sorted above the grouping by whatever keys
            // follow a leading key that is not a grouping column.
            Some(_) | None if order == GroupOrder::Other => {}
            Some(_) => return Ok(None),
            None if at == 0 => order = GroupOrder::Other,
            None => return Ok(None),
        }
    }
    if order == GroupOrder::Other {
        ordered_len = 0;
    }
    let mut columns_in_order = [0usize; MAX_ORDER_KEYS];
    let mut directions = [None::<(bool, bool)>; MAX_ORDER_KEYS];
    for (at, (column, direction)) in ordered[..ordered_len].iter().flatten().enumerate() {
        columns_in_order[at] = *column;
        directions[at] = Some(*direction);
    }
    let mut count = ordered_len;
    for &column in &group[..group_len] {
        if !columns_in_order[..count].contains(&column) {
            columns_in_order[count] = column;
            count += 1;
        }
    }
    // A column of a variable-length type is as wide as ANALYZE measured.
    let mut output_width = output_width;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Some(column) = column_of(expression)
            && scope.defs.stored(0).is_some_and(|definition| {
                definition.columns()[column].ctype.physical_layout().length <= 0
            })
        {
            output_width = output_width - 32
                + query::index_paths::column_width(storage, txid, scope.slots[0], column);
        }
    }
    // A `HAVING` reading its aggregates: its cost and selectivity.
    let having = match statement.having {
        Some(having) => {
            match query::index_paths::having_quals(having, storage, txid, columns, arena)? {
                Some(quals) => Some(quals),
                None => return Ok(None),
            }
        }
        None => None,
    };
    // The aggregates' costs, over a list of aggregates, columns and
    // constants, with the `HAVING` qualification's aggregates.
    let expressions = arena
        .alloc_slice_with(statement.items.len() + statement.order_by.len() + 1, |_| {
            &Expr::Null
        })
        .map_err(|_| arena_full())?;
    if let Some(condition) = statement.having {
        expressions[statement.items.len() + statement.order_by.len()] = condition;
    }
    for (slot, item) in expressions.iter_mut().zip(statement.items) {
        let SelectItem::Expr { expression, .. } = item else {
            return Ok(None);
        };
        if !matches!(expression, Expr::Call { .. } | Expr::Column { .. })
            && !expression.is_constant(false)
        {
            return Ok(None);
        }
        if matches!(expression, Expr::Call { .. })
            && query::aggregate_routine(expression, storage, txid, columns)?.is_none()
        {
            return Ok(None);
        }
        *slot = expression;
    }
    for (slot, key) in expressions[statement.items.len()..]
        .iter_mut()
        .zip(statement.order_by)
    {
        *slot = resolve(key.expression);
    }
    let Some(aggregates) =
        query::index_paths::aggregate_costs(expressions, storage, txid, columns, arena)?
            .filter(|aggregates| !aggregates.ordered)
    else {
        return Ok(None);
    };
    let mut group_mask = ColumnSet::EMPTY;
    for &column in &columns_in_order[..count] {
        group_mask.insert(column);
    }
    let output = query::streaming_pax_columns_ordered(
        scope,
        from,
        statement.items,
        None,
        statement.order_by,
    )
    .selected_columns(0)
    .map(|set| set.union(group_mask));
    let needed = query::streaming_pax_columns_ordered(
        scope,
        from,
        statement.items,
        statement.where_clause,
        statement.order_by,
    )
    .selected_columns(0)
    .map(|set| set.union(group_mask));
    // The grouping sorts by its columns (`group_pathkeys`), which the scan
    // may return ordered.
    let mut shape = QueryShape {
        only: from.base.inheritance == crate::sql::ast::RelationInheritance::Only,
        output,
        grouping: Some(GroupingShape {
            columns: columns_in_order,
            len: count,
            aggregates,
            output_width: f64::from(output_width),
            order,
            having,
        }),
        limit,
        order_len: count,
        ..QueryShape::default()
    };
    for at in 0..count {
        let (descending, nulls_first) = directions[at].unwrap_or((false, false));
        shape.order[at] = query::index_paths::OrderKey::Column {
            column: columns_in_order[at],
            descending,
            nulls_first,
        };
    }
    let planned = query::index_paths::scan_path(
        storage,
        scope,
        txid,
        split_gating(storage, txid, statement.where_clause, arena)?.1,
        needed,
        &[],
        arena,
        shape,
    )?;
    let query::index_paths::Planned::Chosen(path) = planned else {
        return Ok(None);
    };
    if path.grouped.is_none() {
        return Ok(None);
    }
    // The keys as the plan prints them: each grouping column by the
    // `GROUP BY` entry naming it.
    let entry = |column: usize| -> &'a Expr<'a> {
        statement
            .group_by
            .iter()
            .copied()
            .find(|key| column_of(key) == Some(column))
            .expect("a grouping column has its entry")
    };
    let keys = path
        .grouped
        .map_or(columns_in_order, |grouped| grouped.keys);
    let direction = |column: usize| {
        columns_in_order[..count]
            .iter()
            .position(|&other| other == column)
            .and_then(|at| directions[at])
            .unwrap_or((false, false))
    };
    let count = path.grouped.map_or(count, |grouped| grouped.key_count);
    let sort_keys = arena
        .alloc_slice_with(count, |at| (entry(keys[at]), Some(direction(keys[at]))))
        .map_err(|_| arena_full())?;
    let group_keys = arena
        .alloc_slice_with(count, |at| (entry(keys[at]), None))
        .map_err(|_| arena_full())?;
    Ok(Some(GroupedScan {
        path,
        shape,
        needed,
        sort_keys,
        group_keys,
        width: output_width,
    }))
}

/// The shape and read columns of a query grouping the rows of one table
/// by its columns, when PostgreSQL's planner plans its scan and grouping
/// ([`grouped_scan_path`]): its execution scans by that path, counting
/// what the path reads.
pub(crate) fn grouped_scan_context<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    arena: &'a Arena,
) -> Result<
    Option<(
        query::index_paths::QueryShape,
        Option<crate::storage::ColumnSet>,
    )>,
    SqlError,
> {
    let (width, _) = projected_shape(statement, Some(scope), None, storage, txid, arena)?;
    Ok(
        grouped_scan_path(storage, txid, statement, scope, from, columns, width, arena)?
            .map(|grouped| (grouped.shape, grouped.needed)),
    )
}

/// The grouping sets of a query of one plain table as PostgreSQL's
/// `consider_groupingsets_paths` plans them, with the scan they read, or
/// `None` outside the model: the shapes [`grouped_scan_path`] leaves out,
/// a grouping key that is not a column, or an index ordering one.
#[allow(clippy::too_many_arguments)]
fn grouping_sets_scan_path<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    from: &'a crate::sql::ast::FromClause<'a>,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    output_width: u32,
    arena: &'a Arena,
) -> Result<
    Option<(
        query::index_paths::ScanPath<'a>,
        query::index_paths::SetsPlan<'a>,
        u32,
    )>,
    SqlError,
> {
    use query::index_paths::{
        GroupOrder, GroupingSet, GroupingShape, MAX_ORDER_KEYS, MAX_SET_COLUMNS, QueryShape,
    };
    if statement.grouping_sets.is_empty()
        || statement.group_by.len() > MAX_ORDER_KEYS.min(MAX_SET_COLUMNS)
        || scope.n != 1
        || !from.joins.is_empty()
        || from.base.sample.is_some()
        || statement.having.is_some()
        || statement.limit.is_some()
        || statement.offset.is_some()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.locking.is_empty()
        || has_window(statement)
        || query::has_set_returning_items(statement.items, storage, txid)
        || scope.slots[0] == usize::MAX
        || scope.derived[0].is_some()
    {
        return Ok(None);
    }
    let slot = scope.slots[0];
    if storage.table_def(slot, txid).partition.is_partitioned()
        || storage.relation_has_descendants(slot, txid)
    {
        return Ok(None);
    }
    let resolve = |expression: &'a Expr<'a>| -> &'a Expr<'a> {
        match expression {
            Expr::Int(position) if *position >= 1 => statement
                .items
                .get(*position as usize - 1)
                .and_then(|item| match item {
                    SelectItem::Expr { expression, .. } => Some(*expression),
                    _ => None,
                })
                .unwrap_or(expression),
            _ => expression,
        }
    };
    let column_of = |expression: &'a Expr<'a>| -> Option<usize> {
        match resolve(expression) {
            Expr::Column { qualifier, name } => match scope.find_column(*qualifier, name) {
                Ok(query::ResolvedColumn::Table(0, column)) => Some(column),
                _ => None,
            },
            _ => None,
        }
    };
    let mut key_columns = [0usize; MAX_ORDER_KEYS];
    for (at, &key) in statement.group_by.iter().enumerate() {
        let Some(column) = column_of(key) else {
            return Ok(None);
        };
        // A column named twice is one grouping key to PostgreSQL.
        if key_columns[..at].contains(&column) {
            return Ok(None);
        }
        key_columns[at] = column;
    }
    let expressions = arena
        .alloc_slice_with(statement.items.len(), |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    for (slot, item) in expressions.iter_mut().zip(statement.items) {
        let SelectItem::Expr { expression, .. } = item else {
            return Ok(None);
        };
        if !matches!(expression, Expr::Call { .. } | Expr::Column { .. })
            && !expression.is_constant(false)
        {
            return Ok(None);
        }
        if matches!(expression, Expr::Call { .. })
            && query::aggregate_routine(expression, storage, txid, columns)?.is_none()
        {
            return Ok(None);
        }
        *slot = expression;
    }
    let Some(aggregates) =
        query::index_paths::aggregate_costs(expressions, storage, txid, columns, arena)?
            .filter(|aggregates| !aggregates.ordered)
    else {
        return Ok(None);
    };
    let mut output_width = output_width;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Some(column) = column_of(expression)
            && scope.defs.stored(0).is_some_and(|definition| {
                definition.columns()[column].ctype.physical_layout().length <= 0
            })
        {
            output_width = output_width - 32
                + query::index_paths::column_width(storage, txid, scope.slots[0], column);
        }
    }
    let condition = split_gating(storage, txid, statement.where_clause, arena)?.1;
    // Each set's groups (`estimate_num_groups`), and the relation's
    // cheapest path, from the grouping of its columns.
    let mut scan = None;
    let mut input = None;
    let n_keys = statement.group_by.len();
    let sets = arena
        .alloc_slice_with(statement.grouping_sets.len(), |_| GroupingSet {
            columns: [0; MAX_SET_COLUMNS],
            len: 0,
            groups: 1.0,
        })
        .map_err(|_| arena_full())?;
    for (set, mask) in sets.iter_mut().zip(statement.grouping_sets) {
        let mut set_columns = [0usize; MAX_ORDER_KEYS];
        for position in (0..n_keys).filter(|&position| mask.contains(position)) {
            set.columns[set.len] = position as u8;
            set_columns[set.len] = key_columns[position];
            set.len += 1;
        }
        if set.len == 0 && scan.is_some() {
            continue;
        }
        let mut group_mask = ColumnSet::EMPTY;
        for &column in &key_columns[..n_keys] {
            group_mask.insert(column);
        }
        let selected = |condition| {
            query::streaming_pax_columns_ordered(scope, from, statement.items, condition, &[])
                .selected_columns(0)
                .map(|set| set.union(group_mask))
        };
        let shape = QueryShape {
            only: from.base.inheritance == crate::sql::ast::RelationInheritance::Only,
            output: selected(None),
            grouping: Some(GroupingShape {
                columns: set_columns,
                len: set.len.max(1),
                aggregates,
                output_width: f64::from(output_width),
                order: GroupOrder::None,
                having: None,
            }),
            ..QueryShape::default()
        };
        let planned = query::index_paths::scan_path(
            storage,
            scope,
            txid,
            condition,
            selected(statement.where_clause),
            &[],
            arena,
            shape,
        )?;
        let query::index_paths::Planned::Chosen(path) = planned else {
            return Ok(None);
        };
        let Some(grouped) = path.grouped else {
            return Ok(None);
        };
        if set.len > 0 {
            set.groups = grouped.rows;
        }
        input = Some(grouped.input);
        scan = Some(query::index_paths::ScanPath {
            grouped: None,
            ..path
        });
    }
    let (Some(scan), Some(input)) = (scan, input) else {
        return Ok(None);
    };
    let order = arena
        .alloc_slice_with(statement.order_by.len(), |at| {
            column_of(statement.order_by[at].expression).and_then(|column| {
                key_columns[..n_keys]
                    .iter()
                    .position(|&key| key == column)
                    .map(|position| position as u8)
            })
        })
        .map_err(|_| arena_full())?;
    let plan = query::index_paths::plan_grouping_sets(
        &query::index_paths::SetsQuery {
            sets,
            distinct: statement.grouping_set_quantifier
                == crate::sql::ast::GroupingSetQuantifier::Distinct,
            order,
            aggregates,
            input,
            output_width: f64::from(output_width),
        },
        arena,
    )?;
    Ok(Some((scan, plan, output_width)))
}

/// The key lines of a grouping sets node, as `show_grouping_sets` prints
/// them: for each rollup, the sort it reads its input from when it sorts
/// it itself, then each set's `Hash Key` or `Group Key`.
fn grouping_sets_keys(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    statement: &Select<'_>,
    plan: &query::index_paths::SetsPlan<'_>,
    arena: &Arena,
) -> Result<PlanKeys, SqlError> {
    use query::index_paths::SetsStrategy;
    let direction = |position: u8| {
        statement
            .order_by
            .iter()
            .find(|key| *key.expression == *statement.group_by[usize::from(position)])
            .map(|key| (key.descending, key.nulls_first))
    };
    let mut keys = PlanKeys {
        label: KeyLabel::Sets,
        plain: StackStr::new(),
        verbose: StackStr::new(),
    };
    let mut record = |tag: char, items: &[u8], sort: bool| -> Result<(), SqlError> {
        let entries = arena
            .alloc_slice_with(items.len(), |at| {
                let position = items[at];
                (
                    statement.group_by[usize::from(position)],
                    sort.then(|| direction(position).unwrap_or((false, false))),
                )
            })
            .map_err(|_| arena_full())?;
        let label = if sort {
            KeyLabel::Sort
        } else {
            KeyLabel::Group
        };
        let printed = plan_keys(storage, txid, scope, statement, label, entries, arena)?;
        for (target, text) in [
            (&mut keys.plain, printed.plain),
            (&mut keys.verbose, printed.verbose),
        ] {
            if !target.as_str().is_empty() {
                let _ = write!(target, "{GROUPING_SET_SEPARATOR}");
            }
            let _ = write!(target, "{tag}{}", text.as_str());
        }
        Ok(())
    };
    for (index, rollup) in plan.rollups.iter().enumerate() {
        record('B', &[], false)?;
        if plan.sorts(index) {
            record('S', rollup.clause.columns(), true)?;
        }
        let hashed = if index == 0 {
            matches!(plan.strategy, SetsStrategy::Hashed | SetsStrategy::Mixed)
        } else {
            rollup.hashed
        };
        for set in rollup.sets {
            record(if hashed { 'H' } else { 'G' }, set.columns(), false)?;
        }
    }
    if keys.plain.is_truncated() || keys.verbose.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "the grouping sets' keys exceed EXPLAIN's 256-byte node property"
        ));
    }
    Ok(keys)
}

/// `cost_agg` of a plain aggregate over the chosen `path` of the one
/// relation `statement` reads: `None` when its select list computes more
/// than aggregates, columns and constants, or an aggregate is outside the
/// model of [`query::index_paths::aggregate_costs`].
fn plain_aggregate_cost<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Select<'a>,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    path: &query::index_paths::ScanPath<'_>,
    arena: &'a Arena,
) -> Result<Option<(f64, f64)>, SqlError> {
    let expressions = arena
        .alloc_slice_with(statement.items.len(), |_| &Expr::Null)
        .map_err(|_| arena_full())?;
    for (slot, item) in expressions.iter_mut().zip(statement.items) {
        let SelectItem::Expr { expression, .. } = item else {
            return Ok(None);
        };
        // The target list's own cost is outside the model.
        if !matches!(expression, Expr::Call { .. } | Expr::Column { .. })
            && !expression.is_constant(false)
        {
            return Ok(None);
        }
        if matches!(expression, Expr::Call { .. })
            && query::aggregate_routine(expression, storage, txid, columns)?.is_none()
        {
            return Ok(None);
        }
        *slot = expression;
    }
    let Some(costs) =
        query::index_paths::aggregate_costs(expressions, storage, txid, columns, arena)?
            .filter(|aggregates| !aggregates.ordered)
    else {
        return Ok(None);
    };
    let (startup, total, _) = query::index_paths::cost_agg(
        query::index_paths::AggStrategy::Plain,
        &costs,
        0,
        1.0,
        query::index_paths::AggInput {
            startup: path.startup_cost,
            total: path.total_cost,
            rows: path.rows,
            width: f64::from(path.width),
        },
    );
    Ok(Some((startup, total)))
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
                    query::ResolvedColumn::Merged(_) | query::ResolvedColumn::TableOid(_) => None,
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
    let non_null = 1.0 - f64::from(column_statistics.null_fraction);
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
            let non_null = 1.0 - f64::from(column.null_fraction);
            return Some((non_null / column.distinct_values.max(1) as f64).clamp(0.0, 1.0));
        }
    }
    None
}

/// PostgreSQL's `cpu_tuple_cost` and `cpu_operator_cost`.
const CPU_TUPLE_COST: f64 = 0.01;
const CPU_OPERATOR_COST: f64 = 0.0025;

/// `clamp_row_est`.
fn clamp_row_estimate(rows: f64) -> f64 {
    if rows <= 1.0 { 1.0 } else { rows.round() }
}

/// The operators and function calls PostgreSQL's `cost_qual_eval` charges
/// `cpu_operator_cost` each for when it evaluates `expression`.
fn qual_operators(expression: Option<&Expr<'_>>) -> f64 {
    fn count(expression: &Expr<'_>) -> f64 {
        let own = match expression {
            Expr::Binary {
                operator: BinaryOp::And | BinaryOp::Or,
                ..
            } => 0.0,
            Expr::Binary { .. } | Expr::Call { .. } | Expr::Like { .. } | Expr::Match { .. } => 1.0,
            Expr::InList { list, .. } => list.len() as f64,
            _ => 0.0,
        };
        let mut children = 0.0;
        let _ = query::walk_children(expression, &mut |child| {
            children += count(child);
            Ok(())
        });
        own + children
    }
    expression.map_or(0.0, count)
}

/// The heap pages ANALYZE records for the table in `slot` (`relpages`):
/// its rows as PostgreSQL lays them out, each a 24-byte header, its
/// values at their alignment and a line pointer, packed into 8 kB pages.
fn heap_pages(storage: &Storage, slot: usize, txid: u32) -> f64 {
    let statistics = storage.table_statistics(slot, txid);
    let definition = storage.table_def(slot, txid);
    let align = |offset: usize, alignment: u8| {
        let to = match alignment {
            b'd' => 8,
            b'i' => 4,
            b's' => 2,
            _ => 1,
        };
        offset.next_multiple_of(to)
    };
    let mut data = 0usize;
    for (column, meta) in definition.columns().iter().enumerate() {
        let layout = meta.ctype.physical_layout();
        if layout.length > 0 {
            data = align(data, layout.alignment) + layout.length as usize;
            continue;
        }
        let width = definition
            .attributes
            .number(column)
            .and_then(|attribute| statistics.column(attribute))
            .map_or(32, |column| column.average_width as usize);
        data += if width < 127 {
            width + 1
        } else {
            align(data, layout.alignment) - data + width + 4
        };
    }
    // A row without NULLs: no null bitmap after the 23-byte header.
    let tuple = 24 + data.next_multiple_of(8) + 4;
    let per_page = (8168 / tuple).clamp(1, 291) as f64;
    (statistics.rows as f64 / per_page).ceil()
}

/// One relation of a joined write as PostgreSQL's planner costs it: the
/// rows its scan returns, the scan's total cost and the distinct values
/// of each hash key among those rows (`None` without statistics).
#[derive(Clone, Copy)]
struct CostedRelation {
    rows: f64,
    total: f64,
    distinct: [Option<f64>; 8],
}

/// Costs the sequential scans of `slots` (one relation, or a tree's
/// members under an `Append`) at `table` of `scope` under `condition`.
#[allow(clippy::too_many_arguments)]
fn costed_relation<'a>(
    storage: &Storage,
    scope: &mut QueryScope<'_>,
    table: usize,
    slots: &[usize],
    condition: Option<&'a Expr<'a>>,
    keys: &[Option<usize>],
    params: &[Datum<'a>],
    routine: Option<crate::sql::exec::RoutineScope<'_, 'a>>,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<CostedRelation>, SqlError> {
    let original = scope.slots[table];
    let operators = qual_operators(condition);
    let mut costed = CostedRelation {
        rows: 0.0,
        total: 0.0,
        distinct: [Some(0.0); 8],
    };
    for &slot in slots {
        scope.slots[table] = slot;
        let tuples = storage.planning_row_estimate(slot) as f64;
        // The rows a condition keeps, which PostgreSQL's statistics
        // estimate closely for an analyzed table.
        let rows = clamp_row_estimate(match condition {
            None => tuples,
            Some(condition) => {
                match crate::sql::exec::relation_rows_kept(
                    storage,
                    slot,
                    scope.names[table],
                    condition,
                    params,
                    routine,
                    txid,
                    arena,
                )? {
                    Some(rows) => rows as f64,
                    None => {
                        scope.slots[table] = original;
                        return Ok(None);
                    }
                }
            }
        });
        costed.rows += rows;
        costed.total += heap_pages(storage, slot, txid)
            + tuples * (CPU_TUPLE_COST + CPU_OPERATOR_COST * operators);
        let statistics = storage.table_statistics(slot, txid);
        let definition = storage.table_def(slot, txid);
        for (distinct, &column) in costed.distinct.iter_mut().zip(keys) {
            // `get_variable_numdistinct` of an expression without
            // statistics: the relation's rows when fewer than
            // `DEFAULT_NUM_DISTINCT`, otherwise that default.
            let Some(column) = column else {
                let values = (tuples < 200.0).then(|| clamp_row_estimate(tuples));
                *distinct = match (*distinct, values) {
                    (Some(sum), Some(values)) => Some(
                        sum + if tuples > 0.0 && rows < tuples {
                            clamp_row_estimate(values * rows / tuples)
                        } else {
                            values
                        },
                    ),
                    _ => None,
                };
                continue;
            };
            let Some(column_statistics) = definition
                .attributes
                .number(column)
                .and_then(|attribute| statistics.column(attribute))
                .filter(|_| statistics.valid)
            else {
                *distinct = None;
                continue;
            };
            let values = crate::storage::column_distinct_estimate(
                &definition.columns()[column],
                column_statistics,
                statistics.rows,
                false,
            );
            // `estimate_hash_bucketsize` scales the distinct values by the
            // fraction of rows the scan keeps.
            let scaled = if tuples > 0.0 && rows < tuples {
                clamp_row_estimate(values * rows / tuples)
            } else {
                values
            };
            *distinct = distinct.map(|sum| sum + scaled);
        }
    }
    scope.slots[table] = original;
    if slots.len() > 1 {
        // `cost_append`'s per-row charge.
        costed.total += CPU_TUPLE_COST * 0.5 * costed.rows;
    }
    for distinct in &mut costed.distinct {
        *distinct = distinct.map(|values| values.min(costed.rows).max(1.0));
    }
    Ok(Some(costed))
}

/// How PostgreSQL's planner joins a joined write's target, its members
/// `leaves`, and its FROM table under `join`: it costs both orders of a
/// hash join (`initial_cost_hashjoin` and `final_cost_hashjoin`) and of a
/// nested loop rescanning its inner relation or a `Materialize` of it, and
/// `add_path` keeps the path generated first unless another costs less by
/// more than its fuzz factor, in total cost, then startup
/// cost, then by any amount. Returns whether the FROM table is the outer
/// relation and the join method; `None` when a relation's condition cannot
/// be estimated.
#[allow(clippy::too_many_arguments)]
pub(crate) fn joined_write_shape<'a>(
    storage: &Storage,
    scope: &mut QueryScope<'_>,
    leaves: &[usize],
    join: &query::JoinedWriteJoin<'a>,
    params: &[Datum<'a>],
    routine: Option<crate::sql::exec::RoutineScope<'_, 'a>>,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<(bool, query::JoinedWriteMethod)>, SqlError> {
    let mut target_keys = [None; 8];
    let mut source_keys = [None; 8];
    for (index, key) in join.keys().iter().enumerate() {
        target_keys[index] = key.target_column;
        source_keys[index] = key.source_column;
    }
    let count = join.keys().len();
    let Some(target) = costed_relation(
        storage,
        scope,
        0,
        leaves,
        join.conditions.target,
        &target_keys[..count],
        params,
        routine,
        txid,
        arena,
    )?
    else {
        return Ok(None);
    };
    let Some(source) = costed_relation(
        storage,
        scope,
        1,
        &[join.source_slot],
        join.source_row_condition,
        &source_keys[..count],
        params,
        routine,
        txid,
        arena,
    )?
    else {
        return Ok(None);
    };
    let filter = CPU_OPERATOR_COST * qual_operators(join.conditions.join);
    let keys = count as f64;
    // (startup, total) of a nested loop with `outer` rescanning `inner`,
    // from a `Materialize` or by scanning it again.
    let nested = |outer: &CostedRelation, inner: &CostedRelation, materialized: bool| {
        let rescans = (outer.rows - 1.0).max(0.0);
        let inner_cost = if materialized {
            inner.total
                + 2.0 * CPU_OPERATOR_COST * inner.rows
                + rescans * CPU_OPERATOR_COST * inner.rows
        } else {
            inner.total * (1.0 + rescans)
        };
        let qual = CPU_TUPLE_COST + CPU_OPERATOR_COST * keys + filter;
        (
            0.0,
            outer.total + inner_cost + outer.rows * inner.rows * qual,
        )
    };
    // (startup, total) of a hash join with `outer` probing `inner`.
    let hashed = |outer: &CostedRelation, inner: &CostedRelation| {
        let startup = inner.total + (CPU_OPERATOR_COST * keys + CPU_TUPLE_COST) * inner.rows;
        let buckets = (inner.rows.max(1.0) as u64).next_power_of_two().max(1024) as f64;
        // `estimate_hash_bucketsize`: the smallest bucket fraction of the
        // keys, 0.1 for a key without statistics.
        let fraction = inner.distinct[..count]
            .iter()
            .map(|distinct| {
                distinct.map_or(0.1_f64.max(1.0 / buckets), |values| {
                    (if values > buckets {
                        1.0 / buckets
                    } else {
                        1.0 / values
                    })
                    .clamp(1.0e-6, 1.0)
                })
            })
            .fold(1.0_f64, f64::min);
        let matches = (0..count).fold(outer.rows * inner.rows, |rows, key| {
            let distinct = |relation: &CostedRelation| relation.distinct[key].unwrap_or(200.0);
            rows / distinct(outer).max(distinct(inner))
        });
        let total = startup
            + outer.total
            + CPU_OPERATOR_COST * keys * outer.rows
            + CPU_OPERATOR_COST
                * keys
                * outer.rows
                * clamp_row_estimate(inner.rows * fraction)
                * 0.5
            + (CPU_TUPLE_COST + filter) * clamp_row_estimate(matches);
        (startup, total)
    };
    // The paths in the order `add_paths_to_joinrel` generates them: the
    // target outer, then the FROM table outer; per order the nested loops
    // (`match_unsorted_outer`), then the hash join.
    let mut paths = [(0.0, 0.0, false, query::JoinedWriteMethod::Rescanned); 6];
    let mut generated = 0;
    for (source_outer, outer, inner) in [(false, &target, &source), (true, &source, &target)] {
        for method in [
            query::JoinedWriteMethod::Rescanned,
            query::JoinedWriteMethod::Materialized,
        ] {
            if method == query::JoinedWriteMethod::Materialized
                && !crate::sql::guc::planner_methods().material
            {
                continue;
            }
            let (startup, total) = nested(
                outer,
                inner,
                method == query::JoinedWriteMethod::Materialized,
            );
            paths[generated] = (startup, total, source_outer, method);
            generated += 1;
        }
        if count != 0 {
            let (startup, total) = hashed(outer, inner);
            paths[generated] = (startup, total, source_outer, query::JoinedWriteMethod::Hash);
            generated += 1;
        }
    }
    // `compare_path_costs_fuzzily` of `new` against `old`: `Some(true)`
    // when `new` is better, `Some(false)` when `old` is.
    let compare = |new: (f64, f64), old: (f64, f64), fuzz: f64| -> Option<bool> {
        if new.1 > old.1 * fuzz {
            Some(false)
        } else if old.1 > new.1 * fuzz {
            Some(true)
        } else if new.0 > old.0 * fuzz {
            Some(false)
        } else if old.0 > new.0 * fuzz {
            Some(true)
        } else {
            None
        }
    };
    // `add_path`: a new path displaces the kept ones it is fuzzily better
    // than, or is rejected by one fuzzily better; among fuzzily equal
    // paths of the same rows the strictly cheaper stays.
    let mut kept = [None::<usize>; 6];
    for new in 0..generated {
        let cost = (paths[new].0, paths[new].1);
        let mut accept = true;
        for slot in kept.iter_mut() {
            let Some(old) = *slot else { continue };
            let old_cost = (paths[old].0, paths[old].1);
            match compare(cost, old_cost, 1.01).or_else(|| compare(cost, old_cost, 1.000_000_000_1))
            {
                Some(true) => *slot = None,
                Some(false) | None => {
                    accept = false;
                    break;
                }
            }
        }
        if accept {
            kept[new] = Some(new);
        }
    }
    // `set_cheapest`: the cheapest total cost, the earliest on a tie.
    let chosen = kept.iter().flatten().copied().reduce(|best, path| {
        if paths[path].1 < paths[best].1 {
            path
        } else {
            best
        }
    });
    Ok(chosen.map(|path| (paths[path].2, paths[path].3)))
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
            let fraction = f64::from(column_statistics.null_fraction);
            if *negated { 1.0 - fraction } else { fraction }
        }
        Expr::InList {
            operand,
            list,
            negated: false,
            ..
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

/// The scan node of the path PostgreSQL's planner chose for the one
/// relation of `scope`, with the index conditions it consumes.
fn chosen_scan_node<'a>(
    scope: &QueryScope<'a>,
    depth: u8,
    path: &query::index_paths::ScanPath<'a>,
) -> (PlanNode, Option<query::IndexAccessPlan<'a>>) {
    use query::index_paths::ScanKind;
    let definition = scope.defs.stored(0).expect("a chosen path scans a table");
    let mut name = StackStr::new();
    let mut bitmap_index = StackStr::new();
    match path.kind {
        ScanKind::Seq => {
            let _ = write!(name, "Seq Scan");
        }
        ScanKind::Index | ScanKind::IndexOnly => {
            let _ = write!(
                name,
                "Index {}Scan {}using {}",
                if path.kind == ScanKind::IndexOnly {
                    "Only "
                } else {
                    ""
                },
                if path.backward { "Backward " } else { "" },
                path.index_name.as_str()
            );
        }
        ScanKind::Bitmap => {
            let _ = write!(name, "Bitmap Heap Scan");
            // A bitmap of one index scan is the heap scan's own child; a
            // tree's nodes follow the scan.
            if let [node] = path.bitmap
                && let query::index_paths::BitmapPlanKind::Index { index_name, .. } = node.kind
            {
                bitmap_index = index_name;
            }
        }
    }
    let mut relation = StackStr::new();
    let mut schema = StackStr::new();
    let mut alias = StackStr::new();
    let exposed = crate::sql::catalog::deparse::printed_name(scope.names[0]);
    let _ = write!(relation, "{}", definition.name.as_str());
    let _ = write!(schema, "{}", definition.schema.as_str());
    if exposed != definition.name.as_str() {
        let _ = write!(alias, "{exposed}");
    }
    let node = PlanNode {
        name,
        bitmap_index,
        relation,
        alias,
        depth,
        startup_cost: path.startup_cost,
        total_cost: path.total_cost,
        rows: path.rows as u64,
        width: path.width,
        schema,
        bitmap_index_estimate: match path.bitmap {
            [node] => Some((node.total_cost, node.rows as u64)),
            _ => None,
        },
        // An index scan that searches an array, or bridges a key column
        // with a skip array, over several leaf pages descends the index as
        // often as its keys fall on different leaf pages.
        analyze_unsupported: path.searches_unmodeled.then_some(
            "EXPLAIN ANALYZE of an index scan searching an array or skipping a key column over several leaf pages is not supported",
        ),
        ..PlanNode::EMPTY
    };
    let used = (path.kind != ScanKind::Seq).then(|| query::IndexAccessPlan::chosen(path));
    (node, used)
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
    chosen: Option<query::index_paths::ScanPath<'a>>,
) -> Result<(PlanNode, Option<query::IndexAccessPlan<'a>>), SqlError> {
    let slot = scope.slots[table];
    if let Some(path) = chosen.filter(|_| table == 0) {
        return Ok(chosen_scan_node(scope, depth, &path));
    }
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
    let use_index = index_identity.is_some_and(|(plan, _)| {
        !derived
            && plan.chosen_for_scan(
                storage,
                slot,
                scope.defs.stored(table).expect("base table"),
                txid,
                ordered.is_some(),
            )
    });
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
    let mut schema = StackStr::new();
    let mut alias = StackStr::new();
    let exposed = crate::sql::catalog::deparse::printed_name(scope.names[table]);
    match scope.defs.stored(table).filter(|_| !derived) {
        Some(definition) => {
            let _ = write!(relation, "{}", definition.name.as_str());
            let _ = write!(schema, "{}", definition.schema.as_str());
            if exposed != definition.name.as_str() {
                let _ = write!(alias, "{exposed}");
            }
        }
        None => {
            let _ = write!(alias, "{exposed}");
        }
    }
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
    let node = PlanNode {
        name,
        bitmap_index,
        relation,
        alias,
        output: StackStr::new(),
        depth,
        startup_cost: 0.0,
        total_cost: io_cost + cpu_cost,
        rows: output_rows,
        width,
        object_requests,
        cache_blocks,
        constant_false: false,
        quals: NO_QUALS,
        keys: PlanKeys::NONE,
        key: None,
        hash_columns: None,
        verbose_quals: NO_QUALS,
        schema,
        subplan: None,
        shift: 0,
        relations: StackStr::new(),
        plain_relations: StackStr::new(),
        output_unprintable: false,
        // Without a modeled path a table's estimates are pos3ql's own; a
        // function's, `WITH` query's or subquery's are set by its caller.
        unmodeled_costs: !derived,
        remote_sql: StackStr::new(),
        batch_size: None,
        passthrough: false,
        subplans_removed: None,
        target_member: false,
        scan_pruned: false,
        uncounted: false,
        inner_unique: false,
        bitmap_index_estimate: None,
        analyze_unsupported: None,
        planned_partitions: None,
        hash_agg: None,
        initplan_charge: 0.0,
    };
    let used = index_identity
        .filter(|_| use_index)
        .map(|(access, _)| access);
    Ok((node, used))
}

pub(super) fn plan_select<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    if statement.with.is_empty() {
        return plan_select_with_ctes(storage, txid, statement, &[], outer, block, arena);
    }
    // A query's own `WITH` list follows the statement's rule
    // (`SS_process_ctes`) at every query level: a query run once is an
    // InitPlan of the query's top node, the others are inlined.
    let (select, ctes) = query::expand_ctes_for_plan(statement, storage, txid, arena)?;
    let prior =
        SUBPLAN_RELATIONS.with(|planned| planned.replace(planned.get() || !ctes.is_empty()));
    let plan = plan_ctes(storage, txid, ctes, outer, block, arena).and_then(|ctes| {
        // A set operation with its own `WITH` list keeps the list's
        // InitPlans on its top node.
        if let Some(body) = select.set_body {
            let set_query = SetQuery {
                with: &[],
                body,
                order_by: select.order_by,
                limit: select.limit,
                offset: select.offset,
                with_ties: select.with_ties,
                locking: select.locking,
            };
            let plan = plan_set_query(storage, txid, &set_query, outer, block, arena)?;
            return attach_subplans(plan, 0, block, ctes, arena);
        }
        plan_select_with_ctes(storage, txid, select, ctes, outer, block, arena)
    });
    SUBPLAN_RELATIONS.with(|planned| planned.set(prior));
    plan
}

/// [`plan_select`] for a query whose `WITH` queries `ctes` run once, as
/// InitPlans of its top node ahead of its subqueries'.
fn plan_select_with_ctes<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    ctes: &[Sublink<'a>],
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    // A lone `min` or `max` of a foreign table is planned as the InitPlan
    // execution runs.
    if let Some(rewritten) =
        crate::sql::foreign::deparse::minmax_subquery(storage, txid, statement, arena)?
    {
        return plan_select_with_ctes(storage, txid, rewritten, ctes, outer, block, arena);
    }
    // `min` and `max` of a local table are planned, and executed, as
    // PostgreSQL plans them when an index orders them, also in a subquery
    // reading no enclosing query's names.
    if (outer.is_none() || !query::select_correlated(statement, storage, txid, arena)?)
        && let Some(rewritten) = query::local_minmax_subquery(storage, txid, statement, arena)?
    {
        return plan_select_with_ctes(storage, txid, rewritten, ctes, None, block, arena);
    }
    // Execution plans the same query: simple subqueries merged into it and
    // outer joins its conditions narrow reduced.
    let outer_types = outer.map(query::OuterColumnTypes);
    let pulled = query::pull_up_subqueries(
        statement,
        storage,
        txid,
        arena,
        outer_types
            .as_ref()
            .map(|types| types as &dyn ColumnLookup<'_>),
    )?;
    // A subquery reads the columns of the query it is in, and of that
    // query's enclosing queries.
    let scope = pulled
        .from
        .as_ref()
        .map(|from| QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    let scope_types = scope.as_ref().map(query::ScopeTypes);
    let enclosed = scope_types.as_ref().map(|types| query::Chained {
        inner: types,
        outer: outer_types
            .as_ref()
            .map(|types| types as &dyn ColumnLookup<'_>),
    });
    let enclosing = match enclosed.as_ref() {
        Some(enclosed) => Some(enclosed as &dyn ColumnLookup<'_>),
        None => outer,
    };
    // PostgreSQL plans, and so numbers, a query's subqueries before it
    // prints the conditions that refer to them.
    let sublinks = plan_sublinks(storage, txid, pulled, enclosing, block, arena)?;
    let sublinks = if ctes.is_empty() {
        sublinks
    } else {
        arena
            .alloc_slice_with(ctes.len() + sublinks.len(), |index| {
                ctes.get(index)
                    .copied()
                    .unwrap_or_else(|| sublinks[index - ctes.len()])
            })
            .map_err(|_| arena_full())?
    };
    let plan = plan_pulled_select(storage, txid, statement, pulled, outer, block, arena)?;
    let relations = pulled.from.as_ref().map_or(0, |from| from.joins.len() + 1);
    attach_subplans(plan, relations, block, sublinks, arena)
}

/// Where a query reads a subquery, which decides the node that runs it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SublinkOrigin {
    Items,
    Where,
    Having,
}

/// A subquery of a query level planned as a subplan.
#[derive(Clone, Copy)]
struct Sublink<'a> {
    kind: SubplanKind,
    number: u16,
    origin: SublinkOrigin,
    nodes: &'a [PlanNode],
    /// The name of the `WITH` query this InitPlan runs (`CTE name`).
    cte: Option<&'a str>,
    /// Whether the subquery is an `EXISTS` test, which reads one row.
    exists: bool,
}

impl Sublink<'_> {
    /// What PostgreSQL's `cost_subplan` charges for running this InitPlan
    /// once (`startup_cost` plus `per_call_cost`): its plan's total cost,
    /// or for `EXISTS` its startup cost and the run cost of one row.
    fn initplan_cost(&self) -> f64 {
        let top = &self.nodes[0];
        if self.exists {
            top.startup_cost
                + (top.total_cost - top.startup_cost) / clamp_row_estimate(top.rows as f64)
        } else {
            top.total_cost
        }
    }
}

thread_local! {
    /// How many subqueries deep the query being planned is: a subquery's
    /// range table entries join its statement's.
    static SUBLINK_DEPTH: core::cell::Cell<u16> = const { core::cell::Cell::new(0) };
    /// Whether the statement being planned runs subplans whose relations
    /// join the statement's range table: `WITH` queries run as their own
    /// plans, or a write's InitPlans that read relations.
    static SUBPLAN_RELATIONS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// Whether a relation pruning leaves no member prints unnamed, and
    /// whether planning met one.
    static NAMELESS_DUMMIES: core::cell::Cell<(bool, bool)> =
        const { core::cell::Cell::new((false, false)) };
    /// Whether the statement being planned scans a partitioned table or an
    /// inheritance tree whose members joined its range table.
    static EXPANDED_RELATIONS: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
    /// The single stored relation of each query level around the one being
    /// planned, by exposed name and slot, which a correlated subquery's
    /// conditions name qualified.
    static OUTER_RELATIONS: core::cell::RefCell<([(StackStr<64>, usize); 8], usize)> =
        const { core::cell::RefCell::new(([(StackStr::new(), 0); 8], 0)) };
    /// How many of the top [`OUTER_RELATIONS`] a join's parameterized scan
    /// reads, all of which its conditions print qualified.
    static JOIN_OUTER_RELATIONS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Pushes the relations of `mask` in `scope` as the ones whose values a
/// join's parameterized scan reads, returning how many it pushed.
fn push_outer_mask(scope: &QueryScope<'_>, mask: u32) -> usize {
    OUTER_RELATIONS.with(|outer| {
        let mut outer = outer.borrow_mut();
        let mut pushed = 0;
        for table in 0..scope.n.min(32) {
            if mask & 1 << table == 0 {
                continue;
            }
            let count = outer.1;
            if count == outer.0.len() {
                break;
            }
            outer.0[count] = (StackStr::from_str(scope.names[table]), scope.slots[table]);
            outer.1 = count + 1;
            pushed += 1;
        }
        JOIN_OUTER_RELATIONS.with(|join| join.set(pushed));
        pushed
    })
}

/// Pops relations [`push_outer_mask`] pushed.
fn pop_outer_relations(pushed: usize) {
    JOIN_OUTER_RELATIONS.with(|join| join.set(0));
    OUTER_RELATIONS.with(|outer| outer.borrow_mut().1 -= pushed);
}

/// Pushes the stored relations left of `table` in `scope` as relations
/// around the query being planned, returning how many it pushed.
fn push_outer_relations(scope: &QueryScope<'_>, table: usize) -> usize {
    OUTER_RELATIONS.with(|outer| {
        let mut outer = outer.borrow_mut();
        let mut pushed = 0;
        for left in 0..table {
            let slot = scope.slots[left];
            if scope.derived[left].is_some() || slot == usize::MAX {
                continue;
            }
            let count = outer.1;
            if count == outer.0.len() {
                break;
            }
            outer.0[count] = (StackStr::from_str(scope.names[left]), slot);
            outer.1 = count + 1;
            pushed += 1;
        }
        pushed
    })
}

/// The relations of the query levels around the one being planned.
fn outer_relations(
    storage: &Storage,
    txid: u32,
    mut visit: impl FnMut(&str, &crate::storage::TableDef),
) {
    OUTER_RELATIONS.with(|outer| {
        let outer = outer.borrow();
        for (name, slot) in &outer.0[..outer.1] {
            visit(name.as_str(), &storage.table_def(*slot, txid));
        }
    });
}

/// Plans the select-list, `WHERE` and `HAVING` subqueries of `statement`
/// that EXPLAIN shows as subplans, numbering each after the subqueries
/// inside it.
fn plan_sublinks<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<&'a [Sublink<'a>], SqlError> {
    let mut found = crate::mem::arena::ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            visit_sublinks(expression, &mut |node| {
                // One occurrence the select list reads twice, such as a
                // min/max aggregate's InitPlan, is one subplan.
                if !found
                    .as_slice()
                    .iter()
                    .any(|(seen, _): &(&Expr<'_>, SublinkOrigin)| core::ptr::eq(*seen, node))
                {
                    let _ = found.push((node, SublinkOrigin::Items));
                }
            });
        }
    }
    if let Some(condition) = statement.where_clause {
        visit_sublinks(condition, &mut |node| {
            let _ = found.push((node, SublinkOrigin::Where));
        });
    }
    if let Some(having) = statement.having {
        visit_sublinks(having, &mut |node| {
            let _ = found.push((node, SublinkOrigin::Having));
        });
    }
    // A `LIMIT` or `OFFSET` subquery is an InitPlan of the top node.
    for count in statement.limit.into_iter().chain(statement.offset) {
        visit_sublinks(count, &mut |node| {
            let _ = found.push((node, SublinkOrigin::Where));
        });
    }
    let found = found.as_slice();
    let relation = statement.from.as_ref().and_then(|from| {
        (from.joins.is_empty() && from.base.subquery.is_none() && !from.base.is_function_source())
            .then(|| {
                (
                    from.base.alias.unwrap_or(from.base.table),
                    from.base.schema,
                    from.base.table,
                )
            })
    });
    let mut planned = crate::mem::arena::ArenaList::new(arena);
    for &(node, origin) in found {
        let Some(select) = sublink_query(node) else {
            continue;
        };
        let correlated = query::subquery_node_correlated(node, storage, txid, arena)?;
        let outer_slot =
            relation.and_then(|(_, schema, table)| relation_slot(storage, txid, schema, table));
        if correlated && outer_slot.is_none() {
            continue;
        }
        let pushed = correlated
            && OUTER_RELATIONS.with(|outer| {
                let mut outer = outer.borrow_mut();
                let count = outer.1;
                if count == outer.0.len() {
                    return false;
                }
                outer.0[count] = (
                    StackStr::from_str(relation.expect("checked").0),
                    outer_slot.expect("checked"),
                );
                outer.1 = count + 1;
                true
            });
        SUBLINK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        let inner = plan_select(
            storage,
            txid,
            select,
            outer,
            block.child(Segment::Sublink(select.instrument_identity())),
            arena,
        );
        SUBLINK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        if pushed {
            OUTER_RELATIONS.with(|outer| outer.borrow_mut().1 -= 1);
        }
        let Plan { nodes, count, .. } = inner?;
        // `simplify_EXISTS_query` drops an `EXISTS` subquery's output.
        if matches!(node, Expr::Exists(_)) {
            nodes[0].width = 0;
        }
        let compared = Compared::of(node);
        // PostgreSQL rescans an uncorrelated subquery it compares row by
        // row from a materialized copy, unless `enable_material` is off or
        // its top node already keeps its output (`ExecMaterializesOutput`).
        let materializes = matches!(
            nodes[0].name.as_str(),
            "Materialize"
                | "Function Scan"
                | "Table Function Scan"
                | "CTE Scan"
                | "WorkTable Scan"
                | "Sort"
        );
        let nodes: &'a [PlanNode] = if !correlated
            && compared == Compared::Scanned
            && crate::sql::guc::planner_methods().material
            && !materializes
        {
            arena
                .alloc_slice_with(count + 1, |index| match index {
                    0 => PlanNode {
                        name: StackStr::from_str("Materialize"),
                        total_cost: nodes[0].total_cost,
                        rows: nodes[0].rows,
                        width: nodes[0].width,
                        output: nodes[0].output,
                        output_unprintable: nodes[0].output_unprintable,
                        ..PlanNode::EMPTY
                    },
                    _ => PlanNode {
                        depth: nodes[index - 1].depth + 1,
                        ..nodes[index - 1]
                    },
                })
                .map_err(|_| arena_full())?
        } else {
            &nodes[..count]
        };
        let number = match register_subplan(select, correlated, compared)? {
            crate::sql::catalog::deparse::SublinkPlan::InitPlan(number)
            | crate::sql::catalog::deparse::SublinkPlan::SubPlan(number)
            | crate::sql::catalog::deparse::SublinkPlan::HashedSubPlan(number) => number,
        };
        planned
            .push(Sublink {
                kind: if correlated || compared != Compared::No {
                    SubplanKind::SubPlan
                } else {
                    SubplanKind::InitPlan
                },
                number,
                origin,
                nodes,
                cte: None,
                exists: matches!(node, Expr::Exists(_)),
            })
            .map_err(|_| arena_full())?;
    }
    Ok(planned.into_mut_slice().map_err(|_| arena_full())?)
}

/// Plans each `WITH` query of the statement PostgreSQL runs once as an
/// InitPlan `CTE name` of the statement's top node. `SS_process_ctes` plans
/// them before the query's own subqueries, so they take the first subplan
/// numbers.
fn plan_ctes<'a>(
    storage: &Storage,
    txid: u32,
    ctes: &[query::PlannedCte<'_>],
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<&'a [Sublink<'a>], SqlError> {
    let mut planned = crate::mem::arena::ArenaList::new(arena);
    for cte in ctes {
        let cte_block = block.child(Segment::Cte(cte.position));
        let plan = match cte.query {
            // Execution runs the query as a set operation's one branch.
            query::PlannedCteQuery::Select(select) => plan_derived(
                storage,
                txid,
                select,
                outer,
                cte_block.child(Segment::Branch(0)),
                arena,
            )?,
            query::PlannedCteQuery::Recursive(select) => {
                plan_recursive_union(storage, txid, cte.name, select, outer, cte_block, arena)?
            }
            query::PlannedCteQuery::Modification(statement) => {
                plan_modification(storage, txid, statement, cte_block, arena)?
            }
        };
        let number = reserve_subplan()?;
        planned
            .push(Sublink {
                kind: SubplanKind::InitPlan,
                number,
                origin: SublinkOrigin::Items,
                nodes: &plan.nodes[..plan.count],
                cte: Some(arena.alloc_str(cte.name).map_err(|_| arena_full())?),
                exists: false,
            })
            .map_err(|_| arena_full())?;
    }
    Ok(planned.into_mut_slice().map_err(|_| arena_full())?)
}

/// The plan of a recursive `WITH` query: a `Recursive Union` of its
/// non-recursive term and its recursive term, whose self-references scan
/// the working table (`WorkTable Scan`).
fn plan_recursive_union<'a>(
    storage: &Storage,
    txid: u32,
    name: &str,
    select: &Select<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let Some(SetTree::Op { left, right, .. }) = select.set_body else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "recursive WITH query \"{}\" is not a UNION",
            name
        ));
    };
    let mut terms = [None, None];
    for (position, (term, slot)) in [left, right].into_iter().zip(&mut terms).enumerate() {
        let term_block = block.child(Segment::Branch(
            u16::try_from(position).map_err(|_| arena_full())?,
        ));
        *slot = Some(match term {
            SetTree::Select(select) => plan_derived(
                storage,
                txid,
                select,
                outer,
                term_block.child(Segment::Branch(0)),
                arena,
            )?,
            SetTree::Op { .. } => plan_set_query(
                storage,
                txid,
                &SetQuery {
                    with: &[],
                    body: term,
                    order_by: &[],
                    limit: None,
                    offset: None,
                    with_ties: false,
                    locking: &[],
                },
                outer,
                term_block,
                arena,
            )?,
        });
    }
    let [Some(base), Some(recursive)] = terms else {
        unreachable!("both terms are planned");
    };
    let mut plan = Plan::new(arena, 1 + base.count + recursive.count)?;
    plan.push(PlanNode {
        name: StackStr::from_str("Recursive Union"),
        key: Some(NodeKey::new(block, Role::SetOperation(0))),
        rows: base.nodes[0].rows.saturating_mul(10),
        width: base.nodes[0].width,
        total_cost: base.nodes[0].total_cost + recursive.nodes[0].total_cost * 10.0,
        ..PlanNode::EMPTY
    })?;
    for node in &base.nodes[..base.count] {
        plan.push(PlanNode {
            depth: node.depth + 1,
            ..*node
        })?;
    }
    for node in &recursive.nodes[..recursive.count] {
        let mut node = *node;
        node.depth += 1;
        // The recursive term reads the query's own rows from the working
        // table.
        if node.name.as_str() == "CTE Scan" && node.relation.as_str() == name {
            node.name = StackStr::from_str("WorkTable Scan");
        }
        plan.push(node)?;
    }
    Ok(plan)
}

/// `plan` with each subplan under the node that runs it: an InitPlan under
/// the query's top node, before its inputs, and a SubPlan after the inputs
/// of the node evaluating the expression that reads it.
fn attach_subplans<'a>(
    plan: Plan<'a>,
    relations: usize,
    block: BlockId,
    sublinks: &[Sublink<'a>],
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    if sublinks.is_empty() {
        return Ok(plan);
    }
    let nodes = &plan.nodes[..plan.count];
    let find = |role: Role| {
        nodes
            .iter()
            .position(|node| node.key == Some(NodeKey::new(block, role)))
    };
    let scan = || match relations {
        // A query without FROM evaluates its conditions in its `Result`.
        0 => None,
        1 => find(Role::Scan(0)),
        _ => find(Role::Join(u16::try_from(relations - 1).unwrap_or(u16::MAX))),
    };
    let host = |sublink: &Sublink<'_>| -> usize {
        let at = match (sublink.kind, sublink.origin) {
            (SubplanKind::InitPlan, _) => None,
            (SubplanKind::SubPlan, SublinkOrigin::Having) => find(Role::Aggregate),
            (SubplanKind::SubPlan, SublinkOrigin::Items) => find(Role::SortedProjection)
                .or_else(|| find(Role::Aggregate))
                .or_else(|| find(Role::Result))
                .or_else(scan),
            (SubplanKind::SubPlan, SublinkOrigin::Where) => scan(),
        };
        at.unwrap_or(0)
    };
    let total = sublinks
        .iter()
        .fold(plan.count, |total, sublink| total + sublink.nodes.len());
    let mut attached = Plan::new(arena, total)?;
    attached.methods = plan.methods;
    attached.planning_micros = plan.planning_micros;
    // PostgreSQL names each relation of the statement's range table
    // uniquely, in order: the query's own first, then each subplan's, a
    // repeated name taking the first free `_1`, `_2`, ... suffix.
    let used = arena
        .alloc_slice_with(total, |_| StackStr::<96>::new())
        .map_err(|_| arena_full())?;
    let mut used_count = 0usize;
    for node in nodes {
        if !node.relation.as_str().is_empty() && used_count < used.len() {
            used[used_count] = StackStr::from_str(node.exposed_name());
            used_count += 1;
        }
    }
    let ordered = arena
        .alloc_slice_with(sublinks.len(), |index| index)
        .map_err(|_| arena_full())?;
    ordered.sort_unstable_by_key(|&index| sublinks[index].number);
    let renamed = arena
        .alloc_slice_with(total, |_| None::<StackStr<96>>)
        .map_err(|_| arena_full())?;
    let mut offsets = [0usize; MAX_SUBPLANS];
    let mut offset = 0usize;
    for (index, sublink) in sublinks.iter().enumerate() {
        offsets[index.min(MAX_SUBPLANS - 1)] = offset;
        offset += sublink.nodes.len();
    }
    for &index in ordered.iter() {
        for (at, node) in sublinks[index].nodes.iter().enumerate() {
            if node.relation.as_str().is_empty() {
                continue;
            }
            // A write's scan of its target reads the write's own range
            // table entry.
            if let Some(target) = sublinks[index].nodes[..at].iter().position(|earlier| {
                matches!(
                    earlier.name.as_str(),
                    "Insert" | "Update" | "Delete" | "Merge"
                ) && earlier.relation == node.relation
                    && earlier.alias == node.alias
            }) {
                renamed[offsets[index] + at] = renamed[offsets[index] + target];
                continue;
            }
            let name = node.exposed_name();
            let mut unique = StackStr::<96>::from_str(name);
            let mut suffix = 1usize;
            while used[..used_count]
                .iter()
                .any(|held| held.as_str() == unique.as_str())
            {
                unique = StackStr::new();
                let _ = write!(unique, "{name}_{suffix}");
                suffix += 1;
            }
            if unique.as_str() != name {
                renamed[offsets[index] + at] = Some(unique);
            }
            if used_count < used.len() {
                used[used_count] = unique;
                used_count += 1;
            }
        }
    }
    let push_subplan = |attached: &mut Plan<'a>, parent: PlanNode, index: usize| {
        let sublink = &sublinks[index];
        for (at, node) in sublink.nodes.iter().enumerate() {
            // The conditions and lists of a renamed relation's query block
            // name it as renamed.
            let mut node = *node;
            for (other, renamed_node) in sublink.nodes.iter().enumerate() {
                let Some(new) = renamed[offsets[index] + other] else {
                    continue;
                };
                if renamed_node.key.map(|key| key.block) == node.key.map(|key| key.block) {
                    node.requalify(renamed_node.exposed_name(), new.as_str());
                }
            }
            let node = &node;
            let mut name = StackStr::<72>::new();
            match sublink.cte {
                Some(cte) => {
                    let _ = write!(name, "CTE {cte}");
                }
                None => {
                    let _ = write!(name, "{} {}", sublink.kind.relationship(), sublink.number);
                }
            }
            attached.push(PlanNode {
                depth: node.depth + parent.depth + 1,
                shift: node.shift + parent.shift + 1,
                subplan: if at == 0 {
                    Some((sublink.kind, name))
                } else {
                    node.subplan
                },
                initplan_charge: if at == 0 && sublink.kind == SubplanKind::InitPlan {
                    sublink.initplan_cost()
                } else {
                    node.initplan_charge
                },
                alias: renamed[offsets[index] + at].unwrap_or(node.alias),
                ..*node
            })?;
        }
        Ok::<(), SqlError>(())
    };
    // `SS_charge_for_initplans`: the query's top node pays for running its
    // InitPlans once, before its first row.
    let initplan_cost = sublinks
        .iter()
        .filter(|sublink| sublink.kind == SubplanKind::InitPlan)
        .map(Sublink::initplan_cost)
        .sum::<f64>();
    // InitPlans precede the inputs of the top node, after a write's result
    // relations, which are its properties.
    let initplans_after = nodes
        .iter()
        .skip(1)
        .take_while(|node| node.target_member)
        .count();
    // A gating `Result` takes its costs from the plan it gates, which
    // already paid them (`create_gating_plan`).
    let charged = if nodes[0].name.as_str() == "Result"
        && nodes[0].passthrough
        && nodes
            .get(1)
            .is_some_and(|child| child.depth == nodes[0].depth + 1)
    {
        2
    } else {
        1
    };
    for (index, node) in nodes.iter().enumerate() {
        if index < charged {
            attached.push(PlanNode {
                startup_cost: if charged == 2 {
                    nodes[charged - 1].startup_cost
                } else {
                    node.startup_cost
                } + initplan_cost,
                total_cost: node.total_cost + initplan_cost,
                ..*node
            })?;
        } else {
            attached.push(*node)?;
        }
        if index == initplans_after {
            for (at, _) in sublinks
                .iter()
                .enumerate()
                .filter(|(_, sublink)| sublink.kind == SubplanKind::InitPlan)
            {
                push_subplan(&mut attached, nodes[0], at)?;
            }
        }
        // SubPlans follow the last node of their host's subtree: each
        // ancestor of this node whose subtree ends here, deepest first.
        let next_depth = nodes.get(index + 1).map(|next| next.depth);
        let mut shallowest = u8::MAX;
        for (host_index, host_node) in nodes[..=index].iter().enumerate().rev() {
            if host_node.depth >= shallowest {
                continue;
            }
            shallowest = host_node.depth;
            if next_depth.is_some_and(|next| next > host_node.depth) {
                break;
            }
            for (at, _) in sublinks.iter().enumerate().filter(|(_, sublink)| {
                sublink.kind == SubplanKind::SubPlan && host(sublink) == host_index
            }) {
                push_subplan(&mut attached, *host_node, at)?;
            }
            if host_node.depth == 0 {
                break;
            }
        }
    }
    Ok(attached)
}

/// [`plan_select`] of `statement` after its subqueries are pulled up into
/// `pulled`.
fn plan_pulled_select<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    pulled: &Select<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let key = |role| Some(NodeKey::new(block, role));
    let started = std::time::Instant::now();
    let written = statement;
    let statement = pulled;
    // A query of one foreign table plans what execution sends: the local
    // plan covers what the remote query leaves.
    let foreign_scan = crate::sql::foreign::deparse::plan_select(storage, txid, statement, arena)?;
    if let Some(pushed) = &foreign_scan
        && pushed.upper.is_some()
    {
        // A query pulled up from a view or subquery, or planned as one, has
        // more range table entries than its one relation, so its output
        // qualifies columns (`show_plan_tlist`).
        let qualified =
            shares_range_table(statement) || nested_block() || pulled_relations(written);
        let mut plan =
            foreign_upper_plan(storage, txid, statement, pushed, qualified, block, arena)?;
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    let statement = match &foreign_scan {
        Some(pushed) => pushed.local_statement(statement, arena)?,
        None => statement,
    };
    // The plan reads `USING` and `NATURAL` joins by their conditions, as
    // execution does.
    let scope = match &statement.from {
        Some(from) => Some(QueryScope::resolve_schema_with_join_conditions(
            storage, from, txid, arena, outer,
        )?),
        None => None,
    };
    if let Some(scope) = scope.as_ref() {
        query::validate_static_grouping(statement, scope, outer, storage, txid, arena)?;
    }
    let expansions = match (scope.as_ref(), statement.from.as_ref()) {
        (Some(scope), Some(from)) => {
            relation_expansions(storage, txid, scope, statement, from, arena)?
        }
        _ => &[],
    };
    let members: usize = expansions
        .iter()
        .flatten()
        .map(|expansion| expansion.started.len() + 1)
        .sum();
    let capacity = scope.as_ref().map_or(MAX_EXPLAIN_STAGES, |scope| {
        scope.n * 3 + MAX_EXPLAIN_STAGES + members
    });
    let mut plan = Plan::new(arena, capacity)?;
    let (mut width, mut output) =
        projected_shape(statement, scope.as_ref(), outer, storage, txid, arena)?;
    if let (Some(scope), Some(from)) = (scope.as_ref(), statement.from.as_ref())
        && scope.n == 1
        && let Some(list) = from.base.subquery.and_then(values_list)
        && let Some(named) = values_output(statement, scope, list)
    {
        output = named;
    }
    let scoped_columns = scope.as_ref().map(|scope| query::CatalogScopeCols {
        scope,
        outer,
        storage,
        txid,
        arena,
    });
    let empty_columns = crate::sql::exec::CatalogCols {
        remote_ctid: false,
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    // A query without FROM items reads only the enclosing queries' columns.
    let enclosing_catalog = query::storage_catalog(storage, arena, txid);
    let enclosing_columns = outer.map(|row| crate::sql::eval::RuntimeColumnTypes {
        row,
        catalog: Some(&enclosing_catalog),
    });
    let columns: &dyn crate::sql::exec::ColTypeResolver =
        match (scoped_columns.as_ref(), enclosing_columns.as_ref()) {
            (Some(columns), _) => columns,
            (None, Some(columns)) => columns,
            (None, None) => &empty_columns,
        };
    let aggregate = has_aggregate(statement, storage, txid, columns)?;
    // A join PostgreSQL's join search plans is explained as that plan; a
    // query of one relation planned there for its windows takes its scan,
    // windows, sort and limit from it.
    let modeled = match (outer, foreign_scan.as_ref(), scope.as_ref()) {
        (None, None, Some(scope)) => query::index_paths::join_query::modeled_join(
            storage,
            txid,
            statement,
            scope,
            &[],
            arena,
        )?,
        _ => None,
    };
    // Windows over a join the join search does not plan would print
    // pos3ql's own join order and sorts.
    if modeled.is_none() && scope.as_ref().is_some_and(|scope| scope.n > 1) && has_window(statement)
    {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of window functions over a join PostgreSQL's join search does not plan here is not supported"
        ));
    }
    let (modeled, windowed_scan) = match modeled {
        Some(model)
            if !model.windows.is_empty() && scope.as_ref().is_some_and(|scope| scope.n == 1) =>
        {
            (None, Some(model))
        }
        modeled => (modeled, None),
    };
    if let Some(scope) = scope.as_ref()
        && let Some(model) = modeled
    {
        // The plan names a parenthesized join's members.
        let scope = model.scope(scope);
        // The sort's keys: the `ORDER BY` items its pathkeys keep.
        let order = order_keys(statement, arena)?;
        let sorted = arena
            .alloc_slice_with(model.sort_items.len(), |at| {
                order[usize::from(model.sort_items[at])]
            })
            .map_err(|_| arena_full())?;
        // Each key shows its class's first member the target list holds,
        // a relation no node scans (a dummy one's) unqualified.
        let scanned = joins::scanned_rels(&model.plan, model.plan.top);
        for (key, &shown) in sorted.iter_mut().zip(model.sort_shown) {
            let Some((table, column)) = shown else {
                continue;
            };
            if let Some(id) =
                query::index_paths::joins::Member::plain(0, column as u16).expression_id()
            {
                key.0 = model.expressions[id].expression;
                continue;
            }
            let Some(definition) = scope.defs.stored(table) else {
                continue;
            };
            let name = arena
                .alloc_str_display(format_args!(
                    "{}",
                    definition.columns()[column].name.as_str()
                ))
                .map_err(|_| arena_full())?;
            key.0 = arena
                .alloc(Expr::Column {
                    qualifier: Some(scope.names[table]),
                    name,
                })
                .map_err(|_| arena_full())?;
        }
        let sorted: &[KeyItem<'_>] = sorted;
        let unscanned = if scanned == 0 { 0 } else { !u64::from(scanned) };
        let sort_keys = plan_keys_unscanned(
            storage,
            txid,
            Some(scope),
            statement,
            KeyLabel::Sort,
            sorted,
            unscanned,
            arena,
        )?;
        let presorted = match model.plan.upper.sort {
            Some((_, _, presorted)) if presorted > 0 => Some(plan_keys_unscanned(
                storage,
                txid,
                Some(scope),
                statement,
                KeyLabel::Sort,
                &sorted[..usize::from(presorted)],
                unscanned,
                arena,
            )?),
            _ => None,
        };
        // A `GROUP BY`'s `HashAggregate`, or its `GroupAggregate` over the
        // joined rows sorted by its keys.
        let mut group_nodes = [PlanNode::EMPTY; 2];
        let mut group_count = 0;
        if let (Some(group), Some(chosen)) = (model.group, model.plan.upper.group) {
            let items = arena
                .alloc_slice_with(group.keys.len(), |at| {
                    let (position, flags) = group.keys[at];
                    (
                        if group.distinct {
                            match statement.items[position] {
                                SelectItem::Expr { expression, .. } => expression,
                                _ => &Expr::Null,
                            }
                        } else {
                            statement.group_by[position]
                        },
                        Some((
                            flags & query::index_paths::joins::KEY_DESCENDING != 0,
                            flags & query::index_paths::joins::KEY_NULLS_FIRST != 0,
                        )),
                    )
                })
                .map_err(|_| arena_full())?;
            let unsorted = arena
                .alloc_slice_with(items.len(), |at| (items[at].0, None))
                .map_err(|_| arena_full())?;
            // The sort below also orders by the presorted aggregates' keys.
            let grouping = items.len();
            let items = arena
                .alloc_slice_with(grouping + group.presorted.len(), |at| {
                    match at.checked_sub(grouping) {
                        None => items[at],
                        Some(tail) => {
                            let (expression, flags) = group.presorted[tail];
                            (
                                expression,
                                Some((
                                    flags & query::index_paths::joins::KEY_DESCENDING != 0,
                                    flags & query::index_paths::joins::KEY_NULLS_FIRST != 0,
                                )),
                            )
                        }
                    }
                })
                .map_err(|_| arena_full())?;
            let group_keys = plan_keys(
                storage,
                txid,
                Some(scope),
                statement,
                KeyLabel::Group,
                unsorted,
                arena,
            )?;
            let unique = group.distinct && !group.hashed;
            group_nodes[0] = PlanNode {
                name: StackStr::from_str(if group.hashed {
                    "HashAggregate"
                } else if unique {
                    "Unique"
                } else if group.keys.is_empty() {
                    "Aggregate"
                } else if group.aggregated {
                    "GroupAggregate"
                } else {
                    "Group"
                }),
                key: Some(NodeKey::new(
                    block,
                    if group.distinct {
                        Role::Unique
                    } else {
                        Role::Aggregate
                    },
                )),
                startup_cost: chosen.startup,
                total_cost: chosen.total,
                rows: chosen.rows as u64,
                width: chosen.width as u32,
                // A `Unique` names no keys.
                keys: if unique { PlanKeys::NONE } else { group_keys },
                ..PlanNode::EMPTY
            }
            .with_qual(
                QualLabel::Filter,
                having_filter(storage, txid, Some(scope), statement.having, false, arena)?,
            )
            .with_verbose_qual(
                QualLabel::Filter,
                having_filter(storage, txid, Some(scope), statement.having, true, arena)?,
            );
            group_count = 1;
            if let Some((startup, total, presorted)) = chosen.input_sort {
                let mut node = PlanNode {
                    name: StackStr::from_str(if presorted == 0 {
                        "Sort"
                    } else {
                        "Incremental Sort"
                    }),
                    key: Some(NodeKey::new(block, Role::GroupSort)),
                    startup_cost: startup,
                    total_cost: total,
                    rows: chosen.input.rows as u64,
                    width: model.output_width as u32,
                    keys: plan_keys(
                        storage,
                        txid,
                        Some(scope),
                        statement,
                        KeyLabel::Sort,
                        items,
                        arena,
                    )?,
                    ..PlanNode::EMPTY
                };
                if presorted > 0 {
                    let prefix = plan_keys(
                        storage,
                        txid,
                        Some(scope),
                        statement,
                        KeyLabel::Sort,
                        &items[..usize::from(presorted)],
                        arena,
                    )?;
                    node = node
                        .with_qual(QualLabel::PresortedKey, presorted_text(&prefix, false))
                        .with_verbose_qual(QualLabel::PresortedKey, presorted_text(&prefix, true));
                }
                // Its tuples carry the presorted aggregates' columns, which
                // the grouping's sort space model does not size.
                if !group.presorted.is_empty() {
                    node.analyze_unsupported =
                        Some("EXPLAIN ANALYZE of a sort for presorted aggregates is not supported");
                }
                group_nodes[1] = node;
                group_count = 2;
            }
        }
        // The window clauses' nodes, top first, above any grouping.
        let mut upper_nodes = [PlanNode::EMPTY; 2 + 2 * query::index_paths::joins::MAX_WINDOWS];
        let mut upper_count = 0;
        let mut window_names: &[&str] = &[];
        if !model.windows.is_empty() {
            let (nodes, _, names) = window_stages(
                storage,
                txid,
                Some(scope),
                statement,
                columns,
                &[],
                Some(ModeledWindows {
                    kept: model.windows,
                    steps: &model.plan.upper.windows[..model.windows.len()],
                    rows: model
                        .plan
                        .upper
                        .group
                        .map_or(model.plan.path(model.plan.top).rows, |group| group.rows),
                }),
                block,
                arena,
            )?;
            window_names = names;
            for node in nodes.iter().rev() {
                upper_nodes[upper_count] = *node;
                // Execution computes windows over the joined rows only.
                if model.plan.upper.group.is_some() {
                    upper_nodes[upper_count].analyze_unsupported = Some(
                        "EXPLAIN ANALYZE of window functions over a join's groups is not supported",
                    );
                }
                // A limit directly above the windows stops them early; their
                // execution computes every row first.
                if model.plan.upper.limit.is_some() && model.plan.upper.sort.is_none() {
                    upper_nodes[upper_count].analyze_unsupported =
                        Some("EXPLAIN ANALYZE of window functions under a limit is not supported");
                }
                upper_count += 1;
            }
        }
        for node in &group_nodes[..group_count] {
            upper_nodes[upper_count] = *node;
            upper_count += 1;
        }
        // `VERBOSE`'s target lists of a join without row locks: the join
        // computes the scan/join target, the grouping and window nodes
        // theirs, the `Sort` and `Limit` above them return their input's.
        let verbose_targets = if statement.locking.is_empty() {
            let mut stages = [targets::Stage {
                name: "Join",
                passthrough: false,
            }; 3 + 2 + 2 * query::index_paths::joins::MAX_WINDOWS];
            let mut count = 1;
            for node in upper_nodes[..upper_count].iter().rev() {
                stages[count].name = node.name.as_str();
                count += 1;
            }
            for (present, name) in [
                (model.plan.upper.sort.is_some(), "Sort"),
                (model.plan.upper.limit.is_some(), "Limit"),
            ] {
                if present {
                    stages[count].name = name;
                    count += 1;
                }
            }
            targets::BlockTargets::new(
                storage,
                txid,
                statement,
                Some(scope),
                true,
                &stages[..count],
                &[],
                window_names,
                aggregate,
                true,
                &[],
                arena,
            )?
            .filter(|found| (0..scope.n).all(|table| !found.reads_whole_row(table)))
        } else {
            None
        };
        let mut plan = joins::join_plan(
            storage,
            txid,
            scope,
            &model,
            (sort_keys, presorted, &upper_nodes[..upper_count]),
            verbose_targets.as_ref(),
            block,
            arena,
        )?;
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
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
            outer,
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
                .selected_columns(0)
                .is_some_and(|demanded| index.covers(demanded))
        }
        _ => false,
    };

    // A condition the planner folds to false leaves no relation to scan:
    // PostgreSQL runs the query's upper nodes over a `Result` that returns
    // no rows.
    let mut constant_false = statement
        .where_clause
        .is_some_and(|predicate| qual_false(predicate, arena));
    // A relation none of whose members is scanned is PostgreSQL's dummy
    // relation: it leaves an inner or semi join no rows, and the nullable
    // side of an outer join or an anti join's inner side reads a `Result`
    // returning none, its columns named by no relation.
    let nameless = arena
        .alloc_slice_with(expansions.len(), |_| false)
        .map_err(|_| arena_full())?;
    if let Some(from) = statement.from.as_ref() {
        use crate::sql::ast::JoinKind;
        for (table, expansion) in expansions.iter().enumerate() {
            let Some(expansion) = expansion else {
                continue;
            };
            if !expansion.planned.leaves.is_empty() {
                continue;
            }
            let side = |kinds: &[JoinKind]| {
                from.joins.iter().enumerate().any(|(index, join)| {
                    kinds.contains(&join.kind)
                        && match join.kind {
                            JoinKind::Right => table <= index,
                            JoinKind::Full => table <= index + 1,
                            _ => index + 1 == table,
                        }
                })
            };
            if side(&[JoinKind::Full, JoinKind::Anti]) {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN of a full or anti join with a partitioned table none of whose partitions is scanned is not supported"
                ));
            }
            let unnamed = NAMELESS_DUMMIES.with(|dummies| {
                let (unnamed, _) = dummies.get();
                dummies.set((unnamed, true));
                unnamed
            });
            if side(&[JoinKind::Left, JoinKind::Right]) {
                // Execution hashes it only when it is partitioned: an
                // excluded inheritance tree's parent rows are read.
                if !scope.as_ref().is_some_and(|scope| {
                    storage
                        .table_def(scope.slots[table], txid)
                        .partition
                        .is_partitioned()
                }) {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of an outer join with an inheritance tree none of whose members is scanned is not supported"
                    ));
                }
                nameless[table] = unnamed;
            } else {
                nameless[table] = unnamed;
                constant_false = true;
            }
        }
    }

    // The path PostgreSQL's planner chooses for a query reading one table
    // without grouping, for its ordering and row limit.
    let mut chosen_tree = None;
    let chosen_path = match (scope.as_ref(), statement.from.as_ref()) {
        _ if windowed_scan.is_some() => Some(windowed_scan_path(
            storage,
            txid,
            windowed_scan.as_ref().expect("checked"),
            arena,
        )?),
        (Some(scope), Some(from)) if single_relation_planned(storage, txid, statement, scope) => {
            let only = from.base.inheritance == crate::sql::ast::RelationInheritance::Only;
            let shape = if aggregate {
                query::index_paths::QueryShape {
                    only,
                    ..query::index_paths::QueryShape::default()
                }
            } else {
                query::index_paths::query_shape(
                    storage,
                    txid,
                    scope,
                    statement.items,
                    statement.order_by,
                    statement.limit,
                    statement.offset,
                    &[],
                    arena,
                )?
            };
            // The scan returns the columns the select list and ordering read.
            let output = query::streaming_pax_columns_ordered(
                scope,
                from,
                statement.items,
                None,
                statement.order_by,
            )
            .selected_columns(0);
            let shape = query::index_paths::QueryShape {
                only,
                output,
                ..shape
            };
            // Pseudoconstant conditions gate the scan instead
            // (`create_gating_plan`) and cost it nothing.
            let planned = query::index_paths::scan_path(
                storage,
                scope,
                txid,
                split_gating(storage, txid, statement.where_clause, arena)?.1,
                query::streaming_pax_columns_ordered(
                    scope,
                    from,
                    statement.items,
                    statement.where_clause,
                    statement.order_by,
                )
                .selected_columns(0),
                &[],
                arena,
                shape,
            )?;
            match planned {
                query::index_paths::Planned::Outside(shape) => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of a scan of one table with {} is not supported",
                        shape
                    ));
                }
                query::index_paths::Planned::Tree(tree) => {
                    chosen_tree = Some(tree);
                    Some(tree.top())
                }
                planned => planned.chosen(),
            }
        }
        _ => None,
    };
    // The grouping PostgreSQL's planner chooses over the scan of one table.
    let grouped = match (scope.as_ref(), statement.from.as_ref()) {
        (Some(scope), Some(from)) if chosen_path.is_none() => {
            grouped_scan_path(storage, txid, statement, scope, from, columns, width, arena)?
        }
        _ => None,
    };
    let grouped_path = grouped.map(|grouped| grouped.path);
    let grouping_sets = match (scope.as_ref(), statement.from.as_ref()) {
        (Some(scope), Some(from)) if grouped.is_none() => {
            grouping_sets_scan_path(storage, txid, statement, scope, from, columns, width, arena)?
        }
        _ => None,
    };
    let chosen_scan = chosen_path
        .or(grouped_path)
        .or(grouping_sets.map(|(path, _, _)| path));
    let mut estimated_rows = 1u64;
    let mut total_cost = 0.01f64;
    let mut object_requests = 0u64;
    let mut cache_blocks = 0u64;
    let mut hash_join = false;
    let mut hash_join_name = "Hash Join";
    // Per FROM position of an outer hash join, the conditions execution
    // moves into its scan.
    let mut hash_pushed: [&[&Expr<'_>]; 2] = [&[], &[]];
    let table_count = scope.as_ref().map_or(0, |scope| scope.n);
    let table_order = arena
        .alloc_slice_with(table_count, |index| index)
        .map_err(|_| arena_full())?;
    let parameterized_indexes = arena
        .alloc_slice_with(table_count, |_| None)
        .map_err(|_| arena_full())?;
    // The index each relation's scan uses, if any.
    let scan_indexes = arena
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
            let (scan, index) = scan_node(
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
                chosen_scan,
            )?;
            scan_indexes[table] = index;
            estimated_rows = estimated_rows.saturating_mul(scan.rows.max(1));
            total_cost += scan.total_cost;
            object_requests = object_requests.saturating_add(scan.object_requests);
            cache_blocks = cache_blocks.saturating_add(scan.cache_blocks);
        }
        if let Some(from) = statement.from.as_ref()
            && let Some(plan) = query::select_hash_join_plan(
                storage,
                scope,
                from,
                statement.where_clause,
                &table_order[..scope.n],
                None,
                true,
                txid,
                arena,
            )?
        {
            if let Some(filters) = plan.outer_side_filters(scope, statement.where_clause, arena)? {
                let layout = plan.key_layout();
                hash_pushed[layout.probe_table] = filters.probe;
                hash_pushed[layout.build_table] = filters.build;
            }
            // The hash join probes its outer relation.
            let layout = plan.key_layout();
            table_order[0] = layout.probe_table;
            table_order[1] = layout.build_table;
            hash_join = true;
            hash_join_name = plan.node_name();
        }
    }

    let mut stages = [PlanNode::EMPTY; MAX_EXPLAIN_STAGES];
    let mut stage_count = 0usize;
    // Whether pseudoconstant conditions gate the one relation's scan.
    let mut gated = false;
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

    // A query without FROM reads one row from a `Result`, which a WHERE
    // condition filters once.
    let fromless = scope.is_none();
    // Execution aggregates a FROM-less query whose list expands sets in a
    // derived query, and expands the sets over its row.
    let aggregated_block = if fromless
        && aggregate
        && query::has_set_returning_items(statement.items, storage, txid)
    {
        block.child(Segment::Derived(0))
    } else {
        block
    };
    if fromless && !constant_false {
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Result"),
                key: Some(NodeKey::new(aggregated_block, Role::Result)),
                total_cost,
                rows: estimated_rows,
                width,
                ..PlanNode::EMPTY
            }
            .with_qual(
                QualLabel::OneTimeFilter,
                one_time_filter(storage, txid, statement.where_clause, arena)?,
            ),
        )?;
    }
    // Whether each of `keys` is a constant PostgreSQL drops from a FROM-less
    // query's keys; ordinals name output columns.
    let constant_keys = |keys: &[&Expr<'_>]| -> Result<bool, SqlError> {
        for key in keys {
            let key = match key {
                Expr::Int(position) if *position >= 1 => statement
                    .items
                    .get(*position as usize - 1)
                    .and_then(|item| match item {
                        SelectItem::Expr { expression, .. } => Some(*expression),
                        _ => None,
                    })
                    .unwrap_or(key),
                _ => key,
            };
            if !query::fromless_key_is_constant(key, storage, txid, arena)? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    if scope.as_ref().is_some_and(|scope| scope.n > 1) && !constant_false {
        total_cost += estimated_rows as f64 * 0.01;
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str(if hash_join {
                    hash_join_name
                } else {
                    join_node(
                        statement
                            .from
                            .as_ref()
                            .and_then(|from| from.joins.last())
                            .map(|join| join.kind),
                        false,
                    )
                }),
                key: key(Role::Join(
                    u16::try_from(scope.as_ref().map_or(1, |scope| scope.n) - 1)
                        .map_err(|_| arena_full())?,
                )),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                // PostgreSQL's join costs (`final_cost_hashjoin`,
                // `final_cost_nestloop`) and join size estimates are not
                // modeled for a query's joins.
                unmodeled_costs: true,
                ..PlanNode::EMPTY
            },
        )?;
    }

    // A FROM-less query grouped by constants groups its one row.
    let constant_group = fromless && constant_keys(statement.group_by)?;
    if aggregate {
        // PostgreSQL's `cost_agg` over the chosen scan of one relation.
        let exact = match chosen_path {
            Some(path) if statement.group_by.is_empty() && statement.having.is_none() => {
                plain_aggregate_cost(storage, txid, statement, columns, &path, arena)?
            }
            _ => None,
        };
        estimated_rows = if statement.group_by.is_empty() {
            1
        } else {
            (estimated_rows as f64).sqrt().ceil().max(1.0) as u64
        };
        let mut aggregate_startup = 0.0;
        // The aggregate's costs are PostgreSQL's `cost_agg` of a modeled
        // path, else pos3ql's own.
        let mut aggregate_modeled = exact.is_some();
        match exact {
            Some((startup, total)) => {
                aggregate_startup = startup;
                total_cost = total;
            }
            None => total_cost += estimated_rows as f64 * 0.02,
        }
        let mut aggregate_name = if statement.group_by.is_empty() {
            "Aggregate"
        } else if constant_group {
            "Group"
        } else {
            "HashAggregate"
        };
        let mut aggregate_keys = group_keys(statement, arena)?;
        let mut planned_partitions = None;
        let mut hash_agg_sizing = None;
        let mut sets_keys = None;
        // Grouping sets outside the cost model still list PostgreSQL's
        // rollups and keys, arranged from pos3ql's own estimates.
        if grouping_sets.is_none() && !statement.grouping_sets.is_empty() {
            use query::index_paths::{GroupingSet, MAX_SET_COLUMNS};
            let n_keys = statement.group_by.len();
            if n_keys <= MAX_SET_COLUMNS {
                let input_rows = (estimated_rows as f64).max(1.0);
                let sets = arena
                    .alloc_slice_with(statement.grouping_sets.len(), |at| {
                        let mut set = GroupingSet {
                            columns: [0; MAX_SET_COLUMNS],
                            len: 0,
                            groups: 1.0,
                        };
                        for position in
                            (0..n_keys).filter(|&key| statement.grouping_sets[at].contains(key))
                        {
                            set.columns[set.len] = position as u8;
                            set.len += 1;
                        }
                        if set.len > 0 {
                            set.groups = input_rows.sqrt().ceil();
                        }
                        set
                    })
                    .map_err(|_| arena_full())?;
                let order = arena
                    .alloc_slice_with(statement.order_by.len(), |at| {
                        statement
                            .group_by
                            .iter()
                            .position(|key| *key == statement.order_by[at].expression)
                            .map(|position| position as u8)
                    })
                    .map_err(|_| arena_full())?;
                let sets = query::index_paths::plan_grouping_sets(
                    &query::index_paths::SetsQuery {
                        sets,
                        distinct: statement.grouping_set_quantifier
                            == crate::sql::ast::GroupingSetQuantifier::Distinct,
                        order,
                        aggregates: query::index_paths::AggregateCosts::default(),
                        input: query::index_paths::AggInput {
                            startup: 0.0,
                            total: total_cost,
                            rows: input_rows,
                            width: f64::from(width),
                        },
                        output_width: f64::from(width),
                    },
                    arena,
                )?;
                aggregate_name = sets.strategy.node_name();
                sets_keys = Some(grouping_sets_keys(
                    storage,
                    txid,
                    scope.as_ref(),
                    statement,
                    &sets,
                    arena,
                )?);
            }
        }
        if let Some((_, sets, sets_width)) = grouping_sets {
            if let Some((startup, total)) = sets.input_sort {
                // The input is sorted for the first rollup not hashed.
                let first = sets
                    .rollups
                    .iter()
                    .find(|rollup| !rollup.hashed)
                    .map_or(sets.rollups[0].clause, |rollup| rollup.clause);
                let sort_keys = arena
                    .alloc_slice_with(first.len, |at| {
                        let entry = statement.group_by[usize::from(first.columns[at])];
                        let direction = statement
                            .order_by
                            .iter()
                            .find(|key| *key.expression == *entry)
                            .map_or((false, false), |key| (key.descending, key.nulls_first));
                        (entry, Some(direction))
                    })
                    .map_err(|_| arena_full())?;
                add_stage(
                    &mut stages,
                    &mut stage_count,
                    PlanNode {
                        name: StackStr::from_str("Sort"),
                        keys: plan_keys(
                            storage,
                            txid,
                            scope.as_ref(),
                            statement,
                            KeyLabel::Sort,
                            sort_keys,
                            arena,
                        )?,
                        startup_cost: startup,
                        total_cost: total,
                        rows: sets.input.rows as u64,
                        width: sets.input.width as u32,
                        object_requests,
                        cache_blocks,
                        key: Some(NodeKey::new(aggregated_block, Role::GroupSort)),
                        ..PlanNode::EMPTY
                    },
                )?;
            }
            aggregate_name = sets.strategy.node_name();
            aggregate_modeled = true;
            aggregate_startup = sets.startup;
            total_cost = sets.total;
            estimated_rows = sets.rows as u64;
            width = sets_width;
            sets_keys = Some(grouping_sets_keys(
                storage,
                txid,
                scope.as_ref(),
                statement,
                &sets,
                arena,
            )?);
        }
        if let Some(grouped) = grouped
            && let Some(path) = grouped.path.grouped
        {
            if let Some((startup, total, presorted)) = path.input_sort {
                let mut node = PlanNode {
                    name: StackStr::from_str(if presorted == 0 {
                        "Sort"
                    } else {
                        "Incremental Sort"
                    }),
                    keys: plan_keys(
                        storage,
                        txid,
                        scope.as_ref(),
                        statement,
                        KeyLabel::Sort,
                        grouped.sort_keys,
                        arena,
                    )?,
                    startup_cost: startup,
                    total_cost: total,
                    rows: grouped.path.rows as u64,
                    width: grouped.path.width,
                    object_requests,
                    cache_blocks,
                    key: Some(NodeKey::new(aggregated_block, Role::GroupSort)),
                    ..PlanNode::EMPTY
                };
                if presorted > 0 {
                    let prefix = plan_keys(
                        storage,
                        txid,
                        scope.as_ref(),
                        statement,
                        KeyLabel::Sort,
                        &grouped.sort_keys[..usize::from(presorted)],
                        arena,
                    )?;
                    node = node
                        .with_qual(QualLabel::PresortedKey, presorted_text(&prefix, false))
                        .with_verbose_qual(QualLabel::PresortedKey, presorted_text(&prefix, true));
                }
                add_stage(&mut stages, &mut stage_count, node)?;
            }
            if !path.hashed {
                aggregate_name = if path.aggregated {
                    "GroupAggregate"
                } else {
                    "Group"
                };
            }
            planned_partitions = path.planned_partitions;
            hash_agg_sizing = Some(path.hash_agg);
            aggregate_keys = Some(grouped.group_keys);
            aggregate_modeled = true;
            aggregate_startup = path.startup;
            total_cost = path.total;
            estimated_rows = path.rows as u64;
            width = grouped.width;
        }
        // A hash table's memory, sized from pos3ql's own estimates outside
        // the cost model.
        let hash_agg = if aggregate_name == "HashAggregate" && sets_keys.is_none() {
            match hash_agg_sizing {
                Some(sizing) => sizing,
                None => {
                    let expressions = arena
                        .alloc_slice_with(statement.items.len() + 1, |at| {
                            match statement.items.get(at) {
                                Some(SelectItem::Expr { expression, .. }) => *expression,
                                _ => statement.having.unwrap_or(&Expr::Null),
                            }
                        })
                        .map_err(|_| arena_full())?;
                    query::index_paths::aggregate_costs(expressions, storage, txid, columns, arena)?
                        .filter(|aggregates| !aggregates.ordered)
                        .and_then(|aggregates| {
                            query::index_paths::hash_agg_sizing(
                                &aggregates,
                                estimated_rows as f64,
                                f64::from(width),
                                statement.group_by.len(),
                            )
                        })
                }
            }
        } else {
            None
        };
        let hash_agg_unsupported = (aggregate_name == "HashAggregate"
            && sets_keys.is_none()
            && hash_agg.is_none())
        .then_some(
            "EXPLAIN ANALYZE of a hash aggregate whose transition states or grouping columns PostgreSQL's memory figure is not modelled for is not supported",
        );
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str(aggregate_name),
                key: Some(NodeKey::new(aggregated_block, Role::Aggregate)),
                planned_partitions,
                hash_agg,
                analyze_unsupported: hash_agg_unsupported,
                keys: match aggregate_keys
                    .filter(|_| !statement.group_by.is_empty() && !constant_group)
                {
                    _ if sets_keys.is_some() => sets_keys.unwrap_or(PlanKeys::NONE),
                    Some(keys) => plan_keys(
                        storage,
                        txid,
                        scope.as_ref(),
                        statement,
                        KeyLabel::Group,
                        keys,
                        arena,
                    )?,
                    None => PlanKeys::NONE,
                },
                startup_cost: aggregate_startup,
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                unmodeled_costs: !aggregate_modeled,
                ..PlanNode::EMPTY
            }
            .with_qual(
                QualLabel::Filter,
                having_filter(
                    storage,
                    txid,
                    scope.as_ref(),
                    statement.having,
                    false,
                    arena,
                )?,
            )
            .with_verbose_qual(
                QualLabel::Filter,
                having_filter(storage, txid, scope.as_ref(), statement.having, true, arena)?,
            ),
        )?;
    }
    // The keys the rows leave the last window sorted by; a foreign scan's
    // rows arrive in the order its remote query applies.
    let mut window_sorted: &[crate::sql::ast::OrderBy<'_>] =
        foreign_scan.as_ref().map_or(&[], |pushed| pushed.presorted);
    // Each window clause's name, as its `WindowAgg` prints it.
    let mut window_names: &[&str] = &[];
    if has_window(statement) {
        total_cost += estimated_rows as f64 * 0.02;
        let modeled = windowed_scan.as_ref().map(|model| ModeledWindows {
            kept: model.windows,
            steps: &model.plan.upper.windows[..model.windows.len()],
            rows: model.plan.path(model.plan.top).rows,
        });
        let (nodes, sorted, names) = window_stages(
            storage,
            txid,
            scope.as_ref(),
            statement,
            columns,
            window_sorted,
            modeled,
            block,
            arena,
        )?;
        window_sorted = sorted;
        window_names = names;
        // Execution reads the windows' input by a sequential scan, to its
        // end, which a limit right above the windows would stop.
        let analyze_unsupported = chosen_path
            .filter(|_| windowed_scan.is_some())
            .is_some_and(|path| {
                path.kind != query::index_paths::ScanKind::Seq
                    || (limit_needed(statement.limit, statement.offset)
                        && path.sort == query::index_paths::SortKind::None)
            })
            .then_some(
                "EXPLAIN ANALYZE of window functions over an index scan or under a LIMIT is not supported",
            );
        for node in nodes {
            let node = &PlanNode {
                analyze_unsupported: analyze_unsupported.or(node.analyze_unsupported),
                ..*node
            };
            add_stage(
                &mut stages,
                &mut stage_count,
                if node.unmodeled_costs {
                    PlanNode {
                        total_cost,
                        rows: estimated_rows,
                        width,
                        object_requests,
                        cache_blocks,
                        ..*node
                    }
                } else {
                    PlanNode {
                        object_requests,
                        cache_blocks,
                        ..*node
                    }
                },
            )?;
        }
    }
    if query::has_set_returning_items(statement.items, storage, txid) {
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("ProjectSet"),
                key: key(Role::ProjectSet),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    // PostgreSQL sorts a FROM-less query only by its keys that are not
    // constants.
    let sorted_keys = if fromless {
        let keys = order_keys(statement, arena)?;
        let kept = arena
            .alloc_slice_with(keys.len(), |_| None)
            .map_err(|_| arena_full())?;
        let mut count = 0;
        for key in keys {
            if !constant_keys(&[key.0])? {
                kept[count] = Some(*key);
                count += 1;
            }
        }
        &*arena
            .alloc_slice_with(count, |index| kept[index].expect("kept key"))
            .map_err(|_| arena_full())?
    } else if scope.as_ref().is_some_and(|scope| scope.n == 1)
        && !statement.with_ties
        && !aggregate
        && !statement.distinct
        && statement.distinct_on.is_empty()
    {
        // A key a `WHERE` equality fixes to a constant orders nothing: its
        // pathkey is redundant in the constant's equivalence class.
        let keys = order_keys(statement, arena)?;
        let kept = arena
            .alloc_slice_with(keys.len(), |_| None)
            .map_err(|_| arena_full())?;
        let mut count = 0;
        for key in keys {
            // A key of a column an earlier key orders is redundant too.
            let repeated = kept[..count].iter().flatten().any(|earlier: &KeyItem<'_>| {
                *earlier.0 == *key.0
                    || match (earlier.0, key.0, scope.as_ref()) {
                        (
                            Expr::Column {
                                qualifier: one,
                                name: first,
                            },
                            Expr::Column {
                                qualifier: two,
                                name: second,
                            },
                            Some(scope),
                        ) => matches!(
                            (scope.find_column(*one, first), scope.find_column(*two, second)),
                            (Ok(left), Ok(right)) if left == right
                        ),
                        _ => false,
                    }
            });
            if !repeated && !constant_fixed_key(statement, key.0, storage, txid, arena)? {
                kept[count] = Some(*key);
                count += 1;
            }
        }
        &*arena
            .alloc_slice_with(count, |index| kept[index].expect("kept key"))
            .map_err(|_| arena_full())?
    } else {
        order_keys(statement, arena)?
    };
    // Rows a window leaves sorted by the query's keys need no sort, and
    // ones sorted by a prefix of them only an incremental one.
    let presorted = match chosen_path.map(|path| path.sort) {
        Some(query::index_paths::SortKind::Incremental(presorted)) => Some(presorted),
        _ => presorted_order_keys(statement, sorted_keys, window_sorted),
    };
    // The chosen path may return the rows ordered; otherwise the legacy
    // ordered index does.
    let scan_ordered = match (chosen_path, grouped_path.and_then(|path| path.grouped)) {
        _ if grouping_sets.is_some() => {
            grouping_sets.is_some_and(|(_, sets, _)| sets.output_sort.is_none())
        }
        (Some(path), _) => path.sort == query::index_paths::SortKind::None,
        (None, Some(grouped)) => grouped.output_sort.is_none(),
        (None, None) => ordered_index.is_some(),
    };
    if !sorted_keys.is_empty()
        && !scan_ordered
        && presorted.is_none_or(|presorted| presorted < sorted_keys.len())
    {
        total_cost += estimated_rows as f64 * (estimated_rows.max(2) as f64).log2() * 0.0025;
        let presorted = presorted.unwrap_or(0);
        let mut node = PlanNode {
            name: StackStr::from_str(if presorted == 0 {
                "Sort"
            } else {
                "Incremental Sort"
            }),
            key: key(Role::Sort),
            keys: plan_keys(
                storage,
                txid,
                scope.as_ref(),
                statement,
                KeyLabel::Sort,
                sorted_keys,
                arena,
            )?,
            total_cost,
            rows: estimated_rows,
            width,
            object_requests,
            cache_blocks,
            ..PlanNode::EMPTY
        };
        // The chosen path's sort costs as PostgreSQL's planner does.
        if let Some(path) = chosen_path
            && let Some((startup, total)) = path.sort_cost
        {
            node.startup_cost = startup;
            node.total_cost = total;
            node.rows = path.rows as u64;
            node.width = windowed_scan
                .as_ref()
                .map_or(path.width, |model| model.final_width as u32);
        }
        if let Some(grouped) = grouped_path.and_then(|path| path.grouped)
            && let Some((startup, total)) = grouped.output_sort
        {
            node.startup_cost = startup;
            node.total_cost = total;
        }
        if let Some((_, sets, _)) = grouping_sets
            && let Some((startup, total)) = sets.output_sort
        {
            node.startup_cost = startup;
            node.total_cost = total;
            node.rows = sets.rows as u64;
        }
        if presorted != 0 {
            let prefix = plan_keys(
                storage,
                txid,
                scope.as_ref(),
                statement,
                KeyLabel::Sort,
                &sorted_keys[..presorted],
                arena,
            )?;
            node = node
                .with_qual(QualLabel::PresortedKey, presorted_text(&prefix, false))
                .with_verbose_qual(QualLabel::PresortedKey, presorted_text(&prefix, true));
        }
        add_stage(&mut stages, &mut stage_count, node)?;
        if postpones_select_list(storage, txid, statement) {
            add_stage(
                &mut stages,
                &mut stage_count,
                PlanNode {
                    name: StackStr::from_str("Result"),
                    key: key(Role::SortedProjection),
                    total_cost,
                    rows: estimated_rows,
                    width,
                    object_requests,
                    cache_blocks,
                    passthrough: true,
                    ..PlanNode::EMPTY
                },
            )?;
        }
    }
    let distinct_items = || -> Result<bool, SqlError> {
        if !statement.distinct_on.is_empty() {
            return constant_keys(statement.distinct_on);
        }
        for item in statement.items {
            let SelectItem::Expr { expression, .. } = item else {
                return Ok(false);
            };
            if !constant_keys(&[expression])? {
                return Ok(false);
            }
        }
        Ok(true)
    };
    if fromless && (statement.distinct || !statement.distinct_on.is_empty()) && distinct_items()? {
        // PostgreSQL keeps the one row of constants with a `Limit`.
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Limit"),
                key: key(Role::Unique),
                total_cost,
                rows: 1,
                width,
                ..PlanNode::EMPTY
            },
        )?;
    } else if statement.distinct || !statement.distinct_on.is_empty() {
        estimated_rows = estimated_rows.div_ceil(2);
        total_cost += estimated_rows as f64 * 0.01;
        add_stage(
            &mut stages,
            &mut stage_count,
            // Without an ordering PostgreSQL hashes the distinct rows.
            PlanNode {
                name: StackStr::from_str(
                    if statement.order_by.is_empty()
                        && statement.distinct_on.is_empty()
                        && window_sorted.is_empty()
                    {
                        "HashAggregate"
                    } else {
                        "Unique"
                    },
                ),
                key: key(Role::Unique),
                keys: match group_keys(statement, arena)?.filter(|_| {
                    statement.order_by.is_empty()
                        && statement.distinct_on.is_empty()
                        && statement.group_by.is_empty()
                        && window_sorted.is_empty()
                }) {
                    Some(keys) => plan_keys(
                        storage,
                        txid,
                        scope.as_ref(),
                        statement,
                        KeyLabel::Group,
                        keys,
                        arena,
                    )?,
                    None => PlanKeys::NONE,
                },
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            }
            .with_distinct_hash(estimated_rows, width),
        )?;
    }
    // A locking clause locks each row below the limit, as PostgreSQL's
    // `LockRows` does; a query without FROM has no rows to lock. A query
    // postgres_fdw runs whole as one remote scan sends its locking clause
    // with it: its final path replaces `LockRows` unless a local `LIMIT`
    // remains above the scan.
    let remote_locking =
        foreign_scan.is_some() && statement.limit.is_none() && statement.offset.is_none();
    if !statement.locking.is_empty() && !fromless && !remote_locking {
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("LockRows"),
                key: key(Role::LockRows),
                total_cost: total_cost + estimated_rows as f64 * 0.01,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }
    // PostgreSQL's Limit node also applies an OFFSET alone.
    if limit_needed(statement.limit, statement.offset) {
        estimated_rows = estimated_rows.min(100);
        add_stage(
            &mut stages,
            &mut stage_count,
            match grouped
                .and_then(|grouped| {
                    grouped
                        .path
                        .grouped
                        .and_then(|path| path.limit)
                        .map(|cost| (grouped.width, cost))
                })
                .or_else(|| {
                    chosen_path.and_then(|path| {
                        path.limit_cost.map(|cost| {
                            (
                                windowed_scan
                                    .as_ref()
                                    .map_or(path.width, |model| model.final_width as u32),
                                cost,
                            )
                        })
                    })
                }) {
                // The chosen path's limit as PostgreSQL's planner costs it.
                Some((width, (startup, total, rows))) => PlanNode {
                    name: StackStr::from_str("Limit"),
                    key: key(Role::Limit),
                    startup_cost: startup,
                    total_cost: total,
                    rows: rows as u64,
                    width,
                    object_requests,
                    cache_blocks,
                    ..PlanNode::EMPTY
                },
                None => PlanNode {
                    name: StackStr::from_str("Limit"),
                    key: key(Role::Limit),
                    total_cost,
                    rows: estimated_rows,
                    width,
                    object_requests,
                    cache_blocks,
                    ..PlanNode::EMPTY
                },
            },
        )?;
    }

    let n = scope.as_ref().map_or(0, |scope| scope.n);
    // The WHERE conjuncts PostgreSQL attaches to each scan and to each join,
    // when every conjunct can be placed and printed.
    // A single relation's index condition and filter as `VERBOSE` prints
    // them.
    let mut verbose_quals: (Option<StackStr<512>>, Option<StackStr<512>>) = (None, None);
    let mut bitmap_recheck = None;
    // A chosen bitmap tree's nodes and each bitmap index scan's conditions.
    let mut bitmap_tree: Option<(&[query::index_paths::BitmapPlanNode<'_>], &[LeafQuals])> = None;
    // Each tree member's conditions.
    let mut member_quals: &[MemberQuals<'_>] = &[];
    let index_conds = arena
        .alloc_slice_with(n.max(1), |_| None::<StackStr<512>>)
        .map_err(|_| arena_full())?;
    let filters = arena
        .alloc_slice_with(n.max(1), |_| None::<StackStr<512>>)
        .map_err(|_| arena_full())?;
    // A joined relation's filter as `VERBOSE` prints it.
    let verbose_filters = arena
        .alloc_slice_with(n.max(1), |_| None::<StackStr<512>>)
        .map_err(|_| arena_full())?;
    let join_filters = arena
        .alloc_slice_with(n.max(1), |_| None::<StackStr<512>>)
        .map_err(|_| arena_full())?;
    // Per join, the filter of its output.
    let post_filters = arena
        .alloc_slice_with(n.max(1), |_| None::<StackStr<512>>)
        .map_err(|_| arena_full())?;
    // Relations joined by commas, CROSS JOIN or inner joins' ON conditions,
    // which PostgreSQL merges with the WHERE conjuncts.
    let inner_joins = scope.as_ref().is_some_and(|scope| {
        scope.n >= 2
            && statement.from.as_ref().is_some_and(|from| {
                from.joins.iter().all(|join| {
                    join.kind.is_cross() || join.kind == crate::sql::ast::JoinKind::Inner
                }) && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
            })
    });
    let mut join_condition = statement.where_clause;
    if inner_joins && let (Some(from), Some(scope)) = (statement.from.as_ref(), scope.as_ref()) {
        // A `USING` or `NATURAL` join's condition is the one analysis
        // synthesized.
        for (index, join) in from.joins.iter().enumerate() {
            join_condition =
                query::and_where(join_condition, join.on.or(scope.join_on[index]), arena)?;
        }
    }
    // Whether each relation's scan is a parameterized index scan.
    let used_parameterized = arena
        .alloc_slice_with(table_count, |table| {
            parameterized_indexes[table].is_some() && scan_indexes[table].is_some()
        })
        .map_err(|_| arena_full())?;
    if let Some(scope) = &scope {
        let relation = |table: usize| {
            scope
                .defs
                .stored(table)
                .map(|def| (scope.names[table], def))
        };
        // The relations a join reads: stored tables and subqueries in FROM.
        let join_relations = arena
            .alloc_slice_with(scope.n, |_| None::<JoinRelation<'_>>)
            .map_err(|_| arena_full())?;
        if scope.n >= 2
            && let Some(from) = statement.from.as_ref()
        {
            let mut used = crate::mem::arena::ArenaList::new(arena);
            for name in &scope.names[..scope.n] {
                used.push(StackStr::<96>::from_str(
                    crate::sql::catalog::deparse::printed_name(name),
                ))
                .map_err(|_| arena_full())?;
            }
            let items =
                core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
            for (table, item) in items.enumerate().take(scope.n) {
                join_relations[table] = match (relation(table), item.subquery) {
                    (Some((exposed, def)), _) => Some(JoinRelation {
                        nameless: nameless[table],
                        ..JoinRelation::table(exposed, def)
                    }),
                    (None, Some(body)) if values_list(body).is_none() => {
                        match scope.defs.get(table) {
                            Some(definition) => {
                                let outputs = subquery_outputs(
                                    storage, txid, body, definition, &mut used, arena,
                                )?;
                                let named = arena
                                    .alloc_slice_with(outputs.len(), |index| {
                                        crate::sql::catalog::deparse::Column {
                                            merged: None,
                                            ..outputs[index]
                                        }
                                    })
                                    .map_err(|_| arena_full())?;
                                Some(JoinRelation {
                                    exposed: scope.names[table],
                                    definition,
                                    table: None,
                                    columns: crate::sql::catalog::deparse::Columns::Derived(named),
                                    subquery: Some(outputs),
                                    nameless: false,
                                })
                            }
                            None => None,
                        }
                    }
                    // A `WITH` query's rows, read by a `CTE Scan`, and a
                    // function's, read by a `Function Scan`.
                    (None, None)
                        if item.cte.is_some()
                            || ((item.func_args.is_some() || item.rows_from.is_some())
                                && !scope.lateral[table]) =>
                    {
                        match scope.defs.get(table) {
                            Some(definition) => Some(JoinRelation {
                                exposed: scope.names[table],
                                definition,
                                table: None,
                                columns: crate::sql::catalog::deparse::Columns::Derived(
                                    arena
                                        .alloc_slice_with(definition.n_columns, |index| {
                                            crate::sql::catalog::deparse::Column {
                                                name: definition.columns[index].name.as_str(),
                                                type_oid: None,
                                                merged: None,
                                            }
                                        })
                                        .map_err(|_| arena_full())?,
                                ),
                                subquery: None,
                                nameless: false,
                            }),
                            None => None,
                        }
                    }
                    _ => None,
                };
            }
        }
        if scope.n == 1
            && let Some((exposed, def)) = relation(0)
        {
            // Conjuncts that read only subplans run once gate the scan
            // from a `Result` above it, as PostgreSQL's pseudoconstant
            // quals do.
            let (gating, scanned) = split_gating(storage, txid, statement.where_clause, arena)?;
            // A condition folded to false or NULL leaves only its `Result`.
            if let Some(gating) = gating.filter(|_| !constant_false) {
                let filter = one_time_filter(storage, txid, Some(gating), arena)?;
                if stage_count == stages.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "EXPLAIN operator stack exceeds {} nodes",
                        stages.len()
                    ));
                }
                stages.copy_within(0..stage_count, 1);
                stages[0] = PlanNode {
                    name: StackStr::from_str("Result"),
                    rows: estimated_rows,
                    width,
                    total_cost,
                    passthrough: true,
                    ..PlanNode::EMPTY
                }
                .with_qual(QualLabel::OneTimeFilter, filter);
                stage_count += 1;
                gated = true;
            }
            let security = select_security_quals(storage, scope, txid, arena)?;
            (index_conds[0], filters[0]) = single_relation_quals(
                storage,
                txid,
                exposed,
                def,
                &[],
                scanned,
                security,
                scan_indexes[0].as_ref(),
                false,
                arena,
            )?;
            verbose_quals = single_relation_quals(
                storage,
                txid,
                exposed,
                def,
                &[],
                scanned,
                security,
                scan_indexes[0].as_ref(),
                true,
                arena,
            )?;
            // A chosen bitmap heap scan rechecks its conditions as written;
            // its bitmap index scan prints them with the indexed side first.
            if chosen_scan.is_some_and(|path| path.kind == query::index_paths::ScanKind::Bitmap)
                && let Some(index) = scan_indexes[0]
            {
                let written = index.written();
                let plain = single_relation_quals(
                    storage,
                    txid,
                    exposed,
                    def,
                    &[],
                    scanned,
                    security,
                    Some(&written),
                    false,
                    arena,
                )?
                .0;
                let verbose = single_relation_quals(
                    storage,
                    txid,
                    exposed,
                    def,
                    &[],
                    scanned,
                    security,
                    Some(&written),
                    true,
                    arena,
                )?
                .0;
                bitmap_recheck = Some((plain, verbose));
                // A tree's bitmap index scans print their own conditions.
                if let Some(path) = chosen_scan.filter(|path| path.bitmap.len() > 1) {
                    let quals = arena
                        .alloc_slice_with(path.bitmap.len(), |_| (None, None))
                        .map_err(|_| arena_full())?;
                    for (at, node) in path.bitmap.iter().enumerate() {
                        let query::index_paths::BitmapPlanKind::Index { conditions, .. } =
                            node.kind
                        else {
                            continue;
                        };
                        let leaf =
                            query::IndexAccessPlan::chosen_conditions(query::ChosenConditions {
                                conditions,
                                recheck: &[],
                                absorbed: &[],
                            });
                        for (verbose, held) in [(false, 0), (true, 1)] {
                            let text = single_relation_quals(
                                storage,
                                txid,
                                exposed,
                                def,
                                &[],
                                scanned,
                                security,
                                Some(&leaf),
                                verbose,
                                arena,
                            )?
                            .0;
                            if held == 0 {
                                quals[at].0 = text;
                            } else {
                                quals[at].1 = text;
                            }
                        }
                    }
                    bitmap_tree = Some((path.bitmap, &*quals));
                }
            }
            // Each member of a tree prints its own scan's conditions.
            if let Some(tree) = chosen_tree {
                member_quals =
                    tree_member_quals(storage, txid, exposed, scanned, security, &tree, arena)?;
            }
        } else if scope.n == 1
            && let Some(definition) = scope.defs.get(0)
            && let Some(from) = statement.from.as_ref()
        {
            filters[0] = derived_relation_filter(
                storage,
                txid,
                scope.names[0],
                definition,
                match from.base.subquery {
                    // PostgreSQL scans a pulled-up `VALUES` list as
                    // `*VALUES*`, its columns named `column1`, ...
                    Some(body) if values_list(body).is_some() => DerivedNames::Values,
                    Some(_) => DerivedNames::Qualified,
                    None => DerivedNames::Bare,
                },
                // Execution pushes a condition into a `VALUES` list's rows;
                // PostgreSQL filters its scan by it.
                if from.base.subquery.and_then(values_list).is_some()
                    && written
                        .from
                        .as_ref()
                        .is_some_and(|written| written.joins.is_empty())
                {
                    written.where_clause
                } else {
                    statement.where_clause
                },
                false,
                arena,
            )?;
            verbose_quals.1 = derived_relation_filter(
                storage,
                txid,
                scope.names[0],
                definition,
                match from.base.subquery {
                    // PostgreSQL scans a pulled-up `VALUES` list as
                    // `*VALUES*`, its columns named `column1`, ...
                    Some(body) if values_list(body).is_some() => DerivedNames::Values,
                    Some(_) => DerivedNames::Qualified,
                    None => DerivedNames::Bare,
                },
                // Execution pushes a condition into a `VALUES` list's rows;
                // PostgreSQL filters its scan by it.
                if from.base.subquery.and_then(values_list).is_some()
                    && written
                        .from
                        .as_ref()
                        .is_some_and(|written| written.joins.is_empty())
                {
                    written.where_clause
                } else {
                    statement.where_clause
                },
                true,
                arena,
            )?;
        } else if inner_joins && join_relations.iter().all(Option::is_some) {
            let relations = arena
                .alloc_slice_with(scope.n, |table| {
                    join_relations[table].expect("join relation")
                })
                .map_err(|_| arena_full())?;
            if let Some(quals) = join_quals(
                storage,
                txid,
                relations,
                &table_order[..scope.n],
                join_condition,
                hash_join,
                scan_indexes,
                used_parameterized,
                arena,
            )? {
                filters.copy_from_slice(quals.filters);
                verbose_filters.copy_from_slice(quals.verbose_filters);
                join_filters.copy_from_slice(quals.join_filters);
                stages[0] = stages[0].with_qual(QualLabel::HashCond, quals.hash_cond);
            }
            joined_index_conditions(storage, txid, relations, scan_indexes, index_conds, arena)?;
        } else if scope.n >= 2
            && let Some(from) = statement.from.as_ref()
            && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
            && table_order[..scope.n]
                .iter()
                .enumerate()
                .all(|(position, &table)| position == table)
            && join_relations.iter().all(Option::is_some)
        {
            let relations = arena
                .alloc_slice_with(scope.n, |table| {
                    join_relations[table].expect("join relation")
                })
                .map_err(|_| arena_full())?;
            if let Some(quals) = outer_join_quals(
                storage,
                txid,
                relations,
                from.joins,
                scope.join_on,
                statement.where_clause,
                hash_join,
                scan_indexes,
                used_parameterized,
                arena,
            )? {
                filters.copy_from_slice(quals.filters);
                verbose_filters.copy_from_slice(quals.verbose_filters);
                join_filters.copy_from_slice(quals.join_filters);
                post_filters.copy_from_slice(quals.post_filters);
                stages[0] = stages[0].with_qual(QualLabel::HashCond, quals.hash_cond);
            }
            joined_index_conditions(storage, txid, relations, scan_indexes, index_conds, arena)?;
        } else if hash_join
            && scope.n == 2
            && let Some(join) = statement.from.as_ref().and_then(|from| from.joins.first())
            && matches!(
                join.kind,
                crate::sql::ast::JoinKind::Left
                    | crate::sql::ast::JoinKind::Right
                    | crate::sql::ast::JoinKind::Full
                    | crate::sql::ast::JoinKind::Semi
                    | crate::sql::ast::JoinKind::Anti
            )
            && let Some(condition) = join.on.or(scope.join_on[0])
            && let (Some(first), Some(second)) = (join_relations[0], join_relations[1])
            && let Some(quals) = outer_hash_quals(
                storage,
                txid,
                &[first, second],
                table_order[0],
                condition,
                statement.where_clause,
                hash_pushed,
                arena,
            )?
        {
            stages[0] = stages[0].with_qual(QualLabel::HashCond, Some(quals.hash_cond));
            join_filters[1] = quals.join_filter;
            post_filters[1] = quals.output_filter;
            filters[..2].copy_from_slice(&quals.scan_filters);
            verbose_filters[..2].copy_from_slice(&quals.verbose_scan_filters);
        }
    }
    // `VERBOSE`'s target list of each node. An entry outside the
    // deparser's coverage leaves every node its output columns' names.
    let shapes = arena
        .alloc_slice_with(stage_count, |index| targets::Stage {
            name: stages[index].name.as_str(),
            passthrough: stages[index].passthrough,
        })
        .map_err(|_| arena_full())?;
    // The subqueries of a condition the remote query took still join the
    // range table.
    let qualify_outputs = n > 1
        || shares_range_table(statement)
        || shares_range_table(written)
        || nested_block()
        || pulled_relations(written);
    let block_targets = targets::BlockTargets::new(
        storage,
        txid,
        statement,
        scope.as_ref(),
        qualify_outputs,
        shapes,
        &table_order[..n],
        window_names,
        aggregate,
        n > 1 || fromless,
        nameless,
        arena,
    )?;
    let stage_output = |index: usize| -> targets::Output {
        block_targets
            .as_ref()
            .map_or_else(|| widened(output), |found| found.stage(index))
    };
    // What the parent of a one-relation block's scan asks of its target
    // list (`create_plan_recurse`'s flags).
    // A gating `Result` projects the scan's rows, which return their
    // physical columns (`create_scan_plan` with pseudoconstant quals).
    let single_demand = if gated {
        targets::Demand::Any
    } else {
        shapes
            .iter()
            .find(|stage| !stage.passthrough && !matches!(stage.name, "Limit" | "LockRows"))
            .map_or(targets::Demand::Exact, |stage| match stage.name {
                "Aggregate" | "HashAggregate" | "GroupAggregate" | "MixedAggregate" | "Group"
                | "ProjectSet" => targets::Demand::Label,
                _ => targets::Demand::Small,
            })
    };
    if constant_false {
        // The dummy relation's columns are zero wide (`set_dummy_rel_pathlist`):
        // its `Result` is as wide as the select list's other expressions.
        let dummy_width = match (scope.as_ref(), aggregate) {
            (Some(scope), false) if scope.n == 1 => query::index_paths::query_shape(
                storage,
                txid,
                scope,
                statement.items,
                statement.order_by,
                None,
                None,
                &[],
                arena,
            )?
            .projection
            .map_or(width, |projection| projection.expression_width),
            _ => width,
        };
        for (depth, stage) in stages[..stage_count].iter().rev().enumerate() {
            plan.push(PlanNode {
                depth: depth as u8,
                output: stage_output(stage_count - 1 - depth),
                ..*stage
            })?;
        }
        // An aggregate without grouping still aggregates the empty input;
        // only a query without FROM counts its `Result`.
        plan.push(PlanNode {
            name: StackStr::from_str("Result"),
            key: if aggregate && statement.group_by.is_empty() && scope.is_none() {
                key(Role::Result)
            } else {
                None
            },
            depth: stage_count as u8,
            output: match block_targets.as_ref() {
                Some(found) => found.scan_join()?,
                None => widened(output),
            },
            rows: 0,
            width: dummy_width,
            total_cost: 0.0,
            constant_false: true,
            ..PlanNode::EMPTY
        })?;
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    for (depth, stage) in stages[..stage_count].iter().rev().enumerate() {
        let mut stage = *stage;
        stage.depth = depth as u8;
        stage.output = stage_output(stage_count - 1 - depth);
        if n >= 2 && depth + 1 == stage_count {
            stage = stage
                .with_qual(QualLabel::JoinFilter, join_filters[n - 1].take())
                .with_qual(QualLabel::Filter, post_filters[n - 1].take());
        }
        plan.push(stage)?;
    }
    if let Some(scope) = &scope {
        // A left-deep join tree: the join adding the relation at position
        // `k` has the join adding position `k - 1` as its outer input and
        // that relation's scan as its inner input. The top join is the
        // last stage pushed above.
        let top = stage_count.saturating_sub(1);
        for position in (1..n.saturating_sub(1)).rev() {
            let join = join_node(
                statement
                    .from
                    .as_ref()
                    .map(|from| from.joins[position - 1].kind),
                false,
            );
            plan.push(
                PlanNode {
                    name: StackStr::from_str(join),
                    key: key(Role::Join(
                        u16::try_from(position).map_err(|_| arena_full())?,
                    )),
                    depth: (top + n - 1 - position) as u8,
                    output: match block_targets.as_ref() {
                        Some(found) => found.join(position)?,
                        None => widened(output),
                    },
                    ..PlanNode::EMPTY
                }
                .with_qual(QualLabel::JoinFilter, join_filters[position].take())
                .with_qual(QualLabel::Filter, post_filters[position].take()),
            )?;
        }
        // The first relation's estimated rows, the outer side of the first
        // nested loop.
        let mut outer_rows = u64::MAX;
        // The range table names taken, which members' names avoid.
        let mut names = RangeTableNames {
            names: arena
                .alloc_slice_with(scope.n + members, |index| {
                    scope.names.get(index).map_or_else(StackStr::new, |name| {
                        StackStr::from_str(crate::sql::catalog::deparse::printed_name(name))
                    })
                })
                .map_err(|_| arena_full())?,
            count: scope.n,
        };
        for (position, &table) in table_order[..scope.n].iter().enumerate() {
            // postgres_fdw plans a foreign table read with other relations
            // (a join, or a semi-join from `IN` or `EXISTS`) with its own
            // estimates and join pushdown, which this plan does not model:
            // neither the scan nor the join method above it would match.
            if foreign_scan.is_none()
                && scope.derived[table].is_none()
                && scope.slots[table] != usize::MAX
                && expansions.get(table).copied().flatten().is_none()
            {
                let definition = storage.table_def(scope.slots[table], txid);
                if definition.kind == crate::storage::TableKind::Foreign {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of foreign table \"{}\" read with other relations or under column aliases is not supported",
                        definition.name.as_str()
                    ));
                }
            }
            let depth = if n == 1 {
                stage_count
            } else {
                top + n - position.max(1)
            };
            let (mut scan, _) = scan_node(
                storage,
                scope,
                table,
                statement.where_clause,
                txid,
                depth as u8,
                arena,
                ordered_index,
                parameterized_indexes[table],
                ordered_index_only,
                chosen_scan,
            )?;
            // A join's outer relation, and an inner one it rescans directly,
            // may return their physical columns; one under a `Hash` or
            // `Materialize` returns only the columns needed.
            let wrapped = position >= 1
                && (hash_join
                    || ((scan.name.as_str() == "Seq Scan"
                        || statement.from.as_ref().is_some_and(|from| {
                            core::iter::once(&from.base)
                                .chain(from.joins.iter().map(|join| &join.table))
                                .nth(table)
                                .and_then(|item| item.subquery)
                                .is_some_and(|body| values_list(body).is_none())
                        }))
                        && !nameless[table]
                        && nested_loop_materializes(
                            if n == 2 { outer_rows } else { u64::MAX },
                            crate::sql::guc::planner_methods().material,
                        )));
            // A partitioned table's members compute the query's scan/join
            // target themselves (`apply_scanjoin_target_to_paths`); in a
            // join they read the columns needed above.
            let expanded = expansions.get(table).copied().flatten().is_some();
            // An inheritance tree's members return the columns needed, and
            // a `Result` above their `Append` computes the scan/join target
            // when it differs (`apply_scanjoin_target_to_paths` projects a
            // relation that is not partitioned as a whole).
            let projection = if expanded
                && n == 1
                && !storage
                    .table_def(scope.slots[table], txid)
                    .partition
                    .is_partitioned()
            {
                let Some(found) = block_targets.as_ref() else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of a select list over an inheritance tree outside the target list deparser's coverage is not supported"
                    ));
                };
                // Each member returns its whole row as the parent's row
                // type, the same value when the member adds no column.
                if found.reads_whole_row(table) {
                    let columns = storage.table_def(scope.slots[table], txid).n_columns;
                    let leaves = expansions[table].map_or(&[][..], |expansion| expansion.started);
                    if leaves
                        .iter()
                        .any(|&leaf| storage.table_def(leaf, txid).n_columns != columns)
                    {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "EXPLAIN of a whole-row reference to an inheritance child with its own columns is not supported"
                        ));
                    }
                    None
                } else {
                    let target = found.scan_join()?;
                    let needed = found.scan(table, false, targets::Demand::Small)?;
                    (target.as_str() != needed.as_str()).then_some((target, needed))
                }
            } else {
                None
            };
            match block_targets.as_ref() {
                Some(found) => {
                    scan.output = found.scan(
                        table,
                        n == 1,
                        if n == 1 && expanded {
                            targets::Demand::Exact
                        } else if n == 1 {
                            single_demand
                        } else if wrapped || expanded {
                            targets::Demand::Small
                        } else {
                            targets::Demand::Any
                        },
                    )?;
                }
                None if !aggregate => scan.output = widened(output),
                None => {}
            }
            if let Some((_, needed)) = projection {
                scan.output = needed;
            }
            // PostgreSQL estimates a relation never analyzed as at least ten
            // pages of rows (`estimate_rel_size`), where pos3ql counts its
            // resident rows: only collected statistics give the outer
            // side's estimate here, and without them the copy stays.
            if position == 0
                && scope.derived[table].is_none()
                && scope.slots[table] != usize::MAX
                && storage.has_planning_statistics(scope.slots[table])
            {
                outer_rows = scan.rows;
            }
            // PostgreSQL scans a function in FROM with a `Function Scan`.
            if let Some(source) = statement.from.as_ref().and_then(|from| {
                core::iter::once(&from.base)
                    .chain(from.joins.iter().map(|join| &join.table))
                    .nth(table)
            }) && source.is_function_source()
                && source.rows_from.is_none()
            {
                scan.name = StackStr::from_str("Function Scan");
                scan.relation = StackStr::from_str(source.table);
                scan.alias = if scope.names[table] == source.table {
                    StackStr::new()
                } else {
                    StackStr::from_str(scope.names[table])
                };
                scan.schema = function_schema(storage, txid, source);
                let estimate = function_scan_estimate(
                    storage,
                    txid,
                    scope,
                    table,
                    source,
                    statement.where_clause.filter(|_| n == 1),
                    arena,
                )?;
                scan.startup_cost = estimate.startup;
                scan.total_cost = estimate.total;
                scan.rows = estimate.rows;
                scan.width = estimate.width;
                scan = scan.with_verbose_qual(
                    QualLabel::FunctionCall,
                    function_call(storage, txid, scope, statement, source, arena)?,
                );
            }
            // A `WITH` query run once is read with a `CTE Scan` of its rows.
            if let Some(source) = statement.from.as_ref().and_then(|from| {
                core::iter::once(&from.base)
                    .chain(from.joins.iter().map(|join| &join.table))
                    .nth(table)
            }) && source.cte.is_some()
            {
                scan.name = StackStr::from_str("CTE Scan");
                scan.relation = StackStr::from_str(source.table);
                scan.alias = if scope.names[table] == source.table {
                    StackStr::new()
                } else {
                    StackStr::from_str(scope.names[table])
                };
            }
            let position_role = u16::try_from(table).map_err(|_| arena_full())?;
            scan.key = key(Role::Scan(position_role));
            // A deeper level's outer side is a join, whose row estimate
            // this plan does not model: it keeps the materialized copy.
            let subquery = statement.from.as_ref().is_some_and(|from| {
                core::iter::once(&from.base)
                    .chain(from.joins.iter().map(|join| &join.table))
                    .nth(table)
                    .and_then(|item| item.subquery)
                    .is_some_and(|body| values_list(body).is_none())
            }) && !scope.lateral[table];
            // A `LATERAL` subquery runs again for each outer row with its
            // parameters (`create_subqueryscan_path` with `required_outer`),
            // so no copy of its rows is kept.
            // A dummy relation's `Result` costs nothing to rescan, so no
            // materialized copy is cheaper (`cost_material`).
            let sequential = (scan.name.as_str() == "Seq Scan" || subquery)
                && !nameless[table]
                && nested_loop_materializes(
                    if n == 2 { outer_rows } else { u64::MAX },
                    crate::sql::guc::planner_methods().material,
                );
            scan = scan
                .with_qual(QualLabel::IndexCond, index_conds[table].take())
                .with_qual(QualLabel::Filter, filters[table].take());
            if n == 1
                && let Some((plain, verbose)) = bitmap_recheck.take()
            {
                scan = scan
                    .with_qual(QualLabel::RecheckCond, plain)
                    .with_verbose_qual(QualLabel::RecheckCond, verbose);
            }
            if n == 1 {
                scan = scan
                    .with_verbose_qual(QualLabel::IndexCond, verbose_quals.0.take())
                    .with_verbose_qual(QualLabel::Filter, verbose_quals.1.take());
            } else {
                scan = scan.with_verbose_qual(QualLabel::Filter, verbose_filters[table].take());
            }
            if position >= 1 {
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
                        key: key(if hash_join {
                            Role::Hash(position_role)
                        } else {
                            Role::Materialize(position_role)
                        }),
                        hash_columns: block_targets
                            .as_ref()
                            .and_then(|found| found.hashed_columns(table)),
                        depth: scan.depth,
                        total_cost: scan.total_cost,
                        rows: scan.rows,
                        width: scan.width,
                        object_requests: scan.object_requests,
                        cache_blocks: scan.cache_blocks,
                        output: scan.output,
                        output_unprintable: scan.output_unprintable,
                        ..PlanNode::EMPTY
                    })?;
                    scan.depth += 1;
                }
            }
            if let Some(pushed) = &foreign_scan {
                let above = foreign_scan_node(
                    &mut scan,
                    pushed,
                    statement,
                    scope,
                    storage,
                    txid,
                    arena,
                    aggregate,
                    gated,
                    qualify_outputs,
                )?;
                // A local limit, or a gating `Result`, over the remote order
                // passes the scan's sort columns up, reading what the scan
                // computes.
                if pushed.order_pushed
                    && let Some(above) = above
                {
                    for node in plan.nodes[..plan.count].iter_mut() {
                        if node.name.as_str() == "Limit" || gated && node.name.as_str() == "Result"
                        {
                            node.output = above;
                            node.output_unprintable = scan.output_unprintable;
                        }
                    }
                }
            }
            match expansions.get(table).copied().flatten() {
                Some(expansion) => push_members(
                    &mut plan,
                    storage,
                    txid,
                    scan,
                    expansion,
                    false,
                    &mut names,
                    &|leaf| {
                        Some(NodeKey::new(
                            block,
                            Role::Member(position_role, u32::try_from(leaf).unwrap_or(u32::MAX)),
                        ))
                    },
                    projection.map(|(target, _)| (target, false)),
                    match chosen_tree.filter(|_| n == 1) {
                        Some(tree) => Some(ChosenTree {
                            path: tree,
                            quals: member_quals,
                            keys: plan_keys(
                                storage,
                                txid,
                                Some(scope),
                                statement,
                                KeyLabel::Sort,
                                &sorted_keys[..tree.order.len().min(sorted_keys.len())],
                                arena,
                            )?,
                        }),
                        None => None,
                    }
                    .as_ref(),
                )?,
                None => {
                    let depth = scan.depth;
                    push_scan(&mut plan, scan)?;
                    if n == 1
                        && let Some((nodes, quals)) = bitmap_tree.take()
                    {
                        push_bitmap_tree(&mut plan, nodes, quals, depth, block)?;
                    }
                }
            }
        }
    }
    // PostgreSQL pulls a derived `VALUES` list into the query and scans it
    // as `*VALUES*`.
    if let (Some(from), Some(scope)) = (statement.from.as_ref(), scope.as_ref()) {
        let items = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
        for (table, item) in items.enumerate().take(scope.n) {
            let Some(tree) = item.subquery.and_then(values_list) else {
                continue;
            };
            for at in 0..plan.count {
                let node = plan.nodes[at];
                if node.name.as_str() == "Subquery Scan"
                    && node.relation.as_str().is_empty()
                    && node.alias.as_str() == scope.names[table]
                {
                    let values = values_scan(&plan, tree, node.depth);
                    plan.nodes[at] = PlanNode {
                        name: values.name,
                        alias: values.alias,
                        rows: values.rows,
                        ..node
                    };
                }
            }
        }
        let mut plan = plan;
        // Each subquery's relations follow the outer query's in the range
        // table, so a name the query already uses takes a suffix.
        let mut used = crate::mem::arena::ArenaList::new(arena);
        for name in &scope.names[..scope.n] {
            used.push(StackStr::<96>::from_str(
                crate::sql::catalog::deparse::printed_name(name),
            ))
            .map_err(|_| arena_full())?;
        }
        let items = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
        for (table, item) in items.enumerate().take(scope.n) {
            let Some(body) = item.subquery.filter(|body| values_list(body).is_none()) else {
                continue;
            };
            // Locked rows carry their row identities out of the subquery,
            // so PostgreSQL keeps its scan.
            // A join reads its input's physical columns (`use_physical_tlist`),
            // which a subquery's own plan outputs in order; a query of the
            // one relation projects its own select list.
            // A plain aggregate over it reads the physical columns too:
            // `use_physical_tlist` refuses only a grouping key's label.
            let plain_aggregate = aggregate
                && statement.having.is_none()
                && statement.group_by.is_empty()
                && statement.grouping_sets.is_empty()
                && statement.windows.is_empty();
            let passes_through = body.locking.is_empty()
                && !has_resjunk_targets(body)
                && (!from.joins.is_empty()
                    || plain_aggregate
                    || statement
                        .items
                        .iter()
                        .all(|item| matches!(item, SelectItem::Wildcard(_))));
            let derived_block = block.child(Segment::Derived(
                u16::try_from(table).map_err(|_| arena_full())?,
            ));
            // A derived table reads the FROM items to its left (`LATERAL`)
            // and the enclosing queries' columns.
            let outer_types = outer.map(query::OuterColumnTypes);
            let enclosing = query::Chained {
                inner: &query::ScopeTypes(scope),
                outer: outer_types
                    .as_ref()
                    .map(|types| types as &dyn ColumnLookup<'_>),
            };
            // Its conditions name the stored relations to its left
            // qualified, as the enclosing query's in a correlated subplan.
            let left_relations = if scope.lateral[table] {
                push_outer_relations(scope, table)
            } else {
                0
            };
            let derived = plan_derived(storage, txid, body, Some(&enclosing), derived_block, arena);
            OUTER_RELATIONS.with(|outer| outer.borrow_mut().1 -= left_relations);
            let derived = derived?;
            for at in 0..derived.count {
                let node = &mut derived.nodes[at];
                if node.relation.as_str().is_empty() {
                    continue;
                }
                let exposed = StackStr::<96>::from_str(node.exposed_name());
                let unique = unique_rtable_name(exposed.as_str(), used.as_slice());
                if unique.as_str() != exposed.as_str() {
                    node.alias = unique;
                    // The derived query's keys and conditions name the
                    // relation as `set_rtable_names` renamed it.
                    for other in derived.nodes[..derived.count].iter_mut() {
                        other.requalify(exposed.as_str(), unique.as_str());
                    }
                }
                used.push(unique).map_err(|_| arena_full())?;
            }
            plan = with_derived_plan(plan, scope.names[table], passes_through, derived, arena)?;
        }
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

/// The schema of the function a `Function Scan` calls: the one written, a
/// visible user routine's, else `pg_catalog`'s built-in.
fn function_schema(
    storage: &Storage,
    txid: u32,
    source: &crate::sql::ast::TableRef<'_>,
) -> StackStr<64> {
    if let Some(schema) = source.schema {
        return StackStr::from_str(schema);
    }
    storage
        .visible_routine_schema(source.table, txid)
        .map_or(StackStr::from_str("pg_catalog"), |schema| {
            StackStr::from_str(schema.as_str())
        })
}

struct FunctionScanEstimate {
    startup: f64,
    total: f64,
    rows: u64,
    width: u32,
}

/// `cost_functionscan` of the function `source`, FROM item `table` of
/// `scope`, under the conditions `predicate`: the function's rows (its
/// `prorows`, or what its support function computes of constant
/// arguments), its call evaluated once at startup, then each row with the
/// conditions; the width is `get_typavgwidth` of its columns.
/// The rows a `generate_series` support function estimates from its
/// constant, non-NULL `start`, `finish` and optional step, `None` where it
/// gives no estimate: `generate_series_int4_support` and `_int8_support`
/// compute `floor((finish - start + step) / step)` in floating point,
/// `_numeric_support` `trunc((finish - start) / step) + 1` exactly (no rows
/// when the step's sign differs from the difference's, a zero difference
/// counting as positive; none for NaN or infinite values) and
/// `_timestamp_support` `floor(diff / step + 1)` over microseconds, a month
/// counting 30 days (none for an infinite bound).
fn series_rows<'v>(
    support: &str,
    values: &[Option<Datum<'v>>],
    arena: &Arena,
) -> Result<Option<f64>, SqlError> {
    match support {
        "generate_series_numeric_support" => {
            fn numeric<'v>(value: &Option<Datum<'v>>) -> Option<crate::sql::numeric::Numeric<'v>> {
                match value {
                    Some(Datum::Numeric(value)) if !value.is_special() => Some(*value),
                    _ => None,
                }
            }
            let (Some(start), Some(finish)) = (numeric(&values[0]), numeric(&values[1])) else {
                return Ok(None);
            };
            let step = match values.get(2) {
                Some(step) => match numeric(step) {
                    Some(step) => Some(step),
                    None => return Ok(None),
                },
                None => None,
            };
            if step.is_some_and(|step| step.is_zero()) {
                return Ok(None);
            }
            let difference = crate::sql::numeric::sub(&finish, &start, arena)?;
            let negative =
                |value: &crate::sql::numeric::Numeric<'_>| !value.is_zero() && value.is_negative();
            if step.is_some_and(|step| negative(&step) != negative(&difference)) {
                return Ok(Some(0.0));
            }
            let steps = match step {
                Some(step) => crate::sql::numeric::trunc_div(&difference, &step, arena)?,
                None => crate::sql::numeric::trunc_div(
                    &difference,
                    &crate::sql::numeric::Numeric::from_i64(1, arena)?,
                    arena,
                )?,
            };
            Ok(Some(steps.to_f64() + 1.0))
        }
        "generate_series_timestamp_support" => {
            let instant = |value: &Option<Datum<'_>>| match value {
                Some(Datum::Timestamp(at) | Datum::Timestamptz(at))
                    if *at != i64::MIN && *at != i64::MAX =>
                {
                    Some(*at)
                }
                _ => None,
            };
            let (Some(start), Some(finish), Some(Some(Datum::Interval(step)))) =
                (instant(&values[0]), instant(&values[1]), values.get(2))
            else {
                return Ok(None);
            };
            let Some(difference) = finish.checked_sub(start) else {
                return Ok(None);
            };
            let step = (f64::from(step.months) * 30.0 + f64::from(step.days)) * 86_400_000_000.0
                + step.micros as f64;
            if step == 0.0 {
                return Ok(None);
            }
            Ok(Some((difference as f64 / step + 1.0).floor()))
        }
        _ => {
            let number = |value: &Option<Datum<'_>>| match value {
                Some(Datum::Int4(value)) => Some(f64::from(*value)),
                Some(Datum::Int8(value)) => Some(*value as f64),
                Some(Datum::Int2(value)) => Some(f64::from(*value)),
                _ => None,
            };
            Ok(
                match (
                    number(&values[0]),
                    number(&values[1]),
                    values.get(2).map_or(Some(1.0), number),
                ) {
                    (Some(start), Some(finish), Some(step)) if step != 0.0 => {
                        Some(((finish - start + step) / step).floor())
                    }
                    _ => None,
                },
            )
        }
    }
}

fn function_scan_estimate<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    table: usize,
    source: &crate::sql::ast::TableRef<'a>,
    predicate: Option<&Expr<'a>>,
    arena: &'a Arena,
) -> Result<FunctionScanEstimate, SqlError> {
    let Some(args) = source.func_args else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a function scan has no function call"
        ));
    };
    let unsupported = |name: &str| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of a function scan of {} with a row-estimating support function is not supported",
            name
        )
    };
    let mark = arena.mark();
    let result = (|| {
        let definition = scope
            .defs
            .get(table)
            .ok_or_else(|| sql_err!(sqlstate::INTERNAL_ERROR, "function scan has no columns"))?;
        let empty = crate::sql::exec::CatalogCols {
            remote_ctid: false,
            definition: None,
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        };
        let mut oids = [crate::sql::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if args.len() > oids.len() {
            return Err(unsupported(source.table));
        }
        for (oid, argument) in oids.iter_mut().zip(args) {
            *oid = crate::sql::exec::infer_routine_argument_oid(argument, &empty)
                .unwrap_or(crate::sql::types::oid::UNKNOWN);
        }
        let name = match source.schema {
            Some(schema) => arena
                .alloc_str_display(format_args!("{schema}.{}", source.table))
                .map_err(|_| arena_full())?,
            None => source.table,
        };
        let constant = |argument: &'a Expr<'a>| -> Option<Datum<'a>> {
            // A support function reads its arguments through
            // `estimate_expression_value`, which also evaluates a stable
            // call (`now()`) that reads no column.
            let mut reads_column = false;
            argument.for_each_column_reference(&mut |_, _| reads_column = true);
            if !argument.is_constant(false)
                && (reads_column
                    || argument.contains_subquery()
                    || query::expression_contains_volatile(argument, storage, txid, false))
            {
                return None;
            }
            let catalog = query::storage_catalog(storage, arena, txid);
            let hooks = crate::sql::eval::EvalHooks {
                catalog: Some(&catalog),
                ..crate::sql::eval::NO_HOOKS
            };
            crate::sql::eval::eval_full(
                argument,
                arena,
                crate::sql::eval::NO_PARAMS,
                &crate::sql::eval::NoColumns,
                &hooks,
            )
            .ok()
        };
        // The function's own cost (`procost`) and rows (`prorows`).
        let (procost, tuples) = match storage.select_routine_call(
            name,
            source.func_argument_names,
            &oids[..args.len()],
            source.func_variadic,
            txid,
            crate::storage::RoutineCallKind::Function,
        ) {
            crate::storage::RoutineCallSelection::Stored(slot) => {
                let routine = storage.routine_for(slot, txid);
                let set = routine.kind.is_set_returning();
                (
                    routine.attributes.cost_bits.map_or(
                        if matches!(
                            routine.language,
                            crate::storage::RoutineLanguage::Native(_)
                                | crate::storage::RoutineLanguage::Internal
                        ) {
                            1.0
                        } else {
                            100.0
                        },
                        f64::from_bits,
                    ),
                    routine
                        .attributes
                        .rows_bits
                        .map_or(if set { 1000.0 } else { 1.0 }, f64::from_bits),
                )
            }
            crate::storage::RoutineCallSelection::Builtin { oid, .. } => {
                let signature = crate::sql::catalog::builtin_callable_by_oid(oid)
                    .ok_or_else(|| unsupported(source.table))?;
                let rows = match signature.catalog.support_name {
                    "" | "-" => signature.catalog.rows,
                    support @ ("generate_series_int4_support"
                    | "generate_series_int8_support"
                    | "generate_series_numeric_support"
                    | "generate_series_timestamp_support") => {
                        let values = arena
                            .alloc_slice_with(args.len(), |_| None)
                            .map_err(|_| arena_full())?;
                        // Each constant as the parameter type it is coerced
                        // to (`10` to `numeric`, `'2024-01-02'` to a
                        // timestamp).
                        for ((value, argument), &oid) in
                            values.iter_mut().zip(args).zip(signature.arguments)
                        {
                            let Some(ctype) = crate::sql::types::ColType::from_oid(oid) else {
                                continue;
                            };
                            let cast = arena
                                .alloc(Expr::Cast {
                                    source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                                    operand: argument,
                                    type_name: ctype.name(),
                                    type_mod: -1,
                                    binding: crate::sql::ast::CastBinding::Builtin,
                                    context: crate::sql::ast::CastContext::Implicit,
                                })
                                .map_err(|_| arena_full())?;
                            *value = constant(cast);
                        }
                        // The bounds and step the support function reads; a
                        // timestamp series also takes a time zone.
                        let read = &values[..values.len().min(3)];
                        if read.iter().any(|value| matches!(value, Some(Datum::Null))) {
                            0.0
                        } else if read.iter().any(Option::is_none) {
                            // An argument `estimate_expression_value` cannot
                            // reduce to a constant (a column, a volatile call
                            // or a subquery) gives the default estimate.
                            signature.catalog.rows
                        } else {
                            series_rows(support, read, arena)?.unwrap_or(signature.catalog.rows)
                        }
                    }
                    "array_unnest_support" if args.len() == 1 => match constant(args[0]) {
                        Some(Datum::Null) => 0.0,
                        Some(Datum::Array { raw, .. }) => crate::sql::array::len(raw) as f64,
                        // `estimate_array_length` of a non-constant array.
                        _ => 10.0,
                    },
                    _ => return Err(unsupported(source.table)),
                };
                (signature.catalog.cost, rows)
            }
            _ => return Err(unsupported(source.table)),
        };
        // `expression_returns_set_rows` clamps the estimate (`clamp_row_est`).
        let tuples = if tuples <= 1.0 {
            1.0
        } else {
            tuples.round_ties_even()
        };
        // The call's argument costs, in `cost_qual_eval`'s half-operator units.
        let arguments: f64 = args
            .iter()
            .map(|argument| query::qual_cost(argument, &empty, storage, txid, arena))
            .sum();
        let startup = procost * 0.0025 + arguments / 2.0 * 0.0025;
        let columns = crate::sql::exec::AliasedDefCols {
            definition,
            alias: Some(scope.names[table]),
            output_aliases: &[],
            remote_ctid: false,
        };
        let (selectivity, qual) = match predicate {
            Some(predicate) => (
                predicate_selectivity(storage, scope, table, predicate, txid, arena),
                query::qual_cost(predicate, &columns, storage, txid, arena) / 2.0 * 0.0025,
            ),
            None => (1.0, 0.0),
        };
        let rows = (tuples * selectivity).round_ties_even().max(1.0);
        let width = definition
            .columns()
            .iter()
            .map(|column| query::index_paths::type_average_width(column.ctype, column.type_mod))
            .sum::<f64>();
        Ok(FunctionScanEstimate {
            startup,
            total: startup + (0.01 + qual) * tuples,
            rows: rows as u64,
            width: width as u32,
        })
    })();
    // SAFETY: the estimate holds no arena data.
    unsafe { arena.rewind_to(mark) };
    result
}

/// A `Function Scan`'s `Function Call`, its call as `VERBOSE` prints it.
fn function_call(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    statement: &Select<'_>,
    source: &crate::sql::ast::TableRef<'_>,
    arena: &Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    let Some(args) = source.func_args else {
        return Ok(None);
    };
    let Some(joined) = key_relations(Some(scope), statement, arena)? else {
        return Ok(None);
    };
    let call = Expr::Call {
        kind: crate::sql::ast::CallKind::Routine,
        name: source.table,
        bound_oid: None,
        bound_strict: None,
        source_position: source.func_source_position,
        args,
        argument_names: source.func_argument_names,
        variadic: source.func_variadic,
        star: false,
        distinct: false,
        order_by: &[],
        over: None,
        filter: None,
    };
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        arena,
        dependencies: None,
    };
    let mark = arena.mark();
    let text = crate::sql::catalog::deparse::deparse_expression(
        &context,
        &call,
        crate::sql::catalog::deparse::Destination::Value,
        arena,
    )
    .map(|text| text.map(StackStr::<512>::from_str));
    // SAFETY: the text was copied out of the arena.
    unsafe { arena.rewind_to(mark) };
    Ok(text?.filter(|text| !text.is_truncated()))
}

/// Whether `VERBOSE` prints `node`'s target list: `show_plan_tlist` skips
/// an empty one and those of `Append`, `MergeAppend` and `Recursive Union`.
fn shows_output(node: &PlanNode) -> bool {
    !node.output.is_empty()
        && !matches!(
            node.name.as_str(),
            "Append" | "Merge Append" | "Recursive Union"
        )
}

/// `text` with each column qualifier `old` (outside string literals and
/// quoted names) replaced by `new`.
fn requalified<const N: usize>(text: &StackStr<N>, old: &str, new: &str) -> StackStr<N> {
    let source = text.as_str();
    let pattern_len = old.len() + 1;
    let mut out = StackStr::<N>::new();
    let bytes = source.as_bytes();
    let mut quoted: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quoted {
            Some(quote) => {
                if byte == quote {
                    quoted = None;
                }
            }
            None if byte == b'\'' || byte == b'"' => quoted = Some(byte),
            None => {
                let starts = index == 0 || {
                    let prior = bytes[index - 1];
                    !(prior.is_ascii_alphanumeric() || prior == b'_' || prior == b'.')
                };
                if starts
                    && source[index..].starts_with(old)
                    && bytes.get(index + old.len()) == Some(&b'.')
                {
                    let _ = out.write_str(new);
                    let _ = out.write_char('.');
                    index += pattern_len;
                    continue;
                }
            }
        }
        let character = source[index..].chars().next().unwrap_or(' ');
        let _ = out.write_char(character);
        index += character.len_utf8();
    }
    if out.is_truncated() { *text } else { out }
}

/// An output column list held as a node's `Output:`.
fn widened(output: StackStr<256>) -> targets::Output {
    targets::Output::from_str(output.as_str())
}

thread_local! {
    /// How many query blocks deep the block being planned is nested in a
    /// derived table or a set operation, whose range table entries join
    /// the statement's.
    static NESTED_BLOCKS: core::cell::Cell<u16> = const { core::cell::Cell::new(0) };
}

/// Whether the block being planned shares its statement's range table with
/// an enclosing block.
fn nested_block() -> bool {
    NESTED_BLOCKS.with(core::cell::Cell::get) != 0
}

/// Runs `plan` as a block nested in the statement's range table.
fn in_nested_block<T>(plan: impl FnOnce() -> T) -> T {
    NESTED_BLOCKS.with(|depth| depth.set(depth.get() + 1));
    let planned = plan();
    NESTED_BLOCKS.with(|depth| depth.set(depth.get() - 1));
    planned
}

/// Whether `written`'s FROM reads a derived table, `WITH` query or view,
/// whose range table entry stays in the statement's range table when the
/// planner pulls its query up.
fn pulled_relations(written: &Select<'_>) -> bool {
    written.from.as_ref().is_some_and(|from| {
        core::iter::once(&from.base)
            .chain(from.joins.iter().map(|join| &join.table))
            .any(|item| item.subquery.is_some() || item.cte.is_some())
    })
}

/// A presorted key list as PostgreSQL prints it: its keys without their
/// directions, which a sort's keys carry.
fn presorted_text(keys: &PlanKeys, verbose: bool) -> Option<StackStr<512>> {
    let (_, keys) = keys.shown(verbose)?;
    let mut text = StackStr::new();
    for (index, key) in keys.enumerate() {
        if index != 0 {
            let _ = write!(text, ", ");
        }
        let key = key
            .split(" NULLS ")
            .next()
            .unwrap_or(key)
            .trim_end_matches(" DESC");
        let _ = write!(text, "{key}");
    }
    (!text.is_truncated()).then_some(text)
}

/// The expression an `ORDER BY` key sorts by: an output position or name
/// stands for its select-list expression.
/// PostgreSQL's `make_sort_input_target`: whether the query evaluates its
/// select list after its sort, from a `Result` above it, because a column
/// that is not a sort key calls a volatile function, or, under a `LIMIT`,
/// is expensive (a user routine or a correlated subplan).
fn postpones_select_list(storage: &Storage, txid: u32, statement: &Select<'_>) -> bool {
    if statement.distinct || !statement.distinct_on.is_empty() {
        return false;
    }
    statement.items.iter().any(|item| {
        let SelectItem::Expr { expression, .. } = item else {
            return false;
        };
        let sorted = statement
            .order_by
            .iter()
            .any(|key| order_key_expression(statement, key.expression) == *expression);
        if sorted {
            return false;
        }
        if query::expression_contains_volatile(expression, storage, txid, false) {
            return true;
        }
        let mut subplan = false;
        visit_sublinks(expression, &mut |node| {
            subplan |= sublink_query(node)
                .and_then(|select| registered_sublink(core::ptr::from_ref(select).cast()))
                .is_some_and(|plan| {
                    matches!(
                        plan,
                        crate::sql::catalog::deparse::SublinkPlan::SubPlan(_)
                            | crate::sql::catalog::deparse::SublinkPlan::HashedSubPlan(_)
                    )
                });
        });
        statement.limit.is_some()
            && (subplan || query::expression_calls_user_routine(expression, storage, txid))
    })
}

fn order_key_expression<'e>(statement: &Select<'e>, key: &'e Expr<'e>) -> &'e Expr<'e> {
    let item = match key {
        Expr::Int(position) => usize::try_from(*position)
            .ok()
            .and_then(|position| statement.items.get(position.checked_sub(1)?)),
        Expr::Column {
            qualifier: None,
            name,
        } => statement.items.iter().find(
            |item| matches!(item, SelectItem::Expr { alias: Some(alias), .. } if alias == name),
        ),
        _ => None,
    };
    match item {
        Some(SelectItem::Expr { expression, .. }) => expression,
        _ => key,
    }
}

/// How many of the query's sort keys rows sorted by `sorted` already
/// follow, when that is some: all of them needs no sort.
fn presorted_order_keys(
    statement: &Select<'_>,
    keys: &[KeyItem<'_>],
    sorted: &[crate::sql::ast::OrderBy<'_>],
) -> Option<usize> {
    let matched = keys
        .iter()
        .zip(sorted)
        .take_while(|((key, order), held)| {
            let (descending, nulls_first) = order.unwrap_or((false, false));
            *order_key_expression(statement, key) == *held.expression
                && held.descending == descending
                && held.nulls_first == nulls_first
        })
        .count();
    (matched != 0).then_some(matched)
}

/// A window definition as PostgreSQL's EXPLAIN prints it: `name AS
/// (PARTITION BY ... ORDER BY ... frame)`, keys without their directions.
#[expect(clippy::too_many_arguments, reason = "plan key context")]
fn window_definition(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    statement: &Select<'_>,
    name: &str,
    spec: &crate::sql::ast::WindowSpec<'_>,
    optimized_frame: bool,
    verbose: bool,
    arena: &Arena,
) -> Result<Option<StackStr<512>>, SqlError> {
    use crate::sql::ast::{FrameBound, FrameExclusion, FrameUnits};
    let mut text = StackStr::<512>::new();
    crate::sql::catalog::write_identifier(&mut text, name);
    let _ = write!(text, " AS (");
    let mut spaced = false;
    let mut write_keys =
        |text: &mut StackStr<512>, label: &str, items: &[KeyItem<'_>]| -> Result<bool, SqlError> {
            if items.is_empty() {
                return Ok(true);
            }
            let keys = plan_keys(
                storage,
                txid,
                scope,
                statement,
                KeyLabel::Sort,
                items,
                arena,
            )?;
            let Some((_, keys)) = keys.shown(verbose) else {
                return Ok(false);
            };
            if spaced {
                let _ = write!(text, " ");
            }
            let _ = write!(text, "{label} ");
            for (index, key) in keys.enumerate() {
                if index != 0 {
                    let _ = write!(text, ", ");
                }
                let _ = write!(text, "{key}");
            }
            spaced = true;
            Ok(true)
        };
    let partition = arena
        .alloc_slice_with(spec.partition_by.len(), |index| {
            (spec.partition_by[index], None)
        })
        .map_err(|_| arena_full())?;
    let order = arena
        .alloc_slice_with(spec.order_by.len(), |index| {
            (spec.order_by[index].expression, None)
        })
        .map_err(|_| arena_full())?;
    if !write_keys(&mut text, "PARTITION BY", partition)?
        || !write_keys(&mut text, "ORDER BY", order)?
    {
        return Ok(None);
    }
    let frame = if optimized_frame {
        Some(crate::sql::ast::WindowFrame {
            units: FrameUnits::Rows,
            between: false,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::CurrentRow,
            exclusion: FrameExclusion::NoOthers,
        })
    } else {
        spec.frame
    };
    if let Some(frame) = frame {
        if spaced {
            let _ = write!(text, " ");
        }
        let _ = write!(
            text,
            "{}",
            match frame.units {
                FrameUnits::Rows => "ROWS",
                FrameUnits::Range => "RANGE",
                FrameUnits::Groups => "GROUPS",
            }
        );
        if frame.between {
            let _ = write!(text, " BETWEEN");
        }
        let context = crate::sql::catalog::deparse::Context {
            storage,
            txid,
            table: None,
            parameters: &[],
            joined: &[],
            pretty: false,
            qualify: false,
            bare: None,
            domain_value: None,
            sublinks: Some(&registered_sublink),
            references: None,
            folded: true,
            arena,
            dependencies: None,
        };
        let bound = |text: &mut StackStr<512>, bound: FrameBound<'_>| -> Result<bool, SqlError> {
            let (offset, direction) = match bound {
                FrameBound::UnboundedPreceding => {
                    let _ = write!(text, " UNBOUNDED PRECEDING");
                    return Ok(true);
                }
                FrameBound::UnboundedFollowing => {
                    let _ = write!(text, " UNBOUNDED FOLLOWING");
                    return Ok(true);
                }
                FrameBound::CurrentRow => {
                    let _ = write!(text, " CURRENT ROW");
                    return Ok(true);
                }
                FrameBound::Preceding(offset) => (offset, "PRECEDING"),
                FrameBound::Following(offset) => (offset, "FOLLOWING"),
            };
            // The planner coerces a `ROWS` or `GROUPS` offset to bigint.
            if frame.units != FrameUnits::Range
                && let Expr::Int(value) = offset
            {
                let _ = write!(text, " '{value}'::bigint {direction}");
                return Ok(true);
            }
            let mark = arena.mark();
            let written = crate::sql::catalog::deparse::deparse_expression(
                &context,
                offset,
                crate::sql::catalog::deparse::Destination::Value,
                arena,
            )
            .map(|part| {
                part.map(|part| {
                    let _ = write!(text, " {part} {direction}");
                })
                .is_some()
            });
            // SAFETY: the text was copied out of the arena.
            unsafe { arena.rewind_to(mark) };
            written
        };
        if !bound(&mut text, frame.start)? {
            return Ok(None);
        }
        if frame.between {
            let _ = write!(text, " AND");
            if !bound(&mut text, frame.end)? {
                return Ok(None);
            }
        }
        let _ = write!(
            text,
            "{}",
            match frame.exclusion {
                FrameExclusion::NoOthers => "",
                FrameExclusion::CurrentRow => " EXCLUDE CURRENT ROW",
                FrameExclusion::Group => " EXCLUDE GROUP",
                FrameExclusion::Ties => " EXCLUDE TIES",
            }
        );
    }
    let _ = write!(text, ")");
    Ok((!text.is_truncated()).then_some(text))
}

/// A query's window nodes, the keys its rows leave sorted by, and each
/// window clause's printed name.
type WindowStages<'e> = (
    &'e [PlanNode],
    &'e [crate::sql::ast::OrderBy<'e>],
    &'e [&'e str],
);

/// The scan PostgreSQL's planner chose for a query of one relation with
/// window functions, carrying the sort and limit above its windows.
fn windowed_scan_path<'a>(
    storage: &'a Storage,
    txid: u32,
    model: &query::index_paths::join_query::ModeledJoin<'a>,
    arena: &'a Arena,
) -> Result<query::index_paths::ScanPath<'a>, SqlError> {
    let query::index_paths::joins::PathKind::Scan { rel, path } =
        model.plan.path(model.plan.top).kind
    else {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of window functions over a relation a constant false condition empties is not supported"
        ));
    };
    let mut scan = model.rels[usize::from(rel)]
        .scan(storage, txid, path, &[], arena)?
        .path;
    let upper = model.plan.upper;
    // The scan computes the windows' input (`make_window_input_target`).
    scan.width = model.output_width as u32;
    scan.total_cost = upper.top_total;
    scan.sort = match upper.sort {
        None => query::index_paths::SortKind::None,
        Some((_, _, 0)) => query::index_paths::SortKind::Full,
        Some((_, _, presorted)) => {
            query::index_paths::SortKind::Incremental(usize::from(presorted))
        }
    };
    scan.sort_cost = upper.sort.map(|(startup, total, _)| (startup, total));
    scan.limit_cost = upper.limit;
    Ok(scan)
}

/// The windows PostgreSQL's planner chose for a query
/// (`create_window_paths`): per clause in evaluation order, the positions in
/// its `window_sort_keys` of the keys its pathkeys keep and its nodes'
/// costs, and the rows they return.
#[derive(Clone, Copy)]
struct ModeledWindows<'m> {
    kept: &'m [([u8; query::index_paths::joins::MAX_KEYS], u8)],
    steps: &'m [query::index_paths::joins::WindowStep],
    rows: f64,
}

/// The `WindowAgg` nodes of a query and the sorts of their inputs, bottom
/// first, the keys the last window leaves its rows sorted by, and each
/// window clause's name. Each window clause sorts its input unless the rows
/// (`presorted` on entry) already follow its keys, incrementally when they
/// follow a prefix of them, as PostgreSQL plans.
///
/// `modeled` holds the plan's windows when PostgreSQL's planner was
/// modeled for them (`create_window_paths`): each clause's kept pathkeys,
/// its nodes' costs and the rows; without it the nodes' costs are pos3ql's.
#[expect(clippy::too_many_arguments, reason = "the window plan's context")]
fn window_stages<'e>(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    statement: &'e Select<'e>,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    presorted: &'e [crate::sql::ast::OrderBy<'e>],
    modeled: Option<ModeledWindows<'_>>,
    block: BlockId,
    arena: &'e Arena,
) -> Result<WindowStages<'e>, SqlError> {
    let mut win_nodes = crate::mem::arena::ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            query::collect_windows(expression, &mut win_nodes, storage, txid, columns)?;
        }
    }
    for order in statement.order_by {
        query::collect_windows(order.expression, &mut win_nodes, storage, txid, columns)?;
    }
    let win_nodes = win_nodes.as_slice();
    let windows = query::window_clauses(statement, win_nodes, arena)?;
    let nodes = arena
        .alloc_slice_with(windows.clauses.len() * 2, |_| PlanNode::EMPTY)
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    let mut sorted: &[crate::sql::ast::OrderBy<'e>] = presorted;
    let names = arena
        .alloc_slice_with(windows.clauses.len(), |_| "")
        .map_err(|_| arena_full())?;
    // Unnamed windows take the names `w1`, `w2`, ... the named ones leave.
    let mut next_name = 1usize;
    let last = windows.clauses.len().saturating_sub(1);
    for (index, &(spec, name)) in windows.clauses.iter().enumerate() {
        let position = u16::try_from(index).map_err(|_| arena_full())?;
        let mut keys = query::window_sort_keys(spec, arena)?;
        let step = modeled.map(|modeled| (modeled.steps[index], modeled.kept[index]));
        if let Some((_, (kept, count))) = step {
            // The keys its pathkeys keep (`prepare_sort_from_pathkeys`).
            let all = keys;
            keys = arena
                .alloc_slice_with(usize::from(count), |at| all[usize::from(kept[at])])
                .map_err(|_| arena_full())?;
        }
        let held = match step {
            Some((step, _)) => step.sort.is_none(),
            None => keys.len() <= sorted.len() && *keys == sorted[..keys.len()],
        };
        if !keys.is_empty() && !held {
            let presorted = match step.and_then(|(step, _)| step.sort) {
                Some((_, _, presorted)) => usize::from(presorted),
                None => keys
                    .iter()
                    .zip(sorted)
                    .take_while(|(key, held)| key == held)
                    .count(),
            };
            let items = arena
                .alloc_slice_with(keys.len(), |at| {
                    (
                        keys[at].expression,
                        Some((keys[at].descending, keys[at].nulls_first)),
                    )
                })
                .map_err(|_| arena_full())?;
            let shown = plan_keys(
                storage,
                txid,
                scope,
                statement,
                KeyLabel::Sort,
                items,
                arena,
            )?;
            let mut sort = PlanNode {
                name: StackStr::from_str(if presorted == 0 {
                    "Sort"
                } else {
                    "Incremental Sort"
                }),
                key: Some(NodeKey::new(block, Role::WindowSort(position))),
                keys: shown,
                unmodeled_costs: step.is_none(),
                ..PlanNode::EMPTY
            };
            if let (Some(modeled), Some((step, _))) = (modeled, step)
                && let Some((startup, total, _)) = step.sort
            {
                sort.startup_cost = startup;
                sort.total_cost = total;
                sort.rows = modeled.rows as u64;
                sort.width = step.sort_width as u32;
            }
            if presorted != 0 {
                let prefix = plan_keys(
                    storage,
                    txid,
                    scope,
                    statement,
                    KeyLabel::Sort,
                    &items[..presorted],
                    arena,
                )?;
                sort = sort
                    .with_qual(QualLabel::PresortedKey, presorted_text(&prefix, false))
                    .with_verbose_qual(QualLabel::PresortedKey, presorted_text(&prefix, true));
            }
            nodes[count] = sort;
            count += 1;
            sorted = keys;
        }
        let mut generated: StackStr<16>;
        let name = match name {
            Some(name) => name,
            None => loop {
                generated = StackStr::new();
                let _ = write!(generated, "w{next_name}");
                next_name += 1;
                if !windows
                    .clauses
                    .iter()
                    .any(|(_, held)| *held == Some(generated.as_str()))
                {
                    break generated.as_str();
                }
            },
        };
        names[index] = arena.alloc_str(name).map_err(|_| arena_full())?;
        let optimized_frame = win_nodes
            .iter()
            .zip(windows.of_node)
            .filter(|(_, clause)| **clause == index)
            .all(|(node, _)| query::frame_independent(node));
        let definition = |verbose| {
            window_definition(
                storage,
                txid,
                scope,
                statement,
                name,
                spec,
                optimized_frame,
                verbose,
                arena,
            )
        };
        nodes[count] = PlanNode {
            name: StackStr::from_str("WindowAgg"),
            key: Some(NodeKey::new(
                block,
                if index == last {
                    Role::Window
                } else {
                    Role::WindowAgg(position)
                },
            )),
            unmodeled_costs: step.is_none(),
            startup_cost: step.map_or(0.0, |(step, _)| step.startup),
            total_cost: step.map_or(0.0, |(step, _)| step.total),
            rows: modeled.map_or(0, |modeled| modeled.rows as u64),
            width: step.map_or(0, |(step, _)| step.width as u32),
            ..PlanNode::EMPTY
        }
        .with_qual(QualLabel::Window, definition(false)?)
        .with_verbose_qual(QualLabel::Window, definition(true)?);
        count += 1;
    }
    Ok((&nodes[..count], sorted, names))
}

/// The plan of a foreign UPDATE or DELETE, as postgres_fdw's: one `Foreign
/// Update`/`Foreign Delete` node when the statement ships whole, else the
/// command each row runs above the scan that locks the rows. `false` hands
/// a table with descendants back to the general plan.
fn foreign_modification<'a>(
    plan: &mut Plan<'a>,
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    slot: usize,
    target_node: PlanNode,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let (assignments, inheritance, alias, table_name, predicate, returning) = match statement {
        Stmt::Update(update) => {
            let folded = arena
                .alloc(query::fold_update_constants(update, arena, storage, txid)?)
                .map_err(|_| arena_full())?;
            (
                Some(folded.assignments),
                folded.inheritance,
                folded.alias,
                folded.table.name,
                folded.where_clause,
                folded.returning,
            )
        }
        Stmt::Delete(delete) => {
            let folded = arena
                .alloc(query::fold_delete_constants(delete, arena, storage, txid)?)
                .map_err(|_| arena_full())?;
            (
                None,
                folded.inheritance,
                folded.alias,
                folded.table.name,
                folded.where_clause,
                folded.returning,
            )
        }
        _ => return Ok(false),
    };
    if storage.relation_has_descendants(slot, txid) {
        return Ok(false);
    }
    let exposed = alias.unwrap_or(table_name);
    let direct = match crate::sql::exec::foreign_direct_returned(
        storage, txid, slot, returning, alias, arena,
    )? {
        Some(returned) => crate::sql::foreign::direct_modify_sql(
            storage,
            slot,
            txid,
            exposed,
            inheritance,
            assignments,
            predicate,
            returned,
            arena,
        )?,
        None => None,
    };
    let table = storage.table_def(slot, txid);
    let lists = targets::RelationTargets::returning_scope(
        storage, txid, &table, exposed, returning, false, arena,
    )?;
    plan.nodes[0].set_output(lists.returning(returning)?);
    let scan = |name: &str| PlanNode {
        name: StackStr::from_str(name),
        relation: target_node.relation,
        alias: target_node.alias,
        schema: target_node.schema,
        depth: 1,
        ..PlanNode::EMPTY
    };
    if let Some(sql) = direct {
        let mut node = scan(if assignments.is_some() {
            "Foreign Update"
        } else {
            "Foreign Delete"
        });
        node.remote_sql = remote_sql_text(sql)?;
        plan.push(node)?;
        return Ok(true);
    }
    let mut updated = crate::storage::ColumnSet::EMPTY;
    for (name, _) in assignments.unwrap_or(&[]) {
        if let Some(column) = table.column_index(name) {
            updated.insert(column);
        }
    }
    let event = if assignments.is_some() {
        crate::sql::ast::TriggerEvents::UPDATE
    } else {
        crate::sql::ast::TriggerEvents::DELETE
    };
    let triggers = crate::sql::exec::foreign_row_trigger(storage, slot, txid, event, None);
    let returned = crate::sql::exec::foreign_modify_returned(
        storage, txid, slot, event, returning, alias, false, arena,
    )?;
    let Some(planned) = crate::sql::foreign::modify_scan(
        storage,
        slot,
        txid,
        exposed,
        assignments.is_some().then_some(updated),
        predicate,
        returned,
        triggers,
        arena,
    )?
    else {
        return Ok(false);
    };
    plan.nodes[0].remote_sql = remote_sql_text(planned.modify_sql)?;
    let mut node = scan("Foreign Scan");
    node.remote_sql = remote_sql_text(planned.scan_sql)?;
    // The new values in column order, the row identity, then the old row an
    // UPDATE (or a trigger) needs.
    node.set_output(lists.modified_row(
        assignments.unwrap_or(&[]),
        alias,
        assignments.is_some() || triggers,
        false,
    )?);
    let (_, filter) = single_relation_quals(
        storage,
        txid,
        exposed,
        &table,
        &[],
        planned.local_where,
        &[],
        None,
        true,
        arena,
    )?;
    node = node.with_qual(QualLabel::Filter, filter);
    plan.push(node)?;
    Ok(true)
}

/// The plan of a query whose grouping postgres_fdw ships: the `Foreign
/// Scan` of the grouped relation (`Aggregate on (...)`), filtered by the
/// `HAVING` conditions that stay local, under the `Sort` and `Limit` the
/// remote query leaves. Its output reads each remote target by reference,
/// which prints it in parentheses.
fn foreign_upper_plan<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    pushed: &crate::sql::foreign::deparse::ForeignSelectPlan<'_>,
    qualified: bool,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let table = storage.table_def(pushed.table_slot, txid);
    let base = &statement
        .from
        .as_ref()
        .expect("a pushed scan reads a FROM item")
        .base;
    let exposed = base.alias.unwrap_or(base.table);
    let upper = pushed
        .upper
        .as_ref()
        .expect("a grouped plan has its relation");
    let plain = targets::RelationTargets::new(storage, txid, &table, exposed, qualified, arena)?;
    // `VERBOSE` prefixes the columns of conditions and keys.
    let prefixed = targets::RelationTargets::new(storage, txid, &table, exposed, true, arena)?;
    let (output, above) = match plain.foreign_upper(upper)? {
        Some((output, above)) => (Some(output), Some(above)),
        None => (None, None),
    };
    // `postgresExplainForeignScan`: the relation schema-qualified under
    // `VERBOSE`, then the name the query gives it when that differs.
    let relations_text = |schema: bool| -> Result<StackStr<256>, SqlError> {
        let mut relations = StackStr::<256>::new();
        let _ = relations.write_str("Aggregate on (");
        if schema {
            crate::sql::catalog::write_identifier(&mut relations, table.schema.as_str());
            let _ = relations.write_char('.');
        }
        crate::sql::catalog::write_identifier(&mut relations, table.name.as_str());
        if exposed != table.name.as_str() {
            let _ = relations.write_char(' ');
            crate::sql::catalog::write_identifier(&mut relations, exposed);
        }
        let _ = relations.write_char(')');
        if relations.is_truncated() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "EXPLAIN relation list exceeds 256 bytes"
            ));
        }
        Ok(relations)
    };
    let (relations, plain_relations) = (relations_text(true)?, relations_text(false)?);
    let limited = !pushed.limit_pushed && (statement.limit.is_some() || statement.offset.is_some());
    let sorted = !upper.local_order.is_empty();
    let mut plan = Plan::new(arena, 3)?;
    let mut depth = 0u8;
    if limited {
        let mut limit = PlanNode {
            name: StackStr::from_str("Limit"),
            key: Some(NodeKey::new(block, Role::Limit)),
            total_cost: 100.0,
            rows: 1,
            ..PlanNode::EMPTY
        };
        limit.set_output(above);
        plan.push(limit)?;
        depth += 1;
    }
    if sorted {
        let mut sort = PlanNode {
            name: StackStr::from_str("Sort"),
            key: Some(NodeKey::new(block, Role::Sort)),
            depth,
            total_cost: 100.0,
            rows: 1,
            ..PlanNode::EMPTY
        };
        sort.set_output(above);
        match foreign_upper_sort_keys(&plain, &prefixed, upper)? {
            Some(keys) => sort.keys = keys,
            None => sort.output_unprintable = true,
        }
        plan.push(sort)?;
        depth += 1;
    }
    let mut scan = PlanNode {
        name: StackStr::from_str("Foreign Scan"),
        key: Some(NodeKey::new(block, Role::Scan(0))),
        depth,
        total_cost: 100.0,
        rows: 1,
        relations,
        plain_relations,
        remote_sql: remote_sql_text(pushed.sql)?,
        ..PlanNode::EMPTY
    };
    scan.set_output(output);
    if let Some(condition) = upper.local_having {
        let text = |targets: &targets::RelationTargets<'_>| -> Result<_, SqlError> {
            Ok(targets
                .upper_text(
                    upper,
                    condition,
                    crate::sql::catalog::deparse::Destination::Condition,
                )?
                .map(|(text, _)| StackStr::<512>::from_str(text)))
        };
        let (shown, verbose) = (text(&plain)?, text(&prefixed)?);
        if shown.is_none() || verbose.is_none() {
            scan.output_unprintable = true;
        }
        scan = scan
            .with_qual(QualLabel::Filter, shown)
            .with_verbose_qual(QualLabel::Filter, verbose);
    }
    plan.push(scan)?;
    Ok(plan)
}

/// The `Sort Key` of a sort above a grouped foreign scan: each key as the
/// sort reads it from the scan's output.
fn foreign_upper_sort_keys(
    plain: &targets::RelationTargets<'_>,
    prefixed: &targets::RelationTargets<'_>,
    upper: &crate::sql::foreign::deparse::ForeignUpper<'_>,
) -> Result<Option<PlanKeys>, SqlError> {
    let mut keys = PlanKeys {
        label: KeyLabel::Sort,
        ..PlanKeys::NONE
    };
    for (index, order) in upper.local_order.iter().enumerate() {
        for (targets, text) in [(plain, &mut keys.plain), (prefixed, &mut keys.verbose)] {
            let Some((_, key)) = targets.upper_text(
                upper,
                order.expression,
                crate::sql::catalog::deparse::Destination::Value,
            )?
            else {
                return Ok(None);
            };
            if index != 0 {
                let _ = text.write_char(KEY_SEPARATOR);
            }
            let _ = text.write_str(key);
            if order.descending {
                let _ = text.write_str(" DESC");
            }
            match (order.descending, order.nulls_first) {
                (false, true) => {
                    let _ = text.write_str(" NULLS FIRST");
                }
                (true, false) => {
                    let _ = text.write_str(" NULLS LAST");
                }
                _ => {}
            }
        }
    }
    if keys.plain.is_truncated() || keys.verbose.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "EXPLAIN sort keys exceed 256 bytes"
        ));
    }
    Ok(Some(keys))
}

/// postgres_fdw's `Foreign Scan` of a query's one foreign table: its output
/// carries the columns of the order the remote query applies.
#[expect(clippy::too_many_arguments, reason = "the scan's plan context")]
fn foreign_scan_node(
    scan: &mut PlanNode,
    pushed: &crate::sql::foreign::deparse::ForeignSelectPlan<'_>,
    statement: &Select<'_>,
    scope: &QueryScope<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    aggregate: bool,
    gated: bool,
    qualify: bool,
) -> Result<Option<targets::Output>, SqlError> {
    scan.name = StackStr::from_str("Foreign Scan");
    scan.remote_sql = remote_sql_text(pushed.sql)?;
    // Under a local aggregate or window the scan returns what those nodes
    // read, as any relation scan there does.
    if aggregate || has_window(statement) {
        return Ok(None);
    }
    let table = storage.table_def(pushed.table_slot, txid);
    let outputs =
        targets::RelationTargets::new(storage, txid, &table, scope.names[0], qualify, arena)?
            .foreign_scan(statement, pushed.sort_keys)?;
    // Under a gating `Result` the scan returns its physical columns and the
    // `Result` computes the target.
    if !gated {
        scan.set_output(outputs.map(|(output, _)| output));
    }
    Ok(outputs.map(|(_, above)| above))
}

/// Whether a query's `LIMIT` and `OFFSET` need a `Limit` node, as
/// PostgreSQL's `limit_needed` decides: a constant `LIMIT ALL` (NULL) limits
/// nothing and a constant `OFFSET` of zero or NULL skips nothing. A subquery
/// keeps its `OFFSET 0` all the same, so it is not pulled up.
fn limit_needed(limit: Option<&Expr<'_>>, offset: Option<&Expr<'_>>) -> bool {
    fn constant<'e>(mut expression: &'e Expr<'e>) -> &'e Expr<'e> {
        while let Expr::Cast { operand, .. } = expression {
            expression = operand;
        }
        expression
    }
    let limited = limit.is_some_and(|limit| !matches!(constant(limit), Expr::Null));
    let offset =
        offset.is_some_and(|offset| !matches!(constant(offset), Expr::Null | Expr::Int(0)));
    limited || offset
}

/// The plan of a derived table's own query.
fn plan_derived<'a>(
    storage: &Storage,
    txid: u32,
    body: &Select<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    in_nested_block(|| plan_derived_body(storage, txid, body, outer, block, arena))
}

fn plan_derived_body<'a>(
    storage: &Storage,
    txid: u32,
    body: &Select<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    match body.set_body {
        Some(tree) => plan_set_query(
            storage,
            txid,
            &SetQuery {
                with: body.with,
                body: query::expand_set_tree(body.with, tree, storage, txid, None, None, arena)?,
                order_by: body.order_by,
                limit: body.limit,
                offset: body.offset,
                with_ties: body.with_ties,
                locking: body.locking,
            },
            outer,
            block,
            arena,
        ),
        None => plan_select(storage, txid, body, outer, block, arena),
    }
}

/// `plan` with `derived`, the plan of the derived table exposed as `name`,
/// under its `Subquery Scan`. PostgreSQL drops a scan that neither filters
/// nor reshapes the rows (`SELECT * FROM (...) s`) and runs the derived
/// plan in its place.
/// Whether a query's target list holds entries its select list lacks
/// (`resjunk`): `ORDER BY`, `GROUP BY`, `DISTINCT ON` or window keys that
/// are not output columns. A subquery whose plan returns them keeps its
/// `Subquery Scan` (`trivial_subqueryscan`).
fn has_resjunk_targets(body: &Select<'_>) -> bool {
    let listed = |key: &Expr<'_>| {
        match key {
        Expr::Int(position) => usize::try_from(*position)
            .is_ok_and(|position| position >= 1 && position <= body.items.len()),
        key => body.items.iter().any(|item| match item {
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                matches!(key, Expr::Column { .. })
            }
            SelectItem::Expr { expression, alias } => {
                **expression == *key
                    || matches!(
                        (key, expression),
                        (Expr::Column { qualifier: None, name }, _) if alias.is_some_and(|alias| alias == *name)
                    )
                    || matches!(
                        (key, expression),
                        (Expr::Column { name, .. }, Expr::Column { name: listed, .. }) if name == listed
                    )
            }
            SelectItem::RecordStar(_) => false,
        }),
    }
    };
    let window_keys = |spec: &crate::sql::ast::WindowSpec<'_>| {
        spec.partition_by.iter().all(|key| listed(key))
            && spec.order_by.iter().all(|key| listed(key.expression))
    };
    let mut inline_listed = true;
    for item in body.items {
        if let SelectItem::Expr { expression, .. } = item {
            // A walk that fails leaves the scan in place, as a resjunk
            // entry would.
            let mut keys_listed = true;
            let walked = query::walk_expression_tree(expression, &mut |node| {
                if let Expr::Call {
                    over: Some(spec), ..
                } = node
                {
                    keys_listed &= window_keys(spec);
                }
            });
            inline_listed &= keys_listed && walked.is_ok();
        }
    }
    !(inline_listed
        && body.windows.iter().all(|spec| window_keys(spec))
        && body
            .order_by
            .iter()
            .map(|key| key.expression)
            .chain(body.group_by.iter().copied())
            .chain(body.distinct_on.iter().copied())
            .all(listed))
}

fn with_derived_plan<'a>(
    plan: Plan<'a>,
    name: &str,
    passes_through: bool,
    derived: Plan<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let Some(at) = plan.nodes[..plan.count].iter().position(|node| {
        node.name.as_str() == "Subquery Scan"
            && node.relation.as_str().is_empty()
            && node.alias.as_str() == name
    }) else {
        return Ok(plan);
    };
    let scan = plan.nodes[at];
    let replaced = passes_through && scan.qualifications().next().is_none();
    // A nested loop rescans an inner plan that keeps its own rows without
    // a `Materialize` (`ExecMaterializesOutput`).
    let materialized_above = at > 0
        && plan.nodes[at - 1].name.as_str() == "Materialize"
        && plan.nodes[at - 1].depth + 1 == scan.depth;
    let unmaterialized = replaced
        && materialized_above
        && derived.nodes[..derived.count].first().is_some_and(|top| {
            matches!(
                top.name.as_str(),
                "Sort" | "Incremental Sort" | "Materialize" | "Function Scan" | "CTE Scan"
            )
        });
    let mut expanded = Plan::new(arena, plan.count + derived.count)?;
    expanded.methods = plan.methods;
    for node in &plan.nodes[..at - usize::from(unmaterialized)] {
        expanded.push(*node)?;
    }
    let depth = if unmaterialized {
        scan.depth - 1
    } else if replaced {
        scan.depth
    } else {
        expanded.push(scan)?;
        scan.depth + 1
    };
    for node in &derived.nodes[..derived.count] {
        // The derived table adds a range table entry, so PostgreSQL
        // qualifies the columns of its keys.
        let mut keys = node.keys;
        keys.plain = keys.verbose;
        expanded.push(PlanNode {
            depth: node.depth.saturating_add(depth),
            keys,
            ..*node
        })?;
    }
    for node in &plan.nodes[at + 1..plan.count] {
        expanded.push(*node)?;
    }
    Ok(expanded)
}

/// One `Sort Key` or `Group Key` item: an expression and, for a sort, its
/// direction and NULLS placement.
type KeyItem<'e> = (&'e Expr<'e>, Option<(bool, bool)>);

/// PostgreSQL's key list for `keys` of `statement`: an output-column
/// position (or, for `ORDER BY`, an output name) stands for that column's
/// expression; a column prints bare and any other expression in
/// parentheses, followed by a sort's `DESC` and non-default `NULLS`
/// placement. No keys when a key reads something other than stored tables.
fn plan_keys(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    statement: &Select<'_>,
    label: KeyLabel,
    keys: &[KeyItem<'_>],
    arena: &Arena,
) -> Result<PlanKeys, SqlError> {
    plan_keys_unscanned(storage, txid, scope, statement, label, keys, 0, arena)
}

/// [`plan_keys`] of a plan that does not scan the relations of
/// `unscanned` (a join's dummy relations), whose columns print unqualified.
#[expect(clippy::too_many_arguments, reason = "explain key plumbing")]
fn plan_keys_unscanned(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    statement: &Select<'_>,
    label: KeyLabel,
    keys: &[KeyItem<'_>],
    unscanned: u64,
    arena: &Arena,
) -> Result<PlanKeys, SqlError> {
    // A query without FROM reads no relation.
    let relations = scope.map_or(0, |scope| scope.n);
    if keys.is_empty() {
        return Ok(PlanKeys::NONE);
    }
    let Some(joined) = key_relations(scope, statement, arena)? else {
        return Ok(PlanKeys::NONE);
    };
    let joined = if unscanned == 0 {
        joined
    } else {
        &*arena
            .alloc_slice_with(joined.len(), |table| {
                let mut relation = joined[table];
                if unscanned & 1 << table != 0 {
                    relation.printed = "";
                }
                relation
            })
            .map_err(|_| arena_full())?
    };
    let mut shown = PlanKeys {
        label,
        ..PlanKeys::NONE
    };
    for (verbose, text) in [(false, &mut shown.plain), (true, &mut shown.verbose)] {
        let context = crate::sql::catalog::deparse::Context {
            storage,
            txid,
            table: None,
            parameters: &[],
            joined,
            pretty: false,
            qualify: verbose || relations > 1 || shares_range_table(statement),
            bare: None,
            domain_value: None,
            sublinks: Some(&registered_sublink),
            references: None,
            folded: true,
            arena,
            dependencies: None,
        };
        for (index, &(key, order)) in keys.iter().enumerate() {
            let expression = match key {
                Expr::Int(position) => match usize::try_from(*position)
                    .ok()
                    .and_then(|position| statement.items.get(position.checked_sub(1)?))
                {
                    Some(SelectItem::Expr { expression, .. }) => *expression,
                    _ => return Ok(PlanKeys::NONE),
                },
                Expr::Column {
                    qualifier: None,
                    name,
                } if order.is_some() => statement
                    .items
                    .iter()
                    .find_map(|item| match item {
                        SelectItem::Expr {
                            expression,
                            alias: Some(alias),
                        } if alias == name => Some(*expression),
                        _ => None,
                    })
                    .unwrap_or(key),
                _ => key,
            };
            let expression = crate::sql::catalog::deparse::planned(expression);
            let mark = arena.mark();
            // A collated sort key prints relabeled, then its collation; a
            // grouping key as the input computes it, its relabeling shown.
            let collated = match label {
                KeyLabel::Sort => {
                    crate::sql::catalog::deparse::planned_collated_key(&context, expression, arena)
                }
                KeyLabel::Group => {
                    crate::sql::catalog::deparse::deparse_shown(&context, expression, arena)
                }
                KeyLabel::None | KeyLabel::Sets => Ok(None),
            };
            let (collated, part) = match collated {
                Ok(Some(part)) => (true, Ok(Some(part))),
                Ok(None) => (
                    false,
                    crate::sql::catalog::deparse::deparse_expression(
                        &context,
                        expression,
                        crate::sql::catalog::deparse::Destination::Value,
                        arena,
                    ),
                ),
                Err(error) => (false, Err(error)),
            };
            let written = part.map(|part| {
                part.map(|part| {
                    if index != 0 {
                        let _ = text.write_char(KEY_SEPARATOR);
                    }
                    // A grouping key prints as written; a sort key reads its
                    // expression from the input's output, which PostgreSQL
                    // parenthesizes.
                    if collated
                        || matches!(expression, Expr::Column { .. })
                        || (label == KeyLabel::Group
                            && (matches!(expression, Expr::Call { .. })
                                || fully_parenthesized(part)))
                    {
                        let _ = write!(text, "{part}");
                    } else {
                        let _ = write!(text, "({part})");
                    }
                })
                .is_some()
            });
            // SAFETY: the text was copied out of the arena.
            unsafe { arena.rewind_to(mark) };
            if !written? {
                return Ok(PlanKeys::NONE);
            }
            if let Some((descending, nulls_first)) = order {
                if descending {
                    let _ = write!(text, " DESC");
                }
                match (descending, nulls_first) {
                    (false, true) => {
                        let _ = write!(text, " NULLS FIRST");
                    }
                    (true, false) => {
                        let _ = write!(text, " NULLS LAST");
                    }
                    _ => {}
                }
            }
        }
        if text.is_truncated() {
            return Ok(PlanKeys::NONE);
        }
    }
    Ok(shown)
}

/// Whether the statement's range table holds relations besides this
/// query's own: a subquery's, or the query around a subquery. PostgreSQL
/// then qualifies the columns of sort and grouping keys.
fn shares_range_table(statement: &Select<'_>) -> bool {
    if SUBLINK_DEPTH.with(core::cell::Cell::get) != 0
        || SUBPLAN_RELATIONS.with(core::cell::Cell::get)
        || EXPANDED_RELATIONS.with(core::cell::Cell::get)
    {
        return true;
    }
    let mut found = false;
    let mut visit = |expression: &Expr<'_>| {
        visit_sublinks(expression, &mut |node| {
            found |= sublink_query(node).is_some()
        });
    };
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            visit(expression);
        }
    }
    if let Some(condition) = statement.where_clause {
        visit(condition);
    }
    if let Some(having) = statement.having {
        visit(having);
    }
    for count in statement.limit.into_iter().chain(statement.offset) {
        visit(count);
    }
    found
}

/// The relations a block's sort and grouping keys read, as PostgreSQL names
/// their columns: a stored table's, a function scan's (`g`) and a pulled-up
/// `VALUES` list's (`"*VALUES*".column1`). None when one is a derived table,
/// whose columns PostgreSQL prints as the expressions of its query.
fn key_relations<'a>(
    scope: Option<&'a QueryScope<'_>>,
    statement: &Select<'_>,
    arena: &'a Arena,
) -> Result<Option<&'a [crate::sql::catalog::deparse::Joined<'a>]>, SqlError> {
    use crate::sql::catalog::deparse::{Column, Columns, Joined};
    let Some(scope) = scope else {
        return Ok(Some(&[]));
    };
    let items = statement
        .from
        .as_ref()
        .map(|from| core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)));
    let mut items = items.into_iter().flatten();
    let joined = arena
        .alloc_slice_with(scope.n, |_| None::<Joined<'a>>)
        .map_err(|_| arena_full())?;
    for (table, slot) in joined.iter_mut().enumerate() {
        let item = items.next();
        if let Some(definition) = scope.defs.stored(table) {
            *slot = Some(Joined::new(scope.names[table], definition));
            continue;
        }
        let Some(item) = item else {
            return Ok(None);
        };
        let values = item.subquery.and_then(values_list).is_some();
        if !(values
            || item.cte.is_some()
            || (item.is_function_source() && item.rows_from.is_none()))
        {
            return Ok(None);
        }
        let Some(definition) = scope.defs.get(table) else {
            return Ok(None);
        };
        let columns = arena
            .alloc_slice_with(definition.n_columns, |index| Column {
                name: definition.columns[index].name.as_str(),
                type_oid: None,
                merged: None,
            })
            .map_err(|_| arena_full())?;
        if values {
            for (index, column) in columns.iter_mut().enumerate() {
                let printed = arena
                    .alloc_str_display(format_args!("column{}", index + 1))
                    .map_err(|_| arena_full())?;
                column.merged = Some((Some("*VALUES*"), printed));
            }
        }
        *slot = Some(Joined {
            unqualified: true,
            ..Joined::rule_columns(scope.names[table], Columns::Derived(columns))
        });
    }
    if joined.iter().any(Option::is_none) {
        return Ok(None);
    }
    Ok(Some(
        &*arena
            .alloc_slice_with(scope.n, |table| joined[table].expect("checked"))
            .map_err(|_| arena_full())?,
    ))
}

/// Whether `text` is one parenthesized expression.
fn fully_parenthesized(text: &str) -> bool {
    let Some(inner) = text.strip_prefix('(') else {
        return false;
    };
    let mut depth = 1usize;
    let mut quoted = None;
    for (index, character) in inner.char_indices() {
        match (quoted, character) {
            (Some(quote), _) if character == quote => quoted = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quoted = Some(character),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return index + 1 == inner.len();
                }
            }
            _ => {}
        }
    }
    false
}

/// `statement`'s `ORDER BY` as key items.
fn order_keys<'e>(statement: &Select<'e>, arena: &'e Arena) -> Result<&'e [KeyItem<'e>], SqlError> {
    arena
        .alloc_slice_with(statement.order_by.len(), |index| {
            let order = statement.order_by[index];
            (
                order.expression,
                Some((order.descending, order.nulls_first)),
            )
        })
        .map(|keys| &*keys)
        .map_err(|_| arena_full())
}

/// `statement`'s `GROUP BY`, or for `SELECT DISTINCT` its output columns, as
/// key items. None when an output column is a wildcard.
fn group_keys<'e>(
    statement: &Select<'e>,
    arena: &'e Arena,
) -> Result<Option<&'e [KeyItem<'e>]>, SqlError> {
    if !statement.group_by.is_empty() {
        return arena
            .alloc_slice_with(statement.group_by.len(), |index| {
                (statement.group_by[index], None)
            })
            .map(|keys| Some(&*keys))
            .map_err(|_| arena_full());
    }
    if statement
        .items
        .iter()
        .any(|item| !matches!(item, SelectItem::Expr { .. }))
    {
        return Ok(None);
    }
    arena
        .alloc_slice_with(statement.items.len(), |index| {
            match statement.items[index] {
                SelectItem::Expr { expression, .. } => (expression, None),
                // Checked above: every output column is an expression.
                _ => (&Expr::Null, None),
            }
        })
        .map(|keys| Some(&*keys))
        .map_err(|_| arena_full())
}

/// The `VALUES` list of several rows `select` consists of, if it is one.
fn values_list<'s>(select: &'s Select<'s>) -> Option<&'s SetTree<'s>> {
    let tree = select.set_body?;
    let plain = select.order_by.is_empty()
        && select.limit.is_none()
        && select.offset.is_none()
        && select.with.is_empty();
    (plain
        && matches!(
            tree,
            SetTree::Op {
                syntax: crate::sql::ast::SetOperationSyntax::Values,
                ..
            }
        )
        && values_rows(tree).is_some())
    .then_some(tree)
}

/// PostgreSQL's name of a join node executing a join of `kind` (the last
/// join's when several are listed).
fn join_node(kind: Option<crate::sql::ast::JoinKind>, hash: bool) -> &'static str {
    match (kind, hash) {
        (Some(crate::sql::ast::JoinKind::Semi), false) => "Nested Loop Semi Join",
        (Some(crate::sql::ast::JoinKind::Anti), false) => "Nested Loop Anti Join",
        (Some(crate::sql::ast::JoinKind::Left), true) => "Hash Left Join",
        (Some(crate::sql::ast::JoinKind::Left), false) => "Nested Loop Left Join",
        // The executor keeps each preserved row that found no match.
        (Some(crate::sql::ast::JoinKind::Right), false) => "Nested Loop Right Join",
        (Some(crate::sql::ast::JoinKind::Full), false) => "Nested Loop Full Join",
        (_, true) => "Hash Join",
        (_, false) => "Nested Loop",
    }
}

/// pos3ql plans no parallel query. With `max_parallel_workers_per_gather`
/// above zero PostgreSQL gives a scan of a relation of at least
/// `min_parallel_table_scan_size` pages partial paths (`create_plain_partial_paths`
/// through `compute_parallel_worker`), which may win: such a plan's EXPLAIN
/// fails with `0A000` rather than print a plan PostgreSQL may not choose.
/// Smaller relations get no parallel workers, so their plans are exact at
/// any setting.
pub(super) fn refuse_parallel_candidates(
    plan: &Plan<'_>,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    if crate::sql::guc::planner_value("max_parallel_workers_per_gather") == 0.0 {
        return Ok(());
    }
    let nodes = &plan.nodes[..plan.count];
    // A write or a row lock is never planned in parallel
    // (`standard_planner`'s `parallelModeOK`, `max_parallel_hazard`).
    if nodes.iter().any(|node| {
        let name = node.name.as_str();
        ["LockRows", "Insert", "Update", "Delete", "Merge"].contains(&name)
    }) {
        return Ok(());
    }
    let threshold = crate::sql::guc::planner_value("min_parallel_table_scan_size");
    for node in nodes {
        // Only a relation's own scan gets partial paths.
        if node.relation.is_empty() || !node.name.as_str().ends_with("Scan") {
            continue;
        }
        let schema = (!node.schema.is_empty()).then_some(node.schema.as_str());
        let Some(slot) = relation_slot(storage, txid, schema, node.relation.as_str()) else {
            continue;
        };
        if query::index_paths::relation_size(storage, slot, txid).pages >= threshold {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN of a scan of \"{}\", which PostgreSQL may run in parallel, is not supported with max_parallel_workers_per_gather above 0",
                node.relation.as_str()
            ));
        }
    }
    Ok(())
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
        alias: StackStr::new(),
        output: StackStr::new(),
        depth,
        startup_cost: 0.0,
        total_cost,
        rows: output_rows,
        width,
        object_requests,
        cache_blocks,
        constant_false: false,
        quals: NO_QUALS,
        keys: PlanKeys::NONE,
        key: None,
        hash_columns: None,
        verbose_quals: NO_QUALS,
        schema: StackStr::from_str(storage.table_def(slot, txid).schema.as_str()),
        subplan: None,
        shift: 0,
        relations: StackStr::new(),
        plain_relations: StackStr::new(),
        output_unprintable: false,
        unmodeled_costs: false,
        remote_sql: StackStr::new(),
        batch_size: None,
        passthrough: false,
        subplans_removed: None,
        target_member: false,
        scan_pruned: false,
        uncounted: false,
        inner_unique: false,
        bitmap_index_estimate: None,
        analyze_unsupported: None,
        planned_partitions: None,
        hash_agg: None,
        initplan_charge: 0.0,
    }
}

/// Pushes the plan of `tree` at `depth`. Its set operations are numbered in
/// pre-order from `numbers.0` and its leaves left to right from `numbers.1`,
/// as execution numbers them. With `flatten`, a `UNION ALL` joins the
/// `Append` it is an input of.
#[allow(clippy::too_many_arguments)]
fn push_set_tree(
    plan: &mut Plan<'_>,
    storage: &Storage,
    txid: u32,
    tree: &SetTree<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    arena: &Arena,
    depth: u8,
    block: BlockId,
    numbers: &mut (u16, u16),
    flatten: bool,
) -> Result<PlanNode, SqlError> {
    match tree {
        SetTree::Op {
            syntax: crate::sql::ast::SetOperationSyntax::Values,
            ..
        } => {
            let node = values_scan(plan, tree, depth);
            plan.push(node)?;
            Ok(node)
        }
        SetTree::Select(select) => {
            let branch = block.child(Segment::Branch(numbers.1));
            numbers.1 = numbers.1.saturating_add(1);
            // A set operation's leaves are subqueries of its statement.
            let leaf =
                in_nested_block(|| plan_select(storage, txid, select, outer, branch, arena))?;
            let root = leaf.nodes[0];
            for node in &leaf.nodes[..leaf.count] {
                let mut node = *node;
                node.depth = node.depth.saturating_add(depth);
                plan.push(node)?;
            }
            Ok(root)
        }
        SetTree::Op {
            operator: SetOp::Union,
            all: true,
            ..
        } if flatten => {
            // An input that is itself a `UNION ALL` joins its parent's
            // `Append`, as PostgreSQL flattens one.
            numbers.0 = numbers.0.saturating_add(1);
            let SetTree::Op { left, right, .. } = tree else {
                unreachable!("matched a set operation");
            };
            let left = push_set_tree(
                plan, storage, txid, left, outer, arena, depth, block, numbers, true,
            )?;
            let right = push_set_tree(
                plan, storage, txid, right, outer, arena, depth, block, numbers, true,
            )?;
            Ok(PlanNode {
                rows: left.rows.saturating_add(right.rows),
                total_cost: left.total_cost + right.total_cost,
                width: left.width.max(right.width),
                object_requests: left.object_requests.saturating_add(right.object_requests),
                cache_blocks: left.cache_blocks.saturating_add(right.cache_blocks),
                ..left
            })
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
            let index = numbers.0;
            let role = Role::SetOperation(index);
            numbers.0 = numbers.0.saturating_add(1);
            // PostgreSQL hashes a distinct union's rows from one `Append`
            // of its inputs.
            let hashed_union = *operator == SetOp::Union && !*all;
            let input_depth = if hashed_union {
                plan.push(PlanNode {
                    name: StackStr::from_str("Append"),
                    depth: depth.saturating_add(1),
                    key: Some(NodeKey::new(block, Role::SetInput(index))),
                    ..PlanNode::EMPTY
                })?;
                depth.saturating_add(2)
            } else {
                depth.saturating_add(1)
            };
            let left_tree = left;
            let appended = *operator == SetOp::Union && *all;
            let left = push_set_tree(
                plan,
                storage,
                txid,
                left,
                outer,
                arena,
                input_depth,
                block,
                numbers,
                appended,
            )?;
            let right = push_set_tree(
                plan,
                storage,
                txid,
                right,
                outer,
                arena,
                input_depth,
                block,
                numbers,
                appended,
            )?;
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
                alias: StackStr::new(),
                output: left.output,
                depth,
                startup_cost: 0.0,
                total_cost: left.total_cost + right.total_cost + rows as f64 * 0.01,
                rows,
                width: left.width.max(right.width),
                object_requests: left.object_requests.saturating_add(right.object_requests),
                cache_blocks: left.cache_blocks.saturating_add(right.cache_blocks),
                constant_false: false,
                quals: NO_QUALS,
                keys: if hashed_union {
                    union_group_keys(left_tree)
                } else {
                    PlanKeys::NONE
                },
                key: Some(NodeKey::new(block, role)),
                hash_columns: None,
                verbose_quals: NO_QUALS,
                schema: StackStr::new(),
                subplan: None,
                shift: 0,
                relations: StackStr::new(),
                plain_relations: StackStr::new(),
                output_unprintable: left.output_unprintable,
                unmodeled_costs: left.unmodeled_costs,
                remote_sql: StackStr::new(),
                batch_size: None,
                passthrough: false,
                subplans_removed: None,
                target_member: false,
                scan_pruned: false,
                uncounted: false,
                inner_unique: false,
                bitmap_index_estimate: None,
                analyze_unsupported: None,
                planned_partitions: None,
                hash_agg: None,
                initplan_charge: 0.0,
            };
            let node = if hashed_union {
                let width = node.width;
                node.with_distinct_hash(rows, width)
            } else {
                node
            };
            plan.nodes[at] = node;
            if hashed_union {
                let input = &mut plan.nodes[at + 1];
                input.rows = left.rows.saturating_add(right.rows);
                input.width = node.width;
                input.output = node.output;
                input.output_unprintable = node.output_unprintable;
                input.total_cost = left.total_cost + right.total_cost;
            }
            Ok(node)
        }
    }
}

/// A distinct union's `Group Key`: the columns of its first branch, which
/// PostgreSQL names by that branch's relation. No keys unless that branch
/// selects only columns of one relation.
fn union_group_keys(tree: &SetTree<'_>) -> PlanKeys {
    let mut first = tree;
    while let SetTree::Op { left, .. } = first {
        first = left;
    }
    let SetTree::Select(branch) = first else {
        return PlanKeys::NONE;
    };
    let Some(from) = branch.from.as_ref().filter(|from| {
        from.joins.is_empty() && from.base.subquery.is_none() && !from.base.is_function_source()
    }) else {
        return PlanKeys::NONE;
    };
    let relation = from.base.alias.unwrap_or(from.base.table);
    let mut keys = PlanKeys {
        label: KeyLabel::Group,
        ..PlanKeys::NONE
    };
    for (index, item) in branch.items.iter().enumerate() {
        let SelectItem::Expr {
            expression: Expr::Column { qualifier, name },
            ..
        } = item
        else {
            return PlanKeys::NONE;
        };
        if qualifier.is_some_and(|qualifier| qualifier != relation) {
            return PlanKeys::NONE;
        }
        if index != 0 {
            let _ = keys.plain.write_char(KEY_SEPARATOR);
        }
        crate::sql::catalog::write_identifier(&mut keys.plain, relation);
        let _ = keys.plain.write_char('.');
        crate::sql::catalog::write_identifier(&mut keys.plain, name);
    }
    keys.verbose = keys.plain;
    if keys.plain.is_truncated() {
        return PlanKeys::NONE;
    }
    keys
}

/// The `VERBOSE` output of a query reading only a `VALUES` list, which
/// PostgreSQL pulls up and names `"*VALUES*".column1`, ...: when every item
/// is a star or a column of the list.
fn values_output(
    statement: &Select<'_>,
    scope: &QueryScope<'_>,
    list: &SetTree<'_>,
) -> Option<StackStr<256>> {
    let (_, width) = values_rows(list)?;
    let definition = scope.defs.get(0)?;
    let mut output = StackStr::<256>::new();
    let mut push = |column: usize| {
        if !output.as_str().is_empty() {
            let _ = write!(output, ", ");
        }
        let _ = write!(output, "\"*VALUES*\".column{}", column + 1);
    };
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) => (0..width).for_each(&mut push),
            SelectItem::Expr {
                expression: Expr::Column { qualifier, name },
                ..
            } if qualifier.is_none_or(|qualifier| qualifier == scope.names[0]) => {
                let column = definition.columns[..definition.n_columns]
                    .iter()
                    .position(|column| column.name.as_str() == *name)?;
                push(column);
            }
            _ => return None,
        }
    }
    (!output.is_truncated()).then_some(output)
}

/// The rows of a `VALUES` list of several rows, which the parser keeps as a
/// `UNION ALL` chain marked as `VALUES`.
fn values_rows(tree: &SetTree<'_>) -> Option<(u64, usize)> {
    match tree {
        SetTree::Select(select) => Some((1, select.items.len())),
        SetTree::Op {
            syntax: crate::sql::ast::SetOperationSyntax::Values,
            left,
            right,
            ..
        } => {
            let (left_rows, width) = values_rows(left)?;
            let (right_rows, _) = values_rows(right)?;
            Some((left_rows + right_rows, width))
        }
        SetTree::Op { .. } => None,
    }
}

/// PostgreSQL's `Values Scan` of a `VALUES` list of several rows. Its range
/// table entry is `*VALUES*`; later lists in the same plan get a numeric
/// suffix (`*VALUES*_1`).
fn values_scan(plan: &Plan<'_>, tree: &SetTree<'_>, depth: u8) -> PlanNode {
    let (rows, columns) = values_rows(tree).unwrap_or((1, 0));
    let earlier = plan.nodes[..plan.count]
        .iter()
        .filter(|node| node.name.as_str() == "Values Scan")
        .count();
    let mut alias = StackStr::new();
    if earlier == 0 {
        let _ = write!(alias, "*VALUES*");
    } else {
        let _ = write!(alias, "*VALUES*_{earlier}");
    }
    let mut output = StackStr::new();
    for column in 1..=columns {
        let _ = write!(
            output,
            "{}column{column}",
            if column == 1 { "" } else { ", " }
        );
    }
    PlanNode {
        name: StackStr::from_str("Values Scan"),
        alias,
        output,
        depth,
        total_cost: rows as f64 * 0.0125,
        rows,
        width: 4 * columns as u32,
        ..PlanNode::EMPTY
    }
}

/// A set operation's `Sort Key`: its result columns, named as its first
/// branch names them. No keys for an ordering by an expression.
fn set_sort_keys(query: &SetQuery<'_>) -> PlanKeys {
    let mut first = query.body;
    while let SetTree::Op { left, .. } = first {
        first = left;
    }
    let SetTree::Select(branch) = first else {
        return PlanKeys::NONE;
    };
    // A set operation's range table holds its branches, so PostgreSQL
    // prints each key as the first branch's column, qualified by its
    // relation.
    let relation = branch.from.as_ref().and_then(|from| {
        (from.joins.is_empty() && from.base.subquery.is_none() && from.base.func_args.is_none())
            .then(|| from.base.alias.unwrap_or(from.base.table))
    });
    fn output_name<'s>(item: &SelectItem<'s>) -> Option<&'s str> {
        match item {
            SelectItem::Expr {
                alias: Some(alias), ..
            } => Some(alias),
            SelectItem::Expr {
                expression: Expr::Column { name, .. },
                alias: None,
            } => Some(name),
            _ => None,
        }
    }
    let column = |position: usize| match branch.items.get(position)? {
        // A `VALUES` row's columns print bare, by their names.
        item if branch.from.is_none() => Some((None, output_name(item)?)),
        SelectItem::Expr {
            expression: Expr::Column { qualifier, name },
            ..
        } => match qualifier.or(relation) {
            Some(qualifier) => Some((Some(qualifier), *name)),
            // A `VALUES` list's columns print bare.
            None if branch.from.as_ref().is_none_or(|from| {
                from.joins.is_empty() && from.base.subquery.and_then(values_list).is_some()
            }) =>
            {
                Some((None, *name))
            }
            None => None,
        },
        _ => None,
    };
    let mut keys = PlanKeys {
        label: KeyLabel::Sort,
        ..PlanKeys::NONE
    };
    for (index, order) in query.order_by.iter().enumerate() {
        let position = match order.expression {
            Expr::Int(position) => usize::try_from(*position)
                .ok()
                .and_then(|position| position.checked_sub(1)),
            Expr::Column {
                qualifier: None,
                name,
            } => branch
                .items
                .iter()
                .position(|item| output_name(item).is_some_and(|output| output == *name)),
            _ => None,
        };
        let Some((qualifier, name)) = position.and_then(column) else {
            return PlanKeys::NONE;
        };
        if index != 0 {
            let _ = keys.plain.write_char(KEY_SEPARATOR);
        }
        if let Some(qualifier) = qualifier {
            crate::sql::catalog::write_identifier(&mut keys.plain, qualifier);
            let _ = keys.plain.write_char('.');
        }
        crate::sql::catalog::write_identifier(&mut keys.plain, name);
        if order.descending {
            let _ = write!(keys.plain, " DESC");
        }
        match (order.descending, order.nulls_first) {
            (false, true) => {
                let _ = write!(keys.plain, " NULLS FIRST");
            }
            (true, false) => {
                let _ = write!(keys.plain, " NULLS LAST");
            }
            _ => {}
        }
    }
    keys.verbose = keys.plain;
    if keys.plain.is_truncated() {
        return PlanKeys::NONE;
    }
    keys
}

pub(super) fn plan_set_query<'a>(
    storage: &Storage,
    txid: u32,
    query: &SetQuery<'_>,
    outer: Option<&dyn ColumnLookup<'_>>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    let limited = limit_needed(query.limit, query.offset);
    let wrapper_count = usize::from(!query.order_by.is_empty()) + usize::from(limited);
    let mut plan = Plan::new(
        arena,
        wrapper_count.saturating_add(set_tree_plan_capacity(query.body)),
    )?;
    for depth in 0..wrapper_count {
        plan.push(PlanNode {
            name: StackStr::from_str(if limited && depth == 0 {
                "Limit"
            } else {
                "Sort"
            }),
            key: Some(NodeKey::new(
                block,
                if limited && depth == 0 {
                    Role::Limit
                } else {
                    Role::Sort
                },
            )),
            depth: depth as u8,
            ..PlanNode::EMPTY
        })?;
    }
    let root = push_set_tree(
        &mut plan,
        storage,
        txid,
        query.body,
        outer,
        arena,
        wrapper_count as u8,
        block,
        &mut (0, 0),
        false,
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
        if node.name.as_str() == "Sort" {
            node.keys = set_sort_keys(query);
        }
        plan.nodes[at] = node;
        child = node;
    }
    // PostgreSQL names each range table entry uniquely: a relation the
    // branches read again gets a numeric suffix (`t1_1`).
    for at in 0..plan.count {
        let node = plan.nodes[at];
        if node.relation.as_str().is_empty() {
            continue;
        }
        let name = node.exposed_name();
        let earlier = plan.nodes[..at]
            .iter()
            .filter(|earlier| !earlier.relation.as_str().is_empty())
            .filter(|earlier| {
                let exposed = earlier.exposed_name();
                exposed == name
                    || exposed
                        .strip_prefix(name)
                        .and_then(|suffix| suffix.strip_prefix('_'))
                        .is_some_and(|suffix| suffix.bytes().all(|byte| byte.is_ascii_digit()))
            })
            .count();
        if earlier != 0 {
            let mut alias = StackStr::new();
            let _ = write!(alias, "{name}_{earlier}");
            plan.nodes[at].alias = alias;
        }
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

pub(super) fn plan_modification<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    // The subqueries are planned, and so numbered, before the conditions
    // that print them.
    let initplans = write_initplans(storage, txid, statement, block, arena)?;
    let reads = initplans.iter().any(|initplan| {
        initplan
            .nodes
            .iter()
            .any(|node| !node.relation.as_str().is_empty())
    });
    let prior = SUBPLAN_RELATIONS.with(|planned| planned.replace(planned.get() || reads));
    let plan = plan_write(storage, txid, statement, block, arena);
    SUBPLAN_RELATIONS.with(|planned| planned.set(prior));
    attach_subplans(plan?, 1, block, initplans, arena)
}

/// The InitPlans of a write: each uncorrelated scalar subquery of an
/// `UPDATE`'s new values or an `INSERT`'s one `VALUES` row, in column order
/// (`SS_process_sublinks` over the processed target list), of several
/// `VALUES` rows in their order, then of the `WHERE`. Their outputs are
/// parameters the write's node computes once. An `INSERT ... SELECT`'s
/// source plans its own; any other subquery there is outside this plan.
fn write_initplans<'a, 'e>(
    storage: &Storage,
    txid: u32,
    statement: &'e Stmt<'e>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<&'a [Sublink<'a>], SqlError> {
    let (target, assignments, condition, returning, joined): (_, &[(&str, &'e Expr<'e>)], _, _, _) =
        match statement {
            Stmt::Update(update) => (
                update.table,
                update.assignments,
                update.where_clause,
                update.returning,
                update.from.is_some(),
            ),
            Stmt::Delete(delete) => (
                delete.table,
                &[],
                delete.where_clause,
                delete.returning,
                delete.using.is_some(),
            ),
            Stmt::Insert(insert) => (insert.table, &[], None, insert.returning, false),
            _ => return Ok(&[]),
        };
    if let Stmt::Insert(insert) = statement
        && let Some(on_conflict) = &insert.on_conflict
    {
        let mut nested = false;
        for expression in on_conflict
            .update
            .unwrap_or(&[])
            .iter()
            .map(|(_, value)| *value)
            .chain(on_conflict.update_where)
            .chain(on_conflict.target_where)
        {
            visit_sublinks(expression, &mut |_| nested = true);
        }
        if nested {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN of a subquery in INSERT ... ON CONFLICT is not supported"
            ));
        }
    }
    let mut found = crate::mem::arena::ArenaList::new(arena);
    let mut collect = |expression: &'e Expr<'e>| -> Result<(), SqlError> {
        let mut failed = false;
        visit_sublinks(expression, &mut |node| {
            failed |= found.push(node).is_err();
        });
        if failed { Err(arena_full()) } else { Ok(()) }
    };
    let definition = relation_slot(storage, txid, target.schema, target.name)
        .map(|slot| storage.table_def(slot, txid));
    match definition.as_deref() {
        Some(definition) => {
            for column in definition.columns() {
                for (name, value) in assignments {
                    if *name == column.name.as_str() {
                        collect(value)?;
                    }
                }
            }
        }
        None => {
            for (_, value) in assignments {
                collect(value)?;
            }
        }
    }
    if let Stmt::Insert(insert) = statement {
        match (insert.rows, definition.as_deref()) {
            ([row], Some(definition)) => {
                for (index, column) in definition.columns().iter().enumerate() {
                    let at = if insert.columns.is_empty() {
                        Some(index)
                    } else {
                        insert
                            .columns
                            .iter()
                            .position(|named| *named == column.name.as_str())
                    };
                    if let Some(value) = at.and_then(|at| row.get(at)) {
                        collect(value)?;
                    }
                }
            }
            (rows, _) => {
                for value in rows.iter().flat_map(|row| row.iter()) {
                    collect(value)?;
                }
            }
        }
    }
    if let Some(condition) = condition {
        collect(condition)?;
    }
    let mut returned = false;
    for item in returning.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            visit_sublinks(expression, &mut |_| returned = true);
        }
    }
    if returned || (joined && !found.as_slice().is_empty()) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of a joined write's subquery or of a subquery in RETURNING is not supported"
        ));
    }
    let mut planned = crate::mem::arena::ArenaList::new(arena);
    for &node in found.as_slice() {
        let Expr::Subquery(select) = node else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN of a write reading a set subquery (EXISTS, IN, ANY, ALL, ARRAY or a row) is not supported"
            ));
        };
        if query::subquery_node_correlated(node, storage, txid, arena)? {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN of a write reading a correlated subquery is not supported"
            ));
        }
        SUBLINK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        let inner = plan_select(
            storage,
            txid,
            select,
            None,
            block.child(Segment::Sublink(select.instrument_identity())),
            arena,
        );
        SUBLINK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        let Plan { nodes, count, .. } = inner?;
        let number = match register_subplan(select, false, Compared::No)? {
            crate::sql::catalog::deparse::SublinkPlan::InitPlan(number)
            | crate::sql::catalog::deparse::SublinkPlan::SubPlan(number)
            | crate::sql::catalog::deparse::SublinkPlan::HashedSubPlan(number) => number,
        };
        planned
            .push(Sublink {
                kind: SubplanKind::InitPlan,
                number,
                origin: SublinkOrigin::Where,
                nodes: &nodes[..count],
                cte: None,
                exists: false,
            })
            .map_err(|_| arena_full())?;
    }
    Ok(planned.into_mut_slice().map_err(|_| arena_full())?)
}

/// The resolution of `on_conflict` on the table in `slot`: the unique
/// indexes `infer_arbiter_indexes` picks, in OID order.
fn conflict_resolution(
    storage: &Storage,
    txid: u32,
    slot: usize,
    on_conflict: &crate::sql::ast::OnConflict<'_>,
) -> Result<Conflict, SqlError> {
    let unsupported = |what: &str| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of INSERT ... ON CONFLICT with {} is not supported",
            what
        )
    };
    if on_conflict.target_where.is_some() {
        return Err(unsupported("a WHERE on its conflict target"));
    }
    if on_conflict.update_where.is_some() {
        return Err(unsupported("a WHERE on DO UPDATE"));
    }
    if on_conflict
        .target
        .iter()
        .any(|target| target.column.is_none())
    {
        return Err(unsupported("an expression in its conflict target"));
    }
    let definition = storage.table_def(slot, txid);
    let mut wanted = 0u64;
    for target in on_conflict.target {
        let column = target
            .column
            .and_then(|name| definition.column_index(name))
            .filter(|&column| column < 64)
            .ok_or_else(|| unsupported("a conflict target column past the 64th"))?;
        wanted |= 1 << column;
    }
    drop(definition);
    let mut found = [(0i32, StackStr::<64>::new()); MAX_ARBITERS];
    let mut count = 0;
    let mut overflow = false;
    crate::sql::catalog::planner_indexes(storage, txid, slot, &mut |index| {
        let keys = &index.columns[..index.n_cols];
        let mut columns = 0u64;
        for &column in keys {
            if column < 64 {
                columns |= 1 << column;
            }
        }
        let arbitrates = match on_conflict.constraint {
            Some(name) => index.name.as_str() == name,
            None => {
                !on_conflict.target.is_empty()
                    && index.unique
                    && index.immediate
                    && !index.partial
                    && !index.expression_keys[..index.n_cols].iter().any(|&key| key)
                    && keys.len() == on_conflict.target.len()
                    && columns == wanted
            }
        };
        if arbitrates {
            match found.get_mut(count) {
                Some(entry) => *entry = (index.oid, index.name),
                None => overflow = true,
            }
            count += 1;
        }
    });
    if overflow {
        return Err(unsupported("more than 8 arbiter indexes"));
    }
    let found = &mut found[..count];
    found.sort_unstable_by_key(|&(oid, _)| oid as u32);
    let mut arbiters = [StackStr::new(); MAX_ARBITERS];
    for (arbiter, (_, name)) in arbiters.iter_mut().zip(found.iter()) {
        *arbiter = *name;
    }
    Ok(Conflict {
        update: on_conflict.update.is_some(),
        arbiters,
        count,
    })
}

/// [`plan_modification`] of the write itself, its subqueries planned.
fn plan_write<'a>(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    block: BlockId,
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
        // A partitioned or inheritance target lists each member and scans
        // it under an `Append`.
        3 + (from.joins.len() + 2) * 3
            + match relation_slot(storage, txid, target.schema, target.name) {
                Some(slot) if query::partition_prune::expands(storage, slot, txid) => {
                    2 * crate::sql::exec::relation_leaves_in_order(storage, slot, txid, arena)?
                        .len()
                }
                _ => 0,
            }
    } else if let Some(select) = source {
        1 + select_plan_capacity(select)
    } else {
        // A partitioned or inheritance target lists each member and scans
        // it under an `Append`; a `MERGE` joins it with its source's scan; a
        // bitmap heap scan's bitmap may hold a tree of bitmap scans.
        3 + 64
            + 4 * usize::from(matches!(statement, Stmt::Merge(_)))
            + match relation_slot(storage, txid, target.schema, target.name) {
                Some(slot) if query::partition_prune::expands(storage, slot, txid) => {
                    2 * crate::sql::exec::relation_leaves_in_order(storage, slot, txid, arena)?
                        .len()
                }
                _ => 0,
            }
    };
    let mut plan = Plan::new(arena, capacity)?;
    if let Stmt::Insert(insert) = statement
        && let Some(on_conflict) = &insert.on_conflict
        && let Some(slot) = relation_slot(storage, txid, target.schema, target.name)
    {
        let conflict = conflict_resolution(storage, txid, slot, on_conflict)?;
        plan.conflict = Some(&*arena.alloc(conflict).map_err(|_| arena_full())?);
    }
    let mut name = StackStr::new();
    let _ = write!(name, "{verb}");
    let target_node = PlanNode {
        name,
        bitmap_index: StackStr::new(),
        relation: StackStr::from_str(target.name),
        alias: StackStr::from_str(alias.filter(|alias| *alias != target.name).unwrap_or("")),
        output: StackStr::new(),
        depth: 0,
        startup_cost: 0.0,
        total_cost: 0.01,
        rows: 0,
        width: 0,
        object_requests: 0,
        cache_blocks: 0,
        constant_false: false,
        quals: NO_QUALS,
        keys: PlanKeys::NONE,
        key: Some(NodeKey::new(block, Role::Modify)),
        hash_columns: None,
        verbose_quals: NO_QUALS,
        schema: relation_slot(storage, txid, target.schema, target.name)
            .map(|slot| StackStr::from_str(storage.table_def(slot, txid).schema.as_str()))
            .unwrap_or_default(),
        subplan: None,
        shift: 0,
        relations: StackStr::new(),
        plain_relations: StackStr::new(),
        output_unprintable: false,
        unmodeled_costs: false,
        remote_sql: StackStr::new(),
        batch_size: None,
        passthrough: false,
        subplans_removed: None,
        target_member: false,
        scan_pruned: false,
        uncounted: false,
        inner_unique: false,
        bitmap_index_estimate: None,
        analyze_unsupported: None,
        planned_partitions: None,
        hash_agg: None,
        initplan_charge: 0.0,
    };
    plan.push(target_node)?;
    if let Some(slot) = relation_slot(storage, txid, target.schema, target.name)
        && storage.table_def(slot, txid).kind == crate::storage::TableKind::Foreign
        && joined_from.is_none()
    {
        if let Stmt::Insert(insert) = statement {
            let (sql, batch_size) =
                crate::sql::exec::foreign_insert_explain(storage, txid, slot, insert, arena)?;
            plan.nodes[0].remote_sql = remote_sql_text(sql.as_str())?;
            plan.nodes[0].batch_size = Some(batch_size);
        } else if foreign_modification(
            &mut plan,
            storage,
            txid,
            statement,
            slot,
            target_node,
            arena,
        )? {
            plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            return Ok(plan);
        }
    }
    if let Stmt::Merge(merge) = statement {
        let mut plan = plan_merge(plan, storage, txid, merge, block, arena)?;
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    if let Some(from) = joined_from {
        let target_slot = relation_slot(storage, txid, target.schema, target.name)
            .expect("target existence checked above");
        // A partitioned or inheritance target joins the `Append` of the
        // members its condition keeps; with none, the join is PostgreSQL's
        // dummy relation and reads a `Result` returning no rows.
        let inheritance = write_inheritance(statement);
        let target_definition = storage.table_def(target_slot, txid);
        let target_expansion = if query::partition_prune::expands(storage, target_slot, txid)
            && (inheritance == crate::sql::ast::RelationInheritance::Descendants
                || target_definition.partition.is_partitioned())
        {
            let expansion = if inheritance == crate::sql::ast::RelationInheritance::Only {
                None
            } else {
                target_members(
                    storage,
                    txid,
                    target_slot,
                    alias.unwrap_or(target.name),
                    predicate,
                    true,
                    arena,
                )?
            };
            if expansion.is_none() {
                plan.push(PlanNode {
                    name: StackStr::from_str("Result"),
                    depth: 1,
                    rows: 0,
                    total_cost: 0.0,
                    constant_false: true,
                    ..PlanNode::EMPTY
                })?;
                plan.nodes[0].set_output(None);
                plan.planning_micros =
                    started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                return Ok(plan);
            }
            expansion
        } else {
            None
        };
        drop(target_definition);
        let leaves = target_expansion.map_or(core::slice::from_ref(&target_slot), |expansion| {
            expansion.planned.leaves
        });
        if let Some(join) = query::joined_write_join(
            storage,
            target,
            alias,
            from,
            predicate,
            leaves,
            &[],
            None,
            txid,
            arena,
        )? {
            let mut plan = plan_joined_write(
                plan,
                storage,
                txid,
                statement,
                target_slot,
                target_expansion,
                join,
                block,
                arena,
            )?;
            plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            return Ok(plan);
        }
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
        // The target joins first, then each FROM item in execution order.
        let relations_count = scope.n + 1;
        let top_depth = 1usize;
        let scan_depth = |position: usize| (top_depth + relations_count - position.max(1)) as u8;
        let mut target_scan = physical_scan_node(
            storage,
            target_slot,
            target.name,
            false,
            txid,
            scan_depth(0),
            None,
        );
        target_scan.alias = target_node.alias;
        let mut total_cost = target_scan.total_cost;
        let mut rows = target_scan.rows.max(1);
        let mut object_requests = target_scan.object_requests;
        let mut cache_blocks = target_scan.cache_blocks;
        let source_nodes = arena
            .alloc_slice_with(scope.n, |_| PlanNode::EMPTY)
            .map_err(|_| arena_full())?;
        // Per relation, the target first: the index its scan uses.
        let scan_indexes = arena
            .alloc_slice_with(scope.n + 1, |_| None)
            .map_err(|_| arena_full())?;
        for (position, &table) in order[..scope.n].iter().enumerate() {
            let (node, index) = scan_node(
                storage,
                &scope,
                table,
                predicate,
                txid,
                scan_depth(position + 1),
                arena,
                None,
                parameterized[table],
                false,
                None,
            )?;
            scan_indexes[table + 1] = index;
            total_cost += node.total_cost;
            rows = rows.saturating_mul(node.rows.max(1));
            object_requests = object_requests.saturating_add(node.object_requests);
            cache_blocks = cache_blocks.saturating_add(node.cache_blocks);
            source_nodes[table] = node;
        }
        // The target hash-joins the first FROM item it reads; later items
        // join that result in nested loops.
        let hashed = order[0] == 0
            && query::target_hash_plan(storage, target, alias, from, predicate, txid, arena)?
                .is_some();
        let target_definition = storage.table_def(target_slot, txid);
        let target_exposed = alias.unwrap_or(target.name);
        // `VERBOSE` lists cover a join of one FROM table, read sequentially.
        let lists = match scope.defs.stored(0) {
            Some(source)
                if scope.n == 1
                    && from.joins.is_empty()
                    && from.base.subquery.is_none()
                    && from.base.cte.is_none()
                    && !scope.lateral[0]
                    && scope.slots[0] != usize::MAX
                    && !storage.relation_has_descendants(scope.slots[0], txid)
                    && source_nodes[0].name.as_str() == "Seq Scan" =>
            {
                let (assignments, returning): (&[(&str, &Expr<'_>)], _) = match statement {
                    Stmt::Update(update) => (update.assignments, update.returning),
                    Stmt::Delete(delete) => (&[], delete.returning),
                    _ => unreachable!("a joined write is an UPDATE or DELETE"),
                };
                let definition = (*target_definition).clone();
                targets::RelationTargets::joined_write(
                    storage,
                    txid,
                    &definition,
                    target_exposed,
                    source,
                    scope.names[0],
                    target_expansion.is_some(),
                    assignments,
                    returning,
                    predicate,
                    arena,
                )?
            }
            _ => None,
        };
        let filters = arena
            .alloc_slice_with(relations_count, |_| None::<StackStr<512>>)
            .map_err(|_| arena_full())?;
        let verbose_filters = arena
            .alloc_slice_with(relations_count, |_| None::<StackStr<512>>)
            .map_err(|_| arena_full())?;
        let join_filters = arena
            .alloc_slice_with(relations_count, |_| None::<StackStr<512>>)
            .map_err(|_| arena_full())?;
        let mut hash_cond = None;
        let inner_joins = from.joins.iter().all(|join| {
            join.kind.is_cross()
                || (join.kind == crate::sql::ast::JoinKind::Inner
                    && join.using.is_none()
                    && !join.natural)
        }) && !scope.lateral[..scope.n].iter().any(|&lateral| lateral);
        if inner_joins && (0..scope.n).all(|table| scope.defs.stored(table).is_some()) {
            let relations = arena
                .alloc_slice_with(relations_count, |_| {
                    JoinRelation::table(target_exposed, &target_definition)
                })
                .map_err(|_| arena_full())?;
            // A FROM table's conditions print the column aliases it is read
            // under.
            let items =
                core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
            for (table, (relation, item)) in relations[1..].iter_mut().zip(items).enumerate() {
                *relation = JoinRelation::aliased(
                    scope.names[table],
                    scope.defs.stored(table).expect("stored relation"),
                    item.col_alias,
                    arena,
                )?;
            }
            let display = arena
                .alloc_slice_with(relations_count, |position| {
                    if position == 0 {
                        0
                    } else {
                        order[position - 1] + 1
                    }
                })
                .map_err(|_| arena_full())?;
            let mut condition = predicate;
            for join in from.joins {
                condition = query::and_where(condition, join.on, arena)?;
            }
            let joined_parameterized = arena
                .alloc_slice_with(relations_count, |relation| {
                    relation != 0
                        && parameterized[relation - 1].is_some()
                        && scan_indexes[relation].is_some()
                })
                .map_err(|_| arena_full())?;
            if let Some(quals) = join_quals(
                storage,
                txid,
                relations,
                display,
                condition,
                hashed,
                scan_indexes,
                joined_parameterized,
                arena,
            )? {
                filters.copy_from_slice(quals.filters);
                verbose_filters.copy_from_slice(quals.verbose_filters);
                join_filters.copy_from_slice(quals.join_filters);
                hash_cond = quals.hash_cond;
            }
            let joined = arena
                .alloc_slice_with(relations_count, |index| relations[index].joined(true))
                .map_err(|_| arena_full())?;
            let context = crate::sql::catalog::deparse::Context {
                storage,
                txid,
                table: None,
                parameters: &[],
                joined,
                pretty: false,
                qualify: true,
                bare: None,
                domain_value: None,
                sublinks: Some(&registered_sublink),
                references: None,
                folded: true,
                arena,
                dependencies: None,
            };
            for table in 0..scope.n {
                if let Some(index) = scan_indexes[table + 1].as_ref() {
                    source_nodes[table] = source_nodes[table].with_qual(
                        QualLabel::IndexCond,
                        index_condition(
                            &context,
                            relations[table + 1].exposed,
                            index,
                            false,
                            arena,
                        )?,
                    );
                }
            }
        }
        let join_kind = |position: usize| (position >= 2).then(|| from.joins[position - 2].kind);
        let hash_at = |position: usize| hashed && position == 1;
        let join = PlanNode {
            name: StackStr::from_str(join_node(
                join_kind(relations_count - 1),
                hash_at(relations_count - 1),
            )),
            depth: top_depth as u8,
            total_cost: total_cost + rows as f64 * 0.01,
            rows,
            width: target_scan.width,
            object_requests,
            cache_blocks,
            ..PlanNode::EMPTY
        }
        .with_qual(
            QualLabel::HashCond,
            if relations_count == 2 {
                hash_cond
            } else {
                None
            },
        )
        .with_qual(
            QualLabel::JoinFilter,
            join_filters[relations_count - 1].take(),
        );
        let mut join = join;
        join.uncounted = true;
        if let Some(lists) = lists.as_ref() {
            join.set_output(Some(lists.join));
        }
        // The result relations, named as the members' scans are.
        let fresh_names = || -> Result<RangeTableNames<'_>, SqlError> {
            let count = target_expansion.map_or(0, |expansion| expansion.planned.leaves.len());
            let names = arena
                .alloc_slice_with(count + 1, |_| StackStr::<96>::new())
                .map_err(|_| arena_full())?;
            names[0] = StackStr::from_str(target_exposed);
            Ok(RangeTableNames { names, count: 1 })
        };
        if let Some(expansion) = target_expansion {
            let mut targets = fresh_names()?;
            for &leaf in expansion.started {
                let name = targets.take(target_exposed)?;
                let member = storage.table_def(leaf, txid);
                plan.push(PlanNode {
                    name: StackStr::from_str(verb),
                    relation: StackStr::from_str(member.name.as_str()),
                    schema: StackStr::from_str(member.schema.as_str()),
                    alias: if name.as_str() == member.name.as_str() {
                        StackStr::new()
                    } else {
                        name
                    },
                    depth: 1,
                    target_member: true,
                    ..PlanNode::EMPTY
                })?;
            }
        }
        let join_index = plan.count;
        plan.push(join)?;
        for position in (1..relations_count - 1).rev() {
            plan.push(
                PlanNode {
                    name: StackStr::from_str(join_node(join_kind(position), hash_at(position))),
                    depth: (top_depth + relations_count - 1 - position) as u8,
                    ..PlanNode::EMPTY
                }
                .with_qual(
                    QualLabel::HashCond,
                    if position == 1 { hash_cond } else { None },
                )
                .with_qual(QualLabel::JoinFilter, join_filters[position].take()),
            )?;
        }
        let mut target_scan = target_scan
            .with_qual(QualLabel::Filter, filters[0].take())
            .with_verbose_qual(QualLabel::Filter, verbose_filters[0].take());
        if let Some(lists) = lists.as_ref() {
            target_scan.set_output(Some(lists.target));
        }
        if let Some(expansion) = target_expansion {
            push_members(
                &mut plan,
                storage,
                txid,
                target_scan,
                expansion,
                true,
                &mut fresh_names()?,
                &|_| None,
                None,
                None,
            )?;
            // One member scanned in place of the `Append` names the
            // target's columns in the join's conditions.
            if expansion.planned.leaves.len() == 1 {
                let member = plan.nodes[plan.count - 1];
                for node in &mut plan.nodes[join_index..plan.count - 1] {
                    node.requalify(target_exposed, member.exposed_name());
                }
            }
        } else {
            push_scan(&mut plan, target_scan)?;
        }
        for (position, &table) in order[..scope.n].iter().enumerate() {
            let mut node = source_nodes[table]
                .with_qual(QualLabel::Filter, filters[table + 1].take())
                .with_verbose_qual(QualLabel::Filter, verbose_filters[table + 1].take());
            if let Some(lists) = lists.as_ref() {
                node.set_output(Some(lists.source));
            }
            let sequential =
                node.name.as_str() == "Seq Scan" && crate::sql::guc::planner_methods().material;
            let hash = hash_at(position + 1);
            // A hash join hashes its inner relation; a nested loop rescans its
            // inner sequential scan from a materialized copy.
            if sequential || hash {
                plan.push(PlanNode {
                    name: StackStr::from_str(if hash { "Hash" } else { "Materialize" }),
                    depth: node.depth,
                    total_cost: node.total_cost,
                    rows: node.rows,
                    width: node.width,
                    object_requests: node.object_requests,
                    cache_blocks: node.cache_blocks,
                    output: node.output,
                    ..PlanNode::EMPTY
                })?;
                node.depth += 1;
            }
            push_scan(&mut plan, node)?;
        }
        plan.nodes[0].total_cost = join.total_cost;
        plan.nodes[0].object_requests = join.object_requests;
        plan.nodes[0].cache_blocks = join.cache_blocks;
        // A FROM item's own query runs under its Subquery Scan.
        let items = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
        for (table, item) in items.enumerate().take(scope.n) {
            let Some(body) = item.subquery.filter(|body| values_list(body).is_none()) else {
                continue;
            };
            plan = with_derived_plan(
                plan,
                scope.names[table],
                false,
                plan_derived(
                    storage,
                    txid,
                    body,
                    Some(&query::ScopeTypes(&scope)),
                    block.child(Segment::Derived(
                        u16::try_from(table).map_err(|_| arena_full())?,
                    )),
                    arena,
                )?,
                arena,
            )?;
        }
        match lists {
            Some(lists) => {
                plan.nodes[0].set_output(Some(lists.returned.unwrap_or_default()));
                // The `RETURNING` list is set against the first result
                // relation, as a one-table write's is.
                if target_expansion.is_some() {
                    let first = unique_rtable_name(
                        target_exposed,
                        &[StackStr::<96>::from_str(target_exposed)],
                    );
                    plan.nodes[0].requalify(target_exposed, first.as_str());
                }
            }
            // Other joined writes' lists are outside this plan.
            None => plan.nodes[0].set_output(None),
        }
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    let child = if let (Some(select), Stmt::Insert(insert)) = (source, statement) {
        // Its `RETURNING` list over a range table of the target and the
        // source is not modelled.
        if !insert.returning.is_empty() {
            plan.nodes[0].set_output(None);
        }
        // The source query's range table entries join the INSERT's.
        let (source_plan, pulled) = in_nested_block(|| {
            rewrite::insert_source_plan(storage, txid, insert, select, block, arena)
        })?;
        let root = source_plan.nodes[0];
        let source_nodes = &source_plan.nodes[..source_plan.count];
        // A pulled-up query's InitPlans belong to the `INSERT`, whose node
        // runs them and pays for them in place of the source's top node.
        let moved = |at: usize| {
            pulled
                && source_nodes[at].depth > root.depth
                && source_nodes[..=at]
                    .iter()
                    .rev()
                    .find(|node| node.depth == root.depth + 1)
                    .is_some_and(|top| {
                        top.subplan
                            .is_some_and(|(kind, _)| kind == SubplanKind::InitPlan)
                    })
        };
        let charge = (1..source_nodes.len())
            .filter(|&at| moved(at) && source_nodes[at].depth == root.depth + 1)
            .map(|at| source_nodes[at].initplan_charge)
            .sum::<f64>();
        for at in (1..source_nodes.len()).filter(|&at| moved(at)) {
            plan.push(source_nodes[at])?;
        }
        for (at, source_node) in source_nodes.iter().enumerate() {
            if at > 0 && moved(at) {
                continue;
            }
            let mut source_node = *source_node;
            if at == 0 {
                source_node.startup_cost -= charge;
                source_node.total_cost -= charge;
            }
            source_node.depth = source_node.depth.saturating_add(1);
            // The INSERT's range table holds the target and its source, so
            // PostgreSQL qualifies the columns of the source's keys.
            source_node.keys.plain = source_node.keys.verbose;
            // PostgreSQL names each range table entry uniquely: the source's
            // reference to the target's own name gets a numeric suffix.
            if source_node.alias.as_str().is_empty()
                && source_node.relation.as_str() == target.name
                && alias.is_none_or(|alias| alias == target.name)
            {
                let _ = write!(source_node.alias, "{}_1", target.name);
            }
            plan.push(source_node)?;
        }
        root
    } else if let (Stmt::Insert(insert), None) = (
        statement,
        relation_slot(storage, txid, target.schema, target.name),
    ) {
        // An INSERT into a view its INSTEAD OF trigger handles reads its rows,
        // whose list over the view's columns is not modelled.
        let several = insert.rows.len() > 1;
        let mut child = PlanNode {
            name: StackStr::from_str(if several { "Values Scan" } else { "Result" }),
            alias: StackStr::from_str(if several { "*VALUES*" } else { "" }),
            key: Some(NodeKey::new(block, Role::Values)),
            depth: 1,
            rows: insert.rows.len() as u64,
            width: 32,
            total_cost: 0.01,
            unmodeled_costs: true,
            ..PlanNode::EMPTY
        };
        child.set_output(None);
        plan.push(child)?;
        child
    } else if let Some(slot) = relation_slot(storage, txid, target.schema, target.name)
        && !joined
        && matches!(statement, Stmt::Update(_) | Stmt::Delete(_))
        && query::partition_prune::expands(storage, slot, txid)
        // `ONLY` of an inheritance parent writes the parent's own rows as a
        // plain table's write does.
        && (write_inheritance(statement) == crate::sql::ast::RelationInheritance::Descendants
            || storage.table_def(slot, txid).partition.is_partitioned())
    {
        expanded_write(
            &mut plan, storage, txid, statement, slot, verb, filtered, predicate, alias, block,
            arena,
        )?
    } else if let Some(slot) = relation_slot(storage, txid, target.schema, target.name) {
        let child = if let Stmt::Insert(insert) = statement {
            // Several rows are scanned from their VALUES list.
            let several = insert.rows.len() > 1;
            let definition = storage.table_def(slot, txid);
            let lists = targets::RelationTargets::returning_scope(
                storage,
                txid,
                &definition,
                insert.alias.unwrap_or(target.name),
                insert.returning,
                true,
                arena,
            )?;
            plan.nodes[0].set_output(lists.returning(insert.returning)?);
            let (width, total_cost, unmodeled_costs) =
                inserted_rows_estimate(storage, txid, &definition, insert, arena);
            let mut values = PlanNode {
                name: StackStr::from_str(if several { "Values Scan" } else { "Result" }),
                alias: StackStr::from_str(if several { "*VALUES*" } else { "" }),
                key: Some(NodeKey::new(block, Role::Values)),
                depth: 1,
                rows: insert.rows.len() as u64,
                width,
                total_cost,
                unmodeled_costs,
                ..PlanNode::EMPTY
            };
            values.set_output(if several {
                lists.values_scan(insert.columns, insert.rows)?
            } else {
                lists.inserted_row(insert.columns, insert.rows[0])?
            });
            values
        } else if !joined && predicate.is_some_and(|predicate| qual_false(predicate, arena)) {
            // A condition the planner folds to false leaves no row to
            // change: the write reads a `Result` that returns none.
            PlanNode {
                name: StackStr::from_str("Result"),
                depth: 1,
                rows: 0,
                width: storage
                    .table_statistics(slot, txid)
                    .average_row_width
                    .max(1),
                total_cost: 0.0,
                constant_false: true,
                ..PlanNode::EMPTY
            }
        } else if let Some((path, scope)) = (!joined
            && !storage.relation_has_descendants(slot, txid))
        .then(|| {
            chosen_write_scan(
                storage,
                txid,
                target,
                alias,
                predicate,
                write_assignments(statement),
                arena,
            )
        })
        .transpose()?
        .flatten()
        {
            let (mut scan, used) = chosen_scan_node(&scope, 1, &path);
            // A write counts the rows it changes, not its bitmap's work.
            if path.kind == query::index_paths::ScanKind::Bitmap {
                scan.analyze_unsupported = Some(
                    "EXPLAIN ANALYZE of an UPDATE or DELETE read by a bitmap heap scan is not supported",
                );
            }
            scan.alias = target_node.alias;
            scan.key = Some(NodeKey::new(block, Role::Scan(0)));
            let definition = storage.table_def(slot, txid);
            let security =
                write_security_quals(storage, statement, slot, &definition, txid, arena)?;
            let exposed = alias.unwrap_or(target.name);
            let quals = |plan: Option<&query::IndexAccessPlan<'_>>, verbose: bool| {
                single_relation_quals(
                    storage,
                    txid,
                    exposed,
                    &definition,
                    &[],
                    predicate,
                    security,
                    plan,
                    verbose,
                    arena,
                )
            };
            let (index_cond, filter) = quals(used.as_ref(), false)?;
            let (verbose_index_cond, verbose_filter) = quals(used.as_ref(), true)?;
            let (returned, read) =
                modification_outputs(storage, txid, statement, &definition, slot, alias, arena)?;
            plan.nodes[0].set_output(returned);
            scan.set_output(read);
            let mut scan = scan
                .with_qual(QualLabel::IndexCond, index_cond)
                .with_qual(QualLabel::Filter, filter)
                .with_verbose_qual(QualLabel::IndexCond, verbose_index_cond)
                .with_verbose_qual(QualLabel::Filter, verbose_filter);
            if path.kind == query::index_paths::ScanKind::Bitmap
                && let Some(used) = used
            {
                let written = used.written();
                scan = scan
                    .with_qual(QualLabel::RecheckCond, quals(Some(&written), false)?.0)
                    .with_verbose_qual(QualLabel::RecheckCond, quals(Some(&written), true)?.0);
                if path.bitmap.len() > 1 {
                    let leaves = arena
                        .alloc_slice_with(path.bitmap.len(), |_| (None, None))
                        .map_err(|_| arena_full())?;
                    for (at, node) in path.bitmap.iter().enumerate() {
                        if let query::index_paths::BitmapPlanKind::Index { conditions, .. } =
                            node.kind
                        {
                            let leaf = query::IndexAccessPlan::chosen_conditions(
                                query::ChosenConditions {
                                    conditions,
                                    recheck: &[],
                                    absorbed: &[],
                                },
                            );
                            leaves[at] =
                                (quals(Some(&leaf), false)?.0, quals(Some(&leaf), true)?.0);
                        }
                    }
                    let depth = scan.depth;
                    push_scan(&mut plan, scan)?;
                    push_bitmap_tree(&mut plan, path.bitmap, leaves, depth, block)?;
                    plan.nodes[0].startup_cost = scan.startup_cost;
                    plan.nodes[0].total_cost = scan.total_cost;
                    plan.planning_micros =
                        started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                    return Ok(plan);
                }
            }
            scan
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
                plan.chosen_for_scan(storage, slot, &storage.table_def(slot, txid), txid, false)
            });
            let index_name = index.as_ref().map(|(plan, name)| {
                (
                    name.as_str(),
                    plan.expected_rows(storage, slot, &storage.table_def(slot, txid), txid),
                    plan.method(),
                )
            });
            let mut scan =
                physical_scan_node(storage, slot, target.name, filtered, txid, 1, index_name);
            scan.alias = target_node.alias;
            scan.key = Some(NodeKey::new(block, Role::Scan(0)));
            let definition = storage.table_def(slot, txid);
            let security =
                write_security_quals(storage, statement, slot, &definition, txid, arena)?;
            let (index_cond, filter) = single_relation_quals(
                storage,
                txid,
                alias.unwrap_or(target.name),
                &definition,
                &[],
                predicate,
                security,
                index.as_ref().map(|(plan, _)| plan),
                false,
                arena,
            )?;
            let (verbose_index_cond, verbose_filter) = single_relation_quals(
                storage,
                txid,
                alias.unwrap_or(target.name),
                &definition,
                &[],
                predicate,
                security,
                index.as_ref().map(|(plan, _)| plan),
                true,
                arena,
            )?;
            let (returned, read) =
                modification_outputs(storage, txid, statement, &definition, slot, alias, arena)?;
            plan.nodes[0].set_output(returned);
            scan.set_output(read);
            scan.with_qual(QualLabel::IndexCond, index_cond)
                .with_qual(QualLabel::Filter, filter)
                .with_verbose_qual(QualLabel::IndexCond, verbose_index_cond)
                .with_verbose_qual(QualLabel::Filter, verbose_filter)
        };
        push_scan(&mut plan, child)?;
        child
    } else {
        let child = PlanNode {
            name: StackStr::from_str("Subquery Scan"),
            alias: StackStr::from_str(target.name),
            depth: 1,
            rows: 1_000,
            width: 32,
            total_cost: 10.0,
            unmodeled_costs: true,
            ..PlanNode::EMPTY
        };
        plan.push(child)?;
        child
    };
    // `create_modifytable_path`: the write costs what its input does and,
    // with a `RETURNING` list, returns its rows at its width.
    plan.nodes[0].startup_cost = child.startup_cost;
    plan.nodes[0].total_cost = child.total_cost;
    if !write_returning(statement).is_empty() {
        plan.nodes[0].rows = child.rows;
        plan.nodes[0].width = child.width;
    }
    plan.nodes[0].object_requests = child.object_requests;
    plan.nodes[0].cache_blocks = child.cache_blocks;
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

/// A write's `RETURNING` items.
fn write_returning<'e>(statement: &Stmt<'e>) -> &'e [SelectItem<'e>] {
    match statement {
        Stmt::Insert(insert) => insert.returning.items,
        Stmt::Update(update) => update.returning.items,
        Stmt::Delete(delete) => delete.returning.items,
        Stmt::Merge(merge) => merge.returning.items,
        _ => &[],
    }
}

/// The width and total cost of the `Result` or `Values Scan` an `INSERT`
/// of listed rows reads, and whether they are outside the model. Its
/// target list holds every attribute of the table: each live column's
/// value coerced to the column's type, whose `get_typavgwidth` it adds,
/// and a NULL `integer` for each dropped one (`expand_insert_targetlist`).
/// A `Result` costs `cpu_tuple_cost` and its values' evaluation
/// (`cost_qual_eval`); a `Values Scan` costs `cpu_operator_cost` and
/// `cpu_tuple_cost` per row (`cost_valuesscan`). A value that is not a
/// constant of its column's type, or an omitted column with a computed
/// default, is evaluated or coerced at a cost this does not model.
fn inserted_rows_estimate<'e>(
    storage: &Storage,
    txid: u32,
    definition: &crate::storage::TableDef,
    insert: &crate::sql::ast::Insert<'e>,
    arena: &Arena,
) -> (u32, f64, bool) {
    let mut width = 4.0 * definition.attributes.dropped() as f64;
    for column in definition.columns() {
        width += query::index_paths::type_average_width(column.ctype, column.type_mod);
    }
    let constant = |value: &Expr<'_>| {
        matches!(
            value,
            Expr::Null
                | Expr::DefaultMarker
                | Expr::BoundLiteral { .. }
                | Expr::Bool(_)
                | Expr::Int(_)
                | Expr::Float(_)
                | Expr::NumericLit(_)
                | Expr::Str(_)
        )
    };
    let mut evaluation = 0.0;
    let mut unmodeled = false;
    for (index, column) in definition.columns().iter().enumerate() {
        let written = if insert.columns.is_empty() {
            Some(index)
        } else {
            insert
                .columns
                .iter()
                .position(|named| *named == column.name.as_str())
        };
        // A value omitted (a `DEFAULT VALUES` row lists none) or written
        // `DEFAULT` is the column's default.
        let value_of = |row: &[&'e Expr<'e>]| {
            written
                .and_then(|at| row.get(at).copied())
                .filter(|value| !matches!(value, Expr::DefaultMarker))
        };
        // A default is folded to a constant of the column's type unless it
        // computes its value.
        if insert.rows.iter().any(|row| value_of(row).is_none()) {
            match &column.default {
                crate::storage::ColumnDefault::None
                | crate::storage::ColumnDefault::Constant { .. } => {}
                crate::storage::ColumnDefault::Expression(text)
                    if !column.is_identity && !column.auto_increment =>
                {
                    match arena
                        .alloc_str(text.as_str())
                        .ok()
                        .and_then(|text| crate::sql::parser::parse_expr(text, arena).ok())
                    {
                        Some(default) if constant(default) => {}
                        Some(Expr::Cast { operand, .. }) if constant(operand) => {}
                        _ => unmodeled = true,
                    }
                }
                _ => unmodeled = true,
            }
        }
        for value in insert.rows.iter().filter_map(|row| value_of(row)) {
            if constant(value) {
                continue;
            }
            // A computed value of the column's own type costs its
            // evaluation; a coercion to the column's type is not modeled.
            let typed =
                crate::sql::exec::infer_routine_argument_oid(value, &crate::sql::exec::NoCols)
                    .ok()
                    .and_then(crate::sql::types::ColType::from_oid);
            if typed != Some(column.ctype) {
                unmodeled = true;
            }
            evaluation += query::qual_cost(value, &crate::sql::exec::NoCols, storage, txid, arena)
                / 2.0
                * CPU_OPERATOR_COST;
        }
    }
    let rows = insert.rows.len() as f64;
    let total = if insert.rows.len() > 1 {
        (CPU_OPERATOR_COST + CPU_TUPLE_COST) * rows
    } else {
        CPU_TUPLE_COST + evaluation
    };
    (width as u32, total, unmodeled)
}

/// Plans an `UPDATE ... FROM` or `DELETE ... USING` joined as execution
/// joins it (`query::joined_write_join`): the target's scan (each member's
/// under an `Append`) and the FROM table's, each filtered by its own
/// conditions, under a hash join building on the relation PostgreSQL's
/// costs pick, or a nested loop rescanning it from a `Materialize`. `plan`
/// holds the write's node.
#[allow(clippy::too_many_arguments)]
fn plan_joined_write<'a, 'e>(
    mut plan: Plan<'a>,
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'e>,
    target_slot: usize,
    expansion: Option<Expansion<'_>>,
    join: query::JoinedWriteJoin<'e>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let (verb, target, alias, assignments, returning, predicate): (
        &str,
        _,
        _,
        &[(&str, &Expr<'_>)],
        _,
        _,
    ) = match statement {
        Stmt::Update(update) => (
            "Update",
            update.table,
            update.alias,
            update.assignments,
            update.returning,
            update.where_clause,
        ),
        Stmt::Delete(delete) => (
            "Delete",
            delete.table,
            delete.alias,
            &[],
            delete.returning,
            delete.where_clause,
        ),
        _ => unreachable!("a joined write is an UPDATE or DELETE"),
    };
    let exposed = alias.unwrap_or(target.name);
    let target_table = (*storage.table_def(target_slot, txid)).clone();
    let source_table = (*storage.table_def(join.source_slot, txid)).clone();
    let source_name = join.source_name;
    // The FROM table's conditions print the column aliases it is read under.
    let source_aliases = match statement {
        Stmt::Update(update) => update.from.and_then(|from| from.base.col_alias),
        Stmt::Delete(delete) => delete.using.and_then(|using| using.base.col_alias),
        _ => None,
    };
    let aliased_source = crate::sql::catalog::deparse::AliasedTable {
        table: &source_table,
        aliases: source_aliases.unwrap_or(&[]),
    };
    let lists = targets::RelationTargets::joined_write(
        storage,
        txid,
        &target_table,
        exposed,
        &aliased_source,
        source_name,
        expansion.is_some(),
        assignments,
        returning,
        predicate,
        arena,
    )?;
    let joined = arena
        .alloc_slice_with(2, |at| {
            if at == 0 {
                crate::sql::catalog::deparse::Joined::new(exposed, &target_table)
            } else {
                crate::sql::catalog::deparse::Joined::new(source_name, &aliased_source)
            }
        })
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        arena,
        dependencies: None,
    };
    let conditions = join.conditions;
    // The hash condition reads the outer relation's key first.
    let mut hash_conds = crate::mem::arena::ArenaList::new(arena);
    if let Some(hash) = conditions.hash {
        let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
        let mut count = 0;
        if split_conjuncts(hash, &mut conjuncts, &mut count, arena).is_none() {
            return Err(arena_full());
        }
        for &conjunct in &conjuncts[..count] {
            let Expr::Binary {
                operator,
                left,
                right,
                source_position,
            } = conjunct
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "a hash join key is not an equality"
                ));
            };
            let reads_target = |side: &Expr<'_>| {
                let mut target_side = false;
                side.for_each_column_reference(&mut |qualifier, name| {
                    target_side |= match qualifier {
                        Some(qualifier) => qualifier == exposed,
                        None => target_table.column_index(name).is_some(),
                    };
                });
                target_side
            };
            let (outer_side, inner_side) = if reads_target(left) != join.source_outer {
                (*left, *right)
            } else {
                (*right, *left)
            };
            hash_conds
                .push(
                    &*arena
                        .alloc(Expr::Binary {
                            operator: *operator,
                            left: outer_side,
                            right: inner_side,
                            source_position: *source_position,
                        })
                        .map_err(|_| arena_full())?,
                )
                .map_err(|_| arena_full())?;
        }
    }
    let hash_cond = deparse_qual(&context, hash_conds.as_slice(), arena)?;
    let mut join_filters = crate::mem::arena::ArenaList::new(arena);
    if let Some(filter) = conditions.join {
        let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
        let mut count = 0;
        if split_conjuncts(filter, &mut conjuncts, &mut count, arena).is_none() {
            return Err(arena_full());
        }
        for &conjunct in &conjuncts[..count] {
            join_filters.push(conjunct).map_err(|_| arena_full())?;
        }
    }
    let join_filter = deparse_qual(&context, join_filters.as_slice(), arena)?;
    let rescanned = join.method == query::JoinedWriteMethod::Rescanned;
    let inner_depth = if rescanned { 2 } else { 3 };
    let (target_depth, source_depth) = if join.source_outer {
        (inner_depth, 2)
    } else {
        (2, inner_depth)
    };
    let mut target_scan = physical_scan_node(
        storage,
        target_slot,
        target.name,
        conditions.target.is_some(),
        txid,
        target_depth,
        None,
    );
    target_scan.alias = plan.nodes[0].alias;
    let target_filter = |verbose: bool| {
        single_relation_quals(
            storage,
            txid,
            exposed,
            &target_table,
            &[],
            conditions.target,
            &[],
            None,
            verbose,
            arena,
        )
        .map(|(_, filter)| filter)
    };
    let mut target_scan = target_scan
        .with_qual(QualLabel::Filter, target_filter(false)?)
        .with_verbose_qual(QualLabel::Filter, target_filter(true)?);
    target_scan.key = Some(NodeKey::new(block, Role::Scan(0)));
    target_scan.set_output(lists.as_ref().map(|lists| lists.target));
    let mut source_scan = physical_scan_node(
        storage,
        join.source_slot,
        source_table.name.as_str(),
        conditions.source.is_some(),
        txid,
        source_depth,
        None,
    );
    if source_name != source_table.name.as_str() {
        source_scan.alias = StackStr::from_str(source_name);
    }
    let source_filter = |verbose: bool| {
        single_relation_quals(
            storage,
            txid,
            source_name,
            &source_table,
            source_aliases.unwrap_or(&[]),
            conditions.source,
            &[],
            None,
            verbose,
            arena,
        )
        .map(|(_, filter)| filter)
    };
    let mut source_scan = source_scan
        .with_qual(QualLabel::Filter, source_filter(false)?)
        .with_verbose_qual(QualLabel::Filter, source_filter(true)?);
    source_scan.key = Some(NodeKey::new(block, Role::Scan(1)));
    source_scan.set_output(lists.as_ref().map(|lists| lists.source));
    let fresh_names = || -> Result<RangeTableNames<'_>, SqlError> {
        let count = expansion.map_or(0, |expansion| expansion.planned.leaves.len());
        let names = arena
            .alloc_slice_with(count + 1, |_| StackStr::<96>::new())
            .map_err(|_| arena_full())?;
        names[0] = StackStr::from_str(exposed);
        Ok(RangeTableNames { names, count: 1 })
    };
    if let Some(expansion) = expansion {
        let mut targets = fresh_names()?;
        for &leaf in expansion.started {
            let name = targets.take(exposed)?;
            let member = storage.table_def(leaf, txid);
            plan.push(PlanNode {
                name: StackStr::from_str(verb),
                relation: StackStr::from_str(member.name.as_str()),
                schema: StackStr::from_str(member.schema.as_str()),
                alias: if name.as_str() == member.name.as_str() {
                    StackStr::new()
                } else {
                    name
                },
                depth: 1,
                target_member: true,
                ..PlanNode::EMPTY
            })?;
        }
    }
    let join_index = plan.count;
    plan.push(
        PlanNode {
            name: StackStr::from_str(if join.hashed() {
                "Hash Join"
            } else {
                "Nested Loop"
            }),
            depth: 1,
            total_cost: target_scan.total_cost + source_scan.total_cost,
            rows: target_scan.rows.max(source_scan.rows),
            width: target_scan.width,
            key: Some(NodeKey::new(block, Role::Join(1))),
            inner_unique: join.inner_unique,
            output: lists.as_ref().map(|lists| lists.join).unwrap_or_default(),
            output_unprintable: lists.is_none(),
            ..PlanNode::EMPTY
        }
        .with_qual(QualLabel::HashCond, hash_cond)
        .with_qual(QualLabel::JoinFilter, join_filter),
    )?;
    let push_target = |plan: &mut Plan<'a>, scan: PlanNode| -> Result<(), SqlError> {
        match expansion {
            Some(expansion) => {
                push_members(
                    plan,
                    storage,
                    txid,
                    scan,
                    expansion,
                    true,
                    &mut fresh_names()?,
                    &|leaf| {
                        Some(NodeKey::new(
                            block,
                            Role::Member(0, u32::try_from(leaf).unwrap_or(u32::MAX)),
                        ))
                    },
                    None,
                    None,
                )?;
                // One member scanned in place of the `Append` names the
                // target's columns in the join's conditions.
                if expansion.planned.leaves.len() == 1 {
                    let member = plan.nodes[plan.count - 1];
                    plan.nodes[join_index].requalify(exposed, member.exposed_name());
                }
                Ok(())
            }
            None => push_scan(plan, scan),
        }
    };
    let hashed_columns = lists.as_ref().map(|lists| lists.hashed);
    let inner = |scan: &PlanNode, relation: usize| PlanNode {
        name: StackStr::from_str(if join.hashed() { "Hash" } else { "Materialize" }),
        key: Some(NodeKey::new(
            block,
            if join.hashed() {
                Role::Hash(relation as u16)
            } else {
                Role::Materialize(relation as u16)
            },
        )),
        depth: 2,
        total_cost: scan.total_cost,
        rows: scan.rows,
        width: scan.width,
        output: scan.output,
        output_unprintable: scan.output_unprintable,
        hash_columns: hashed_columns.and_then(|hashed| hashed[relation]),
        // A hashed or stored row's size needs the values it holds.
        uncounted: hashed_columns.and_then(|hashed| hashed[relation]).is_none(),
        ..PlanNode::EMPTY
    };
    if join.source_outer {
        push_scan(&mut plan, source_scan)?;
        if !rescanned {
            plan.push(inner(&target_scan, 0))?;
        }
        push_target(&mut plan, target_scan)?;
    } else {
        push_target(&mut plan, target_scan)?;
        if !rescanned {
            plan.push(inner(&source_scan, 1))?;
        }
        push_scan(&mut plan, source_scan)?;
    }
    plan.nodes[0].total_cost = plan.nodes[join_index].total_cost;
    match lists {
        Some(lists) => {
            plan.nodes[0].set_output(Some(lists.returned.unwrap_or_default()));
            // The `RETURNING` list is set against the first result relation.
            if expansion.is_some() {
                let first = unique_rtable_name(exposed, &[StackStr::<96>::from_str(exposed)]);
                plan.nodes[0].requalify(exposed, first.as_str());
            }
        }
        None => plan.nodes[0].set_output(None),
    }
    Ok(plan)
}

/// Plans a `MERGE` as execution joins its target and source
/// (`query::merge_join_plan`): a hash join building on the smaller
/// relation, or a nested loop rescanning it, whose `ON` conditions reading
/// one side filter that side's scan unless the join keeps that side's
/// unmatched rows. `plan` holds the write's node.
fn plan_merge<'a, 'e>(
    mut plan: Plan<'a>,
    storage: &Storage,
    txid: u32,
    merge: &'e crate::sql::ast::Merge<'e>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let unsupported = |what: &str| {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of MERGE {} is not supported",
            what
        )
    };
    let target = merge.target;
    let Some(target_slot) = relation_slot(storage, txid, target.schema, target.name) else {
        return Err(unsupported("into a view"));
    };
    let role = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    let source = merge.source;
    // Whether the `ON` condition reads column `name` of the target
    // (`relation` 0) or the source (1), or reads either's whole row.
    let target_name = merge.target_alias.unwrap_or(target.name);
    let source_name = source.alias.unwrap_or(source.table);
    let reads_in = |condition: &Expr<'_>,
                    relation: usize,
                    name: &str,
                    columns: &[crate::storage::ColumnMeta]| {
        let named = |column: &str| columns.iter().any(|meta| meta.name.as_str() == column);
        let mut reads = false;
        condition.for_each_column_reference(&mut |qualifier, column| {
            reads |= column == name
                && match qualifier {
                    Some(qualifier) => {
                        qualifier
                            == if relation == 0 {
                                target_name
                            } else {
                                source_name
                            }
                    }
                    None => named(column),
                };
        });
        let mut whole = false;
        visit_whole_rows(condition, &mut |table| {
            whole |= table
                == if relation == 0 {
                    target_name
                } else {
                    source_name
                };
        });
        reads || whole
    };
    let on_reads = |relation: usize, name: &str, columns: &[crate::storage::ColumnMeta]| {
        reads_in(merge.on, relation, name, columns)
    };
    // What the indexes of the relation in `slot` (`relation` 0 the target,
    // 1 the source) offer the join over the `key` columns PostgreSQL may
    // hash: an index on another column `ON` reads, or a partial or
    // expression index, may be scanned, joined or merged by as its costs
    // choose; an index on hash keys only serves a nested loop rescanning
    // that relation. A unique one on those keys makes it unique for the
    // join.
    let index_reach = |slot: usize, relation: usize, key: &dyn Fn(&str) -> bool| {
        let definition = storage.table_def(slot, txid);
        let columns = definition.columns();
        let name = |column: u16| {
            columns
                .get(usize::from(column))
                .map(|meta| meta.name.as_str())
        };
        let mut reach = IndexReach {
            // A column's own `PRIMARY KEY` or `UNIQUE` constraint.
            unique: columns
                .iter()
                .any(|meta| (meta.unique || meta.primary) && key(meta.name.as_str())),
            ..IndexReach::default()
        };
        for index in storage.indexes_for_slot(slot, txid) {
            let plain = index.predicate.is_none()
                && index.expressions[..index.n_cols]
                    .iter()
                    .all(Option::is_none);
            if !plain {
                reach.other = true;
                continue;
            }
            let keys = &index.columns[..index.n_cols];
            reach.unique |=
                index.unique && keys.iter().all(|&column| name(column).is_some_and(key));
        }
        for binding in 0..storage.value_binding_count(slot) {
            let (keys, count) = storage.value_binding_columns(slot, binding);
            for &column in &keys[..count] {
                match name(column) {
                    Some(name) if !on_reads(relation, name, columns) => {}
                    Some(name) if key(name) => reach.keys = true,
                    _ => reach.other = true,
                }
            }
        }
        reach
    };
    let values = source.subquery.and_then(values_list);
    let source_slot = match storage.resolve_relation(source.schema, source.table, txid) {
        Some(crate::storage::ResolvedRelation::Table(slot))
            if source.subquery.is_none()
                && source.cte.is_none()
                && source.func_args.is_none()
                && source.rows_from.is_none()
                && source.join_members.is_none() =>
        {
            Some(slot)
        }
        _ => None,
    };
    if source_slot.is_none() && values.is_none() {
        return Err(unsupported(
            "from a source other than a table or VALUES list",
        ));
    }
    if let Some(slot) = source_slot
        && (storage.relation_has_descendants(slot, txid)
            || storage.table_def(slot, txid).kind != crate::storage::TableKind::Local
            || storage.row_security_applies(slot, role, txid))
    {
        return Err(unsupported("from a table with row security or descendants"));
    }
    if storage.row_security_applies(target_slot, role, txid) {
        return Err(unsupported("into a table with row security"));
    }
    let join = query::merge_join_plan(storage, merge, txid, arena)?;
    let inheritance = merge.target_inheritance;
    let target_definition = (*storage.table_def(target_slot, txid)).clone();
    let exposed = merge.target_alias.unwrap_or(target.name);
    let tree = query::partition_prune::expands(storage, target_slot, txid)
        && (inheritance == crate::sql::ast::RelationInheritance::Descendants
            || target_definition.partition.is_partitioned());
    let expansion = if tree {
        if inheritance == crate::sql::ast::RelationInheritance::Only {
            return Err(unsupported("into ONLY a partitioned table"));
        }
        match target_members(
            storage,
            txid,
            target_slot,
            exposed,
            (!join.preserves_target).then_some(merge.on),
            true,
            arena,
        )? {
            Some(expansion) => Some(expansion),
            None => return Err(unsupported("whose target pruning leaves no member")),
        }
    } else {
        None
    };
    let leaves = match expansion {
        Some(expansion) => expansion.planned.leaves,
        None => core::slice::from_ref(&target_slot),
    };
    if leaves
        .iter()
        .any(|&leaf| storage.table_def(leaf, txid).kind != crate::storage::TableKind::Local)
    {
        return Err(unsupported("into a table with a foreign member"));
    }
    // A column `ON` reads only in the hash join's key equalities.
    let key_only = |relation: usize, name: &str, columns: &[crate::storage::ColumnMeta]| {
        let conditions = join.conditions;
        [conditions.target, conditions.source, conditions.join]
            .into_iter()
            .flatten()
            .all(|condition| !reads_in(condition, relation, name, columns))
    };
    let target_key = |name: &str| {
        join.keys()
            .iter()
            .any(|&(column, _)| target_definition.columns()[column].name.as_str() == name)
            && key_only(0, name, target_definition.columns())
    };
    let source_columns = source_slot.map(|slot| (*storage.table_def(slot, txid)).clone());
    let source_key = |name: &str| {
        join.keys().iter().any(|&(_, column)| column == name)
            && source_columns
                .as_ref()
                .is_some_and(|definition| key_only(1, name, definition.columns()))
    };
    let target_reach = match expansion {
        Some(_) => leaves.iter().fold(IndexReach::default(), |reach, &leaf| {
            reach.with(index_reach(leaf, 0, &target_key))
        }),
        None => index_reach(target_slot, 0, &target_key),
    };
    let source_reach = source_slot.map_or(IndexReach::default(), |slot| {
        index_reach(slot, 1, &source_key)
    });
    // PostgreSQL merges relations it knows no statistics for.
    let target_rows = leaves.iter().try_fold(0u64, |rows, &leaf| {
        storage
            .table_statistics(leaf, txid)
            .valid
            .then(|| rows.saturating_add(storage.planning_row_estimate(leaf)))
    });
    let values_count = values
        .and_then(values_rows)
        .map_or(0, |(rows, _)| rows as usize);
    let source_rows =
        crate::sql::exec::merge_source_planned_rows(storage, merge, values_count, txid);
    let (Some(target_rows), Some(source_rows)) = (target_rows, source_rows) else {
        return Err(unsupported("of relations without statistics"));
    };
    let source_outer = join.source_outer(target_rows, source_rows);
    // An index on the hashed relation's keys alone leaves the hash join,
    // whose outer relation PostgreSQL would rescan by it otherwise.
    let (outer_reach, inner_reach) = if source_outer {
        (source_reach, target_reach)
    } else {
        (target_reach, source_reach)
    };
    if outer_reach.keys
        || outer_reach.other
        || inner_reach.other
        || (inner_reach.keys && !join.hashed())
    {
        return Err(unsupported(
            "with an index on a column its join condition reads",
        ));
    }
    // The relations' names and columns, as the conditions print them.
    let source_from = &*arena
        .alloc(crate::sql::ast::FromClause {
            base: source,
            joins: &[],
        })
        .map_err(|_| arena_full())?;
    let (scope, _) = query::dml_join_scope(
        storage,
        target,
        merge.target_alias,
        Some(source_from),
        txid,
        arena,
        None,
    )?;
    let source_name = scope.names[1];
    let source_definition = source_slot.map(|slot| (*storage.table_def(slot, txid)).clone());
    let source_joined = match source_definition.as_ref() {
        Some(definition) => crate::sql::catalog::deparse::Joined::new(source_name, definition),
        None => {
            let definition = scope.defs.get(1).expect("resolved source");
            let columns = arena
                .alloc_slice_with(definition.n_columns, |index| {
                    crate::sql::catalog::deparse::Column {
                        name: definition.columns[index].name.as_str(),
                        type_oid: None,
                        merged: None,
                    }
                })
                .map_err(|_| arena_full())?;
            for (index, column) in columns.iter_mut().enumerate() {
                let printed = arena
                    .alloc_str_display(format_args!("column{}", index + 1))
                    .map_err(|_| arena_full())?;
                column.merged = Some((Some("*VALUES*"), printed));
            }
            crate::sql::catalog::deparse::Joined {
                unqualified: true,
                printed: crate::sql::catalog::deparse::printed_name(source_name),
                ..crate::sql::catalog::deparse::Joined::rule_columns(
                    source_name,
                    crate::sql::catalog::deparse::Columns::Derived(columns),
                )
            }
        }
    };
    let target_table = &target_definition;
    let joined = arena
        .alloc_slice_with(2, |at| {
            if at == 0 {
                crate::sql::catalog::deparse::Joined::new(exposed, target_table)
            } else {
                source_joined
            }
        })
        .map_err(|_| arena_full())?;
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        table: None,
        parameters: &[],
        joined,
        pretty: false,
        qualify: true,
        bare: None,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        references: None,
        folded: true,
        arena,
        dependencies: None,
    };
    // Each `ON` conjunct by the relations it reads: a hash key, a filter of
    // one side's scan, or the join's filter.
    let resolve = |qualifier: Option<&str>, name: &str| -> Option<usize> {
        let reads = |relation: &crate::sql::catalog::deparse::Joined<'_>| {
            qualifier.is_none_or(|qualifier| qualifier == relation.name)
                && relation.columns.position(name).is_some()
        };
        if reads(&joined[0]) {
            Some(0)
        } else if reads(&joined[1]) {
            Some(1)
        } else {
            None
        }
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(merge.on, &mut conjuncts, &mut count, arena).is_none() {
        return Err(unsupported("with this join condition"));
    }
    let column_of = |side: &Expr<'e>| -> Option<(usize, &'e str)> {
        match side {
            Expr::Column { qualifier, name } => {
                resolve(*qualifier, name).map(|relation| (relation, *name))
            }
            _ => None,
        }
    };
    let mut hash_conds = crate::mem::arena::ArenaList::new(arena);
    let mut join_filters = crate::mem::arena::ArenaList::new(arena);
    // The conjuncts the join evaluates, in `ON` order.
    let mut join_conjuncts = crate::mem::arena::ArenaList::new(arena);
    let mut target_filters = crate::mem::arena::ArenaList::new(arena);
    let mut source_filters = crate::mem::arena::ArenaList::new(arena);
    for &conjunct in &conjuncts[..count] {
        if conjunct.contains_subquery() {
            return Err(unsupported("with a subquery in its join condition"));
        }
        let key = match conjunct {
            Expr::Binary {
                operator: crate::sql::ast::BinaryOp::Eq,
                left,
                right,
                source_position,
            } => match (column_of(left), column_of(right)) {
                (Some((0, target_column)), Some((1, source_column)))
                | (Some((1, source_column)), Some((0, target_column)))
                    if join.keys().iter().any(|&(column, name)| {
                        target_table.columns()[column].name.as_str() == target_column
                            && name == source_column
                    }) =>
                {
                    let (target_side, source_side) =
                        if column_of(left).is_some_and(|(relation, _)| relation == 0) {
                            (*left, *right)
                        } else {
                            (*right, *left)
                        };
                    let (outer_side, inner_side) = if source_outer {
                        (source_side, target_side)
                    } else {
                        (target_side, source_side)
                    };
                    Some(
                        &*arena
                            .alloc(Expr::Binary {
                                operator: crate::sql::ast::BinaryOp::Eq,
                                left: outer_side,
                                right: inner_side,
                                source_position: *source_position,
                            })
                            .map_err(|_| arena_full())?,
                    )
                }
                _ => None,
            },
            _ => None,
        };
        if let Some(key) = key {
            hash_conds.push(key).map_err(|_| arena_full())?;
            join_conjuncts.push(conjunct).map_err(|_| arena_full())?;
            continue;
        }
        let list = match relation_mask(conjunct, &resolve) {
            Some(1) if !join.preserves_target => &mut target_filters,
            Some(2) if !join.preserves_source => &mut source_filters,
            Some(1..=3) => {
                join_conjuncts.push(conjunct).map_err(|_| arena_full())?;
                &mut join_filters
            }
            _ => return Err(unsupported("with this join condition")),
        };
        list.push(conjunct).map_err(|_| arena_full())?;
    }
    if values.is_some() && !source_filters.as_slice().is_empty() {
        return Err(unsupported("filtering its VALUES list"));
    }
    let hash_cond = deparse_qual(&context, hash_conds.as_slice(), arena)?;
    let join_filter = deparse_qual(&context, join_filters.as_slice(), arena)?;
    let and_all = |list: &[&'e Expr<'e>]| -> Result<Option<&Expr<'_>>, SqlError> {
        let mut condition = None;
        for &conjunct in list {
            condition = query::and_where(condition, Some(conjunct), arena)?;
        }
        Ok(condition)
    };
    let target_condition = and_all(target_filters.as_slice())?;
    let source_condition = and_all(source_filters.as_slice())?;
    // The scans, the outer one at depth 2 and the inner one below a `Hash`
    // or `Materialize`.
    let (outer_preserved, inner_preserved) = if source_outer {
        (join.preserves_source, join.preserves_target)
    } else {
        (join.preserves_target, join.preserves_source)
    };
    let join_name = match (join.hashed(), outer_preserved, inner_preserved) {
        (true, false, false) => "Hash Join",
        (true, true, false) => "Hash Left Join",
        (true, false, true) => "Hash Right Join",
        (true, true, true) => "Hash Full Join",
        (false, false, false) => "Nested Loop",
        (false, true, false) => "Nested Loop Left Join",
        (false, _, true) => return Err(unsupported("joining without an equality")),
    };
    if !join.hashed() && (values.is_some() || !crate::sql::guc::planner_methods().material) {
        return Err(unsupported("joining without an equality"));
    }
    let lists = targets::RelationTargets::merge_write(
        storage,
        txid,
        target_table,
        exposed,
        joined,
        values.is_some(),
        expansion.is_some(),
        merge,
        join.preserves_target,
        join_conjuncts.as_slice(),
        arena,
    )?;
    let (target_depth, source_depth) = if source_outer { (3, 2) } else { (2, 3) };
    let mut target_scan = physical_scan_node(
        storage,
        target_slot,
        target.name,
        target_condition.is_some(),
        txid,
        target_depth,
        None,
    );
    target_scan.alias = plan.nodes[0].alias;
    let filter = |verbose: bool| {
        single_relation_quals(
            storage,
            txid,
            exposed,
            target_table,
            &[],
            target_condition,
            &[],
            None,
            verbose,
            arena,
        )
        .map(|(_, filter)| filter)
    };
    let mut target_scan = target_scan
        .with_qual(QualLabel::Filter, filter(false)?)
        .with_verbose_qual(QualLabel::Filter, filter(true)?);
    target_scan.key = Some(NodeKey::new(block, Role::Scan(0)));
    target_scan.set_output(lists.as_ref().map(|lists| lists.scans[0]));
    let source_scan = match source_slot {
        Some(slot) => {
            let definition = &storage.table_def(slot, txid);
            let mut scan = physical_scan_node(
                storage,
                slot,
                source.table,
                source_condition.is_some(),
                txid,
                source_depth,
                None,
            );
            if source_name != source.table {
                scan.alias = StackStr::from_str(source_name);
            }
            let filter = |verbose: bool| {
                single_relation_quals(
                    storage,
                    txid,
                    source_name,
                    definition,
                    &[],
                    source_condition,
                    &[],
                    None,
                    verbose,
                    arena,
                )
                .map(|(_, filter)| filter)
            };
            scan.with_qual(QualLabel::Filter, filter(false)?)
                .with_verbose_qual(QualLabel::Filter, filter(true)?)
        }
        None => PlanNode {
            name: StackStr::from_str("Values Scan"),
            alias: StackStr::from_str("*VALUES*"),
            depth: source_depth,
            rows: source_rows,
            total_cost: 0.01,
            ..PlanNode::EMPTY
        },
    };
    let mut source_scan = source_scan;
    source_scan.set_output(lists.as_ref().map(|lists| lists.scans[1]));
    source_scan.key = Some(NodeKey::new(block, Role::Scan(1)));
    let fresh_names = || -> Result<RangeTableNames<'_>, SqlError> {
        let names = arena
            .alloc_slice_with(leaves.len() + 1, |_| StackStr::<96>::new())
            .map_err(|_| arena_full())?;
        names[0] = StackStr::from_str(exposed);
        Ok(RangeTableNames { names, count: 1 })
    };
    if let Some(expansion) = expansion {
        let mut targets = fresh_names()?;
        for &leaf in expansion.started {
            let name = targets.take(exposed)?;
            let member = storage.table_def(leaf, txid);
            plan.push(PlanNode {
                name: StackStr::from_str("Merge"),
                relation: StackStr::from_str(member.name.as_str()),
                schema: StackStr::from_str(member.schema.as_str()),
                alias: if name.as_str() == member.name.as_str() {
                    StackStr::new()
                } else {
                    name
                },
                depth: 1,
                target_member: true,
                ..PlanNode::EMPTY
            })?;
        }
    }
    let join_index = plan.count;
    plan.push(
        PlanNode {
            name: StackStr::from_str(join_name),
            depth: 1,
            total_cost: target_scan.total_cost + source_scan.total_cost,
            rows: target_scan.rows.max(source_scan.rows),
            width: target_scan.width,
            key: Some(NodeKey::new(block, Role::Join(1))),
            inner_unique: inner_reach.unique,
            output: lists.as_ref().map(|lists| lists.join).unwrap_or_default(),
            output_unprintable: lists.is_none(),
            ..PlanNode::EMPTY
        }
        .with_qual(QualLabel::HashCond, hash_cond)
        .with_qual(QualLabel::JoinFilter, join_filter),
    )?;
    let push_target = |plan: &mut Plan<'a>, scan: PlanNode| -> Result<(), SqlError> {
        match expansion {
            Some(expansion) => {
                push_members(
                    plan,
                    storage,
                    txid,
                    scan,
                    expansion,
                    true,
                    &mut fresh_names()?,
                    &|leaf| {
                        Some(NodeKey::new(
                            block,
                            Role::Member(0, u32::try_from(leaf).unwrap_or(u32::MAX)),
                        ))
                    },
                    None,
                    None,
                )?;
                // One member scanned in place of the `Append` names the
                // target's columns in the join's conditions.
                if expansion.planned.leaves.len() == 1 {
                    let member = plan.nodes[plan.count - 1];
                    plan.nodes[join_index].requalify(exposed, member.exposed_name());
                }
                Ok(())
            }
            None => push_scan(plan, scan),
        }
    };
    let inner = |scan: &PlanNode, relation: usize| PlanNode {
        name: StackStr::from_str(if join.hashed() { "Hash" } else { "Materialize" }),
        key: Some(NodeKey::new(
            block,
            if join.hashed() {
                Role::Hash(relation as u16)
            } else {
                Role::Materialize(relation as u16)
            },
        )),
        depth: 2,
        total_cost: scan.total_cost,
        rows: scan.rows,
        width: scan.width,
        output: scan.output,
        output_unprintable: scan.output_unprintable,
        hash_columns: lists.as_ref().map(|lists| lists.hashed[relation]),
        ..PlanNode::EMPTY
    };
    if source_outer {
        push_scan(&mut plan, source_scan)?;
        plan.push(inner(&target_scan, 0))?;
        push_target(&mut plan, target_scan)?;
    } else {
        push_target(&mut plan, target_scan)?;
        plan.push(inner(&source_scan, 1))?;
        push_scan(&mut plan, source_scan)?;
    }
    plan.nodes[0].total_cost = plan.nodes[join_index].total_cost;
    match lists {
        Some(lists) => {
            plan.nodes[0].set_output(Some(lists.returned.unwrap_or_default()));
            // The `RETURNING` list is set against the first result relation.
            if expansion.is_some() {
                let first = unique_rtable_name(exposed, &[StackStr::<96>::from_str(exposed)]);
                plan.nodes[0].requalify(exposed, first.as_str());
            }
        }
        None => plan.nodes[0].set_output(None),
    }
    Ok(plan)
}

/// What a relation's indexes offer a `MERGE`'s join: one on a column
/// `ON` reads only as a hash key (`keys`), one on another column it reads
/// or a partial or expression index (`other`), and a unique one on hash
/// keys (`unique`).
#[derive(Clone, Copy, Default)]
struct IndexReach {
    keys: bool,
    other: bool,
    unique: bool,
}

impl IndexReach {
    /// The reach of a tree's members together, which PostgreSQL does not
    /// prove unique.
    fn with(self, member: Self) -> Self {
        Self {
            keys: self.keys || member.keys,
            other: self.other || member.other,
            unique: false,
        }
    }
}

/// Calls `visit` with the relation of each whole-row reference of
/// `expression`, its subqueries included.
fn visit_whole_rows(expression: &Expr<'_>, visit: &mut dyn FnMut(&str)) {
    if let Expr::WholeRow(table) = expression {
        visit(table);
    }
    let _ = query::walk_children(expression, &mut |child| {
        visit_whole_rows(child, visit);
        Ok(())
    });
}

/// Whether a write returns nothing.
fn returning_empty(statement: &Stmt<'_>) -> bool {
    match statement {
        Stmt::Update(update) => update.returning.items.is_empty(),
        Stmt::Delete(delete) => delete.returning.items.is_empty(),
        Stmt::Insert(insert) => insert.returning.items.is_empty(),
        _ => true,
    }
}

/// Whether an `UPDATE` or `DELETE` writes its target's descendants too.
fn write_inheritance(statement: &Stmt<'_>) -> crate::sql::ast::RelationInheritance {
    match statement {
        Stmt::Update(update) => update.inheritance,
        Stmt::Delete(delete) => delete.inheritance,
        _ => crate::sql::ast::RelationInheritance::Descendants,
    }
}

/// Plans an `UPDATE` or `DELETE` of a partitioned table or inheritance
/// tree without other relations: each member it reaches is a result
/// relation listed under the write, and its scans form the `Append` below
/// it. Returns the write's input node.
#[allow(clippy::too_many_arguments)]
fn expanded_write<'e>(
    plan: &mut Plan<'_>,
    storage: &Storage,
    txid: u32,
    statement: &'e Stmt<'e>,
    slot: usize,
    verb: &str,
    filtered: bool,
    predicate: Option<&'e Expr<'e>>,
    alias: Option<&str>,
    block: BlockId,
    arena: &'e Arena,
) -> Result<PlanNode, SqlError> {
    let inheritance = write_inheritance(statement);
    let definition = (*storage.table_def(slot, txid)).clone();
    let exposed = alias.unwrap_or(definition.name.as_str());
    let width = storage
        .table_statistics(slot, txid)
        .average_row_width
        .max(1);
    let empty = PlanNode {
        name: StackStr::from_str("Result"),
        depth: 1,
        rows: 0,
        width,
        total_cost: 0.0,
        constant_false: true,
        ..PlanNode::EMPTY
    };
    // `ONLY` of a partitioned table writes no rows.
    if inheritance == crate::sql::ast::RelationInheritance::Only {
        plan.push(empty)?;
        return Ok(empty);
    }
    if predicate.is_some_and(|predicate| qual_false(predicate, arena)) {
        plan.push(empty)?;
        return Ok(empty);
    }
    let Some(Expansion {
        planned, started, ..
    }) = target_members(storage, txid, slot, exposed, predicate, false, arena)?
    else {
        plan.push(empty)?;
        return Ok(empty);
    };
    let mut scan = physical_scan_node(
        storage,
        slot,
        definition.name.as_str(),
        filtered,
        txid,
        1,
        None,
    );
    scan.alias = plan.nodes[0].alias;
    scan.key = Some(NodeKey::new(block, Role::Scan(0)));
    let security = write_security_quals(storage, statement, slot, &definition, txid, arena)?;
    // The members' paths PostgreSQL's costs choose.
    let tree = {
        let (target, write_alias) = match statement {
            Stmt::Update(update) => (update.table, update.alias),
            Stmt::Delete(delete) => (delete.table, delete.alias),
            _ => unreachable!("an expanded write is an UPDATE or DELETE"),
        };
        let (scope, shape) = query::index_paths::write_shape(
            storage,
            txid,
            target,
            write_alias,
            write_assignments(statement),
            arena,
        )?;
        match query::index_paths::scan_path(
            storage,
            &scope,
            txid,
            predicate,
            None,
            &[],
            arena,
            shape,
        )? {
            query::index_paths::Planned::Tree(tree) => Some(tree),
            query::index_paths::Planned::Outside(shape) => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN of a scan of one table with {} is not supported",
                    shape
                ));
            }
            _ => None,
        }
    };
    if let Some(tree) = tree {
        scan.startup_cost = tree.startup_cost;
        scan.total_cost = tree.total_cost;
        scan.rows = tree.rows as u64;
        scan.width = tree.width;
    }
    let member_quals = match tree {
        Some(tree) => tree_member_quals(storage, txid, exposed, predicate, security, &tree, arena)?,
        None => &[],
    };
    let (_, filter) = single_relation_quals(
        storage,
        txid,
        exposed,
        &definition,
        &[],
        predicate,
        security,
        None,
        false,
        arena,
    )?;
    let (_, verbose_filter) = single_relation_quals(
        storage,
        txid,
        exposed,
        &definition,
        &[],
        predicate,
        security,
        None,
        true,
        arena,
    )?;
    // An `UPDATE` of an inheritance tree computes its new values in a
    // `Result` above the members' `Append`, which cannot project; a
    // partitioned table's members compute them (`apply_scanjoin_target_to_paths`).
    let projects = match tree {
        Some(tree) => tree.result_cost.is_some(),
        None => matches!(statement, Stmt::Update(_)) && !definition.partition.is_partitioned(),
    };
    let (returned, modified, read) = {
        let (assignments, returning): (&[(&str, &Expr<'_>)], _) = match statement {
            Stmt::Update(update) => (update.assignments, update.returning),
            Stmt::Delete(delete) => (&[], delete.returning),
            _ => unreachable!("an expanded write is an UPDATE or DELETE"),
        };
        let mark = arena.mark();
        let lists = (|| {
            let lists = targets::RelationTargets::returning_scope(
                storage,
                txid,
                &definition,
                exposed,
                returning,
                true,
                arena,
            )?;
            let returned = lists.returning(returning)?;
            let modified = lists.modified_row(assignments, alias, false, true)?;
            // A partitioned table's members compute the new values
            // themselves; an inheritance tree's return the columns read and
            // a `Result` above computes them.
            let read = if projects {
                lists.modified_row_reads(assignments)?
            } else {
                modified
            };
            Ok::<_, SqlError>((returned, modified, read))
        })();
        // SAFETY: the lists were copied out of the arena.
        unsafe { arena.rewind_to(mark) };
        lists?
    };
    plan.nodes[0].set_output(returned);
    scan.set_output(read);
    let projection = projects.then(|| {
        let mut result = PlanNode::EMPTY;
        result.set_output(modified);
        result
    });
    let scan = scan
        .with_qual(QualLabel::Filter, filter)
        .with_verbose_qual(QualLabel::Filter, verbose_filter);
    // The result relations, named as the members' scans are.
    let fresh_names = || -> Result<RangeTableNames<'_>, SqlError> {
        let names = arena
            .alloc_slice_with(started.len() + 1, |_| StackStr::<96>::new())
            .map_err(|_| arena_full())?;
        names[0] = StackStr::from_str(exposed);
        Ok(RangeTableNames { names, count: 1 })
    };
    let mut targets = fresh_names()?;
    // The write's `RETURNING` list is set against its first result
    // relation (`set_plan_refs` uses the first `returningLists` entry), so
    // its columns print under that member's name.
    match started.first() {
        Some(_) => {
            let first = unique_rtable_name(exposed, &targets.names[..targets.count]);
            plan.nodes[0].requalify(exposed, first.as_str());
        }
        None if !returning_empty(statement) => plan.nodes[0].output_unprintable = true,
        None => {}
    }
    for &leaf in started {
        let name = targets.take(exposed)?;
        let member = storage.table_def(leaf, txid);
        plan.push(PlanNode {
            name: StackStr::from_str(verb),
            relation: StackStr::from_str(member.name.as_str()),
            schema: StackStr::from_str(member.schema.as_str()),
            alias: if name.as_str() == member.name.as_str() {
                StackStr::new()
            } else {
                name
            },
            depth: 1,
            target_member: true,
            ..PlanNode::EMPTY
        })?;
    }
    push_members(
        plan,
        storage,
        txid,
        scan,
        Expansion {
            planned,
            started,
            foreign_shipped: false,
            foreign_local: &[],
        },
        true,
        &mut fresh_names()?,
        &|leaf| {
            Some(NodeKey::new(
                block,
                Role::Member(0, u32::try_from(leaf).unwrap_or(u32::MAX)),
            ))
        },
        projection.map(|result| (result.output, result.output_unprintable)),
        tree.map(|path| ChosenTree {
            path,
            quals: member_quals,
            keys: PlanKeys::NONE,
        })
        .as_ref(),
    )?;
    // The write reads the `Result` computing its new values, when there is
    // one.
    Ok(match tree.and_then(|tree| tree.result_cost) {
        Some((startup_cost, total_cost)) if projects => PlanNode {
            startup_cost,
            total_cost,
            ..scan
        },
        _ => scan,
    })
}

/// The members an `UPDATE` or `DELETE` of the partitioned table or
/// inheritance tree `slot`, exposed as `exposed`, reaches under its
/// condition: those planning keeps, and of them those executor startup
/// keeps. `None` when pruning leaves none. A `joined` write's FROM items'
/// columns prune nothing.
fn target_members<'e>(
    storage: &Storage,
    txid: u32,
    slot: usize,
    exposed: &str,
    predicate: Option<&'e Expr<'e>>,
    joined: bool,
    arena: &'e Arena,
) -> Result<Option<Expansion<'e>>, SqlError> {
    use query::partition_prune::{PruneStage, ScanRelation, ScanValues, conjuncts, members};
    let definition = storage.table_def(slot, txid);
    let columns = arena
        .alloc_slice_copy(definition.columns())
        .map_err(|_| arena_full())?;
    let values = ScanValues {
        storage,
        txid,
        relation: ScanRelation::Target {
            columns,
            name: exposed,
            joined,
        },
        arena,
        params: crate::sql::eval::generic_plan_parameters(),
        hooks: &crate::sql::eval::NO_HOOKS,
        outer: None,
    };
    let conditions = conjuncts(predicate, arena)?;
    let planned = members(
        storage,
        txid,
        slot,
        conditions,
        &values,
        PruneStage::Plan,
        arena,
    )?;
    // The planner chooses each member's path of a write that joins no
    // other relation.
    if joined {
        let own = |qualifier: Option<&str>, _: &str| qualifier.is_none_or(|q| q == exposed);
        reject_member_indexes(storage, txid, planned.leaves, &|name| {
            reads_column(conditions.iter().copied(), &own, name)
        })?;
    }
    if planned.leaves.is_empty() {
        return Ok(None);
    }
    let started = if planned.leaves.len() >= 2 {
        let kept = members(
            storage,
            txid,
            slot,
            conditions,
            &values,
            PruneStage::Startup,
            arena,
        )?;
        let started = arena
            .alloc_slice_with(planned.leaves.len(), |_| 0usize)
            .map_err(|_| arena_full())?;
        let mut count = 0;
        for &leaf in planned.leaves {
            if kept.leaves.contains(&leaf) {
                started[count] = leaf;
                count += 1;
            }
        }
        &started[..count]
    } else {
        planned.leaves
    };
    Ok(Some(Expansion {
        planned,
        started,
        foreign_shipped: false,
        foreign_local: &[],
    }))
}

/// The target lists `VERBOSE` prints for a one-table write: the
/// `RETURNING` list on the write's node, and on an `UPDATE` or `DELETE`
/// scan the new values of the assigned columns in column order
/// (`rewriteTargetListIU`) followed by the row identity `ctid`. `None` for
/// a list outside the deparser's coverage, as are `MERGE`'s and those of a
/// table with descendants, whose children PostgreSQL scans each.
fn modification_outputs(
    storage: &Storage,
    txid: u32,
    statement: &Stmt<'_>,
    definition: &crate::storage::TableDef,
    slot: usize,
    alias: Option<&str>,
    arena: &Arena,
) -> Result<(Option<targets::Output>, Option<targets::Output>), SqlError> {
    let (assignments, returning): (&[(&str, &Expr<'_>)], _) = match statement {
        Stmt::Update(update) => (update.assignments, update.returning),
        Stmt::Delete(delete) => (&[], delete.returning),
        _ => return Ok((None, None)),
    };
    if write_inheritance(statement) == crate::sql::ast::RelationInheritance::Descendants
        && storage.relation_has_descendants(slot, txid)
    {
        return Ok((None, None));
    }
    let exposed = alias.unwrap_or(definition.name.as_str());
    let mark = arena.mark();
    let result = (|| {
        // The target's columns print qualified when the statement's range
        // table holds other relations: a `WITH` query run as its own plan.
        let lists = targets::RelationTargets::returning_scope(
            storage,
            txid,
            definition,
            exposed,
            returning,
            SUBPLAN_RELATIONS.with(core::cell::Cell::get),
            arena,
        )?;
        Ok((
            lists.returning(returning)?,
            lists.modified_row(assignments, alias, false, false)?,
        ))
    })();
    // SAFETY: the lists were copied out of the arena.
    unsafe { arena.rewind_to(mark) };
    result
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

/// The column `node`'s property lines start at, two further for each
/// subplan it is nested in.
fn node_indent(node: &PlanNode) -> usize {
    property_indent(node.depth) + 2 * usize::from(node.shift)
}

fn text_line(node: &PlanNode, options: ExplainOptions, actual: NodeActual) -> StackStr<2048> {
    let mut line = StackStr::new();
    // PostgreSQL indents each child's `->  ` arrow six columns past its
    // parent's, starting two columns in.
    if node.depth > 0 {
        for _ in 0..node_indent(node) - 6 {
            let _ = write!(line, " ");
        }
        let _ = write!(line, "->  ");
    }
    let _ = write!(line, "{}", node.name.as_str());
    if node.relation.as_str().is_empty() && !node.alias.as_str().is_empty() {
        let _ = write!(line, " on ");
        crate::sql::catalog::write_identifier(&mut line, node.alias.as_str());
    }
    if !node.relation.as_str().is_empty() {
        let _ = write!(line, " on ");
        if options.verbose && !node.schema.as_str().is_empty() {
            crate::sql::catalog::write_identifier(&mut line, node.schema.as_str());
            let _ = write!(line, ".");
        }
        let _ = write!(line, "{}", node.relation.as_str());
        if !node.alias.as_str().is_empty() {
            let _ = write!(line, " {}", node.alias.as_str());
        }
    }
    if options.costs {
        let _ = write!(
            line,
            "  (cost={:.2}..{:.2} rows={} width={})",
            node.startup_cost, node.total_cost, node.rows, node.width
        );
    }
    match actual {
        NodeActual::Unmeasured => {}
        NodeActual::NeverExecuted => {
            let _ = write!(line, " (never executed)");
        }
        NodeActual::Ran(counters) => {
            let loops = counters.loops as f64;
            let _ = write!(line, " (actual ");
            if options.timing {
                let _ = write!(
                    line,
                    "time={:.3}..{:.3} ",
                    counters.startup_nanos as f64 / 1e6 / loops,
                    counters.total_nanos as f64 / 1e6 / loops
                );
            }
            let _ = write!(
                line,
                "rows={:.2} loops={})",
                counters.rows as f64 / loops,
                counters.loops
            );
        }
    }
    line
}

/// The hash table a `Hash` node built while `EXPLAIN ANALYZE` ran: pos3ql
/// builds it in one batch, in the arrays whose bytes it reports.
/// A `Hash` node's table as PostgreSQL reports it: `ExecChooseHashTableSize`
/// sizes its buckets from the node's row estimate, `ExecHashTableInsert`
/// doubles them while the rows outnumber them, and its memory is its rows'
/// bytes and its bucket pointers. The rows' bytes are pos3ql's own when a
/// hashed value has no PostgreSQL size here.
struct PostgresHashTable {
    buckets: u64,
    original_buckets: u64,
    batches: u64,
    original_batches: u64,
    bytes: usize,
}

impl PostgresHashTable {
    fn of(node: &PlanNode, table: query::instrument::HashTable) -> Self {
        /// `MaxAllocSize / sizeof(HashJoinTuple)`.
        const MAX_ALLOC_POINTERS: u64 = 0x3fff_ffff / 8;
        // `get_hash_memory_limit`.
        let memory_limit = crate::sql::guc::hash_memory_bytes().min(u64::MAX as f64) as u64;
        let pointers = (memory_limit / 8).clamp(1, MAX_ALLOC_POINTERS);
        let max_pointers = (1u64 << pointers.ilog2()).min(i32::MAX as u64 / 2 + 1);
        let estimated = if node.rows == 0 { 1000 } else { node.rows };
        let original = estimated.min(max_pointers).max(1024).next_power_of_two();
        let mut buckets = original;
        // A row inserted while the rows before it outnumber the buckets
        // doubles them.
        while table.tuples.saturating_sub(1) > buckets
            && buckets <= i32::MAX as u64 / 2
            && buckets * 2 <= MAX_ALLOC_POINTERS
        {
            buckets *= 2;
        }
        if let Some(batches) = table.batches {
            return Self {
                buckets: u64::from(batches.buckets),
                original_buckets: u64::from(batches.original_buckets),
                batches: u64::from(batches.batches),
                original_batches: u64::from(batches.original_batches),
                bytes: batches.peak,
            };
        }
        let rows = table.tuple_bytes.unwrap_or(table.own_bytes);
        Self {
            buckets,
            original_buckets: original,
            batches: 1,
            original_batches: 1,
            bytes: rows.saturating_add(buckets as usize * 8),
        }
    }
}

fn hash_table(actual: NodeActual) -> Option<query::instrument::HashTable> {
    match actual {
        NodeActual::Ran(counters) => counters.hash,
        NodeActual::Unmeasured | NodeActual::NeverExecuted => None,
    }
}

/// How a sort node sorted while `EXPLAIN ANALYZE` ran: the method and the
/// space of the rows it held, which pos3ql measures in its own row
/// encoding, not PostgreSQL's allocator.
fn sort_space(actual: NodeActual) -> Option<query::instrument::SortSpace> {
    match actual {
        NodeActual::Ran(counters) => counters.sort,
        NodeActual::Unmeasured | NodeActual::NeverExecuted => None,
    }
}

/// The count of rows `label`'s qualification of `node` removed, when
/// PostgreSQL shows one.
fn removed_count(
    node: &PlanNode,
    actual: NodeActual,
    label: QualLabel,
) -> Option<(&'static str, f64)> {
    let name = node.name.as_str();
    // A bitmap index scan's condition only marks the candidate rows; its
    // heap scan rechecks them.
    if label == QualLabel::IndexCond && !name.starts_with("Index") {
        return None;
    }
    Some((label.removed_label()?, actual.removed(label)?))
}

/// The lines an analyzed index or bitmap scan prints after its
/// conditions: an index-only scan's `Heap Fetches`, a bitmap heap scan's
/// `Heap Blocks` and each index scan's `Index Searches`, one per loop.
fn index_scan_details(node: &PlanNode, actual: NodeActual) -> impl Iterator<Item = StackStr<64>> {
    let counters = match actual {
        NodeActual::Unmeasured => None,
        NodeActual::NeverExecuted => Some(NodeCounters::default()),
        NodeActual::Ran(counters) => Some(counters),
    };
    let name = node.name.as_str();
    let index_only = name.starts_with("Index Only Scan ");
    let searched =
        index_only || name.starts_with("Index Scan ") || name.starts_with("Bitmap Index Scan on ");
    let mut lines = [StackStr::<64>::new(), StackStr::new()];
    let mut count = 0;
    if let Some(counters) = counters {
        if index_only {
            let _ = write!(lines[count], "Heap Fetches: {}", counters.heap_fetches);
            count += 1;
        }
        if name == "Bitmap Heap Scan" && counters.heap_blocks > 0 {
            let _ = write!(lines[count], "Heap Blocks: exact={}", counters.heap_blocks);
            count += 1;
        }
        if searched {
            let _ = write!(lines[count], "Index Searches: {}", counters.loops);
            count += 1;
        }
    }
    lines.into_iter().take(count)
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

/// The `Buffers` and, for the top node, `WAL` lines of the node at `index`.
fn node_usage_text<E>(
    plan: &Plan<'_>,
    index: usize,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    emit: &mut impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let node = &plan.nodes[index];
    let executed = !matches!(plan.actual(index, actual), NodeActual::NeverExecuted);
    if options.buffers && actual.is_some() && executed {
        let usage = plan.buffers(index);
        if !usage.is_zero() {
            emit(buffers_text(node_indent(node), usage).as_str())?;
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
    Ok(())
}

/// The lines of a write's top node after its result relations
/// (`show_modifytable_info`), before the node at `next`: an `ON CONFLICT`'s
/// resolution and outcome, a `MERGE`'s outcome, then its buffers and WAL.
fn write_tail_text<E>(
    plan: &Plan<'_>,
    next: usize,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    emit: &mut impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let indent = node_indent(&plan.nodes[0]);
    let line = |text: core::fmt::Arguments<'_>| {
        let mut line = StackStr::<768>::new();
        for _ in 0..indent {
            let _ = write!(line, " ");
        }
        let _ = line.write_fmt(text);
        line
    };
    if let Some(conflict) = plan.conflict {
        let action = if conflict.update { "UPDATE" } else { "NOTHING" };
        emit(line(format_args!("Conflict Resolution: {action}")).as_str())?;
        if conflict.count > 0 {
            let mut names = StackStr::<640>::new();
            for (at, name) in conflict.arbiters().enumerate() {
                let _ = write!(names, "{}{name}", if at == 0 { "" } else { ", " });
            }
            emit(line(format_args!("Conflict Arbiter Indexes: {}", names.as_str())).as_str())?;
        }
        if let Some((inserted, conflicting)) = Conflict::tuples(plan) {
            emit(line(format_args!("Tuples Inserted: {inserted}")).as_str())?;
            emit(line(format_args!("Conflicting Tuples: {conflicting}")).as_str())?;
        }
    }
    if let Some(tuples) = plan.merge_tuples(next)
        && tuples.total > 0
    {
        let mut text = StackStr::<256>::new();
        let _ = write!(text, "Tuples:");
        for (label, count) in [
            ("inserted", tuples.inserted),
            ("updated", tuples.updated),
            ("deleted", tuples.deleted),
            ("skipped", tuples.skipped),
        ] {
            if count > 0 {
                let _ = write!(text, " {label}={count}");
            }
        }
        emit(line(format_args!("{}", text.as_str())).as_str())?;
    }
    node_usage_text(plan, 0, options, actual, emit)
}

pub(super) fn visit_text_rows<E>(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    mut emit: impl FnMut(&str) -> Result<(), E>,
) -> Result<(), E> {
    let write = plan.count > 0 && writes(&plan.nodes[0]);
    let mut tail_written = false;
    for (index, node) in plan.nodes[..plan.count].iter().enumerate() {
        // A write's resolution and outcome follow its result relations, and
        // its buffers and WAL follow them.
        if write
            && index > 0
            && !node.target_member
            && plan.nodes[1..index].iter().all(|node| node.target_member)
        {
            write_tail_text(plan, index, options, actual, &mut emit)?;
            tail_written = true;
        }
        if node.target_member {
            let mut line = StackStr::<512>::new();
            let parent =
                property_indent(node.depth.saturating_sub(1)) + 2 * usize::from(node.shift);
            for _ in 0..parent {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "{} on ", node.name.as_str());
            if options.verbose {
                let _ = write!(line, "{}.", node.schema.as_str());
            }
            let _ = write!(line, "{}", node.relation.as_str());
            if !node.alias.as_str().is_empty() {
                let _ = write!(line, " {}", node.alias.as_str());
            }
            emit(line.as_str())?;
            continue;
        }
        let node_actual = plan.actual(index, actual);
        // A subplan's name precedes it where its parent's children start.
        if let Some((_, name)) = node.subplan.as_ref() {
            let mut label = StackStr::<128>::new();
            for _ in 0..node_indent(node) - 8 {
                let _ = write!(label, " ");
            }
            let _ = write!(label, "{}", name.as_str());
            emit(label.as_str())?;
        }
        let line = text_line(node, options, node_actual);
        emit(line.as_str())?;
        if plan.disabled(node) {
            let mut disabled = StackStr::<512>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(disabled, " ");
            }
            let _ = write!(disabled, "Disabled: true");
            emit(disabled.as_str())?;
        }
        if options.verbose && shows_output(node) {
            let mut output = StackStr::<1100>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(output, " ");
            }
            let _ = write!(output, "Output: {}", node.output.as_str());
            emit(output.as_str())?;
        }
        // Text shows a join's proven unique inner relation under VERBOSE.
        if options.verbose && node.inner_unique {
            let mut line = StackStr::<64>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "Inner Unique: true");
            emit(line.as_str())?;
        }
        if node.keys.label == KeyLabel::Sets {
            let records = if options.verbose {
                node.keys.verbose.as_str()
            } else {
                node.keys.plain.as_str()
            };
            // The lines after a rollup's `Sort Key` are indented by two.
            let mut extra = 0;
            for record in records.split(GROUPING_SET_SEPARATOR) {
                let mut chars = record.chars();
                let tag = chars.next().unwrap_or('B');
                let items = chars.as_str();
                let label = match tag {
                    'B' => {
                        extra = 0;
                        continue;
                    }
                    'S' => "Sort Key",
                    'H' => "Hash Key",
                    _ => "Group Key",
                };
                let mut line = StackStr::<2048>::new();
                for _ in 0..node_indent(node) + extra {
                    let _ = write!(line, " ");
                }
                if items.is_empty() {
                    let _ = write!(line, "{label}: ()");
                } else {
                    let _ = write!(line, "{label}: ");
                    for (at, key) in items.split(KEY_SEPARATOR).enumerate() {
                        let _ = write!(line, "{}{key}", if at == 0 { "" } else { ", " });
                    }
                }
                emit(line.as_str())?;
                if tag == 'S' {
                    extra = 2;
                }
            }
        }
        if let Some((label, mut keys)) = node.keys.shown(options.verbose) {
            let mut line = StackStr::<2048>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "{label}: {}", keys.next().unwrap_or_default());
            for key in keys {
                let _ = write!(line, ", {key}");
            }
            emit(line.as_str())?;
        }
        if let Some(space) = sort_space(node_actual).filter(|_| node.name.as_str() == "Sort") {
            let mut line = StackStr::<256>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(
                line,
                "Sort Method: {}  {}: {}kB",
                space.method.name(),
                space.method.space_type(),
                space.bytes.div_ceil(1024)
            );
            emit(line.as_str())?;
        }
        // `show_hashagg_info`, which an aggregate prints between its
        // `Filter` and the rows that removed.
        let planned = options
            .costs
            .then_some(node.planned_partitions)
            .flatten()
            .filter(|&partitions| partitions > 0);
        let peak = match node_actual {
            NodeActual::Ran(counters) => counters.hash_agg_peak,
            NodeActual::Unmeasured | NodeActual::NeverExecuted => None,
        };
        let hash_agg_line = (planned.is_some() || peak.is_some()).then(|| {
            let mut line = StackStr::<128>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            if let Some(partitions) = planned {
                let _ = write!(line, "Planned Partitions: {partitions}");
            }
            if let Some(bytes) = peak {
                if planned.is_some() {
                    let _ = write!(line, "  ");
                }
                let _ = write!(line, "Batches: 1  Memory Usage: {}kB", bytes.div_ceil(1024));
            }
            line
        });
        let mut hash_agg_shown = false;
        for (label, text) in node.shown_qualifications(options.verbose) {
            let mut qual = StackStr::<2048>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(qual, " ");
            }
            let _ = write!(qual, "{}: {}", label.text(), text);
            emit(qual.as_str())?;
            if let Some(line) = hash_agg_line.as_ref().filter(|_| !hash_agg_shown) {
                emit(line.as_str())?;
                hash_agg_shown = true;
            }
            // In text PostgreSQL leaves out a count of no rows.
            if let Some((removed_label, removed)) = removed_count(node, node_actual, label)
                && removed > 0.0
            {
                let mut line = StackStr::<128>::new();
                for _ in 0..node_indent(node) {
                    let _ = write!(line, " ");
                }
                let _ = write!(line, "{removed_label}: {removed:.0}");
                emit(line.as_str())?;
            }
        }
        if let Some(line) = hash_agg_line.as_ref().filter(|_| !hash_agg_shown) {
            emit(line.as_str())?;
        }
        for line in index_scan_details(node, node_actual) {
            let mut text = StackStr::<128>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(text, " ");
            }
            let _ = write!(text, "{line}");
            emit(text.as_str())?;
        }
        // `show_incremental_sort_info`.
        if let NodeActual::Ran(NodeCounters {
            incremental: Some(groups),
            ..
        }) = node_actual
            && groups.full.count > 0
        {
            for (label, kind) in [("Full-sort", groups.full), ("Pre-sorted", groups.presorted)] {
                if kind.count == 0 {
                    continue;
                }
                let mut line = StackStr::<256>::new();
                for _ in 0..node_indent(node) {
                    let _ = write!(line, " ");
                }
                let _ = write!(
                    line,
                    "{label} Groups: {}  Sort Method{}",
                    kind.count,
                    if kind.methods().count() > 1 {
                        "s: "
                    } else {
                        ": "
                    }
                );
                for (at, name) in kind.methods().enumerate() {
                    let _ = write!(line, "{}{name}", if at > 0 { ", " } else { "" });
                }
                if kind.peak_kb > 0 {
                    let _ = write!(
                        line,
                        "  Average Memory: {}kB  Peak Memory: {}kB",
                        kind.average_kb(),
                        kind.peak_kb
                    );
                }
                emit(line.as_str())?;
            }
        }
        if let NodeActual::Ran(NodeCounters {
            stored: Some(bytes),
            ..
        }) = node_actual
        {
            let mut line = StackStr::<256>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(
                line,
                "Storage: Memory  Maximum Storage: {}kB",
                bytes.div_ceil(1024)
            );
            emit(line.as_str())?;
        }
        if actual.is_some()
            && let Some((disk, bytes)) = plan.cte_storage(index)
        {
            let mut line = StackStr::<256>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(
                line,
                "Storage: {}  Maximum Storage: {}kB",
                if disk { "Disk" } else { "Memory" },
                bytes.div_ceil(1024)
            );
            emit(line.as_str())?;
        }
        if !node.relations.is_empty() {
            let mut line = StackStr::<512>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "Relations: {}", node.shown_relations(options.verbose));
            emit(line.as_str())?;
        }
        if options.verbose && !node.remote_sql.is_empty() {
            let mut line = StackStr::<{ REMOTE_SQL_BYTES + 256 }>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "Remote SQL: {}", node.remote_sql.as_str());
            emit(line.as_str())?;
        }
        if options.verbose
            && let Some(size) = node.batch_size
        {
            let mut line = StackStr::<64>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "Batch Size: {size}");
            emit(line.as_str())?;
        }
        if let NodeActual::Ran(counters) = node_actual
            && let Some(cache) = counters.memoize
        {
            let mut line = StackStr::<256>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(
                line,
                "Hits: {}  Misses: {}  Evictions: {}  Overflows: {}  Memory Usage: {}kB",
                cache.hits,
                cache.misses,
                cache.evictions,
                cache.overflows,
                cache.peak.div_ceil(1024)
            );
            emit(line.as_str())?;
        }
        if let Some(table) = hash_table(node_actual) {
            let table = PostgresHashTable::of(node, table);
            let mut line = StackStr::<256>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let memory = table.bytes.div_ceil(1024);
            if table.original_buckets == table.buckets && table.original_batches == table.batches {
                let _ = write!(
                    line,
                    "Buckets: {}  Batches: {}  Memory Usage: {memory}kB",
                    table.buckets, table.batches
                );
            } else {
                let _ = write!(
                    line,
                    "Buckets: {} (originally {})  Batches: {} (originally {})  Memory Usage: {memory}kB",
                    table.buckets, table.original_buckets, table.batches, table.original_batches
                );
            }
            emit(line.as_str())?;
        }
        if node.constant_false {
            let mut filter = StackStr::<64>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(filter, " ");
            }
            let _ = write!(filter, "One-Time Filter: false");
            emit(filter.as_str())?;
        }
        if !(write && index == 0) {
            node_usage_text(plan, index, options, actual, &mut emit)?;
        }
        if let Some(removed) = node.subplans_removed.filter(|removed| *removed > 0) {
            let mut line = StackStr::<128>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "Subplans Removed: {removed}");
            emit(line.as_str())?;
        }
    }
    if write && !tail_written {
        write_tail_text(plan, plan.count, options, actual, &mut emit)?;
    }
    if options.settings && !settings.entries().is_empty() {
        // Holds every reported setting at its longest shown value.
        let mut line = StackStr::<4096>::new();
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
    }
    // `ExplainPrintSerialize`, whatever `SUMMARY` says; sending the rows
    // reads no page, so no `Buffers` line follows it.
    if let Some(actual) = actual
        && options.serialize != ExplainSerialize::None
    {
        let mut serialization = StackStr::<512>::new();
        let format = match options.serialize {
            ExplainSerialize::None => unreachable!(),
            ExplainSerialize::Text => "text",
            ExplainSerialize::Binary => "binary",
        };
        let _ = write!(serialization, "Serialization: ");
        if options.timing {
            let _ = write!(
                serialization,
                "time={:.3} ms  ",
                actual.serialization_micros as f64 / 1_000.0
            );
        }
        let _ = write!(
            serialization,
            "output={}kB  format={}",
            actual.serialized_bytes.div_ceil(1024),
            format
        );
        emit(serialization.as_str())?;
    }
    if options.summary
        && let Some(actual) = actual
    {
        let mut execution = StackStr::<512>::new();
        let _ = write!(
            execution,
            "Execution Time: {:.3} ms",
            actual.elapsed_micros as f64 / 1_000.0
        );
        emit(execution.as_str())?;
    }
    Ok(())
}

pub(super) fn plan_statement<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let plan = plan_statement_nodes(storage, txid, statement, block, arena)?;
    if block == BlockId::ROOT {
        reject_joined_foreign_scans(&plan)?;
    }
    Ok(plan)
}

/// A `Foreign Scan` below a join: postgres_fdw's estimates for a join's
/// input, which choose PostgreSQL's join method and order, and its join
/// pushdown (`foreign_join_ok`) are not modeled, so neither the join nor
/// the scans under it would print as PostgreSQL's.
fn reject_joined_foreign_scans(plan: &Plan<'_>) -> Result<(), SqlError> {
    let nodes = &plan.nodes[..plan.count];
    for (index, node) in nodes.iter().enumerate() {
        if node.name.as_str() != "Foreign Scan" {
            continue;
        }
        // Each ancestor up to the root of the subplan holding the scan.
        let mut at = index;
        while nodes[at].subplan.is_none() {
            let depth = nodes[at].depth;
            let Some(parent) = (0..at).rev().find(|&earlier| nodes[earlier].depth < depth) else {
                break;
            };
            at = parent;
            let name = nodes[at].name.as_str();
            if name.contains(" Join") || name.starts_with("Nested Loop") {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN of a join over a foreign table scan is not supported"
                ));
            }
        }
    }
    Ok(())
}

fn plan_statement_nodes<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    // The planner plans the statement its constant folding produced, and
    // raises the folding's notices and errors (`eval_const_expressions`).
    // PostgreSQL's folding also replaces a call of a SQL function whose body
    // is one plain expression by that expression (`inline_function`), which
    // then prints, costs, estimates and matches index keys in its place. A
    // call the folding could not decide (an argument reading an enclosing
    // query's column, a packed variadic or polymorphic call, a `BEGIN
    // ATOMIC` body) is refused.
    let (statement, uninlined) = crate::sql::eval::folding_uninlined_calls(|| {
        fold_statement(storage, txid, statement, arena)
    });
    let statement = statement?;
    if uninlined {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN of a call of a SQL function PostgreSQL inlines, with an outer column, packed variadic or polymorphic argument or a BEGIN ATOMIC body, is not supported"
        ));
    }
    if block != BlockId::ROOT {
        return plan_folded_statement(storage, txid, statement, block, arena);
    }
    // `set_rtable_names` leaves a relation the plan never scans unnamed,
    // unless the plan scans none at all (`rels_used` is empty).
    let mut named = false;
    loop {
        reset_subplans();
        EXPANDED_RELATIONS.with(|expanded| expanded.set(false));
        NAMELESS_DUMMIES.with(|dummies| dummies.set((!named, false)));
        let plan = plan_folded_statement(storage, txid, statement, block, arena);
        let (_, seen) = NAMELESS_DUMMIES.with(|dummies| dummies.replace((false, false)));
        let plan = plan?;
        let scans = plan.nodes[..plan.count]
            .iter()
            .any(|node| !node.relation.as_str().is_empty() || node.name.as_str() == "Values Scan");
        if named || !seen || scans {
            return Ok(plan);
        }
        named = true;
    }
}

fn plan_folded_statement<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    match statement {
        Stmt::Select(select) => {
            if select.with.is_empty() {
                let select = if storage.has_any_view() {
                    // A view is read through its query (its `_RETURN` rule).
                    query::expand_ctes(select, storage, txid, arena)?
                } else {
                    select
                };
                return plan_select(storage, txid, select, None, block, arena);
            }
            let (select, ctes) = query::expand_ctes_for_plan(select, storage, txid, arena)?;
            let prior = SUBPLAN_RELATIONS.with(|planned| planned.replace(!ctes.is_empty()));
            let plan = plan_ctes(storage, txid, ctes, None, block, arena).and_then(|ctes| {
                plan_select_with_ctes(storage, txid, select, ctes, None, block, arena)
            });
            SUBPLAN_RELATIONS.with(|planned| planned.set(prior));
            plan
        }
        Stmt::SetQuery(set_query) if !set_query.with.is_empty() => {
            // The statement's `WITH` queries PostgreSQL runs once are
            // InitPlans of its top node (`SS_process_ctes`).
            let wrapper = query::wrap_set_tree_with(set_query.with, set_query.body, arena)?;
            let (expanded, ctes) = query::expand_ctes_for_plan(wrapper, storage, txid, arena)?;
            let planned = SetQuery {
                with: &[],
                body: expanded.set_body.expect("the wrapper keeps its set body"),
                ..*set_query
            };
            let prior = SUBPLAN_RELATIONS.with(|planned| planned.replace(!ctes.is_empty()));
            let plan = plan_ctes(storage, txid, ctes, None, block, arena).and_then(|ctes| {
                let plan = plan_set_query(storage, txid, &planned, None, block, arena)?;
                attach_subplans(plan, 0, block, ctes, arena)
            });
            SUBPLAN_RELATIONS.with(|planned| planned.set(prior));
            plan
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
            plan_set_query(storage, txid, &planned, None, block, arena)
        }
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_)
            if storage.has_any_view() =>
        {
            // The views a statement reads are read through their queries;
            // its target stays as written.
            let expanded =
                query::expand_dml_ctes_schema(statement, &[], storage, txid, None, None, arena)?;
            plan_modification(
                storage,
                txid,
                rewrite::pull_up_sources(storage, txid, expanded, arena)?,
                block,
                arena,
            )
        }
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => {
            plan_modification(storage, txid, statement, block, arena)
        }
        Stmt::With { ctes, statement } => {
            // The `WITH` queries PostgreSQL runs once are InitPlans of the
            // write's top node (`SS_process_ctes`); the write reads them by
            // `CTE Scan`.
            let (statement, planned) =
                query::expand_dml_ctes_for_plan(statement, ctes, storage, txid, arena)?;
            let statement = if storage.has_any_view() {
                rewrite::pull_up_sources(storage, txid, statement, arena)?
            } else {
                statement
            };
            let prior = SUBPLAN_RELATIONS.with(|cte_plans| cte_plans.replace(!planned.is_empty()));
            let plan = plan_ctes(storage, txid, planned, None, block, arena).and_then(|ctes| {
                let plan = plan_modification(storage, txid, statement, block, arena)?;
                attach_subplans(plan, 0, block, ctes, arena)
            });
            SUBPLAN_RELATIONS.with(|cte_plans| cte_plans.set(prior));
            plan
        }
        _ => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN does not support this statement type"
        )),
    }
}

/// `statement` with its constants folded, as execution folds them before it
/// reads a row. A data-modifying `WITH` and `MERGE` fold where they run.
fn fold_statement<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    arena: &'a Arena,
) -> Result<&'a Stmt<'a>, SqlError> {
    let folded = match statement {
        Stmt::Select(select) if select.with.iter().all(|cte| cte.dml.is_none()) => {
            let folded = preprocess_select(select, storage, txid, arena)?;
            if core::ptr::eq(folded, select) {
                return Ok(statement);
            }
            Stmt::Select(*folded)
        }
        Stmt::Update(update) => {
            Stmt::Update(query::fold_update_constants(update, arena, storage, txid)?)
        }
        Stmt::Delete(delete) => {
            Stmt::Delete(query::fold_delete_constants(delete, arena, storage, txid)?)
        }
        Stmt::Insert(insert) => {
            Stmt::Insert(query::fold_insert_constants(insert, arena, storage, txid)?)
        }
        _ => return Ok(statement),
    };
    arena
        .alloc(folded)
        .map(|folded| &*folded)
        .map_err(|_| arena_full())
}

/// `subquery_planner`'s preprocessing of a query level: its views expanded
/// (the rewriter's), its set-returning SQL functions in FROM inlined, each
/// subquery in FROM preprocessed in turn, then its expressions folded, so a
/// subquery the planner pulls up brings its folded, inlined expressions.
fn preprocess_select<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let expanded = if select.with.is_empty() && storage.has_any_view() {
        query::expand_ctes(select, storage, txid, arena)?
    } else {
        select
    };
    let inlined = inline_set_returning_functions(expanded, storage, txid, arena)?;
    let inlined = match &inlined.from {
        Some(from) if inlined.with.is_empty() => {
            let preprocess = |item: &crate::sql::ast::TableRef<'a>| {
                Ok::<_, SqlError>(match item.subquery {
                    Some(body) if !item.lateral && item.join_members.is_none() => {
                        let folded = preprocess_select(body, storage, txid, arena)?;
                        (!core::ptr::eq(folded, body)).then_some(folded)
                    }
                    _ => None,
                })
            };
            let base = preprocess(&from.base)?;
            let mut joins = None;
            for (at, join) in from.joins.iter().enumerate() {
                if let Some(folded) = preprocess(&join.table)? {
                    let copy = match &mut joins {
                        Some(copy) => copy,
                        None => joins.insert(
                            arena
                                .alloc_slice_copy(from.joins)
                                .map_err(|_| arena_full())?,
                        ),
                    };
                    copy[at].table.subquery = Some(folded);
                }
            }
            if base.is_none() && joins.is_none() {
                inlined
            } else {
                let mut copy = *inlined;
                let mut from = *from;
                if let Some(base) = base {
                    from.base.subquery = Some(base);
                }
                if let Some(joins) = joins {
                    from.joins = joins;
                }
                copy.from = Some(from);
                arena
                    .alloc(copy)
                    .map(|copy| &*copy)
                    .map_err(|_| arena_full())?
            }
        }
        _ => inlined,
    };
    let folded = query::fold_select_constants(inlined, arena, storage, txid)?;
    let folded = query::fold_sublinks(folded, arena, storage, txid, None)?;
    Ok(
        if core::ptr::eq(folded, expanded) && core::ptr::eq(expanded, select) {
            select
        } else {
            folded
        },
    )
}

/// `inline_set_returning_functions`: each FROM item calling a SQL function
/// PostgreSQL inlines becomes a subquery of its body, which the planner then
/// pulls up like any other.
fn inline_set_returning_functions<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    use crate::sql::planner_inputs::inline::{SetInlining, inline_set_returning_call};
    let Some(from) = &select.from else {
        return Ok(select);
    };
    let inline = |item: &crate::sql::ast::TableRef<'a>| {
        Ok(
            match inline_set_returning_call(item, storage, txid, arena)? {
                SetInlining::Kept => None,
                SetInlining::Inlined(inlined) => Some(*inlined),
                SetInlining::Unmodeled => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of a SQL function in FROM PostgreSQL inlines, with a default, variadic, polymorphic or lateral argument, a body with WITH, windows or a column coercion, is not supported"
                    ));
                }
            },
        )
    };
    let base = inline(&from.base)?;
    let mut joins = None;
    for (at, join) in from.joins.iter().enumerate() {
        if let Some(inlined) = inline(&join.table)? {
            let copy = match &mut joins {
                Some(copy) => copy,
                None => joins.insert(
                    arena
                        .alloc_slice_copy(from.joins)
                        .map_err(|_| arena_full())?,
                ),
            };
            copy[at].table = inlined;
        }
    }
    if base.is_none() && joins.is_none() {
        return Ok(select);
    }
    let mut copy = *select;
    copy.from = Some(crate::sql::ast::FromClause {
        base: base.unwrap_or(from.base),
        joins: joins.map_or(from.joins, |joins| &*joins),
    });
    arena
        .alloc(copy)
        .map(|copy| &*copy)
        .map_err(|_| arena_full())
}

/// PostgreSQL's structured EXPLAIN writer (`explain_format.c`): groups and
/// properties laid out for JSON, XML or YAML with its indentation, so a
/// document of one or several queries matches PostgreSQL's text exactly.
struct Document<'a> {
    format: ExplainFormat,
    out: crate::mem::arena::ArenaString<'a>,
    indent: usize,
    /// Per open group, whether it already holds an entry (JSON, YAML).
    grouping: [bool; 64],
    groups: usize,
    full: bool,
}

impl<'a> Document<'a> {
    fn new(format: ExplainFormat, arena: &'a Arena) -> Self {
        let mut document = Self {
            format,
            out: crate::mem::arena::ArenaString::new(arena),
            indent: 0,
            grouping: [false; 64],
            groups: 0,
            full: false,
        };
        match format {
            ExplainFormat::Xml => {
                document.push("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n");
                document.indent += 1;
            }
            ExplainFormat::Json => {
                document.push("[");
                document.push_group(false);
                document.indent += 1;
            }
            ExplainFormat::Yaml => document.push_group(false),
            ExplainFormat::Text => {}
        }
        document
    }

    fn push(&mut self, text: &str) {
        if self.out.write_str(text).is_err() {
            self.full = true;
        }
    }

    fn spaces(&mut self, count: usize) {
        for _ in 0..count {
            self.push(" ");
        }
    }

    fn push_group(&mut self, entered: bool) {
        if self.groups == self.grouping.len() {
            self.full = true;
            return;
        }
        self.grouping[self.groups] = entered;
        self.groups += 1;
    }

    fn pop_group(&mut self) {
        self.groups = self.groups.saturating_sub(1);
    }

    /// `ExplainJSONLineEnding`: a comma after an earlier entry, then a line.
    fn json_line_ending(&mut self) {
        let top = self.groups - 1;
        if self.grouping[top] {
            self.push(",");
        } else {
            self.grouping[top] = true;
        }
        self.push("\n");
    }

    /// `ExplainYAMLLineStarting`: every entry but a group's first starts a
    /// new line at the current indentation.
    fn yaml_line_starting(&mut self) {
        let top = self.groups - 1;
        if self.grouping[top] {
            self.push("\n");
            self.spaces(self.indent * 2);
        } else {
            self.grouping[top] = true;
        }
    }

    fn xml_tag(&mut self, name: &str, closing: bool, whitespace: bool) {
        if whitespace {
            self.spaces(self.indent * 2);
        }
        self.push(if closing { "</" } else { "<" });
        for character in name.chars() {
            let mut encoded = [0u8; 4];
            self.push(if character == ' ' {
                "-"
            } else {
                character.encode_utf8(&mut encoded)
            });
        }
        self.push(">");
        if whitespace {
            self.push("\n");
        }
    }

    fn json_text(&mut self, value: &str) {
        self.push("\"");
        for character in value.chars() {
            match character {
                '"' => self.push("\\\""),
                '\\' => self.push("\\\\"),
                '\n' => self.push("\\n"),
                '\r' => self.push("\\r"),
                '\t' => self.push("\\t"),
                '\u{08}' => self.push("\\b"),
                '\u{0c}' => self.push("\\f"),
                character if (character as u32) < 0x20 => {
                    let escaped = stack_format!(8, "\\u{:04x}", character as u32);
                    self.push(escaped.as_str());
                }
                character => {
                    let mut encoded = [0u8; 4];
                    self.push(character.encode_utf8(&mut encoded));
                }
            }
        }
        self.push("\"");
    }

    fn xml_text(&mut self, value: &str) {
        for character in value.chars() {
            match character {
                '&' => self.push("&amp;"),
                '<' => self.push("&lt;"),
                '>' => self.push("&gt;"),
                character => {
                    let mut encoded = [0u8; 4];
                    self.push(character.encode_utf8(&mut encoded));
                }
            }
        }
    }

    /// `ExplainOpenGroup`. `label` names the group within its parent;
    /// `labeled` says whether its members are named (an object, not a list).
    fn open_group(&mut self, kind: &str, label: Option<&str>, labeled: bool) {
        match self.format {
            ExplainFormat::Xml => {
                self.xml_tag(kind, false, true);
                self.indent += 1;
            }
            ExplainFormat::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                if let Some(label) = label {
                    self.json_text(label);
                    self.push(": ");
                }
                self.push(if labeled { "{" } else { "[" });
                self.push_group(false);
                self.indent += 1;
            }
            ExplainFormat::Yaml => {
                self.yaml_line_starting();
                match label {
                    Some(label) => {
                        self.push(label);
                        self.push(": ");
                        self.push_group(true);
                    }
                    None => {
                        self.push("- ");
                        self.push_group(false);
                    }
                }
                self.indent += 1;
            }
            ExplainFormat::Text => {}
        }
    }

    fn close_group(&mut self, kind: &str, labeled: bool) {
        match self.format {
            ExplainFormat::Xml => {
                self.indent -= 1;
                self.xml_tag(kind, true, true);
            }
            ExplainFormat::Json => {
                self.indent -= 1;
                self.push("\n");
                self.spaces(self.indent * 2);
                self.push(if labeled { "}" } else { "]" });
                self.pop_group();
            }
            ExplainFormat::Yaml => {
                self.indent -= 1;
                self.pop_group();
            }
            ExplainFormat::Text => {}
        }
    }

    /// `ExplainProperty`: a numeric value is written bare, text quoted.
    fn property(&mut self, label: &str, value: &str, numeric: bool) {
        match self.format {
            ExplainFormat::Xml => {
                self.spaces(self.indent * 2);
                self.xml_tag(label, false, false);
                self.xml_text(value);
                self.xml_tag(label, true, false);
                self.push("\n");
            }
            ExplainFormat::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                self.json_text(label);
                self.push(": ");
                if numeric {
                    self.push(value);
                } else {
                    self.json_text(value);
                }
            }
            ExplainFormat::Yaml => {
                self.yaml_line_starting();
                self.push(label);
                self.push(": ");
                if numeric {
                    self.push(value);
                } else {
                    self.json_text(value);
                }
            }
            ExplainFormat::Text => {}
        }
    }

    fn text(&mut self, label: &str, value: &str) {
        self.property(label, value, false);
    }

    fn number(&mut self, label: &str, value: impl core::fmt::Display) {
        let value = stack_format!(48, "{value}");
        self.property(label, value.as_str(), true);
    }

    fn boolean(&mut self, label: &str, value: bool) {
        self.property(label, if value { "true" } else { "false" }, true);
    }

    /// `ExplainPropertyList`.
    fn list<'v>(&mut self, label: &str, items: impl Iterator<Item = &'v str>) {
        match self.format {
            ExplainFormat::Xml => {
                self.xml_tag(label, false, true);
                for item in items {
                    self.spaces(self.indent * 2 + 2);
                    self.xml_tag("Item", false, false);
                    self.xml_text(item);
                    self.xml_tag("Item", true, false);
                    self.push("\n");
                }
                self.xml_tag(label, true, true);
            }
            ExplainFormat::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                self.json_text(label);
                self.push(": [");
                for (index, item) in items.enumerate() {
                    if index != 0 {
                        self.push(", ");
                    }
                    self.json_text(item);
                }
                self.push("]");
            }
            ExplainFormat::Yaml => {
                self.yaml_line_starting();
                self.push(label);
                self.push(": ");
                for item in items {
                    self.push("\n");
                    self.spaces(self.indent * 2 + 2);
                    self.push("- ");
                    self.json_text(item);
                }
            }
            ExplainFormat::Text => {}
        }
    }

    /// `ExplainPropertyListNested`: a list inside a list, written inline.
    fn nested_list<'v>(&mut self, label: &str, items: impl Iterator<Item = &'v str>) {
        match self.format {
            ExplainFormat::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                self.push("[");
                for (index, item) in items.enumerate() {
                    if index != 0 {
                        self.push(", ");
                    }
                    self.json_text(item);
                }
                self.push("]");
            }
            ExplainFormat::Yaml => {
                self.yaml_line_starting();
                self.push("- [");
                for (index, item) in items.enumerate() {
                    if index != 0 {
                        self.push(", ");
                    }
                    self.json_text(item);
                }
                self.push("]");
            }
            ExplainFormat::Xml | ExplainFormat::Text => self.list(label, items),
        }
    }

    /// `ExplainDummyGroup` without a label: a group with no contents.
    fn dummy_group(&mut self, kind: &str) {
        match self.format {
            ExplainFormat::Xml => {
                self.spaces(self.indent * 2);
                self.push("<");
                self.push(kind);
                self.push(" />\n");
            }
            ExplainFormat::Json => {
                self.json_line_ending();
                self.spaces(self.indent * 2);
                self.json_text(kind);
            }
            ExplainFormat::Yaml => {
                self.yaml_line_starting();
                self.push("- ");
                self.json_text(kind);
            }
            ExplainFormat::Text => {}
        }
    }

    /// `ExplainEndOutput`.
    fn finish(mut self) -> Result<&'a str, SqlError> {
        match self.format {
            ExplainFormat::Xml => {
                self.indent -= 1;
                self.push("</explain>");
            }
            ExplainFormat::Json => {
                self.indent -= 1;
                self.push("\n]");
                self.pop_group();
            }
            ExplainFormat::Yaml => self.pop_group(),
            ExplainFormat::Text => {}
        }
        if self.full {
            return Err(arena_full());
        }
        Ok(self.out.as_str())
    }
}

fn structured_node_name(node: &PlanNode) -> (&str, Option<&str>) {
    let name = node.name.as_str();
    match name.strip_prefix("Index Scan using ") {
        Some(index) => ("Index Scan", Some(index)),
        None => (name, None),
    }
}

/// PostgreSQL's node type and the properties its name implies: an
/// aggregate's strategy, a modification's operation and a join's type.
struct NodeKind<'n> {
    node_type: &'n str,
    strategy: Option<&'static str>,
    operation: Option<&'n str>,
    join_type: Option<&'n str>,
    index_name: Option<&'n str>,
    scan_direction: bool,
}

fn node_kind(node: &PlanNode) -> NodeKind<'_> {
    let (name, index_name) = structured_node_name(node);
    let mut kind = NodeKind {
        node_type: name,
        strategy: None,
        operation: None,
        join_type: None,
        index_name,
        scan_direction: index_name.is_some(),
    };
    match name {
        "Insert" | "Update" | "Delete" | "Merge" => {
            kind.node_type = "ModifyTable";
            kind.operation = Some(name);
        }
        "Foreign Scan" => kind.operation = Some("Select"),
        "Foreign Update" | "Foreign Delete" => {
            kind.node_type = "Foreign Scan";
            kind.operation = Some(&name["Foreign ".len()..]);
        }
        "Aggregate" => kind.strategy = Some("Plain"),
        "HashAggregate" => {
            kind.node_type = "Aggregate";
            kind.strategy = Some("Hashed");
        }
        "GroupAggregate" => {
            kind.node_type = "Aggregate";
            kind.strategy = Some("Sorted");
        }
        "MixedAggregate" => {
            kind.node_type = "Aggregate";
            kind.strategy = Some("Mixed");
        }
        "Nested Loop" | "Hash Join" | "Merge Join" => kind.join_type = Some("Inner"),
        // `Nested Loop Left Join`, `Hash Right Semi Join`, ...: the node
        // type, then the join type.
        _ => {
            let joined = name
                .strip_prefix("Nested Loop ")
                .map(|rest| ("Nested Loop", rest))
                .or_else(|| name.strip_prefix("Hash ").map(|rest| ("Hash Join", rest)))
                .or_else(|| name.strip_prefix("Merge ").map(|rest| ("Merge Join", rest)));
            if let Some((node_type, rest)) = joined
                && let Some(join_type) = rest.strip_suffix(" Join")
            {
                kind.node_type = node_type;
                kind.join_type = Some(join_type);
            }
        }
    }
    kind
}

/// The buffer counters PostgreSQL shows for a node, for planning or for
/// serialization (`show_buffer_usage`), every one even when zero.
fn structured_buffers(document: &mut Document<'_>, usage: BufferUsage) {
    for (label, value) in [
        ("Shared Hit Blocks", usage.shared_hit),
        ("Shared Read Blocks", usage.shared_read),
        ("Shared Dirtied Blocks", usage.shared_dirtied),
        ("Shared Written Blocks", usage.shared_written),
        ("Local Hit Blocks", usage.local_hit),
        ("Local Read Blocks", usage.local_read),
        ("Local Dirtied Blocks", usage.local_dirtied),
        ("Local Written Blocks", usage.local_written),
        ("Temp Read Blocks", usage.temp_read),
        ("Temp Written Blocks", usage.temp_written),
    ] {
        document.number(label, value);
    }
}

/// A text plan's `Buffers:` line at `indent`, which shows only the
/// counters above zero (`show_buffer_usage`).
fn buffers_text(indent: usize, usage: BufferUsage) -> StackStr<512> {
    let mut line = StackStr::<512>::new();
    for _ in 0..indent {
        let _ = write!(line, " ");
    }
    let _ = write!(line, "Buffers:");
    let groups = [
        (
            "shared",
            [
                ("hit", usage.shared_hit),
                ("read", usage.shared_read),
                ("dirtied", usage.shared_dirtied),
                ("written", usage.shared_written),
            ],
        ),
        (
            "local",
            [
                ("hit", usage.local_hit),
                ("read", usage.local_read),
                ("dirtied", usage.local_dirtied),
                ("written", usage.local_written),
            ],
        ),
        (
            "temp",
            [
                ("hit", 0),
                ("read", usage.temp_read),
                ("dirtied", 0),
                ("written", usage.temp_written),
            ],
        ),
    ];
    let mut first = true;
    for (name, counters) in groups {
        if counters.iter().all(|&(_, value)| value == 0) {
            continue;
        }
        if !first {
            let _ = write!(line, ",");
        }
        first = false;
        let _ = write!(line, " {name}");
        for (label, value) in counters {
            if value > 0 {
                let _ = write!(line, " {label}={value}");
            }
        }
    }
    line
}

/// `ExplainNode` for the subtree at `at`; returns the index after it.
fn structured_node(
    document: &mut Document<'_>,
    plan: &Plan<'_>,
    at: usize,
    relationship: Option<&str>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
) -> usize {
    let node = &plan.nodes[at];
    let kind = node_kind(node);
    document.open_group("Plan", relationship.is_none().then_some("Plan"), true);
    document.text("Node Type", kind.node_type);
    if let Some(strategy) = kind.strategy {
        document.text("Strategy", strategy);
        document.text("Partial Mode", "Simple");
    }
    if let Some(operation) = kind.operation {
        document.text("Operation", operation);
    }
    if let Some(relationship) = relationship {
        document.text("Parent Relationship", relationship);
    }
    if let Some((_, name)) = node.subplan.as_ref() {
        document.text("Subplan Name", name.as_str());
    }
    document.boolean("Parallel Aware", false);
    document.boolean("Async Capable", false);
    if kind.scan_direction {
        document.text("Scan Direction", "Forward");
    }
    if let Some(index_name) = kind.index_name {
        document.text("Index Name", index_name);
    }
    if !node.bitmap_index.as_str().is_empty() && node.relation.as_str().is_empty() {
        document.text("Index Name", node.bitmap_index.as_str());
    }
    if !node.relation.as_str().is_empty() {
        document.text(
            match node.name.as_str() {
                "Function Scan" => "Function Name",
                "CTE Scan" | "WorkTable Scan" => "CTE Name",
                _ => "Relation Name",
            },
            node.relation.as_str(),
        );
        if options.verbose && !node.schema.as_str().is_empty() {
            document.text("Schema", node.schema.as_str());
        }
    }
    if !node.exposed_name().is_empty() {
        document.text("Alias", node.exposed_name());
    }
    if let Some(join_type) = kind.join_type {
        document.text("Join Type", join_type);
    }
    if options.costs {
        document.number(
            "Startup Cost",
            stack_format!(32, "{:.2}", node.startup_cost),
        );
        document.number("Total Cost", stack_format!(32, "{:.2}", node.total_cost));
        document.number("Plan Rows", node.rows);
        document.number("Plan Width", node.width);
    }
    let full_actual = actual;
    let node_actual = plan.actual(at, actual);
    match node_actual {
        NodeActual::Unmeasured => {}
        NodeActual::NeverExecuted => {
            if options.timing {
                document.number("Actual Startup Time", "0.000");
                document.number("Actual Total Time", "0.000");
            }
            document.number("Actual Rows", 0);
            document.number("Actual Loops", 0);
        }
        NodeActual::Ran(counters) => {
            let loops = counters.loops as f64;
            if options.timing {
                document.number(
                    "Actual Startup Time",
                    stack_format!(32, "{:.3}", counters.startup_nanos as f64 / 1e6 / loops),
                );
                document.number(
                    "Actual Total Time",
                    stack_format!(32, "{:.3}", counters.total_nanos as f64 / 1e6 / loops),
                );
            }
            document.number(
                "Actual Rows",
                stack_format!(32, "{:.2}", counters.rows as f64 / loops),
            );
            document.number("Actual Loops", counters.loops);
        }
    }
    let actual = actual.filter(|_| at == 0);
    document.boolean("Disabled", plan.disabled(node));
    if options.verbose && shows_output(node) {
        document.list("Output", node.output.as_str().split(", "));
    }
    // A write's result relations (`show_modifytable_info`).
    let mut members_end = at + 1;
    while members_end < plan.count && plan.nodes[members_end].target_member {
        members_end += 1;
    }
    if members_end > at + 1 {
        document.open_group("Target Tables", Some("Target Tables"), false);
        for member in &plan.nodes[at + 1..members_end] {
            document.open_group("Target Table", None, true);
            document.text("Relation Name", member.relation.as_str());
            if options.verbose {
                document.text("Schema", member.schema.as_str());
            }
            document.text("Alias", member.exposed_name());
            document.close_group("Target Table", true);
        }
    }
    // A `MERGE`'s outcome, inside its result relations' group.
    if at == 0
        && let Some(conflict) = plan.conflict
    {
        document.text(
            "Conflict Resolution",
            if conflict.update { "UPDATE" } else { "NOTHING" },
        );
        if conflict.count > 0 {
            document.list("Conflict Arbiter Indexes", conflict.arbiters());
        }
        if let Some((inserted, conflicting)) = Conflict::tuples(plan) {
            document.number("Tuples Inserted", inserted);
            document.number("Conflicting Tuples", conflicting);
        }
    }
    if at == 0
        && let Some(tuples) = plan.merge_tuples(members_end)
    {
        tuples.properties(&mut |label, count| document.number(label, count));
    }
    if members_end > at + 1 {
        document.close_group("Target Tables", false);
    }
    if kind.join_type.is_some() {
        document.boolean("Inner Unique", node.inner_unique);
    }
    if node.keys.label == KeyLabel::Sets {
        let records = if options.verbose {
            node.keys.verbose.as_str()
        } else {
            node.keys.plain.as_str()
        };
        document.open_group("Grouping Sets", Some("Grouping Sets"), false);
        // The open groups: a rollup's, then its key set's.
        let mut open: Option<&str> = None;
        let close = |document: &mut Document<'_>, open: &mut Option<&str>| {
            if let Some(keys) = open.take() {
                document.close_group(keys, false);
                document.close_group("Grouping Set", true);
            }
        };
        let mut started = false;
        for record in records.split(GROUPING_SET_SEPARATOR) {
            let mut chars = record.chars();
            let tag = chars.next().unwrap_or('B');
            let items = chars.as_str();
            let entries = items.split(KEY_SEPARATOR).filter(|_| !items.is_empty());
            match tag {
                'B' => {
                    if started {
                        if open.is_none() {
                            document.close_group("Grouping Set", true);
                        }
                        close(document, &mut open);
                    }
                    started = true;
                    document.open_group("Grouping Set", None, true);
                }
                'S' => document.list("Sort Key", entries),
                _ => {
                    let (name, set) = if tag == 'H' {
                        ("Hash Keys", "Hash Key")
                    } else {
                        ("Group Keys", "Group Key")
                    };
                    if open.is_none() {
                        document.open_group(name, Some(name), false);
                        open = Some(name);
                    }
                    document.nested_list(set, entries);
                }
            }
        }
        if started {
            if open.is_none() {
                document.close_group("Grouping Set", true);
            }
            close(document, &mut open);
        }
        document.close_group("Grouping Sets", false);
    }
    if let Some((label, keys)) = node.keys.shown(options.verbose) {
        document.list(label, keys);
    }
    if let Some(space) = sort_space(node_actual).filter(|_| node.name.as_str() == "Sort") {
        document.text("Sort Method", space.method.name());
        document.number("Sort Space Used", space.bytes.div_ceil(1024));
        document.text("Sort Space Type", space.method.space_type());
    }
    if node.constant_false {
        document.text("One-Time Filter", "false");
    }
    if let Some(table) = hash_table(node_actual) {
        let table = PostgresHashTable::of(node, table);
        document.number("Hash Buckets", table.buckets);
        document.number("Original Hash Buckets", table.original_buckets);
        document.number("Hash Batches", table.batches);
        document.number("Original Hash Batches", table.original_batches);
        document.number("Peak Memory Usage", table.bytes.div_ceil(1024));
    }
    for (label, text) in node.shown_qualifications(options.verbose) {
        if label == QualLabel::PresortedKey {
            document.list(label.text(), text.split(", "));
            continue;
        }
        document.text(label.text(), text);
        if let Some((removed_label, removed)) = removed_count(node, node_actual, label) {
            document.number(removed_label, stack_format!(32, "{removed:.0}"));
        }
    }
    if options.costs
        && let Some(partitions) = node.planned_partitions
    {
        document.number("Planned Partitions", partitions);
    }
    if let NodeActual::Ran(NodeCounters {
        hash_agg_peak: Some(bytes),
        ..
    }) = node_actual
    {
        document.number("HashAgg Batches", 1);
        document.number("Peak Memory Usage", bytes.div_ceil(1024));
        document.number("Disk Usage", 0);
    }
    let counters = match node_actual {
        NodeActual::Unmeasured => None,
        NodeActual::NeverExecuted => Some(NodeCounters::default()),
        NodeActual::Ran(counters) => Some(counters),
    };
    if let Some(counters) = counters {
        let name = node.name.as_str();
        if name.starts_with("Index Only Scan ") {
            document.number("Heap Fetches", counters.heap_fetches);
        }
        if name == "Bitmap Heap Scan" {
            document.number("Exact Heap Blocks", counters.heap_blocks);
            document.number("Lossy Heap Blocks", 0);
        }
        if name.starts_with("Index Scan ")
            || name.starts_with("Index Only Scan ")
            || name.starts_with("Bitmap Index Scan on ")
        {
            document.number("Index Searches", counters.loops);
        }
    }
    if let NodeActual::Ran(NodeCounters {
        incremental: Some(groups),
        ..
    }) = node_actual
        && groups.full.count > 0
    {
        for (label, kind) in [
            ("Full-sort Groups", groups.full),
            ("Pre-sorted Groups", groups.presorted),
        ] {
            if kind.count == 0 {
                continue;
            }
            document.open_group("Incremental Sort Groups", Some(label), true);
            document.number("Group Count", kind.count);
            document.list("Sort Methods Used", kind.methods());
            document.open_group("Sort Space", Some("Sort Space Memory"), true);
            document.number("Average Sort Space Used", kind.average_kb());
            document.number("Peak Sort Space Used", kind.peak_kb);
            document.close_group("Sort Space", true);
            document.close_group("Incremental Sort Groups", true);
        }
    }
    if let NodeActual::Ran(NodeCounters {
        stored: Some(bytes),
        ..
    }) = node_actual
    {
        document.text("Storage", "Memory");
        document.number("Maximum Storage", bytes.div_ceil(1024));
    }
    if actual.is_some()
        && let Some((disk, bytes)) = plan.cte_storage(at)
    {
        document.text("Storage", if disk { "Disk" } else { "Memory" });
        document.number("Maximum Storage", bytes.div_ceil(1024));
    }
    if !node.relations.is_empty() {
        document.text("Relations", node.shown_relations(options.verbose));
    }
    if options.verbose && !node.remote_sql.is_empty() {
        document.text("Remote SQL", node.remote_sql.as_str());
    }
    if options.verbose
        && let Some(size) = node.batch_size
    {
        document.number("Batch Size", size);
    }
    if options.buffers {
        // Every node counts its buffers under `BUFFERS`, analyzed or not.
        structured_buffers(document, plan.buffers(at));
    }
    if let Some(actual) = actual
        && options.wal
    {
        document.number("WAL Records", actual.wal_records);
        document.number("WAL FPI", 0);
        document.number("WAL Bytes", actual.wal_bytes);
        document.number("WAL Buffers Full", 0);
    }
    // An `Append`'s members pruned at executor startup, which structured
    // formats report even when none were (`ExplainMissingMembers`).
    let members = matches!(node.name.as_str(), "Append" | "Merge Append");
    if members {
        document.number("Subplans Removed", node.subplans_removed.unwrap_or(0));
    }
    let mut next = members_end;
    let mut children = 0;
    let mut inputs = 0;
    while next < plan.count && plan.nodes[next].depth == node.depth + 1 {
        if children == 0 {
            document.open_group("Plans", Some("Plans"), false);
        }
        // A subplan is no input of its parent: its first input is the
        // outer, and an `Append`'s inputs are its members.
        let relationship = match plan.nodes[next].subplan.as_ref() {
            Some((kind, _)) => kind.relationship(),
            None if members => "Member",
            None if inputs == 0 => "Outer",
            None => "Inner",
        };
        if plan.nodes[next].subplan.is_none() {
            inputs += 1;
        }
        next = structured_node(
            document,
            plan,
            next,
            Some(relationship),
            options,
            full_actual,
        );
        children += 1;
    }
    if children != 0 {
        document.close_group("Plans", false);
    }
    document.close_group("Plan", true);
    next
}

/// `ExplainOnePlan` for one query of a structured document.
fn structured_query(
    document: &mut Document<'_>,
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
) {
    document.open_group("Query", None, true);
    structured_node(document, plan, 0, None, options, actual);
    if options.settings {
        document.open_group("Settings", Some("Settings"), true);
        for setting in settings.entries() {
            document.text(setting.name(), setting.value());
        }
        document.close_group("Settings", true);
    }
    // Planning reads no page: pos3ql's catalogs and statistics are resident.
    if options.memory || options.buffers {
        document.open_group("Planning", Some("Planning"), true);
        if options.buffers {
            structured_buffers(document, BufferUsage::default());
        }
        if options.memory {
            let (used, allocated) = plan.memory_kib();
            document.number("Memory Used", used);
            document.number("Memory Allocated", allocated);
        }
        document.close_group("Planning", true);
    }
    if options.summary {
        document.number(
            "Planning Time",
            stack_format!(32, "{:.3}", plan.planning_micros as f64 / 1_000.0),
        );
    }
    if let Some(actual) = actual {
        document.open_group("Triggers", Some("Triggers"), false);
        document.close_group("Triggers", false);
        if options.serialize != ExplainSerialize::None {
            document.open_group("Serialization", Some("Serialization"), true);
            if options.timing {
                document.number(
                    "Time",
                    stack_format!(32, "{:.3}", actual.serialization_micros as f64 / 1_000.0),
                );
            }
            document.number("Output Volume", actual.serialized_bytes.div_ceil(1024));
            document.text(
                "Format",
                if options.serialize == ExplainSerialize::Binary {
                    "binary"
                } else {
                    "text"
                },
            );
            if options.buffers {
                structured_buffers(document, BufferUsage::default());
            }
            document.close_group("Serialization", true);
        }
        if options.summary {
            document.number(
                "Execution Time",
                stack_format!(32, "{:.3}", actual.elapsed_micros as f64 / 1_000.0),
            );
        }
    }
    document.close_group("Query", true);
}

/// A structured EXPLAIN document of each query in `plans`.
fn render_document<'a>(
    plans: &[(&Plan<'_>, Option<ExplainActual>)],
    options: ExplainOptions,
    settings: &PlannerSettings,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    let mut document = Document::new(options.format, arena);
    for &(plan, actual) in plans {
        structured_query(&mut document, plan, options, actual, settings);
    }
    document.finish()
}

/// Rejects `BUFFERS` (on by default under `ANALYZE`) of a plan whose scan
/// ran on a path that does not count its pages, whose write changed rows
/// the page model does not cover, or whose hash join ran in batches, whose
/// work files are not sized, rather than print a plan missing their
/// `Buffers` lines.
fn buffers_counted(
    nodes: &[PlanNode],
    counters: &[NodeCounters],
    buffers: bool,
) -> Result<(), SqlError> {
    if !buffers {
        return Ok(());
    }
    for (node, counters) in nodes.iter().zip(counters) {
        let unpaged = if writes(node) {
            counters.unmodeled
        } else {
            counters.loops != 0 && !node.target_member && !counters.paged && accesses_pages(node)
        };
        if unpaged {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE cannot count the buffers of node \"{}\"",
                node.name.as_str()
            ));
        }
        if counters
            .hash
            .and_then(|table| table.batches)
            .is_some_and(|batches| batches.batches > 1 || batches.original_batches > 1)
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN ANALYZE cannot count the work file buffers of a hash join run in batches"
            ));
        }
    }
    Ok(())
}

/// Whether `node` writes a relation.
fn writes(node: &PlanNode) -> bool {
    matches!(node.name.as_str(), "Insert" | "Update" | "Delete" | "Merge")
}

/// Whether `node` reads a relation's pages.
fn accesses_pages(node: &PlanNode) -> bool {
    let name = node.name.as_str();
    matches!(
        name,
        "Seq Scan"
            | "Bitmap Heap Scan"
            | "Bitmap Index Scan"
            | "Tid Scan"
            | "Tid Range Scan"
            | "Sample Scan"
    ) || name.starts_with("Index Scan")
        || name.starts_with("Index Only Scan")
}

/// Rejects a plan showing costs pos3ql does not model as PostgreSQL does,
/// and a `VERBOSE` plan holding a target list outside the deparser's
/// coverage, rather than print wrong estimates or a wrong `Output:`.
fn printable(plan: &Plan<'_>, options: ExplainOptions) -> Result<(), SqlError> {
    if options.costs
        && let Some(node) = plan.nodes[..plan.count]
            .iter()
            .find(|node| node.unmodeled_costs)
    {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN cannot print the estimated costs of node \"{}\"",
            node.name.as_str()
        ));
    }
    if !options.verbose {
        return Ok(());
    }
    match plan.nodes[..plan.count]
        .iter()
        .find(|node| node.output_unprintable)
    {
        Some(node) => Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "EXPLAIN VERBOSE cannot print the output list of node \"{}\"",
            node.name.as_str()
        )),
        None => Ok(()),
    }
}

/// [`printable`] for each query a rule produces.
fn rewritten_printable(
    queries: &[RewrittenQuery<'_>],
    options: ExplainOptions,
) -> Result<(), SqlError> {
    queries.iter().try_for_each(|query| match query {
        RewrittenQuery::Planned(plan) => printable(plan, options),
        RewrittenQuery::Notify => Ok(()),
    })
}

pub(super) fn visit_plan_rows(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    arena: &Arena,
    mut emit: impl FnMut(&str) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    printable(plan, options)?;
    if options.format == ExplainFormat::Text {
        return visit_text_rows(plan, options, actual, settings, |line| emit(line));
    }
    // The document stays in the statement arena: `emit` may keep its row
    // there (a PL/pgSQL `EXECUTE ... INTO` copies it into the same arena), so
    // rewinding past it would free a value still in use.
    render_document(&[(plan, actual)], options, settings, arena).and_then(&mut emit)
}

/// An expression rewrite's failure: the arena filled, or the rewrite's own
/// error.
struct RewriteFailure(SqlError);

impl From<crate::mem::arena::ArenaFull> for RewriteFailure {
    fn from(_: crate::mem::arena::ArenaFull) -> Self {
        Self(arena_full())
    }
}

/// One query of a statement a rewrite rule changes: a planned statement,
/// or a `NOTIFY` action, which has no plan.
pub(super) enum RewrittenQuery<'a> {
    Planned(Plan<'a>),
    Notify,
}

/// The queries a rewrite rule turns a data-changing statement into, each
/// planned, in PostgreSQL's order. None when no rule applies.
pub(super) fn rewritten_queries<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    arena: &'a Arena,
) -> Result<Option<crate::mem::arena::OwnedArenaSlice<'a, RewrittenQuery<'a>>>, SqlError> {
    let original = statement;
    let Some(statements) = rewrite::rewritten_statements(storage, txid, statement, arena)? else {
        return Ok(None);
    };
    let mut queries = arena
        .alloc_owned_slice_with(statements.len(), |_| RewrittenQuery::Notify)
        .map_err(|_| arena_full())?;
    for (position, (query, statement)) in queries.iter_mut().zip(statements.iter()).enumerate() {
        if !matches!(statement, Stmt::Notify { .. }) {
            // Execution counts each query in its own block.
            let block = BlockId::ROOT.child(Segment::Query(
                u16::try_from(position).map_err(|_| arena_full())?,
            ));
            let plan = plan_statement(storage, txid, statement, block, arena)?;
            // A rule action inserting one row of expressions reads it from
            // the statement's rows: execution scans that one-row source
            // where PostgreSQL shows the action's `Result`.
            let source_result = Some(NodeKey::new(block.child(Segment::Source), Role::Result));
            if !core::ptr::eq(*statement, original)
                && matches!(statement, Stmt::Insert(_))
                && let Some(node) = plan.nodes[..plan.count]
                    .iter_mut()
                    .find(|node| node.key == source_result)
            {
                node.key = Some(NodeKey::new(block.child(Segment::Source), Role::Scan(0)));
            }
            *query = RewrittenQuery::Planned(plan);
        }
    }
    Ok(Some(queries))
}

/// Runs `execute`, the statement rewrite rules turned into `queries`,
/// counting each query's nodes in its own block as the executor starts it.
pub(super) fn analyze_rewritten<'a, R>(
    queries: &mut [RewrittenQuery<'a>],
    arena: &'a Arena,
    timing: bool,
    buffers: bool,
    execute: impl FnOnce() -> R,
) -> Result<R, SqlError> {
    let planned = |query: &RewrittenQuery<'a>| match query {
        RewrittenQuery::Planned(plan) => plan.count,
        RewrittenQuery::Notify => 0,
    };
    let total: usize = queries.iter().map(planned).sum();
    let slot_at = |mut index: usize| {
        for query in queries.iter() {
            if let RewrittenQuery::Planned(plan) = query {
                if index < plan.count {
                    let node = &plan.nodes[index];
                    return Slot::new(node.key)
                        .with_hash_columns(node.hash_columns)
                        .with_hash_agg(node.hash_agg);
                }
                index -= plan.count;
            }
        }
        Slot::new(None)
    };
    let slots = arena
        .alloc_slice_with(total, slot_at)
        .map_err(|_| query::arena_full_pub())?;
    let result = query::instrument::analyze_queries(slots, timing, buffers, execute);
    let mut start = 0;
    for query in queries.iter_mut() {
        if let RewrittenQuery::Planned(plan) = query {
            let counters = arena
                .alloc_slice_with(plan.count, |index| slots[start + index].counters())
                .map_err(|_| query::arena_full_pub())?;
            mirror_passthrough(&plan.nodes[..plan.count], counters);
            buffers_counted(&plan.nodes[..plan.count], counters, buffers)?;
            plan.measured = Some(counters);
            start += plan.count;
        }
    }
    Ok(result)
}

/// Gives each passthrough `Subquery Scan` the figures of its child, the
/// node after it, from the last such scan up.
fn mirror_passthrough(nodes: &[PlanNode], counters: &mut [NodeCounters]) {
    for index in (0..nodes.len().saturating_sub(1)).rev() {
        if nodes[index].passthrough {
            let child = counters[index + 1];
            counters[index] = NodeCounters {
                hash: None,
                sort: None,
                stored: None,
                memoize: None,
                buffers: BufferUsage::default(),
                ..child
            };
        }
    }
}

/// EXPLAIN of a statement a rewrite rule changes: each query the rule
/// produces, separated by an empty line in text. Under ANALYZE the statement
/// ran once as a whole; its time and I/O are reported on the last query.
pub(super) fn emit_rewritten(
    queries: &[RewrittenQuery<'_>],
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    arena: &Arena,
    responder: &mut Responder,
) -> Result<Result<(), SqlError>, WireFull> {
    if let Err(error) = rewritten_printable(queries, options) {
        return Ok(Err(error));
    }
    responder.row_description(&[result_column(options.format)])?;
    if let Err(error) = visit_rewritten_rows(queries, options, actual, settings, arena, |line| {
        responder.data_row(&[Datum::Text(line)])
    })? {
        return Ok(Err(error));
    }
    responder.command_complete("EXPLAIN")?;
    Ok(Ok(()))
}

/// The rows of [`emit_rewritten`]'s result, each passed to `emit`.
pub(super) fn visit_rewritten_rows<E>(
    queries: &[RewrittenQuery<'_>],
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    arena: &Arena,
    mut emit: impl FnMut(&str) -> Result<(), E>,
) -> Result<Result<(), SqlError>, E> {
    if let Err(error) = rewritten_printable(queries, options) {
        return Ok(Err(error));
    }
    let last = queries.len().saturating_sub(1);
    let query_actual = |index: usize| {
        actual.map(|actual| {
            if index == last {
                ExplainActual { rows: 0, ..actual }
            } else {
                ExplainActual {
                    rows: 0,
                    elapsed_micros: 0,
                    serialized_bytes: 0,
                    serialization_micros: 0,
                    wal_records: 0,
                    wal_bytes: 0,
                }
            }
        })
    };
    if options.format == ExplainFormat::Text {
        for (index, query) in queries.iter().enumerate() {
            if index != 0 {
                emit("")?;
            }
            match query {
                RewrittenQuery::Notify => emit("NOTIFY")?,
                RewrittenQuery::Planned(plan) => {
                    visit_text_rows(plan, options, query_actual(index), settings, &mut emit)?;
                }
            }
        }
        return Ok(Ok(()));
    }
    let mut document = Document::new(options.format, arena);
    for (index, query) in queries.iter().enumerate() {
        match query {
            RewrittenQuery::Notify => document.dummy_group("Notify"),
            RewrittenQuery::Planned(plan) => {
                structured_query(&mut document, plan, options, query_actual(index), settings);
            }
        }
    }
    match document.finish() {
        Ok(text) => emit(text).map(Ok),
        Err(error) => Ok(Err(error)),
    }
}

/// EXPLAIN's result column: `json` and `xml` documents carry their types,
/// as PostgreSQL's `ExplainResultDesc` declares them.
pub(super) const fn result_column(format: ExplainFormat) -> ColDesc<'static> {
    ColDesc::new(
        "QUERY PLAN",
        match format {
            ExplainFormat::Json => oid::JSON,
            ExplainFormat::Xml => oid::XML,
            ExplainFormat::Text | ExplainFormat::Yaml => oid::TEXT,
        },
        -1,
    )
}

pub(super) fn emit_plan(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    arena: &Arena,
    responder: &mut Responder,
) -> Result<Result<(), SqlError>, WireFull> {
    if let Err(error) = printable(plan, options) {
        return Ok(Err(error));
    }
    if options.format == ExplainFormat::Text {
        emit_text(plan, options, actual, settings, responder)?;
        return Ok(Ok(()));
    }
    let document = match render_document(&[(plan, actual)], options, settings, arena) {
        Ok(document) => document,
        Err(error) => return Ok(Err(error)),
    };
    responder.row_description(&[result_column(options.format)])?;
    responder.data_row(&[Datum::Text(document)])?;
    responder.command_complete("EXPLAIN")?;
    Ok(Ok(()))
}

#[cfg(all(
    test,
    not(all(miri, feature = "miri-ownership")),
    not(feature = "sql-fixture-suite")
))]
mod tests {
    use super::{PlanNode, PostgresHashTable, object_request_cost, query, structured_node_name};
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

    #[test]
    fn hash_tables_size_buckets_as_postgresql() {
        let table = |rows, tuples| {
            PostgresHashTable::of(
                &PlanNode {
                    rows,
                    ..PlanNode::EMPTY
                },
                query::instrument::HashTable {
                    tuples,
                    tuple_bytes: Some(1000),
                    own_bytes: 7,
                    batches: None,
                },
            )
        };
        // At least 1024 buckets, doubled once the rows before an insert
        // outnumber them.
        let small = table(15, 1025);
        assert_eq!((small.buckets, small.original_buckets), (1024, 1024));
        assert_eq!(small.bytes, 1000 + 1024 * 8);
        let grown = table(15, 3000);
        assert_eq!((grown.buckets, grown.original_buckets), (4096, 1024));
        let estimated = table(1500, 1500);
        assert_eq!(
            (estimated.buckets, estimated.original_buckets),
            (2048, 2048)
        );
        // A relation with no estimate is taken as 1000 rows.
        assert_eq!(table(0, 1).original_buckets, 1024);
    }
}
