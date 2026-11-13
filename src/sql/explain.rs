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
    Filter,
    HashCond,
    IndexCond,
    RecheckCond,
    JoinFilter,
}

impl QualLabel {
    /// PostgreSQL's EXPLAIN shows a join's or an index's conditions before
    /// its filter.
    const fn rank(self) -> u8 {
        match self {
            Self::HashCond | Self::IndexCond | Self::RecheckCond => 0,
            Self::JoinFilter => 1,
            Self::Filter => 2,
            Self::None => 3,
        }
    }

    const fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Filter => "Filter",
            Self::HashCond => "Hash Cond",
            Self::IndexCond => "Index Cond",
            Self::RecheckCond => "Recheck Cond",
            Self::JoinFilter => "Join Filter",
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

    /// The labelled qualifications, in display order.
    fn qualifications(&self) -> impl Iterator<Item = (QualLabel, &str)> {
        self.quals
            .iter()
            .filter(|(label, _)| *label != QualLabel::None)
            .map(|(label, qual)| (*label, qual.as_str()))
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

/// The `Hash Cond` and `Join Filter` of an outer hash join of two relations
/// with no WHERE clause: its condition's equalities between the probe and
/// build relations, outer side first, and its other conjuncts. None when a
/// conjunct reads only one side, which PostgreSQL places elsewhere.
fn outer_hash_quals<'a>(
    storage: &Storage,
    txid: u32,
    relations: &[(&'a str, &'a crate::storage::TableDef)],
    probe: usize,
    condition: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<Option<(StackStr<512>, Option<StackStr<512>>)>, SqlError> {
    let resolve =
        |qualifier: Option<&str>, name: &str| resolve_relation(relations, qualifier, name);
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
        if relation_mask(conjunct, &resolve) != Some(0b11) {
            return Ok(None);
        }
        if let Some(equality) = equijoin(conjunct, &resolve, probe, arena)? {
            hash_conds[hash_count] = equality;
            hash_count += 1;
        } else {
            filters[filter_count] = conjunct;
            filter_count += 1;
        }
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
    };
    let Some(hash_cond) = deparse_qual(&context, &hash_conds[..hash_count], arena)? else {
        return Ok(None);
    };
    let join_filter = deparse_qual(&context, &filters[..filter_count], arena)?;
    if filter_count != 0 && join_filter.is_none() {
        return Ok(None);
    }
    Ok(Some((hash_cond, join_filter)))
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
        arena,
        dependencies: None,
    };
    for ((index, condition), (exposed, _)) in
        indexes.iter().zip(conditions.iter_mut()).zip(relations)
    {
        if let Some(index) = index {
            *condition = index_condition(&context, exposed, index, arena)?;
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
fn single_relation_quals<'a>(
    storage: &Storage,
    txid: u32,
    exposed: &'a str,
    table: &'a crate::storage::TableDef,
    condition: Option<&'a Expr<'a>>,
    index: Option<&query::IndexAccessPlan<'a>>,
    arena: &'a Arena,
) -> Result<(Option<StackStr<512>>, Option<StackStr<512>>), SqlError> {
    let Some(condition) = condition else {
        return Ok((None, None));
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
    let joined = [crate::sql::catalog::deparse::Joined::new(exposed, table)];
    let context = crate::sql::catalog::deparse::Context {
        storage,
        txid,
        arena,
        dependencies: None,
        table: Some(table),
        parameters: &[],
        joined: &joined,
        pretty: false,
        qualify: false,
        domain_value: None,
        bare: None,
    };
    let index_cond = match index {
        Some(index) => index_condition(&context, exposed, index, arena)?,
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
    if !bitmap_index.as_str().is_empty()
        && let Some(entry) = scan
            .quals
            .iter_mut()
            .find(|(label, _)| *label == QualLabel::IndexCond)
    {
        entry.0 = QualLabel::RecheckCond;
        index_cond = Some(entry.1);
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
            .with_qual(QualLabel::IndexCond, index_cond),
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
    let context = crate::sql::catalog::deparse::Context {
        bare: Some(exposed),
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
    let mut alias = StackStr::new();
    match scope.defs.stored(table).filter(|_| !derived) {
        Some(definition) => {
            let _ = write!(relation, "{}", definition.name.as_str());
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
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let started = std::time::Instant::now();
    // Execution plans the same query: simple subqueries merged into it and
    // outer joins its conditions narrow reduced.
    let statement = query::pull_up_subqueries(statement, storage, txid, arena, None)?;
    let scope = match &statement.from {
        Some(from) => Some(QueryScope::resolve_schema(
            storage, from, txid, arena, None,
        )?),
        None => None,
    };
    let capacity = scope
        .as_ref()
        .map_or(MAX_EXPLAIN_STAGES, |scope| scope.n * 3 + MAX_EXPLAIN_STAGES);
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
    let mut hash_join_name = "Hash Join";
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

    if scope.as_ref().is_some_and(|scope| scope.n > 1) {
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
                keys: match group_keys(statement, arena)?.filter(|_| !statement.group_by.is_empty())
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
                keys: plan_keys(
                    storage,
                    txid,
                    scope.as_ref(),
                    statement,
                    KeyLabel::Sort,
                    order_keys(statement, arena)?,
                    arena,
                )?,
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
            // Without an ordering PostgreSQL hashes the distinct rows.
            PlanNode {
                name: StackStr::from_str(
                    if statement.order_by.is_empty() && statement.distinct_on.is_empty() {
                        "HashAggregate"
                    } else {
                        "Unique"
                    },
                ),
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
    let n = scope.as_ref().map_or(0, |scope| scope.n);
    // The WHERE conjuncts PostgreSQL attaches to each scan and to each join,
    // when every conjunct can be placed and printed.
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
            (index_conds[0], filters[0]) = single_relation_quals(
                storage,
                txid,
                exposed,
                def,
                statement.where_clause,
                scan_indexes[0].as_ref(),
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
            && statement.where_clause.is_none()
            && let Some(join) = statement.from.as_ref().and_then(|from| from.joins.first())
            && matches!(
                join.kind,
                crate::sql::ast::JoinKind::Left
                    | crate::sql::ast::JoinKind::Right
                    | crate::sql::ast::JoinKind::Full
            )
            && let Some(condition) = join.on.or(scope.join_on[0])
            && let (Some(first), Some(second)) = (relation(0), relation(1))
            && let Some((hash_cond, join_filter)) = outer_hash_quals(
                storage,
                txid,
                &[first, second],
                table_order[0],
                condition,
                arena,
            )?
        {
            stages[0] = stages[0].with_qual(QualLabel::HashCond, Some(hash_cond));
            join_filters[1] = join_filter;
        }
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
            let sequential =
                scan.name.as_str() == "Seq Scan" && crate::sql::guc::planner_methods().material;
            scan = scan
                .with_qual(QualLabel::IndexCond, index_conds[table].take())
                .with_qual(QualLabel::Filter, filters[table].take());
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
            let passes_through = from.joins.is_empty()
                && statement
                    .items
                    .iter()
                    .all(|item| matches!(item, SelectItem::Wildcard(_)));
            plan = with_derived_plan(
                plan,
                scope.names[table],
                passes_through,
                plan_derived(storage, txid, body, arena)?,
                arena,
            )?;
        }
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    Ok(plan)
}

/// The plan of a derived table's own query.
fn plan_derived<'a>(
    storage: &Storage,
    txid: u32,
    body: &Select<'_>,
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
            arena,
        ),
        None if body.with.is_empty() => plan_select(storage, txid, body, arena),
        None => plan_select(
            storage,
            txid,
            query::expand_ctes(body, storage, txid, arena)?,
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
    let Some(scope) = scope else {
        return Ok(PlanKeys::NONE);
    };
    if keys.is_empty() || (0..scope.n).any(|table| scope.defs.stored(table).is_none()) {
        return Ok(PlanKeys::NONE);
    }
    let joined = arena
        .alloc_slice_with(scope.n, |table| {
            crate::sql::catalog::deparse::Joined::new(
                scope.names[table],
                scope.defs.stored(table).expect("stored relation"),
            )
        })
        .map_err(|_| arena_full())?;
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
            qualify: verbose || scope.n > 1,
            bare: None,
            domain_value: None,
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
                    if matches!(expression, Expr::Column { .. }) {
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
        SetTree::Op {
            syntax: crate::sql::ast::SetOperationSyntax::Values,
            ..
        } => {
            let node = values_scan(plan, tree, depth);
            plan.push(node)?;
            Ok(node)
        }
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
                keys: PlanKeys::NONE,
            };
            plan.nodes[at] = node;
            Ok(node)
        }
    }
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
    let column_name = |position: usize| match branch.items.get(position)? {
        SelectItem::Expr {
            alias: Some(alias), ..
        } => Some(*alias),
        SelectItem::Expr {
            expression: Expr::Column { name, .. },
            alias: None,
        } => Some(*name),
        _ => None,
    };
    let mut keys = PlanKeys {
        label: KeyLabel::Sort,
        ..PlanKeys::NONE
    };
    for (index, order) in query.order_by.iter().enumerate() {
        let name = match order.expression {
            Expr::Int(position) => usize::try_from(*position)
                .ok()
                .and_then(|position| column_name(position.checked_sub(1)?)),
            Expr::Column {
                qualifier: None,
                name,
            } => Some(*name),
            _ => None,
        };
        let Some(name) = name else {
            return PlanKeys::NONE;
        };
        if index != 0 {
            let _ = keys.plain.write_char(KEY_SEPARATOR);
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
        if node.name.as_str() == "Sort" {
            node.keys = set_sort_keys(query);
        }
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
                arena,
                dependencies: None,
            };
            for table in 0..scope.n {
                if let Some(index) = scan_indexes[table + 1].as_ref() {
                    source_nodes[table] = source_nodes[table].with_qual(
                        QualLabel::IndexCond,
                        index_condition(&context, relations[table + 1].0, index, arena)?,
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
        plan.planning_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        return Ok(plan);
    }
    let child = if let Some(select) = source {
        let source_plan = plan_select(storage, txid, select, arena)?;
        let root = source_plan.nodes[0];
        for source_node in &source_plan.nodes[..source_plan.count] {
            let mut source_node = *source_node;
            source_node.depth = source_node.depth.saturating_add(1);
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
    } else if let Some(slot) = relation_slot(storage, txid, target.schema, target.name) {
        let child = if let Stmt::Insert(insert) = statement {
            // Several rows are scanned from their VALUES list.
            let several = insert.rows.len() > 1;
            PlanNode {
                name: StackStr::from_str(if several { "Values Scan" } else { "Result" }),
                alias: StackStr::from_str(if several { "*VALUES*" } else { "" }),
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
            let definition = storage.table_def(slot, txid);
            let (index_cond, filter) = single_relation_quals(
                storage,
                txid,
                alias.unwrap_or(target.name),
                &definition,
                predicate,
                index.as_ref().map(|(plan, _)| plan),
                arena,
            )?;
            scan.with_qual(QualLabel::IndexCond, index_cond)
                .with_qual(QualLabel::Filter, filter)
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

fn text_line(
    node: &PlanNode,
    options: ExplainOptions,
    actual: Option<ExplainActual>,
    root: bool,
) -> StackStr<2048> {
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
    if node.relation.as_str().is_empty() && !node.alias.as_str().is_empty() {
        let _ = write!(line, " on ");
        crate::sql::catalog::write_identifier(&mut line, node.alias.as_str());
    }
    if !node.relation.as_str().is_empty() {
        let _ = write!(line, " on {}", node.relation.as_str());
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
        if plan.disabled(node) {
            let mut disabled = StackStr::<512>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(disabled, " ");
            }
            let _ = write!(disabled, "Disabled: true");
            emit(disabled.as_str())?;
        }
        if options.verbose && !node.output.as_str().is_empty() {
            let mut output = StackStr::<512>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(output, " ");
            }
            let _ = write!(output, "Output: {}", node.output.as_str());
            emit(output.as_str())?;
        }
        if let Some((label, mut keys)) = node.keys.shown(options.verbose) {
            let mut line = StackStr::<2048>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(line, " ");
            }
            let _ = write!(line, "{label}: {}", keys.next().unwrap_or_default());
            for key in keys {
                let _ = write!(line, ", {key}");
            }
            emit(line.as_str())?;
        }
        for (label, text) in node.qualifications() {
            let mut qual = StackStr::<2048>::new();
            for _ in 0..property_indent(node.depth) {
                let _ = write!(qual, " ");
            }
            let _ = write!(qual, "{}: {}", label.text(), text);
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
        document.text("Relation Name", node.relation.as_str());
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
    let actual = actual.filter(|_| at == 0);
    if let Some(actual) = actual {
        if options.timing {
            document.number("Actual Startup Time", "0.000");
            document.number(
                "Actual Total Time",
                stack_format!(32, "{:.3}", actual.elapsed_micros as f64 / 1_000.0),
            );
        }
        document.number(
            "Actual Rows",
            stack_format!(32, "{:.2}", actual.rows as f64),
        );
        document.number("Actual Loops", 1);
    }
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
    if node.constant_false {
        document.text("One-Time Filter", "false");
    }
    for (label, text) in node.qualifications() {
        document.text(label.text(), text);
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
    while next < plan.count && plan.nodes[next].depth == node.depth + 1 {
        if children == 0 {
            document.open_group("Plans", Some("Plans"), false);
        }
        let relationship = if children == 0 { "Outer" } else { "Inner" };
        next = structured_node(document, plan, next, Some(relationship), options, None);
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
    let slot = match storage.resolve_relation(table.schema, table.name, txid) {
        Some(crate::storage::ResolvedRelation::Table(slot)) => slot,
        Some(crate::storage::ResolvedRelation::View(view))
            if storage
                .firing_rules_for(RuleTarget::View(view as u16), event, false, txid)
                .next()
                .is_some() =>
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN of a statement a rewrite rule changes is not supported for a statement on a view"
            ));
        }
        _ => return Ok(None),
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
            _ => Err(unsupported(
                "an action on a view or on a relation with rules",
            )),
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
    // The original INSERT's rows, or the simple query PostgreSQL pulls up
    // into each action in place of `NEW`.
    let (inserted_rows, inserted_query): (&'a [&'a [&'a Expr<'a>]], Option<&'a Select<'a>>) =
        match statement {
            Stmt::Insert(insert) => match insert.select {
                Some(select)
                    if !select.distinct
                        && select.distinct_on.is_empty()
                        && select.windows.is_empty()
                        && select.group_by.is_empty()
                        && select.grouping_sets.is_empty()
                        && select.having.is_none()
                        && select.order_by.is_empty()
                        && select.limit.is_none()
                        && select.offset.is_none()
                        && select.with.is_empty()
                        && select.set_body.is_none()
                        && select.locking.is_empty()
                        && select
                            .items
                            .iter()
                            .all(|item| matches!(item, SelectItem::Expr { .. }))
                        && !selects_aggregate(select, storage, txid, arena)? =>
                {
                    (&[], Some(select))
                }
                Some(_) => return Err(unsupported("an INSERT of an aggregated or ordered query")),
                None => (insert.rows, None),
            },
            _ => (&[], None),
        };
    let inserted_count = inserted_rows.len().max(1);
    let new_value = |name: &'a str, row: usize| -> Result<&'a Expr<'a>, SqlError> {
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
                let position = if insert.columns.is_empty() {
                    definition.column_index(name)
                } else {
                    insert.columns.iter().position(|column| *column == name)
                };
                if let Some(select) = inserted_query {
                    return Ok(
                        match position.and_then(|position| select.items.get(position)) {
                            Some(SelectItem::Expr { expression, .. }) => expression,
                            _ => &Expr::Null,
                        },
                    );
                }
                match position.and_then(|position| inserted_rows[row].get(position)) {
                    Some(Expr::DefaultMarker) => {
                        let column = definition
                            .column_index(name)
                            .map(|column| &definition.columns()[column]);
                        let default = column
                            .filter(|column| !column.default.is_generated())
                            .and_then(|column| column.default.expression());
                        match default.map(StackStr::as_str) {
                            None => Ok(&Expr::Null),
                            Some(sql) => {
                                let sql = arena.alloc_str(sql).map_err(|_| arena_full())?;
                                crate::sql::parser::parse_expr(sql, arena)
                            }
                        }
                    }
                    Some(value) => Ok(value),
                    None => Ok(&Expr::Null),
                }
            }
            _ => Err(unsupported("NEW in a DELETE rule")),
        }
    };
    let substitute_row = |expression: &'a Expr<'a>, row: usize| -> Result<&'a Expr<'a>, SqlError> {
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
                    new_value(name, row).map(Some).map_err(RewriteFailure)
                }
                _ => Ok(None),
            }
        })
        .map_err(|RewriteFailure(error)| error)
    };
    let substitute = |expression: &'a Expr<'a>| substitute_row(expression, 0);
    let source = (event != RewriteEvent::Insert).then(|| crate::sql::ast::FromClause {
        base: query::rule_relation_ref(table, alias),
        joins: &[],
    });
    // An action's own FROM items, then the original relation PostgreSQL
    // adds to its range table.
    let with_source = |own: Option<crate::sql::ast::FromClause<'a>>|
     -> Result<Option<&'a crate::sql::ast::FromClause<'a>>, SqlError> {
        let from = match (own, source) {
            (None, None) => return Ok(None),
            (Some(own), None) => own,
            (None, Some(source)) => source,
            (Some(own), Some(source)) => {
                let joins = arena
                    .alloc_slice_with(own.joins.len() + 1, |index| {
                        own.joins.get(index).copied().unwrap_or(crate::sql::ast::Join {
                            table: source.base,
                            kind: crate::sql::ast::JoinKind::Comma,
                            on: None,
                            using: None,
                            natural: false,
                        })
                    })
                    .map_err(|_| arena_full())?;
                crate::sql::ast::FromClause { base: own.base, joins }
            }
        };
        Ok(Some(&*arena.alloc(from).map_err(|_| arena_full())?))
    };
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
                    // `AddInvertedQual`: the original keeps the rows for
                    // which the condition is not true.
                    let whens = arena
                        .alloc_slice_copy(&[(rule_condition, &Expr::Bool(false))])
                        .map_err(|_| arena_full())?;
                    let not_true = arena
                        .alloc(Expr::Case {
                            operand: None,
                            whens,
                            otherwise: Some(&Expr::Bool(true)),
                            origin: crate::sql::ast::CaseOrigin::BooleanTest {
                                value: true,
                                negated: true,
                            },
                        })
                        .map_err(|_| arena_full())?;
                    exclusions = query::and_where(exclusions, Some(not_true), arena)?;
                }
            }
        }
        let filter = query::and_where(original_condition, rule_condition, arena)?;
        for action in rule.action_sql() {
            let action = arena.alloc_str(action).map_err(|_| arena_full())?;
            let action = crate::sql::parser::parse_stored_statement(action, arena)?;
            let rewritten: Stmt<'a> = match *action {
                Stmt::Notify { .. } => *action,
                Stmt::Insert(insert) if insert.select.is_none() => {
                    // Each action row, once per original row it reads.
                    let mut rows = crate::mem::arena::ArenaList::new(arena);
                    for original in 0..inserted_count {
                        for row in insert.rows {
                            let mut values = crate::mem::arena::ArenaList::new(arena);
                            for value in row.iter() {
                                values
                                    .push(substitute_row(value, original)?)
                                    .map_err(|_| arena_full())?;
                            }
                            rows.push(values.as_slice()).map_err(|_| arena_full())?;
                        }
                    }
                    let rows = rows.as_slice();
                    let reads_query = inserted_query.is_some_and(|query| query.from.is_some())
                        || inserted_query.is_some_and(|query| query.where_clause.is_some());
                    if source.is_none() && filter.is_none() && !reads_query {
                        Stmt::Insert(crate::sql::ast::Insert { rows, ..insert })
                    } else {
                        if rows.len() != 1 {
                            return Err(unsupported("a conditional action inserting several rows"));
                        }
                        let items = arena
                            .alloc_slice_with(rows[0].len(), |index| SelectItem::Expr {
                                expression: rows[0][index],
                                alias: None,
                            })
                            .map_err(|_| arena_full())?;
                        let from = match (inserted_query.and_then(|query| query.from), source) {
                            (Some(from), None) => Some(from),
                            (None, source) => source,
                            (Some(_), Some(_)) => unreachable!("an INSERT rule reads no relation"),
                        };
                        let where_clause = query::and_where(
                            inserted_query.and_then(|query| query.where_clause),
                            filter,
                            arena,
                        )?;
                        let select = arena
                            .alloc(query::rule_select(items, from, where_clause))
                            .map_err(|_| arena_full())?;
                        Stmt::Insert(crate::sql::ast::Insert {
                            rows: &[],
                            select: Some(select),
                            ..insert
                        })
                    }
                }
                Stmt::Insert(insert) => {
                    let select = insert.select.expect("matched above");
                    if inserted_count > 1
                        || inserted_query.is_some_and(|query| {
                            query.from.is_some() || query.where_clause.is_some()
                        })
                    {
                        return Err(unsupported(
                            "an INSERT ... SELECT action of an INSERT of several or selected rows",
                        ));
                    }
                    let items = arena
                        .alloc_slice_with(select.items.len(), |index| select.items[index])
                        .map_err(|_| arena_full())?;
                    for item in items.iter_mut() {
                        if let SelectItem::Expr { expression, .. } = item {
                            *expression = substitute(expression)?;
                        }
                    }
                    let own = select.where_clause.map(substitute).transpose()?;
                    let from = with_source(select.from)?;
                    let select = arena
                        .alloc(Select {
                            items,
                            from: from.copied(),
                            where_clause: query::and_where(own, filter, arena)?,
                            ..*select
                        })
                        .map_err(|_| arena_full())?;
                    Stmt::Insert(crate::sql::ast::Insert {
                        select: Some(select),
                        ..insert
                    })
                }
                Stmt::Update(update) => {
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
                    Stmt::Update(crate::sql::ast::Update {
                        assignments,
                        from: with_source(update.from.copied())?,
                        where_clause: query::and_where(own, filter, arena)?,
                        ..update
                    })
                }
                Stmt::Delete(delete) => {
                    let own = delete
                        .where_clause
                        .map(|own| substitute(qualify_action(own, delete.table)?))
                        .transpose()?;
                    Stmt::Delete(crate::sql::ast::Delete {
                        using: with_source(delete.using.copied())?,
                        where_clause: query::and_where(own, filter, arena)?,
                        ..delete
                    })
                }
                _ => return Err(unsupported("this rule action")),
            };
            // PostgreSQL rewrites an action on a relation with rules again.
            let (action_table, action_event) = match &rewritten {
                Stmt::Insert(insert) => (Some(insert.table), RewriteEvent::Insert),
                Stmt::Update(update) => (Some(update.table), RewriteEvent::Update),
                Stmt::Delete(delete) => (Some(delete.table), RewriteEvent::Delete),
                _ => (None, RewriteEvent::Insert),
            };
            if let Some(action_table) = action_table {
                let action_target =
                    match storage.resolve_relation(action_table.schema, action_table.name, txid) {
                        Some(crate::storage::ResolvedRelation::Table(slot)) => {
                            RuleTarget::Table(slot as u16)
                        }
                        Some(crate::storage::ResolvedRelation::View(view)) => {
                            RuleTarget::View(view as u16)
                        }
                        _ => return Err(unsupported("this rule action")),
                    };
                if matches!(action_target, RuleTarget::View(_))
                    || storage
                        .firing_rules_for(action_target, action_event, false, txid)
                        .next()
                        .is_some()
                {
                    return Err(unsupported(
                        "an action on a view or on a relation with rules",
                    ));
                }
            }
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
            Stmt::Insert(insert) => {
                let select = match inserted_query {
                    Some(select) => Select {
                        where_clause: query::and_where(
                            select.where_clause,
                            Some(exclusions),
                            arena,
                        )?,
                        ..*select
                    },
                    None => {
                        let [row] = inserted_rows else {
                            return Err(unsupported(
                                "a conditional INSTEAD rule on an INSERT of several rows",
                            ));
                        };
                        let items = arena
                            .alloc_slice_with(row.len(), |index| SelectItem::Expr {
                                expression: row[index],
                                alias: None,
                            })
                            .map_err(|_| arena_full())?;
                        query::rule_select(items, None, Some(exclusions))
                    }
                };
                let select = &*arena.alloc(select).map_err(|_| arena_full())?;
                Stmt::Insert(crate::sql::ast::Insert {
                    rows: &[],
                    select: Some(select),
                    ..*insert
                })
            }
            _ => unreachable!("only INSERT, UPDATE and DELETE are rewritten"),
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
    let Some(statements) = rule_rewritten_statements(storage, txid, statement, arena)? else {
        return Ok(None);
    };
    let mut queries = arena
        .alloc_owned_slice_with(statements.len(), |_| RewrittenQuery::Notify)
        .map_err(|_| arena_full())?;
    for (query, statement) in queries.iter_mut().zip(statements.iter()) {
        if !matches!(statement, Stmt::Notify { .. }) {
            *query = RewrittenQuery::Planned(plan_statement(storage, txid, statement, arena)?);
        }
    }
    Ok(Some(queries))
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
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
    if options.format == ExplainFormat::Text {
        for (index, query) in queries.iter().enumerate() {
            if index != 0 {
                responder.data_row(&[Datum::Text("")])?;
            }
            match query {
                RewrittenQuery::Notify => responder.data_row(&[Datum::Text("NOTIFY")])?,
                RewrittenQuery::Planned(plan) => {
                    visit_text_rows(plan, options, query_actual(index), settings, |line| {
                        responder.data_row(&[Datum::Text(line)])
                    })?;
                }
            }
        }
    } else {
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
            Ok(text) => responder.data_row(&[Datum::Text(text)])?,
            Err(error) => return Ok(Err(error)),
        }
    }
    responder.command_complete("EXPLAIN")?;
    Ok(Ok(()))
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
    responder.row_description(&[ColDesc::new("QUERY PLAN", oid::TEXT, -1)])?;
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
