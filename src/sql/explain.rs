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
use crate::sql::query::instrument::{BlockId, NodeCounters, NodeKey, Role, Segment, Slot};
use crate::sql::query::{self, QueryScope};
use crate::sql::types::{ColDesc, Datum, oid};
use crate::sql_err;
use crate::stack_format;
use crate::storage::Storage;
use crate::store::BlockIoStats;
use crate::util::StackStr;

mod rewrite;

const MAX_EXPLAIN_STAGES: usize = 24;

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
    /// The name the query gives the relation, when it differs from it.
    alias: StackStr<96>,
    output: StackStr<256>,
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
}

impl PlanKeys {
    const NONE: Self = Self {
        label: KeyLabel::None,
        plain: StackStr::new(),
        verbose: StackStr::new(),
    };

    /// The label and items to print, if any.
    fn shown(&self, verbose: bool) -> Option<(&'static str, impl Iterator<Item = &str>)> {
        let label = match self.label {
            KeyLabel::None => return None,
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
    IndexCond,
    RecheckCond,
    JoinFilter,
    /// A `WindowAgg`'s window definition.
    Window,
    /// An incremental sort's keys its input is already sorted by.
    PresortedKey,
}

impl QualLabel {
    /// PostgreSQL's EXPLAIN shows a join's or an index's conditions before
    /// its filter.
    const fn rank(self) -> u8 {
        match self {
            Self::Window
            | Self::PresortedKey
            | Self::OneTimeFilter
            | Self::HashCond
            | Self::IndexCond
            | Self::RecheckCond => 0,
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
            Self::IndexCond => "Index Cond",
            Self::RecheckCond => "Recheck Cond",
            Self::JoinFilter => "Join Filter",
            Self::Window => "Window",
            Self::PresortedKey => "Presorted Key",
        }
    }
}

impl PlanNode {
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
        self.quals
            .iter()
            .filter(|(label, _)| *label != QualLabel::None)
            .map(move |(label, qual)| {
                let shown = self
                    .verbose_quals
                    .iter()
                    .find(|(held, _)| verbose && held == label)
                    .map_or(qual.as_str(), |(_, verbose)| verbose.as_str());
                (*label, shown)
            })
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
        verbose_quals: NO_QUALS,
        schema: StackStr::new(),
        subplan: None,
        shift: 0,
    };
}

/// What `EXPLAIN ANALYZE` reports for one node.
#[derive(Clone, Copy)]
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
            | QualLabel::Window
            | QualLabel::PresortedKey
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
            | Self::Window
            | Self::PresortedKey
            | Self::None => None,
        }
    }
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
    /// Per node, what it did while `EXPLAIN ANALYZE` ran the statement.
    measured: Option<&'a [NodeCounters]>,
    planning_micros: u64,
    /// The plan methods enabled while planning, which mark nodes that had
    /// to use a disabled one.
    methods: crate::sql::guc::PlannerMethods,
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
    /// of each node that has a key; `timing` also measures its time.
    pub(super) fn analyze<R>(
        &mut self,
        arena: &'a Arena,
        timing: bool,
        execute: impl FnOnce() -> R,
    ) -> Result<R, SqlError> {
        let nodes = &self.nodes[..self.count];
        let slots = arena
            .alloc_slice_with(self.count, |index| Slot::new(nodes[index].key))
            .map_err(|_| query::arena_full_pub())?;
        let result = query::instrument::analyze(slots, timing, execute);
        let counters = arena
            .alloc_slice_with(self.count, |index| slots[index].counters())
            .map_err(|_| query::arena_full_pub())?;
        self.measured = Some(counters);
        Ok(result)
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
            .filter(|_| node.key.is_some())
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
        let plan = if correlated {
            crate::sql::catalog::deparse::SublinkPlan::SubPlan(number)
        } else {
            crate::sql::catalog::deparse::SublinkPlan::InitPlan(number)
        };
        subplans.0[count] = (core::ptr::from_ref(select) as usize, plan);
        subplans.1 = count + 1;
        Ok(plan)
    })
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

/// The query of a subquery EXPLAIN can show as a subplan.
fn sublink_query<'e>(expression: &'e Expr<'e>) -> Option<&'e Select<'e>> {
    match expression {
        Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => Some(select),
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

/// `condition` split into its conjuncts that read no column and only
/// subplans run once, which PostgreSQL checks once, and the rest.
fn split_gating<'e>(
    condition: Option<&'e Expr<'e>>,
    arena: &'e Arena,
) -> Result<(Option<&'e Expr<'e>>, Option<&'e Expr<'e>>), SqlError> {
    let Some(condition) = condition else {
        return Ok((None, None));
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none() {
        return Ok((None, Some(condition)));
    }
    let (mut gating, mut rest) = (None, None);
    for conjunct in &conjuncts[..count] {
        let mut columns = false;
        conjunct.for_each_column_reference(&mut |_, _| columns = true);
        if !columns && conjunct.contains_subquery() && sublinks_registered(conjunct, false) {
            gating = query::and_where(gating, Some(conjunct), arena)?;
        } else {
            rest = query::and_where(rest, Some(conjunct), arena)?;
        }
    }
    Ok((gating, rest))
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

/// The qualifications PostgreSQL attaches to a left-deep join of relations:
/// each relation's own conjuncts as its scan's filter, and each conjunct
/// reading several relations at the join that adds the last of them, as the
/// join's hash condition (equalities, under a hash join of two relations) or
/// join filter. None when a conjunct cannot be placed.
struct JoinQuals<'a> {
    /// Per relation, in the relations' order.
    filters: &'a mut [Option<StackStr<512>>],
    hash_cond: Option<StackStr<512>>,
    /// Per join, indexed by the depth of the relation it adds (`0` unused).
    join_filters: &'a mut [Option<StackStr<512>>],
}

/// `relations` are named as the query exposes them; `order` lists them in
/// the order the joins add them.
#[allow(clippy::too_many_arguments)]
fn join_quals<'a>(
    storage: &Storage,
    txid: u32,
    relations: &[(&'a str, &'a crate::storage::TableDef)],
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
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none() {
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
        arena,
    )? {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(count_relations, |index| {
            crate::sql::catalog::deparse::Joined::new(relations[index].0, relations[index].1)
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
    hash_cond: StackStr<512>,
    join_filter: Option<StackStr<512>>,
    output_filter: Option<StackStr<512>>,
}

#[allow(clippy::too_many_arguments)]
fn outer_hash_quals<'a>(
    storage: &Storage,
    txid: u32,
    relations: &[(&'a str, &'a crate::storage::TableDef)],
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
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none() {
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
        if split_conjuncts(where_clause, &mut conjuncts, &mut count).is_none() {
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
    if !self::scan_filters(
        storage,
        txid,
        relations,
        own,
        own_count,
        moved,
        moved_count,
        &mut scan_filters,
        arena,
    )? {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(relations.len(), |index| {
            crate::sql::catalog::deparse::Joined::new(relations[index].0, relations[index].1)
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
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none() {
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
    storage: &Storage,
    txid: u32,
    relations: &[(&'a str, &'a crate::storage::TableDef)],
    own: &mut [&'a Expr<'a>],
    own_count: &[usize],
    moved: &[&'a Expr<'a>],
    moved_count: &[usize],
    filters: &mut [Option<StackStr<512>>],
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    for (index, (exposed, table)) in relations.iter().enumerate() {
        let own = &mut own[index * MAX_CONJUNCTS..index * MAX_CONJUNCTS + own_count[index]];
        order_restrictions(own);
        let moved = &moved[index * MAX_CONJUNCTS..index * MAX_CONJUNCTS + moved_count[index]];
        filters[index] = if moved.is_empty() {
            let joined = [crate::sql::catalog::deparse::Joined::new(exposed, table)];
            let context = crate::sql::catalog::deparse::Context {
                storage,
                txid,
                table: Some(table),
                parameters: &[],
                joined: &joined,
                pretty: false,
                qualify: false,
                bare: None,
                domain_value: None,
                sublinks: Some(&registered_sublink),
                folded: true,
                arena,
                dependencies: None,
            };
            deparse_qual(&context, own, arena)?
        } else {
            let all = arena
                .alloc_slice_with(own.len() + moved.len(), |position| {
                    own.get(position)
                        .copied()
                        .unwrap_or_else(|| moved[position - own.len()])
                })
                .map_err(|_| arena_full())?;
            let joined = arena
                .alloc_slice_with(relations.len(), |index| {
                    crate::sql::catalog::deparse::Joined::new(
                        relations[index].0,
                        relations[index].1,
                    )
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
                bare: Some(exposed),
                domain_value: None,
                sublinks: Some(&registered_sublink),
                folded: true,
                arena,
                dependencies: None,
            };
            deparse_qual(&context, all, arena)?
        };
        if (!own.is_empty() || !moved.is_empty()) && filters[index].is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The relation of `relations` a column reference names, when exactly one
/// has it.
fn resolve_relation(
    relations: &[(&str, &crate::storage::TableDef)],
    qualifier: Option<&str>,
    name: &str,
) -> Option<usize> {
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
        None => {
            let mut found = relations
                .iter()
                .enumerate()
                .filter(|(_, (_, table))| has(table))
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
    storage: &Storage,
    txid: u32,
    relations: &[(&'a str, &'a crate::storage::TableDef)],
    joins: &'a [crate::sql::ast::Join<'a>],
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
        let source = if depth == 0 {
            condition
        } else {
            joins[depth - 1].on
        };
        let Some(source) = source else {
            continue;
        };
        let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
        let mut conjunct_count = 0;
        if split_conjuncts(source, &mut conjuncts, &mut conjunct_count).is_none() {
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
                    (JoinKind::Left, Some(relation)) => relation == depth,
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
        arena,
    )? {
        return Ok(None);
    }
    let joined = arena
        .alloc_slice_with(count, |index| {
            crate::sql::catalog::deparse::Joined::new(relations[index].0, relations[index].1)
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
    relations: &[(&'a str, &'a crate::storage::TableDef)],
    indexes: &[Option<query::IndexAccessPlan<'a>>],
    conditions: &mut [Option<StackStr<512>>],
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let joined = arena
        .alloc_slice_with(relations.len(), |index| {
            crate::sql::catalog::deparse::Joined::new(relations[index].0, relations[index].1)
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
        folded: true,
        arena,
        dependencies: None,
    };
    for ((index, condition), (exposed, _)) in
        indexes.iter().zip(conditions.iter_mut()).zip(relations)
    {
        if let Some(index) = index {
            *condition = index_condition(&context, exposed, index, false, arena)?;
        }
    }
    Ok(())
}

/// [`outer_join_quals`]' placement, per relation and per join depth.
struct OuterJoinQuals<'a> {
    filters: &'a mut [Option<StackStr<512>>],
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
        qualify: relations > 1,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        folded: true,
        bare: None,
    };
    let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
    let mut count = 0;
    if split_conjuncts(having, &mut conjuncts, &mut count).is_none() {
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
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none()
        || conjuncts[..count]
            .iter()
            .any(|conjunct| relation_mask(conjunct, &resolve).is_none())
    {
        return Ok(None);
    }
    order_restrictions(&mut conjuncts[..count]);
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
        folded: true,
        bare: None,
    };
    deparse_qual(&context, &conjuncts[..count], arena)
}

#[allow(clippy::too_many_arguments)]
fn single_relation_quals<'a>(
    storage: &Storage,
    txid: u32,
    exposed: &'a str,
    table: &'a crate::storage::TableDef,
    condition: Option<&'a Expr<'a>>,
    index: Option<&query::IndexAccessPlan<'a>>,
    verbose: bool,
    arena: &'a Arena,
) -> Result<(Option<StackStr<512>>, Option<StackStr<512>>), SqlError> {
    let Some(condition) = condition else {
        return Ok((None, None));
    };
    let resolve = |qualifier: Option<&str>, name: &str| -> Option<usize> {
        if qualifier.is_none_or(|qualifier| qualifier == exposed)
            && table
                .columns()
                .iter()
                .any(|column| column.name.as_str() == name)
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
    if split_conjuncts(condition, &mut conjuncts, &mut count).is_none()
        || !conjuncts[..count]
            .iter()
            .all(|conjunct| placeable(conjunct))
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
    order_restrictions(&mut conjuncts[..kept]);
    // The enclosing query's relation, whose columns print qualified.
    let outer = OUTER_RELATIONS.with(|outer| {
        let outer = outer.borrow();
        outer.1.checked_sub(1).map(|top| outer.0[top])
    });
    let outer_definition = outer.map(|(_, slot)| storage.table_def(slot, txid));
    let outer_name = match outer.as_ref() {
        Some((name, _)) => Some(arena.alloc_str(name.as_str()).map_err(|_| arena_full())?),
        None => None,
    };
    let own = [crate::sql::catalog::deparse::Joined::new(exposed, table)];
    let with_outer;
    let joined: &[crate::sql::catalog::deparse::Joined<'_>] =
        match (outer_name, outer_definition.as_deref()) {
            (Some(name), Some(definition)) => {
                with_outer = [
                    own[0],
                    crate::sql::catalog::deparse::Joined::new(name, definition),
                ];
                &with_outer
            }
            _ => &own,
        };
    let outer_count = usize::from(outer.is_some());
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: Some(table),
        parameters: &[],
        joined,
        pretty: false,
        qualify: verbose,
        domain_value: None,
        sublinks: Some(&registered_sublink),
        folded: true,
        bare: (outer_count != 0 && !verbose).then_some(exposed),
    };
    let index_cond = match index {
        Some(index) => index_condition(&context, exposed, index, verbose, arena)?,
        None => None,
    };
    Ok((
        index_cond,
        deparse_qual(&context, &conjuncts[..kept], arena)?,
    ))
}

/// Pushes `scan`; a bitmap heap scan rechecks the index condition its
/// bitmap index scan child evaluates.
fn push_scan(plan: &mut Plan<'_>, mut scan: PlanNode) -> Result<(), SqlError> {
    let bitmap_index = scan.bitmap_index;
    let bitmap_depth = scan.depth.saturating_add(1);
    let bitmap_rows = scan.rows;
    let bitmap_cost = scan.total_cost * 0.4;
    let mut index_cond = None;
    let mut verbose_index_cond = None;
    if !bitmap_index.as_str().is_empty()
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
    plan.push(scan)?;
    if !bitmap_index.as_str().is_empty() {
        let mut name = StackStr::new();
        let _ = write!(name, "Bitmap Index Scan on {}", bitmap_index.as_str());
        plan.push(
            PlanNode {
                name,
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
    for (source, key, operator, operand) in index.index_conditions() {
        if count == MAX_CONJUNCTS {
            return Ok(None);
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
    index.is_some_and(|index| {
        index
            .index_conditions()
            .any(|(source, ..)| core::ptr::eq(source, conjunct))
    })
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
) -> Result<(PlanNode, Option<query::IndexAccessPlan<'a>>), SqlError> {
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
    match scope.defs.stored(table).filter(|_| !derived) {
        Some(definition) => {
            let _ = write!(relation, "{}", definition.name.as_str());
            let _ = write!(schema, "{}", definition.schema.as_str());
            if scope.names[table] != definition.name.as_str() {
                let _ = write!(alias, "{}", scope.names[table]);
            }
        }
        None => {
            let _ = write!(alias, "{}", scope.names[table]);
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
        verbose_quals: NO_QUALS,
        schema,
        subplan: None,
        shift: 0,
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
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    // Execution plans the same query: simple subqueries merged into it and
    // outer joins its conditions narrow reduced.
    let pulled = query::pull_up_subqueries(statement, storage, txid, arena, None)?;
    // PostgreSQL plans, and so numbers, a query's subqueries before it
    // prints the conditions that refer to them.
    let sublinks = plan_sublinks(storage, txid, pulled, block, arena)?;
    let plan = plan_pulled_select(storage, txid, statement, pulled, block, arena)?;
    attach_subplans(plan, pulled, block, sublinks, arena)
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
}

thread_local! {
    /// How many subqueries deep the query being planned is: a subquery's
    /// range table entries join its statement's.
    static SUBLINK_DEPTH: core::cell::Cell<u16> = const { core::cell::Cell::new(0) };
    /// The single stored relation of each query level around the one being
    /// planned, by exposed name and slot, which a correlated subquery's
    /// conditions name qualified.
    static OUTER_RELATIONS: core::cell::RefCell<([(StackStr<64>, usize); 8], usize)> =
        const { core::cell::RefCell::new(([(StackStr::new(), 0); 8], 0)) };
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
    block: BlockId,
    arena: &'a Arena,
) -> Result<&'a [Sublink<'a>], SqlError> {
    let mut found = crate::mem::arena::ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            visit_sublinks(expression, &mut |node| {
                let _ = found.push((node, SublinkOrigin::Items));
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
        let inner = plan_select(storage, txid, select, block.child(Segment::Sublink), arena);
        SUBLINK_DEPTH.with(|depth| depth.set(depth.get() - 1));
        if pushed {
            OUTER_RELATIONS.with(|outer| outer.borrow_mut().1 -= 1);
        }
        let Plan { nodes, count, .. } = inner?;
        // Execution does not count a subquery's operators per subplan yet,
        // so `EXPLAIN ANALYZE` prints no figures for them.
        for node in &mut nodes[..count] {
            node.key = None;
        }
        let nodes: &'a [PlanNode] = &nodes[..count];
        let number = match register_subplan(select, correlated)? {
            crate::sql::catalog::deparse::SublinkPlan::InitPlan(number)
            | crate::sql::catalog::deparse::SublinkPlan::SubPlan(number) => number,
        };
        planned
            .push(Sublink {
                kind: if correlated {
                    SubplanKind::SubPlan
                } else {
                    SubplanKind::InitPlan
                },
                number,
                origin,
                nodes,
            })
            .map_err(|_| arena_full())?;
    }
    Ok(planned.into_mut_slice().map_err(|_| arena_full())?)
}

/// `plan` with each subplan under the node that runs it: an InitPlan under
/// the query's top node, before its inputs, and a SubPlan after the inputs
/// of the node evaluating the expression that reads it.
fn attach_subplans<'a>(
    plan: Plan<'a>,
    statement: &Select<'_>,
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
    let relations = statement
        .from
        .as_ref()
        .map_or(0, |from| from.joins.len() + 1);
    let scan = || {
        if relations == 1 {
            find(Role::Scan(0))
        } else {
            find(Role::Join(u16::try_from(relations - 1).unwrap_or(u16::MAX)))
        }
    };
    let host = |sublink: &Sublink<'_>| -> usize {
        let at = match (sublink.kind, sublink.origin) {
            (SubplanKind::InitPlan, _) => None,
            (SubplanKind::SubPlan, SublinkOrigin::Having) => find(Role::Aggregate),
            (SubplanKind::SubPlan, SublinkOrigin::Items) => find(Role::Aggregate)
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
            let mut name = StackStr::<72>::new();
            let _ = write!(name, "{} {}", sublink.kind.relationship(), sublink.number);
            attached.push(PlanNode {
                depth: node.depth + parent.depth + 1,
                shift: node.shift + parent.shift + 1,
                subplan: if at == 0 {
                    Some((sublink.kind, name))
                } else {
                    node.subplan
                },
                alias: renamed[offsets[index] + at].unwrap_or(node.alias),
                ..*node
            })?;
        }
        Ok::<(), SqlError>(())
    };
    for (index, node) in nodes.iter().enumerate() {
        attached.push(*node)?;
        // InitPlans precede the inputs of the top node.
        if index == 0 {
            for (at, _) in sublinks
                .iter()
                .enumerate()
                .filter(|(_, sublink)| sublink.kind == SubplanKind::InitPlan)
            {
                push_subplan(&mut attached, *node, at)?;
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
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let key = |role| Some(NodeKey::new(block, role));
    let started = std::time::Instant::now();
    let written = statement;
    let statement = pulled;
    let scope = match &statement.from {
        Some(from) => Some(QueryScope::resolve_schema(
            storage, from, txid, arena, None,
        )?),
        None => None,
    };
    if let Some(scope) = scope.as_ref() {
        query::validate_static_grouping(statement, scope, storage, txid, arena)?;
    }
    let capacity = scope
        .as_ref()
        .map_or(MAX_EXPLAIN_STAGES, |scope| scope.n * 3 + MAX_EXPLAIN_STAGES);
    let mut plan = Plan::new(arena, capacity)?;
    let (width, mut output) = projected_shape(statement, scope.as_ref(), storage, txid, arena)?;
    if let (Some(scope), Some(from)) = (scope.as_ref(), statement.from.as_ref())
        && scope.n == 1
        && let Some(list) = from.base.subquery.and_then(values_list)
        && let Some(named) = values_output(statement, scope, list)
    {
        output = named;
    }
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

    // A condition the planner folds to false leaves no relation to scan:
    // PostgreSQL runs the query's upper nodes over a `Result` that returns
    // no rows.
    let constant_false = statement
        .where_clause
        .and_then(|predicate| query::plan_time_bool(predicate, arena))
        == Some(false);

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
                ..PlanNode::EMPTY
            },
        )?;
    }

    // A FROM-less query grouped by constants groups its one row.
    let constant_group = fromless && constant_keys(statement.group_by)?;
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
                } else if constant_group {
                    "Group"
                } else {
                    "HashAggregate"
                }),
                key: Some(NodeKey::new(aggregated_block, Role::Aggregate)),
                keys: match group_keys(statement, arena)?
                    .filter(|_| !statement.group_by.is_empty() && !constant_group)
                {
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
            .with_qual(
                QualLabel::Filter,
                having_filter(storage, txid, scope.as_ref(), statement.having, arena)?,
            ),
        )?;
    }
    // The keys the rows leave the last window sorted by.
    let mut window_sorted: &[crate::sql::ast::OrderBy<'_>] = &[];
    if has_window(statement) {
        total_cost += estimated_rows as f64 * 0.02;
        let (nodes, sorted) = window_stages(
            storage,
            txid,
            scope.as_ref(),
            statement,
            columns,
            block,
            arena,
        )?;
        window_sorted = sorted;
        for node in nodes {
            add_stage(
                &mut stages,
                &mut stage_count,
                PlanNode {
                    total_cost,
                    rows: estimated_rows,
                    width,
                    object_requests,
                    cache_blocks,
                    ..*node
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
    } else {
        order_keys(statement, arena)?
    };
    // Rows a window leaves sorted by the query's keys need no sort, and
    // ones sorted by a prefix of them only an incremental one.
    let presorted = presorted_order_keys(statement, sorted_keys, window_sorted);
    if !sorted_keys.is_empty()
        && ordered_index.is_none()
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
                    if statement.order_by.is_empty() && statement.distinct_on.is_empty() {
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
            },
        )?;
    }
    // A locking clause locks each row below the limit, as PostgreSQL's
    // `LockRows` does; a query without FROM has no rows to lock.
    if !statement.locking.is_empty() && !fromless {
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
    if statement.limit.is_some() || statement.offset.is_some() {
        estimated_rows = estimated_rows.min(100);
        add_stage(
            &mut stages,
            &mut stage_count,
            PlanNode {
                name: StackStr::from_str("Limit"),
                key: key(Role::Limit),
                total_cost,
                rows: estimated_rows,
                width,
                object_requests,
                cache_blocks,
                ..PlanNode::EMPTY
            },
        )?;
    }

    let n = scope.as_ref().map_or(0, |scope| scope.n);
    // The WHERE conjuncts PostgreSQL attaches to each scan and to each join,
    // when every conjunct can be placed and printed.
    // A single relation's index condition and filter as `VERBOSE` prints
    // them.
    let mut verbose_quals: (Option<StackStr<512>>, Option<StackStr<512>>) = (None, None);
    let index_conds = arena
        .alloc_slice_with(n.max(1), |_| None::<StackStr<512>>)
        .map_err(|_| arena_full())?;
    let filters = arena
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
                    join.kind.is_cross()
                        || (join.kind == crate::sql::ast::JoinKind::Inner
                            && join.using.is_none()
                            && !join.natural)
                }) && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
            })
    });
    let mut join_condition = statement.where_clause;
    if inner_joins && let Some(from) = statement.from.as_ref() {
        for join in from.joins {
            join_condition = query::and_where(join_condition, join.on, arena)?;
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
        if scope.n == 1
            && let Some((exposed, def)) = relation(0)
        {
            // Conjuncts that read only subplans run once gate the scan
            // from a `Result` above it, as PostgreSQL's pseudoconstant
            // quals do.
            let (gating, scanned) = split_gating(statement.where_clause, arena)?;
            if let Some(gating) = gating {
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
                    ..PlanNode::EMPTY
                }
                .with_qual(QualLabel::OneTimeFilter, filter);
                stage_count += 1;
            }
            (index_conds[0], filters[0]) = single_relation_quals(
                storage,
                txid,
                exposed,
                def,
                scanned,
                scan_indexes[0].as_ref(),
                false,
                arena,
            )?;
            verbose_quals = single_relation_quals(
                storage,
                txid,
                exposed,
                def,
                scanned,
                scan_indexes[0].as_ref(),
                true,
                arena,
            )?;
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
        } else if inner_joins && (0..scope.n).all(|table| relation(table).is_some()) {
            let relations = arena
                .alloc_slice_with(scope.n, |table| relation(table).expect("stored relation"))
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
                join_filters.copy_from_slice(quals.join_filters);
                stages[0] = stages[0].with_qual(QualLabel::HashCond, quals.hash_cond);
            }
            joined_index_conditions(storage, txid, relations, scan_indexes, index_conds, arena)?;
        } else if scope.n >= 2
            && let Some(from) = statement.from.as_ref()
            && from
                .joins
                .iter()
                .all(|join| join.using.is_none() && !join.natural)
            && !scope.lateral[..scope.n].iter().any(|&lateral| lateral)
            && table_order[..scope.n]
                .iter()
                .enumerate()
                .all(|(position, &table)| position == table)
            && (0..scope.n).all(|table| relation(table).is_some())
        {
            let relations = arena
                .alloc_slice_with(scope.n, |table| relation(table).expect("stored relation"))
                .map_err(|_| arena_full())?;
            if let Some(quals) = outer_join_quals(
                storage,
                txid,
                relations,
                from.joins,
                statement.where_clause,
                hash_join,
                scan_indexes,
                used_parameterized,
                arena,
            )? {
                filters.copy_from_slice(quals.filters);
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
            )
            && let Some(condition) = join.on.or(scope.join_on[0])
            && let (Some(first), Some(second)) = (relation(0), relation(1))
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
        }
    }
    if constant_false {
        for (depth, stage) in stages[..stage_count].iter().rev().enumerate() {
            plan.push(PlanNode {
                depth: depth as u8,
                output,
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
    for (depth, stage) in stages[..stage_count].iter().rev().enumerate() {
        let mut stage = *stage;
        stage.depth = depth as u8;
        stage.output = output;
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
                    output,
                    ..PlanNode::EMPTY
                }
                .with_qual(QualLabel::JoinFilter, join_filters[position].take())
                .with_qual(QualLabel::Filter, post_filters[position].take()),
            )?;
        }
        for (position, &table) in table_order[..scope.n].iter().enumerate() {
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
            )?;
            scan.output = output;
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
            }
            let position_role = u16::try_from(table).map_err(|_| arena_full())?;
            scan.key = key(Role::Scan(position_role));
            let sequential =
                scan.name.as_str() == "Seq Scan" && crate::sql::guc::planner_methods().material;
            scan = scan
                .with_qual(QualLabel::IndexCond, index_conds[table].take())
                .with_qual(QualLabel::Filter, filters[table].take());
            if n == 1 {
                scan = scan
                    .with_verbose_qual(QualLabel::IndexCond, verbose_quals.0.take())
                    .with_verbose_qual(QualLabel::Filter, verbose_quals.1.take());
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
        let items = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
        for (table, item) in items.enumerate().take(scope.n) {
            let Some(body) = item.subquery.filter(|body| values_list(body).is_none()) else {
                continue;
            };
            // Locked rows carry their row identities out of the subquery,
            // so PostgreSQL keeps its scan.
            let passes_through = from.joins.is_empty()
                && body.locking.is_empty()
                && statement
                    .items
                    .iter()
                    .all(|item| matches!(item, SelectItem::Wildcard(_)));
            let derived_block = block.child(Segment::Derived(
                u16::try_from(table).map_err(|_| arena_full())?,
            ));
            plan = with_derived_plan(
                plan,
                scope.names[table],
                passes_through,
                plan_derived(storage, txid, body, derived_block, arena)?,
                arena,
            )?;
        }
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
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

/// PostgreSQL's window functions whose result does not depend on the frame,
/// for which its planner evaluates the window `ROWS UNBOUNDED PRECEDING`.
fn frame_independent(node: &Expr<'_>) -> bool {
    let Expr::Call { name, .. } = node else {
        return false;
    };
    [
        "row_number",
        "rank",
        "dense_rank",
        "percent_rank",
        "cume_dist",
        "ntile",
    ]
    .iter()
    .any(|function| name.eq_ignore_ascii_case(function))
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

/// The `WindowAgg` nodes of a query and the sorts of their inputs, bottom
/// first, and the keys the last window leaves its rows sorted by. Each
/// window clause sorts its input unless the rows already follow its keys,
/// incrementally when they follow a prefix of them, as PostgreSQL plans.
fn window_stages<'a, 'e>(
    storage: &Storage,
    txid: u32,
    scope: Option<&QueryScope<'_>>,
    statement: &'e Select<'e>,
    columns: &dyn crate::sql::exec::ColTypeResolver,
    block: BlockId,
    arena: &'e Arena,
) -> Result<(&'e [PlanNode], &'e [crate::sql::ast::OrderBy<'e>]), SqlError> {
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
    let mut sorted: &[crate::sql::ast::OrderBy<'e>] = &[];
    // Unnamed windows take the names `w1`, `w2`, ... the named ones leave.
    let mut next_name = 1usize;
    let last = windows.clauses.len().saturating_sub(1);
    for (index, &(spec, name)) in windows.clauses.iter().enumerate() {
        let position = u16::try_from(index).map_err(|_| arena_full())?;
        let keys = query::window_sort_keys(spec, arena)?;
        let held = keys.len() <= sorted.len() && *keys == sorted[..keys.len()];
        if !keys.is_empty() && !held {
            let presorted = keys
                .iter()
                .zip(sorted)
                .take_while(|(key, held)| key == held)
                .count();
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
                ..PlanNode::EMPTY
            };
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
        let optimized_frame = win_nodes
            .iter()
            .zip(windows.of_node)
            .filter(|(_, clause)| **clause == index)
            .all(|(node, _)| frame_independent(node));
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
            ..PlanNode::EMPTY
        }
        .with_qual(QualLabel::Window, definition(false)?)
        .with_verbose_qual(QualLabel::Window, definition(true)?);
        count += 1;
    }
    Ok((&nodes[..count], sorted))
}

/// The plan of a derived table's own query.
fn plan_derived<'a>(
    storage: &Storage,
    txid: u32,
    body: &Select<'_>,
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
            block,
            arena,
        ),
        None if body.with.is_empty() => plan_select(storage, txid, body, block, arena),
        None => plan_select(
            storage,
            txid,
            query::expand_ctes(body, storage, txid, arena)?,
            block,
            arena,
        ),
    }
}

/// `plan` with `derived`, the plan of the derived table exposed as `name`,
/// under its `Subquery Scan`. PostgreSQL drops a scan that neither filters
/// nor reshapes the rows (`SELECT * FROM (...) s`) and runs the derived
/// plan in its place.
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
    let mut expanded = Plan::new(arena, plan.count + derived.count)?;
    expanded.methods = plan.methods;
    for node in &plan.nodes[..at] {
        expanded.push(*node)?;
    }
    let depth = if replaced {
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
    // A query without FROM reads no relation.
    let relations = scope.map_or(0, |scope| scope.n);
    if keys.is_empty() {
        return Ok(PlanKeys::NONE);
    }
    let Some(joined) = key_relations(scope, statement, arena)? else {
        return Ok(PlanKeys::NONE);
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
            let mark = arena.mark();
            let part = crate::sql::catalog::deparse::deparse_expression(
                &context,
                expression,
                crate::sql::catalog::deparse::Destination::Value,
                arena,
            );
            let written = part.map(|part| {
                part.map(|part| {
                    if index != 0 {
                        let _ = text.write_char(KEY_SEPARATOR);
                    }
                    // A grouping key prints as written; a sort key reads its
                    // expression from the input's output, which PostgreSQL
                    // parenthesizes.
                    if matches!(expression, Expr::Column { .. })
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
    if SUBLINK_DEPTH.with(core::cell::Cell::get) != 0 {
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
        if !(values || (item.is_function_source() && item.rows_from.is_none())) {
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
        (Some(crate::sql::ast::JoinKind::Left), true) => "Hash Left Join",
        (Some(crate::sql::ast::JoinKind::Left), false) => "Nested Loop Left Join",
        // The executor keeps each preserved row that found no match.
        (Some(crate::sql::ast::JoinKind::Right), false) => "Nested Loop Right Join",
        (Some(crate::sql::ast::JoinKind::Full), false) => "Nested Loop Full Join",
        (_, true) => "Hash Join",
        (_, false) => "Nested Loop",
    }
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
        verbose_quals: NO_QUALS,
        schema: StackStr::from_str(storage.table_def(slot, txid).schema.as_str()),
        subplan: None,
        shift: 0,
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
            let leaf = plan_select(storage, txid, select, branch, arena)?;
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
                plan, storage, txid, left, arena, depth, block, numbers, true,
            )?;
            let right = push_set_tree(
                plan, storage, txid, right, arena, depth, block, numbers, true,
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
                verbose_quals: NO_QUALS,
                schema: StackStr::new(),
                subplan: None,
                shift: 0,
            };
            plan.nodes[at] = node;
            if hashed_union {
                let input = &mut plan.nodes[at + 1];
                input.rows = left.rows.saturating_add(right.rows);
                input.width = node.width;
                input.output = node.output;
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
        SelectItem::Expr {
            expression: Expr::Column { qualifier, name },
            ..
        } => Some((qualifier.or(relation)?, *name)),
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
        crate::sql::catalog::write_identifier(&mut keys.plain, qualifier);
        let _ = keys.plain.write_char('.');
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
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    let limited = query.limit.is_some() || query.offset.is_some();
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
        3 + (from.joins.len() + 2) * 3
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
        verbose_quals: NO_QUALS,
        schema: relation_slot(storage, txid, target.schema, target.name)
            .map(|slot| StackStr::from_str(storage.table_def(slot, txid).schema.as_str()))
            .unwrap_or_default(),
        subplan: None,
        shift: 0,
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
        let filters = arena
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
                .alloc_slice_with(relations_count, |relation| {
                    if relation == 0 {
                        (target_exposed, &*target_definition)
                    } else {
                        (
                            scope.names[relation - 1],
                            scope.defs.stored(relation - 1).expect("stored relation"),
                        )
                    }
                })
                .map_err(|_| arena_full())?;
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
                join_filters.copy_from_slice(quals.join_filters);
                hash_cond = quals.hash_cond;
            }
            let joined = arena
                .alloc_slice_with(relations_count, |index| {
                    crate::sql::catalog::deparse::Joined::new(
                        relations[index].0,
                        relations[index].1,
                    )
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
                folded: true,
                arena,
                dependencies: None,
            };
            for table in 0..scope.n {
                if let Some(index) = scan_indexes[table + 1].as_ref() {
                    source_nodes[table] = source_nodes[table].with_qual(
                        QualLabel::IndexCond,
                        index_condition(&context, relations[table + 1].0, index, false, arena)?,
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
        let target_scan = target_scan.with_qual(QualLabel::Filter, filters[0].take());
        push_scan(&mut plan, target_scan)?;
        for (position, &table) in order[..scope.n].iter().enumerate() {
            let mut node =
                source_nodes[table].with_qual(QualLabel::Filter, filters[table + 1].take());
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
                    block.child(Segment::Derived(
                        u16::try_from(table).map_err(|_| arena_full())?,
                    )),
                    arena,
                )?,
                arena,
            )?;
        }
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    let child = if let (Some(select), Stmt::Insert(insert)) = (source, statement) {
        let source_plan = rewrite::insert_source_plan(storage, txid, insert, select, block, arena)?;
        let root = source_plan.nodes[0];
        for source_node in &source_plan.nodes[..source_plan.count] {
            let mut source_node = *source_node;
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
        // An INSERT into a view its INSTEAD OF trigger handles reads its rows.
        let several = insert.rows.len() > 1;
        let child = PlanNode {
            name: StackStr::from_str(if several { "Values Scan" } else { "Result" }),
            alias: StackStr::from_str(if several { "*VALUES*" } else { "" }),
            key: Some(NodeKey::new(block, Role::Values)),
            depth: 1,
            rows: insert.rows.len() as u64,
            width: 32,
            total_cost: 0.01,
            ..PlanNode::EMPTY
        };
        plan.push(child)?;
        child
    } else if let Some(slot) = relation_slot(storage, txid, target.schema, target.name) {
        let child = if let Stmt::Insert(insert) = statement {
            // Several rows are scanned from their VALUES list.
            let several = insert.rows.len() > 1;
            PlanNode {
                name: StackStr::from_str(if several { "Values Scan" } else { "Result" }),
                alias: StackStr::from_str(if several { "*VALUES*" } else { "" }),
                key: Some(NodeKey::new(block, Role::Values)),
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
            let (index_cond, filter) = single_relation_quals(
                storage,
                txid,
                alias.unwrap_or(target.name),
                &definition,
                predicate,
                index.as_ref().map(|(plan, _)| plan),
                false,
                arena,
            )?;
            let (verbose_index_cond, verbose_filter) = single_relation_quals(
                storage,
                txid,
                alias.unwrap_or(target.name),
                &definition,
                predicate,
                index.as_ref().map(|(plan, _)| plan),
                true,
                arena,
            )?;
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
        if options.verbose && !node.output.as_str().is_empty() {
            let mut output = StackStr::<512>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(output, " ");
            }
            let _ = write!(output, "Output: {}", node.output.as_str());
            emit(output.as_str())?;
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
        for (label, text) in node.shown_qualifications(options.verbose) {
            let mut qual = StackStr::<2048>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(qual, " ");
            }
            let _ = write!(qual, "{}: {}", label.text(), text);
            emit(qual.as_str())?;
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
        if let Some(table) = hash_table(node_actual) {
            let mut line = StackStr::<256>::new();
            for _ in 0..node_indent(node) {
                let _ = write!(line, " ");
            }
            let memory = table.bytes.div_ceil(1024);
            if table.original_buckets == table.buckets {
                let _ = write!(
                    line,
                    "Buckets: {}  Batches: 1  Memory Usage: {memory}kB",
                    table.buckets
                );
            } else {
                let _ = write!(
                    line,
                    "Buckets: {} (originally {})  Batches: 1 (originally 1)  Memory Usage: {memory}kB",
                    table.buckets, table.original_buckets
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
    block: BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    if block == BlockId::ROOT {
        reset_subplans();
    }
    match statement {
        Stmt::Select(select) => {
            let select = if select.with.iter().any(|cte| cte.dml.is_some()) {
                return plan_data_modifying_cte_select(select, arena);
            } else if select.with.is_empty() && !storage.has_any_view() {
                select
            } else {
                // A view is read through its query (its `_RETURN` rule).
                query::expand_ctes(select, storage, txid, arena)?
            };
            plan_select(storage, txid, select, block, arena)
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
            plan_set_query(storage, txid, &planned, block, arena)
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
        Stmt::With { statement, .. } => plan_modification(storage, txid, statement, block, arena),
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
    join_type: Option<&'static str>,
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
        "Aggregate" => kind.strategy = Some("Plain"),
        "HashAggregate" => {
            kind.node_type = "Aggregate";
            kind.strategy = Some("Hashed");
        }
        "Nested Loop" | "Hash Join" => kind.join_type = Some("Inner"),
        "Nested Loop Left Join" => {
            kind.node_type = "Nested Loop";
            kind.join_type = Some("Left");
        }
        "Hash Left Join" => {
            kind.node_type = "Hash Join";
            kind.join_type = Some("Left");
        }
        "Hash Right Join" => {
            kind.node_type = "Hash Join";
            kind.join_type = Some("Right");
        }
        "Hash Full Join" => {
            kind.node_type = "Hash Join";
            kind.join_type = Some("Full");
        }
        _ => {}
    }
    kind
}

/// The buffer counters PostgreSQL shows for a node or for planning. Cache
/// tiers map onto PostgreSQL's shared hit/read vocabulary; unmodelled local
/// and temp counters are zero.
fn structured_buffers(document: &mut Document<'_>, io: Option<BlockIoStats>) {
    let io = io.unwrap_or_default();
    let hit = io.ram_hits.saturating_add(io.disk_hits);
    for (label, value) in [
        ("Shared Hit Blocks", hit),
        ("Shared Read Blocks", io.object_gets),
        ("Shared Dirtied Blocks", 0),
        ("Shared Written Blocks", 0),
        ("Local Hit Blocks", 0),
        ("Local Read Blocks", 0),
        ("Local Dirtied Blocks", 0),
        ("Local Written Blocks", 0),
        ("Temp Read Blocks", 0),
        ("Temp Written Blocks", 0),
    ] {
        document.number(label, value);
    }
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
            if node.name.as_str() == "Function Scan" {
                "Function Name"
            } else {
                "Relation Name"
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
    if options.verbose && !node.output.as_str().is_empty() {
        document.list("Output", node.output.as_str().split(", "));
    }
    if kind.join_type.is_some() {
        document.boolean("Inner Unique", false);
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
        document.number("Hash Buckets", table.buckets);
        document.number("Original Hash Buckets", table.original_buckets);
        document.number("Hash Batches", 1);
        document.number("Original Hash Batches", 1);
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
    if let NodeActual::Ran(NodeCounters {
        stored: Some(bytes),
        ..
    }) = node_actual
    {
        document.text("Storage", "Memory");
        document.number("Maximum Storage", bytes.div_ceil(1024));
    }
    if let Some(actual) = actual {
        if options.buffers {
            structured_buffers(document, Some(actual.io));
        }
        if options.wal {
            document.number("WAL Records", actual.wal_records);
            document.number("WAL FPI", 0);
            document.number("WAL Bytes", actual.wal_bytes);
            document.number("WAL Buffers Full", 0);
        }
    }
    let mut next = at + 1;
    let mut children = 0;
    let mut inputs = 0;
    while next < plan.count && plan.nodes[next].depth == node.depth + 1 {
        if children == 0 {
            document.open_group("Plans", Some("Plans"), false);
        }
        // A subplan is no input of its parent: its first input is the outer.
        let relationship = match plan.nodes[next].subplan.as_ref() {
            Some((kind, _)) => kind.relationship(),
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
    if options.memory || (options.buffers && options.summary) {
        document.open_group("Planning", Some("Planning"), true);
        if options.buffers {
            structured_buffers(document, None);
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
                structured_buffers(document, None);
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

pub(super) fn visit_plan_rows(
    plan: &Plan<'_>,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    settings: &PlannerSettings,
    arena: &Arena,
    mut emit: impl FnMut(&str) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
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
    execute: impl FnOnce() -> R,
) -> Result<R, SqlError> {
    let planned = |query: &RewrittenQuery<'a>| match query {
        RewrittenQuery::Planned(plan) => plan.count,
        RewrittenQuery::Notify => 0,
    };
    let total: usize = queries.iter().map(planned).sum();
    let key_at = |mut index: usize| {
        for query in queries.iter() {
            if let RewrittenQuery::Planned(plan) = query {
                if index < plan.count {
                    return plan.nodes[index].key;
                }
                index -= plan.count;
            }
        }
        None
    };
    let slots = arena
        .alloc_slice_with(total, key_at)
        .map_err(|_| query::arena_full_pub())?;
    let slots = arena
        .alloc_slice_with(total, |index| Slot::new(slots[index]))
        .map_err(|_| query::arena_full_pub())?;
    let result = query::instrument::analyze_queries(slots, timing, execute);
    let mut start = 0;
    for query in queries.iter_mut() {
        if let RewrittenQuery::Planned(plan) = query {
            let counters = arena
                .alloc_slice_with(plan.count, |index| slots[start + index].counters())
                .map_err(|_| query::arena_full_pub())?;
            plan.measured = Some(counters);
            start += plan.count;
        }
    }
    Ok(result)
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
    let last = queries.len().saturating_sub(1);
    let query_actual = |index: usize| {
        actual.map(|actual| {
            if index == last {
                ExplainActual { rows: 0, ..actual }
            } else {
                ExplainActual {
                    rows: 0,
                    elapsed_micros: 0,
                    io: BlockIoStats::default(),
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
