//! The paths PostgreSQL builds for a scan of one table (`set_plain_rel_
//! pathlist`): the sequential scan, each btree index's index and
//! index-only scans (`build_index_paths`, costed by `cost_index` with
//! `btcostestimate`), and the bitmap heap scan over the cheapest
//! combination of bitmap index scans (`choose_bitmap_and`, with the
//! `BitmapOr` paths of `generate_bitmap_or_paths`), kept or discarded as
//! `add_path` does, then sorted and limited as `create_ordered_paths` and
//! `create_limit_path` do.

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Expr, IndexAccessMethod, SourcePosition};
use crate::sql::catalog::PlannerIndex;
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::Storage;

use super::super::scope::QueryScope;
use super::clauses::{self, Analyzer, Clause, QualCost, Restriction, column_bit};
use super::predicate::{self, Prop};
use super::{
    BitmapPlanKind, BitmapPlanNode, Comparison, Costs, IndexCondition, IndexInfo, IndexKeyOrder,
    MAX_INDEXES, OrderKey, Planned, Planner, QueryShape, ScanKind, ScanPath, SortKind,
    clamp_row_est, index_pages_fetched,
};

/// `add_path`'s `STD_FUZZ_FACTOR`.
const FUZZ_FACTOR: f64 = 1.01;
/// The index paths one query builds.
const MAX_INDEX_PATHS: usize = 256;
/// The candidate paths `add_path` weighs at once.
pub(super) const MAX_CANDIDATES: usize = 2 * MAX_INDEXES + 2;
/// The children of one `BitmapAnd` or `BitmapOr`.
const MAX_CHILDREN: usize = 64;
/// The index clauses of one index path.
const MAX_INDEX_CLAUSES: usize = 32;
/// `DEFAULT_PAGE_CPU_MULTIPLIER`.
const PAGE_CPU_MULTIPLIER: f64 = 50.0;

type Shape = &'static str;

/// An `OR` of comparisons of one index column with constants, which the
/// index applies as `column op ANY (array)` (`match_orclause_to_indexcol`).
#[derive(Clone, Copy)]
struct OrArray<'a> {
    comparison: Comparison,
    elements: &'a [Datum<'a>],
    operands: &'a [&'a Expr<'a>],
    key: &'a Expr<'a>,
}

/// A restriction an index applies (`IndexClause`).
#[derive(Clone, Copy)]
struct IndexClause<'a> {
    /// The index key position it constrains.
    position: usize,
    restriction: &'a Restriction<'a>,
    or_array: Option<OrArray<'a>>,
    /// The index condition's own selectivity.
    selectivity: f64,
}

impl IndexClause<'_> {
    /// `(column, lower bound)` of a range bound, for `clauselist_selectivity`.
    fn range(&self) -> Option<(usize, bool)> {
        if self.or_array.is_some() {
            return None;
        }
        range_bound(&self.restriction.clause)
    }

    fn is_equality(&self) -> bool {
        match self.or_array {
            Some(array) => array.comparison == Comparison::Eq,
            None => matches!(
                self.restriction.clause,
                Clause::Compare {
                    comparison: Comparison::Eq,
                    ..
                } | Clause::Array {
                    comparison: Comparison::Eq,
                    ..
                }
            ),
        }
    }

    /// The array length of a `ScalarArrayOpExpr` condition.
    fn array_length(&self) -> Option<f64> {
        match (self.or_array, self.restriction.clause) {
            (Some(array), _) => Some(array.elements.len() as f64),
            (None, Clause::Array { length, .. }) => Some(length),
            _ => None,
        }
    }
}

/// A clause's range bound: `(column, lower)` for a column compared by `<`,
/// `<=`, `>` or `>=` with a value.
pub(super) fn range_bound(clause: &Clause<'_>) -> Option<(usize, bool)> {
    match *clause {
        Clause::Compare {
            column, comparison, ..
        } => match comparison {
            Comparison::Lt | Comparison::Le => Some((column, false)),
            Comparison::Gt | Comparison::Ge => Some((column, true)),
            _ => None,
        },
        Clause::Expression { bound, .. } => bound,
        _ => None,
    }
}

/// One index path (`IndexPath`).
#[derive(Clone, Copy)]
struct IndexPath<'a> {
    index: usize,
    clauses: &'a [IndexClause<'a>],
    backward: bool,
    index_only: bool,
    /// Its pathkeys are the first this many of the query's.
    presorted: usize,
    /// `btcostestimate`'s figures.
    index_total: f64,
    selectivity: f64,
    /// `cost_index`'s figures.
    startup: f64,
    total: f64,
}

/// A node of a bitmap: an index path, or an AND or OR of nodes.
#[derive(Clone, Copy)]
enum BitmapKind<'a> {
    Index(usize),
    And(&'a [usize]),
    Or(&'a [usize]),
}

#[derive(Clone, Copy)]
struct BitmapNode<'a> {
    kind: BitmapKind<'a>,
    /// `cost_bitmap_tree_node`.
    tree_cost: f64,
    selectivity: f64,
    /// The path's own total cost (an index path's full `cost_index`).
    total: f64,
    /// The identities of the clauses it applies (`classify_index_clause_usage`).
    clauses: u64,
}

/// One candidate path of the relation, or of its ordered or limited
/// result.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Candidate {
    pub(super) kind: ScanKind,
    /// The index path, or the bitmap node of a bitmap heap scan; for a
    /// path of a partition or inheritance tree, its append path.
    pub(super) path: usize,
    pub(super) startup: f64,
    pub(super) total: f64,
    pub(super) disabled: u32,
    /// The path's pathkeys are the first this many of the query's.
    pub(super) presorted: usize,
    pub(super) sort: SortKind,
}

impl Candidate {
    pub(super) fn same_scan(&self, other: &Self) -> bool {
        (self.kind, self.path) == (other.kind, other.path)
    }
}

/// Fixed-capacity storage in the query's arena.
struct Pool<'a, T: Copy> {
    items: &'a mut [T],
    count: usize,
}

impl<'a, T: Copy> Pool<'a, T> {
    fn new(arena: &'a Arena, capacity: usize, filler: T) -> Result<Self, SqlError> {
        Ok(Self {
            items: arena
                .alloc_slice_with(capacity, |_| filler)
                .map_err(|_| crate::sql::eval::arena_full())?,
            count: 0,
        })
    }

    fn push(&mut self, item: T, full: Shape) -> Result<usize, Shape> {
        if self.count == self.items.len() {
            return Err(full);
        }
        self.items[self.count] = item;
        self.count += 1;
        Ok(self.count - 1)
    }
}

/// The state of one relation's path generation.
struct Paths<'p, 's, 'a> {
    planner: &'p Planner<'s, 'a>,
    arena: &'a Arena,
    /// Every restriction of the relation, in PostgreSQL's order.
    restrictions: &'a [Restriction<'a>],
    /// The modeled indexes.
    usable: &'p [bool],
    /// The query's pathkeys.
    order: &'p [OrderKey],
    /// Columns an equality with a constant fixes.
    fixed: u64,
    needed: Option<u64>,
    rows: f64,
    all_cost: QualCost,
    index_paths: Pool<'a, IndexPath<'a>>,
    nodes: Pool<'a, BitmapNode<'a>>,
    /// The next free clause identity for an `OR` grouped from part of
    /// another's arms.
    next_id: usize,
    /// Each partial index's predicate.
    predicates: &'p [&'a [Restriction<'a>]],
    /// Each partial index's predicate follows from the restrictions.
    pred_ok: &'p [bool],
    /// Per index, the restrictions (by position) its predicate implies,
    /// which leave its scans' conditions and filters.
    predicate_implied: &'p [u64],
}

/// The result of path generation, or the shape outside the model.
type Built<T> = Result<Result<T, Shape>, SqlError>;

macro_rules! outside {
    ($value:expr) => {
        match $value {
            Ok(value) => value,
            Err(shape) => return Ok(Err(shape)),
        }
    };
}

impl<'p, 's, 'a> Paths<'p, 's, 'a> {
    fn costs(&self) -> Costs {
        self.planner.costs
    }

    fn index(&self, index: usize) -> &IndexInfo {
        &self.planner.indexes[index]
    }

    /// `predicate_implied_by`.
    fn prove(&self, predicate: &Prop<'a>, clause: &Prop<'a>) -> Result<bool, SqlError> {
        let planner = self.planner;
        let compare = |column: usize, left: &Datum<'_>, right: &Datum<'_>| {
            planner.compare(planner.column_collation(column), left, right)
        };
        predicate::implied_by(predicate, clause, &compare, self.arena)
    }

    /// The predicate of `index` when it is a usable partial index.
    fn predicate(&self, index: usize) -> &'a [Restriction<'a>] {
        self.predicates.get(index).copied().unwrap_or(&[])
    }

    /// Whether the predicate of `index` implies `restriction`, which then
    /// leaves the index's scans (`indrestrictinfo`).
    fn implied_by_predicate(&self, index: usize, restriction: &Restriction<'a>) -> bool {
        self.restrictions
            .iter()
            .position(|own| core::ptr::eq(own, restriction))
            .is_some_and(|at| at < 64 && self.predicate_implied[index] & (1 << at) != 0)
    }

    /// `add_predicate_to_index_quals`: the selectivity of `clauses` with
    /// the parts of the predicate of `index` they do not imply.
    fn with_predicate(&self, index: usize, clauses: &[IndexClause<'a>]) -> Result<f64, SqlError> {
        let planner = self.planner;
        let arena = self.arena;
        let mut extra = [None::<(Option<(usize, bool)>, f64)>; clauses::MAX_CONJUNCTS];
        let mut extra_count = 0;
        let predicate = self.predicate(index);
        if !predicate.is_empty() {
            let props = arena
                .alloc_slice_with(clauses.len(), |_| Prop::And(&[]))
                .map_err(|_| crate::sql::eval::arena_full())?;
            for (prop, clause) in props.iter_mut().zip(clauses) {
                *prop = index_prop(clause, arena)?;
            }
            let quals = Prop::And(props);
            let compare = |column: usize, left: &Datum<'_>, right: &Datum<'_>| {
                planner.compare(planner.column_collation(column), left, right)
            };
            for part in predicate {
                let target = Prop::of(part, arena)?;
                if !predicate::implied_by(&target, &quals, &compare, arena)? {
                    extra[extra_count] = Some((range_bound(&part.clause), part.selectivity));
                    extra_count += 1;
                }
            }
        }
        Ok(planner.clauselist(
            extra[..extra_count].iter().flatten().copied().chain(
                clauses
                    .iter()
                    .map(|clause| (clause.range(), clause.selectivity)),
            ),
        ))
    }

    /// The index conditions a `LIKE` restriction's fixed prefix gives key
    /// `position` of `index`: equality with an exact pattern, else the
    /// prefix's range, by the key's class's operators; none for another
    /// column, or a default class of a collation other than `C`.
    fn prefix_quals(
        &self,
        restriction: &'a Restriction<'a>,
        index: usize,
        position: usize,
    ) -> &'a [Restriction<'a>] {
        let Clause::Like {
            column,
            plain,
            pattern_class,
            ..
        } = restriction.clause
        else {
            return &[];
        };
        let info = &self.index(index).catalog;
        if usize::from(info.columns[position]) != column {
            return &[];
        }
        if info.pattern_keys[position] {
            pattern_class
        } else if info.column_collation[position]
            && super::c_collation(self.planner.column_collation(column))
        {
            plain
        } else {
            &[]
        }
    }

    /// `match_clause_to_indexcol`.
    fn match_clause(
        &self,
        restriction: &'a Restriction<'a>,
        index: usize,
        position: usize,
    ) -> Built<Option<IndexClause<'a>>> {
        let column = usize::from(self.index(index).catalog.columns[position]);
        // A pattern class serves only equality among the comparisons; a key
        // of another collation than its column's serves no comparison
        // (`IndexCollMatchesExprColl`).
        let pattern = self.index(index).catalog.pattern_keys[position];
        if !self.index(index).catalog.column_collation[position]
            && !matches!(restriction.clause, Clause::NullTest { .. })
        {
            return Ok(Ok(None));
        }
        let plain = |selectivity| {
            Ok(Ok(Some(IndexClause {
                position,
                restriction,
                or_array: None,
                selectivity,
            })))
        };
        match restriction.clause {
            Clause::Compare {
                column: other,
                comparison,
                pattern: pattern_operator,
                ..
            } if other == column
                && comparison != Comparison::Ne
                && if pattern {
                    pattern_operator || comparison == Comparison::Eq
                } else {
                    !pattern_operator
                } =>
            {
                plain(restriction.selectivity)
            }
            Clause::NullTest { column: other, .. } if other == column => {
                plain(restriction.selectivity)
            }
            Clause::Array {
                column: other,
                comparison,
                any: true,
                ..
            } if other == column
                && comparison != Comparison::Ne
                && (!pattern || comparison == Comparison::Eq) =>
            {
                plain(restriction.selectivity)
            }
            Clause::Or(arms) => self.match_or(restriction, arms, column, position, pattern),
            _ => Ok(Ok(None)),
        }
    }

    /// `match_orclause_to_indexcol`: an `OR` whose arms each compare the
    /// column with a constant by one operator.
    fn match_or(
        &self,
        restriction: &'a Restriction<'a>,
        arms: &'a [&'a [Restriction<'a>]],
        column: usize,
        position: usize,
        pattern: bool,
    ) -> Built<Option<IndexClause<'a>>> {
        let mut first: Option<(Comparison, &'a Expr<'a>, core::mem::Discriminant<Datum<'a>>)> =
            None;
        for arm in arms {
            let [arm] = arm else {
                return Ok(Ok(None));
            };
            let Clause::Compare {
                column: other,
                comparison,
                constant,
                key,
                pattern: false,
                ..
            } = arm.clause
            else {
                return Ok(Ok(None));
            };
            if other != column {
                return Ok(Ok(None));
            }
            let Some(constant) = constant else {
                return Ok(Err("an OR of comparisons with parameters"));
            };
            let kind = core::mem::discriminant(&constant);
            match first {
                None => {
                    if comparison == Comparison::Ne || (pattern && comparison != Comparison::Eq) {
                        return Ok(Ok(None));
                    }
                    first = Some((comparison, key, kind));
                }
                Some((first_comparison, _, first_kind)) => {
                    if first_comparison != comparison || first_kind != kind {
                        return Ok(Ok(None));
                    }
                }
            }
        }
        let Some((comparison, key, _)) = first else {
            return Ok(Ok(None));
        };
        let elements = self
            .arena
            .alloc_slice_with(arms.len(), |at| match arms[at][0].clause {
                Clause::Compare {
                    constant: Some(constant),
                    ..
                } => constant,
                _ => Datum::Null,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        let operands = self
            .arena
            .alloc_slice_with(arms.len(), |at| match arms[at][0].clause {
                Clause::Compare { operand, .. } => operand,
                _ => &Expr::Null,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        let selectivity =
            outside!(
                self.planner
                    .array_selectivity(column, comparison, true, Some(elements))?
            );
        Ok(Ok(Some(IndexClause {
            position,
            restriction,
            or_array: Some(OrArray {
                comparison,
                elements,
                operands,
                key,
            }),
            selectivity,
        })))
    }

    /// `match_clauses_to_index` over `lists` in turn: each clause with the
    /// first key column it constrains, in key column order.
    fn index_clauses(
        &self,
        index: usize,
        lists: &[&[&'a Restriction<'a>]],
    ) -> Built<&'a [IndexClause<'a>]> {
        let keys = self.index(index).catalog.n_cols;
        let mut matched = [None::<IndexClause<'a>>; MAX_INDEX_CLAUSES];
        let mut count = 0;
        for list in lists {
            for &restriction in *list {
                if matched[..count]
                    .iter()
                    .flatten()
                    .any(|clause| core::ptr::eq(clause.restriction, restriction))
                {
                    continue;
                }
                if let Clause::Like { .. } = restriction.clause {
                    // `match_pattern_prefix`: the pattern's prefix bounds the
                    // first key column it can bound.
                    for position in 0..keys {
                        let derived = self.prefix_quals(restriction, index, position);
                        if derived.is_empty() {
                            continue;
                        }
                        for derived in derived {
                            if let Some(clause) =
                                outside!(self.match_clause(derived, index, position)?)
                            {
                                if count == MAX_INDEX_CLAUSES {
                                    return Ok(Err("more than 32 index conditions"));
                                }
                                matched[count] = Some(clause);
                                count += 1;
                            }
                        }
                        break;
                    }
                    continue;
                }
                for position in 0..keys {
                    if let Some(clause) =
                        outside!(self.match_clause(restriction, index, position)?)
                    {
                        if count == MAX_INDEX_CLAUSES {
                            return Ok(Err("more than 32 index conditions"));
                        }
                        matched[count] = Some(clause);
                        count += 1;
                        break;
                    }
                }
            }
        }
        // Key column order, each column's clauses in the order matched.
        let matched = &mut matched[..count];
        for at in 1..matched.len() {
            let mut at = at;
            while at > 0
                && matched[at - 1].expect("matched").position
                    > matched[at].expect("matched").position
            {
                matched.swap(at - 1, at);
                at -= 1;
            }
        }
        let clauses = self
            .arena
            .alloc_slice_with(count, |at| matched[at].expect("matched"))
            .map_err(|_| crate::sql::eval::arena_full())?;
        Ok(Ok(clauses))
    }

    /// `build_index_pathkeys` truncated as `truncate_useless_pathkeys`
    /// does: how many of the query's leading pathkeys the index scan in
    /// this direction returns.
    fn presorted(&self, index: usize, backward: bool) -> usize {
        if self.order.is_empty() {
            return 0;
        }
        let info = &self.index(index).catalog;
        let mut keys = [(usize::MAX, false, false); crate::storage::MAX_INDEX_COLS];
        let mut count = 0;
        for position in 0..info.n_cols {
            // A pattern class's order, or another collation's, is no
            // query's.
            if info.pattern_keys[position] || !info.column_collation[position] {
                break;
            }
            let column = usize::from(info.columns[position]);
            let ordered = self.order.iter().any(
                |key| matches!(key, OrderKey::Column { column: other, .. } if *other == column),
            );
            let fixed = self.fixed & column_bit(column) != 0;
            // A column in no equivalence class ends the index's pathkeys.
            if !ordered && !fixed {
                break;
            }
            // A pathkey a constant fixes, or an earlier key repeats, is
            // redundant.
            if fixed || keys[..count].iter().any(|key| key.0 == column) {
                continue;
            }
            keys[count] = (
                column,
                info.descending[position] != backward,
                info.nulls_first[position] != backward,
            );
            count += 1;
        }
        keys[..count]
            .iter()
            .zip(self.order)
            .take_while(|((column, descending, nulls_first), key)| {
                **key
                    == OrderKey::Column {
                        column: *column,
                        descending: *descending,
                        nulls_first: *nulls_first,
                    }
            })
            .count()
    }

    /// `check_index_only`.
    fn index_only(&self, index: usize) -> bool {
        if !crate::sql::guc::planner_methods().indexonlyscan {
            return false;
        }
        let Some(needed) = self.needed else {
            return false;
        };
        let info = &self.index(index).catalog;
        let mut covered = 0u64;
        for (position, &column) in info.columns[..info.n_cols]
            .iter()
            .chain(&info.include_columns[..info.n_include_cols])
            .enumerate()
        {
            // An expression key returns no column (`check_index_only`).
            if usize::from(column) < 64
                && (position >= info.n_cols || !info.expression_keys[position])
            {
                covered |= 1u64 << column;
            }
        }
        needed & !covered == 0
    }

    /// `btcostestimate`: startup and total cost, selectivity, correlation.
    /// The cost of evaluating the operands an index's conditions compare
    /// its keys with: a constant or an InitPlan's output costs nothing, a
    /// stable expression its functions.
    fn operand_cost(&self, clauses: &[IndexClause<'a>]) -> f64 {
        let planner = self.planner;
        let columns = crate::sql::exec::AliasedDefCols {
            definition: crate::storage::RelationDefinition::from_definition(planner.definition),
            alias: None,
            output_aliases: &[],
            remote_ctid: false,
        };
        clauses
            .iter()
            .filter_map(|clause| match clause.restriction.clause {
                Clause::Compare { operand, .. }
                    if !operand.is_constant(false)
                        && !matches!(operand, Expr::Param(_) | Expr::Subquery(_)) =>
                {
                    Some(
                        super::super::plan::qual_cost(
                            operand,
                            &columns,
                            planner.storage,
                            planner.txid,
                            self.arena,
                        ) / 2.0
                            * self.costs().cpu_operator,
                    )
                }
                _ => None,
            })
            .sum()
    }

    fn btcostestimate(
        &self,
        index: usize,
        clauses: &[IndexClause<'a>],
    ) -> Result<(f64, f64, f64, f64), SqlError> {
        let planner = self.planner;
        let costs = self.costs();
        let info = self.index(index);
        let keys = info.catalog.n_cols;
        let tuples = planner.size.tuples;
        let index_entries = info.tuples;
        let correlation_of = |planner: &Planner<'_, '_>| {
            // An expression key's correlation is the index's own statistic,
            // a partial index's too.
            let distribution = if info.catalog.expression_keys[0] {
                if planner.statistics.index_column(info.stamp, 1).is_none() {
                    return 0.0;
                }
                planner.statistics.index_distribution(info.stamp, 1)
            } else {
                let variable = planner.variable(usize::from(info.catalog.columns[0]));
                if !variable.analyzed {
                    return 0.0;
                }
                variable.distribution
            };
            distribution
                .and_then(|distribution| distribution.correlation())
                .map_or(0.0, |correlation| {
                    let mut correlation = f64::from(correlation);
                    if info.catalog.descending[0] {
                        correlation = -correlation;
                    }
                    if keys > 1 {
                        correlation * 0.75
                    } else {
                        correlation
                    }
                })
        };
        // The boundary conditions: leading equalities and the next column's
        // ranges, with skip arrays bridging columns without one.
        let mut bound = [None::<IndexClause<'a>>; MAX_INDEX_CLAUSES];
        let mut bound_count = 0;
        let mut skip = [None::<IndexClause<'a>>; MAX_INDEX_CLAUSES];
        let mut skip_count = 0;
        let mut column = 0;
        let mut equality_here = false;
        let mut found_array = false;
        let mut found_is_null = false;
        let mut sa_scans = 1.0f64;
        let mut correlation = None;
        'clauses: for clause in clauses {
            if column < clause.position {
                let previous_scans = sa_scans;
                if equality_here {
                    column += 1;
                    skip_count = 0;
                }
                equality_here = false;
                while column < clause.position {
                    found_array = true;
                    let key = usize::from(info.catalog.columns[column]);
                    let variable = planner.variable(key);
                    let (mut distinct, default) = planner.distinct(&variable);
                    if column == 0 {
                        correlation = Some(correlation_of(planner));
                    }
                    if default {
                        sa_scans = previous_scans;
                        break;
                    }
                    if skip_count > 0 {
                        let fraction = planner.clauselist(
                            skip[..skip_count]
                                .iter()
                                .flatten()
                                .map(|clause| (clause.range(), clause.selectivity)),
                        );
                        if fraction < super::DEFAULT_RANGE_INEQ_SEL {
                            sa_scans = previous_scans;
                            break;
                        }
                        distinct = (distinct * fraction).round_ties_even().max(1.0);
                    } else {
                        distinct += 1.0;
                    }
                    sa_scans *= distinct;
                    if info.pages < sa_scans {
                        sa_scans = previous_scans;
                        break;
                    }
                    column += 1;
                    skip_count = 0;
                }
                if column != clause.position {
                    break 'clauses;
                }
            }
            if let Some(length) = clause.array_length() {
                found_array = true;
                if length > 1.0 {
                    sa_scans *= length;
                }
            }
            if matches!(
                clause.restriction.clause,
                Clause::NullTest { is_null: true, .. }
            ) {
                found_is_null = true;
                equality_here = true;
            }
            if clause.is_equality() {
                equality_here = true;
            }
            bound[bound_count] = Some(*clause);
            bound_count += 1;
            if !equality_here && column < keys - 1 {
                skip[skip_count] = Some(*clause);
                skip_count += 1;
            }
        }
        let index_tuples = if info.catalog.unique
            && column == keys - 1
            && equality_here
            && !found_array
            && !found_is_null
        {
            1.0
        } else {
            let bound = self
                .arena
                .alloc_slice_with(bound_count, |at| {
                    bound[at].expect("bound clauses are dense")
                })
                .map_err(|_| crate::sql::eval::arena_full())?;
            let selectivity = self.with_predicate(index, bound)?;
            sa_scans = sa_scans.min((info.pages * 0.333_333_3).ceil()).max(1.0);
            (selectivity * tuples / sa_scans).round_ties_even()
        };
        // `genericcostestimate`.
        let selectivity = self.with_predicate(index, clauses)?;
        let mut index_tuples = if index_tuples <= 0.0 {
            (selectivity * tuples / sa_scans).round_ties_even()
        } else {
            index_tuples
        };
        if index_tuples > index_entries {
            index_tuples = index_entries;
        }
        if index_tuples < 1.0 {
            index_tuples = 1.0;
        }
        let index_pages = if info.pages > 1.0 && index_entries > 1.0 {
            (index_tuples * info.pages / index_entries).ceil()
        } else {
            1.0
        };
        let mut total = if sa_scans > 1.0 {
            let fetched = index_pages_fetched(
                index_pages * sa_scans,
                info.pages,
                info.pages,
                costs.effective_cache_size,
            );
            fetched * costs.random_page
        } else {
            index_pages * costs.random_page
        };
        let qual_op = costs.cpu_operator * clauses.len() as f64;
        // `index_other_operands_eval_cost`: the index conditions' other
        // operands are evaluated once, when the scan starts.
        let operands = self.operand_cost(clauses);
        total += operands;
        total += index_tuples * sa_scans * (costs.cpu_index_tuple + qual_op);
        let mut startup = operands;
        if index_entries > 1.0 {
            let descent = (index_entries.ln() / 2.0f64.ln()).ceil() * costs.cpu_operator;
            startup += descent;
            total += sa_scans * descent;
        }
        let descent = f64::from(info.height + 1) * PAGE_CPU_MULTIPLIER * costs.cpu_operator;
        startup += descent;
        total += sa_scans * descent;
        let correlation = correlation.unwrap_or_else(|| correlation_of(planner));
        Ok((startup, total, selectivity, correlation))
    }

    /// `create_index_path` with `cost_index`.
    fn index_path(
        &mut self,
        index: usize,
        clauses: &'a [IndexClause<'a>],
        backward: bool,
        index_only: bool,
        presorted: usize,
    ) -> Result<Result<usize, Shape>, SqlError> {
        let planner = self.planner;
        let costs = self.costs();
        let size = planner.size;
        let (index_startup, index_total, selectivity, correlation) =
            self.btcostestimate(index, clauses)?;
        let info = self.index(index);
        // The restrictions the index does not apply.
        let mut other = QualCost::default();
        for restriction in self.restrictions {
            if !clauses
                .iter()
                .any(|clause| core::ptr::eq(clause.restriction, restriction))
                && !self.implied_by_predicate(index, restriction)
            {
                other = other.add(restriction.cost);
            }
        }
        let tuples_fetched = clamp_row_est(selectivity * size.tuples);
        let visible_fraction = if index_only {
            1.0 - size.allvisfrac
        } else {
            1.0
        };
        let total_pages = (size.pages + info.pages).max(1.0);
        let pages_fetched = (index_pages_fetched(
            tuples_fetched,
            size.pages,
            total_pages,
            costs.effective_cache_size,
        ) * visible_fraction)
            .ceil();
        let max_io = pages_fetched * costs.random_page;
        let minimum_pages = ((selectivity * size.pages).ceil() * visible_fraction).ceil();
        let min_io = if minimum_pages > 0.0 {
            costs.random_page + (minimum_pages - 1.0).max(0.0) * costs.seq_page
        } else {
            0.0
        };
        let startup = index_startup + other.startup;
        let run = index_total - index_startup
            + max_io
            + correlation * correlation * (min_io - max_io)
            + (costs.cpu_tuple + other.per_tuple) * tuples_fetched;
        let path = IndexPath {
            index,
            clauses,
            backward,
            index_only,
            presorted,
            index_total,
            selectivity,
            startup,
            total: startup + run,
        };
        Ok(self.index_paths.push(path, "more than 256 index paths"))
    }

    /// A bitmap node of the index path `path`.
    fn index_node(&mut self, path: usize) -> Result<usize, Shape> {
        let index_path = self.index_paths.items[path];
        let mut clauses = 0u64;
        for clause in index_path.clauses {
            clauses |= 1u64 << clause.restriction.id.min(63);
        }
        for part in self.predicate(index_path.index) {
            clauses |= 1u64 << part.id.min(63);
        }
        let node = BitmapNode {
            kind: BitmapKind::Index(path),
            tree_cost: index_path.index_total + 0.1 * self.costs().cpu_operator * self.rows,
            selectivity: index_path.selectivity,
            total: index_path.total,
            clauses,
        };
        self.nodes.push(node, "more than 256 bitmap nodes")
    }

    /// `cost_bitmap_heap_scan` over a bitmap whose tree costs `tree_cost`
    /// and keeps `selectivity` of the rows: startup and total cost.
    fn bitmap_heap(&self, tree_cost: f64, selectivity: f64) -> (f64, f64) {
        let costs = self.costs();
        let size = self.planner.size;
        let tuples_fetched = clamp_row_est(selectivity * size.tuples);
        let pages = if size.pages > 1.0 { size.pages } else { 1.0 };
        let mut pages_fetched = (2.0 * pages * tuples_fetched) / (2.0 * pages + tuples_fetched);
        if pages_fetched >= pages {
            pages_fetched = pages;
        } else {
            pages_fetched = pages_fetched.ceil();
        }
        let cost_per_page = if pages_fetched >= 2.0 {
            costs.random_page
                - (costs.random_page - costs.seq_page) * (pages_fetched / pages).sqrt()
        } else {
            costs.random_page
        };
        let startup = tree_cost + self.all_cost.startup;
        let run = pages_fetched * cost_per_page
            + (costs.cpu_tuple + self.all_cost.per_tuple) * tuples_fetched;
        (startup, startup + run)
    }

    /// `create_bitmap_and_path` or `create_bitmap_or_path` of `children`.
    fn combine(&mut self, children: &[usize], and: bool) -> Built<usize> {
        let operator = self.costs().cpu_operator;
        let mut cost = 0.0;
        let mut selectivity = if and { 1.0 } else { 0.0 };
        let mut clauses = 0u64;
        for (position, &child) in children.iter().enumerate() {
            let node = self.nodes.items[child];
            cost += node.tree_cost;
            clauses |= node.clauses;
            if and {
                selectivity *= node.selectivity;
                if position > 0 {
                    cost += 100.0 * operator;
                }
            } else {
                selectivity += node.selectivity;
                if position > 0 && !matches!(node.kind, BitmapKind::Index(_)) {
                    cost += 100.0 * operator;
                }
            }
        }
        if !and {
            selectivity = selectivity.min(1.0);
        }
        let children = &*self
            .arena
            .alloc_slice_copy(children)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let node = BitmapNode {
            kind: if and {
                BitmapKind::And(children)
            } else {
                BitmapKind::Or(children)
            },
            tree_cost: cost,
            selectivity,
            total: cost,
            clauses,
        };
        Ok(self.nodes.push(node, "more than 256 bitmap nodes"))
    }

    /// `choose_bitmap_and`.
    fn choose_bitmap_and(&mut self, candidates: &[usize]) -> Built<usize> {
        if candidates.len() == 1 {
            return Ok(Ok(candidates[0]));
        }
        // Paths applying the same clauses keep the cheaper.
        let mut kept = [usize::MAX; MAX_CHILDREN];
        let mut count = 0;
        for &candidate in candidates {
            let node = self.nodes.items[candidate];
            match kept[..count]
                .iter()
                .position(|&other| self.nodes.items[other].clauses == node.clauses)
            {
                Some(at) => {
                    if node.tree_cost < self.nodes.items[kept[at]].tree_cost {
                        kept[at] = candidate;
                    }
                }
                None => {
                    if count == MAX_CHILDREN {
                        return Ok(Err("more than 64 bitmap candidates"));
                    }
                    kept[count] = candidate;
                    count += 1;
                }
            }
        }
        if count == 1 {
            return Ok(Ok(kept[0]));
        }
        let kept = &mut kept[..count];
        // `path_usage_comparator`, stably.
        for at in 1..kept.len() {
            let mut at = at;
            while at > 0 && {
                let (left, right) = (self.nodes.items[kept[at - 1]], self.nodes.items[kept[at]]);
                left.tree_cost > right.tree_cost
                    || (left.tree_cost == right.tree_cost && left.selectivity > right.selectivity)
            } {
                kept.swap(at - 1, at);
                at -= 1;
            }
        }
        let mut best = [usize::MAX; MAX_CHILDREN];
        let mut best_count = 0;
        let mut best_cost = 0.0;
        for leader in 0..kept.len() {
            let mut group = [usize::MAX; MAX_CHILDREN];
            group[0] = kept[leader];
            let mut group_count = 1;
            let first = self.nodes.items[kept[leader]];
            let mut cost_so_far = self.bitmap_heap(first.tree_cost, first.selectivity).1;
            let mut clauses_so_far = first.clauses;
            let mut quals_so_far = [Prop::And(&[]); 64];
            let mut quals_count = 0;
            self.node_props(kept[leader], false, &mut quals_so_far, &mut quals_count)?;
            for &next in &kept[leader + 1..] {
                let node = self.nodes.items[next];
                if node.clauses & clauses_so_far != 0 {
                    continue;
                }
                // A predicate the group's conditions already imply adds
                // nothing.
                let mut predicates = [Prop::And(&[]); 64];
                let mut predicate_count = 0;
                self.node_props(next, true, &mut predicates, &mut predicate_count)?;
                if predicate_count > 0 {
                    let so_far = &*self
                        .arena
                        .alloc_slice_copy(&quals_so_far[..quals_count])
                        .map_err(|_| crate::sql::eval::arena_full())?;
                    let mut redundant = false;
                    for predicate in &predicates[..predicate_count] {
                        if self.prove(predicate, &Prop::And(so_far))? {
                            redundant = true;
                            break;
                        }
                    }
                    if redundant {
                        continue;
                    }
                }
                group[group_count] = next;
                // `bitmap_and_cost_est`.
                let mut cost = 0.0;
                let mut selectivity = 1.0;
                for (position, &member) in group[..=group_count].iter().enumerate() {
                    let member = self.nodes.items[member];
                    cost += member.tree_cost;
                    selectivity *= member.selectivity;
                    if position > 0 {
                        cost += 100.0 * self.costs().cpu_operator;
                    }
                }
                let new_cost = self.bitmap_heap(cost, selectivity).1;
                if new_cost < cost_so_far {
                    cost_so_far = new_cost;
                    clauses_so_far |= node.clauses;
                    self.node_props(next, false, &mut quals_so_far, &mut quals_count)?;
                    group_count += 1;
                }
            }
            if leader == 0 || cost_so_far < best_cost {
                best[..group_count].copy_from_slice(&group[..group_count]);
                best_count = group_count;
                best_cost = cost_so_far;
            }
        }
        if best_count == 1 {
            return Ok(Ok(best[0]));
        }
        self.combine(&best[..best_count], true)
    }

    /// `find_indexpath_quals` of the bitmap `node`: its clauses as written
    /// and its indexes' predicates, or the predicates alone.
    fn node_props(
        &self,
        node: usize,
        predicates_only: bool,
        out: &mut [Prop<'a>; 64],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        match self.nodes.items[node].kind {
            BitmapKind::Index(path) => {
                let mut push = |prop: Prop<'a>| {
                    if *count < out.len() {
                        out[*count] = prop;
                        *count += 1;
                    }
                };
                let index_path = self.index_paths.items[path];
                if !predicates_only {
                    for clause in index_path.clauses {
                        push(Prop::of(clause.restriction, self.arena)?);
                    }
                }
                for part in self.predicate(index_path.index) {
                    push(Prop::of(part, self.arena)?);
                }
            }
            BitmapKind::And(children) | BitmapKind::Or(children) => {
                for &child in children {
                    self.node_props(child, predicates_only, out, count)?;
                }
            }
        }
        Ok(())
    }

    /// `build_paths_for_OR`: each index's bitmap path applying some of
    /// `clauses`, with whatever of `other` it can apply too.
    fn paths_for_or(
        &mut self,
        clauses: &[&'a Restriction<'a>],
        other: &[&'a Restriction<'a>],
        out: &mut [usize; MAX_CHILDREN],
        out_count: &mut usize,
    ) -> Built<()> {
        for index in 0..self.planner.indexes.len() {
            if !self.usable[index] {
                continue;
            }
            // A partial index serves an arm whose conditions, with the
            // others, imply its predicate; it is useful by its predicate
            // alone when the others do not.
            let mut useful_predicate = false;
            if self.planner.indexes[index].catalog.partial && !self.pred_ok[index] {
                let predicate = Prop::all(self.predicate(index), self.arena)?;
                let mut both = [Prop::And(&[]); 2 * clauses::MAX_CONJUNCTS];
                let mut both_count = 0;
                for &restriction in clauses.iter().chain(other) {
                    if both_count < both.len() {
                        both[both_count] = Prop::of(restriction, self.arena)?;
                        both_count += 1;
                    }
                }
                let both = &*self
                    .arena
                    .alloc_slice_copy(&both[..both_count])
                    .map_err(|_| crate::sql::eval::arena_full())?;
                if !self.prove(&predicate, &Prop::And(both))? {
                    continue;
                }
                let others = &both[clauses.len()..];
                useful_predicate = !self.prove(&predicate, &Prop::And(others))?;
            }
            let own = outside!(self.index_clauses(index, &[clauses])?);
            if own.is_empty() && !useful_predicate {
                continue;
            }
            let all = outside!(self.index_clauses(index, &[clauses, other])?);
            let path = outside!(self.index_path(index, all, false, false, 0)?);
            let node = outside!(self.index_node(path));
            if *out_count == MAX_CHILDREN {
                return Ok(Err("more than 64 bitmap candidates"));
            }
            out[*out_count] = node;
            *out_count += 1;
        }
        Ok(Ok(()))
    }

    /// `group_similar_or_args`: the arms of `arms`, those comparing the
    /// same index column by one operator grouped, in the order of each
    /// group's first arm; `None` when no arm matches an index column.
    fn group_or_arms(
        &self,
        arms: &[&'a [Restriction<'a>]],
        groups: &mut [u8; 32],
    ) -> Option<usize> {
        // (index, key position, comparison, constant kind) per arm.
        let mut matches = [None::<(usize, usize, Comparison, u8)>; 32];
        let mut matched = false;
        for (at, arm) in arms.iter().enumerate() {
            let [arm] = arm else {
                continue;
            };
            let Clause::Compare {
                column,
                comparison,
                constant,
                ..
            } = arm.clause
            else {
                continue;
            };
            let kind = constant.map_or(u8::MAX, |constant| datum_kind(&constant));
            'indexes: for index in 0..self.planner.indexes.len() {
                if !self.usable[index] {
                    continue;
                }
                for (position, key) in self.index(index).key_columns().enumerate() {
                    if key == column {
                        matches[at] = Some((index, position, comparison, kind));
                        matched = true;
                        break 'indexes;
                    }
                }
            }
        }
        if !matched {
            return None;
        }
        // Each arm's group is the first arm with its match.
        for at in 0..arms.len() {
            groups[at] = at as u8;
            if let Some(key) = matches[at]
                && let Some(first) = (0..at).find(|&earlier| matches[earlier] == Some(key))
            {
                groups[at] = groups[first];
            }
        }
        Some(arms.len())
    }

    /// `generate_bitmap_or_paths` over the `OR` restrictions of `clauses`.
    fn or_paths(
        &mut self,
        clauses: &[&'a Restriction<'a>],
        other: &[&'a Restriction<'a>],
        out: &mut [usize; MAX_CHILDREN],
        out_count: &mut usize,
    ) -> Built<()> {
        let Some(&filler) = clauses
            .iter()
            .find(|restriction| matches!(restriction.clause, Clause::Or(_)))
        else {
            return Ok(Ok(()));
        };
        let mut all = [filler; 2 * clauses::MAX_CONJUNCTS];
        if clauses.len() + other.len() > all.len() {
            return Ok(Err("more than 64 conditions"));
        }
        all[..clauses.len()].copy_from_slice(clauses);
        all[clauses.len()..clauses.len() + other.len()].copy_from_slice(other);
        let all = &all[..clauses.len() + other.len()];
        for &restriction in clauses {
            let Clause::Or(arms) = restriction.clause else {
                continue;
            };
            let mut groups = [0u8; 32];
            let grouped = self.group_or_arms(arms, &mut groups).is_some();
            let mut inner_other = [filler; 2 * clauses::MAX_CONJUNCTS];
            let mut inner_count = 0;
            if grouped {
                for &clause in all {
                    if !core::ptr::eq(clause, restriction) {
                        inner_other[inner_count] = clause;
                        inner_count += 1;
                    }
                }
            }
            let inner_other = &inner_other[..inner_count];
            let mut children = [usize::MAX; MAX_CHILDREN];
            let mut child_count = 0;
            let mut failed = false;
            for (at, arm) in arms.iter().enumerate() {
                if grouped && usize::from(groups[at]) != at {
                    continue;
                }
                let members = if grouped {
                    (at..arms.len())
                        .filter(|&member| usize::from(groups[member]) == at)
                        .count()
                } else {
                    1
                };
                let mut found = [usize::MAX; MAX_CHILDREN];
                let mut found_count = 0;
                if members > 1 {
                    let group = outside!(self.group_restriction(restriction, arms, &groups, at)?);
                    let mut paths = [usize::MAX; MAX_CHILDREN];
                    let mut path_count = 0;
                    outside!(self.paths_for_or_group(
                        group,
                        inner_other,
                        &mut paths,
                        &mut path_count
                    )?);
                    if path_count == 0 {
                        failed = true;
                        break;
                    }
                    for &path in &paths[..path_count] {
                        if child_count == MAX_CHILDREN {
                            return Ok(Err("more than 64 bitmap candidates"));
                        }
                        children[child_count] = path;
                        child_count += 1;
                    }
                    continue;
                }
                if arm.len() > 1 {
                    let conjuncts = self
                        .arena
                        .alloc_slice_with(arm.len(), |position| &arm[position])
                        .map_err(|_| crate::sql::eval::arena_full())?;
                    outside!(self.paths_for_or(conjuncts, all, &mut found, &mut found_count)?);
                    outside!(self.or_paths(conjuncts, all, &mut found, &mut found_count)?);
                } else {
                    outside!(self.paths_for_or(&[&arm[0]], all, &mut found, &mut found_count)?);
                }
                if found_count == 0 {
                    failed = true;
                    break;
                }
                let chosen = outside!(self.choose_bitmap_and(&found[..found_count])?);
                if child_count == MAX_CHILDREN {
                    return Ok(Err("more than 64 bitmap candidates"));
                }
                children[child_count] = chosen;
                child_count += 1;
            }
            if failed || child_count == 0 {
                continue;
            }
            let node = outside!(self.combine(&children[..child_count], false)?);
            if *out_count == MAX_CHILDREN {
                return Ok(Err("more than 64 bitmap candidates"));
            }
            out[*out_count] = node;
            *out_count += 1;
        }
        Ok(Ok(()))
    }

    /// The `OR` of the arms of `restriction` in the group led by `leader`.
    fn group_restriction(
        &mut self,
        restriction: &'a Restriction<'a>,
        arms: &'a [&'a [Restriction<'a>]],
        groups: &[u8; 32],
        leader: usize,
    ) -> Built<&'a Restriction<'a>> {
        let members = (leader..arms.len())
            .filter(|&member| usize::from(groups[member]) == leader)
            .count();
        if members == arms.len() {
            return Ok(Ok(restriction));
        }
        let mut selected: [&'a [Restriction<'a>]; 32] = [&[]; 32];
        let mut count = 0;
        let mut source: Option<&'a Expr<'a>> = None;
        for member in leader..arms.len() {
            if usize::from(groups[member]) != leader {
                continue;
            }
            selected[count] = arms[member];
            count += 1;
            let arm = arms[member][0].source;
            source = Some(match source {
                None => arm,
                Some(left) => &*self
                    .arena
                    .alloc(Expr::Binary {
                        source_position: SourcePosition::UNKNOWN,
                        operator: BinaryOp::Or,
                        left,
                        right: arm,
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?,
            });
        }
        let selected = &*self
            .arena
            .alloc_slice_copy(&selected[..count])
            .map_err(|_| crate::sql::eval::arena_full())?;
        let mut selectivity = 0.0;
        let mut cost = QualCost::default();
        for arm in selected {
            let arm_selectivity = self.planner.selectivity(arm);
            selectivity = selectivity + arm_selectivity - selectivity * arm_selectivity;
            cost = arm
                .iter()
                .fold(cost, |cost, restriction| cost.add(restriction.cost));
        }
        if self.next_id >= 64 {
            return Ok(Err("more than 64 distinct conditions"));
        }
        let id = self.next_id;
        self.next_id += 1;
        let group = self
            .arena
            .alloc(Restriction {
                // An OR the planner builds keeps its arms apart from the
                // OR around it.
                source: match source.expect("a group has arms") {
                    Expr::Binary {
                        operator: BinaryOp::Or,
                        left,
                        right,
                        ..
                    } => &*self
                        .arena
                        .alloc(Expr::Binary {
                            source_position: SourcePosition::UNKNOWN.expanded(),
                            operator: BinaryOp::Or,
                            left,
                            right,
                        })
                        .map_err(|_| crate::sql::eval::arena_full())?,
                    other => other,
                },
                clause: Clause::Or(selected),
                selectivity,
                cost,
                id,
                lossy_of: None,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        Ok(Ok(&*group))
    }

    /// `make_bitmap_paths_for_or_group`: the group's paths applied jointly,
    /// or arm by arm when that costs less.
    fn paths_for_or_group(
        &mut self,
        group: &'a Restriction<'a>,
        other: &[&'a Restriction<'a>],
        out: &mut [usize; MAX_CHILDREN],
        out_count: &mut usize,
    ) -> Built<()> {
        let mut found = [usize::MAX; MAX_CHILDREN];
        let mut found_count = 0;
        outside!(self.paths_for_or(&[group], other, &mut found, &mut found_count)?);
        let joint = if found_count > 0 {
            let node = outside!(self.choose_bitmap_and(&found[..found_count])?);
            Some((node, self.nodes.items[node].total))
        } else {
            None
        };
        if let Some((node, _)) = joint
            && other.is_empty()
        {
            out[*out_count] = node;
            *out_count += 1;
            return Ok(Ok(()));
        }
        let Clause::Or(arms) = group.clause else {
            unreachable!("a group is an OR");
        };
        let mut split = [usize::MAX; MAX_CHILDREN];
        let mut split_count = 0;
        let mut split_cost = 0.0;
        for arm in arms {
            let mut found = [usize::MAX; MAX_CHILDREN];
            let mut found_count = 0;
            let conjuncts = self
                .arena
                .alloc_slice_with(arm.len(), |position| &arm[position])
                .map_err(|_| crate::sql::eval::arena_full())?;
            outside!(self.paths_for_or(conjuncts, other, &mut found, &mut found_count)?);
            if found_count == 0 {
                split_count = 0;
                break;
            }
            let node = outside!(self.choose_bitmap_and(&found[..found_count])?);
            split_cost += self.nodes.items[node].total;
            split[split_count] = node;
            split_count += 1;
        }
        let use_split = match joint {
            None => true,
            Some(_) if split_count == 0 => false,
            Some((_, joint_cost)) => joint_cost >= split_cost,
        };
        if use_split {
            for &node in &split[..split_count] {
                if *out_count == MAX_CHILDREN {
                    return Ok(Err("more than 64 bitmap candidates"));
                }
                out[*out_count] = node;
                *out_count += 1;
            }
        } else if let Some((node, _)) = joint {
            out[*out_count] = node;
            *out_count += 1;
        }
        Ok(Ok(()))
    }
}

impl<'a> Paths<'_, '_, 'a> {
    /// Whether the index path searches an array or skips a key column over
    /// an index of more than one leaf page.
    fn searches_unmodeled(&self, path: &IndexPath<'a>) -> bool {
        let info = self.index(path.index);
        if info.pages <= 2.0 {
            return false;
        }
        let mut equal_through = 0;
        for clause in path.clauses {
            if clause.array_length().is_some() || clause.position > equal_through {
                return true;
            }
            if clause.is_equality() && clause.position == equal_through {
                equal_through += 1;
            }
        }
        false
    }

    /// [`Self::searches_unmodeled`] of any index path of the bitmap `node`.
    fn bitmap_searches_unmodeled(&self, node: usize) -> bool {
        match self.nodes.items[node].kind {
            BitmapKind::Index(path) => self.searches_unmodeled(&self.index_paths.items[path]),
            BitmapKind::And(children) | BitmapKind::Or(children) => children
                .iter()
                .any(|&child| self.bitmap_searches_unmodeled(child)),
        }
    }

    /// Whether an index could serve `restriction`: it matches a key column
    /// of a modeled index, or it is a condition the model has no form for
    /// over one.
    fn serves(&self, restriction: &'a Restriction<'a>) -> Built<bool> {
        for index in 0..self.planner.indexes.len() {
            if !self.usable[index] {
                continue;
            }
            let info = &self.index(index).catalog;
            for position in 0..info.n_cols {
                if outside!(self.match_clause(restriction, index, position)?).is_some()
                    || !self.prefix_quals(restriction, index, position).is_empty()
                {
                    return Ok(Ok(true));
                }
                if let Clause::Other {
                    columns,
                    indexable: true,
                    pattern_only,
                    ..
                } = restriction.clause
                    && columns & column_bit(usize::from(info.columns[position])) != 0
                    && (!pattern_only || info.pattern_keys[position])
                {
                    return Ok(Ok(true));
                }
            }
        }
        Ok(Ok(false))
    }

    /// Whether PostgreSQL builds any index path for the relation: one with
    /// index conditions, an ordering or the query's columns, or a
    /// `BitmapOr` each of whose arms an index serves.
    fn index_paths_possible(&self, all: &'a [&'a Restriction<'a>]) -> Built<bool> {
        for index in 0..self.planner.indexes.len() {
            if !self.usable[index] {
                continue;
            }
            if self.presorted(index, false) > 0
                || self.presorted(index, true) > 0
                || self.index_only(index)
            {
                return Ok(Ok(true));
            }
        }
        for &restriction in all {
            if outside!(self.serves(restriction)?) {
                return Ok(Ok(true));
            }
            if let Clause::Or(arms) = restriction.clause {
                let mut every = true;
                for arm in arms {
                    let mut served = false;
                    for conjunct in arm.iter() {
                        served |= outside!(self.serves(conjunct)?)
                            || matches!(conjunct.clause, Clause::Or(_));
                    }
                    every &= served;
                }
                if every {
                    return Ok(Ok(true));
                }
            }
        }
        Ok(Ok(false))
    }
}

/// A small tag of a datum's type, for grouping an `OR`'s constants.
fn datum_kind(value: &Datum<'_>) -> u8 {
    match value {
        Datum::Int2(_) => 1,
        Datum::Int4(_) => 2,
        Datum::Int8(_) => 3,
        Datum::Float4(_) => 4,
        Datum::Float8(_) => 5,
        Datum::Numeric(_) => 6,
        Datum::Text(_) => 7,
        Datum::Bpchar(_) => 8,
        Datum::Date(_) => 9,
        Datum::Timestamp(_) => 10,
        Datum::Timestamptz(_) => 11,
        Datum::Bool(_) => 12,
        _ => 0,
    }
}

/// `compare_path_costs_fuzzily` of `new` against `old`: `Some(true)` when
/// `new` is better, `Some(false)` when `old` is, `None` when they are
/// fuzzily equal; the outer `None` when they differ the other way on
/// startup.
fn fuzzily_better(
    new: &Candidate,
    old: &Candidate,
    fuzz: f64,
    startup: bool,
) -> Option<Option<bool>> {
    if new.disabled != old.disabled {
        return Some(Some(new.disabled < old.disabled));
    }
    if new.total > old.total * fuzz {
        if startup && old.startup > new.startup * fuzz {
            return None;
        }
        return Some(Some(false));
    }
    if old.total > new.total * fuzz {
        if startup && new.startup > old.startup * fuzz {
            return None;
        }
        return Some(Some(true));
    }
    if new.startup > old.startup * fuzz {
        return Some(Some(false));
    }
    if old.startup > new.startup * fuzz {
        return Some(Some(true));
    }
    Some(None)
}

/// `add_path` over paths of one relation with the same rows and
/// parameterization, whose pathkeys are prefixes of the query's: the kept
/// paths, cheapest total first.
pub(super) fn add_paths(
    paths: &[Candidate],
    consider_startup: bool,
    kept: &mut [Option<Candidate>; MAX_CANDIDATES],
) -> usize {
    let mut count = 0;
    for new in paths {
        let mut accept = true;
        let mut insert_at = 0;
        let mut position = 0;
        while position < count {
            let old = kept[position].expect("kept paths are dense");
            // `compare_pathkeys`: Some(true) when only `new` has the longer
            // pathkeys.
            let keys = (new.presorted != old.presorted).then_some(new.presorted > old.presorted);
            let mut remove = false;
            match fuzzily_better(new, &old, FUZZ_FACTOR, consider_startup) {
                None => {}
                Some(Some(true)) => remove = keys != Some(false),
                Some(Some(false)) => accept = keys == Some(true),
                Some(None) => match keys {
                    Some(true) => remove = true,
                    Some(false) => accept = false,
                    None => match fuzzily_better(new, &old, 1.000_000_000_1, consider_startup) {
                        Some(Some(true)) => remove = true,
                        _ => accept = false,
                    },
                },
            }
            if remove {
                for at in position..count - 1 {
                    kept[at] = kept[at + 1];
                }
                count -= 1;
                kept[count] = None;
                continue;
            }
            if new.disabled > old.disabled
                || (new.disabled == old.disabled && new.total >= old.total)
            {
                insert_at = position + 1;
            }
            if !accept {
                break;
            }
            position += 1;
        }
        if accept && count < MAX_CANDIDATES {
            for at in (insert_at..count).rev() {
                kept[at + 1] = kept[at];
            }
            kept[insert_at] = Some(*new);
            count += 1;
        }
    }
    count
}

/// `set_cheapest`: the lowest total cost, then startup cost, then the
/// longer pathkeys; the first on a tie.
fn cheapest_total(paths: &[Option<Candidate>]) -> Option<Candidate> {
    paths.iter().flatten().copied().reduce(|best, path| {
        let order = path
            .disabled
            .cmp(&best.disabled)
            .then(path.total.total_cmp(&best.total))
            .then(path.startup.total_cmp(&best.startup))
            .then(best.presorted.cmp(&path.presorted));
        if order.is_lt() { path } else { best }
    })
}

/// `estimate_num_groups` of the relation's `columns` over its `rows`.
pub(super) fn estimate_groups(planner: &Planner<'_, '_>, columns: &[usize], rows: f64) -> f64 {
    let input = clamp_row_est(rows);
    let mut groups = 1.0;
    let mut product = 1.0;
    let mut largest = 1.0f64;
    let mut count = 0;
    for (at, &column) in columns.iter().enumerate() {
        if columns[..at].contains(&column) {
            continue;
        }
        if planner.column_type(column) == ColType::Bool {
            groups *= 2.0;
            continue;
        }
        let variable = planner.variable(column);
        let distinct = planner.distinct(&variable).0;
        product *= distinct;
        largest = largest.max(distinct);
        count += 1;
    }
    let tuples = planner.size.tuples;
    if count > 0 && tuples > 0.0 {
        let mut clamp = tuples;
        if count > 1 {
            clamp *= 0.1;
            if clamp < largest {
                clamp = largest.min(tuples);
            }
        }
        if product > clamp {
            product = clamp;
        }
        if product > 0.0 && rows < tuples {
            product *= 1.0 - ((tuples - rows) / tuples).powf(tuples / product);
        }
        groups *= clamp_row_est(product);
    }
    groups.ceil().min(input).max(1.0)
}

/// `tuplesort_merge_order`.
fn merge_order(memory: f64) -> f64 {
    (memory / (34.0 * 8192.0)).floor().clamp(6.0, 500.0)
}

/// `cost_tuplesort`: the startup and run cost of sorting `tuples` rows of
/// `width` bytes, `limit` of them wanted.
pub(super) fn tuplesort_cost(
    tuples: f64,
    width: f64,
    limit: Option<f64>,
    costs: Costs,
) -> (f64, f64) {
    let tuples = tuples.max(2.0);
    let input_bytes = tuples * ((width as usize).next_multiple_of(8) + 24) as f64;
    let comparison = 2.0 * costs.cpu_operator;
    let (output_tuples, output_bytes) = match limit.filter(|&limit| limit > 0.0 && limit < tuples) {
        Some(limit) => (
            limit,
            limit * ((width as usize).next_multiple_of(8) + 24) as f64,
        ),
        None => (tuples, input_bytes),
    };
    let memory = f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0;
    let mut startup;
    if output_bytes > memory {
        let pages = (input_bytes / 8192.0).ceil();
        let runs = input_bytes / memory;
        let order = merge_order(memory);
        let log_runs = if runs > order {
            (runs.ln() / order.ln()).ceil()
        } else {
            1.0
        };
        let accesses = 2.0 * pages * log_runs;
        startup = comparison * tuples * tuples.log2();
        startup += accesses * (costs.seq_page * 0.75 + costs.random_page * 0.25);
    } else if tuples > 2.0 * output_tuples || input_bytes > memory {
        startup = comparison * tuples * (2.0 * output_tuples).log2();
    } else {
        startup = comparison * tuples * tuples.log2();
    }
    (startup, costs.cpu_operator * tuples)
}

/// The path PostgreSQL chooses to scan table 0 of the one-relation `scope`
/// under `condition`, reading the columns `needed` (`None` when the whole
/// row is read).
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
pub(crate) fn scan_path<'a>(
    storage: &Storage,
    scope: &QueryScope<'_>,
    txid: u32,
    condition: Option<&'a Expr<'a>>,
    needed: Option<u64>,
    params: &[Datum<'a>],
    arena: &'a Arena,
    shape: QueryShape,
) -> Result<Planned<'a>, SqlError> {
    // A generic plan knows no parameter's value; a custom plan knows the
    // values it was planned for.
    let params = if crate::sql::eval::generic_plan_active() {
        &[]
    } else if params.is_empty() {
        crate::sql::eval::custom_plan_parameters()
    } else {
        params
    };
    if scope.n != 1 || scope.derived[0].is_some() || scope.lateral[0] {
        return Ok(Planned::NotTable);
    }
    let slot = scope.slots[0];
    if slot == usize::MAX {
        return Ok(Planned::NotTable);
    }
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(Planned::NotTable);
    };
    if definition.kind != crate::storage::TableKind::Local {
        return Ok(Planned::NotTable);
    }
    // A partition or inheritance tree is planned member by member.
    if definition.partition.is_partitioned()
        || (!shape.only && storage.relation_has_descendants(slot, txid))
    {
        return super::append::tree_path(
            storage, scope, txid, condition, needed, params, arena, shape, slot, definition,
        );
    }
    // Extended statistics change PostgreSQL's selectivities in ways the
    // model leaves out, which matters only among index paths.
    let extended_statistics = storage
        .extended_statistics_for_table(slot, txid)
        .next()
        .is_some();
    let statistics = storage.table_statistics(slot, txid);
    let size = super::relation_size(storage, slot, txid);
    // The table's indexes, newest first as `get_relation_info` lists them.
    let Some(catalog) = catalog_indexes(storage, txid, slot, arena)? else {
        return Ok(Planned::Outside("more than 32 indexes"));
    };
    let index_count = catalog.len();
    // The key expressions, each index's renumbered to them.
    let (variables, unread_expressions) = super::expressions::table_expressions(
        storage,
        txid,
        slot,
        definition,
        &statistics,
        catalog,
        arena,
    )?;
    let entries = if statistics.valid {
        statistics.rows as f64
    } else {
        storage.planning_row_estimate(slot) as f64
    };
    let indexes = arena
        .alloc_slice_with(index_count, |position| {
            let index = catalog[position];
            IndexInfo {
                catalog: index,
                stamp: storage
                    .indexes_for_slot(slot, txid)
                    .find(|definition| definition.name_for(txid).as_str() == index.name.as_str())
                    .map_or(0, |definition| definition.created_at),
                pages: 1.0,
                height: 0,
                deduplicated: false,
                tuples: size.tuples,
            }
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut planner = Planner {
        variables,
        storage,
        txid,
        slot,
        definition,
        statistics: &statistics,
        statistics_key: 0,
        size,
        costs: Costs::current(),
        indexes,
        arena,
    };
    // The btree layouts, which read the planner's distinct estimates.
    let mut layouts = [(1.0, 0u32, false); MAX_INDEXES];
    for (position, index) in planner.indexes.iter().enumerate() {
        if index.catalog.method == IndexAccessMethod::Btree {
            layouts[position] = super::btree_layout(&planner, index, entries);
        }
    }
    let indexes = arena
        .alloc_slice_with(index_count, |position| IndexInfo {
            pages: layouts[position].0,
            height: layouts[position].1,
            deduplicated: layouts[position].2,
            ..planner.indexes[position]
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    planner.indexes = indexes;
    let usable: &mut [bool] = arena
        .alloc_slice_with(index_count, |position| {
            let index = &planner.indexes[position].catalog;
            index.method == IndexAccessMethod::Btree
                && !index.catalog_classes
                && unread_expressions & (1 << position.min(63)) == 0
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    // The restrictions: conditions without the table's columns are
    // constants or one-time filters above the scan.
    let mut conjuncts = [&Expr::Null; clauses::MAX_CONJUNCTS];
    let mut count = 0;
    if let Some(condition) = condition
        && !super::super::plan::flatten_and(condition, &mut conjuncts, &mut count)
    {
        return Ok(Planned::Outside("more than 32 conditions"));
    }
    let mut kept_conjuncts = [&Expr::Null; clauses::MAX_CONJUNCTS];
    let mut kept_count = 0;
    for &conjunct in &conjuncts[..count] {
        // A column of the table, or its whole row, reads it.
        let mut reads_table = false;
        conjunct.for_each_column_reference(&mut |qualifier, name| {
            if matches!(
                scope.find_column(qualifier, name),
                Ok(super::super::scope::ResolvedColumn::Table(0, _))
            ) || (qualifier.is_none() && name == scope.names[0])
            {
                reads_table = true;
            }
        });
        let _ = super::super::walk_expression_tree(conjunct, &mut |node| {
            reads_table |= matches!(node, Expr::WholeRow(_));
        });
        if !reads_table && conjunct.contains_volatile_function().is_none() {
            if conjunct.is_constant(false) && !conjunct.calls_routine() {
                match crate::sql::eval::eval(conjunct, arena, params, &crate::sql::eval::NoColumns)
                {
                    Ok(Datum::Bool(true)) => continue,
                    // A false condition leaves no scan.
                    Ok(Datum::Bool(false) | Datum::Null) => return Ok(Planned::NotTable),
                    _ => {}
                }
            } else {
                continue;
            }
        }
        kept_conjuncts[kept_count] = conjunct;
        kept_count += 1;
    }
    let mut seen = [&Expr::Null; 64];
    let mut analyzer = Analyzer {
        planner: &planner,
        scope: scope_ref(scope),
        params,
        arena,
        seen: &mut seen,
        seen_count: 0,
        expressions: [&Expr::Null; clauses::MAX_CONJUNCTS],
        expression_count: 0,
    };
    let restrictions = match analyzer.conjuncts(&kept_conjuncts[..kept_count], 0)? {
        Ok(restrictions) => restrictions,
        Err(clauses::CONTRADICTION) => return Ok(Planned::NotTable),
        Err(shape) => return Ok(Planned::Outside(shape)),
    };
    // Each partial btree's predicate, read as restrictions.
    let mut predicates: [&'a [Restriction<'a>]; MAX_INDEXES] = [&[]; MAX_INDEXES];
    let mut predicate_unread = [false; MAX_INDEXES];
    for position in 0..index_count {
        let index = planner.indexes[position].catalog;
        if !usable[position] || !index.partial {
            continue;
        }
        let Some(expression) = partial_predicate(storage, txid, slot, &index, arena)? else {
            predicate_unread[position] = true;
            continue;
        };
        match analyzer.and_list(expression, 0)? {
            Ok(predicate)
                if predicate
                    .iter()
                    .all(|part| part.clause.unmodeled().is_none()) =>
            {
                predicates[position] = predicate;
            }
            _ => predicate_unread[position] = true,
        }
    }
    let next_id = analyzer.seen_count;
    // A partial index serves the query when its predicate follows from the
    // restrictions (`check_index_predicates`), and holds the rows its
    // predicate keeps.
    let compare = |column: usize, left: &Datum<'_>, right: &Datum<'_>| {
        planner.compare(planner.column_collation(column), left, right)
    };
    let all_props = Prop::all(restrictions, arena)?;
    let mut pred_ok = [false; MAX_INDEXES];
    let mut partial_layouts = [None::<(f64, u32, bool, f64)>; MAX_INDEXES];
    for position in 0..index_count {
        let index = planner.indexes[position].catalog;
        if !usable[position] || !index.partial {
            continue;
        }
        if predicate_unread[position] {
            // Only a query reading its columns could prove the predicate.
            if restrictions
                .iter()
                .any(|restriction| restriction.clause.columns() & index_columns_mask(&index) != 0)
                || !restrictions.is_empty()
            {
                return Ok(Planned::Outside(
                    "a partial index whose predicate the model does not read",
                ));
            }
            usable[position] = false;
            continue;
        }
        let predicate_props = Prop::all(predicates[position], arena)?;
        pred_ok[position] = !restrictions.is_empty()
            && predicate::implied_by(&predicate_props, &all_props, &compare, arena)?;
        let Some(entries) = planner.rows_matching(&predicate_props)? else {
            return Ok(Planned::Outside(
                "a partial index whose predicate the model does not read",
            ));
        };
        let (pages, height, deduplicated) =
            super::btree_layout(&planner, &planner.indexes[position], entries);
        partial_layouts[position] = Some((pages, height, deduplicated, entries.min(size.tuples)));
    }
    // The restrictions each usable predicate implies (`indrestrictinfo`); a
    // write's target keeps them all.
    let mut predicate_implied = [0u64; MAX_INDEXES];
    for position in 0..index_count {
        if !pred_ok[position] || shape.target {
            continue;
        }
        let predicate_props = Prop::all(predicates[position], arena)?;
        for (at, restriction) in restrictions.iter().enumerate() {
            let target = Prop::of(restriction, arena)?;
            if at < 64 && predicate::implied_by(&target, &predicate_props, &compare, arena)? {
                predicate_implied[position] |= 1 << at;
            }
        }
    }
    let indexes = arena
        .alloc_slice_with(index_count, |position| match partial_layouts[position] {
            Some((pages, height, deduplicated, tuples)) => IndexInfo {
                pages,
                height,
                deduplicated,
                tuples,
                ..planner.indexes[position]
            },
            None => planner.indexes[position],
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    planner.indexes = indexes;
    // Constant equalities fix their columns' order.
    let mut fixed = 0u64;
    for restriction in restrictions {
        if let Some(column) = restriction.clause.constant_equality() {
            fixed |= column_bit(column);
        }
    }
    let mut order = [OrderKey::Expression; super::MAX_ORDER_KEYS];
    let mut order_count = 0;
    for key in shape.order() {
        if let OrderKey::Column { column, .. } = *key
            && fixed & column_bit(column) != 0
        {
            continue;
        }
        order[order_count] = *key;
        order_count += 1;
    }
    let order = &order[..order_count];
    let unmodeled = restrictions
        .iter()
        .find_map(|restriction| restriction.clause.unmodeled());
    // An index the model does not cover is outside it when the query could
    // use it.
    for (position, index) in planner.indexes.iter().enumerate() {
        if usable[position] {
            continue;
        }
        let catalog_index = &index.catalog;
        let mut index_columns = 0u64;
        // An expression key the model reads is the variable it was
        // renumbered to.
        let read_expression = |position: usize| {
            planner
                .variables
                .get(usize::from(catalog_index.columns[position]))
                .is_some()
        };
        for (position, &column) in catalog_index.columns[..catalog_index.n_cols]
            .iter()
            .enumerate()
        {
            if !catalog_index.expression_keys[position] || read_expression(position) {
                index_columns |= column_bit(usize::from(column));
            }
        }
        let restricted = restrictions.iter().fold(0u64, |mask, restriction| {
            mask | restriction.clause.columns()
        });
        // A GiST or SP-GiST index may return the columns it holds.
        let mut covered = index_columns;
        for &column in &catalog_index.include_columns[..catalog_index.n_include_cols] {
            covered |= column_bit(usize::from(column));
        }
        let returns = matches!(
            catalog_index.method,
            IndexAccessMethod::Gist | IndexAccessMethod::SpGist
        ) && needed.is_some_and(|needed| needed & !covered == 0);
        // A partial btree whose predicate the model does not read serves no
        // query without conditions.
        let unread_partial = catalog_index.partial
            && (catalog_index.method != IndexAccessMethod::Btree || !restrictions.is_empty());
        // An expression key serves a condition on an expression, which a
        // condition the model has no form for may be, or an ORDER BY
        // expression; one it reads also serves its variable's conditions.
        let expression_served = catalog_index.expression_keys[..catalog_index.n_cols]
            .iter()
            .any(|&expression| expression)
            && (unmodeled.is_some() || order.contains(&OrderKey::Expression));
        // A GiST or SP-GiST index may order by a distance (`<->`).
        let orders_by_distance = matches!(
            catalog_index.method,
            IndexAccessMethod::Gist | IndexAccessMethod::SpGist
        ) && order.contains(&OrderKey::Expression);
        let relevant = expression_served
            || unread_partial
            || returns
            || orders_by_distance
            || index_columns & restricted != 0
            || order.iter().any(|key| {
                matches!(key, OrderKey::Column { column, .. } if index_columns & column_bit(*column) != 0)
            });
        if relevant {
            return Ok(Planned::Outside(match catalog_index.method {
                IndexAccessMethod::Hash => "a hash index",
                IndexAccessMethod::Gist => "a GiST index",
                IndexAccessMethod::Gin => "a GIN index",
                IndexAccessMethod::Brin => "a BRIN index",
                IndexAccessMethod::SpGist => "an SP-GiST index",
                IndexAccessMethod::Btree if catalog_index.partial => "a partial index",
                IndexAccessMethod::Btree
                    if catalog_index.expression_keys[..catalog_index.n_cols]
                        .iter()
                        .any(|&expression| expression) =>
                {
                    "an expression index"
                }
                _ => "an index with a non-default operator class or collation",
            }));
        }
    }
    let all_cost = restrictions
        .iter()
        .fold(QualCost::default(), |total, restriction| {
            total.add(restriction.cost)
        });
    // Each index builds at most two paths of its own and one per OR arm it
    // serves, grouped or split.
    let arms: usize = restrictions
        .iter()
        .map(|restriction| or_arms(&restriction.clause))
        .sum();
    let path_capacity = (index_count * (2 + 2 * arms) + 4).min(MAX_INDEX_PATHS);
    let mut paths = Paths {
        planner: &planner,
        arena,
        restrictions,
        usable,
        order,
        fixed,
        needed,
        rows: f64::NAN,
        all_cost,
        index_paths: Pool::new(
            arena,
            path_capacity,
            IndexPath {
                index: 0,
                clauses: &[],
                backward: false,
                index_only: false,
                presorted: 0,
                index_total: 0.0,
                selectivity: 1.0,
                startup: 0.0,
                total: 0.0,
            },
        )?,
        nodes: Pool::new(
            arena,
            2 * path_capacity,
            BitmapNode {
                kind: BitmapKind::Index(0),
                tree_cost: 0.0,
                selectivity: 1.0,
                total: 0.0,
                clauses: 0,
            },
        )?,
        next_id,
        predicates: &predicates[..index_count],
        pred_ok: &pred_ok[..index_count],
        predicate_implied: &predicate_implied[..index_count],
    };
    // With a condition of unknown selectivity only a plan without index
    // paths is exact: a sequential scan, sorted when asked.
    let all_restrictions = arena
        .alloc_slice_with(restrictions.len(), |position| &restrictions[position])
        .map_err(|_| crate::sql::eval::arena_full())?;
    if shape.probe {
        return Ok(match paths.index_paths_possible(all_restrictions)? {
            Ok(false) => Planned::SeqOnly,
            Ok(true) => Planned::Outside("index paths"),
            Err(shape) => Planned::Outside(shape),
        });
    }
    let extended = extended_statistics.then_some("a table with extended statistics");
    if let Some(shape) = unmodeled.or(shape.unmodeled).or(extended) {
        return Ok(match paths.index_paths_possible(all_restrictions)? {
            Ok(false) => Planned::SeqOnly,
            Ok(true) => Planned::Outside(shape),
            Err(shape) => Planned::Outside(shape),
        });
    }
    let rows = clamp_row_est(planner.size.tuples * planner.selectivity(restrictions));
    paths.rows = rows;
    match build(
        &mut paths,
        all_restrictions,
        shape,
        definition,
        needed,
        rows,
    )? {
        Ok(planned) => Ok(planned),
        Err(shape) => Ok(Planned::Outside(shape)),
    }
}

/// The indexes of the table in `slot`, newest first as
/// `get_relation_info` lists them; `None` past [`MAX_INDEXES`].
pub(super) fn catalog_indexes<'a>(
    storage: &Storage,
    txid: u32,
    slot: usize,
    arena: &'a Arena,
) -> Result<Option<&'a mut [PlannerIndex]>, SqlError> {
    let mut catalog = [None::<PlannerIndex>; MAX_INDEXES];
    let mut index_count = 0;
    let mut too_many = false;
    crate::sql::catalog::planner_indexes(storage, txid, slot, &mut |index| {
        if index_count == MAX_INDEXES {
            too_many = true;
            return;
        }
        catalog[index_count] = Some(index);
        index_count += 1;
    });
    if too_many {
        return Ok(None);
    }
    let catalog = &mut catalog[..index_count];
    catalog.sort_unstable_by_key(|index| core::cmp::Reverse(index.map_or(0, |index| index.oid)));
    Ok(Some(
        arena
            .alloc_slice_with(index_count, |position| {
                catalog[position].expect("dense indexes")
            })
            .map_err(|_| crate::sql::eval::arena_full())?,
    ))
}

/// The arms of the ORs in `clause`, nested ones included.
fn or_arms(clause: &Clause<'_>) -> usize {
    match clause {
        Clause::Or(arms) => arms
            .iter()
            .map(|arm| 1 + arm.iter().map(|part| or_arms(&part.clause)).sum::<usize>())
            .sum(),
        _ => 0,
    }
}

/// The predicate of the partial index `index`, bound to its table's
/// columns; `None` when its definition is not found.
fn partial_predicate<'a>(
    storage: &Storage,
    txid: u32,
    slot: usize,
    index: &PlannerIndex,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    let Some(definition) = storage
        .indexes_for_slot(slot, txid)
        .find(|definition| definition.name_for(txid).as_str() == index.name.as_str())
    else {
        return Ok(None);
    };
    let Some(text) = definition.predicate else {
        return Ok(None);
    };
    Ok(Some(
        crate::sql::exec::expression_bindings::bound_index_expression(
            text.as_str(),
            storage,
            &definition,
            crate::storage::ExpressionOwner::IndexPredicate,
            txid,
            arena,
        )?,
    ))
}

fn index_columns_mask(index: &PlannerIndex) -> u64 {
    index.columns[..index.n_cols]
        .iter()
        .fold(0u64, |mask, &column| mask | column_bit(usize::from(column)))
}

/// The same scope, borrowed for the analyzer's lifetime.
fn scope_ref<'p>(scope: &'p QueryScope<'_>) -> &'p QueryScope<'p> {
    // SAFETY: `QueryScope` is covariant in its lifetime; the analyzer only
    // reads through it while `scope` lives.
    unsafe { &*(scope as *const QueryScope<'_>).cast::<QueryScope<'p>>() }
}

/// Builds the relation's paths and chooses among them, or, for a member of
/// a partition or inheritance tree, returns its paths or describes one.
fn build<'a>(
    paths: &mut Paths<'_, '_, 'a>,
    all: &'a [&'a Restriction<'a>],
    shape: QueryShape,
    definition: &crate::storage::TableDef,
    needed: Option<u64>,
    rows: f64,
) -> Built<Planned<'a>> {
    let planner = paths.planner;
    let arena = paths.arena;
    let kept = outside!(rel_candidates(paths, all, shape, rows)?);
    let width = f64::from(shape.projection.map_or_else(
        || {
            super::output_width(definition, planner.statistics, shape.output.or(needed))
                + shape.identity_width
        },
        |projection| projection.width,
    ));
    match shape.member {
        super::Member::Paths => {
            return Ok(Ok(Planned::Members(RelPaths {
                rows,
                tuples: planner.size.tuples,
                width,
                fixed: paths.fixed,
                kept,
            })));
        }
        super::Member::Describe(candidate) => {
            let path = outside!(describe(
                paths, &candidate, &candidate, shape, definition, needed, rows
            )?);
            return Ok(Ok(Planned::Chosen(path)));
        }
        super::Member::No => {}
    }
    if let Some(grouping) = shape.grouping {
        return grouped(
            paths, kept, shape, grouping, definition, needed, rows, width,
        );
    }
    let order = paths.order;
    let groups = |presorted: usize| {
        let mut columns = [0usize; super::MAX_ORDER_KEYS];
        for (at, key) in order[..presorted].iter().enumerate() {
            if let OrderKey::Column { column, .. } = key {
                columns[at] = *column;
            }
        }
        estimate_groups(planner, &columns[..presorted], rows)
    };
    let upper = upper_paths(
        kept,
        rows,
        width,
        order.len(),
        shape,
        planner.costs,
        &groups,
        arena,
    )?;
    let mut path = outside!(describe(
        paths,
        &upper.chosen,
        &upper.scan,
        shape,
        definition,
        needed,
        rows
    )?);
    path.sort_cost = upper.sort_cost;
    path.limit_cost = upper.limit_cost;
    Ok(Ok(Planned::Chosen(path)))
}

/// `add_paths_to_grouping_rel` over the relation's `kept` paths, which no
/// index orders by a grouping column: a `GroupAggregate` over the cheapest
/// path sorted and a `HashAggregate` over it, kept as `add_path` keeps
/// them, then `create_ordered_paths` for the query's `ORDER BY`.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
fn grouped<'a>(
    paths: &mut Paths<'_, '_, 'a>,
    kept: &[Candidate],
    shape: QueryShape,
    grouping: super::GroupingShape,
    definition: &crate::storage::TableDef,
    needed: Option<u64>,
    rows: f64,
    width: f64,
) -> Built<Planned<'a>> {
    use super::aggregate::{AggInput, AggStrategy, cost_agg};
    let planner = paths.planner;
    let costs = planner.costs;
    let methods = crate::sql::guc::planner_methods();
    let columns = &grouping.columns[..grouping.len];
    // An index ordered by a grouping column gives PostgreSQL presorted
    // input paths, which the model leaves out.
    for (index, info) in planner.indexes.iter().enumerate() {
        let catalog = &info.catalog;
        if paths.usable[index]
            && catalog.n_cols > 0
            && columns.contains(&usize::from(catalog.columns[0]))
        {
            return Ok(Err("a grouping column an index orders"));
        }
    }
    let kept_options = paths
        .arena
        .alloc_slice_with(kept.len(), |at| Some(kept[at]))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let cheapest = cheapest_total(kept_options).expect("the relation keeps a path");
    let groups = estimate_groups(planner, columns, rows);
    // The sorted strategy over the cheapest path, sorted.
    let (sort_startup, sort_run) = tuplesort_cost(rows, width, None, costs);
    let input_sort = (
        cheapest.total + sort_startup,
        cheapest.total + sort_startup + sort_run,
    );
    let sorted_input = AggInput {
        startup: input_sort.0,
        total: input_sort.1,
        rows,
        width,
    };
    let (startup, total, _) = cost_agg(
        AggStrategy::Sorted,
        &grouping.aggregates,
        columns.len(),
        groups,
        sorted_input,
    );
    let sorted = Candidate {
        startup,
        total,
        presorted: columns.len(),
        disabled: cheapest.disabled + u32::from(!methods.sort),
        ..cheapest
    };
    let hashed_input = AggInput {
        startup: cheapest.startup,
        total: cheapest.total,
        rows,
        width,
    };
    let (startup, total, _) = cost_agg(
        AggStrategy::Hashed,
        &grouping.aggregates,
        columns.len(),
        groups,
        hashed_input,
    );
    let hashed = Candidate {
        startup,
        total,
        presorted: 0,
        disabled: cheapest.disabled + u32::from(!methods.hashagg),
        ..cheapest
    };
    let mut grouped_paths = [None::<Candidate>; MAX_CANDIDATES];
    let grouped_count = add_paths(&[sorted, hashed], false, &mut grouped_paths);
    let grouped_paths = &grouped_paths[..grouped_count];
    let cheapest_grouped = cheapest_total(grouped_paths).expect("a grouped path is kept");
    // `create_ordered_paths`: a path in the `ORDER BY` order as it is, the
    // cheapest one sorted.
    let (sort_startup, sort_run) = tuplesort_cost(groups, grouping.output_width, None, costs);
    let sort_groups = |path: Candidate| Candidate {
        startup: path.total + sort_startup,
        total: path.total + sort_startup + sort_run,
        disabled: path.disabled + u32::from(!methods.sort),
        ..path
    };
    let mut finals = [cheapest; MAX_CANDIDATES];
    let mut final_count = 0;
    for path in grouped_paths.iter().flatten() {
        let satisfied = match grouping.order {
            super::GroupOrder::None => true,
            super::GroupOrder::Grouped => path.presorted > 0,
            super::GroupOrder::Other => false,
        };
        finals[final_count] = if satisfied {
            *path
        } else if path.presorted == cheapest_grouped.presorted
            && path.total == cheapest_grouped.total
        {
            sort_groups(*path)
        } else {
            continue;
        };
        final_count += 1;
    }
    let mut ordered = [None::<Candidate>; MAX_CANDIDATES];
    let ordered_count = add_paths(&finals[..final_count], false, &mut ordered);
    let chosen = cheapest_total(&ordered[..ordered_count]).expect("an ordered path is kept");
    let hashed_chosen = chosen.presorted == 0;
    let aggregate = if hashed_chosen { hashed } else { sorted };
    let output_sort = (grouping.order != super::GroupOrder::None
        && (hashed_chosen || grouping.order == super::GroupOrder::Other))
        .then_some((
            aggregate.total + sort_startup,
            aggregate.total + sort_startup + sort_run,
        ));
    let mut path = outside!(describe(
        paths, &cheapest, &cheapest, shape, definition, needed, rows
    )?);
    path.grouped = Some(super::GroupedPath {
        input_sort: (!hashed_chosen).then_some(input_sort),
        planned_partitions: hashed_chosen
            .then(|| super::aggregate::planned_partitions(&grouping.aggregates, groups, width)),
        startup: aggregate.startup,
        total: aggregate.total,
        rows: groups,
        output_sort,
    });
    Ok(Ok(Planned::Chosen(path)))
}

/// A relation's paths as `add_path` keeps them, cheapest total first, each
/// with the cost of the target list it computes, and the relation's size.
#[derive(Clone, Copy)]
pub(crate) struct RelPaths<'a> {
    pub(super) rows: f64,
    pub(super) tuples: f64,
    pub(super) width: f64,
    /// The columns a condition fixes to a constant, whose sort keys the
    /// paths leave out.
    pub(super) fixed: u64,
    pub(super) kept: &'a [Candidate],
}

/// The relation's paths (`set_plain_rel_pathlist` through `add_path`), each
/// charged the target list's evaluation.
fn rel_candidates<'a>(
    paths: &mut Paths<'_, '_, 'a>,
    all: &'a [&'a Restriction<'a>],
    shape: QueryShape,
    rows: f64,
) -> Built<&'a [Candidate]> {
    let methods = crate::sql::guc::planner_methods();
    let planner = paths.planner;
    let costs = planner.costs;
    let size = planner.size;
    let arena = paths.arena;
    let order_len = paths.order.len();
    let mut candidates = [None::<Candidate>; MAX_CANDIDATES];
    let mut candidate_count = 0;
    // `cost_seqscan`.
    candidates[candidate_count] = Some(Candidate {
        kind: ScanKind::Seq,
        path: usize::MAX,
        startup: paths.all_cost.startup,
        total: paths.all_cost.startup
            + costs.seq_page * size.pages
            + (costs.cpu_tuple + paths.all_cost.per_tuple) * size.tuples,
        disabled: u32::from(!methods.seqscan),
        presorted: 0,
        sort: SortKind::None,
    });
    candidate_count += 1;
    // `create_index_paths`.
    let mut bitmap_candidates = [usize::MAX; MAX_CHILDREN];
    let mut bitmap_count = 0;
    for index in 0..planner.indexes.len() {
        let partial = planner.indexes[index].catalog.partial;
        if !paths.usable[index] || (partial && !paths.pred_ok[index]) {
            continue;
        }
        // A partial index matches the restrictions its predicate leaves.
        let own = arena
            .alloc_slice_with(all.len(), |at| all[at])
            .map_err(|_| crate::sql::eval::arena_full())?;
        let mut own_count = 0;
        for &restriction in all {
            if !paths.implied_by_predicate(index, restriction) {
                own[own_count] = restriction;
                own_count += 1;
            }
        }
        let clauses = outside!(paths.index_clauses(index, &[&own[..own_count]])?);
        let index_only = paths.index_only(index);
        let forward = paths.presorted(index, false);
        let mut built = [usize::MAX; 2];
        let mut built_count = 0;
        if !clauses.is_empty() || forward > 0 || index_only || partial {
            built[built_count] =
                outside!(paths.index_path(index, clauses, false, index_only, forward)?);
            built_count += 1;
        }
        if order_len > 0 {
            let backward = paths.presorted(index, true);
            if backward > 0 {
                built[built_count] =
                    outside!(paths.index_path(index, clauses, true, index_only, backward)?);
                built_count += 1;
            }
        }
        for &path in &built[..built_count] {
            let index_path = paths.index_paths.items[path];
            if candidate_count == MAX_CANDIDATES {
                return Ok(Err("more than 66 paths"));
            }
            candidates[candidate_count] = Some(Candidate {
                kind: if index_path.index_only {
                    ScanKind::IndexOnly
                } else {
                    ScanKind::Index
                },
                path,
                startup: index_path.startup,
                total: index_path.total,
                disabled: u32::from(!methods.indexscan),
                presorted: index_path.presorted,
                sort: SortKind::None,
            });
            candidate_count += 1;
            if index_path.presorted == 0 || index_path.selectivity < 1.0 {
                let node = outside!(paths.index_node(path));
                if bitmap_count == MAX_CHILDREN {
                    return Ok(Err("more than 64 bitmap candidates"));
                }
                bitmap_candidates[bitmap_count] = node;
                bitmap_count += 1;
            }
        }
    }
    outside!(paths.or_paths(all, &[], &mut bitmap_candidates, &mut bitmap_count)?);
    if bitmap_count > 0 {
        let node = outside!(paths.choose_bitmap_and(&bitmap_candidates[..bitmap_count])?);
        let tree = paths.nodes.items[node];
        let (startup, total) = paths.bitmap_heap(tree.tree_cost, tree.selectivity);
        if candidate_count == MAX_CANDIDATES {
            return Ok(Err("more than 66 paths"));
        }
        candidates[candidate_count] = Some(Candidate {
            kind: ScanKind::Bitmap,
            path: node,
            startup,
            total,
            disabled: u32::from(!methods.bitmapscan),
            presorted: 0,
            sort: SortKind::None,
        });
        candidate_count += 1;
    }
    let candidates = arena
        .alloc_slice_with(candidate_count, |at| {
            candidates[at].expect("dense candidates")
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut kept = [None::<Candidate>; MAX_CANDIDATES];
    let kept_count = add_paths(candidates, consider_startup(shape), &mut kept);
    // The target list's expressions are evaluated by the scan below any
    // sort.
    if let Some(projection) = shape.projection {
        for path in kept[..kept_count].iter_mut().flatten() {
            path.total += projection.per_tuple * rows;
        }
    }
    Ok(Ok(&*arena
        .alloc_slice_with(kept_count, |at| kept[at].expect("dense kept paths"))
        .map_err(|_| crate::sql::eval::arena_full())?))
}

/// `rel->consider_startup`: a `LIMIT` gives the query a tuple fraction.
fn consider_startup(shape: QueryShape) -> bool {
    shape
        .limit
        .is_some_and(|(count, _)| count.is_some_and(|count| count > 0.0))
}

/// The path the query chooses above a relation's `kept` paths, and the
/// figures of the sort and limit over it.
pub(super) struct Upper {
    pub(super) chosen: Candidate,
    /// The chosen relation path, under any sort and limit.
    pub(super) scan: Candidate,
    pub(super) sort_cost: Option<(f64, f64)>,
    pub(super) limit_cost: Option<(f64, f64, f64)>,
}

/// `create_ordered_paths` and `create_limit_path` over the relation's
/// `kept` paths of `rows` rows of `width` bytes, then the cheapest
/// (`get_cheapest_fractional_path` as `add_path` leaves it); `groups` is
/// `estimate_num_groups` of the first that many query sort keys.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
pub(super) fn upper_paths(
    kept: &[Candidate],
    rows: f64,
    width: f64,
    order_len: usize,
    shape: QueryShape,
    costs: super::Costs,
    groups: &dyn Fn(usize) -> f64,
    arena: &Arena,
) -> Result<Upper, SqlError> {
    let methods = crate::sql::guc::planner_methods();
    let consider_startup = consider_startup(shape);
    let kept_options = arena
        .alloc_slice_with(kept.len(), |at| Some(kept[at]))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let cheapest = cheapest_total(kept_options).expect("the relation keeps a path");
    let limit_tuples = shape
        .limit
        .and_then(|(count, offset)| count.map(|count| count + offset));
    // `create_ordered_paths`.
    let mut finals = [None::<Candidate>; MAX_CANDIDATES];
    let mut final_count = 0;
    for path in kept {
        let mut path = *path;
        if order_len > 0 && path.presorted < order_len {
            let incremental = path.presorted > 0 && methods.incremental_sort;
            if !path.same_scan(&cheapest) && !incremental {
                continue;
            }
            if incremental {
                let groups = groups(path.presorted);
                let input_tuples = rows.max(2.0);
                let group_tuples = input_tuples / groups;
                let input_run = path.total - path.startup;
                let group_input_run = input_run / groups;
                let (group_startup, group_run) =
                    tuplesort_cost(group_tuples, width, limit_tuples, costs);
                let startup = group_startup + path.startup + group_input_run;
                let mut run = group_run
                    + (group_run + group_startup) * (groups - 1.0)
                    + group_input_run * (groups - 1.0);
                run += (costs.cpu_tuple + 2.0 * costs.cpu_operator) * input_tuples;
                run += 2.0 * costs.cpu_tuple * groups;
                path = Candidate {
                    startup,
                    total: startup + run,
                    presorted: order_len,
                    sort: SortKind::Incremental(path.presorted),
                    ..path
                };
            } else {
                let (startup, run) = tuplesort_cost(rows, width, limit_tuples, costs);
                let startup = startup + path.total;
                path = Candidate {
                    startup,
                    total: startup + run,
                    presorted: order_len,
                    sort: SortKind::Full,
                    disabled: path.disabled + u32::from(!methods.sort),
                    ..path
                };
            }
        }
        if final_count < MAX_CANDIDATES {
            finals[final_count] = Some(path);
            final_count += 1;
        }
    }
    let finals = arena
        .alloc_slice_with(final_count, |at| finals[at].expect("dense finals"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut ordered = [None::<Candidate>; MAX_CANDIDATES];
    let ordered_count = add_paths(finals, consider_startup, &mut ordered);
    // `create_limit_path` scales each path's run cost to the rows read.
    let mut limited = [None::<Candidate>; MAX_CANDIDATES];
    let mut limited_count = 0;
    for path in ordered[..ordered_count].iter().flatten() {
        let mut path = *path;
        if let Some((count, offset)) = shape.limit {
            let mut rows_left = rows;
            if offset > 0.0 {
                let offset_rows = clamp_row_est(offset).min(rows_left);
                path.startup += (path.total - path.startup) * offset_rows / rows;
                rows_left = (rows_left - offset_rows).max(1.0);
            }
            if let Some(count) = count {
                let count_rows = clamp_row_est(count).min(rows_left);
                path.total = path.startup + (path.total - path.startup) * count_rows / rows;
            }
        }
        limited[limited_count] = Some(path);
        limited_count += 1;
    }
    let limited = arena
        .alloc_slice_with(limited_count, |at| limited[at].expect("dense limited"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut final_paths = [None::<Candidate>; MAX_CANDIDATES];
    let final_paths_count = add_paths(limited, consider_startup, &mut final_paths);
    let chosen = cheapest_total(&final_paths[..final_paths_count]).expect("a path is kept");
    // The scan's own costs, under any sort and limit.
    let scan = kept
        .iter()
        .find(|path| path.same_scan(&chosen))
        .copied()
        .unwrap_or(chosen);
    // The sort's and the limit's own figures.
    let sort_cost = if chosen.sort == SortKind::None {
        None
    } else {
        ordered[..ordered_count]
            .iter()
            .flatten()
            .find(|path| path.same_scan(&chosen))
            .map(|sorted| (sorted.startup, sorted.total))
    };
    let limit_cost = shape.limit.map(|(count, offset)| {
        let mut limit_rows = rows;
        if offset > 0.0 {
            limit_rows = (limit_rows - clamp_row_est(offset).min(limit_rows)).max(1.0);
        }
        if let Some(count) = count {
            limit_rows = clamp_row_est(count).min(limit_rows);
        }
        (chosen.startup, chosen.total, limit_rows)
    });
    Ok(Upper {
        chosen,
        scan,
        sort_cost,
        limit_cost,
    })
}

/// The chosen path as EXPLAIN and execution read it.
fn describe<'a>(
    paths: &mut Paths<'_, '_, 'a>,
    chosen: &Candidate,
    scan: &Candidate,
    shape: QueryShape,
    definition: &crate::storage::TableDef,
    needed: Option<u64>,
    rows: f64,
) -> Built<ScanPath<'a>> {
    let planner = paths.planner;
    let arena = paths.arena;
    let mut path = ScanPath {
        kind: chosen.kind,
        index_oid: 0,
        index_name: crate::util::StackStr::new(),
        keys: &[],
        conditions: &[],
        bitmap: &[],
        recheck: &[],
        absorbed: &[],
        rows,
        width: shape.projection.map_or_else(
            || {
                super::output_width(definition, planner.statistics, shape.output.or(needed))
                    + shape.identity_width
            },
            |projection| projection.width,
        ),
        startup_cost: scan.startup,
        total_cost: scan.total,
        backward: false,
        sort: chosen.sort,
        deduplicated: false,
        single_row: false,
        searches_unmodeled: false,
        predicate: &[],
        sort_cost: None,
        limit_cost: None,
        grouped: None,
        // A scan that returns the requested order stops when its limit has
        // the rows it needs.
        stop_after: match shape.limit {
            Some((Some(count), offset))
                if !paths.order.is_empty()
                    && chosen.sort == SortKind::None
                    && shape.stop_at_limit
                    && matches!(shape.member, super::Member::No) =>
            {
                Some((count.max(0.0) + offset.max(0.0)) as u64)
            }
            _ => None,
        },
    };
    match chosen.kind {
        ScanKind::Seq => {}
        ScanKind::Index | ScanKind::IndexOnly => {
            let index_path = paths.index_paths.items[chosen.path];
            let info = planner.indexes[index_path.index];
            path.index_oid = info.catalog.oid;
            path.index_name = info.catalog.name;
            path.backward = index_path.backward;
            path.deduplicated = info.deduplicated;
            path.keys = arena
                .alloc_slice_with(info.catalog.n_cols, |position| {
                    let column = usize::from(info.catalog.columns[position]);
                    IndexKeyOrder {
                        column,
                        descending: info.catalog.descending[position],
                        nulls_first: info.catalog.nulls_first[position],
                        expression: planner.variables.get(column).map(|variable| {
                            super::KeyExpression {
                                position: position as u8,
                                collation: variable.collation,
                            }
                        }),
                    }
                })
                .map_err(|_| crate::sql::eval::arena_full())?;
            // A unique index whose every key an equality fixes reaches one
            // row.
            path.single_row = info.catalog.unique
                && info.key_columns().all(|column| {
                    index_path.clauses.iter().any(|clause| {
                        clause.or_array.is_none()
                            && matches!(clause.restriction.clause,
                                Clause::Compare { column: equal, comparison: Comparison::Eq, .. }
                                if equal == column)
                    })
                });
            path.conditions = outside!(conditions(arena, index_path.clauses)?);
            path.predicate = predicate_sources(arena, paths.predicate(index_path.index))?;
            path.searches_unmodeled = paths.searches_unmodeled(&index_path);
            // The restrictions the index conditions apply or imply.
            let mut index_props = [Prop::And(&[]); MAX_INDEX_CLAUSES];
            for (at, clause) in index_path.clauses.iter().enumerate() {
                index_props[at] = index_prop(clause, arena)?;
            }
            let props = &*arena
                .alloc_slice_copy(&index_props[..index_path.clauses.len()])
                .map_err(|_| crate::sql::eval::arena_full())?;
            let mut absorbed = [&Expr::Null; clauses::MAX_CONJUNCTS];
            let mut absorbed_count = 0;
            for restriction in paths.restrictions {
                let applied = index_path
                    .clauses
                    .iter()
                    .any(|clause| core::ptr::eq(clause.restriction, restriction));
                if applied
                    || paths.implied_by_predicate(index_path.index, restriction)
                    || implied(paths, restriction, &Prop::And(props))?
                {
                    absorbed[absorbed_count] = restriction.source;
                    absorbed_count += 1;
                }
            }
            path.absorbed = arena
                .alloc_slice_copy(&absorbed[..absorbed_count])
                .map_err(|_| crate::sql::eval::arena_full())?;
        }
        ScanKind::Bitmap => {
            let mut nodes = [None::<BitmapPlanNode<'a>>; 64];
            let mut node_count = 0;
            let (quals, index_quals) = outside!(bitmap_plan(
                paths,
                chosen.path,
                1,
                &mut nodes,
                &mut node_count
            )?);
            path.bitmap = arena
                .alloc_slice_with(node_count, |at| nodes[at].expect("dense nodes"))
                .map_err(|_| crate::sql::eval::arena_full())?;
            path.searches_unmodeled = paths.bitmap_searches_unmodeled(chosen.path);
            // A bitmap of one index scan prints its conditions as the heap
            // scan's child.
            if let [node] = path.bitmap
                && let BitmapPlanKind::Index {
                    index_name,
                    conditions,
                    predicate,
                } = node.kind
            {
                path.conditions = conditions;
                path.predicate = predicate;
                path.index_name = index_name;
            }
            // `create_bitmap_scan_plan`: the restrictions the bitmap's
            // conditions repeat or imply leave the filter, and the
            // recheck keeps the rest of its conditions.
            let mut absorbed = [&Expr::Null; clauses::MAX_CONJUNCTS];
            let mut absorbed_count = 0;
            let mut filtered = [&Expr::Null; clauses::MAX_CONJUNCTS];
            let mut filtered_count = 0;
            for restriction in paths.restrictions {
                if implied(paths, restriction, &index_quals)? {
                    absorbed[absorbed_count] = restriction.source;
                    absorbed_count += 1;
                } else {
                    filtered[filtered_count] = restriction.source;
                    filtered_count += 1;
                }
            }
            let mut recheck = [&Expr::Null; 64];
            let mut recheck_count = 0;
            for &qual in quals {
                if !filtered[..filtered_count]
                    .iter()
                    .any(|filter| core::ptr::eq(*filter, qual))
                    && recheck_count < recheck.len()
                {
                    recheck[recheck_count] = qual;
                    recheck_count += 1;
                }
            }
            path.absorbed = arena
                .alloc_slice_copy(&absorbed[..absorbed_count])
                .map_err(|_| crate::sql::eval::arena_full())?;
            path.recheck = arena
                .alloc_slice_copy(&recheck[..recheck_count])
                .map_err(|_| crate::sql::eval::arena_full())?;
        }
    }
    Ok(Ok(path))
}

/// The written conditions of a partial index's predicate.
fn predicate_sources<'a>(
    arena: &'a Arena,
    predicate: &[Restriction<'a>],
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    Ok(&*arena
        .alloc_slice_with(predicate.len(), |at| predicate[at].source)
        .map_err(|_| crate::sql::eval::arena_full())?)
}

/// Whether the proof reads `restriction` as implied by `props`.
fn implied<'a>(
    paths: &Paths<'_, '_, 'a>,
    restriction: &Restriction<'a>,
    props: &Prop<'a>,
) -> Result<bool, SqlError> {
    let planner = paths.planner;
    let arena = paths.arena;
    let target = Prop::of(restriction, arena)?;
    let compare = |column: usize, left: &Datum<'_>, right: &Datum<'_>| {
        planner.compare(planner.column_collation(column), left, right)
    };
    predicate::implied_by(&target, props, &compare, arena)
}

/// The condition an index clause applies, as the proof reads it.
fn index_prop<'a>(clause: &IndexClause<'a>, arena: &'a Arena) -> Result<Prop<'a>, SqlError> {
    Ok(match clause.or_array {
        Some(array) => Prop::Array {
            column: match clause.restriction.clause {
                Clause::Or(arms) => arms[0][0].clause.column().unwrap_or(usize::MAX),
                _ => usize::MAX,
            },
            comparison: array.comparison,
            any: true,
            elements: array.elements,
            source: clause.restriction.source,
        },
        None => Prop::of(clause.restriction, arena)?,
    })
}

/// Each index clause as the index applies and EXPLAIN prints it.
fn conditions<'a>(
    arena: &'a Arena,
    clauses: &[IndexClause<'a>],
) -> Built<&'a [IndexCondition<'a>]> {
    let conditions = arena
        .alloc_slice_with(clauses.len(), |_| IndexCondition {
            source: &Expr::Null,
            printed: &Expr::Null,
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    for (condition, clause) in conditions.iter_mut().zip(clauses) {
        let source = clause.restriction.source;
        let printed: &'a Expr<'a> = match (clause.or_array, clause.restriction.clause) {
            (Some(array), _) => {
                let expression = if array.comparison == Comparison::Eq {
                    Expr::InList {
                        operand: array.key,
                        list: array.operands,
                        negated: false,
                    }
                } else {
                    Expr::AnyAll {
                        operand: array.key,
                        operator: array.comparison.binary(),
                        array: arena
                            .alloc(Expr::Array(array.operands))
                            .map_err(|_| crate::sql::eval::arena_full())?,
                        all: false,
                    }
                };
                arena
                    .alloc(expression)
                    .map_err(|_| crate::sql::eval::arena_full())?
            }
            (
                None,
                Clause::Compare {
                    comparison,
                    key,
                    operand,
                    pattern,
                    ..
                },
            ) => {
                let source_position = match source {
                    Expr::Binary {
                        source_position, ..
                    } => *source_position,
                    _ => SourcePosition::UNKNOWN,
                };
                arena
                    .alloc(Expr::Binary {
                        source_position,
                        operator: if pattern {
                            match comparison {
                                Comparison::Lt => BinaryOp::PatternLt,
                                Comparison::Le => BinaryOp::PatternLtEq,
                                Comparison::Gt => BinaryOp::PatternGt,
                                _ => BinaryOp::PatternGtEq,
                            }
                        } else {
                            comparison.binary()
                        },
                        left: key,
                        right: operand,
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?
            }
            _ => source,
        };
        *condition = IndexCondition { source, printed };
    }
    Ok(Ok(&*conditions))
}

/// `create_bitmap_subplan` of the bitmap node `node`: its plan nodes in
/// order, its original conditions (`qual`) and its index conditions
/// (`indexqual`) as the proof reads them.
fn bitmap_plan<'a>(
    paths: &Paths<'_, '_, 'a>,
    node: usize,
    depth: u8,
    out: &mut [Option<BitmapPlanNode<'a>>; 64],
    count: &mut usize,
) -> Built<(&'a [&'a Expr<'a>], Prop<'a>)> {
    let arena = paths.arena;
    let tuples = paths.planner.size.tuples;
    let bitmap = paths.nodes.items[node];
    let mut push = |plan: BitmapPlanNode<'a>| -> Result<(), Shape> {
        if *count == out.len() {
            return Err("more than 64 bitmap plan nodes");
        }
        out[*count] = Some(plan);
        *count += 1;
        Ok(())
    };
    match bitmap.kind {
        BitmapKind::Index(path) => {
            let index_path = paths.index_paths.items[path];
            let info = paths.planner.indexes[index_path.index];
            let conditions = outside!(conditions(arena, index_path.clauses)?);
            let predicate = paths.predicate(index_path.index);
            outside!(push(BitmapPlanNode {
                kind: BitmapPlanKind::Index {
                    index_name: info.catalog.name,
                    conditions,
                    predicate: predicate_sources(arena, predicate)?,
                },
                depth,
                total_cost: index_path.index_total,
                rows: clamp_row_est(index_path.selectivity * tuples),
            }));
            // The predicate parts the conditions do not imply are checked
            // with them.
            let clause_count = index_path.clauses.len();
            let quals = arena
                .alloc_slice_with(clause_count + predicate.len(), |at| {
                    if at < clause_count {
                        // A lossy condition rechecks the restriction it
                        // comes from.
                        let restriction = index_path.clauses[at].restriction;
                        restriction.lossy_of.unwrap_or(restriction.source)
                    } else {
                        &Expr::Null
                    }
                })
                .map_err(|_| crate::sql::eval::arena_full())?;
            let props = arena
                .alloc_slice_with(clause_count + predicate.len(), |_| Prop::And(&[]))
                .map_err(|_| crate::sql::eval::arena_full())?;
            let mut clause_props = [Prop::And(&[]); MAX_INDEX_CLAUSES];
            for (at, clause) in index_path.clauses.iter().enumerate() {
                props[at] = index_prop(clause, arena)?;
                clause_props[at] = Prop::of(clause.restriction, arena)?;
            }
            let written = &*arena
                .alloc_slice_copy(&clause_props[..clause_count])
                .map_err(|_| crate::sql::eval::arena_full())?;
            let mut count = clause_count;
            for part in predicate {
                let prop = Prop::of(part, arena)?;
                if !paths.prove(&prop, &Prop::And(written))? {
                    quals[count] = part.source;
                    props[count] = prop;
                    count += 1;
                }
            }
            Ok(Ok((&quals[..count], Prop::And(&props[..count]))))
        }
        BitmapKind::And(children) | BitmapKind::Or(children) => {
            let and = matches!(bitmap.kind, BitmapKind::And(_));
            // A one-child OR is its child.
            let own = and || children.len() > 1;
            if own {
                outside!(push(BitmapPlanNode {
                    kind: if and {
                        BitmapPlanKind::And
                    } else {
                        BitmapPlanKind::Or
                    },
                    depth,
                    total_cost: bitmap.total,
                    rows: clamp_row_est(bitmap.selectivity * tuples),
                }));
            }
            let child_depth = if own { depth + 1 } else { depth };
            let mut quals = [&Expr::Null; 64];
            let mut qual_count = 0;
            let mut props = [Prop::And(&[]); MAX_CHILDREN];
            for (at, &child) in children.iter().enumerate() {
                let (child_quals, child_props) =
                    outside!(bitmap_plan(paths, child, child_depth, out, count)?);
                props[at] = child_props;
                if and {
                    for &qual in child_quals {
                        if !quals[..qual_count].iter().any(|held| **held == *qual) {
                            if qual_count == quals.len() {
                                return Ok(Err("more than 64 bitmap conditions"));
                            }
                            quals[qual_count] = qual;
                            qual_count += 1;
                        }
                    }
                } else {
                    // Each child's conditions AND-ed, the children OR-ed.
                    let mut anded: Option<&'a Expr<'a>> = None;
                    for &qual in child_quals {
                        anded = Some(match anded {
                            None => qual,
                            Some(left) => &*arena
                                .alloc(Expr::Binary {
                                    source_position: SourcePosition::UNKNOWN.expanded(),
                                    operator: BinaryOp::And,
                                    left,
                                    right: qual,
                                })
                                .map_err(|_| crate::sql::eval::arena_full())?,
                        });
                    }
                    if let Some(anded) = anded {
                        quals[qual_count] = anded;
                        qual_count += 1;
                    }
                }
            }
            let props = &*arena
                .alloc_slice_copy(&props[..children.len()])
                .map_err(|_| crate::sql::eval::arena_full())?;
            if !and && children.len() > 1 {
                let mut ored: Option<&'a Expr<'a>> = None;
                for &qual in &quals[..qual_count] {
                    ored = Some(match ored {
                        None => qual,
                        Some(left) => &*arena
                            .alloc(Expr::Binary {
                                source_position: SourcePosition::UNKNOWN,
                                operator: BinaryOp::Or,
                                left,
                                right: qual,
                            })
                            .map_err(|_| crate::sql::eval::arena_full())?,
                    });
                }
                let quals = arena
                    .alloc_slice_copy(&[ored.expect("an OR has children")])
                    .map_err(|_| crate::sql::eval::arena_full())?;
                return Ok(Ok((&*quals, Prop::Or(props))));
            }
            let quals = arena
                .alloc_slice_copy(&quals[..qual_count])
                .map_err(|_| crate::sql::eval::arena_full())?;
            Ok(Ok((&*quals, if and { Prop::And(props) } else { props[0] })))
        }
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    fn path(kind: ScanKind, startup: f64, total: f64, presorted: usize) -> Candidate {
        Candidate {
            kind,
            path: 0,
            startup,
            total,
            disabled: 0,
            presorted,
            sort: SortKind::None,
        }
    }

    #[test]
    fn add_path_keeps_the_first_of_fuzzily_equal_paths_and_ordered_ones() {
        let mut kept = [None; MAX_CANDIDATES];
        // Within one percent the earlier path stays.
        let count = add_paths(
            &[
                path(ScanKind::Seq, 0.0, 100.0, 0),
                path(ScanKind::Bitmap, 0.0, 100.0, 0),
            ],
            false,
            &mut kept,
        );
        assert_eq!(count, 1);
        assert_eq!(cheapest_total(&kept[..count]).unwrap().kind, ScanKind::Seq);
        // Fuzzily equal but strictly cheaper replaces the earlier path.
        let count = add_paths(
            &[
                path(ScanKind::Seq, 0.0, 100.0, 0),
                path(ScanKind::Bitmap, 0.0, 99.5, 0),
            ],
            false,
            &mut kept,
        );
        assert_eq!(count, 1);
        assert_eq!(
            cheapest_total(&kept[..count]).unwrap().kind,
            ScanKind::Bitmap
        );
        // A dearer path that returns more of the requested order survives.
        let count = add_paths(
            &[
                path(ScanKind::Seq, 0.0, 100.0, 0),
                path(ScanKind::Index, 0.3, 150.0, 1),
                path(ScanKind::IndexOnly, 0.3, 160.0, 2),
            ],
            false,
            &mut kept,
        );
        assert_eq!(count, 3);
        // Disabled paths lose to enabled ones whatever their cost.
        let mut disabled = path(ScanKind::Seq, 0.0, 1.0, 0);
        disabled.disabled = 1;
        let count = add_paths(
            &[disabled, path(ScanKind::Index, 0.0, 50.0, 0)],
            false,
            &mut kept,
        );
        assert_eq!(
            cheapest_total(&kept[..count]).unwrap().kind,
            ScanKind::Index
        );
    }

    #[test]
    fn tuplesort_bounds_a_limited_sort_and_spills_past_work_mem() {
        let costs = Costs {
            seq_page: 1.0,
            random_page: 4.0,
            cpu_tuple: 0.01,
            cpu_index_tuple: 0.005,
            cpu_operator: 0.0025,
            effective_cache_size: 524_288.0,
        };
        let (quick, _) = tuplesort_cost(1000.0, 8.0, None, costs);
        let (bounded, _) = tuplesort_cost(1000.0, 8.0, Some(10.0), costs);
        assert!(bounded < quick);
        assert_eq!(merge_order(4.0 * 1024.0 * 1024.0), 15.0);
        assert_eq!(merge_order(64.0 * 1024.0), 6.0);
    }
}
