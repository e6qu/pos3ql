//! The queries whose joins PostgreSQL's join search ([`super::joins`])
//! plans in pos3ql: a `SELECT` of plain columns from two to 128 plain,
//! analyzed local tables joined by inner, left, right and full joins on
//! equalities of their columns and other conditions. Their join tree is
//! prepared as PostgreSQL prepares it ([`super::join_tree`]); EXPLAIN prints
//! the plan the search chooses and execution runs it
//! ([`crate::sql::query::join_exec`]); other joins keep pos3ql's own join
//! order.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    BinaryOp, Expr, FromClause, JoinKind, Select, SelectItem, TableRef, UnaryOp,
};
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::{ColumnSet, Storage, TableStatistics};

use super::join_tree::{self, Placed, Qual, Shape, Tree, TreeJoin, TreeNode, VarList};
use super::joins::{
    BASE_RELIDS, BasePath, BaseRel, Clause, ClauseForm, ClauseId, ClauseList, EquivalenceClass,
    ForeignKeyJoin, JoinPlan, JoinProblem, JoinSettings, JoinStatistics, KEY_CLASS, Keys,
    MAX_CLAUSES, MAX_JOIN_EXPRESSIONS, MAX_JOIN_RELS, Member, NO_KEY_CLASS, NeededColumn, Relids,
};
use super::paths::JoinCandidate;
use super::{
    Comparison, Costs, JoinScan, MAX_JOIN_PARAMS, NO_CLASS, Planned, Planner, QueryShape, ScanPath,
    clamp_probability,
};
use crate::sql::query::scope::{QueryScope, ResolvedColumn};

/// One base relation of a modeled join.
#[derive(Clone, Copy)]
pub(crate) struct ModeledRel<'a> {
    /// The FROM item, read alone.
    pub(crate) from: &'a FromClause<'a>,
    /// Its own conditions, which its scan filters by.
    pub(crate) restriction: Option<&'a Expr<'a>>,
    /// Its column equated with each other relation's column, as
    /// [`JoinScan::params`] lists them.
    pub(crate) joins: &'a [&'a Expr<'a>],
    pub(crate) scan: JoinScan,
    /// The columns its scan reads, and those above it.
    pub(crate) read: u64,
    pub(crate) output: u64,
    /// Its paths, by the search's [`super::joins::PathKind::Scan`] number.
    pub(crate) paths: &'a [JoinCandidate],
}

/// The scan of one path of a modeled join's base relation: the path, the
/// condition it runs under (its restrictions and the join clauses it moves
/// in) and the shape that plans it again when it runs.
#[derive(Clone, Copy)]
pub(crate) struct RelScan<'a> {
    pub(crate) path: ScanPath<'a>,
    pub(crate) condition: Option<&'a Expr<'a>>,
    pub(crate) shape: QueryShape,
    pub(crate) read: u64,
    /// The relations whose values the condition reads, and the outer
    /// joins that null them.
    pub(crate) param: Relids,
}

impl<'a> ModeledRel<'a> {
    /// Path `path` of the relation as its scan runs it: a parameterized
    /// path's join clauses among those of its parameterization only, so
    /// they read rows already bound.
    pub(crate) fn scan(
        &self,
        storage: &Storage,
        txid: u32,
        path: u16,
        params: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<RelScan<'a>, SqlError> {
        let candidate = self.paths[usize::from(path)];
        let mut scan = JoinScan {
            param_count: 0,
            clause_params: 0,
            describe: Some(candidate.describe),
            ..self.scan
        };
        let mut joins = [&Expr::Null; MAX_JOIN_PARAMS];
        for (at, &(class, rel)) in self.scan.params[..usize::from(self.scan.param_count)]
            .iter()
            .enumerate()
        {
            if (Relids::of(rel) | self.scan.param_others[at]) & !candidate.param == Relids::EMPTY {
                if self.scan.clause_params & 1 << at != 0 {
                    scan.clause_params |= 1 << scan.param_count;
                }
                scan.param_others[usize::from(scan.param_count)] = self.scan.param_others[at];
                scan.params[usize::from(scan.param_count)] = (class, rel);
                joins[usize::from(scan.param_count)] = self.joins[at];
                scan.param_count += 1;
            }
        }
        let condition = joined_condition(
            self.restriction,
            &joins[..usize::from(scan.param_count)],
            arena,
        )?;
        let single = QueryScope::resolve_schema(storage, self.from, txid, arena, None)?;
        let shape = QueryShape {
            output: Some(ColumnSet::from_low_word(self.output)),
            member: super::Member::Join(scan),
            ..QueryShape::default()
        };
        let planned = super::scan_path(
            storage,
            &single,
            txid,
            condition,
            Some(ColumnSet::from_low_word(self.read)),
            params,
            arena,
            shape,
        )?;
        let Planned::Chosen(path) = planned else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "the join's scan of a relation is no longer planned"
            ));
        };
        Ok(RelScan {
            path,
            condition,
            shape,
            read: self.read,
            param: candidate.param,
        })
    }
}

/// A member as an expression: a plain column reference
/// `relation.column`, or the expression an expression member is.
fn column_expression<'a>(
    scope: &QueryScope<'a>,
    expressions: &Expressions<'a>,
    member: Member,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if let Some(id) = member.expression_id() {
        return Ok(expressions.get(id).expression);
    }
    let table = usize::from(member.rel);
    let definition = scope.defs.stored(table).expect("plain table");
    let name = arena
        .alloc_str(
            definition.columns()[usize::from(member.column)]
                .name
                .as_str(),
        )
        .map_err(|_| crate::sql::eval::arena_full())?;
    arena
        .alloc(Expr::Column {
            qualifier: Some(scope.names[table]),
            name,
        })
        .map(|expression| &*expression)
        .map_err(|_| crate::sql::eval::arena_full())
}

/// `flatten_join_alias_vars`: `expression` with each merged `USING` column
/// that is not one column of its type replaced by its join alias variable's
/// expression, a full join's `COALESCE` or a coercion.
pub(crate) fn flatten_merged<'a>(
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    struct Failed(SqlError);
    impl From<crate::mem::arena::ArenaFull> for Failed {
        fn from(_: crate::mem::arena::ArenaFull) -> Self {
            Self(crate::sql::eval::arena_full())
        }
    }
    crate::sql::ast::rewrite_expression(expression, arena, &mut |node: &'a Expr<'a>| -> Result<
        Option<&'a Expr<'a>>,
        Failed,
    > {
        let Expr::Column { qualifier, name } = node else {
            return Ok(None);
        };
        match scope.probe_column(*qualifier, name) {
            Some(ResolvedColumn::Merged(merged)) if scope.merged_plain_var(merged).is_none() => {
                scope
                    .star_expression(ResolvedColumn::Merged(merged), arena)
                    .map(Some)
                    .map_err(Failed)
            }
            _ => Ok(None),
        }
    })
    .map_err(|Failed(error)| error)
}

/// `flatten_join_alias_vars` of the query: `statement` reading each merged
/// `USING` column that is not one column of its type as its join alias
/// variable's expression, in its output, conditions, grouping and ordering.
pub(crate) fn flatten_merged_statement<'a>(
    scope: &QueryScope<'a>,
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    if (0..scope.n_merged).all(|merged| scope.merged_plain_var(merged).is_some()) {
        return Ok(statement);
    }
    let flatten = |expression| flatten_merged(scope, expression, arena);
    let full = |_| crate::sql::eval::arena_full();
    let items = arena.alloc_slice_copy(statement.items).map_err(full)?;
    for item in items.iter_mut() {
        if let SelectItem::Expr { expression, .. } = item {
            *expression = flatten(*expression)?;
        }
    }
    let group_by = arena.alloc_slice_copy(statement.group_by).map_err(full)?;
    for expression in group_by.iter_mut() {
        *expression = flatten(*expression)?;
    }
    let distinct_on = arena
        .alloc_slice_copy(statement.distinct_on)
        .map_err(full)?;
    for expression in distinct_on.iter_mut() {
        *expression = flatten(*expression)?;
    }
    let order_by = arena.alloc_slice_copy(statement.order_by).map_err(full)?;
    for order in order_by.iter_mut() {
        order.expression = flatten(order.expression)?;
    }
    let flattened = Select {
        items,
        group_by,
        distinct_on,
        order_by,
        where_clause: statement.where_clause.map(flatten).transpose()?,
        having: statement.having.map(flatten).transpose()?,
        ..*statement
    };
    Ok(&*arena.alloc(flattened).map_err(full)?)
}

/// `left = right`, each a column named by its relation.
fn equality<'a>(
    scope: &QueryScope<'a>,
    expressions: &Expressions<'a>,
    left: Member,
    right: Member,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let left = column_expression(scope, expressions, left, arena)?;
    let right = column_expression(scope, expressions, right, arena)?;
    arena
        .alloc(Expr::Binary {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operator: BinaryOp::Eq,
            left,
            right,
        })
        .map(|expression| &*expression)
        .map_err(|_| crate::sql::eval::arena_full())
}

/// `member.column = value`, the column named by its relation.
fn equals_constant<'a>(
    scope: &QueryScope<'a>,
    expressions: &Expressions<'a>,
    member: Member,
    value: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let column = column_expression(scope, expressions, member, arena)?;
    arena
        .alloc(Expr::Binary {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operator: BinaryOp::Eq,
            left: column,
            right: value,
        })
        .map(|expression| &*expression)
        .map_err(|_| crate::sql::eval::arena_full())
}

/// `restriction AND join1 AND join2 ...`, left-deep so its conjuncts keep
/// that order.
fn joined_condition<'a>(
    restriction: Option<&'a Expr<'a>>,
    joins: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    let mut condition = restriction;
    for &join in joins {
        condition = Some(match condition {
            None => join,
            Some(held) => and(held, join, arena)?,
        });
    }
    Ok(condition)
}

/// The columns of the one relation of `scope` that `restriction` reads.
fn restriction_columns(scope: &QueryScope<'_>, restriction: Option<&Expr<'_>>) -> u64 {
    let mut mask = 0u64;
    if let Some(restriction) = restriction {
        restriction.for_each_column_reference(&mut |qualifier, name| {
            if let Ok(ResolvedColumn::Table(0, column)) = scope.find_column(qualifier, name)
                && column < 64
            {
                mask |= 1 << column;
            }
        });
    }
    mask
}

/// Pathkeys extended for presorted aggregates: the keys, each added key's
/// expression and flags, and how many were added.
type PresortKeys<'a> = (Keys, [(&'a Expr<'a>, u8); super::joins::MAX_KEYS], usize);

/// The plan of a modeled join and what its nodes read.
pub(crate) struct ModeledJoin<'a> {
    pub(crate) plan: JoinPlan<'a>,
    pub(crate) rels: &'a [ModeledRel<'a>],
    /// The equivalence classes, whose members are FROM positions and
    /// columns.
    pub(crate) classes: &'a [EquivalenceClass<'a>],
    /// The query's conditions, by [`Clause::source`].
    pub(crate) quals: &'a [&'a Expr<'a>],
    /// The width of the query's target list, its ordering columns
    /// included, which the top join's paths carry.
    pub(crate) output_width: f64,
    /// The `ORDER BY` items whose pathkeys the plan's sort sorts by.
    pub(crate) sort_items: &'a [u8],
    /// The width of the query's final target list: the windows' output,
    /// else [`Self::output_width`].
    pub(crate) final_width: f64,
    /// Per window clause in evaluation order, the positions in its
    /// `window_sort_keys` of the keys its pathkeys keep.
    pub(crate) windows: &'a [([u8; super::joins::MAX_KEYS], u8)],
    /// The query's `GROUP BY` above the join.
    pub(crate) group: Option<ModeledGroup<'a>>,
    /// The column each sort key shows: its class's first member the
    /// target list holds; `None` for an expression's key.
    pub(crate) sort_shown: &'a [Option<(usize, usize)>],
    /// The classes' expression members, by [`Member::expression_id`].
    pub(crate) expressions: &'a [JoinExpression<'a>],
    /// The base relations of the written FROM's parenthesized joins, which
    /// the plan's relation numbers name; `None` when the plan's relations
    /// are the written FROM's entries.
    pub(crate) nested: Option<NestedMembers<'a>>,
    /// Two scans of one table joined on columns a unique index covers,
    /// which PostgreSQL 18's `remove_useless_self_joins` may merge into
    /// one scan; the search does not model that removal.
    pub(crate) self_join: bool,
}

/// The base relations of a FROM clause holding parenthesized joins
/// (`a LEFT JOIN (b JOIN c ON x) ON y`), each its own relation of the join
/// search.
pub(crate) struct NestedMembers<'a> {
    /// The members, resolved in written order.
    pub(crate) scope: QueryScope<'a>,
    /// Each member's written FROM entry.
    pub(crate) entries: [u8; MAX_JOIN_RELS],
    /// The written entries that are parenthesized joins, by bit: each
    /// one's row holds its members' columns in order, each member's again
    /// (`member.*`), then each member's `num_nulls` of its whole row.
    pub(crate) joined: Relids,
}

impl<'a> ModeledJoin<'a> {
    /// The scope the plan's relation numbers resolve in: the members of
    /// the written FROM's parenthesized joins, or `written` itself.
    pub(crate) fn scope<'s>(&'s self, written: &'s QueryScope<'a>) -> &'s QueryScope<'a> {
        self.nested.as_ref().map_or(written, |nested| &nested.scope)
    }
}

/// How the plan groups the joined rows.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ModeledGroup<'a> {
    /// A `HashAggregate`, whose groups come back in its hash table's
    /// order, rather than a `GroupAggregate`.
    pub(crate) hashed: bool,
    /// The query calls an aggregate: a sorted grouping without one is a
    /// `Group`.
    pub(crate) aggregated: bool,
    /// The grouping columns in its `Group Key` order, by their position
    /// in the `GROUP BY` (a `DISTINCT`'s in the select list), each with the
    /// direction its sort orders it.
    pub(crate) keys: &'a [(usize, u8)],
    /// What sizes a grouping `HashAggregate`'s memory, when the model
    /// covers it.
    pub(crate) hash_agg: Option<crate::sql::query::postgres_memory::HashAggSizing>,
    /// [`Self::hash_agg`] without the transition states the model does not
    /// size: a lower bound on the table's memory, which orders the groups
    /// when even it does not spill.
    pub(crate) order_sizing: Option<crate::sql::query::postgres_memory::HashAggSizing>,
    /// A `DISTINCT`'s `Unique` or `HashAggregate`, whose keys are select
    /// list positions.
    pub(crate) distinct: bool,
    /// The ordered aggregates' keys a `GroupAggregate`'s or `Aggregate`'s
    /// sort adds after the grouping keys, with their flags.
    pub(crate) presorted: &'a [(&'a Expr<'a>, u8)],
}

impl<'a> ModeledJoin<'a> {
    /// The FROM position and column of member `member` of class `class`.
    pub(crate) fn member(&self, class: u8, member: u8) -> (usize, usize) {
        let member = self.classes[usize::from(class)].members()[usize::from(member)];
        (usize::from(member.rel), usize::from(member.column))
    }

    /// The `(left, right)` columns a mergeable join clause equates, as
    /// written; `None` for another condition.
    pub(crate) fn clause_columns(
        &self,
        clause: &Clause,
    ) -> Option<((usize, usize), (usize, usize))> {
        match clause.form {
            ClauseForm::Equality { class, left, right } => {
                Some((self.member(class, left), self.member(class, right)))
            }
            ClauseForm::Join { left, right, .. } => Some((
                (usize::from(left.rel), usize::from(left.column)),
                (usize::from(right.rel), usize::from(right.column)),
            )),
            ClauseForm::Other => None,
        }
    }

    /// The condition a clause evaluates, when it is not a modeled equality.
    pub(crate) fn condition(&self, clause: &Clause) -> Option<&'a Expr<'a>> {
        match clause.form {
            ClauseForm::Other => clause.source.map(|qual| self.quals[usize::from(qual)]),
            _ => None,
        }
    }
}

/// `extract_or_clause`: the `OR` of each arm's conditions on `table` alone
/// (an arm's `AND` of them, a nested `OR`'s own extraction), or `None` when
/// an arm has none.
fn extract_or_clause<'a>(
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    table: usize,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    let mut arms = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
    let mut arm_count = 0;
    if !or_arms(expression, &mut arms, &mut arm_count) {
        return Ok(None);
    }
    let mut result: Option<&'a Expr<'a>> = None;
    for &arm in &arms[..arm_count] {
        let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
        let mut count = 0;
        if !super::super::plan::flatten_and(arm, &mut conjuncts, &mut count) {
            return Ok(None);
        }
        let mut subclauses: Option<&'a Expr<'a>> = None;
        for &conjunct in &conjuncts[..count] {
            let sub = if matches!(
                conjunct,
                Expr::Binary {
                    operator: BinaryOp::Or,
                    ..
                }
            ) {
                extract_or_clause(scope, conjunct, table, arena)?
            } else {
                // `is_safe_restriction_clause_for`: it reads `table` alone
                // and calls nothing volatile.
                let vars = qual_vars(scope, conjunct);
                vars.filter(|vars| {
                    !vars.as_slice().is_empty()
                        && vars
                            .as_slice()
                            .iter()
                            .all(|&(rel, _)| usize::from(rel) == table)
                        && conjunct.contains_volatile_function().is_none()
                })
                .map(|_| conjunct)
            };
            if let Some(sub) = sub {
                subclauses = Some(match subclauses {
                    None => sub,
                    Some(held) => and(held, sub, arena)?,
                });
            }
        }
        let Some(sub) = subclauses else {
            return Ok(None);
        };
        // A nested `OR`'s arms join the list.
        let mut parts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
        let mut part_count = 0;
        if !or_arms(sub, &mut parts, &mut part_count) {
            return Ok(None);
        }
        for &part in &parts[..part_count] {
            result = Some(match result {
                None => part,
                Some(held) => arena
                    .alloc(Expr::Binary {
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                        operator: BinaryOp::Or,
                        left: held,
                        right: part,
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?,
            });
        }
    }
    Ok(result)
}

/// The arms of a (nested) `OR`, in order; false past the list's capacity.
fn or_arms<'a>(expression: &'a Expr<'a>, arms: &mut [&'a Expr<'a>], count: &mut usize) -> bool {
    match expression {
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
            ..
        } => or_arms(left, arms, count) && or_arms(right, arms, count),
        _ => {
            let Some(slot) = arms.get_mut(*count) else {
                return false;
            };
            *slot = expression;
            *count += 1;
            true
        }
    }
}

/// Whether each arm of the `OR` `expression`, and each condition of an arm
/// that is an `AND`, reads a single relation.
fn or_reads_one_relation_each(scope: &QueryScope<'_>, expression: &Expr<'_>) -> bool {
    let mut arms = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if !or_arms(expression, &mut arms, &mut count) {
        return false;
    }
    arms[..count].iter().all(|arm| {
        let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
        let mut conjunct_count = 0;
        super::super::plan::flatten_and(arm, &mut conjuncts, &mut conjunct_count)
            && conjuncts[..conjunct_count].iter().all(|conjunct| {
                qual_vars(scope, conjunct).is_some_and(|vars| {
                    vars.as_slice()
                        .iter()
                        .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel))
                        .count_ones()
                        == 1
                })
            })
    })
}

/// An `ORDER BY` item: a column, or another expression.
#[derive(Clone, Copy, PartialEq)]
enum SortTarget<'a> {
    Column(usize, usize),
    Expression(&'a Expr<'a>),
}

/// The column types whose equality the model hashes and merges: those
/// whose default hash operator class [`crate::sql::query::join_exec::datum_hash`]
/// computes.
fn joinable(ctype: ColType) -> bool {
    crate::sql::query::join_exec::datum_type_hashable(ctype)
}

/// The conditions one query holds.
const MAX_QUALS: usize = join_tree::MAX_QUALS;

/// The constants equated with columns, each once by value and type
/// (`em_is_const` members compare by `equal`).
struct Constants<'a> {
    held: [(&'a Expr<'a>, ColType); MAX_QUALS + 1],
    count: usize,
}

impl<'a> Constants<'a> {
    fn id(&mut self, value: &'a Expr<'a>, ctype: ColType) -> Option<u16> {
        if let Some(at) = self.held[..self.count]
            .iter()
            .position(|&(held, held_type)| *held == *value && held_type == ctype)
        {
            return Some(at as u16);
        }
        let at = self.count;
        *self.held.get_mut(at)? = (value, ctype);
        self.count += 1;
        Some(at as u16)
    }
}

/// `find_nonnullable_vars` of `expression` (`top_level` within an `AND`
/// list), or `None` for an expression whose strictness the model does not
/// read.
fn nonnullable(
    scope: &QueryScope<'_>,
    strict_call: &dyn Fn(&Expr<'_>) -> Option<bool>,
    expression: &Expr<'_>,
    top_level: bool,
) -> Option<VarList> {
    let union = |left: &Expr<'_>, right: &Expr<'_>, top: bool| -> Option<VarList> {
        let mut all = nonnullable(scope, strict_call, left, top)?;
        for &var in nonnullable(scope, strict_call, right, top)?.as_slice() {
            all.add(var)?;
        }
        Some(all)
    };
    match expression {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_) => Some(VarList::default()),
        Expr::Column { .. } => {
            let (table, column) = column_of(scope, expression)?;
            let mut vars = VarList::default();
            vars.add((table as u8, column as u16))?;
            Some(vars)
        }
        Expr::Binary {
            operator: BinaryOp::And,
            left,
            right,
            ..
        } if top_level => union(left, right, true),
        Expr::Binary {
            operator: BinaryOp::And | BinaryOp::Or,
            left,
            right,
            ..
        } => {
            // The columns every arm is strict for.
            let (one, two) = (
                nonnullable(scope, strict_call, left, top_level)?,
                nonnullable(scope, strict_call, right, top_level)?,
            );
            let mut both = VarList::default();
            for &var in one.as_slice() {
                if two.as_slice().contains(&var) {
                    both.add(var)?;
                }
            }
            Some(both)
        }
        Expr::Binary {
            operator:
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
                | BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod,
            left,
            right,
            ..
        } => union(left, right, false),
        Expr::Unary {
            operator: UnaryOp::Not | UnaryOp::Neg,
            operand,
            ..
        } => nonnullable(scope, strict_call, operand, false),
        Expr::IsNull { operand, negated } => {
            if *negated && top_level {
                nonnullable(scope, strict_call, operand, false)
            } else {
                Some(VarList::default())
            }
        }
        // A cast is strict (`RelabelType`, `CoerceViaIO` or a strict
        // cast function), as is a collation.
        Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => {
            nonnullable(scope, strict_call, operand, false)
        }
        // `COALESCE`, `GREATEST`, `LEAST` and `NULLIF` are expressions of
        // their own, never strict (`find_nonnullable_vars`).
        Expr::Call {
            name,
            over: None,
            filter: None,
            ..
        } if ["coalesce", "greatest", "least", "nullif"]
            .iter()
            .any(|form| name.eq_ignore_ascii_case(form)) =>
        {
            Some(VarList::default())
        }
        // `func_strict`: a strict function is null when an argument is.
        Expr::Call {
            args,
            over: None,
            filter: None,
            distinct: false,
            star: false,
            order_by: [],
            ..
        } => match strict_call(expression)? {
            true => {
                let mut all = VarList::default();
                for argument in *args {
                    for &var in nonnullable(scope, strict_call, argument, false)?.as_slice() {
                        all.add(var)?;
                    }
                }
                Some(all)
            }
            false => Some(VarList::default()),
        },
        Expr::Like {
            operand,
            pattern,
            escape: None,
            ..
        }
        | Expr::Match {
            operand, pattern, ..
        } => union(operand, pattern, false),
        _ => None,
    }
}

/// The columns `expression` reads, an aggregate's `ORDER BY` and `FILTER`
/// included, or `None` when it reads something other than the query's base
/// columns.
fn qual_vars(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<VarList> {
    let mut vars = VarList::default();
    let mut ok = true;
    let mut add = |qualifier: Option<&str>, name: &str| {
        let var = match scope.probe_column(qualifier, name) {
            Some(ResolvedColumn::Table(table, column)) => Some((table, column)),
            Some(ResolvedColumn::Merged(merged)) => scope.merged_plain_var(merged),
            _ => None,
        };
        match var {
            Some((table, column)) if column < 64 => {
                ok &= vars.add((table as u8, column as u16)).is_some();
            }
            _ => ok = false,
        }
    };
    expression.for_each_column_reference(&mut add);
    let mut whole_row = false;
    let _ = super::super::walk_expression_tree(expression, &mut |node| {
        whole_row |= matches!(node, Expr::WholeRow(_));
        if let Expr::Call {
            order_by, filter, ..
        } = node
        {
            for order in order_by.iter() {
                order.expression.for_each_column_reference(&mut add);
            }
            if let Some(filter) = filter {
                filter.for_each_column_reference(&mut add);
            }
        }
    });
    (ok && !whole_row && !expression.contains_subquery()).then_some(vars)
}

/// The constant side of `column = constant`, as the model equates them.
fn plain_constant<'a>(expression: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
    (expression.is_constant(false)
        && !expression.calls_routine()
        && expression.contains_volatile_function().is_none()
        && !matches!(expression, Expr::Null))
    .then_some(expression)
}

/// An expression of one relation's columns that an equivalence class
/// holds (an `EquivalenceMember` whose `em_expr` is no `Var`): `a.k + 1` of
/// `a.k + 1 = b.k`, which the relation's scan computes for each row.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JoinExpression<'a> {
    /// The first relation it reads, which computes it alone.
    pub(crate) rel: u8,
    /// The other relations it reads: their join computes it.
    pub(crate) reads: Relids,
    /// As written, its columns read in the query's scope.
    pub(crate) expression: &'a Expr<'a>,
    /// With its columns named by the relation's own column names, the form
    /// equal expressions share (`equal()` of `em_expr`).
    canonical: &'a Expr<'a>,
    pub(crate) ctype: ColType,
    pub(crate) collation: crate::sql::ast::Collation,
    /// `get_expr_width`.
    pub(crate) width: f64,
    /// `cost_qual_eval`'s per-row cost.
    pub(crate) cost: f64,
    /// The columns it reads.
    pub(crate) vars: VarList,
}

impl JoinExpression<'_> {
    /// Whether relation `table`'s scan computes it: it reads that
    /// relation alone.
    pub(crate) fn of(&self, table: usize) -> bool {
        usize::from(self.rel) == table && self.reads == Relids::EMPTY
    }
}

/// The expression members of one query, by [`Member::expression_id`].
pub(crate) struct Expressions<'a> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    held: [Option<JoinExpression<'a>>; MAX_JOIN_EXPRESSIONS],
    count: usize,
    /// The operands of each expression that is a full join's merged
    /// column (`COALESCE(left, right)`) equated with a constant.
    parts: [Option<(join_tree::Var, join_tree::Var)>; MAX_JOIN_EXPRESSIONS],
}

impl<'a> Expressions<'a> {
    fn new(storage: &'a Storage, txid: u32, arena: &'a Arena) -> Self {
        Self {
            storage,
            txid,
            arena,
            held: [None; MAX_JOIN_EXPRESSIONS],
            count: 0,
            parts: [None; MAX_JOIN_EXPRESSIONS],
        }
    }

    /// Expression `id`.
    pub(crate) fn get(&self, id: usize) -> &JoinExpression<'a> {
        self.held[id].as_ref().expect("a registered expression")
    }

    /// The expressions, in the arena.
    fn alloc(&self) -> Result<&'a [JoinExpression<'a>], SqlError> {
        self.arena
            .alloc_slice_with(self.count, |at| self.held[at].expect("dense expressions"))
            .map(|held| &*held)
            .map_err(|_| crate::sql::eval::arena_full())
    }

    /// `func_strict` of the routine a call runs.
    fn strict_call(&self, scope: &QueryScope<'a>, expression: &Expr<'_>) -> Option<bool> {
        if let Expr::Call {
            bound_strict: Some(strict),
            ..
        } = expression
        {
            return Some(*strict);
        }
        let columns = crate::sql::query::CatalogScopeCols {
            scope,
            outer: None,
            storage: self.storage,
            txid: self.txid,
            arena: self.arena,
        };
        super::super::plan::routine_strict(expression, &columns, self.storage, self.txid)
    }

    /// The expression member `expression` already is, if any.
    fn find(&self, scope: &QueryScope<'a>, expression: &'a Expr<'a>) -> Option<Member> {
        if column_of(scope, expression).is_some() || self.count == 0 {
            return None;
        }
        let vars = qual_vars(scope, expression)?;
        let rel = vars.as_slice().first()?.0;
        if vars.as_slice().iter().any(|&(other, _)| other != rel) {
            return None;
        }
        let table = usize::from(rel);
        let definition = scope.defs.stored(table)?;
        let resolve = |qualifier: Option<&str>, name: &str| match scope.find_column(qualifier, name)
        {
            Ok(ResolvedColumn::Table(at, column)) if at == table => Some(column),
            _ => None,
        };
        let canonical =
            super::expressions::canonical(expression, &resolve, definition, self.arena).ok()??;
        self.held[..self.count]
            .iter()
            .flatten()
            .position(|held| {
                held.rel == rel && held.reads == Relids::EMPTY && *held.canonical == *canonical
            })
            .map(|id| Member::expression(rel, id as u16))
    }

    /// The class member `expression` is, with its type and collation: a
    /// column, or an expression of one or several relations' columns of a
    /// type the model hashes and merges; `None` for anything else.
    fn member(
        &mut self,
        scope: &QueryScope<'a>,
        expression: &'a Expr<'a>,
    ) -> Result<Option<(Member, ColType, crate::sql::ast::Collation)>, SqlError> {
        if let Some((table, column)) = column_of(scope, expression) {
            let meta = &scope.defs.stored(table).expect("plain table").columns()[column];
            return Ok(Some((
                Member::plain(table as u8, column as u16),
                meta.ctype,
                meta.collation,
            )));
        }
        let Some(vars) = qual_vars(scope, expression) else {
            return Ok(None);
        };
        let rels = vars
            .as_slice()
            .iter()
            .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
        if rels.is_empty()
            || expression.contains_volatile_function().is_some()
            || crate::sql::query::expr_has_window(expression)
            || crate::sql::query::expression_has_project_set(expression, self.storage, self.txid)
        {
            return Ok(None);
        }
        let table = rels.trailing_zeros() as usize;
        let columns = crate::sql::query::CatalogScopeCols {
            scope,
            outer: None,
            storage: self.storage,
            txid: self.txid,
            arena: self.arena,
        };
        if crate::sql::query::expr_has_aggregate(expression, self.storage, self.txid, &columns)? {
            return Ok(None);
        }
        let Ok((type_oid, type_mod)) = crate::sql::exec::infer_type_res(expression, &columns)
        else {
            return Ok(None);
        };
        let Some((ctype, None)) =
            crate::sql::exec::catalog_column_type(self.storage, self.txid, type_oid)
        else {
            return Ok(None);
        };
        if !joinable(ctype) {
            return Ok(None);
        }
        let collation = scope.expression_collation(self.storage, self.arena, expression)?;
        let reads = rels & !Relids::of(table);
        if !reads.is_empty() {
            // Equal expressions of several relations are equal as written.
            let found = self.held[..self.count].iter().flatten().position(|held| {
                held.rel == table as u8 && held.reads == reads && *held.expression == *expression
            });
            let id = match found {
                Some(id) => id,
                None => {
                    if self.count == MAX_JOIN_EXPRESSIONS {
                        return Ok(None);
                    }
                    self.held[self.count] = Some(JoinExpression {
                        rel: table as u8,
                        reads,
                        expression,
                        canonical: expression,
                        ctype,
                        collation,
                        width: super::type_average_width(ctype, i32::from(type_mod)),
                        cost: super::super::plan::qual_cost(
                            expression,
                            &columns,
                            self.storage,
                            self.txid,
                            self.arena,
                        ) / 2.0
                            * Costs::current().cpu_operator,
                        vars,
                    });
                    self.count += 1;
                    self.count - 1
                }
            };
            return Ok(Some((Member::several(rels, id as u16), ctype, collation)));
        }
        let definition = scope.defs.stored(table).expect("plain table");
        let resolve = |qualifier: Option<&str>, name: &str| match scope.find_column(qualifier, name)
        {
            Ok(ResolvedColumn::Table(at, column)) if at == table => Some(column),
            _ => None,
        };
        let Some(canonical) =
            super::expressions::canonical(expression, &resolve, definition, self.arena)?
        else {
            return Ok(None);
        };
        let found = self.held[..self.count].iter().flatten().position(|held| {
            usize::from(held.rel) == table
                && held.reads == Relids::EMPTY
                && *held.canonical == *canonical
        });
        let id = match found {
            Some(id) => id,
            None => {
                if self.count == MAX_JOIN_EXPRESSIONS {
                    return Ok(None);
                }
                self.held[self.count] = Some(JoinExpression {
                    rel: table as u8,
                    reads: Relids::EMPTY,
                    expression,
                    canonical,
                    ctype,
                    collation,
                    width: super::type_average_width(ctype, i32::from(type_mod)),
                    cost: super::super::plan::qual_cost(
                        expression,
                        &columns,
                        self.storage,
                        self.txid,
                        self.arena,
                    ) / 2.0
                        * Costs::current().cpu_operator,
                    vars,
                });
                self.count += 1;
                self.count - 1
            }
        };
        Ok(Some((
            Member::expression(table as u8, id as u16),
            ctype,
            collation,
        )))
    }
}

/// The operands of `side` when it is `COALESCE(one, two)` whose operands
/// are, in order, the left and right operands of a full join's clause
/// (`full_clauses`), as a full join's merged `USING` column is: each a
/// column, an expression, or such a `COALESCE` again, whose operands
/// [`Expressions::parts`] records. `reconsider_full_join_clause` matches no
/// other `COALESCE`: one with other arguments stays an ordinary filter.
fn coalesce_operands<'a>(
    scope: &QueryScope<'a>,
    expressions: &mut Expressions<'a>,
    full_clauses: &[join_tree::Var],
    side: &'a Expr<'a>,
) -> Result<Option<(Member, Member, ColType)>, SqlError> {
    let Expr::Call {
        name,
        args: [one, two],
        over: None,
        filter: None,
        distinct: false,
        star: false,
        ..
    } = side
    else {
        return Ok(None);
    };
    if !name.eq_ignore_ascii_case("coalesce") {
        return Ok(None);
    }
    let (Some((left, left_type, left_collation)), Some((right, right_type, right_collation))) = (
        expressions.member(scope, one)?,
        expressions.member(scope, two)?,
    ) else {
        return Ok(None);
    };
    if left.base_relids() & right.base_relids() != Relids::EMPTY
        || left_type != right_type
        || left_collation != right_collation
        || !joinable(left_type)
        || !full_clauses
            .as_chunks::<2>()
            .0
            .contains(&[(left.rel, left.column), (right.rel, right.column)])
    {
        return Ok(None);
    }
    for (member, operand) in [(left, *one), (right, *two)] {
        if let Some(id) = member.expression_id() {
            let Some((inner_left, inner_right, _)) =
                coalesce_operands(scope, expressions, full_clauses, operand)?
            else {
                return Ok(None);
            };
            expressions.parts[id] = Some((
                (inner_left.rel, inner_left.column),
                (inner_right.rel, inner_right.column),
            ));
        }
    }
    Ok(Some((left, right, left_type)))
}

/// A condition as the preparation reads it, or `None` when it is outside
/// the model.
fn analyze_qual<'a>(
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    constants: &mut Constants<'a>,
    expressions: &mut Expressions<'a>,
    full_clauses: &[join_tree::Var],
) -> Result<Option<Qual>, SqlError> {
    // `COALESCE(left, right) = constant` of two relations' columns or such
    // `COALESCE`s, written or a full join's merged column.
    if let Expr::Binary {
        operator: BinaryOp::Eq,
        left,
        right,
        ..
    } = expression
    {
        let is_coalesce = |side: &Expr<'_>| matches!(side, Expr::Call { name, .. } if name.eq_ignore_ascii_case("coalesce"));
        let found = match (plain_constant(left), plain_constant(right)) {
            (None, Some(value)) if is_coalesce(left) => Some((*left, value)),
            (Some(value), None) if is_coalesce(right) => Some((*right, value)),
            _ => None,
        };
        if let Some((side, value)) = found
            && let Some((one, two, ctype)) =
                coalesce_operands(scope, expressions, full_clauses, side)?
        {
            // The `COALESCE` itself, a member of the constant's class
            // that `reconsider_full_join_clause` finds by its parts.
            let Some((whole, ..)) = expressions.member(scope, side)? else {
                return Ok(None);
            };
            let id = whole.expression_id().expect("an expression");
            expressions.parts[id] = Some(((one.rel, one.column), (two.rel, two.column)));
            let Some(constant) = constants.id(value, ctype) else {
                return Ok(None);
            };
            let Some(vars) = qual_vars(scope, side) else {
                return Ok(None);
            };
            return Ok(Some(Qual {
                rels: whole.base_relids(),
                vars,
                // `COALESCE` is not strict.
                nonnullable: Relids::EMPTY,
                strict_vars: VarList::default(),
                forced_null: None,
                shape: Shape::Constant {
                    var: (whole.rel, whole.column),
                    constant,
                },
            }));
        }
    }
    let Some(vars) = qual_vars(scope, expression) else {
        return Ok(None);
    };
    let rels: Relids = vars
        .as_slice()
        .iter()
        .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
    let volatile = expression.contains_volatile_function().is_some();
    let strict = nonnullable(
        scope,
        &|call| expressions.strict_call(scope, call),
        expression,
        true,
    );
    let forced_null = match expression {
        Expr::IsNull {
            operand,
            negated: false,
        } => column_of(scope, operand).map(|(table, column)| (table as u8, column as u16)),
        _ => None,
    };
    let var = |member: Member| (member.rel, member.column);
    let shape = match expression {
        Expr::Binary {
            operator: BinaryOp::Eq,
            left,
            right,
            ..
        } if !volatile => {
            // `process_equivalence`: each side a column, an expression of
            // one relation's columns, or a constant.
            let one = match plain_constant(left) {
                Some(_) => None,
                None => expressions.member(scope, left)?,
            };
            let two = match plain_constant(right) {
                Some(_) => None,
                None => expressions.member(scope, right)?,
            };
            match (one, two) {
                (
                    Some((one, left_type, left_collation)),
                    Some((two, right_type, right_collation)),
                ) if one.base_relids() & two.base_relids() == Relids::EMPTY => {
                    if comparable_types(left_type, right_type)
                        && joinable(left_type)
                        && left_collation == right_collation
                    {
                        Shape::Columns(var(one), var(two))
                    } else {
                        Shape::Other
                    }
                }
                // A constant equated with an expression of several
                // relations filters their join.
                (Some((member, ..)), None) | (None, Some((member, ..)))
                    if member.reads != Relids::EMPTY =>
                {
                    Shape::Other
                }
                (Some((member, ctype, _)), None) | (None, Some((member, ctype, _))) => {
                    let value = if one.is_some() { right } else { left };
                    match plain_constant(value) {
                        Some(value) if joinable(ctype) => {
                            let Some(constant) = constants.id(value, ctype) else {
                                return Ok(None);
                            };
                            Shape::Constant {
                                var: var(member),
                                constant,
                            }
                        }
                        _ => Shape::Other,
                    }
                }
                _ => Shape::Other,
            }
        }
        _ => Shape::Other,
    };
    // A condition above one relation must be one the model estimates.
    if rels.count_ones() > 1 && shape == Shape::Other && !join_condition(scope, expression) {
        return Ok(None);
    }
    let Some(strict) = strict else {
        return Ok(None);
    };
    Ok(Some(Qual {
        rels,
        vars,
        nonnullable: strict
            .as_slice()
            .iter()
            .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel)),
        strict_vars: strict,
        forced_null,
        shape,
    }))
}

/// A condition of several relations the model estimates: a comparison of
/// two relations' columns, or an `OR` whose arms are such comparisons or
/// conditions of one relation, or `AND`s of those
/// (`clauselist_selectivity_or`).
fn join_condition(scope: &QueryScope<'_>, expression: &Expr<'_>) -> bool {
    match expression {
        Expr::Binary {
            operator: BinaryOp::Or,
            left,
            right,
            ..
        } => [left, right].iter().all(|arm| {
            let Some(vars) = qual_vars(scope, arm) else {
                return false;
            };
            let rels = vars
                .as_slice()
                .iter()
                .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
            if rels.count_ones() <= 1 {
                return true;
            }
            // An `AND` arm: each condition one relation's or a join
            // condition.
            let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
            let mut count = 0;
            if matches!(
                arm,
                Expr::Binary {
                    operator: BinaryOp::And,
                    ..
                }
            ) {
                return super::super::plan::flatten_and(arm, &mut conjuncts, &mut count)
                    && conjuncts[..count].iter().all(|conjunct| {
                        qual_vars(scope, conjunct).is_some_and(|vars| {
                            vars.as_slice()
                                .iter()
                                .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel))
                                .count_ones()
                                <= 1
                        }) || join_condition(scope, conjunct)
                    });
            }
            join_condition(scope, arm)
        }),
        _ => {
            join_comparison(scope, expression)
                || join_inequality(scope, expression)
                || join_equality(scope, expression)
                || several_relations_constant(scope, expression).is_some()
        }
    }
}

/// `left = right` of columns of two relations of one joinable type, an arm
/// of an `OR` that `eqjoinsel` estimates.
fn join_equality(scope: &QueryScope<'_>, expression: &Expr<'_>) -> bool {
    let Expr::Binary {
        operator: BinaryOp::Eq,
        left,
        right,
        ..
    } = expression
    else {
        return false;
    };
    let (Some(one), Some(two)) = (column_of(scope, left), column_of(scope, right)) else {
        return false;
    };
    let ctype = |(table, column): (usize, usize)| {
        let meta = &scope.defs.stored(table).expect("plain table").columns()[column];
        (meta.ctype, meta.collation)
    };
    one.0 != two.0
        && comparable_types(ctype(one).0, ctype(two).0)
        && ctype(one).1 == ctype(two).1
        && joinable(ctype(one).0)
}

/// Whether two column types' equality is one operator family's, which
/// merges and hashes them alike and puts both in one equivalence class:
/// the same type, or two of `integer_ops` (`int48eq`, ...) or `float_ops`
/// (`float48eq`), which compare without a cast.
fn comparable_types(left: ColType, right: ColType) -> bool {
    let family = |ctype: ColType| match ctype {
        ColType::Int2 | ColType::Int4 | ColType::Int8 => Some(0),
        ColType::Float4 | ColType::Float8 => Some(1),
        _ => None,
    };
    left == right || family(left).is_some_and(|family_of| Some(family_of) == family(right))
}

/// `clause_selectivity` and `cost_qual_eval` of a condition of several
/// relations [`join_condition`] accepts: a comparison's `DEFAULT_INEQ_SEL`,
/// an inequality's complement of `eqjoinsel`, an `OR`'s arms combined as
/// `s1 + s2 - s1 * s2`, an arm of one relation estimated as its restriction.
/// `clauselist_selectivity` of an `AND` of several relations' conditions:
/// each relation's own conditions estimated together, times each join
/// condition's estimate; the cost is theirs summed.
fn and_estimate<'a>(
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    reader: &RelStatistics<'_, '_, '_>,
    restriction: &dyn Fn(usize, &'a Expr<'a>) -> Result<Option<(f64, f64)>, SqlError>,
    expression_cost: &dyn Fn(&'a Expr<'a>) -> f64,
    costs: Costs,
    arena: &'a Arena,
) -> Result<Option<(f64, f64)>, SqlError> {
    let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
    let mut count = 0;
    if !super::super::plan::flatten_and(expression, &mut conjuncts, &mut count) {
        return Ok(None);
    }
    let (mut selectivity, mut cost) = (1.0, 0.0);
    let mut done = [false; super::super::plan::MAX_CONJUNCTS];
    for at in 0..count {
        if done[at] {
            continue;
        }
        let Some(vars) = qual_vars(scope, conjuncts[at]) else {
            return Ok(None);
        };
        let rels = vars
            .as_slice()
            .iter()
            .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
        let estimate = if rels.count_ones() == 1 {
            // This relation's conditions, estimated as one list.
            let table = rels.trailing_zeros() as usize;
            let mut own: Option<&'a Expr<'a>> = None;
            for later in at..count {
                let Some(later_vars) = qual_vars(scope, conjuncts[later]) else {
                    return Ok(None);
                };
                if later_vars
                    .as_slice()
                    .iter()
                    .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel))
                    == rels
                {
                    done[later] = true;
                    own = Some(match own {
                        None => conjuncts[later],
                        Some(held) => and(held, conjuncts[later], arena)?,
                    });
                }
            }
            restriction(table, own.expect("this condition"))?
        } else if rels.is_empty() {
            None
        } else {
            join_estimate(
                scope,
                conjuncts[at],
                reader,
                restriction,
                expression_cost,
                costs,
                arena,
            )?
        };
        let Some((own_selectivity, own_cost)) = estimate else {
            return Ok(None);
        };
        selectivity *= own_selectivity;
        cost += own_cost;
    }
    Ok(Some((selectivity, cost)))
}

fn join_estimate<'a>(
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    reader: &RelStatistics<'_, '_, '_>,
    restriction: &dyn Fn(usize, &'a Expr<'a>) -> Result<Option<(f64, f64)>, SqlError>,
    expression_cost: &dyn Fn(&'a Expr<'a>) -> f64,
    costs: Costs,
    arena: &'a Arena,
) -> Result<Option<(f64, f64)>, SqlError> {
    if let Expr::Binary {
        operator: BinaryOp::Or,
        left,
        right,
        ..
    } = expression
    {
        let (mut selectivity, mut cost) = (0.0, 0.0);
        for arm in [*left, *right] {
            let Some(vars) = qual_vars(scope, arm) else {
                return Ok(None);
            };
            let rels = vars
                .as_slice()
                .iter()
                .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
            let estimate = if rels.count_ones() == 1 {
                restriction(rels.trailing_zeros() as usize, arm)?
            } else if rels.is_empty() {
                None
            } else if matches!(
                arm,
                Expr::Binary {
                    operator: BinaryOp::And,
                    ..
                }
            ) {
                and_estimate(
                    scope,
                    arm,
                    reader,
                    restriction,
                    expression_cost,
                    costs,
                    arena,
                )?
            } else {
                join_estimate(
                    scope,
                    arm,
                    reader,
                    restriction,
                    expression_cost,
                    costs,
                    arena,
                )?
            };
            let Some((arm_selectivity, arm_cost)) = estimate else {
                return Ok(None);
            };
            selectivity = selectivity + arm_selectivity - selectivity * arm_selectivity;
            cost += arm_cost;
        }
        return Ok(Some((selectivity, cost)));
    }
    if matches!(expression, Expr::Bool(true)) {
        return Ok(Some((1.0, 0.0)));
    }
    if join_comparison(scope, expression) {
        return Ok(Some((super::DEFAULT_INEQ_SEL, costs.cpu_operator)));
    }
    // An expression of several relations compared with a constant: the
    // join selectivity of operands without statistics, whose distinct count
    // is the default (`eqjoinsel` and `neqjoinsel` over
    // `DEFAULT_NUM_DISTINCT`, `scalarineqsel`'s default).
    if let Some(operator) = several_relations_constant(scope, expression) {
        let equal = 1.0 / super::DEFAULT_NUM_DISTINCT;
        let selectivity = match operator {
            BinaryOp::Eq => equal,
            BinaryOp::NotEq => 1.0 - equal,
            _ => super::DEFAULT_INEQ_SEL,
        };
        return Ok(Some((selectivity, expression_cost(expression))));
    }
    let equality = join_equality(scope, expression);
    if equality || join_inequality(scope, expression) {
        let Expr::Binary { left, right, .. } = expression else {
            unreachable!("a comparison");
        };
        let member = |side: &Expr<'_>| {
            column_of(scope, side).map(|(table, column)| Member::plain(table as u8, column as u16))
        };
        let (Some(one), Some(two)) = (member(left), member(right)) else {
            return Ok(None);
        };
        let equal = reader.equality(one, two)?.clamp(0.0, 1.0);
        let selectivity = if equality { equal } else { 1.0 - equal };
        return Ok(Some((selectivity, costs.cpu_operator)));
    }
    Ok(None)
}

/// The comparison operator of an expression of several relations' columns
/// with a plain constant (`(a.x + b.y) = 10`, `COALESCE(b.k, a.k) = 30`).
fn several_relations_constant(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<BinaryOp> {
    let Expr::Binary {
        operator:
            operator @ (BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq),
        left,
        right,
        ..
    } = expression
    else {
        return None;
    };
    let side = match (plain_constant(left), plain_constant(right)) {
        (None, Some(_)) => left,
        (Some(_), None) => right,
        _ => return None,
    };
    if side.contains_volatile_function().is_some() {
        return None;
    }
    let rels = qual_vars(scope, side)?
        .as_slice()
        .iter()
        .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
    (rels.count_ones() > 1).then_some(*operator)
}

/// `left <> right` of columns of two relations of one joinable type, which
/// `neqjoinsel` estimates as the equality's complement.
fn join_inequality(scope: &QueryScope<'_>, expression: &Expr<'_>) -> bool {
    let Expr::Binary {
        operator: BinaryOp::NotEq,
        left,
        right,
        ..
    } = expression
    else {
        return false;
    };
    let (Some(one), Some(two)) = (column_of(scope, left), column_of(scope, right)) else {
        return false;
    };
    let ctype = |(table, column): (usize, usize)| {
        let meta = &scope.defs.stored(table).expect("plain table").columns()[column];
        (meta.ctype, meta.collation)
    };
    one.0 != two.0
        && comparable_types(ctype(one).0, ctype(two).0)
        && ctype(one).1 == ctype(two).1
        && joinable(ctype(one).0)
}

/// `left < right` (or `<=`, `>`, `>=`) of columns of two relations, which
/// `scalarltjoinsel` estimates as `DEFAULT_INEQ_SEL`.
fn join_comparison(scope: &QueryScope<'_>, expression: &Expr<'_>) -> bool {
    let Expr::Binary {
        operator: BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq,
        left,
        right,
        ..
    } = expression
    else {
        return false;
    };
    let (Some(one), Some(two)) = (column_of(scope, left), column_of(scope, right)) else {
        return false;
    };
    let ctype = |(table, column): (usize, usize)| {
        let meta = &scope.defs.stored(table).expect("plain table").columns()[column];
        (meta.ctype, meta.collation)
    };
    one.0 != two.0
        && comparable_types(ctype(one).0, ctype(two).0)
        && ctype(one).1 == ctype(two).1
        && joinable(ctype(one).0)
}

/// The plan PostgreSQL's join search chooses for `statement`, when the
/// query is inside the model; `None` otherwise.
/// Builds the written join tree's nodes: each comma-separated item a
/// chain of joins, an unaliased parenthesized join its own subtree, base
/// relations numbered in written order, and each outer join's range table
/// entry after those of the joins inside it, as parse analysis numbers
/// them.
struct ChainBuilder<'t, 'a> {
    tree: &'t mut Tree,
    next: u8,
    rtindex: u8,
    /// Each join's node and condition, inner joins' first.
    sources: [(u8, &'a Expr<'a>); MAX_JOIN_RELS],
    source_count: usize,
}

impl<'a> ChainBuilder<'_, 'a> {
    fn source(&mut self, node: u8, on: &'a Expr<'a>) -> Option<()> {
        *self.sources.get_mut(self.source_count)? = (node, on);
        self.source_count += 1;
        Some(())
    }

    /// The node of FROM item `table`: a base relation, or a parenthesized
    /// join's subtree.
    fn item(&mut self, table: &'a TableRef<'a>) -> Option<u8> {
        match parenthesized_join(table) {
            Some(inner) => {
                let mut items = [0u8; MAX_JOIN_RELS];
                let mut count = 0;
                self.chain(&inner.base, inner.joins, |_| None, &mut items, &mut count)?;
                (count == 1).then_some(items[0])
            }
            None => {
                let rel = self.next;
                self.next = rel.checked_add(1)?;
                self.tree.node(TreeNode::Rel(rel))
            }
        }
    }

    /// The chain `base` then `joins`, its comma-separated items' nodes
    /// added to `items`; `implied` gives a written join's `USING` or
    /// `NATURAL` equalities.
    fn chain(
        &mut self,
        base: &'a TableRef<'a>,
        joins: &'a [crate::sql::ast::Join<'a>],
        implied: impl Fn(usize) -> Option<&'a Expr<'a>>,
        items: &mut [u8; MAX_JOIN_RELS],
        count: &mut usize,
    ) -> Option<()> {
        let mut current = self.item(base)?;
        for (index, join) in joins.iter().enumerate() {
            let right = self.item(&join.table)?;
            let (kind, numbered) = match join.kind {
                JoinKind::Comma => {
                    *items.get_mut(*count)? = current;
                    *count += 1;
                    current = right;
                    continue;
                }
                JoinKind::Left => (TreeJoin::Left, true),
                JoinKind::Right => (TreeJoin::Right, true),
                JoinKind::Full => (TreeJoin::Full, true),
                JoinKind::Inner | JoinKind::Cross => (TreeJoin::Inner, false),
                _ => return None,
            };
            let node = self.tree.node(TreeNode::Join {
                kind,
                left: current,
                right,
                rtindex: numbered.then_some(self.rtindex),
            })?;
            if numbered {
                self.rtindex += 1;
            }
            if let Some(on) = join.on.or_else(|| implied(index)) {
                self.source(node, on)?;
            }
            current = node;
        }
        *items.get_mut(*count)? = current;
        *count += 1;
        Some(())
    }
}

/// The join an unaliased parenthesized FROM item holds, when its members
/// join by written conditions only (`a LEFT JOIN (b JOIN c ON x) ON y`):
/// PostgreSQL's `deconstruct_jointree` reads it as a node of the join tree.
fn parenthesized_join<'a>(table: &'a TableRef<'a>) -> Option<&'a FromClause<'a>> {
    if table.alias.is_some()
        || table.col_alias.is_some()
        || table.lateral
        || table.join_members.is_none()
    {
        return None;
    }
    let select = table.subquery?;
    let from = select.from.as_ref()?;
    from.joins
        .iter()
        .all(|join| join.using.is_none() && !join.natural)
        .then_some(from)
}

pub(crate) fn modeled_join<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Option<ModeledJoin<'a>>, SqlError> {
    let Some(written) = statement.from.as_ref() else {
        return Ok(None);
    };
    let modeled = modeled_join_written(storage, txid, statement, written, scope, params, arena)?;
    // A `USING` or `NATURAL` join beside a parenthesized join, or one in a
    // parenthesized join nested in another, that the search does not read
    // would run in pos3ql's own join order.
    if modeled.is_none() && unread_parenthesized_using(storage, txid, written, arena)? {
        // Parse analysis reports its own errors first.
        let mark = arena.mark();
        let analyzed =
            QueryScope::resolve_schema_with_join_conditions(storage, written, txid, arena, None)
                .and_then(|resolved| {
                    crate::sql::query::validate_static_grouping(
                        statement, &resolved, None, storage, txid, arena,
                    )
                });
        // SAFETY: the analysis holds no arena data past its result.
        unsafe { arena.rewind_to(mark) };
        analyzed?;
        return Err(crate::sql_err!(
            crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
            "a parenthesized join's USING or NATURAL join in this query is not supported by the join search"
        ));
    }
    Ok(modeled)
}

/// Whether `written` joins a parenthesized join by `USING` or `NATURAL`,
/// or holds a parenthesized join with such a join inside another
/// parenthesized join, its base relations all plain tables the join
/// search plans.
fn unread_parenthesized_using<'a>(
    storage: &'a Storage,
    txid: u32,
    written: &'a FromClause<'a>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    fn items<'f, 'a>(from: &'f FromClause<'a>) -> impl Iterator<Item = &'f TableRef<'a>> + Clone {
        core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table))
    }
    fn inner<'a>(item: &TableRef<'a>) -> Option<&'a FromClause<'a>> {
        item.join_members?;
        item.subquery.and_then(|body| body.from.as_ref())
    }
    fn holds_using(from: &FromClause<'_>) -> bool {
        from.joins
            .iter()
            .any(|join| join.using.is_some() || join.natural)
            || items(from).filter_map(inner).any(holds_using)
    }
    let beside = items(written).any(|item| inner(item).is_some())
        && written
            .joins
            .iter()
            .any(|join| join.using.is_some() || join.natural);
    let nested = items(written)
        .filter_map(inner)
        .any(|body| items(body).filter_map(inner).any(holds_using));
    if !beside && !nested {
        return Ok(false);
    }
    fn leaves<'a>(
        table: &'a TableRef<'a>,
        out: &mut crate::mem::arena::ArenaList<'a, &'a TableRef<'a>>,
    ) -> Result<(), SqlError> {
        match inner(table) {
            Some(body) => {
                for item in items(body) {
                    leaves(item, out)?;
                }
                Ok(())
            }
            None => out.push(table).map_err(|_| crate::sql::eval::arena_full()),
        }
    }
    let mark = arena.mark();
    let mut out = crate::mem::arena::ArenaList::new(arena);
    for item in items(written) {
        leaves(item, &mut out)?;
    }
    let out = out.as_slice();
    let joins = arena
        .alloc_slice_with(out.len() - 1, |at| crate::sql::ast::Join {
            table: *out[at + 1],
            kind: JoinKind::Comma,
            on: None,
            using: None,
            natural: false,
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    let from = &*arena
        .alloc(FromClause {
            base: *out[0],
            joins,
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    // Members that do not resolve as one FROM list (a lateral reference)
    // are not plain tables.
    let plain = match QueryScope::resolve_schema(storage, from, txid, arena, None) {
        Ok(members) => (0..members.n)
            .all(|table| plain_table(storage, txid, &members, table, source(from, table))),
        Err(_) => false,
    };
    // SAFETY: the answer holds no arena data.
    unsafe { arena.rewind_to(mark) };
    Ok(plain)
}

/// [`modeled_join`] of the written FROM `written`.
fn modeled_join_written<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    written: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Option<ModeledJoin<'a>>, SqlError> {
    let Some(from) = nested_members(written, arena)? else {
        let modeled = modeled_join_over(storage, txid, statement, written, scope, params, arena)?;
        if modeled.is_none()
            && scope.has_full_merged()
            && (0..scope.n)
                .all(|table| plain_table(storage, txid, scope, table, source(written, table)))
        {
            // Its rows and their order would be pos3ql's join order's.
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a full join's merged USING column in this query is not supported by the join search"
            ));
        }
        return Ok(modeled);
    };
    // A parenthesized join's members are the join's relations, each its
    // own scope entry; its written entry's columns are theirs in order.
    if written
        .joins
        .iter()
        .any(|join| join.using.is_some() || join.natural)
    {
        return Ok(None);
    }
    let members = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
    let mut entries = [0u8; MAX_JOIN_RELS];
    let mut joined = Relids::EMPTY;
    let mut leaf = 0;
    for (table, item) in core::iter::once(&written.base)
        .chain(written.joins.iter().map(|join| &join.table))
        .enumerate()
    {
        let count = leaf_count(item);
        if table >= scope.n || leaf + count > members.n {
            return Ok(None);
        }
        let mut width: usize = (leaf..leaf + count)
            .map(|member| members.stored_width(member))
            .sum();
        // A parenthesized join's row: its columns, each member's again
        // (`member.*`), then whether each member's row is NULL.
        if parenthesized_join(item).is_some() {
            width = 2 * width + count;
            joined |= Relids::of(table);
        }
        if width != scope.stored_width(table) {
            return Ok(None);
        }
        entries[leaf..leaf + count].fill(table as u8);
        leaf += count;
    }
    let Some(mut model) =
        modeled_join_over(storage, txid, statement, from, &members, params, arena)?
    else {
        return Ok(None);
    };
    model.nested = Some(NestedMembers {
        scope: members,
        entries,
        joined,
    });
    Ok(Some(model))
}

/// A statement's join plan, planned at its first use. Every pass one
/// execution of the statement makes over its join (sizing groups, reading
/// aggregates, ordering groups or rows) reads the one plan, as PostgreSQL
/// plans a query once. Its holder fixes the statement, scope, parameters
/// and arena that plan it.
pub(crate) struct PlannedJoin<'a>(core::cell::OnceCell<Option<ModeledJoin<'a>>>);

impl<'a> PlannedJoin<'a> {
    pub(crate) const fn new() -> Self {
        Self(core::cell::OnceCell::new())
    }

    /// The statement's plan, `None` when the join search does not plan it.
    pub(crate) fn get(
        &self,
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        scope: &QueryScope<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Option<&ModeledJoin<'a>>, SqlError> {
        if self.0.get().is_none() {
            let planned = modeled_join(storage, txid, statement, scope, params, arena)?;
            let _ = self.0.set(planned);
        }
        Ok(self.0.get().and_then(Option::as_ref))
    }
}

/// Whether the join search plans `statement`; the arena space the plan
/// took is reclaimed.
pub(crate) fn models_join<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let mark = arena.mark();
    let modeled = modeled_join(storage, txid, statement, scope, params, arena)
        .map(|planned| planned.is_some());
    // SAFETY: the answer and its error hold no arena data, and planning
    // writes into nothing that outlives it.
    unsafe { arena.rewind_to(mark) };
    modeled
}

/// The FROM item count of `table`: one base relation, or a parenthesized
/// join's members.
fn leaf_count(table: &TableRef<'_>) -> usize {
    match parenthesized_join(table) {
        Some(inner) => {
            leaf_count(&inner.base)
                + inner
                    .joins
                    .iter()
                    .map(|join| leaf_count(&join.table))
                    .sum::<usize>()
        }
        None => 1,
    }
}

/// `from` with each parenthesized join replaced by its members, joined by
/// commas, or `None` when it holds none: the base relations of its join
/// tree in written order.
fn nested_members<'a>(
    from: &'a FromClause<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a FromClause<'a>>, SqlError> {
    fn leaves<'a>(
        table: &'a TableRef<'a>,
        out: &mut crate::mem::arena::ArenaList<'a, &'a TableRef<'a>>,
    ) -> Result<(), SqlError> {
        match parenthesized_join(table) {
            Some(inner) => {
                leaves(&inner.base, out)?;
                for join in inner.joins {
                    leaves(&join.table, out)?;
                }
                Ok(())
            }
            None => out.push(table).map_err(|_| crate::sql::eval::arena_full()),
        }
    }
    let items = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
    if items.clone().all(|item| parenthesized_join(item).is_none()) {
        return Ok(None);
    }
    let mut out = crate::mem::arena::ArenaList::new(arena);
    for item in items {
        leaves(item, &mut out)?;
    }
    let out = out.as_slice();
    let joins = arena
        .alloc_slice_with(out.len() - 1, |at| crate::sql::ast::Join {
            table: *out[at + 1],
            kind: JoinKind::Comma,
            on: None,
            using: None,
            natural: false,
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    arena
        .alloc(FromClause {
            base: *out[0],
            joins,
        })
        .map(|from| Some(&*from))
        .map_err(|_| crate::sql::eval::arena_full())
}

/// [`modeled_join`] of the base relations `from` (the written FROM, or its
/// parenthesized joins' members) resolved by `scope`, its join tree as
/// written.
fn modeled_join_over<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Option<ModeledJoin<'a>>, SqlError> {
    let statement = flatten_merged_statement(scope, statement, arena)?;
    let n = scope.n;
    // A query of one relation is planned here for its window functions.
    let windowed = statement.items.iter().any(|item| {
        matches!(item, SelectItem::Expr { expression, .. }
            if crate::sql::query::expr_has_window(expression))
    }) || statement
        .order_by
        .iter()
        .any(|order| crate::sql::query::expr_has_window(order.expression));
    // So is one whose aggregates have `DISTINCT` or `ORDER BY` inputs,
    // which its grouping may presort.
    let mut presorting = false;
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(statement.having)
        .chain(statement.order_by.iter().map(|order| order.expression))
    {
        let _ = super::super::walk_expression_tree(expression, &mut |node| {
            presorting |= matches!(node, Expr::Call { over: None, distinct, order_by, .. }
                if *distinct || !order_by.is_empty());
        });
    }
    if !(if windowed || presorting { 1 } else { 2 }..=MAX_JOIN_RELS).contains(&n)
        || n != from.joins.len() + 1
        || statement.set_body.is_some()
        || !statement.with.is_empty()
        || !statement.distinct_on.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.with_ties
        || !statement.locking.is_empty()
        || (windowed
            && (statement.distinct
                || (n == 1 && (!statement.group_by.is_empty() || statement.having.is_some()))))
    {
        return Ok(None);
    }
    let Some(limit) = super::constant_limit(statement.limit, statement.offset, arena)? else {
        return Ok(None);
    };
    let methods = crate::sql::guc::planner_methods();
    let value = crate::sql::guc::planner_value;
    for (index, join) in from.joins.iter().enumerate() {
        if !matches!(
            join.kind,
            JoinKind::Inner
                | JoinKind::Cross
                | JoinKind::Comma
                | JoinKind::Left
                | JoinKind::Right
                | JoinKind::Full
                | JoinKind::Semi
                | JoinKind::Anti
        ) || ((join.using.is_some() || join.natural) && scope.join_on[index].is_none())
        {
            return Ok(None);
        }
    }
    for table in 0..n {
        let tref = source(from, table);
        if !plain_table(storage, txid, scope, table, tref) {
            return Ok(None);
        }
    }
    // The output: plain columns and wildcards.
    let mut output = [0u64; MAX_JOIN_RELS];
    // Each relation's columns in the order its target list takes them:
    // the output's first, then those the joins need
    // (`build_base_rel_tlists`, then `add_vars_to_targetlist`).
    let mut appearance = [[0u8; 64]; MAX_JOIN_RELS];
    let mut appeared = [0usize; MAX_JOIN_RELS];
    let mut note = |table: usize, column: usize| {
        if column < 64 && !appearance[table][..appeared[table]].contains(&(column as u8)) {
            appearance[table][appeared[table]] = column as u8;
            appeared[table] += 1;
        }
    };
    let width = |table: usize| {
        scope
            .defs
            .stored(table)
            .map_or(0, |def| def.columns().len().min(64))
    };
    // The select list's expressions' widths and their cost per row; an
    // aggregating query's per group (`create_agg_path`'s target cost).
    let mut expression_width = 0.0;
    // The window functions' share of it.
    let mut window_items_width = 0.0;
    let mut target_cost = 0.0;
    let mut group_target_cost = 0.0;
    let mut aggregated = false;
    let mut unaggregated = false;
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) if !scope.merged_output().is_empty() => {
                for &entry in scope.merged_output() {
                    let single;
                    let columns = match entry {
                        ResolvedColumn::Table(table, column) => {
                            single = [(table, column)];
                            &single[..]
                        }
                        // The columns its join alias variable reads.
                        ResolvedColumn::Merged(merged) => scope.merged[merged].vars,
                        _ => {
                            return Ok(None);
                        }
                    };
                    for &(table, column) in columns {
                        if column >= 64 {
                            return Ok(None);
                        }
                        output[table] |= 1 << column;
                        note(table, column);
                    }
                }
            }
            SelectItem::Wildcard(_) => {
                for (table, mask) in output.iter_mut().enumerate().take(n) {
                    *mask = u64::MAX;
                    (0..width(table)).for_each(|column| note(table, column));
                }
            }
            SelectItem::TableWildcard(qualifier) => {
                let Some(table) = (0..n).find(|&table| scope.names[table] == *qualifier) else {
                    return Ok(None);
                };
                output[table] = u64::MAX;
                (0..width(table)).for_each(|column| note(table, column));
            }
            SelectItem::Expr { expression, .. } => match column_of(scope, expression) {
                Some((table, column)) if column < 64 => {
                    output[table] |= 1 << column;
                    note(table, column);
                    unaggregated = true;
                }
                Some(_) => {
                    return Ok(None);
                }
                // An expression: the columns it reads, in the order it
                // reads them (`pull_var_clause`), and its width and cost.
                None => {
                    let Some(vars) = qual_vars(scope, expression) else {
                        return Ok(None);
                    };
                    let columns = crate::sql::query::CatalogScopeCols {
                        scope,
                        outer: None,
                        storage,
                        txid,
                        arena,
                    };
                    // A window function's value: computed by its window,
                    // which reads its arguments' columns.
                    if matches!(expression, Expr::Call { over: Some(_), .. }) {
                        let Some((entry_width, _)) =
                            target_entry(storage, txid, scope, expression, arena)
                        else {
                            return Ok(None);
                        };
                        expression_width += entry_width;
                        window_items_width += entry_width;
                        for &(table, column) in vars.as_slice() {
                            output[usize::from(table)] |= 1 << column;
                            note(usize::from(table), usize::from(column));
                        }
                        continue;
                    }
                    if crate::sql::query::expr_has_window(expression)
                        || crate::sql::query::has_set_returning_items(
                            core::slice::from_ref(item),
                            storage,
                            txid,
                        )
                    {
                        return Ok(None);
                    }
                    let Some((entry_width, cost)) =
                        target_entry(storage, txid, scope, expression, arena)
                    else {
                        return Ok(None);
                    };
                    group_target_cost += cost;
                    if crate::sql::query::expr_has_aggregate(expression, storage, txid, &columns)? {
                        // An aggregate: the join returns the columns its
                        // arguments read.
                        aggregated = true;
                    } else {
                        expression_width += entry_width;
                        target_cost += cost;
                        unaggregated = true;
                    }
                    for &(table, column) in vars.as_slice() {
                        output[usize::from(table)] |= 1 << column;
                        note(usize::from(table), usize::from(column));
                    }
                }
            },
            SelectItem::RecordStar(_) => {
                return Ok(None);
            }
        }
    }
    // The `GROUP BY` columns, once each, which the joined rows return.
    let grouped = !statement.group_by.is_empty();
    // Windows over one relation's groups are not planned here.
    if windowed && n == 1 && aggregated {
        return Ok(None);
    }
    // A `HAVING` left after its conditions without aggregates moved to
    // `WHERE` filters the groups: its cost and selectivity (`cost_agg`).
    let having = match statement.having {
        Some(condition) => {
            let columns = crate::sql::query::CatalogScopeCols {
                scope,
                outer: None,
                storage,
                txid,
                arena,
            };
            match super::having_quals(condition, storage, txid, &columns, arena)? {
                Some(quals) => Some(quals),
                None => {
                    return Ok(None);
                }
            }
        }
        None => None,
    };
    let mut group_columns = [(0usize, 0usize); super::joins::MAX_KEYS];
    let mut group_count = 0;
    // The grouping keys in `GROUP BY` order, each with its position there:
    // columns, and expressions such as a full join's merged column.
    let mut group_targets = [(SortTarget::Column(0, 0), 0usize); super::joins::MAX_KEYS];
    let mut group_target_count = 0;
    for (position, &expression) in statement.group_by.iter().enumerate() {
        let Some((table, column)) = column_of(scope, expression) else {
            if group_targets[..group_target_count]
                .iter()
                .any(|&(held, _)| held == SortTarget::Expression(expression))
            {
                continue;
            }
            let Some(vars) = qual_vars(scope, expression) else {
                return Ok(None);
            };
            if vars.as_slice().is_empty()
                || expression.contains_volatile_function().is_some()
                || expression.contains_subquery()
                || crate::sql::query::expr_has_window(expression)
                || crate::sql::query::expression_has_project_set(expression, storage, txid)
                || group_target_count == group_targets.len()
            {
                return Ok(None);
            }
            for &(table, column) in vars.as_slice() {
                output[usize::from(table)] |= 1 << column;
                note(usize::from(table), usize::from(column));
            }
            group_targets[group_target_count] = (SortTarget::Expression(expression), position);
            group_target_count += 1;
            continue;
        };
        let ctype = scope
            .defs
            .stored(table)
            .map(|def| def.columns()[column].ctype);
        if column >= 64 || !ctype.is_some_and(joinable) {
            return Ok(None);
        }
        if group_columns[..group_count].contains(&(table, column)) {
            continue;
        }
        if group_count == group_columns.len() {
            return Ok(None);
        }
        group_columns[group_count] = (table, column);
        group_count += 1;
        group_targets[group_target_count] = (SortTarget::Column(table, column), position);
        group_target_count += 1;
        output[table] |= 1 << column;
        note(table, column);
    }
    let group_columns = &group_columns[..group_count];
    let group_targets = &group_targets[..group_target_count];
    // A grouped query's select list is its grouping columns, aggregates
    // and expressions of them, which parse analysis checked.
    if grouped
        && statement.items.iter().any(|item| match item {
            SelectItem::Expr { expression, .. } => {
                column_of(scope, expression).is_some_and(|column| !group_columns.contains(&column))
            }
            _ => true,
        })
    {
        return Ok(None);
    }
    // A plain aggregate's query returns one row of aggregates.
    if aggregated
        && !grouped
        && (unaggregated
            || statement
                .items
                .iter()
                .any(|item| !matches!(item, SelectItem::Expr { .. })))
    {
        return Ok(None);
    }
    if having.is_some() && !grouped && !aggregated {
        return Ok(None);
    }
    // A `DISTINCT`'s keys: the select list's columns and expressions, once
    // each, with their select list positions.
    let distinct = statement.distinct;
    let mut distinct_targets = [(SortTarget::Column(0, 0), 0usize); super::joins::MAX_KEYS];
    let mut distinct_count = 0;
    if distinct {
        if grouped || aggregated {
            return Ok(None);
        }
        for (position, item) in statement.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else {
                return Ok(None);
            };
            let target = match column_of(scope, expression) {
                Some((table, column)) => {
                    let ctype = scope
                        .defs
                        .stored(table)
                        .map(|def| def.columns()[column].ctype);
                    if !ctype.is_some_and(joinable) {
                        return Ok(None);
                    }
                    SortTarget::Column(table, column)
                }
                None => {
                    if qual_vars(scope, expression).is_none_or(|vars| vars.as_slice().is_empty())
                        || expression.contains_volatile_function().is_some()
                        || expression.contains_subquery()
                        || crate::sql::query::expr_has_window(expression)
                        || crate::sql::query::expression_has_project_set(expression, storage, txid)
                    {
                        return Ok(None);
                    }
                    SortTarget::Expression(expression)
                }
            };
            if distinct_targets[..distinct_count]
                .iter()
                .any(|&(held, _)| held == target)
            {
                continue;
            }
            if distinct_count == distinct_targets.len() {
                return Ok(None);
            }
            distinct_targets[distinct_count] = (target, position);
            distinct_count += 1;
        }
    }
    let distinct_targets = &distinct_targets[..distinct_count];
    // The `ORDER BY` items, columns and expressions, which the target list
    // holds after the output's when it does not list them (`resjunk`
    // entries).
    let mut sort_entries =
        [(SortTarget::Expression(&Expr::Null), 0u8, 0u8); super::joins::MAX_KEYS];
    let mut sort_count = 0;
    let mut junk_columns = [(0usize, 0usize); super::joins::MAX_KEYS];
    let mut junk_count = 0;
    // The width of the unlisted expressions, below and above a grouping.
    let mut junk_width = 0.0;
    let mut grouped_junk_width = 0.0;
    for (item, order) in statement.order_by.iter().enumerate() {
        let target = crate::sql::query::resolve_order_target(
            order.expression,
            statement.items,
            scope,
            arena,
            storage,
            txid,
            None,
        )?;
        if sort_count == super::joins::MAX_KEYS {
            return Ok(None);
        }
        let flags = if order.descending {
            super::joins::KEY_DESCENDING
        } else {
            0
        } | if order.nulls_first {
            super::joins::KEY_NULLS_FIRST
        } else {
            0
        };
        let Some((table, column)) = column_of(scope, target) else {
            // An expression: a class of its own, its columns read below
            // the sort (or the grouping that computes it), and its width
            // and cost when the select list does not hold it.
            let Some(vars) = qual_vars(scope, target) else {
                return Ok(None);
            };
            let columns = crate::sql::query::CatalogScopeCols {
                scope,
                outer: None,
                storage,
                txid,
                arena,
            };
            if crate::sql::query::expr_has_window(target)
                || target.contains_subquery()
                || crate::sql::query::expression_has_project_set(target, storage, txid)
            {
                return Ok(None);
            }
            let has_aggregate =
                crate::sql::query::expr_has_aggregate(target, storage, txid, &columns)?;
            if has_aggregate && !(grouped || aggregated) {
                return Ok(None);
            }
            let Some((entry_width, cost)) = target_entry(storage, txid, scope, target, arena)
            else {
                return Ok(None);
            };
            let listed = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *target)
            });
            if !listed {
                if grouped || aggregated {
                    grouped_junk_width += entry_width;
                    group_target_cost += cost;
                } else {
                    junk_width += entry_width;
                    target_cost += cost;
                }
            }
            {
                for &(table, column) in vars.as_slice() {
                    output[usize::from(table)] |= 1 << column;
                    note(usize::from(table), usize::from(column));
                }
            }
            sort_entries[sort_count] = (SortTarget::Expression(target), flags, item as u8);
            sort_count += 1;
            continue;
        };
        let ctype = scope
            .defs
            .stored(table)
            .map(|def| def.columns()[column].ctype);
        if column >= 64
            || !ctype.is_some_and(joinable)
            || (grouped && !group_columns.contains(&(table, column)))
            || (aggregated && !grouped)
        {
            return Ok(None);
        }
        sort_entries[sort_count] = (SortTarget::Column(table, column), flags, item as u8);
        sort_count += 1;
        // `findTargetlistEntrySQL92`: an output column the key names.
        let listed = statement.items.iter().any(|item| match item {
            SelectItem::Wildcard(_) if !scope.merged_output().is_empty() => {
                scope.merged_output().iter().any(|&entry| match entry {
                    ResolvedColumn::Table(listed, at) => (listed, at) == (table, column),
                    ResolvedColumn::Merged(merged) => {
                        scope.merged_plain_var(merged) == Some((table, column))
                    }
                    _ => false,
                })
            }
            SelectItem::Wildcard(_) => true,
            SelectItem::TableWildcard(qualifier) => scope.names[table] == *qualifier,
            SelectItem::Expr { expression, .. } => {
                column_of(scope, expression) == Some((table, column))
            }
            SelectItem::RecordStar(_) => false,
        });
        if !listed && !junk_columns[..junk_count].contains(&(table, column)) {
            junk_columns[junk_count] = (table, column);
            junk_count += 1;
        }
        output[table] |= 1 << column;
        note(table, column);
    }
    let sort_entries = &sort_entries[..sort_count];
    // The join tree as written: each comma-separated item a chain of joins
    // (each join's range table entry numbered in written order), then the
    // `FROM` list holding the `WHERE` conditions.
    let mut tree = Tree::new(n);
    tree.enable_hashagg = value("enable_hashagg") != 0.0;
    tree.from_collapse_limit = value("from_collapse_limit") as i64;
    tree.join_collapse_limit = value("join_collapse_limit") as i64;
    // The pulled-up `EXISTS` and `IN` subqueries' semi and anti joins come
    // last; each wraps the joins and `WHERE` before it
    // (`pull_up_sublinks`).
    let written = statement.from.as_ref().expect("a modeled join reads FROM");
    let semi_start = written
        .joins
        .iter()
        .position(|join| join.kind.is_semi_or_anti())
        .unwrap_or(written.joins.len());
    if written.joins[semi_start..]
        .iter()
        .any(|join| !join.kind.is_semi_or_anti())
    {
        return Ok(None);
    }
    // Each written join's node and its `ON` (a `USING` or `NATURAL` join's
    // implied equalities), in the order `deconstruct_jointree` reads them.
    let mut chain = ChainBuilder {
        tree: &mut tree,
        next: 0,
        rtindex: 0,
        sources: [(0, &Expr::Null); MAX_JOIN_RELS],
        source_count: 0,
    };
    let mut items = [0u8; MAX_JOIN_RELS];
    let mut item_count = 0;
    let built = chain.chain(
        &written.base,
        &written.joins[..semi_start],
        |index| scope.join_on.get(index).copied().flatten(),
        &mut items,
        &mut item_count,
    );
    let Some(()) = built else {
        return Ok(None);
    };
    let Some(inner_root) = chain.tree.list_of(&items[..item_count]) else {
        return Ok(None);
    };
    let mut current = inner_root;
    for (index, join) in written.joins.iter().enumerate().skip(semi_start) {
        let Some(right) = chain.item(&join.table) else {
            return Ok(None);
        };
        let kind = if join.kind == JoinKind::Semi {
            TreeJoin::Semi
        } else {
            TreeJoin::Anti
        };
        let Some(node) = chain.tree.node(TreeNode::Join {
            kind,
            left: current,
            right,
            rtindex: None,
        }) else {
            return Ok(None);
        };
        if let Some(on) = join.on.or(scope.join_on.get(index).copied().flatten())
            && chain.source(node, on).is_none()
        {
            return Ok(None);
        }
        current = node;
    }
    if usize::from(chain.next) != n {
        return Ok(None);
    }
    let root = if semi_start == written.joins.len() {
        inner_root
    } else {
        let Some(root) = chain.tree.list_of(&[current]) else {
            return Ok(None);
        };
        root
    };
    let joined = &chain.sources[..chain.source_count];
    let joined = arena
        .alloc_slice_copy(joined)
        .map_err(|_| crate::sql::eval::arena_full())?;
    tree.root = root;
    // The conditions: each join's `ON`, then `WHERE`.
    let mut quals = [&Expr::Null; MAX_QUALS + 1];
    let mut constants = Constants {
        held: [(&Expr::Null, ColType::Int4); MAX_QUALS + 1],
        count: 0,
    };
    let mut expressions = Expressions::new(storage, txid, arena);
    // The written left and right operands of each full join's equality
    // clause, in pairs, which a `COALESCE` above it must name in order.
    let mut full_clauses = [(0u8, 0u16); 2 * (MAX_QUALS + 1)];
    let mut full_clause_count = 0;
    let sources = joined.iter().map(|&(node, on)| (Some(node), on)).chain(
        statement
            .where_clause
            .map(|condition| (Some(inner_root), condition)),
    );
    for (node, condition) in sources {
        let Some(node) = node else {
            return Ok(None);
        };
        let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
        let mut count = 0;
        if !super::super::plan::flatten_and(condition, &mut conjuncts, &mut count) {
            return Err(preparation_full());
        }
        for &conjunct in &conjuncts[..count] {
            let conjunct = flatten_merged(scope, conjunct, arena)?;
            let Some(qual) = analyze_qual(
                scope,
                conjunct,
                &mut constants,
                &mut expressions,
                &full_clauses[..full_clause_count],
            )?
            else {
                return Ok(None);
            };
            if let (
                TreeNode::Join {
                    kind: TreeJoin::Full,
                    ..
                },
                Shape::Columns(one, two),
            ) = (tree.nodes[usize::from(node)], qual.shape)
            {
                full_clauses[full_clause_count] = one;
                full_clauses[full_clause_count + 1] = two;
                full_clause_count += 2;
            }
            let Some(id) = tree.qual(node, qual) else {
                return Err(preparation_full());
            };
            quals[usize::from(id)] = conjunct;
        }
    }
    for (id, vars) in tree
        .expression_vars
        .iter_mut()
        .enumerate()
        .take(expressions.count)
    {
        *vars = expressions.get(id).vars;
    }
    tree.expression_parts = expressions.parts;
    let mut prepared = match join_tree::prepare(&tree, arena) {
        Ok(prepared) => prepared,
        Err(join_tree::FULL) => return Err(preparation_full()),
        Err(join_tree::ARENA) => return Err(crate::sql::eval::arena_full()),
        Err(_) => return Ok(None),
    };
    // The constant `true` that replaced outer join clauses
    // (`reconsider_outer_join_clauses`) is the condition after the query's.
    quals[tree.qual_count] = &Expr::Bool(true);
    for placed in prepared
        .placed
        .as_mut_slice()
        .map_err(|_| crate::sql::eval::arena_full())?
    {
        if let Placed::Join(join) = placed
            && join.qual == join_tree::TRUE_QUAL
        {
            join.qual = tree.qual_count as join_tree::QualId;
        }
    }
    let quals = &quals[..=tree.qual_count];
    // `remove_useless_joins`: a left join to one relation no row above
    // reads, which a unique index makes match at most one row, is dropped
    // with it.
    let mut removed: Relids = Relids::EMPTY;
    'restart: loop {
        for at in 0..prepared.special_count {
            let info = prepared.specials[at];
            if info.kind != super::joins::SpecialKind::Left
                || info.min_right.count_ones() != 1
                || info.min_right & !BASE_RELIDS != Relids::EMPTY
            {
                continue;
            }
            let table = info.min_right.trailing_zeros() as usize;
            if !join_is_removable(storage, txid, scope, &prepared, &info, table, output[table]) {
                continue;
            }
            if info.syn_right != info.min_right {
                return Ok(None);
            }
            if prepared.remove_left_join(at).is_err() {
                return Ok(None);
            }
            removed |= info.min_right;
            continue 'restart;
        }
        break;
    }
    let self_join = removable_self_join(storage, txid, scope, &prepared, removed);
    let classes = prepared.classes();
    let class_count = classes.len();
    // The constant a class equates its members with.
    let constant_of = |class: usize| -> Option<&'a Expr<'a>> {
        let qual = prepared.class_constant[class]?;
        let Expr::Binary { left, right, .. } = quals[usize::from(qual)] else {
            return None;
        };
        Some(if plain_constant(right).is_some() {
            right
        } else {
            left
        })
    };
    // Each relation's restrictions in the order they reach it
    // (`baserestrictinfo`).
    let mut restrictions = [None::<&'a Expr<'a>>; MAX_JOIN_RELS];
    for placed in prepared.placed() {
        let (table, restriction) = match *placed {
            Placed::Restriction { rel, qual } => (usize::from(rel), quals[usize::from(qual)]),
            // An expression of several relations equated with the
            // constant filters their join, below.
            Placed::Implied { member, .. } if member.reads != Relids::EMPTY => continue,
            Placed::Implied { member, class } => {
                let Some(value) = constant_of(usize::from(class)) else {
                    return Ok(None);
                };
                (
                    usize::from(member.rel),
                    equals_constant(scope, &expressions, member, value, arena)?,
                )
            }
            Placed::Equated { left, right } => (
                usize::from(left.rel),
                equality(scope, &expressions, left, right, arena)?,
            ),
            Placed::Join(_) => continue,
        };
        restrictions[table] = Some(match restrictions[table] {
            None => restriction,
            Some(held) => and(held, restriction, arena)?,
        });
    }
    // The join clauses, in the order PostgreSQL distributes them; each
    // relation's `joininfo` holds those required above it.
    let costs = Costs::current();
    let mut clauses = crate::mem::arena::ArenaList::<Clause>::new(arena);
    // The equalities of expressions of several relations with their
    // class's constant, conditions after the query's.
    let mut implied = [&Expr::Null; MAX_JOIN_EXPRESSIONS];
    let mut implied_count = 0;
    for placed in prepared.placed() {
        let join = match *placed {
            Placed::Join(join) => join,
            // `process_implied_equality` of a member reading several
            // relations: a join clause of them all.
            Placed::Implied { member, class } if member.reads != Relids::EMPTY => {
                let Some(value) = constant_of(usize::from(class)) else {
                    return Ok(None);
                };
                if implied_count == implied.len() {
                    return Ok(None);
                }
                implied[implied_count] =
                    equals_constant(scope, &expressions, member, value, arena)?;
                implied_count += 1;
                let relids = member.relids();
                join_tree::JoinQual {
                    qual: (quals.len() + implied_count - 1) as join_tree::QualId,
                    relids,
                    required: relids,
                    pushed_down: true,
                    clone: super::joins::CloneKind::None,
                    incompatible: Relids::EMPTY,
                    outer_relids: Relids::EMPTY,
                    merge: None,
                }
            }
            _ => continue,
        };
        let member_cost = |member: Member| {
            member
                .expression_id()
                .map_or(0.0, |id| expressions.get(id).cost)
        };
        let (form, cost) = match join.merge {
            Some((left, right, left_class, right_class)) => (
                ClauseForm::Join {
                    left,
                    right,
                    left_class,
                    right_class,
                },
                costs.cpu_operator + member_cost(left) + member_cost(right),
            ),
            None => (ClauseForm::Other, costs.cpu_operator),
        };
        clauses
            .push(Clause {
                form,
                relids: join.relids,
                required: join.required,
                pushed_down: join.pushed_down,
                clone: join.clone,
                incompatible: join.incompatible,
                cost,
                selectivity: None,
                outer_selectivity: None,
                outer_relids: join.outer_relids,
                operands: None,
                other: super::joins::OtherShape::Plain,
                constant: quals
                    .get(usize::from(join.qual))
                    .is_some_and(|qual| matches!(qual, Expr::Bool(true))),
                source: Some(join.qual),
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    let placed_join_count = clauses.len();
    let quals = &*arena
        .alloc_slice_with(quals.len() + implied_count, |at| {
            match at.checked_sub(quals.len()) {
                Some(extra) => implied[extra],
                None => quals[at],
            }
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    // The outer joins that can null each relation (`nulling_relids`).
    let mut nulling_relids = [Relids::EMPTY as Relids; MAX_JOIN_RELS];
    for special in &prepared.specials[..prepared.special_count] {
        let nullable = match special.kind {
            super::joins::SpecialKind::Left | super::joins::SpecialKind::Anti => special.syn_right,
            super::joins::SpecialKind::Full => special.syn_left | special.syn_right,
            super::joins::SpecialKind::Semi => Relids::EMPTY,
        };
        for (table, relids) in nulling_relids.iter_mut().enumerate().take(n) {
            if nullable & Relids::of(table) != Relids::EMPTY {
                *relids |= special.oj;
            }
        }
    }
    // The outer joins that null any of the base relations `relids`.
    let member_nulling = |relids: Relids| {
        (0..n)
            .filter(|&table| relids & Relids::of(table) != Relids::EMPTY)
            .fold(Relids::EMPTY, |nulling, table| {
                nulling | nulling_relids[table]
            })
            & !BASE_RELIDS
    };
    // The query's pathkeys (`make_pathkeys_for_sortclauses`): each `ORDER
    // BY` column's class, a new one when no class holds the column as the
    // query's output reads it, a key of a class already ordered or of a
    // constant's dropped as redundant.
    let mut extended = [EquivalenceClass {
        members: &[],
        has_const: false,
    }; join_tree::MAX_CLASSES];
    extended[..class_count].copy_from_slice(classes);
    let mut class_count = class_count;
    // `group_pathkeys` (`preprocess_groupclause`,
    // `make_pathkeys_for_sortclauses`): the grouping columns the `ORDER BY`
    // leads with, in its order, then the others, each sorted as the first
    // `ORDER BY` item naming it sorts; a column of a class with a constant
    // or of an earlier key's class is redundant and leaves the grouping.
    let mut group_keys = Keys::NONE;
    let mut group_members = [Member::plain(0, 0); super::joins::MAX_KEYS];
    // Each key's position in the `GROUP BY` (a `DISTINCT`'s in the select
    // list) and class.
    let mut group_shown = [(0usize, 0u8); super::joins::MAX_KEYS];
    // The class of each expression key, which an `ORDER BY` of the same
    // expression shares.
    let mut group_expression_classes = [(0u8, &Expr::Null); super::joins::MAX_KEYS];
    let mut group_expression_count = 0;
    // A `DISTINCT`'s pathkeys (`transformDistinctClause`): its `ORDER BY`
    // items, which it lists, then its other columns ascending.
    let keyed = if grouped {
        group_targets
    } else {
        distinct_targets
    };
    if grouped || distinct {
        let mut ordered = [((SortTarget::Column(0, 0), 0usize), 0u8); super::joins::MAX_KEYS];
        let mut ordered_count = 0;
        for &(target, flags, _) in sort_entries {
            let Some(&key) = keyed.iter().find(|&&(held, _)| held == target) else {
                if distinct {
                    return Ok(None);
                }
                break;
            };
            if !ordered[..ordered_count]
                .iter()
                .any(|&((held, _), _)| held == target)
            {
                ordered[ordered_count] = (key, flags);
                ordered_count += 1;
            }
        }
        for &key in keyed {
            if ordered[..ordered_count]
                .iter()
                .any(|&((held, _), _)| held == key.0)
            {
                continue;
            }
            let flags = sort_entries
                .iter()
                .find(|&&(target, ..)| target == key.0)
                .map_or(0, |&(_, flags, _)| flags);
            ordered[ordered_count] = (key, flags);
            ordered_count += 1;
        }
        for &((target, position), flags) in &ordered[..ordered_count] {
            let member = match target {
                SortTarget::Column(table, column) => Member {
                    rel: table as u8,
                    column: column as u16,
                    nulling: nulling_relids[table] & !BASE_RELIDS,
                    reads: Relids::EMPTY,
                },
                // An expression key is a member of its own class
                // (`get_eclass_for_sort_expr`), which the join of the
                // relations it reads computes.
                SortTarget::Expression(expression) => {
                    let Some((member, ..)) = expressions.member(scope, expression)? else {
                        return Ok(None);
                    };
                    Member {
                        nulling: member_nulling(member.base_relids()),
                        ..member
                    }
                }
            };
            let class = match extended[..class_count]
                .iter()
                .position(|class| class.members().contains(&member))
            {
                Some(class) => class,
                None => {
                    if class_count == extended.len() {
                        return Err(preparation_full());
                    }
                    extended[class_count].members = single_member(member, arena)?;
                    class_count += 1;
                    class_count - 1
                }
            };
            if extended[class].has_const
                || group_keys
                    .as_slice()
                    .iter()
                    .any(|&key| usize::from(key & KEY_CLASS) == class)
            {
                continue;
            }
            if let SortTarget::Expression(expression) = target {
                group_expression_classes[group_expression_count] = (class as u8, expression);
                group_expression_count += 1;
            }
            let at = group_keys.as_slice().len();
            group_members[at] = member;
            group_shown[at] = (position, class as u8);
            let _ = group_keys.push_class(class as u8 | flags);
        }
        if group_keys.as_slice().is_empty() {
            return Ok(None);
        }
    }
    let mut sort_keys = Keys::NONE;
    let mut sort_items = [0u8; super::joins::MAX_KEYS];
    let mut sort_shown = [None::<(usize, usize)>; super::joins::MAX_KEYS];
    // The class of each expression key, which an equal expression shares.
    let mut expression_classes = group_expression_classes;
    let mut expression_class_count = group_expression_count;
    for &(target, flags, item) in sort_entries {
        let mut several: Relids = Relids::EMPTY;
        let (table, column) = match target {
            SortTarget::Column(table, column) => (table, column),
            SortTarget::Expression(expression) => {
                // A constant's key is redundant (`ec_has_const`).
                if expression.is_constant(false) {
                    continue;
                }
                // An expression of one relation's columns sorting the joined
                // rows is a member of its class (`get_eclass_for_sort_expr`).
                if !(grouped || aggregated || windowed)
                    && let Some((member, ..)) = expressions.member(scope, expression)?
                {
                    several = member.reads;
                    (usize::from(member.rel), usize::from(member.column))
                } else {
                    let class = match expression_classes[..expression_class_count]
                        .iter()
                        .find(|&&(_, held)| *held == *expression)
                    {
                        Some(&(class, _)) => usize::from(class),
                        None => {
                            if class_count == extended.len() {
                                return Err(preparation_full());
                            }
                            extended[class_count].members = &[];
                            extended[class_count].has_const = false;
                            expression_classes[expression_class_count] =
                                (class_count as u8, expression);
                            expression_class_count += 1;
                            class_count += 1;
                            class_count - 1
                        }
                    };
                    if extended[class].has_const
                        || sort_keys
                            .as_slice()
                            .iter()
                            .any(|&key| usize::from(key & KEY_CLASS) == class)
                    {
                        continue;
                    }
                    sort_shown[sort_keys.as_slice().len()] = None;
                    sort_items[sort_keys.as_slice().len()] = item;
                    let _ = sort_keys.push_class(class as u8 | flags);
                    continue;
                }
            }
        };
        let member = Member {
            rel: table as u8,
            column: column as u16,
            nulling: member_nulling(Relids::of(table) | several),
            reads: several,
        };
        let class = match extended[..class_count]
            .iter()
            .position(|class| class.members().contains(&member))
        {
            Some(class) => class,
            None => {
                if class_count == extended.len() {
                    return Err(preparation_full());
                }
                extended[class_count].members = single_member(member, arena)?;
                class_count += 1;
                class_count - 1
            }
        };
        if extended[class].has_const
            || sort_keys
                .as_slice()
                .iter()
                .any(|&key| usize::from(key & KEY_CLASS) == class)
        {
            continue;
        }
        // The sort shows the first target list entry its class holds
        // (`prepare_sort_from_pathkeys`).
        let in_class = |(table, column): (usize, usize)| {
            extended[class].members().iter().any(|held| {
                (usize::from(held.rel), usize::from(held.column)) == (table, column)
                    && held.nulling == nulling_relids[table] & !BASE_RELIDS
            })
        };
        let mut shown_member = None;
        for item in statement.items {
            let found = match item {
                SelectItem::Wildcard(_) if !scope.merged_output().is_empty() => {
                    scope.merged_output().iter().find_map(|&entry| match entry {
                        ResolvedColumn::Table(table, column) => {
                            Some((table, column)).filter(|&entry| in_class(entry))
                        }
                        ResolvedColumn::Merged(merged) => scope
                            .merged_plain_var(merged)
                            .filter(|&part| in_class(part)),
                        _ => None,
                    })
                }
                SelectItem::Wildcard(_) => (0..n).find_map(|table| {
                    (0..width(table))
                        .map(|column| (table, column))
                        .find(|&entry| in_class(entry))
                }),
                SelectItem::TableWildcard(qualifier) => (0..n)
                    .filter(|&table| scope.names[table] == *qualifier)
                    .find_map(|table| {
                        (0..width(table))
                            .map(|column| (table, column))
                            .find(|&entry| in_class(entry))
                    }),
                SelectItem::Expr { expression, .. } => column_of(scope, expression)
                    .or_else(|| {
                        expressions
                            .find(scope, expression)
                            .map(|member| (usize::from(member.rel), usize::from(member.column)))
                    })
                    .filter(|&entry| in_class(entry)),
                SelectItem::RecordStar(_) => None,
            };
            if found.is_some() {
                shown_member = found;
                break;
            }
        }
        let shown_member = shown_member
            .or_else(|| {
                junk_columns[..junk_count]
                    .iter()
                    .copied()
                    .find(|&entry| in_class(entry))
            })
            .unwrap_or((table, column));
        sort_shown[sort_keys.as_slice().len()] = Some(shown_member);
        sort_items[sort_keys.as_slice().len()] = item;
        let _ = sort_keys.push_class(class as u8 | flags);
    }
    // `adjust_group_pathkeys_for_groupagg`: the grouping's pathkeys
    // extended by the ordered aggregates' (`DISTINCT` or `ORDER BY`
    // inputs) that the most of them share, which then aggregate presorted
    // rows; each added key's expression and flags.
    let group_columns_count = group_keys.as_slice().len() as u8;
    let mut presort_tail = [(&Expr::Null, 0u8); super::joins::MAX_KEYS];
    let mut presort_count = 0usize;
    if (grouped || aggregated) && !distinct && value("enable_presorted_aggregate") != 0.0 {
        let columns = crate::sql::query::CatalogScopeCols {
            scope,
            outer: None,
            storage,
            txid,
            arena,
        };
        let mut calls = crate::mem::arena::ArenaList::new(arena);
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                crate::sql::query::collect_aggs(expression, &mut calls, storage, txid, &columns)?;
            }
        }
        for &(target, ..) in sort_entries {
            if let SortTarget::Expression(expression) = target {
                crate::sql::query::collect_aggs(expression, &mut calls, storage, txid, &columns)?;
            }
        }
        if let Some(having) = statement.having {
            crate::sql::query::collect_aggs(having, &mut calls, storage, txid, &columns)?;
        }
        // Each ordered aggregate's pathkeys (`make_pathkeys_for_sortclauses`
        // of its `DISTINCT` arguments or its `ORDER BY`), with the
        // expression and flags of each key.
        const MAX_ORDERED: usize = 16;
        let mut ordered = [(Keys::NONE, [(&Expr::Null, 0u8); super::joins::MAX_KEYS]); MAX_ORDERED];
        let mut ordered_count = 0usize;
        let calls = calls.as_slice();
        for (at, &(_, call)) in calls.iter().enumerate() {
            let Expr::Call {
                args,
                filter,
                distinct: distinct_input,
                order_by,
                ..
            } = call
            else {
                continue;
            };
            if (!*distinct_input && order_by.is_empty())
                || calls[..at].iter().any(|&(_, earlier)| *earlier == *call)
            {
                continue;
            }
            // A filtered aggregate presorts only plain arguments.
            if filter.is_some()
                && args.iter().any(|argument| {
                    column_of(scope, argument).is_none() && !argument.is_constant(false)
                })
            {
                continue;
            }
            if ordered_count == MAX_ORDERED {
                return Ok(None);
            }
            let mut keys = Keys::NONE;
            let mut shown = [(&Expr::Null, 0u8); super::joins::MAX_KEYS];
            let items = args.len().max(order_by.len());
            for index in 0..items {
                let (expression, flags) = if *distinct_input {
                    match args.get(index) {
                        Some(&argument) => (argument, 0u8),
                        None => break,
                    }
                } else {
                    match order_by.get(index) {
                        Some(order) => (
                            order.expression,
                            if order.descending {
                                super::joins::KEY_DESCENDING
                            } else {
                                0
                            } | if order.nulls_first {
                                super::joins::KEY_NULLS_FIRST
                            } else {
                                0
                            },
                        ),
                        None => break,
                    }
                };
                if expression.is_constant(false) {
                    continue;
                }
                // A column, or an expression of one relation's columns
                // (`get_eclass_for_sort_expr`).
                let Some((member, ctype, _)) = expressions.member(scope, expression)? else {
                    return Ok(None);
                };
                if (member.expression_id().is_none() && member.column >= 64) || !joinable(ctype) {
                    return Ok(None);
                }
                let member = Member {
                    nulling: member_nulling(member.base_relids()),
                    ..member
                };
                let class = match extended[..class_count]
                    .iter()
                    .position(|class| class.members().contains(&member))
                {
                    Some(class) => class,
                    None => {
                        if class_count == extended.len() {
                            return Err(preparation_full());
                        }
                        extended[class_count].members = single_member(member, arena)?;
                        extended[class_count].has_const = false;
                        class_count += 1;
                        class_count - 1
                    }
                };
                if extended[class].has_const
                    || keys
                        .as_slice()
                        .iter()
                        .any(|&held| usize::from(held & KEY_CLASS) == class)
                {
                    continue;
                }
                let at = keys.as_slice().len();
                if keys.push_class(class as u8 | flags).is_none() {
                    return Ok(None);
                }
                shown[at] = (expression, flags);
            }
            ordered[ordered_count] = (keys, shown);
            ordered_count += 1;
        }
        // `append_pathkeys` of the grouping's keys and an aggregate's,
        // with each key's expression for the aggregate's part.
        let appended = |keys: &Keys, shown: &[(&'a Expr<'a>, u8); super::joins::MAX_KEYS]| {
            let mut all = group_keys;
            let mut tail = [(&Expr::Null, 0u8); super::joins::MAX_KEYS];
            let mut tail_count = 0;
            for (at, &key) in keys.as_slice().iter().enumerate() {
                if all
                    .as_slice()
                    .iter()
                    .any(|&held| held & KEY_CLASS == key & KEY_CLASS)
                {
                    continue;
                }
                if all.push_class(key).is_some() {
                    tail[tail_count] = shown[at];
                    tail_count += 1;
                }
            }
            (all, tail, tail_count)
        };
        let mut unprocessed = (1u32 << ordered_count) - 1;
        let mut best = 0u32;
        let mut best_keys = (Keys::NONE, [(&Expr::Null, 0u8); super::joins::MAX_KEYS], 0);
        while unprocessed.count_ones() > best.count_ones() {
            let mut chosen = 0u32;
            let mut current: Option<PresortKeys<'a>> = None;
            for at in (0..ordered_count).filter(|at| unprocessed & 1 << at != 0) {
                let candidate = appended(&ordered[at].0, &ordered[at].1);
                match current {
                    None => {
                        current = Some(candidate);
                        chosen |= 1 << at;
                    }
                    Some(held) => {
                        // `compare_pathkeys`: one list a prefix of the other.
                        let (old, new) = (held.0.as_slice(), candidate.0.as_slice());
                        if new.starts_with(old) {
                            if new.len() > old.len() {
                                current = Some(candidate);
                            }
                            chosen |= 1 << at;
                        } else if old.starts_with(new) {
                            chosen |= 1 << at;
                        }
                    }
                }
            }
            unprocessed &= !chosen;
            if chosen.count_ones() > best.count_ones() {
                best = chosen;
                best_keys = current.expect("an aggregate chosen");
            }
        }
        if best != 0 && !best_keys.0.as_slice().is_empty() {
            group_keys = best_keys.0;
            presort_tail = best_keys.1;
            presort_count = best_keys.2;
        }
    }
    // The window clauses in evaluation order (`select_active_windows`),
    // each with its pathkeys (`make_pathkeys_for_window`) and costs.
    let mut windows = super::joins::WindowsProblem::NONE;
    let mut window_kept = [([0u8; super::joins::MAX_KEYS], 0u8); super::joins::MAX_WINDOWS];
    // The expression members the window clauses sort by, and the columns
    // only they read, which the windows' input does not carry.
    let mut window_expressions = 0u32;
    let mut key_only_columns = [0u64; MAX_JOIN_RELS];
    if windowed {
        let columns = crate::sql::query::CatalogScopeCols {
            scope,
            outer: None,
            storage,
            txid,
            arena,
        };
        let mut win_nodes = crate::mem::arena::ArenaList::new(arena);
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                crate::sql::query::collect_windows(
                    expression,
                    &mut win_nodes,
                    storage,
                    txid,
                    &columns,
                )?;
            }
        }
        let win_nodes = win_nodes.as_slice();
        let clauses = crate::sql::query::window_clauses(statement, win_nodes, arena)?;
        if clauses.clauses.len() > super::joins::MAX_WINDOWS {
            return Ok(None);
        }
        let operator = Costs::current().cpu_operator;
        for (index, &(spec, _)) in clauses.clauses.iter().enumerate() {
            let window = &mut windows.clauses[index];
            // `numPartCols`, `numOrderCols` and the columns whose groups
            // `estimate_num_groups` counts: each clause's distinct items.
            for (at, &expression) in spec.partition_by.iter().enumerate() {
                if spec.partition_by[..at].contains(&expression) {
                    continue;
                }
                let member = match column_of(scope, expression) {
                    Some((table, column)) => Member::plain(table as u8, column as u16),
                    None if !(grouped || aggregated) => {
                        match expressions.member(scope, expression)? {
                            Some((member, ..)) => member,
                            None => return Ok(None),
                        }
                    }
                    None => return Ok(None),
                };
                if usize::from(window.partition_count) == super::joins::MAX_KEYS {
                    return Ok(None);
                }
                window.partition[usize::from(window.partition_count)] = member;
                window.partition_count += 1;
            }
            for (at, order) in spec.order_by.iter().enumerate() {
                if spec.order_by[..at].contains(order) {
                    continue;
                }
                let member = match column_of(scope, order.expression) {
                    Some((table, column)) => Member::plain(table as u8, column as u16),
                    None if !(grouped || aggregated) => {
                        match expressions.member(scope, order.expression)? {
                            Some((member, ..)) => member,
                            None => return Ok(None),
                        }
                    }
                    None => return Ok(None),
                };
                if usize::from(window.order_count) == super::joins::MAX_KEYS {
                    return Ok(None);
                }
                window.order[usize::from(window.order_count)] = member;
                window.order_count += 1;
            }
            let keys = crate::sql::query::window_sort_keys(spec, arena)?;
            for (at, key) in keys.iter().enumerate() {
                let Some((table, column)) = column_of(scope, key.expression) else {
                    // An expression of one relation's columns: the windows'
                    // input computes it (`make_window_input_target`), and
                    // the query's target list holds it.
                    if grouped || aggregated {
                        return Ok(None);
                    }
                    let Some((member, ..)) = expressions.member(scope, key.expression)? else {
                        return Ok(None);
                    };
                    let id = member.expression_id().expect("an expression member");
                    let member = Member {
                        nulling: member_nulling(member.base_relids()),
                        ..member
                    };
                    if window_expressions & 1 << id == 0 {
                        window_expressions |= 1 << id;
                        let held = expressions.get(id);
                        for &(rel, column) in held.vars.as_slice() {
                            let (rel, column) = (usize::from(rel), usize::from(column));
                            if output[rel] & 1 << column == 0 {
                                key_only_columns[rel] |= 1 << column;
                            }
                            output[rel] |= 1 << column;
                            note(rel, column);
                        }
                    }
                    let class = match extended[..class_count]
                        .iter()
                        .position(|class| class.members().contains(&member))
                    {
                        Some(class) => class,
                        None => {
                            if class_count == extended.len() {
                                return Err(preparation_full());
                            }
                            extended[class_count].members = single_member(member, arena)?;
                            extended[class_count].has_const = false;
                            class_count += 1;
                            class_count - 1
                        }
                    };
                    if extended[class].has_const
                        || window
                            .keys
                            .as_slice()
                            .iter()
                            .any(|&held| usize::from(held & KEY_CLASS) == class)
                    {
                        continue;
                    }
                    let flags = if key.descending {
                        super::joins::KEY_DESCENDING
                    } else {
                        0
                    } | if key.nulls_first {
                        super::joins::KEY_NULLS_FIRST
                    } else {
                        0
                    };
                    let kept = &mut window_kept[index];
                    if usize::from(kept.1) == super::joins::MAX_KEYS {
                        return Ok(None);
                    }
                    kept.0[usize::from(kept.1)] = at as u8;
                    kept.1 += 1;
                    let _ = window.keys.push_class(class as u8 | flags);
                    continue;
                };
                let ctype = scope
                    .defs
                    .stored(table)
                    .map(|def| def.columns()[column].ctype);
                if column >= 64 || !ctype.is_some_and(joinable) {
                    return Ok(None);
                }
                let member = Member {
                    rel: table as u8,
                    column: column as u16,
                    nulling: nulling_relids[table] & !BASE_RELIDS,
                    reads: Relids::EMPTY,
                };
                key_only_columns[table] &= !(1 << column);
                if !statement.items.iter().any(|item| match item {
                    SelectItem::Wildcard(_) => true,
                    SelectItem::TableWildcard(qualifier) => scope.names[table] == *qualifier,
                    SelectItem::Expr { expression, .. } => {
                        column_of(scope, expression) == Some((table, column))
                    }
                    SelectItem::RecordStar(_) => false,
                }) && !junk_columns[..junk_count].contains(&(table, column))
                {
                    if junk_count == junk_columns.len() {
                        return Ok(None);
                    }
                    junk_columns[junk_count] = (table, column);
                    junk_count += 1;
                }
                output[table] |= 1 << column;
                note(table, column);
                let class = match extended[..class_count]
                    .iter()
                    .position(|class| class.members().contains(&member))
                {
                    Some(class) => class,
                    None => {
                        if class_count == extended.len() {
                            return Err(preparation_full());
                        }
                        extended[class_count].members = single_member(member, arena)?;
                        extended[class_count].has_const = false;
                        class_count += 1;
                        class_count - 1
                    }
                };
                // A key of a constant's class or of an earlier key's is
                // redundant.
                if extended[class].has_const
                    || window
                        .keys
                        .as_slice()
                        .iter()
                        .any(|&held| usize::from(held & KEY_CLASS) == class)
                {
                    continue;
                }
                let flags = if key.descending {
                    super::joins::KEY_DESCENDING
                } else {
                    0
                } | if key.nulls_first {
                    super::joins::KEY_NULLS_FIRST
                } else {
                    0
                };
                let kept = &mut window_kept[index];
                if usize::from(kept.1) == super::joins::MAX_KEYS {
                    return Ok(None);
                }
                kept.0[usize::from(kept.1)] = at as u8;
                kept.1 += 1;
                let _ = window.keys.push_class(class as u8 | flags);
            }
            // The frame's end, as the window functions' support functions
            // leave it (`optimize_window_clauses`).
            let functions = || {
                win_nodes
                    .iter()
                    .zip(clauses.of_node)
                    .filter(move |(_, clause)| **clause == index)
                    .map(|(node, _)| *node)
            };
            window.frame_end = if functions().all(crate::sql::query::frame_independent) {
                super::joins::FrameEnd::CurrentRow { rows: true }
            } else {
                match spec.frame {
                    None => super::joins::FrameEnd::CurrentRow { rows: false },
                    Some(frame) => {
                        let rows = frame.units == crate::sql::ast::FrameUnits::Rows;
                        match frame.end {
                            crate::sql::ast::FrameBound::UnboundedFollowing => {
                                super::joins::FrameEnd::UnboundedFollowing
                            }
                            crate::sql::ast::FrameBound::CurrentRow => {
                                super::joins::FrameEnd::CurrentRow { rows }
                            }
                            crate::sql::ast::FrameBound::Preceding(_) => {
                                super::joins::FrameEnd::OffsetPreceding
                            }
                            crate::sql::ast::FrameBound::UnboundedPreceding => {
                                return Ok(None);
                            }
                            crate::sql::ast::FrameBound::Following(offset) => {
                                let offset = match offset {
                                    Expr::Null => Some(1.0),
                                    Expr::Int(value) => Some(*value as f64),
                                    _ => None,
                                };
                                super::joins::FrameEnd::OffsetFollowing { rows, offset }
                            }
                        }
                    }
                }
            };
            // Each distinct function's cost (its `procost` of one, then its
            // arguments' and filter's) and result width.
            for (at, node) in functions().enumerate() {
                if functions().take(at).any(|earlier| *earlier == *node) {
                    continue;
                }
                let Expr::Call { args, filter, .. } = node else {
                    return Ok(None);
                };
                let Some((entry_width, _)) = target_entry(storage, txid, scope, node, arena) else {
                    return Ok(None);
                };
                window.added_width += entry_width;
                window.per_row += operator;
                for &argument in args.iter().chain(filter.iter()) {
                    window.per_row +=
                        super::super::plan::qual_cost(argument, &columns, storage, txid, arena)
                            / 2.0
                            * operator;
                }
            }
        }
        windows.count = clauses.clauses.len() as u8;
    }
    // `query_pathkeys`: the grouping's, else the first window's, else the
    // longer of the `DISTINCT`'s and the `ORDER BY`'s.
    let query_keys = if windowed {
        windows.clauses[0].keys
    } else if grouped
        || (aggregated && !group_keys.as_slice().is_empty())
        || (distinct && group_keys.as_slice().len() > sort_keys.as_slice().len())
    {
        group_keys
    } else {
        sort_keys
    };
    let classes: &[EquivalenceClass] = arena
        .alloc_slice_copy(&extended[..class_count])
        .map_err(|_| crate::sql::eval::arena_full())?;
    // The columns each other condition of two relations compares, each
    // nulled by the outer joins the clause reads it above.
    for clause in &mut clauses
        .as_mut_slice()
        .map_err(|_| crate::sql::eval::arena_full())?[..placed_join_count]
    {
        if clause.form != ClauseForm::Other {
            continue;
        }
        if let Expr::Binary {
            operator:
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq,
            left,
            right,
            ..
        } = quals[usize::from(clause.source.expect("written"))]
            && let (Some(one), Some(two)) = (column_of(scope, left), column_of(scope, right))
            && one.0 != two.0
            && one.1 < 64
            && two.1 < 64
        {
            let member = |(table, column): (usize, usize)| Member {
                rel: table as u8,
                column: column as u16,
                nulling: clause.relids & nulling_relids[table] & !BASE_RELIDS,
                reads: Relids::EMPTY,
            };
            clause.operands = Some((member(one), member(two)));
        }
    }
    // Each relation's join clauses an index may apply: the outer joins'
    // clauses movable into it (`join_clause_is_movable_to`), and its
    // column of each class equated with every other member
    // (`generate_implied_equalities_for_column`).
    let mut scans = [JoinScan {
        params: [(0, 0); MAX_JOIN_PARAMS],
        param_count: 0,
        clause_params: 0,
        param_others: [Relids::EMPTY; MAX_JOIN_PARAMS],
        classes: [NO_CLASS; 64],
        merging: 0,
        order: Keys::NONE,
        rel_rows: [0.0; MAX_JOIN_RELS],
        total_table_pages: 0.0,
        describe: None,
    }; MAX_JOIN_RELS];
    let mut join_conjuncts: [[&'a Expr<'a>; MAX_JOIN_PARAMS]; MAX_JOIN_RELS] =
        [[&Expr::Null; MAX_JOIN_PARAMS]; MAX_JOIN_RELS];
    // The clause each parameter is: an outer join's clause, or `None` for
    // a class's.
    let mut param_clause = [[None::<ClauseId>; MAX_JOIN_PARAMS]; MAX_JOIN_RELS];
    let mut total_table_pages = 0.0;
    let member_class = |member: Member| {
        classes
            .iter()
            .position(|class| class.members().contains(&member))
    };
    // Each expression member's variable among its relation's index key
    // expressions, by which an index on the expression orders and
    // parameterizes the relation's scans.
    let mut expression_keys = [None::<u16>; MAX_JOIN_EXPRESSIONS];
    for table in 0..n {
        if !(0..expressions.count).any(|id| expressions.get(id).of(table)) {
            continue;
        }
        let slot = scope.slots[table];
        let Some(catalog) = super::paths::catalog_indexes(storage, txid, slot, arena)? else {
            return Ok(None);
        };
        let (keyed, _) = super::expressions::table_expressions(
            storage,
            txid,
            slot,
            scope.defs.stored(table).expect("plain table"),
            &storage.table_statistics(slot, txid),
            catalog,
            arena,
        )?;
        for (id, key) in expression_keys
            .iter_mut()
            .enumerate()
            .take(expressions.count)
        {
            let held = expressions.get(id);
            if !held.of(table) {
                continue;
            }
            *key = keyed
                .variables
                .iter()
                .position(|variable| *variable.canonical == *held.canonical)
                .and_then(|at| u16::try_from(keyed.first + at).ok());
        }
    }
    for table in 0..n {
        if removed & Relids::of(table) == Relids::EMPTY {
            total_table_pages += super::relation_size(storage, scope.slots[table], txid).pages;
        }
        let scan = &mut scans[table];
        scan.order = query_keys;
        let relids: Relids = Relids::of(table);
        // `pathkeys_useful_for_merging` at the relation: a class with a
        // member elsewhere, or one a mergeable clause of its `joininfo`
        // sorts by.
        let eclass_joins = classes.iter().any(|class| {
            class.count() + usize::from(class.has_const) > 1
                && class.relids() & relids != Relids::EMPTY
                && class.relids() & !relids != Relids::EMPTY
        });
        // Each column, then each expression member an index keys by its
        // variable (`match_index_to_operand`).
        let keyed_expressions = (0..expressions.count).filter_map(|id| {
            let column = usize::from(expression_keys[id]?);
            (expressions.get(id).of(table) && column < 64)
                .then(|| (column, Member::expression(table as u8, id as u16)))
        });
        let members = (0..width(table))
            .map(|column| (column, Member::plain(table as u8, column as u16)))
            .chain(keyed_expressions);
        for (column, member) in members {
            let slot = &mut scan.classes[column];
            let Some(class) = member_class(member) else {
                continue;
            };
            let held = &classes[class];
            let useful_by_class = eclass_joins
                && !held.has_const
                && held.count() > 1
                && held.relids() & !relids != Relids::EMPTY
                && held
                    .members()
                    .iter()
                    .any(|member| member.relids() & relids == Relids::EMPTY);
            let useful_by_clause = clauses.borrowed_slice()[..placed_join_count].iter().any(
                |clause| {
                    clause.in_joininfo_of(relids)
                        && matches!(clause.form, ClauseForm::Join { left_class, right_class, .. }
                        if usize::from(left_class) == class || usize::from(right_class) == class)
                },
            );
            if useful_by_class || useful_by_clause {
                *slot = class as u8;
                scan.merging |= 1 << class;
            } else if query_keys
                .as_slice()
                .iter()
                .any(|&key| usize::from(key & KEY_CLASS) == class)
            {
                *slot = class as u8;
            }
        }
        // The columns its btree indexes key.
        let mut keyed = 0u64;
        let mut btree = false;
        crate::sql::catalog::planner_indexes(storage, txid, scope.slots[table], &mut |index| {
            if index.method == crate::sql::ast::IndexAccessMethod::Btree {
                btree = true;
                for position in 0..index.n_cols {
                    if !index.expression_keys[position] && index.columns[position] < 64 {
                        keyed |= 1 << index.columns[position];
                    }
                }
            }
        });
        for (id, clause) in clauses.borrowed_slice()[..placed_join_count]
            .iter()
            .enumerate()
        {
            if !clause.in_joininfo_of(relids)
                || clause.relids & relids == Relids::EMPTY
                || clause.outer_relids & relids != Relids::EMPTY
                || clause.relids & nulling_relids[table] != Relids::EMPTY
                || clause.clone == super::joins::CloneKind::Is
            {
                continue;
            }
            // Without an index the relation has no parameterized path; with
            // one, each path parameterized by the clause's other relations
            // filters by it unless an index applies it
            // (`get_baserel_parampathinfo`).
            if keyed == 0 {
                continue;
            }
            let expression = quals[usize::from(clause.source.expect("written"))];
            // An equality's sides sort by their own classes; another
            // comparison of two columns has none.
            let (own_class, other_rel) = match clause.form {
                ClauseForm::Join {
                    left,
                    right,
                    left_class,
                    right_class,
                } => {
                    if usize::from(left.rel) == table {
                        (left_class, right.rel)
                    } else {
                        (right_class, left.rel)
                    }
                }
                _ => {
                    let Expr::Binary { left, right, .. } = expression else {
                        return Ok(None);
                    };
                    match (column_of(scope, left), column_of(scope, right)) {
                        (Some((one, _)), Some((two, _))) if one == table && two != table => {
                            (NO_CLASS, two as u8)
                        }
                        (Some((one, _)), Some((two, _))) if two == table && one != table => {
                            (NO_CLASS, one as u8)
                        }
                        _ => {
                            return Ok(None);
                        }
                    }
                }
            };
            let at = usize::from(scan.param_count);
            if at == MAX_JOIN_PARAMS {
                return Err(join_params_full());
            }
            scan.params[at] = (own_class, other_rel);
            scan.param_others[at] = clause.relids & !BASE_RELIDS;
            scan.clause_params |= 1 << at;
            join_conjuncts[table][at] = expression;
            param_clause[table][at] = Some(id as ClauseId);
            scan.param_count += 1;
        }
        for (class_id, class) in classes.iter().enumerate() {
            // Without a btree index no path is parameterized by a class.
            if !btree {
                break;
            }
            let Some(own) = class.members().iter().find(|member| {
                usize::from(member.rel) == table
                    && member.nulling == Relids::EMPTY
                    && member.reads == Relids::EMPTY
            }) else {
                continue;
            };
            // An expression no index keys parameterizes no scan.
            if let Some(id) = own.expression_id()
                && expression_keys[id].is_none()
            {
                continue;
            }
            if class.has_const || class.count() <= 1 {
                continue;
            }
            for other in class.members() {
                // A member reading the relation is no value from outside it
                // (`generate_implied_equalities_for_column`); an expression
                // of several relations parameterizes the scan by them all.
                if other.base_relids() & relids != Relids::EMPTY {
                    continue;
                }
                let at = usize::from(scan.param_count);
                if at == MAX_JOIN_PARAMS {
                    return Err(join_params_full());
                }
                scan.params[at] = (class_id as u8, other.rel);
                scan.param_others[at] = other.nulling | other.reads;
                join_conjuncts[table][at] = equality(scope, &expressions, *own, *other, arena)?;
                scan.param_count += 1;
            }
        }
    }
    for scan in &mut scans[..n] {
        scan.total_table_pages = total_table_pages;
    }
    // The columns above each relation's scan: the output's and those a join
    // or outer join reads (`attr_needed`).
    let mut join_columns = [0u64; MAX_JOIN_RELS];
    for &((rel, column), relids, _) in prepared.needed.borrowed_slice() {
        if relids != Relids::EMPTY && column < 64 {
            join_columns[usize::from(rel)] |= 1 << column;
            note(usize::from(rel), usize::from(column));
        }
    }
    // Each relation's paths (`set_base_rel_sizes`, then
    // `set_base_rel_pathlists`, whose parameterized paths read the other
    // relations' rows).
    let mut rels = [None::<ModeledRel<'a>>; MAX_JOIN_RELS];
    let mut planned_paths = [None::<super::paths::JoinRelPaths<'a>>; MAX_JOIN_RELS];
    let mut needed = crate::mem::arena::ArenaList::<NeededColumn>::new(arena);
    let mut rel_rows = [0.0f64; MAX_JOIN_RELS];
    // The indexes each relation's first sizing found, which the
    // extracted restrictions' estimates read.
    let mut sized_indexes = [&[][..]; MAX_JOIN_RELS];
    // Each join `OR` clause's selectivity divisors, one per restriction
    // extracted from it (`consider_new_or_clause`).
    let mut or_divisors = crate::mem::arena::ArenaList::<(usize, f64)>::new(arena);
    // Sizing, sizing again with the restrictions extracted from join `OR`
    // clauses, then the paths.
    for pass in 0..3 {
        if pass == 1 {
            // `extract_restriction_or_clauses`.
            let mut extracted = false;
            for table in 0..n {
                let relids: Relids = Relids::of(table);
                for (id, clause) in clauses.borrowed_slice()[..placed_join_count]
                    .iter()
                    .enumerate()
                {
                    if clause.form != ClauseForm::Other
                        || !clause.in_joininfo_of(relids)
                        || clause.relids & relids == Relids::EMPTY
                        || clause.outer_relids & relids != Relids::EMPTY
                        || clause.relids & nulling_relids[table] != Relids::EMPTY
                        || clause.clone == super::joins::CloneKind::Is
                    {
                        continue;
                    }
                    let expression = quals[usize::from(clause.source.expect("written"))];
                    if !matches!(
                        expression,
                        Expr::Binary {
                            operator: BinaryOp::Or,
                            ..
                        }
                    ) {
                        continue;
                    }
                    let Some(restriction) = extract_or_clause(scope, expression, table, arena)?
                    else {
                        continue;
                    };
                    let rel = rels[table].expect("sized");
                    let single = QueryScope::resolve_schema(storage, rel.from, txid, arena, None)?;
                    let statistics = storage.table_statistics(scope.slots[table], txid);
                    let definition = scope.defs.stored(table).expect("plain table");
                    let planner = Planner {
                        variables: super::ExpressionVariables {
                            first: definition.columns().len(),
                            variables: &[],
                        },
                        storage,
                        txid,
                        slot: scope.slots[table],
                        definition,
                        statistics: &statistics,
                        statistics_key: 0,
                        size: super::relation_size(storage, scope.slots[table], txid),
                        costs,
                        indexes: sized_indexes[table],
                        extended: &[],
                        arena,
                    };
                    let Some((or_selectivity, _)) = super::paths::restriction_estimate(
                        &planner,
                        &single,
                        restriction,
                        params,
                        arena,
                    )?
                    else {
                        return Ok(None);
                    };
                    if or_selectivity > 0.9 {
                        continue;
                    }
                    restrictions[table] = Some(match restrictions[table] {
                        None => restriction,
                        Some(held) => and(held, restriction, arena)?,
                    });
                    if or_selectivity > 0.0 {
                        or_divisors
                            .push((id, or_selectivity))
                            .map_err(|_| crate::sql::eval::arena_full())?;
                    }
                    extracted = true;
                }
            }
            if !extracted {
                continue;
            }
        }
        for table in 0..n {
            let single = match rels[table] {
                Some(rel) => rel.from,
                None => arena
                    .alloc(FromClause {
                        base: *source(from, table),
                        joins: &[],
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?,
            };
            let single_scope = QueryScope::resolve_schema(storage, single, txid, arena, None)?;
            let definition = scope.defs.stored(table).expect("plain table");
            let columns = definition.columns().len();
            let all = if columns >= 64 {
                u64::MAX
            } else {
                (1u64 << columns) - 1
            };
            let above = (output[table] | join_columns[table]) & all;
            let scan = JoinScan {
                rel_rows,
                ..scans[table]
            };
            let joins = arena
                .alloc_slice_copy(&join_conjuncts[table][..usize::from(scan.param_count)])
                .map_err(|_| crate::sql::eval::arena_full())?;
            let read = above | restriction_columns(&single_scope, restrictions[table]);
            let condition = joined_condition(restrictions[table], joins, arena)?;
            let planned = super::scan_path(
                storage,
                &single_scope,
                txid,
                condition,
                Some(ColumnSet::from_low_word(read)),
                params,
                arena,
                QueryShape {
                    output: Some(ColumnSet::from_low_word(above)),
                    member: super::Member::Join(scan),
                    ..QueryShape::default()
                },
            )?;
            let Planned::Join(paths) = planned else {
                return Ok(None);
            };
            if pass < 2 {
                rel_rows[table] = paths.rows;
                sized_indexes[table] = paths.indexes;
                rels[table] = Some(ModeledRel {
                    from: single,
                    restriction: restrictions[table],
                    joins,
                    scan,
                    read,
                    output: above,
                    paths: &[],
                });
                continue;
            }
            planned_paths[table] = Some(paths);
            let slot = scope.slots[table];
            for column in appearance[table][..appeared[table]]
                .iter()
                .map(|&column| usize::from(column))
                .filter(|&column| above & 1 << column != 0)
            {
                needed
                    .push(NeededColumn {
                        rel: table as u8,
                        column: column as u16,
                        width: if prepared.false_relids == Relids::of(table) {
                            0.0
                        } else {
                            f64::from(super::column_width(storage, txid, slot, column))
                        },
                        needed_by: prepared.needed_by((table as u8, column as u16))
                            & !Relids::of(table),
                        output: output[table] & 1 << column != 0,
                    })
                    .map_err(|_| crate::sql::eval::arena_full())?;
            }
            rels[table] = Some(ModeledRel {
                scan,
                paths: paths.candidates,
                ..rels[table].expect("sized")
            });
        }
    }
    // Each expression member's width, which a `Memoize` keyed by it
    // counts (`get_expr_width`).
    for id in 0..expressions.count {
        let expression = expressions.get(id);
        needed
            .push(NeededColumn {
                rel: expression.rel,
                column: Member::expression(expression.rel, id as u16).column,
                width: expression.width,
                needed_by: Relids::EMPTY,
                output: false,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
    }
    // The join clauses the base relations' index paths derive, in the
    // order and orientation `create_index_paths` creates them: each index's
    // key columns equated with the other members of their class, then the
    // clauses its parameterized paths move into the scan
    // (`get_baserel_parampathinfo`).
    let mut derive = |class: u8, left: u8, right: u8| -> Result<ClauseId, SqlError> {
        let found = clauses.borrowed_slice().iter().position(|clause| {
            clause.source.is_none()
                && matches!(clause.form, ClauseForm::Equality { class: c, left: l, right: r }
                    if c == class && ((l, r) == (left, right) || (l, r) == (right, left)))
        });
        if let Some(at) = found {
            return Ok(at as ClauseId);
        }
        if clauses.len() == MAX_CLAUSES {
            return Err(super::joins::too_many_clauses());
        }
        let members = classes[usize::from(class)].members();
        let relids = members[usize::from(left)].relids() | members[usize::from(right)].relids();
        let member_cost = |member: Member| {
            member
                .expression_id()
                .map_or(0.0, |id| expressions.get(id).cost)
        };
        clauses
            .push(Clause {
                form: ClauseForm::Equality { class, left, right },
                relids,
                required: relids,
                pushed_down: true,
                clone: super::joins::CloneKind::None,
                incompatible: Relids::EMPTY,
                cost: costs.cpu_operator
                    + member_cost(members[usize::from(left)])
                    + member_cost(members[usize::from(right)]),
                selectivity: None,
                outer_selectivity: None,
                outer_relids: Relids::EMPTY,
                operands: None,
                other: super::joins::OtherShape::Plain,
                constant: false,
                source: None,
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        Ok((clauses.len() - 1) as ClauseId)
    };
    // `reduce_unique_semijoins` runs before the base relations' paths: for
    // a semi join to one relation with a unique index
    // (`rel_supports_distinctness`), it derives each class's clause from
    // the first member of the join's left side to the first of the inner
    // relation, so those clauses take that orientation.
    for special in &prepared.specials[..prepared.special_count] {
        let inner = special.min_right;
        if special.kind != super::joins::SpecialKind::Semi
            || inner.count_ones() != 1
            || inner & !BASE_RELIDS != Relids::EMPTY
        {
            continue;
        }
        let Some(paths) = planned_paths[inner.trailing_zeros() as usize] else {
            continue;
        };
        if !paths
            .indexes
            .iter()
            .any(|index| index.catalog.unique && index.catalog.immediate && !index.catalog.partial)
        {
            continue;
        }
        let joined = special.min_left | inner;
        for class in 0..class_count as u8 {
            let held = &classes[usize::from(class)];
            if held.has_const || held.count() <= 1 {
                continue;
            }
            let first = |side: Relids| {
                held.members().iter().position(|member| {
                    member.relids() & !joined == Relids::EMPTY
                        && member.relids() & !side == Relids::EMPTY
                })
            };
            if let (Some(outer), Some(within)) = (first(special.min_left), first(inner)) {
                derive(class, outer as u8, within as u8)?;
            }
        }
    }
    let mut base_paths = [&[][..]; MAX_JOIN_RELS];
    let mut base = [BaseRel {
        rows: 0.0,
        width: 0.0,
        paths: &[],
        unique: &[],
        fixed: 0,
    }; MAX_JOIN_RELS];
    for table in 0..n {
        let rel = rels[table].expect("planned");
        let paths = planned_paths[table].expect("planned");
        // A dummy relation has no paths and derives no clauses.
        if prepared.false_relids == Relids::of(table) {
            base[table] = BaseRel {
                rows: 0.0,
                width: 0.0,
                paths: &[],
                unique: &[],
                fixed: paths.fixed,
            };
            continue;
        }
        // The relation's member of a class: the one of `column`, else its
        // first.
        let member_of = |class: u8, column: Option<usize>| {
            let members = classes[usize::from(class)].members();
            column
                .and_then(|column| {
                    members.iter().position(|member| {
                        member.relids() == Relids::of(table)
                            && match member.expression_id() {
                                Some(id) => expression_keys[id] == Some(column as u16),
                                None => usize::from(member.column) == column,
                            }
                    })
                })
                .or_else(|| {
                    members
                        .iter()
                        .position(|member| member.relids() == Relids::of(table))
                })
        };
        // The clauses a path parameterized by `param` moves into the scan:
        // the outer joins' movable clauses, then one per class.
        let moved = |param: Relids,
                     derive: &mut dyn FnMut(u8, u8, u8) -> Result<ClauseId, SqlError>|
         -> Result<Option<ClauseList>, SqlError> {
            let mut list = ClauseList::EMPTY;
            let params = &rel.scan.params[..usize::from(rel.scan.param_count)];
            for (at, (&(_, other), clause)) in params.iter().zip(param_clause[table]).enumerate() {
                if let Some(id) = clause
                    && (Relids::of(other) | rel.scan.param_others[at]) & !param == Relids::EMPTY
                {
                    list.push_id(id)
                        .ok_or_else(super::joins::too_many_clauses)?;
                }
            }
            for class in 0..class_count as u8 {
                let held = &classes[usize::from(class)];
                if held.has_const || held.count() <= 1 {
                    continue;
                }
                let Some(own) = member_of(class, None) else {
                    continue;
                };
                // A scan parameterized by other clauses would filter by an
                // expression no index keys, which the model's scans leave.
                if held.members()[own]
                    .expression_id()
                    .is_some_and(|id| expression_keys[id].is_none())
                    && held
                        .members()
                        .iter()
                        .any(|member| member.relids() & !param == Relids::EMPTY)
                {
                    return Ok(None);
                }
                let Some(outer) = held
                    .members()
                    .iter()
                    .position(|member| member.relids() & !param == Relids::EMPTY)
                else {
                    continue;
                };
                list.push_id(derive(class, outer as u8, own as u8)?)
                    .ok_or_else(super::joins::too_many_clauses)?;
            }
            Ok(Some(list))
        };
        let mut enforced = [ClauseList::EMPTY; 128];
        for (index_at, index) in paths.indexes.iter().enumerate() {
            let info = &index.catalog;
            if info.method != crate::sql::ast::IndexAccessMethod::Btree || info.partial {
                continue;
            }
            for position in 0..info.n_cols {
                if info.pattern_keys[position] || !info.column_collation[position] {
                    continue;
                }
                // An expression key is its variable, whose class an
                // expression member gave it.
                let column = usize::from(info.columns[position]);
                let class = rel.scan.classes.get(column).copied().unwrap_or(NO_CLASS);
                if class == NO_CLASS {
                    continue;
                }
                let held = &classes[usize::from(class)];
                if held.has_const || held.count() <= 1 {
                    continue;
                }
                let own = member_of(class, Some(column)).expect("a class of the relation") as u8;
                for (other, member) in held.members().iter().enumerate() {
                    if usize::from(member.rel) != table {
                        derive(class, own, other as u8)?;
                    }
                }
            }
            for (at, candidate) in paths.candidates.iter().enumerate() {
                if usize::from(candidate.describe.index) == index_at && !candidate.param.is_empty()
                {
                    let Some(list) = moved(candidate.param, &mut derive)? else {
                        return Ok(None);
                    };
                    enforced[at] = list;
                }
            }
        }
        for (at, candidate) in paths.candidates.iter().enumerate() {
            if !candidate.param.is_empty() && candidate.describe.index == u16::MAX {
                let Some(list) = moved(candidate.param, &mut derive)? else {
                    return Ok(None);
                };
                enforced[at] = list;
            }
        }
        base_paths[table] = arena
            .alloc_slice_with(paths.candidates.len(), |at| {
                let candidate = paths.candidates[at];
                BasePath {
                    startup: candidate.candidate.startup,
                    total: candidate.candidate.total,
                    rows: candidate.rows,
                    disabled: candidate.candidate.disabled,
                    keys: Keys::of(&candidate.keys[..usize::from(candidate.key_count)]),
                    param: candidate.param,
                    enforced: enforced[at],
                    mark_restore: matches!(
                        candidate.candidate.kind,
                        super::ScanKind::Index | super::ScanKind::IndexOnly
                    ),
                    indexed_join_quals: candidate.indexed_join_quals,
                }
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        // `rel_supports_distinctness`: the unique, immediately enforced
        // whole-table indexes whose keys a join's clauses can match.
        let mut unique = [0u64; 32];
        let mut unique_count = 0;
        for index in paths.indexes {
            let info = &index.catalog;
            if !info.unique
                || !info.immediate
                || info.partial
                || info.method != crate::sql::ast::IndexAccessMethod::Btree
                || unique_count == unique.len()
            {
                continue;
            }
            let keys = &info.columns[..info.n_cols];
            if (0..info.n_cols).any(|position| {
                info.expression_keys[position]
                    || info.pattern_keys[position]
                    || usize::from(info.columns[position]) >= 64
            }) {
                continue;
            }
            unique[unique_count] = keys.iter().fold(0, |mask, &column| mask | 1 << column);
            unique_count += 1;
        }
        base[table] = BaseRel {
            rows: paths.rows,
            width: paths.width,
            paths: base_paths[table],
            unique: arena
                .alloc_slice_copy(&unique[..unique_count])
                .map_err(|_| crate::sql::eval::arena_full())?,
            fixed: paths.fixed,
        };
    }
    // Each relation's class clauses as the derived clauses orient them,
    // which a scan's `Recheck Cond` and `Filter` print.
    for (table, slot) in rels.iter_mut().enumerate().take(n) {
        let rel = slot.expect("planned");
        let mut joins = [&Expr::Null; MAX_JOIN_PARAMS];
        for (at, &(class, other_rel)) in rel.scan.params[..usize::from(rel.scan.param_count)]
            .iter()
            .enumerate()
        {
            if param_clause[table][at].is_some() {
                joins[at] = rel.joins[at];
                continue;
            }
            let members = classes[usize::from(class)].members();
            let own = members
                .iter()
                .position(|member| {
                    usize::from(member.rel) == table && member.reads == Relids::EMPTY
                })
                .expect("a member of the relation") as u8;
            let reads = rel.scan.param_others[at] & BASE_RELIDS;
            let other = members
                .iter()
                .position(|member| member.rel == other_rel && member.reads == reads)
                .expect("a member of the other relations") as u8;
            let reversed = clauses.borrowed_slice().iter().any(|clause| {
                clause.source.is_none()
                    && clause.form
                        == ClauseForm::Equality {
                            class,
                            left: other,
                            right: own,
                        }
            });
            joins[at] = if reversed {
                equality(
                    scope,
                    &expressions,
                    members[usize::from(other)],
                    members[usize::from(own)],
                    arena,
                )?
            } else {
                rel.joins[at]
            };
        }
        *slot = Some(ModeledRel {
            joins: arena
                .alloc_slice_copy(&joins[..usize::from(rel.scan.param_count)])
                .map_err(|_| crate::sql::eval::arena_full())?,
            ..rel
        });
    }
    // The statistics the selectivities read.
    let statistics: [Option<TableStatistics>; MAX_JOIN_RELS] = core::array::from_fn(|table| {
        (table < n).then(|| storage.table_statistics(scope.slots[table], txid))
    });
    // Each relation's variables: its indexes' key expressions
    // (`examine_variable` reads their statistics), then its expression
    // members that no key expression is.
    let mut variables = [None::<super::ExpressionVariables<'a>>; MAX_JOIN_RELS];
    let mut expression_columns = [0usize; MAX_JOIN_EXPRESSIONS];
    let mut expression_vars = [VarList::default(); MAX_JOIN_EXPRESSIONS];
    // An expression of several relations has no variable of its own:
    // `estimate_num_groups` counts the columns it reads.
    for (id, vars) in expression_vars
        .iter_mut()
        .enumerate()
        .take(expressions.count)
    {
        if expressions.get(id).reads != Relids::EMPTY {
            *vars = expressions.get(id).vars;
        }
    }
    for table in 0..n {
        let ids = (0..expressions.count).filter(|&id| expressions.get(id).of(table));
        if ids.clone().next().is_none() {
            continue;
        }
        let definition = scope.defs.stored(table).expect("plain table");
        let indexes = planned_paths[table].map_or(&[][..], |paths| paths.indexes);
        let catalog = arena
            .alloc_slice_with(indexes.len(), |at| indexes[at].catalog)
            .map_err(|_| crate::sql::eval::arena_full())?;
        let (keyed, _) = super::expressions::table_expressions(
            storage,
            txid,
            scope.slots[table],
            definition,
            statistics[table].as_ref().expect("a relation's statistics"),
            catalog,
            arena,
        )?;
        let mut held = [None::<super::expressions::ExpressionVariable<'a>>;
            super::expressions::MAX_EXPRESSION_VARIABLES + MAX_JOIN_EXPRESSIONS];
        for (slot, variable) in held.iter_mut().zip(keyed.variables) {
            *slot = Some(*variable);
        }
        let mut count = keyed.variables.len();
        for id in ids {
            let expression = expressions.get(id);
            expression_vars[id] = expression.vars;
            let number = match keyed
                .variables
                .iter()
                .position(|variable| *variable.canonical == *expression.canonical)
            {
                // An index's key expression, whose statistics ANALYZE
                // gathered (`examine_variable`).
                Some(number) => number,
                None => {
                    held[count] = Some(super::expressions::ExpressionVariable {
                        canonical: expression.canonical,
                        bound: expression.expression,
                        ctype: expression.ctype,
                        collation: expression.collation,
                        statistics: None,
                        extended: None,
                        unique: false,
                        indexed: false,
                    });
                    count += 1;
                    count - 1
                }
            };
            expression_columns[id] = keyed.first + number;
        }
        variables[table] = Some(super::ExpressionVariables {
            first: keyed.first,
            variables: arena
                .alloc_slice_with(count, |at| held[at].expect("dense variables"))
                .map_err(|_| crate::sql::eval::arena_full())?,
        });
    }
    let planners: [Option<Planner<'_, 'a>>; MAX_JOIN_RELS] = core::array::from_fn(|table| {
        let statistics = statistics[table].as_ref()?;
        let slot = scope.slots[table];
        let definition = scope.defs.stored(table)?;
        Some(Planner {
            variables: variables[table].unwrap_or(super::ExpressionVariables {
                first: definition.columns().len(),
                variables: &[],
            }),
            storage,
            txid,
            slot,
            definition,
            statistics,
            statistics_key: 0,
            size: super::relation_size(storage, slot, txid),
            costs,
            indexes: planned_paths[table].map_or(&[][..], |paths| paths.indexes),
            extended: &[],
            arena,
        })
    });
    // The conditions other than equalities evaluated at joins: one
    // relation's (`clause_selectivity` as a restriction, its evaluation
    // cost) or several relations' (`join_estimate`).
    let early_reader = RelStatistics {
        planners: &planners,
        rels: &base,
        expression_columns: &expression_columns,
        expression_vars: &expression_vars,
    };
    let restriction =
        |table: usize, expression: &'a Expr<'a>| -> Result<Option<(f64, f64)>, SqlError> {
            let rel = rels[table].expect("planned");
            let single = QueryScope::resolve_schema(storage, rel.from, txid, arena, None)?;
            let planner = planners[table].as_ref().expect("planned");
            super::paths::restriction_estimate(planner, &single, expression, params, arena)
        };
    // `cost_qual_eval` of a condition over several relations.
    let scope_columns = crate::sql::query::CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    };
    let expression_cost = |expression: &'a Expr<'a>| {
        super::super::plan::qual_cost(expression, &scope_columns, storage, txid, arena) / 2.0
            * costs.cpu_operator
    };
    let mut single_rel_columns = [0u64; MAX_JOIN_RELS];
    for (id, clause) in clauses
        .as_mut_slice()
        .map_err(|_| crate::sql::eval::arena_full())?[..placed_join_count]
        .iter_mut()
        .enumerate()
    {
        if clause.form != ClauseForm::Other {
            continue;
        }
        let expression = quals[usize::from(clause.source.expect("written"))];
        let base_relids = clause.relids & BASE_RELIDS;
        let (selectivity, cost) = if base_relids.count_ones() == 1 {
            let table = base_relids.trailing_zeros() as usize;
            // Two conditions of one column at a join pair their range
            // bounds (`addRangeClause`), which the model leaves; a copy of a
            // commuting outer join's condition is the same condition.
            if clause.clone != super::joins::CloneKind::Is {
                let mut columns = 0u64;
                expression.for_each_column_reference(&mut |qualifier, name| {
                    if let Ok(ResolvedColumn::Table(_, column)) = scope.find_column(qualifier, name)
                        && column < 64
                    {
                        columns |= 1 << column;
                    }
                });
                if single_rel_columns[table] & columns != 0 {
                    return Ok(None);
                }
                single_rel_columns[table] |= columns;
            }
            match restriction(table, expression)? {
                Some(estimate) => estimate,
                None => {
                    return Ok(None);
                }
            }
        } else {
            match join_estimate(
                scope,
                expression,
                &early_reader,
                &restriction,
                &expression_cost,
                costs,
                arena,
            )? {
                Some(estimate) => estimate,
                None => {
                    return Ok(None);
                }
            }
        };
        clause.outer_selectivity = Some(selectivity);
        // The restrictions extracted from it divide its selectivity
        // (`norm_selec`), each quotient clamped to one.
        clause.selectivity = Some(
            or_divisors
                .borrowed_slice()
                .iter()
                .filter(|&&(clause, _)| clause == id)
                .fold(selectivity, |held, &(_, divisor)| (held / divisor).min(1.0)),
        );
        clause.cost = cost;
        clause.other = match expression {
            // An `OR` whose every condition reads one relation is estimated
            // alike for every join: each is a restriction's selectivity.
            Expr::Binary {
                operator: BinaryOp::Or,
                ..
            } if !or_reads_one_relation_each(scope, expression) => {
                super::joins::OtherShape::Composite
            }
            Expr::Binary { left, right, .. } if join_inequality(scope, expression) => {
                let nulls = |side: &Expr<'_>| {
                    column_of(scope, side).map_or(0.0, |(table, column)| {
                        let variable = planners[table].as_ref().expect("planned").variable(column);
                        if variable.analyzed {
                            variable.null_fraction
                        } else {
                            0.0
                        }
                    })
                };
                super::joins::OtherShape::Inequality {
                    left_nulls: nulls(left),
                    right_nulls: nulls(right),
                }
            }
            _ => super::joins::OtherShape::Plain,
        };
        // A semi or anti join estimates these its own way, once.
        if clause.other != super::joins::OtherShape::Plain {
            clause.outer_selectivity = None;
        }
    }
    // The foreign keys between the query's relations whose every column
    // pair a class or join clauses equate (`get_relation_foreign_keys`,
    // then `match_foreign_keys_to_quals`), by referencing relation.
    let mut foreign_keys = [ForeignKeyJoin {
        referencing: 0,
        referenced: 0,
        classes: [0; super::joins::MAX_KEYS],
        count: 0,
        clauses: ClauseList::EMPTY,
        referenced_tuples: 1.0,
        referenced_rows: 1.0,
    }; 32];
    let mut foreign_count = 0;
    for referencing in 0..n {
        let definition = scope.defs.stored(referencing).expect("plain table");
        for key in definition.fkeys() {
            if key.validation == crate::storage::ConstraintValidation::NotEnforced {
                continue;
            }
            for referenced in (0..n).filter(|&other| other != referencing) {
                let parent = scope.defs.stored(referenced).expect("plain table");
                if parent.name.as_str() != key.parent.as_str()
                    || parent.schema.as_str() != key.parent_schema.as_str()
                {
                    continue;
                }
                if key.n_cols > super::joins::MAX_KEYS || foreign_count == foreign_keys.len() {
                    return Ok(None);
                }
                let mut matched = [0u8; super::joins::MAX_KEYS];
                let mut matched_clauses = ClauseList::EMPTY;
                let mut all = true;
                let pairs = key.columns[..key.n_cols]
                    .iter()
                    .zip(&key.parent_cols[..key.n_cols]);
                for (held, (&column, &parent_column)) in matched.iter_mut().zip(pairs) {
                    let own = Member::plain(referencing as u8, column);
                    let other = Member::plain(referenced as u8, parent_column);
                    match classes.iter().position(|class| {
                        class.members().contains(&own) && class.members().contains(&other)
                    }) {
                        // `nconst_ec`: a constant's class the model leaves out.
                        Some(class) if classes[class].has_const => {
                            return Ok(None);
                        }
                        Some(class) => *held = class as u8,
                        None => {
                            // `rinfos`: the referencing relation's join clauses
                            // equating the pair, every copy.
                            *held = NO_KEY_CLASS;
                            let mut found = false;
                            for (id, clause) in clauses.borrowed_slice()[..placed_join_count]
                                .iter()
                                .enumerate()
                            {
                                let ClauseForm::Join { left, right, .. } = clause.form else {
                                    continue;
                                };
                                let pair = |a: Member, b: Member| {
                                    (a.rel, a.column) == (own.rel, own.column)
                                        && (b.rel, b.column) == (other.rel, other.column)
                                };
                                if clause.in_joininfo_of(Relids::of(referencing))
                                    && (pair(left, right) || pair(right, left))
                                {
                                    if matched_clauses.push_id(id as ClauseId).is_none() {
                                        return Err(super::joins::too_many_clauses());
                                    }
                                    found = true;
                                }
                            }
                            all &= found;
                        }
                    }
                }
                if !all {
                    continue;
                }
                let size = super::relation_size(storage, scope.slots[referenced], txid);
                foreign_keys[foreign_count] = ForeignKeyJoin {
                    referencing: referencing as u8,
                    referenced: referenced as u8,
                    classes: matched,
                    count: key.n_cols as u8,
                    clauses: matched_clauses,
                    referenced_tuples: size.tuples.max(1.0),
                    referenced_rows: base[referenced].rows,
                };
                foreign_count += 1;
            }
        }
    }
    // The output's width (`apply_scanjoin_target_to_paths`): each output
    // column's, a full join's merged column at its type's.
    // A dummy relation's columns were never sized (`attr_widths`).
    let column_width = |(table, column): (usize, usize)| {
        if prepared.false_relids == Relids::of(table) {
            return 0.0;
        }
        f64::from(super::column_width(
            storage,
            txid,
            scope.slots[table],
            column,
        ))
    };
    let mut output_width = 0.0;
    for item in statement.items {
        match item {
            SelectItem::Wildcard(_) if !scope.merged_output().is_empty() => {
                for &entry in scope.merged_output() {
                    output_width += match entry {
                        ResolvedColumn::Table(table, column) => column_width((table, column)),
                        ResolvedColumn::Merged(merged) => match scope.merged_plain_var(merged) {
                            Some(var) => column_width(var),
                            None => type_width(scope.merged[merged].ctype),
                        },
                        ResolvedColumn::TableOid(_) => {
                            return Ok(None);
                        }
                    };
                }
            }
            SelectItem::Wildcard(_) => {
                for table in 0..n {
                    for column in 0..width(table) {
                        output_width += column_width((table, column));
                    }
                }
            }
            SelectItem::TableWildcard(qualifier) => {
                let table = (0..n)
                    .find(|&table| scope.names[table] == *qualifier)
                    .expect("resolved");
                for column in 0..width(table) {
                    output_width += column_width((table, column));
                }
            }
            SelectItem::Expr { expression, .. } => {
                if let Some(column) = column_of(scope, expression) {
                    output_width += column_width(column);
                }
            }
            SelectItem::RecordStar(_) => {
                return Ok(None);
            }
        }
    }
    for &junk in &junk_columns[..junk_count] {
        output_width += column_width(junk);
    }
    output_width += expression_width + junk_width;
    // The groups' width: their select list's, then the grouping columns
    // and `ORDER BY` expressions it does not list.
    let mut grouped_width = 0.0;
    if grouped || aggregated {
        grouped_width += grouped_junk_width;
        for &column in group_columns {
            if !statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. }
                    if column_of(scope, expression) == Some(column))
            }) {
                grouped_width += column_width(column);
            }
        }
        for &(target, _) in group_targets {
            let SortTarget::Expression(key) = target else {
                continue;
            };
            if !statement.items.iter().any(
                |item| matches!(item, SelectItem::Expr { expression, .. } if **expression == *key),
            ) {
                let Some((entry_width, _)) = target_entry(storage, txid, scope, key, arena) else {
                    return Ok(None);
                };
                grouped_width += entry_width;
            }
        }
        for item in statement.items {
            let SelectItem::Expr { expression, .. } = item else {
                return Ok(None);
            };
            grouped_width += match column_of(scope, expression) {
                Some(column) => column_width(column),
                None => match target_entry(storage, txid, scope, expression, arena) {
                    Some((entry_width, _)) => entry_width,
                    None => {
                        return Ok(None);
                    }
                },
            };
        }
    }
    // The window clauses' expression keys: the windows' input computes
    // each, and the query's target list holds those it does not list.
    let mut window_input_cost = 0.0;
    // Those the select list does not hold, which the final target list
    // computes again in its costs.
    let mut window_resjunk_cost = 0.0;
    let mut window_keys_width = 0.0;
    for id in (0..MAX_JOIN_EXPRESSIONS).filter(|id| window_expressions & 1 << id != 0) {
        let held = expressions.get(id);
        window_input_cost += held.cost;
        window_keys_width += held.width;
        let listed = statement.items.iter().any(|item| {
            matches!(item, SelectItem::Expr { expression, .. }
                if expressions.find(scope, expression)
                    == Some(Member::expression(held.rel, id as u16)))
        });
        if !listed {
            output_width += held.width;
            window_resjunk_cost += held.cost;
        }
    }
    // The windows' input is each column the select list, their keys and
    // their functions read, once, an expression key's columns as the
    // expression (`make_window_input_target`).
    let final_width = output_width;
    if windowed && !(grouped || aggregated) {
        windows.input_width = window_keys_width;
        for (table, &mask) in output.iter().enumerate().take(n) {
            let mask = mask & !key_only_columns[table];
            for column in (0..64).filter(|column| mask & 1 << column != 0) {
                windows.input_width += column_width((table, column));
            }
        }
        output_width = windows.input_width;
    }
    // An aggregate's input is each column its arguments and the grouping
    // read, once.
    if aggregated || grouped {
        output_width = 0.0;
        for (table, &mask) in output.iter().enumerate().take(n) {
            for column in (0..64).filter(|column| mask & 1 << column != 0) {
                output_width += column_width((table, column));
            }
        }
    }
    // The aggregates' costs (`get_agg_clause_costs`).
    let mut problem_group_aggregates = None;
    let group = if distinct {
        problem_group_aggregates = Some(super::AggregateCosts::default());
        Some(super::joins::GroupProblem {
            keys: group_keys,
            columns: group_columns_count,
            sort: sort_keys,
            members: group_members,
            aggregates: super::AggregateCosts::default(),
            input_width: output_width,
            output_width,
            hashable: true,
            hashagg: value("enable_hashagg") != 0.0,
            group_by_reordering: value("enable_distinct_reordering") != 0.0,
            distinct: true,
            having: None,
            target_cost: 0.0,
        })
    } else if grouped || aggregated {
        // The select list's, `HAVING`'s and `ORDER BY`'s aggregates.
        let listed = statement.items.len();
        let expressions = arena
            .alloc_slice_with(listed + 1 + statement.order_by.len(), |at| match statement
                .items
                .get(at)
            {
                Some(SelectItem::Expr { expression, .. }) => *expression,
                Some(_) => &Expr::Null,
                None if at == listed => statement.having.unwrap_or(&Expr::Null),
                None => sort_entries
                    .get(at - listed - 1)
                    .map_or(&Expr::Null, |&(target, ..)| match target {
                        SortTarget::Expression(expression) => expression,
                        SortTarget::Column(..) => &Expr::Null,
                    }),
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        let columns = crate::sql::query::CatalogScopeCols {
            scope,
            outer: None,
            storage,
            txid,
            arena,
        };
        let Some(aggregates) = super::aggregate_costs(expressions, storage, txid, &columns, arena)?
        else {
            return Ok(None);
        };
        problem_group_aggregates = Some(aggregates);
        Some(super::joins::GroupProblem {
            keys: group_keys,
            columns: group_columns_count,
            sort: sort_keys,
            members: group_members,
            aggregates,
            input_width: output_width,
            // Windows above read the groups without their functions.
            output_width: if windowed {
                grouped_width - window_items_width
            } else {
                grouped_width
            },
            hashable: !aggregates.ordered,
            hashagg: value("enable_hashagg") != 0.0,
            group_by_reordering: value("enable_group_by_reordering") != 0.0,
            distinct: false,
            having,
            target_cost: if windowed { 0.0 } else { group_target_cost },
        })
    } else {
        None
    };
    let expression_costs: [f64; MAX_JOIN_EXPRESSIONS] = core::array::from_fn(|id| {
        if id < expressions.count {
            expressions.get(id).cost
        } else {
            0.0
        }
    });
    let reader = RelStatistics {
        planners: &planners,
        rels: &base,
        expression_columns: &expression_columns,
        expression_vars: &expression_vars,
    };
    let subproblems = &prepared.subproblems[..prepared.subproblem_count];
    let problem = JoinProblem {
        rels: &base[..n],
        classes,
        specials: &prepared.specials[..prepared.special_count],
        subproblems,
        subproblem_items: &prepared.subproblem_items[..prepared.subproblem_item_count],
        clauses: clauses.as_slice(),
        needed: needed.as_slice(),
        expression_costs: &expression_costs,
        foreign_keys: &foreign_keys[..foreign_count],
        removed,
        upper: super::joins::UpperProblem {
            // `query_pathkeys`: the grouping's, else the longer of the
            // `DISTINCT`'s and the `ORDER BY`'s.
            sort: if grouped
                || (distinct && group_keys.as_slice().len() > sort_keys.as_slice().len())
            {
                group_keys
            } else {
                sort_keys
            },
            limit,
            width: if grouped || aggregated {
                grouped_width
            } else {
                final_width
            },
            // A grouping's input is the columns it reads; the windows' the
            // columns and expression keys they read; a `DISTINCT`'s the
            // final target list.
            target_cost: if group.is_some() && !distinct {
                0.0
            } else if windowed {
                window_input_cost
            } else {
                target_cost
            },
            group,
            windows: super::joins::WindowsProblem {
                input_width: if grouped || aggregated {
                    grouped_width - window_items_width
                } else {
                    windows.input_width
                },
                ..windows
            },
            windows_output_cost: if grouped || aggregated {
                group_target_cost
            } else {
                target_cost + window_resjunk_cost
            },
        },
        dummy: prepared.contradictory,
        false_relids: prepared.false_relids,
        settings: JoinSettings {
            costs,
            work_mem_bytes: f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0,
            hash_mem_bytes: crate::sql::guc::hash_memory_bytes(),
            nestloop: methods.nestloop,
            hashjoin: methods.hashjoin,
            mergejoin: value("enable_mergejoin") != 0.0,
            material: methods.material,
            sort: methods.sort,
            incremental_sort: methods.incremental_sort,
            memoize: value("enable_memoize") != 0.0,
            geqo: super::joins::GeqoSettings::current(),
        },
    };
    let Ok(plan) = super::joins::search(&problem, &reader, arena)? else {
        return Ok(None);
    };
    // A hashed unique-ified relation returns its groups in its hash table's
    // order, by the hash values only some key types have here.
    // The type of a class member.
    let member_type = |member: &Member| match member.expression_id() {
        Some(id) => expressions.get(id).ctype,
        None => {
            scope
                .defs
                .stored(usize::from(member.rel))
                .expect("plain table")
                .columns()[usize::from(member.column)]
            .ctype
        }
    };
    if reaches_hashed_unique(&plan, &|member| {
        crate::sql::query::join_exec::datum_type_hashable(member_type(member))
    }) {
        return Ok(None);
    }
    // A hash join planned with several batches, or one returning its
    // inner rows without a match or at their first match, walks its table
    // by the keys' hash values, which only some key types have here.
    for path in plan.paths {
        if let super::joins::PathKind::HashJoin {
            hash,
            batches,
            jointype,
            ..
        } = path.kind
            && (batches > 1
                || jointype.fills_inner()
                || jointype == super::joins::JoinType::RightSemi)
            && hash.as_slice().iter().any(|&id| {
                let clause = &plan.clauses[usize::from(id)];
                let member = match clause.form {
                    ClauseForm::Equality { class, left, .. } => {
                        classes[usize::from(class)].members()[usize::from(left)]
                    }
                    ClauseForm::Join { left, .. } => left,
                    ClauseForm::Other => return true,
                };
                !crate::sql::query::join_exec::datum_type_hashable(member_type(&member))
            })
        {
            return Ok(None);
        }
    }
    // The grouping the plan chose: its keys as the `GROUP BY` names them.
    let modeled_group = match plan.upper.group {
        Some(chosen) => {
            let keys = arena
                .alloc_slice_with(usize::from(group_columns_count), |at| {
                    let key = chosen.keys.as_slice()[at];
                    let (position, _) = group_shown
                        .iter()
                        .find(|&&(_, class)| class == key & KEY_CLASS)
                        .copied()
                        .expect("a grouping key");
                    (position, key & !KEY_CLASS)
                })
                .map_err(|_| crate::sql::eval::arena_full())?;
            let hash_agg =
                problem_group_aggregates
                    .filter(|_| chosen.hashed)
                    .and_then(|aggregates| {
                        super::aggregate::hash_agg_sizing(
                            &aggregates,
                            chosen.groups,
                            output_width,
                            usize::from(group_columns_count),
                        )
                    });
            let order_sizing =
                problem_group_aggregates
                    .filter(|_| chosen.hashed)
                    .and_then(|aggregates| {
                        super::aggregate::hash_agg_sizing(
                            &super::aggregate::AggregateCosts {
                                unsized_states: false,
                                ..aggregates
                            },
                            chosen.groups,
                            output_width,
                            usize::from(group_columns_count),
                        )
                    });
            Some(ModeledGroup {
                order_sizing,
                hashed: chosen.hashed,
                aggregated: problem_group_aggregates
                    .is_some_and(|aggregates| aggregates.aggregated()),
                hash_agg,
                distinct,
                keys,
                presorted: arena
                    .alloc_slice_copy(&presort_tail[..presort_count])
                    .map_err(|_| crate::sql::eval::arena_full())?,
            })
        }
        None => None,
    };
    let rels = arena
        .alloc_slice_with(n, |table| rels[table].expect("planned"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let quals = arena
        .alloc_slice_copy(quals)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let windows = arena
        .alloc_slice_with(usize::from(windows.count), |at| window_kept[at])
        .map_err(|_| crate::sql::eval::arena_full())?;
    Ok(Some(ModeledJoin {
        plan,
        rels,
        classes: &classes[..class_count],
        quals,
        output_width,
        final_width,
        windows,
        group: modeled_group,
        sort_items: arena
            .alloc_slice_copy(&sort_items[..sort_keys.as_slice().len()])
            .map_err(|_| crate::sql::eval::arena_full())?,
        sort_shown: arena
            .alloc_slice_copy(&sort_shown[..sort_keys.as_slice().len()])
            .map_err(|_| crate::sql::eval::arena_full())?,
        expressions: expressions.alloc()?,
        nested: None,
        self_join,
    }))
}

/// A select-list expression's average width and its cost per row
/// (`set_pathtarget_cost_width`); `None` for a type the model does not
/// size.
fn target_entry<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    arena: &'a Arena,
) -> Option<(f64, f64)> {
    let columns = crate::sql::query::CatalogScopeCols {
        scope,
        outer: None,
        storage,
        txid,
        arena,
    };
    let (type_oid, type_mod) = crate::sql::exec::infer_type_res(expression, &columns).ok()?;
    let Some((ctype, None)) = crate::sql::exec::catalog_column_type(storage, txid, type_oid) else {
        return None;
    };
    Some((
        super::type_average_width(ctype, i32::from(type_mod)),
        super::super::plan::qual_cost(expression, &columns, storage, txid, arena) / 2.0
            * Costs::current().cpu_operator,
    ))
}

/// `join_is_removable` of left join `info` to relation `table`: no row
/// above it reads the relation, and a unique index's keys are each equated
/// with the join's left side by a mergeable clause of the join or with a
/// constant (`rel_is_distinct_for`).
fn join_is_removable(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    prepared: &join_tree::Prepared,
    info: &super::joins::SpecialJoin,
    table: usize,
    output: u64,
) -> bool {
    let inputs = info.min_left | info.min_right;
    let joinrelids = inputs | info.oj;
    if output != 0
        || prepared
            .needed
            .borrowed_slice()
            .iter()
            .any(|&((rel, _), relids, _)| {
                usize::from(rel) == table && relids & !inputs != Relids::EMPTY
            })
    {
        return false;
    }
    let mut matched = 0u64;
    for class in prepared.classes() {
        if class.has_const {
            for member in class.members() {
                if usize::from(member.rel) == table
                    && member.nulling == Relids::EMPTY
                    && member.column < 64
                {
                    matched |= 1 << member.column;
                }
            }
        }
    }
    for placed in prepared.placed() {
        let Placed::Join(join) = placed else {
            continue;
        };
        if join.clone == super::joins::CloneKind::Is
            || join.required & info.min_right == Relids::EMPTY
            || join.pushed_down
            || join.required & !joinrelids != Relids::EMPTY
        {
            continue;
        }
        let Some((left, right, ..)) = join.merge else {
            continue;
        };
        for (own, other) in [(left, right), (right, left)] {
            if own.relids() == info.min_right
                && other.relids() & !info.min_left == Relids::EMPTY
                && own.column < 64
            {
                matched |= 1 << own.column;
            }
        }
    }
    let mut distinct = false;
    crate::sql::catalog::planner_indexes(storage, txid, scope.slots[table], &mut |index| {
        if !index.unique
            || !index.immediate
            || index.partial
            || index.method != crate::sql::ast::IndexAccessMethod::Btree
        {
            return;
        }
        distinct |= (0..index.n_cols).all(|position| {
            !index.expression_keys[position]
                && index.columns[position] < 64
                && matched & 1 << index.columns[position] != 0
        });
    });
    distinct
}

/// Whether `remove_useless_self_joins` may remove one of two scans of the
/// same table that no outer join separates: the columns one class equates
/// between them cover a unique index (`innerrel_is_unique_ext`). A
/// superset of PostgreSQL's removals, for `EXPLAIN` to refuse rather than
/// print a join PostgreSQL does not run.
fn removable_self_join(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    prepared: &join_tree::Prepared,
    removed: Relids,
) -> bool {
    if crate::sql::guc::planner_value("enable_self_join_elimination") == 0.0 {
        return false;
    }
    let n = scope.n.min(MAX_JOIN_RELS);
    for r in 0..n {
        for k in r + 1..n {
            if removed & (Relids::of(r) | Relids::of(k)) != Relids::EMPTY
                || scope.slots[r] != scope.slots[k]
            {
                continue;
            }
            let (r_bit, k_bit): (Relids, Relids) = (Relids::of(r), Relids::of(k));
            if prepared.specials[..prepared.special_count]
                .iter()
                .any(|info| {
                    (info.syn_left & r_bit != Relids::EMPTY)
                        != (info.syn_left & k_bit != Relids::EMPTY)
                        || (info.syn_right & r_bit != Relids::EMPTY)
                            != (info.syn_right & k_bit != Relids::EMPTY)
                })
            {
                continue;
            }
            let mut covered = 0u64;
            for class in prepared.classes() {
                let plain = |rel: usize| {
                    class.members().iter().filter(move |member| {
                        usize::from(member.rel) == rel
                            && member.nulling == Relids::EMPTY
                            && member.reads == Relids::EMPTY
                            && member.column < 64
                            && member.expression_id().is_none()
                    })
                };
                for member in plain(k) {
                    if plain(r).any(|other| other.column == member.column) {
                        covered |= 1 << member.column;
                    }
                }
            }
            if covered == 0 {
                continue;
            }
            let mut unique = false;
            crate::sql::catalog::planner_indexes(storage, txid, scope.slots[k], &mut |index| {
                unique |= index.unique
                    && index.immediate
                    && (0..index.n_cols).all(|position| {
                        !index.expression_keys[position]
                            && index.columns[position] < 64
                            && covered & 1 << index.columns[position] != 0
                    });
            });
            if unique {
                return true;
            }
        }
    }
    false
}

fn and<'a>(
    left: &'a Expr<'a>,
    right: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    arena
        .alloc(Expr::Binary {
            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
            operator: BinaryOp::And,
            left,
            right,
        })
        .map(|expression| &*expression)
        .map_err(|_| crate::sql::eval::arena_full())
}

pub(crate) fn source<'a>(from: &'a FromClause<'a>, table: usize) -> &'a TableRef<'a> {
    if table == 0 {
        &from.base
    } else {
        &from.joins[table - 1].table
    }
}

/// A class of `member` alone, in `arena`.
fn single_member(member: Member, arena: &Arena) -> Result<&[Member], SqlError> {
    Ok(arena
        .alloc_slice_copy(&[member])
        .map_err(|_| crate::sql::eval::arena_full())?)
}

/// A relation's scan equated with more relations than it holds join
/// parameters for.
fn join_params_full() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "join search capacity exceeded: an indexed relation's scan holds at most {} join clauses",
        MAX_JOIN_PARAMS
    )
}

/// The join preparation's capacities exceeded.
fn preparation_full() -> SqlError {
    crate::sql_err!(
        crate::sql::eval::sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "join search capacity exceeded: a query holds at most {} conditions and {} equivalence classes of {} members",
        join_tree::MAX_QUALS,
        join_tree::MAX_CLASSES,
        super::joins::MAX_MEMBERS
    )
}

/// The FROM position and column a plain column reference reads: a merged
/// `USING` column whose join alias variable is one column of its type
/// (`flatten_join_alias_vars`) reads that column.
pub(crate) fn column_of(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<(usize, usize)> {
    let Expr::Column { qualifier, name } = expression else {
        return None;
    };
    match scope.probe_column(*qualifier, name)? {
        ResolvedColumn::Table(table, column) => Some((table, column)),
        ResolvedColumn::Merged(merged) => scope.merged_plain_var(merged),
        ResolvedColumn::TableOid(_) => None,
    }
}

/// A plain local table without descendants, indexes, row security or
/// sampling, read under its own name.
fn plain_table(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    table: usize,
    tref: &TableRef<'_>,
) -> bool {
    let slot = scope.slots[table];
    if slot == usize::MAX
        || scope.derived[table].is_some()
        || scope.lateral[table]
        || tref.subquery.is_some()
        || tref.func_args.is_some()
        || tref.rows_from.is_some()
        || tref.col_alias.is_some()
        || tref.sample.is_some()
        || tref.cte.is_some()
    {
        return false;
    }
    let Some(definition) = scope.defs.stored(table) else {
        return false;
    };
    definition.kind == crate::storage::TableKind::Local
        && !definition.partition.is_partitioned()
        && !storage.relation_has_descendants(slot, txid)
        && storage.table_statistics(slot, txid).valid
        && !definition.row_level_security.enabled
}

struct RelStatistics<'r, 's, 'a> {
    planners: &'r [Option<Planner<'s, 'a>>; MAX_JOIN_RELS],
    rels: &'r [BaseRel<'r>; MAX_JOIN_RELS],
    /// Each expression member's variable in its relation's planner.
    expression_columns: &'r [usize; MAX_JOIN_EXPRESSIONS],
    /// Each expression member's columns.
    expression_vars: &'r [VarList; MAX_JOIN_EXPRESSIONS],
}

impl RelStatistics<'_, '_, '_> {
    fn planner(&self, member: Member) -> &Planner<'_, '_> {
        self.planners[usize::from(member.rel)]
            .as_ref()
            .expect("a member's relation has statistics")
    }

    /// The planner's variable of a member: its column, or the variable of
    /// its expression (`examine_variable`).
    fn column(&self, member: Member) -> usize {
        match member.expression_id() {
            Some(id) => self.expression_columns[id],
            None => usize::from(member.column),
        }
    }

    /// Each relation the members read, in order of first appearance, with
    /// the variables `estimate_num_groups` counts there.
    fn grouped_rels<'s>(
        &'s self,
        members: &'s [Member],
    ) -> impl Iterator<Item = (usize, ([usize; 64], usize))> + 's {
        let mut order = [0u8; MAX_JOIN_RELS];
        let mut count = 0;
        let mut done = Relids::EMPTY;
        for member in members {
            for rel in (member.base_relids() & !done).rels() {
                done |= Relids::of(rel);
                order[count] = rel;
                count += 1;
            }
        }
        (0..count).map(move |at| {
            let rel = order[at];
            let mut columns = [0usize; 64];
            let found = self.group_columns(members, rel, &mut columns);
            (usize::from(rel), (columns, found))
        })
    }

    /// The variables `estimate_num_groups` counts for the members of one
    /// relation: a column, an expression with statistics or a unique
    /// index of its own, else the columns the expression reads.
    fn group_columns(&self, members: &[Member], rel: u8, columns: &mut [usize; 64]) -> usize {
        let mut count = 0;
        let mut add = |column: usize, count: &mut usize| {
            if *count < columns.len() {
                columns[*count] = column;
                *count += 1;
            }
        };
        // An expression of several relations: its columns of this one.
        for member in members
            .iter()
            .filter(|member| member.reads != Relids::EMPTY)
        {
            let id = member.expression_id().expect("an expression");
            for &(held, var) in self.expression_vars[id].as_slice() {
                if held == rel {
                    add(usize::from(var), &mut count);
                }
            }
        }
        for member in members
            .iter()
            .filter(|member| member.rel == rel && member.reads == Relids::EMPTY)
        {
            let column = self.column(*member);
            if member.expression_id().is_none() {
                add(column, &mut count);
                continue;
            }
            let variable = self.planner(*member).variable(column);
            if variable.analyzed || variable.unique {
                add(column, &mut count);
                continue;
            }
            let id = member.expression_id().expect("an expression");
            for &(_, var) in self.expression_vars[id].as_slice() {
                add(usize::from(var), &mut count);
            }
        }
        count
    }

    /// `get_variable_range` of an analyzed column: the least and greatest
    /// of its histogram bounds and common values. Without a histogram the
    /// common values count only when they and the NULLs cover the whole
    /// table, as they then hold every value.
    fn range<'v>(
        &self,
        planner: &'v Planner<'_, '_>,
        column: usize,
    ) -> Result<Option<(Datum<'v>, Datum<'v>)>, SqlError> {
        let variable = planner.variable(column);
        if !variable.analyzed {
            return Ok(None);
        }
        let Some(distribution) = variable.distribution else {
            return Ok(None);
        };
        let mut range: Option<(Datum<'v>, Datum<'v>)> = None;
        let histogram = distribution.histogram_len();
        if histogram > 0 {
            range = Some((
                distribution.histogram_value(0, variable.ctype)?,
                distribution.histogram_value(histogram - 1, variable.ctype)?,
            ));
        } else {
            let common: f64 = (0..distribution.mcv_len())
                .map(|index| f64::from(distribution.mcv_frequency(index)))
                .sum();
            if common + variable.null_fraction <= 0.99999 {
                return Ok(None);
            }
        }
        for index in 0..distribution.mcv_len() {
            let value = distribution.mcv_value(index, variable.ctype)?;
            range = Some(match range {
                None => (value, value),
                Some((low, high)) => (
                    if planner.compare(variable.collation, &value, &low)?.is_lt() {
                        value
                    } else {
                        low
                    },
                    if planner.compare(variable.collation, &value, &high)?.is_gt() {
                        value
                    } else {
                        high
                    },
                ),
            });
        }
        Ok(range)
    }
}

impl JoinStatistics for RelStatistics<'_, '_, '_> {
    fn equality(&self, left: Member, right: Member) -> Result<f64, SqlError> {
        // An expression of several relations has no statistics and the
        // default distinct count (`get_variable_numdistinct` without a
        // relation): `eqjoinsel_inner` without MCV lists.
        if left.reads != Relids::EMPTY || right.reads != Relids::EMPTY {
            let side = |member: Member| {
                if member.reads != Relids::EMPTY {
                    return (super::DEFAULT_NUM_DISTINCT, 0.0);
                }
                let planner = self.planner(member);
                let variable = planner.variable(self.column(member));
                let nulls = if variable.analyzed {
                    variable.null_fraction
                } else {
                    0.0
                };
                (planner.distinct(&variable).0, nulls)
            };
            let ((nd1, null1), (nd2, null2)) = (side(left), side(right));
            return Ok((1.0 - null1) * (1.0 - null2) / nd1.max(nd2));
        }
        let (first, second) = (self.planner(left), self.planner(right));
        let one = first.variable(self.column(left));
        let two = second.variable(self.column(right));
        let (nd1, _) = first.distinct(&one);
        let (nd2, _) = second.distinct(&two);
        let mcv1 = one
            .distribution
            .filter(|distribution| one.analyzed && distribution.mcv_len() > 0);
        let mcv2 = two
            .distribution
            .filter(|distribution| two.analyzed && distribution.mcv_len() > 0);
        let (Some(mcv1), Some(mcv2)) = (mcv1, mcv2) else {
            let null1 = if one.analyzed { one.null_fraction } else { 0.0 };
            let null2 = if two.analyzed { two.null_fraction } else { 0.0 };
            return Ok((1.0 - null1) * (1.0 - null2) / nd1.max(nd2));
        };
        let (count1, count2) = (mcv1.mcv_len(), mcv2.mcv_len());
        let mut matched2 = [false; 10_000];
        if count2 > matched2.len() {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a join of columns with more than 10000 most common values is not supported"
            ));
        }
        let mut matched1_freq = 0.0;
        let mut unmatched1 = 0.0;
        let mut product = 0.0;
        let mut matches = 0usize;
        for i in 0..count1 {
            let value = mcv1.mcv_value(i, one.ctype)?;
            let frequency = f64::from(mcv1.mcv_frequency(i));
            let mut hit = false;
            for (j, matched) in matched2.iter_mut().enumerate().take(count2) {
                if *matched {
                    continue;
                }
                let other = mcv2.mcv_value(j, two.ctype)?;
                if first.compare(one.collation, &value, &other)?.is_eq() {
                    *matched = true;
                    hit = true;
                    product += frequency * f64::from(mcv2.mcv_frequency(j));
                    matches += 1;
                    break;
                }
            }
            if hit {
                matched1_freq += frequency;
            } else {
                unmatched1 += frequency;
            }
        }
        let product = clamp_probability(product);
        let matched1 = clamp_probability(matched1_freq);
        let unmatched1 = clamp_probability(unmatched1);
        let (mut matched2_freq, mut unmatched2) = (0.0, 0.0);
        for (j, matched) in matched2.iter().enumerate().take(count2) {
            let frequency = f64::from(mcv2.mcv_frequency(j));
            if *matched {
                matched2_freq += frequency;
            } else {
                unmatched2 += frequency;
            }
        }
        let matched2 = clamp_probability(matched2_freq);
        let unmatched2 = clamp_probability(unmatched2);
        let other1 = clamp_probability(1.0 - one.null_fraction - matched1 - unmatched1);
        let other2 = clamp_probability(1.0 - two.null_fraction - matched2 - unmatched2);
        let mut total1 = product;
        if nd2 > count2 as f64 {
            total1 += unmatched1 * other2 / (nd2 - count2 as f64);
        }
        if nd2 > matches as f64 {
            total1 += other1 * (other2 + unmatched2) / (nd2 - matches as f64);
        }
        let mut total2 = product;
        if nd1 > count1 as f64 {
            total2 += unmatched2 * other1 / (nd1 - count1 as f64);
        }
        if nd1 > matches as f64 {
            total2 += other2 * (other1 + unmatched1) / (nd1 - matches as f64);
        }
        Ok(total1.min(total2))
    }

    fn semi(&self, outer: Member, inner: Member, inner_rows: f64) -> Result<f64, SqlError> {
        // An expression of several relations: a default distinct count,
        // `eqjoinsel_semi` without MCV lists.
        if outer.reads != Relids::EMPTY || inner.reads != Relids::EMPTY {
            let nulls = if outer.reads != Relids::EMPTY {
                0.0
            } else {
                let variable = self.planner(outer).variable(self.column(outer));
                if variable.analyzed {
                    variable.null_fraction
                } else {
                    0.0
                }
            };
            return Ok(0.5 * (1.0 - nulls));
        }
        let (first, second) = (self.planner(outer), self.planner(inner));
        let one = first.variable(self.column(outer));
        let two = second.variable(self.column(inner));
        let (mut nd1, default1) = first.distinct(&one);
        let (mut nd2, mut default2) = second.distinct(&two);
        // The inner side has no more distinct values than rows, its base
        // relation's or its input relation's.
        let base_rows = self.rels[usize::from(inner.rel)].rows;
        if nd2 >= base_rows {
            nd2 = base_rows;
            default2 = false;
        }
        if nd2 >= inner_rows {
            nd2 = inner_rows;
            default2 = false;
        }
        let mcv1 = one
            .distribution
            .filter(|distribution| one.analyzed && two.analyzed && distribution.mcv_len() > 0);
        let mcv2 = two
            .distribution
            .filter(|distribution| one.analyzed && two.analyzed && distribution.mcv_len() > 0);
        let null1 = if one.analyzed { one.null_fraction } else { 0.0 };
        let (Some(mcv1), Some(mcv2)) = (mcv1, mcv2) else {
            return Ok(if !default1 && !default2 {
                if nd1 <= nd2 || nd2 < 0.0 {
                    1.0 - null1
                } else {
                    (nd2 / nd1) * (1.0 - null1)
                }
            } else {
                0.5 * (1.0 - null1)
            });
        };
        // Only the first `nd2` common values of the inner side can appear.
        let count2 = (mcv2.mcv_len() as f64).min(nd2) as usize;
        let mut matched2 = [false; 10_000];
        if count2 > matched2.len() {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "a join of columns with more than 10000 most common values is not supported"
            ));
        }
        let mut matched_frequency = 0.0;
        let mut matches = 0usize;
        for i in 0..mcv1.mcv_len() {
            let value = mcv1.mcv_value(i, one.ctype)?;
            for (j, matched) in matched2.iter_mut().enumerate().take(count2) {
                if *matched {
                    continue;
                }
                let other = mcv2.mcv_value(j, two.ctype)?;
                if first.compare(one.collation, &value, &other)?.is_eq() {
                    *matched = true;
                    matches += 1;
                    matched_frequency += f64::from(mcv1.mcv_frequency(i));
                    break;
                }
            }
        }
        let matched_frequency = clamp_probability(matched_frequency);
        let uncertain_fraction = if !default1 && !default2 {
            nd1 -= matches as f64;
            nd2 -= matches as f64;
            if nd1 <= nd2 || nd2 < 0.0 {
                1.0
            } else {
                nd2 / nd1
            }
        } else {
            0.5
        };
        let uncertain = clamp_probability(1.0 - matched_frequency - one.null_fraction);
        Ok(matched_frequency + uncertain_fraction * uncertain)
    }

    fn groups(&self, members: &[Member], calls: f64) -> Result<f64, SqlError> {
        let mut groups = 1.0;
        let mut used_default = false;
        for (rel, columns) in self.grouped_rels(members) {
            let (factor, default) = super::paths::relation_groups(
                self.planners[rel]
                    .as_ref()
                    .expect("a member's relation has statistics"),
                &columns.0[..columns.1],
                self.rels[rel].rows,
            );
            groups *= factor;
            used_default |= default;
        }
        if used_default {
            return Ok(calls);
        }
        Ok(groups.ceil().min(calls).max(1.0))
    }

    fn num_groups(&self, members: &[Member], rows: f64) -> Result<f64, SqlError> {
        let mut groups = 1.0;
        for (rel, columns) in self.grouped_rels(members) {
            groups *= super::paths::relation_groups(
                self.planners[rel]
                    .as_ref()
                    .expect("a member's relation has statistics"),
                &columns.0[..columns.1],
                self.rels[rel].rows,
            )
            .0;
        }
        Ok(groups.ceil().min(super::clamp_row_est(rows)).max(1.0))
    }

    fn bucket(&self, member: Member, buckets: f64) -> Result<(f64, f64), SqlError> {
        // No statistics: `estimate_hash_bucket_stats`' default.
        if member.reads != Relids::EMPTY {
            return Ok((0.0, 0.1));
        }
        let planner = self.planner(member);
        let variable = planner.variable(self.column(member));
        let mcv_frequency = variable
            .distribution
            .filter(|distribution| variable.analyzed && distribution.mcv_len() > 0)
            .map_or(0.0, |distribution| f64::from(distribution.mcv_frequency(0)));
        let (mut distinct, default) = planner.distinct(&variable);
        if default {
            return Ok((mcv_frequency, mcv_frequency.max(0.1)));
        }
        let null_fraction = if variable.analyzed {
            variable.null_fraction
        } else {
            0.0
        };
        let average = (1.0 - null_fraction) / distinct;
        let tuples = planner.size.tuples;
        if tuples > 0.0 {
            distinct =
                super::clamp_row_est(distinct * self.rels[usize::from(member.rel)].rows / tuples);
        }
        let mut fraction = if distinct > buckets {
            1.0 / buckets
        } else {
            1.0 / distinct
        };
        if average > 0.0 && mcv_frequency > average {
            fraction *= mcv_frequency / average;
        }
        Ok((mcv_frequency, fraction.clamp(1.0e-6, 1.0)))
    }

    fn merge_range(
        &self,
        left: Member,
        right: Member,
        descending: bool,
        nulls_first: bool,
    ) -> Result<Option<[f64; 4]>, SqlError> {
        let mut range = [0.0, 1.0, 0.0, 1.0];
        // No histogram bounds: `mergejoinscansel` keeps the whole ranges.
        if left.reads != Relids::EMPTY || right.reads != Relids::EMPTY {
            return Ok(Some(range));
        }
        let (first, second) = (self.planner(left), self.planner(right));
        let (left_column, right_column) = (self.column(left), self.column(right));
        let (Some((left_low, left_high)), Some((right_low, right_high))) = (
            self.range(first, left_column)?,
            self.range(second, right_column)?,
        ) else {
            return Ok(Some(range));
        };
        // Merged descending, each side starts at its maximum: the ranges
        // swap ends and the comparisons turn to `>=` and `>`.
        let (left_min, left_max, right_min, right_max, end_op, start_op) = if descending {
            (
                left_high,
                left_low,
                right_high,
                right_low,
                Comparison::Ge,
                Comparison::Gt,
            )
        } else {
            (
                left_low,
                left_high,
                right_low,
                right_high,
                Comparison::Le,
                Comparison::Lt,
            )
        };
        let default = super::DEFAULT_INEQ_SEL;
        let selectivity = |planner: &Planner<'_, '_>, column, comparison, value| {
            planner.inequality_selectivity(column, comparison, value)
        };
        let Ok(left_end) = selectivity(first, left_column, end_op, right_max)? else {
            return Ok(None);
        };
        let Ok(right_end) = selectivity(second, right_column, end_op, left_max)? else {
            return Ok(None);
        };
        if left_end != default {
            range[1] = left_end;
        }
        if right_end != default {
            range[3] = right_end;
        }
        if range[1] > range[3] {
            range[1] = 1.0;
        } else if range[1] < range[3] {
            range[3] = 1.0;
        } else {
            range[1] = 1.0;
            range[3] = 1.0;
        }
        let Ok(left_start) = selectivity(first, left_column, start_op, right_min)? else {
            return Ok(None);
        };
        let Ok(right_start) = selectivity(second, right_column, start_op, left_min)? else {
            return Ok(None);
        };
        if left_start != default {
            range[0] = left_start;
        }
        if right_start != default {
            range[2] = right_start;
        }
        if range[0] < range[2] {
            range[0] = 0.0;
        } else if range[0] > range[2] {
            range[2] = 0.0;
        } else {
            range[0] = 0.0;
            range[2] = 0.0;
        }
        // NULLs sorted first come before every value.
        if nulls_first {
            for (planner, column, at) in [(first, left_column, 0), (second, right_column, 2)] {
                let nulls = planner.variable(column).null_fraction;
                range[at] = (range[at] + nulls).clamp(0.0, 1.0);
                range[at + 1] = (range[at + 1] + nulls).clamp(0.0, 1.0);
            }
        }
        if range[0] >= range[1] {
            range[0] = 0.0;
            range[1] = 1.0;
        }
        if range[2] >= range[3] {
            range[2] = 0.0;
            range[3] = 1.0;
        }
        Ok(Some(range))
    }
}

/// Whether the chosen plan hashes a unique-ified relation by a key
/// `hashable` rejects.
fn reaches_hashed_unique(plan: &JoinPlan<'_>, hashable: &dyn Fn(&Member) -> bool) -> bool {
    fn reaches(plan: &JoinPlan<'_>, id: u32, hashable: &dyn Fn(&Member) -> bool) -> bool {
        use super::joins::PathKind;
        match plan.path(id).kind {
            PathKind::Unique {
                method,
                input,
                keys,
                key_count,
            } => {
                (method == super::joins::UniqueMethod::Hash
                    && !plan.keys(keys, key_count).iter().all(hashable))
                    || reaches(plan, input, hashable)
            }
            PathKind::Scan { .. } | PathKind::Dummy => false,
            PathKind::Material { input } | PathKind::Memoize { input, .. } => {
                reaches(plan, input, hashable)
            }
            PathKind::NestLoop { outer, inner, .. }
            | PathKind::HashJoin { outer, inner, .. }
            | PathKind::MergeJoin { outer, inner, .. } => {
                reaches(plan, outer, hashable) || reaches(plan, inner, hashable)
            }
        }
    }
    reaches(plan, plan.top, hashable)
}

/// `get_typavgwidth` of a column type without a type modifier.
fn type_width(ctype: ColType) -> f64 {
    match ctype {
        ColType::Bool => 1.0,
        ColType::Int2 => 2.0,
        ColType::Int4 | ColType::Date => 4.0,
        ColType::Int8 => 8.0,
        _ => 32.0,
    }
}
