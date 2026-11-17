//! The queries whose joins PostgreSQL's join search ([`super::joins`])
//! plans in pos3ql: a `SELECT` of plain columns from two to eight plain,
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
use crate::storage::{Storage, TableStatistics};

use super::join_tree::{self, Placed, Qual, Shape, Tree, TreeJoin, TreeNode, VarList};
use super::joins::{
    BASE_RELIDS, BasePath, BaseRel, Clause, ClauseForm, ClauseList, ForeignKeyJoin, JoinPlan,
    JoinProblem, JoinSettings, JoinStatistics, Keys, MAX_CLAUSES, MAX_JOIN_RELS, MAX_MEMBERS,
    Member, NO_KEY_CLASS, NeededColumn, Relids,
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
    /// The relations whose values the condition reads.
    pub(crate) param: u8,
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
            describe: Some(candidate.describe),
            ..self.scan
        };
        let mut joins = [&Expr::Null; MAX_JOIN_PARAMS];
        for (at, &(class, rel)) in self.scan.params[..usize::from(self.scan.param_count)]
            .iter()
            .enumerate()
        {
            if candidate.param & 1 << rel != 0 {
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
            output: Some(self.output),
            member: super::Member::Join(scan),
            ..QueryShape::default()
        };
        let planned = super::scan_path(
            storage,
            &single,
            txid,
            condition,
            Some(self.read),
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

/// A plain column reference `relation.column`.
fn column_expression<'a>(
    scope: &QueryScope<'a>,
    member: Member,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
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

/// `left = right`, each a column named by its relation.
fn equality<'a>(
    scope: &QueryScope<'a>,
    left: Member,
    right: Member,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let left = column_expression(scope, left, arena)?;
    let right = column_expression(scope, right, arena)?;
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
    member: Member,
    value: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let column = column_expression(scope, member, arena)?;
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

/// The plan of a modeled join and what its nodes read.
pub(crate) struct ModeledJoin<'a> {
    pub(crate) plan: JoinPlan<'a>,
    pub(crate) rels: &'a [ModeledRel<'a>],
    /// Each class member's FROM position and column.
    pub(crate) columns: &'a [[(usize, usize); MAX_MEMBERS]],
    /// The query's conditions, by [`Clause::source`].
    pub(crate) quals: &'a [&'a Expr<'a>],
}

impl<'a> ModeledJoin<'a> {
    /// The FROM position and column of member `member` of class `class`.
    pub(crate) fn member(&self, class: u8, member: u8) -> (usize, usize) {
        self.columns[usize::from(class)][usize::from(member)]
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

/// The column types whose equality the model hashes and merges.
fn joinable(ctype: ColType) -> bool {
    matches!(
        ctype,
        ColType::Int2
            | ColType::Int4
            | ColType::Int8
            | ColType::Text
            | ColType::Numeric
            | ColType::Date
            | ColType::Bool
    )
}

/// The conditions one query holds.
const MAX_QUALS: usize = join_tree::MAX_QUALS;

/// The constants equated with columns, each once by value and type
/// (`em_is_const` members compare by `equal`).
struct Constants<'a> {
    held: [(&'a Expr<'a>, ColType); MAX_QUALS],
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
fn nonnullable(scope: &QueryScope<'_>, expression: &Expr<'_>, top_level: bool) -> Option<VarList> {
    let union = |left: &Expr<'_>, right: &Expr<'_>, top: bool| -> Option<VarList> {
        let mut all = nonnullable(scope, left, top)?;
        for &var in nonnullable(scope, right, top)?.as_slice() {
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
                nonnullable(scope, left, top_level)?,
                nonnullable(scope, right, top_level)?,
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
        } => nonnullable(scope, operand, false),
        Expr::IsNull { operand, negated } => {
            if *negated && top_level {
                nonnullable(scope, operand, false)
            } else {
                Some(VarList::default())
            }
        }
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

/// The columns `expression` reads, or `None` when it reads something other
/// than the query's base columns.
fn qual_vars(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<VarList> {
    let mut vars = VarList::default();
    let mut ok = true;
    expression.for_each_column_reference(&mut |qualifier, name| match scope
        .find_column(qualifier, name)
    {
        Ok(ResolvedColumn::Table(table, column)) if column < 64 => {
            ok &= vars.add((table as u8, column as u16)).is_some();
        }
        _ => ok = false,
    });
    let mut whole_row = false;
    let _ = super::super::walk_expression_tree(expression, &mut |node| {
        whole_row |= matches!(node, Expr::WholeRow(_));
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

/// A condition as the preparation reads it, or `None` when it is outside
/// the model.
fn analyze_qual<'a>(
    scope: &QueryScope<'a>,
    expression: &'a Expr<'a>,
    constants: &mut Constants<'a>,
) -> Option<Qual> {
    let vars = qual_vars(scope, expression)?;
    let rels: Relids = vars
        .as_slice()
        .iter()
        .fold(0, |relids, &(rel, _)| relids | 1 << rel);
    let volatile = expression.contains_volatile_function().is_some();
    let strict = nonnullable(scope, expression, true);
    let forced_null = match expression {
        Expr::IsNull {
            operand,
            negated: false,
        } => column_of(scope, operand).map(|(table, column)| (table as u8, column as u16)),
        _ => None,
    };
    let ctype = |(table, column): (usize, usize)| {
        let meta = &scope.defs.stored(table).expect("plain table").columns()[column];
        (meta.ctype, meta.collation)
    };
    let shape = match expression {
        Expr::Binary {
            operator: BinaryOp::Eq,
            left,
            right,
            ..
        } if !volatile => match (column_of(scope, left), column_of(scope, right)) {
            (Some(one), Some(two)) if one.0 != two.0 => {
                let ((left_type, left_collation), (right_type, right_collation)) =
                    (ctype(one), ctype(two));
                if left_type == right_type
                    && joinable(left_type)
                    && left_collation == right_collation
                {
                    Shape::Columns((one.0 as u8, one.1 as u16), (two.0 as u8, two.1 as u16))
                } else {
                    Shape::Other
                }
            }
            (Some(column), None) | (None, Some(column)) => {
                let value = if column_of(scope, left).is_some() {
                    right
                } else {
                    left
                };
                match plain_constant(value) {
                    Some(value) if joinable(ctype(column).0) => Shape::Constant {
                        var: (column.0 as u8, column.1 as u16),
                        constant: constants.id(value, ctype(column).0)?,
                    },
                    _ => Shape::Other,
                }
            }
            _ => Shape::Other,
        },
        _ => Shape::Other,
    };
    // A condition above one relation must be one the model estimates.
    if rels.count_ones() > 1 && shape == Shape::Other && !join_comparison(scope, expression) {
        return None;
    }
    let strict = strict?;
    Some(Qual {
        rels,
        vars,
        nonnullable: strict
            .as_slice()
            .iter()
            .fold(0, |relids, &(rel, _)| relids | 1 << rel),
        strict_vars: strict,
        forced_null,
        shape,
    })
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
    one.0 != two.0 && ctype(one) == ctype(two) && joinable(ctype(one).0)
}

/// The plan PostgreSQL's join search chooses for `statement`, when the
/// query is inside the model; `None` otherwise.
pub(crate) fn modeled_join<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Option<ModeledJoin<'a>>, SqlError> {
    let Some(from) = statement.from.as_ref() else {
        return Ok(None);
    };
    let n = scope.n;
    if !(2..=MAX_JOIN_RELS).contains(&n)
        || n != from.joins.len() + 1
        || statement.set_body.is_some()
        || !statement.with.is_empty()
        || statement.distinct
        || !statement.distinct_on.is_empty()
        || !statement.group_by.is_empty()
        || !statement.grouping_sets.is_empty()
        || statement.having.is_some()
        || !statement.order_by.is_empty()
        || statement.limit.is_some()
        || statement.offset.is_some()
        || !statement.windows.is_empty()
        || !statement.locking.is_empty()
        || scope.n_merged != 0
    {
        return Ok(None);
    }
    let methods = crate::sql::guc::planner_methods();
    let value = crate::sql::guc::planner_value;
    // Above `geqo_threshold` PostgreSQL searches genetically, and past the
    // collapse limits it plans several join problems.
    if (value("geqo") != 0.0 && n as f64 >= value("geqo_threshold"))
        || n as f64 > value("join_collapse_limit")
        || n as f64 > value("from_collapse_limit")
    {
        return Ok(None);
    }
    for (index, join) in from.joins.iter().enumerate() {
        if !matches!(
            join.kind,
            JoinKind::Inner
                | JoinKind::Cross
                | JoinKind::Comma
                | JoinKind::Left
                | JoinKind::Right
                | JoinKind::Full
        ) || join.using.is_some()
            || join.natural
            || scope.join_on[index].is_some()
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
    for item in statement.items {
        match item {
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
                }
                _ => return Ok(None),
            },
            SelectItem::RecordStar(_) => return Ok(None),
        }
    }
    // The join tree as written: each comma-separated item a chain of joins
    // (each join's range table entry numbered in written order), then the
    // `FROM` list holding the `WHERE` conditions.
    let mut tree = Tree::new(n);
    let mut items = [0u8; MAX_JOIN_RELS];
    let mut item_count = 0;
    let mut join_nodes = [None::<u8>; MAX_JOIN_RELS];
    let Some(mut current) = tree.node(TreeNode::Rel(0)) else {
        return Ok(None);
    };
    let mut rtindex = 0u8;
    for (index, join) in from.joins.iter().enumerate() {
        let table = (index + 1) as u8;
        let Some(right) = tree.node(TreeNode::Rel(table)) else {
            return Ok(None);
        };
        let (kind, numbered) = match join.kind {
            JoinKind::Comma => {
                items[item_count] = current;
                item_count += 1;
                current = right;
                continue;
            }
            JoinKind::Left => (TreeJoin::Left, true),
            JoinKind::Right => (TreeJoin::Right, true),
            JoinKind::Full => (TreeJoin::Full, true),
            _ => (TreeJoin::Inner, false),
        };
        let Some(node) = tree.node(TreeNode::Join {
            kind,
            left: current,
            right,
            rtindex: numbered.then_some(rtindex),
        }) else {
            return Ok(None);
        };
        if numbered {
            rtindex += 1;
        }
        join_nodes[index] = Some(node);
        current = node;
    }
    items[item_count] = current;
    item_count += 1;
    let Some(root) = tree.list_of(&items[..item_count]) else {
        return Ok(None);
    };
    tree.root = root;
    // The conditions: each join's `ON`, then `WHERE`.
    let mut quals = [&Expr::Null; MAX_QUALS];
    let mut constants = Constants {
        held: [(&Expr::Null, ColType::Int4); MAX_QUALS],
        count: 0,
    };
    let sources = from
        .joins
        .iter()
        .enumerate()
        .filter_map(|(index, join)| join.on.map(|on| (join_nodes[index], on)))
        .chain(
            statement
                .where_clause
                .map(|condition| (Some(root), condition)),
        );
    for (node, condition) in sources {
        let Some(node) = node else {
            return Ok(None);
        };
        let mut conjuncts = [&Expr::Null; super::super::plan::MAX_CONJUNCTS];
        let mut count = 0;
        if !super::super::plan::flatten_and(condition, &mut conjuncts, &mut count) {
            return Ok(None);
        }
        for &conjunct in &conjuncts[..count] {
            let Some(qual) = analyze_qual(scope, conjunct, &mut constants) else {
                return Ok(None);
            };
            let Some(id) = tree.qual(node, qual) else {
                return Ok(None);
            };
            quals[usize::from(id)] = conjunct;
        }
    }
    let quals = &quals[..tree.qual_count];
    let Ok(prepared) = join_tree::prepare(&tree) else {
        return Ok(None);
    };
    let classes = prepared.classes();
    let class_count = classes.len();
    // The constant a class equates its members with.
    let constant_of = |class: usize| -> Option<&'a Expr<'a>> {
        let qual = prepared.class_constant[class]?;
        let Expr::Binary { left, right, .. } = quals[usize::from(qual)] else {
            return None;
        };
        Some(if column_of(scope, left).is_some() {
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
            Placed::Implied { member, class } => {
                let Some(value) = constant_of(usize::from(class)) else {
                    return Ok(None);
                };
                (
                    usize::from(member.rel),
                    equals_constant(scope, member, value, arena)?,
                )
            }
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
    let mut clauses = [Clause {
        form: ClauseForm::Other,
        relids: 0,
        required: 0,
        pushed_down: true,
        clone: super::joins::CloneKind::None,
        incompatible: 0,
        cost: costs.cpu_operator,
        selectivity: None,
        outer_selectivity: None,
        source: None,
    }; MAX_CLAUSES];
    let mut clause_count = 0;
    // Each join clause's `outer_relids` (the side of an outer join its
    // clause cannot move into).
    let mut outer_relids = [0 as Relids; MAX_CLAUSES];
    for placed in prepared.placed() {
        let Placed::Join(join) = *placed else {
            continue;
        };
        if clause_count == MAX_CLAUSES {
            return Ok(None);
        }
        let form = match join.merge {
            Some((left, right, left_class, right_class)) => ClauseForm::Join {
                left,
                right,
                left_class,
                right_class,
            },
            None => ClauseForm::Other,
        };
        clauses[clause_count] = Clause {
            form,
            relids: join.relids,
            required: join.required,
            pushed_down: join.pushed_down,
            clone: join.clone,
            incompatible: join.incompatible,
            cost: costs.cpu_operator,
            selectivity: None,
            outer_selectivity: None,
            source: Some(u16::from(join.qual)),
        };
        outer_relids[clause_count] = join.outer_relids;
        clause_count += 1;
    }
    let placed_join_count = clause_count;
    // The outer joins that can null each relation (`nulling_relids`).
    let mut nulling_relids = [0 as Relids; MAX_JOIN_RELS];
    for special in &prepared.specials[..prepared.special_count] {
        let nullable = match special.kind {
            super::joins::SpecialKind::Left | super::joins::SpecialKind::Anti => special.syn_right,
            super::joins::SpecialKind::Full => special.syn_left | special.syn_right,
            super::joins::SpecialKind::Semi => 0,
        };
        for (table, relids) in nulling_relids.iter_mut().enumerate().take(n) {
            if nullable & 1 << table != 0 {
                *relids |= special.oj;
            }
        }
    }
    // Each relation's join clauses an index may apply: the outer joins'
    // clauses movable into it (`join_clause_is_movable_to`), and its
    // column of each class equated with every other member
    // (`generate_implied_equalities_for_column`).
    let mut scans = [JoinScan {
        params: [(0, 0); MAX_JOIN_PARAMS],
        param_count: 0,
        classes: [NO_CLASS; 64],
        rel_rows: [0.0; 8],
        total_table_pages: 0.0,
        describe: None,
    }; MAX_JOIN_RELS];
    let mut join_conjuncts: [[&'a Expr<'a>; MAX_JOIN_PARAMS]; MAX_JOIN_RELS] =
        [[&Expr::Null; MAX_JOIN_PARAMS]; MAX_JOIN_RELS];
    // The clause each parameter is: an outer join's clause, or `None` for
    // a class's.
    let mut param_clause = [[None::<u8>; MAX_JOIN_PARAMS]; MAX_JOIN_RELS];
    let mut total_table_pages = 0.0;
    let member_class = |member: Member| {
        classes
            .iter()
            .position(|class| class.members().contains(&member))
    };
    for table in 0..n {
        total_table_pages += super::relation_size(storage, scope.slots[table], txid).pages;
        let scan = &mut scans[table];
        let relids: Relids = 1 << table;
        // `pathkeys_useful_for_merging` at the relation: a class with a
        // member elsewhere, or one a mergeable clause of its `joininfo`
        // sorts by.
        let eclass_joins = classes.iter().any(|class| {
            usize::from(class.count) + usize::from(class.has_const) > 1
                && class.relids() & relids != 0
                && class.relids() & !relids != 0
        });
        for (column, slot) in scan.classes.iter_mut().enumerate().take(width(table)) {
            let Some(class) = member_class(Member::plain(table as u8, column as u16)) else {
                continue;
            };
            let held = &classes[class];
            let useful_by_class = eclass_joins
                && !held.has_const
                && held.count > 1
                && held.relids() & !relids != 0
                && held
                    .members()
                    .iter()
                    .any(|member| member.relids() & relids == 0);
            let useful_by_clause = clauses[..placed_join_count].iter().any(|clause| {
                clause.in_joininfo_of(relids)
                    && matches!(clause.form, ClauseForm::Join { left_class, right_class, .. }
                        if usize::from(left_class) == class || usize::from(right_class) == class)
            });
            if useful_by_class || useful_by_clause {
                *slot = class as u8;
            }
        }
        // The columns its btree indexes key: a join clause reading none of
        // them gives the relation no parameterized path.
        let mut keyed = 0u64;
        crate::sql::catalog::planner_indexes(storage, txid, scope.slots[table], &mut |index| {
            if index.method == crate::sql::ast::IndexAccessMethod::Btree {
                for position in 0..index.n_cols {
                    if !index.expression_keys[position] && index.columns[position] < 64 {
                        keyed |= 1 << index.columns[position];
                    }
                }
            }
        });
        let reads_key = |expression: &Expr<'_>| {
            let mut reads = false;
            expression.for_each_column_reference(&mut |qualifier, name| {
                if let Ok(ResolvedColumn::Table(at, column)) = scope.find_column(qualifier, name) {
                    reads |= at == table && column < 64 && keyed & 1 << column != 0;
                }
            });
            reads
        };
        let mut oj_params = false;
        for (id, clause) in clauses[..placed_join_count].iter().enumerate() {
            if !clause.in_joininfo_of(relids)
                || clause.relids & relids == 0
                || outer_relids[id] & relids != 0
                || clause.relids & nulling_relids[table] != 0
                || clause.clone == super::joins::CloneKind::Is
            {
                continue;
            }
            // Without an index the relation has no parameterized path; with
            // one, each path parameterized by the clause's other relations
            // also filters by every other movable clause
            // (`get_baserel_parampathinfo`), which the model applies only as
            // an index key's equality.
            if keyed == 0 {
                continue;
            }
            if !reads_key(quals[usize::from(clause.source.expect("written"))]) {
                return Ok(None);
            }
            let ClauseForm::Join {
                left,
                right,
                left_class,
                right_class,
            } = clause.form
            else {
                return Ok(None);
            };
            let (own, other, own_class) = if usize::from(left.rel) == table {
                (left, right, left_class)
            } else {
                (right, left, right_class)
            };
            if own.nulling != 0 || other.relids() & !BASE_RELIDS != 0 {
                return Ok(None);
            }
            let at = usize::from(scan.param_count);
            if at == MAX_JOIN_PARAMS
                || scan.params[..at]
                    .iter()
                    .any(|&(class, _)| class == own_class)
            {
                return Ok(None);
            }
            scan.params[at] = (own_class, other.rel);
            join_conjuncts[table][at] = quals[usize::from(clause.source.expect("written"))];
            param_clause[table][at] = Some(id as u8);
            scan.param_count += 1;
            oj_params = true;
        }
        for (class_id, class) in classes.iter().enumerate() {
            let Some(own) = class
                .members()
                .iter()
                .find(|member| usize::from(member.rel) == table && member.nulling == 0)
            else {
                continue;
            };
            if class.has_const || class.count <= 1 {
                continue;
            }
            for other in class.members() {
                if other.rel == own.rel {
                    continue;
                }
                // Clauses of outer joins and of classes moved into one scan
                // order their index conditions in ways the model leaves.
                if (oj_params && own.column < 64 && keyed & 1 << own.column != 0)
                    || other.nulling != 0
                {
                    return Ok(None);
                }
                let at = usize::from(scan.param_count);
                if at == MAX_JOIN_PARAMS {
                    return Ok(None);
                }
                scan.params[at] = (class_id as u8, other.rel);
                join_conjuncts[table][at] = equality(scope, *own, *other, arena)?;
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
    for &((rel, column), relids) in &prepared.needed[..prepared.needed_count] {
        if relids & !(1 << rel) != 0 && column < 64 {
            join_columns[usize::from(rel)] |= 1 << column;
            note(usize::from(rel), usize::from(column));
        }
    }
    // Each relation's paths (`set_base_rel_sizes`, then
    // `set_base_rel_pathlists`, whose parameterized paths read the other
    // relations' rows).
    let mut rels = [None::<ModeledRel<'a>>; MAX_JOIN_RELS];
    let mut planned_paths = [None::<super::paths::JoinRelPaths<'a>>; MAX_JOIN_RELS];
    let mut needed = [NeededColumn {
        rel: 0,
        column: 0,
        width: 0.0,
        needed_by: 0,
        output: false,
    }; 64 * 2];
    let mut needed_count = 0;
    let mut rel_rows = [0.0f64; 8];
    for pass in 0..2 {
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
                Some(read),
                params,
                arena,
                QueryShape {
                    output: Some(above),
                    member: super::Member::Join(scan),
                    ..QueryShape::default()
                },
            )?;
            let Planned::Join(paths) = planned else {
                return Ok(None);
            };
            if pass == 0 {
                rel_rows[table] = paths.rows;
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
                if needed_count == needed.len() {
                    return Ok(None);
                }
                needed[needed_count] = NeededColumn {
                    rel: table as u8,
                    column: column as u16,
                    width: f64::from(super::column_width(storage, txid, slot, column)),
                    needed_by: prepared.needed_by((table as u8, column as u16)) & !(1 << table),
                    output: output[table] & 1 << column != 0,
                };
                needed_count += 1;
            }
            rels[table] = Some(ModeledRel {
                scan,
                paths: paths.candidates,
                ..rels[table].expect("sized")
            });
        }
    }
    // The join clauses the base relations' index paths derive, in the
    // order and orientation `create_index_paths` creates them: each index's
    // key columns equated with the other members of their class, then the
    // clauses its parameterized paths move into the scan
    // (`get_baserel_parampathinfo`).
    let mut derive = |class: u8, left: u8, right: u8| -> Option<u8> {
        let found = clauses[..clause_count].iter().position(|clause| {
            clause.source.is_none()
                && matches!(clause.form, ClauseForm::Equality { class: c, left: l, right: r }
                    if c == class && ((l, r) == (left, right) || (l, r) == (right, left)))
        });
        if let Some(at) = found {
            return Some(at as u8);
        }
        if clause_count == MAX_CLAUSES {
            return None;
        }
        let members = classes[usize::from(class)].members();
        let relids = members[usize::from(left)].relids() | members[usize::from(right)].relids();
        clauses[clause_count] = Clause {
            form: ClauseForm::Equality { class, left, right },
            relids,
            required: relids,
            pushed_down: true,
            clone: super::joins::CloneKind::None,
            incompatible: 0,
            cost: costs.cpu_operator,
            selectivity: None,
            outer_selectivity: None,
            source: None,
        };
        clause_count += 1;
        Some(clause_count as u8 - 1)
    };
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
        let member_of = |class: u8| {
            classes[usize::from(class)]
                .members()
                .iter()
                .position(|member| usize::from(member.rel) == table)
        };
        // The clauses a path parameterized by `param` moves into the scan:
        // the outer joins' movable clauses, then one per class.
        let moved = |param: u8, derive: &mut dyn FnMut(u8, u8, u8) -> Option<u8>| {
            let mut list = ClauseList::EMPTY;
            let params = &rel.scan.params[..usize::from(rel.scan.param_count)];
            for (&(_, other), clause) in params.iter().zip(param_clause[table]) {
                if let Some(id) = clause
                    && param & 1 << other != 0
                {
                    list.push_id(id)?;
                }
            }
            for class in 0..class_count as u8 {
                let held = &classes[usize::from(class)];
                if held.has_const || held.count <= 1 {
                    continue;
                }
                let Some(own) = member_of(class) else {
                    continue;
                };
                let Some(outer) = held
                    .members()
                    .iter()
                    .position(|member| param & 1 << member.rel != 0)
                else {
                    continue;
                };
                list.push_id(derive(class, outer as u8, own as u8)?)?;
            }
            Some(list)
        };
        let mut enforced = [ClauseList::EMPTY; 128];
        for (index_at, index) in paths.indexes.iter().enumerate() {
            let info = &index.catalog;
            if info.method != crate::sql::ast::IndexAccessMethod::Btree || info.partial {
                continue;
            }
            for position in 0..info.n_cols {
                if info.expression_keys[position]
                    || info.pattern_keys[position]
                    || !info.column_collation[position]
                {
                    continue;
                }
                let column = usize::from(info.columns[position]);
                let class = rel.scan.classes.get(column).copied().unwrap_or(NO_CLASS);
                if class == NO_CLASS {
                    continue;
                }
                let held = &classes[usize::from(class)];
                if held.has_const || held.count <= 1 {
                    continue;
                }
                let own = member_of(class).expect("a class of the relation") as u8;
                for (other, member) in held.members().iter().enumerate() {
                    if usize::from(member.rel) != table && derive(class, own, other as u8).is_none()
                    {
                        return Ok(None);
                    }
                }
            }
            for (at, candidate) in paths.candidates.iter().enumerate() {
                if usize::from(candidate.describe.index) == index_at && candidate.param != 0 {
                    let Some(list) = moved(candidate.param, &mut derive) else {
                        return Ok(None);
                    };
                    enforced[at] = list;
                }
            }
        }
        for (at, candidate) in paths.candidates.iter().enumerate() {
            if candidate.param != 0 && candidate.describe.index == u16::MAX {
                let Some(list) = moved(candidate.param, &mut derive) else {
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
                    param: Relids::from(candidate.param),
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
                .position(|member| usize::from(member.rel) == table)
                .expect("a member of the relation") as u8;
            let other = members
                .iter()
                .position(|member| member.rel == other_rel)
                .expect("a member of the other relation") as u8;
            let reversed = clauses[..clause_count].iter().any(|clause| {
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
    let planners: [Option<Planner<'_, 'a>>; MAX_JOIN_RELS] = core::array::from_fn(|table| {
        let statistics = statistics[table].as_ref()?;
        let slot = scope.slots[table];
        let definition = scope.defs.stored(table)?;
        Some(Planner {
            variables: super::ExpressionVariables {
                first: definition.columns().len(),
                variables: &[],
            },
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
    // cost) or a comparison of two relations' columns (`DEFAULT_INEQ_SEL`).
    let mut single_rel_columns = [0u64; MAX_JOIN_RELS];
    for clause in clauses[..placed_join_count].iter_mut() {
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
            let rel = rels[table].expect("planned");
            let single = QueryScope::resolve_schema(storage, rel.from, txid, arena, None)?;
            let planner = planners[table].as_ref().expect("planned");
            match super::paths::restriction_estimate(planner, &single, expression, params, arena)? {
                Some(estimate) => estimate,
                None => return Ok(None),
            }
        } else {
            (super::DEFAULT_INEQ_SEL, costs.cpu_operator)
        };
        clause.selectivity = Some(selectivity);
        clause.outer_selectivity = Some(selectivity);
        clause.cost = cost;
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
                        Some(class) if classes[class].has_const => return Ok(None),
                        Some(class) => *held = class as u8,
                        None => {
                            // `rinfos`: the referencing relation's join clauses
                            // equating the pair, every copy.
                            *held = NO_KEY_CLASS;
                            let mut found = false;
                            for (id, clause) in clauses[..placed_join_count].iter().enumerate() {
                                let ClauseForm::Join { left, right, .. } = clause.form else {
                                    continue;
                                };
                                let pair = |a: Member, b: Member| {
                                    (a.rel, a.column) == (own.rel, own.column)
                                        && (b.rel, b.column) == (other.rel, other.column)
                                };
                                if clause.in_joininfo_of(1 << referencing)
                                    && (pair(left, right) || pair(right, left))
                                {
                                    if matched_clauses.push_id(id as u8).is_none() {
                                        return Ok(None);
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
    let reader = RelStatistics {
        planners: &planners,
        rels: &base,
    };
    let subproblems = &prepared.subproblems[..prepared.subproblem_count];
    let problem = JoinProblem {
        rels: &base[..n],
        classes,
        specials: &prepared.specials[..prepared.special_count],
        subproblems,
        clauses: &clauses[..clause_count],
        needed: &needed[..needed_count],
        foreign_keys: &foreign_keys[..foreign_count],
        settings: JoinSettings {
            costs,
            work_mem_bytes: f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0,
            hash_mem_bytes: f64::from(crate::sql::guc::work_mem_kilobytes()) * 1024.0 * 2.0,
            nestloop: methods.nestloop,
            hashjoin: methods.hashjoin,
            mergejoin: value("enable_mergejoin") != 0.0,
            material: methods.material,
            sort: methods.sort,
            memoize: value("enable_memoize") != 0.0,
        },
    };
    let Ok(plan) = super::joins::search(&problem, &reader, arena)? else {
        return Ok(None);
    };
    // A hash join planned with several batches, or one returning its
    // inner rows without a match, walks its table by the keys' hash
    // values, which only some key types have here.
    for path in plan.paths {
        if let super::joins::PathKind::HashJoin {
            hash,
            batches,
            jointype,
            ..
        } = path.kind
            && (batches > 1 || jointype.fills_inner())
            && hash.as_slice().iter().any(|&id| {
                let clause = &plan.clauses[usize::from(id)];
                let member = match clause.form {
                    ClauseForm::Equality { class, left, .. } => {
                        classes[usize::from(class)].members()[usize::from(left)]
                    }
                    ClauseForm::Join { left, .. } => left,
                    ClauseForm::Other => return true,
                };
                let ctype = scope
                    .defs
                    .stored(usize::from(member.rel))
                    .expect("plain table")
                    .columns()[usize::from(member.column)]
                .ctype;
                !matches!(
                    ctype,
                    ColType::Int2
                        | ColType::Int4
                        | ColType::Int8
                        | ColType::Date
                        | ColType::Bool
                        | ColType::Text
                )
            })
        {
            return Ok(None);
        }
    }
    let rels = arena
        .alloc_slice_with(n, |table| rels[table].expect("planned"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    let columns = arena
        .alloc_slice_with(class_count, |class| {
            let mut columns = [(0usize, 0usize); MAX_MEMBERS];
            for (at, member) in classes[class].members().iter().enumerate() {
                columns[at] = (usize::from(member.rel), usize::from(member.column));
            }
            columns
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    let quals = arena
        .alloc_slice_copy(quals)
        .map_err(|_| crate::sql::eval::arena_full())?;
    Ok(Some(ModeledJoin {
        plan,
        rels,
        columns,
        quals,
    }))
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

/// The FROM position and column a plain column reference reads.
fn column_of(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<(usize, usize)> {
    let Expr::Column { qualifier, name } = expression else {
        return None;
    };
    match scope.find_column(*qualifier, name).ok()? {
        ResolvedColumn::Table(table, column) => Some((table, column)),
        _ => None,
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
}

impl RelStatistics<'_, '_, '_> {
    fn planner(&self, member: Member) -> &Planner<'_, '_> {
        self.planners[usize::from(member.rel)]
            .as_ref()
            .expect("a member's relation has statistics")
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
        let (first, second) = (self.planner(left), self.planner(right));
        let one = first.variable(usize::from(left.column));
        let two = second.variable(usize::from(right.column));
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
        let (first, second) = (self.planner(outer), self.planner(inner));
        let one = first.variable(usize::from(outer.column));
        let two = second.variable(usize::from(inner.column));
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
        let mut done = 0u32;
        for member in members {
            if done & 1 << member.rel != 0 {
                continue;
            }
            done |= 1 << member.rel;
            let mut columns = [0usize; super::joins::MAX_KEYS];
            let mut count = 0;
            for other in members.iter().filter(|other| other.rel == member.rel) {
                columns[count] = usize::from(other.column);
                count += 1;
            }
            let (factor, default) = super::paths::relation_groups(
                self.planner(*member),
                &columns[..count],
                self.rels[usize::from(member.rel)].rows,
            );
            groups *= factor;
            used_default |= default;
        }
        if used_default {
            return Ok(calls);
        }
        Ok(groups.ceil().min(calls).max(1.0))
    }

    fn bucket(&self, member: Member, buckets: f64) -> Result<(f64, f64), SqlError> {
        let planner = self.planner(member);
        let variable = planner.variable(usize::from(member.column));
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

    fn merge_range(&self, left: Member, right: Member) -> Result<Option<[f64; 4]>, SqlError> {
        let (first, second) = (self.planner(left), self.planner(right));
        let (left_column, right_column) = (usize::from(left.column), usize::from(right.column));
        let mut range = [0.0, 1.0, 0.0, 1.0];
        let (Some((left_min, left_max)), Some((right_min, right_max))) = (
            self.range(first, left_column)?,
            self.range(second, right_column)?,
        ) else {
            return Ok(Some(range));
        };
        let default = super::DEFAULT_INEQ_SEL;
        let selectivity = |planner: &Planner<'_, '_>, column, comparison, value| {
            planner.inequality_selectivity(column, comparison, value)
        };
        let Ok(left_end) = selectivity(first, left_column, Comparison::Le, right_max)? else {
            return Ok(None);
        };
        let Ok(right_end) = selectivity(second, right_column, Comparison::Le, left_max)? else {
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
        let Ok(left_start) = selectivity(first, left_column, Comparison::Lt, right_min)? else {
            return Ok(None);
        };
        let Ok(right_start) = selectivity(second, right_column, Comparison::Lt, left_min)? else {
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
