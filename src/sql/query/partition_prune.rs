//! The members of a partitioned table or inheritance tree a scan reads.
//!
//! PostgreSQL decides this in two steps. Partition pruning (`partprune.c`)
//! compares the scan's conditions on the partition key with each level's
//! partition bounds; constraint exclusion (`relation_excluded_by_constraints`
//! with `predtest.c`'s refutation rules) then drops a member whose conditions
//! contradict each other or its own `CHECK` and `NOT NULL` constraints.
//!
//! Pruning runs at three points, each knowing more values than the one
//! before ([`PruneStage`]): planning, executor startup and every scan. The
//! planner's members are what `EXPLAIN` prints; startup pruning removes the
//! members `Subplans Removed` counts; execution reads the members a scan's
//! own values leave. Execution and `EXPLAIN` both ask [`members`], so the
//! plan names what the executor reads.

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{BinaryOp, CaseOrigin, Expr, UnaryOp};
use crate::sql::eval::{SqlError, arena_full, compare_datums_collated, sqlstate};
use crate::sql::types::{ColType, Datum, oid};
use crate::sql_err;
use crate::storage::{
    ColumnMeta, MAX_PARTITION_KEYS, PartitionBound, PartitionBoundValue, PartitionStrategy,
    Storage,
};
use core::cmp::Ordering;

/// When a pruning value is known, in PostgreSQL's order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum PruneStage {
    /// Planning (`PARTTARGET_PLANNER`): constants, folded immutable
    /// expressions and a custom plan's parameters.
    Plan,
    /// Executor startup (`PARTTARGET_INITIAL`): also a generic plan's
    /// parameters, stable functions and stable cross-type operators.
    Startup,
    /// Each scan (`PARTTARGET_EXEC`): also the values of enclosing queries
    /// and subplans.
    Scan,
}

/// The query a pruned scan belongs to: how its expressions name the scanned
/// relation and what their values are.
pub(crate) trait PruneValues<'a> {
    /// The root relation's column `expression` reads, when it is one.
    fn column(&self, expression: &'a Expr<'a>) -> Option<usize>;
    /// The first stage that knows `value`, or `None` when no stage does: it
    /// reads the query's own relations or calls a volatile function.
    fn stage(&self, value: &'a Expr<'a>) -> Option<PruneStage>;
    /// `value`, which [`Self::stage`] placed at or before the current stage.
    fn evaluate(&self, value: &'a Expr<'a>) -> Result<Datum<'a>, SqlError>;
    /// Whether `value`, which no stage knows here, is one PostgreSQL knows
    /// during each scan: a subplan's result or an enclosing query's column.
    fn scan_value(&self, value: &'a Expr<'a>) -> bool;
}

/// The members a scan of a relation reads, in PostgreSQL's order.
#[derive(Clone, Copy)]
pub(crate) struct Members<'a> {
    /// The leaf relations scanned, as storage slots.
    pub(crate) leaves: &'a [usize],
    /// Range table entries the relation's expansion added: every member
    /// pruning kept, partitioned or excluded by its constraints too.
    pub(crate) expanded: usize,
    /// Whether PostgreSQL prunes the members again during each scan, by a
    /// key compared with a subplan's result or an enclosing query's column.
    pub(crate) scan_pruned: bool,
}

/// Whether `slot` is a relation whose scan reads other members: a
/// partitioned table, or an inheritance parent with visible children.
pub(crate) fn expands(storage: &Storage, slot: usize, txid: u32) -> bool {
    storage.table_def(slot, txid).partition.is_partitioned()
        || storage.relation_has_descendants(slot, txid)
}

/// The members a scan of `root` reads at `stage` under `conditions` (its
/// restriction conjuncts). Values `stage` does not yet know prune nothing.
pub(crate) fn members<'a>(
    storage: &Storage,
    txid: u32,
    root: usize,
    conditions: &[&'a Expr<'a>],
    values: &dyn PruneValues<'a>,
    stage: PruneStage,
    arena: &'a Arena,
) -> Result<Members<'a>, SqlError> {
    let (partitioned, root_columns) = {
        let definition = storage.table_def(root, txid);
        (
            definition.partition.is_partitioned(),
            arena
                .alloc_slice_copy(definition.columns())
                .map_err(|_| arena_full())?,
        )
    };
    let context = Context {
        storage,
        txid,
        root: &*root_columns,
        values,
        stage,
        arena,
        scan_pruned: core::cell::Cell::new(false),
    };
    let mut bound = ArenaList::new(arena);
    for condition in conditions {
        bound
            .push(context.bind(condition)?)
            .map_err(|_| arena_full())?;
    }
    let clauses = bound.as_slice();
    let mut leaves = ArenaList::new(arena);
    let mut expanded = 0usize;
    if partitioned {
        context.expand_partitioned(root, clauses, &[], &mut leaves, &mut expanded)?;
    } else {
        // An inheritance parent and its children, each scanned on its own;
        // the parent reads only its own rows.
        let ordered = crate::sql::exec::relation_leaves_in_order(storage, root, txid, arena)?;
        for &member in ordered {
            expanded += 1;
            if !context.excluded(member, clauses)? {
                leaves.push(member).map_err(|_| arena_full())?;
            }
        }
    }
    Ok(Members {
        leaves: leaves.as_slice(),
        expanded,
        scan_pruned: context.scan_pruned.get(),
    })
}

struct Context<'c, 'a> {
    storage: &'c Storage,
    txid: u32,
    /// The scanned relation's columns, which conditions name by position.
    root: &'a [ColumnMeta],
    values: &'c dyn PruneValues<'a>,
    stage: PruneStage,
    arena: &'a Arena,
    /// Set when a key is compared with a value only a scan knows.
    scan_pruned: core::cell::Cell<bool>,
}

/// A comparison operator in btree strategy terms, with `<>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
    Ne,
}

impl Cmp {
    fn of(operator: BinaryOp) -> Option<Self> {
        Some(match operator {
            BinaryOp::Lt => Self::Lt,
            BinaryOp::LtEq => Self::Le,
            BinaryOp::Eq => Self::Eq,
            BinaryOp::GtEq => Self::Ge,
            BinaryOp::Gt => Self::Gt,
            BinaryOp::NotEq => Self::Ne,
            _ => return None,
        })
    }

    const fn commuted(self) -> Self {
        match self {
            Self::Lt => Self::Gt,
            Self::Le => Self::Ge,
            Self::Ge => Self::Le,
            Self::Gt => Self::Lt,
            other => other,
        }
    }

    const fn negated(self) -> Self {
        match self {
            Self::Lt => Self::Ge,
            Self::Le => Self::Gt,
            Self::Eq => Self::Ne,
            Self::Ge => Self::Lt,
            Self::Gt => Self::Le,
            Self::Ne => Self::Eq,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Lt => 0,
            Self::Le => 1,
            Self::Eq => 2,
            Self::Ge => 3,
            Self::Gt => 4,
            Self::Ne => 5,
        }
    }

    /// Whether `ordering` (`left` compared with `right`) satisfies
    /// `left self right`.
    fn holds(self, ordering: Ordering) -> bool {
        match self {
            Self::Lt => ordering.is_lt(),
            Self::Le => ordering.is_le(),
            Self::Eq => ordering.is_eq(),
            Self::Ge => ordering.is_ge(),
            Self::Gt => ordering.is_gt(),
            Self::Ne => ordering.is_ne(),
        }
    }
}

/// A value a condition compares with, as the current stage knows it.
#[derive(Clone, Copy)]
struct Value<'a> {
    expression: Option<&'a Expr<'a>>,
    stage: PruneStage,
    /// The value, when the current stage knows it.
    datum: Option<Datum<'a>>,
    /// Its type, after domains; `None` for an unknown literal coerced to
    /// the column it is compared with.
    type_oid: Option<i32>,
}

impl<'a> Value<'a> {
    /// A planner constant (`IsA(expr, Const)`).
    fn constant(self) -> Option<Datum<'a>> {
        self.datum.filter(|_| self.stage == PruneStage::Plan)
    }
}

#[derive(Clone, Copy)]
enum Operand<'a> {
    /// A column of the scanned relation, by its root position.
    Column(usize),
    /// A value that reads none of the query's relations.
    Value(Value<'a>),
    /// Any other expression, and whether it calls a non-immutable function.
    Other(&'a Expr<'a>, bool),
}

impl Operand<'_> {
    /// Whether two operands are the same expression (`equal()`).
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Column(left), Self::Column(right)) => left == right,
            (Self::Other(left, _), Self::Other(right, _)) => left == right,
            (Self::Value(left), Self::Value(right)) => match (left.expression, right.expression)
            {
                (Some(left), Some(right)) => left == right,
                _ => false,
            },
            _ => false,
        }
    }
}

/// A restriction as pruning and predicate proofs read it, after
/// PostgreSQL's planner pushed `NOT` down (`negate_clause`).
#[derive(Clone, Copy)]
enum Clause<'a> {
    And(&'a [Clause<'a>]),
    Or(&'a [Clause<'a>]),
    Compare {
        left: Operand<'a>,
        op: Cmp,
        right: Operand<'a>,
    },
    /// `operand op ANY (items)`, or `ALL` when `!any`.
    Array {
        operand: Operand<'a>,
        op: Cmp,
        any: bool,
        items: &'a [Operand<'a>],
    },
    NullTest {
        operand: Operand<'a>,
        not_null: bool,
    },
    /// A constant condition: true, false or null.
    Constant(Option<bool>),
    /// A condition neither pruning nor the proofs read; `mutable` when it
    /// calls a non-immutable function.
    Opaque {
        mutable: bool,
    },
}

impl<'a> Context<'_, 'a> {
    fn bind(&self, expression: &'a Expr<'a>) -> Result<Clause<'a>, SqlError> {
        self.bind_signed(expression, false)
    }

    /// `expression`, negated when `negate`, as `negate_clause` pushes `NOT`
    /// into comparisons, null tests and `AND`/`OR`.
    fn bind_signed(&self, expression: &'a Expr<'a>, negate: bool) -> Result<Clause<'a>, SqlError> {
        match expression {
            Expr::Binary {
                operator: operator @ (BinaryOp::And | BinaryOp::Or),
                left,
                right,
                ..
            } => {
                let and = (*operator == BinaryOp::And) != negate;
                let mut items = ArenaList::new(self.arena);
                self.flatten(left, and, negate, &mut items)?;
                self.flatten(right, and, negate, &mut items)?;
                let items = items.as_slice();
                Ok(if and {
                    Clause::And(items)
                } else {
                    Clause::Or(items)
                })
            }
            Expr::Unary {
                operator: UnaryOp::Not,
                operand,
                ..
            } => self.bind_signed(operand, !negate),
            Expr::Bool(value) => Ok(Clause::Constant(Some(*value != negate))),
            Expr::Null => Ok(Clause::Constant(None)),
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } if Cmp::of(*operator).is_some() => {
                let op = Cmp::of(*operator).expect("checked");
                let op = if negate { op.negated() } else { op };
                let left_operand = self.operand(left, Some(right))?;
                let right_operand = self.operand(right, Some(left))?;
                Ok(Clause::Compare {
                    left: left_operand,
                    op,
                    right: right_operand,
                })
            }
            Expr::IsNull { operand, negated } => Ok(Clause::NullTest {
                operand: self.operand(operand, None)?,
                not_null: *negated != negate,
            }),
            Expr::InList {
                operand,
                list,
                negated,
            } => {
                let column = self.operand(operand, None)?;
                let items = self
                    .arena
                    .alloc_slice_with(list.len(), |_| Operand::Other(&Expr::Null, false))
                    .map_err(|_| arena_full())?;
                for (slot, item) in items.iter_mut().zip(list.iter()) {
                    *slot = self.operand(item, Some(operand))?;
                }
                let any = *negated == negate;
                Ok(Clause::Array {
                    operand: column,
                    op: if any { Cmp::Eq } else { Cmp::Ne },
                    any,
                    items,
                })
            }
            Expr::AnyAll {
                operand,
                operator,
                array: Expr::Array(list),
                all,
            } if Cmp::of(*operator).is_some() => {
                let column = self.operand(operand, None)?;
                let items = self
                    .arena
                    .alloc_slice_with(list.len(), |_| Operand::Other(&Expr::Null, false))
                    .map_err(|_| arena_full())?;
                for (slot, item) in items.iter_mut().zip(list.iter()) {
                    *slot = self.operand(item, Some(operand))?;
                }
                let op = Cmp::of(*operator).expect("checked");
                Ok(Clause::Array {
                    operand: column,
                    op: if negate { op.negated() } else { op },
                    any: *all == negate,
                    items,
                })
            }
            Expr::Case {
                origin: CaseOrigin::BooleanTest { value, negated },
                whens,
                ..
            } => {
                // `b IS [NOT] TRUE` / `b IS [NOT] FALSE` of a boolean column.
                let Some(Operand::Column(column)) = whens
                    .first()
                    .map(|(condition, _)| match condition {
                        Expr::Unary {
                            operator: UnaryOp::Not,
                            operand,
                            ..
                        } => *operand,
                        other => *other,
                    })
                    .map(|tested| self.operand(tested, None))
                    .transpose()?
                else {
                    return Ok(self.opaque(expression));
                };
                let negated = *negated != negate;
                let equals = |value: bool| Clause::Compare {
                    left: Operand::Column(column),
                    op: Cmp::Eq,
                    right: Operand::Value(Value {
                        expression: None,
                        stage: PruneStage::Plan,
                        datum: Some(Datum::Bool(value)),
                        type_oid: Some(oid::BOOL),
                    }),
                };
                if !negated {
                    return Ok(equals(*value));
                }
                let items = self
                    .arena
                    .alloc_slice_copy(&[
                        equals(!*value),
                        Clause::NullTest {
                            operand: Operand::Column(column),
                            not_null: false,
                        },
                    ])
                    .map_err(|_| arena_full())?;
                Ok(Clause::Or(items))
            }
            Expr::Case {
                origin: CaseOrigin::UnknownTest { negated },
                whens,
                ..
            } => match whens.first() {
                Some((Expr::IsNull { operand, .. }, _)) => Ok(Clause::NullTest {
                    operand: self.operand(operand, None)?,
                    not_null: *negated != negate,
                }),
                _ => Ok(self.opaque(expression)),
            },
            _ => {
                // A boolean column read as a condition: `b` is `b = true`.
                if let Some(column) = self.values.column(expression)
                    && self.root[column].ctype == ColType::Bool
                {
                    return Ok(Clause::Compare {
                        left: Operand::Column(column),
                        op: Cmp::Eq,
                        right: Operand::Value(Value {
                            expression: None,
                            stage: PruneStage::Plan,
                            datum: Some(Datum::Bool(!negate)),
                            type_oid: Some(oid::BOOL),
                        }),
                    });
                }
                Ok(self.opaque(expression))
            }
        }
    }

    fn opaque(&self, expression: &'a Expr<'a>) -> Clause<'a> {
        Clause::Opaque {
            mutable: self.calls_mutable(expression),
        }
    }

    /// `contain_mutable_functions` of an expression.
    fn calls_mutable(&self, expression: &Expr<'_>) -> bool {
        expression
            .find_function(&|name, arity| {
                crate::sql::ast::is_nonimmutable_call(name, arity)
                    || self
                        .storage
                        .has_nonimmutable_function_routine_candidate(name, arity, self.txid)
            })
            .is_some()
            || matches!(expression, Expr::Param(index) if !crate::sql::eval::custom_plan_binds(*index))
    }

    /// Adds `expression`'s terms to `items`, flattening nested `AND`s (or
    /// `OR`s) as `eval_const_expressions` does.
    fn flatten(
        &self,
        expression: &'a Expr<'a>,
        and: bool,
        negate: bool,
        items: &mut ArenaList<'a, Clause<'a>>,
    ) -> Result<(), SqlError> {
        let clause = self.bind_signed(expression, negate)?;
        match clause {
            Clause::And(nested) if and => {
                for item in nested {
                    items.push(*item).map_err(|_| arena_full())?;
                }
            }
            Clause::Or(nested) if !and => {
                for item in nested {
                    items.push(*item).map_err(|_| arena_full())?;
                }
            }
            other => items.push(other).map_err(|_| arena_full())?,
        }
        Ok(())
    }

    /// `expression` as an operand; `other` is the expression it is compared
    /// with, whose column type an unknown literal takes.
    fn operand(
        &self,
        expression: &'a Expr<'a>,
        other: Option<&'a Expr<'a>>,
    ) -> Result<Operand<'a>, SqlError> {
        if let Some(column) = self.values.column(expression) {
            return Ok(Operand::Column(column));
        }
        let Some(stage) = self.values.stage(expression) else {
            return Ok(Operand::Other(expression, self.calls_mutable(expression)));
        };
        let declared = crate::sql::eval::expression_type_identity(
            expression,
            &crate::sql::eval::NoColumns,
            &crate::sql::eval::NO_HOOKS,
        )?
        .known_oid()
        .filter(|type_oid| *type_oid != oid::UNKNOWN)
        .map(|type_oid| crate::sql::catalog::domain_root_type_oid(self.storage, self.txid, type_oid));
        let target = other
            .and_then(|other| self.values.column(other))
            .map(|column| &self.root[column]);
        let datum = if stage <= self.stage {
            let raw = self.values.evaluate(expression)?;
            Some(match (declared, target) {
                // An unknown literal takes the type of the column it is
                // compared with, as the parser coerces it.
                (None, Some(column)) if !raw.is_null() => crate::sql::exec::coerce(
                    raw,
                    column,
                    self.storage,
                    self.txid,
                    self.arena,
                    None,
                )?,
                _ => raw,
            })
        } else {
            None
        };
        let type_oid = match (declared, target, datum) {
            (Some(declared), _, _) => Some(declared),
            (None, Some(column), _) => column.ctype.try_oid(),
            (None, None, Some(datum)) if !datum.is_null() => Some(datum.type_oid()),
            (None, None, _) => None,
        };
        Ok(Operand::Value(Value {
            expression: Some(expression),
            stage,
            datum,
            type_oid,
        }))
    }
}

/// Whether an earlier key's clause with `prefix` can lead a lookup key for
/// a clause with `op`: an equality always, `<=` before `<` or `<=`, and
/// `>=` before `>` or `>=`.
fn prefix_usable(op: Cmp, prefix: Cmp) -> bool {
    prefix == Cmp::Eq
        || (matches!(op, Cmp::Lt | Cmp::Le) && prefix == Cmp::Le)
        || (matches!(op, Cmp::Gt | Cmp::Ge) && prefix == Cmp::Ge)
}

/// The btree operator family of a type, which decides whether a comparison
/// of a partition key with a value of another type uses the key's ordering.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Integer,
    Float,
    Text,
    Datetime,
    Exact(i32),
}

fn btree_family(type_oid: i32) -> Family {
    match type_oid {
        oid::INT2 | oid::INT4 | oid::INT8 => Family::Integer,
        oid::FLOAT4 | oid::FLOAT8 => Family::Float,
        oid::TEXT | oid::VARCHAR => Family::Text,
        oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ => Family::Datetime,
        other => Family::Exact(other),
    }
}

/// The stage from which comparing `key` with `value` by a btree operator
/// is allowed, if their types share an operator family: cross-type
/// comparisons with `timestamptz` are only stable.
fn operator_stage(key: i32, value: i32, hash: bool) -> Option<PruneStage> {
    let family = btree_family(key);
    if family != btree_family(value) {
        return None;
    }
    if family == Family::Datetime && key != value {
        // Hash operator families keep each datetime type apart.
        if hash {
            return None;
        }
        if key == oid::TIMESTAMPTZ || value == oid::TIMESTAMPTZ {
            return Some(PruneStage::Startup);
        }
    }
    Some(PruneStage::Plan)
}

/// One partitioned table's bounds, as `PartitionBoundInfo` orders them.
struct Level<'a> {
    strategy: PartitionStrategy,
    n_keys: usize,
    /// Each key's root column position.
    keys: [usize; MAX_PARTITION_KEYS],
    key_columns: &'a [ColumnMeta],
    /// The partitions in canonical order, as storage slots.
    children: &'a [usize],
    bounds: &'a [PartitionBound],
    /// LIST: every non-null value, sorted, with its partition.
    list: &'a [(Datum<'a>, usize)],
    /// RANGE: the distinct bounds, sorted, by partition and side.
    range: &'a [(usize, bool)],
    /// Per bound offset, its partition or -1 (`indexes`).
    indexes: &'a [i32],
    null_index: Option<usize>,
    default_index: Option<usize>,
    /// The table's own partition constraint, which pruning adds to the
    /// conditions when it has a default partition.
    qual: &'a [Clause<'a>],
}

/// `PruneStepResult`: the bound offsets kept, and the special partitions.
#[derive(Clone, Copy)]
struct StepResult<'a> {
    offsets: &'a [bool],
    scan_null: bool,
    scan_default: bool,
}

/// What a list of clauses yields: no usable steps, a result, or a proof that
/// nothing matches.
#[derive(Clone, Copy)]
enum Steps<'a> {
    None,
    Result(StepResult<'a>),
    Contradictory,
}

/// A clause matched to a partition key (`PartClauseInfo`).
#[derive(Clone, Copy)]
struct KeyClause<'a> {
    key: usize,
    op: Cmp,
    /// A `<>` clause of a LIST key (`op_is_ne`).
    not_equal: bool,
    value: Datum<'a>,
}

/// `match_clause_to_partition_key`'s outcomes.
enum Matched<'a> {
    Clause(KeyClause<'a>),
    Nullness { not_null: bool },
    Steps(Steps<'a>),
    NoMatch,
    Unsupported,
}

impl<'a> Context<'_, 'a> {
    fn expand_partitioned(
        &self,
        table: usize,
        clauses: &[Clause<'a>],
        qual: &'a [Clause<'a>],
        leaves: &mut ArenaList<'a, usize>,
        expanded: &mut usize,
    ) -> Result<(), SqlError> {
        let mut level = self.level(table)?;
        if level.default_index.is_some() {
            level.qual = qual;
        }
        let live = self.prune_level(&level, clauses)?;
        for (position, &child) in level.children.iter().enumerate() {
            if !live[position] {
                continue;
            }
            *expanded += 1;
            if self
                .storage
                .table_def(child, self.txid)
                .partition
                .is_partitioned()
            {
                let qual = self.partition_qual(&level, position, qual)?;
                self.expand_partitioned(child, clauses, qual, leaves, expanded)?;
            } else if !self.excluded(child, clauses)? {
                leaves.push(child).map_err(|_| arena_full())?;
            }
        }
        Ok(())
    }

    fn level(&self, table: usize) -> Result<Level<'a>, SqlError> {
        let (scheme, key_columns) = {
            let definition = self.storage.table_def(table, self.txid);
            let scheme = definition.partition.scheme.ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "partition pruning reached a table that is not partitioned"
                )
            })?;
            let columns = definition.columns();
            let key_columns = self
                .arena
                .alloc_slice_with(usize::from(scheme.n_keys), |index| {
                    columns[usize::from(scheme.keys[index])]
                })
                .map_err(|_| arena_full())?;
            (scheme, &*key_columns)
        };
        let n_keys = usize::from(scheme.n_keys);
        let mut keys = [0usize; MAX_PARTITION_KEYS];
        for (index, column) in key_columns.iter().enumerate() {
            keys[index] = self
                .root
                .iter()
                .position(|candidate| candidate.name.as_str() == column.name.as_str())
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "partition key \"{}\" is not a column of the partitioned table",
                        column.name.as_str()
                    )
                })?;
        }
        let mut found = ArenaList::new(self.arena);
        for child in 0..self.storage.table_count() {
            if self
                .storage
                .relation_visible_to_current_session(child, self.txid)
                && self
                    .storage
                    .table_def(child, self.txid)
                    .partition
                    .attachment
                    .is_some_and(|attachment| {
                        usize::from(attachment.parent) == table && attachment.state.routable()
                    })
            {
                found.push(child).map_err(|_| arena_full())?;
            }
        }
        let children = found.into_mut_slice().map_err(|_| arena_full())?;
        let mut failed = None;
        children.sort_unstable_by(|&left, &right| {
            crate::sql::exec::partition_bound_order(self.storage, left, right, self.txid)
                .unwrap_or_else(|error| {
                    failed.get_or_insert(error);
                    Ordering::Equal
                })
        });
        if let Some(error) = failed {
            return Err(error);
        }
        let children: &'a [usize] = &*children;
        let bounds = self
            .arena
            .alloc_slice_with(children.len(), |_| PartitionBound::Default)
            .map_err(|_| arena_full())?;
        for (slot, &child) in bounds.iter_mut().zip(children) {
            *slot = self
                .storage
                .table_def(child, self.txid)
                .partition
                .attachment
                .map_or(PartitionBound::Default, |attachment| attachment.bound);
        }
        let bounds: &'a [PartitionBound] = &*bounds;
        let mut level = Level {
            strategy: scheme.strategy,
            n_keys,
            keys,
            key_columns,
            children,
            bounds,
            list: &[],
            range: &[],
            indexes: &[],
            null_index: None,
            default_index: bounds
                .iter()
                .position(|bound| matches!(bound, PartitionBound::Default)),
            qual: &[],
        };
        match scheme.strategy {
            PartitionStrategy::List => self.list_bounds(&mut level)?,
            PartitionStrategy::Range => self.range_bounds(&mut level)?,
            PartitionStrategy::Hash => self.hash_bounds(&mut level)?,
        }
        Ok(level)
    }

    fn compare_key(
        &self,
        level: &Level<'a>,
        key: usize,
        left: &Datum<'_>,
        right: &Datum<'_>,
    ) -> Result<Ordering, SqlError> {
        compare_datums_collated(
            self.storage,
            self.txid,
            level.key_columns[key].collation,
            left,
            right,
        )
    }

    /// `create_list_bounds`.
    fn list_bounds(&self, level: &mut Level<'a>) -> Result<(), SqlError> {
        let mut values = ArenaList::new(self.arena);
        for (position, bound) in level.bounds.iter().enumerate() {
            if let PartitionBound::List {
                values: list,
                n_values,
            } = bound
            {
                for value in &list[..usize::from(*n_values)] {
                    let datum = value.as_datum();
                    if datum.is_null() {
                        level.null_index = Some(position);
                    } else {
                        values.push((datum, position)).map_err(|_| arena_full())?;
                    }
                }
            }
        }
        let values: &'a mut [(Datum<'a>, usize)] = values.into_mut_slice().map_err(|_| arena_full())?;
        let mut failed = None;
        let collation = level.key_columns[0].collation;
        values.sort_unstable_by(|left, right| {
            compare_datums_collated(self.storage, self.txid, collation, &left.0, &right.0)
                .unwrap_or_else(|error| {
                    failed.get_or_insert(error);
                    Ordering::Equal
                })
        });
        if let Some(error) = failed {
            return Err(error);
        }
        let indexes = self
            .arena
            .alloc_slice_with(values.len(), |index| values[index].1 as i32)
            .map_err(|_| arena_full())?;
        level.list = &*values;
        level.indexes = &*indexes;
        Ok(())
    }

    /// The bound tuple of `child`'s lower or upper side.
    fn range_tuple(level: &Level<'a>, child: usize, lower: bool) -> &'a [PartitionBoundValue] {
        match &level.bounds[child] {
            PartitionBound::Range {
                lower: low,
                upper,
                n_keys,
            } => {
                let tuple = if lower { low } else { upper };
                &tuple[..usize::from(*n_keys)]
            }
            _ => &[],
        }
    }

    /// `partition_rbound_cmp`, which orders an exclusive (upper) bound
    /// before an equal inclusive (lower) one.
    fn compare_bounds(
        &self,
        level: &Level<'a>,
        left: (usize, bool),
        right: (usize, bool),
    ) -> Result<Ordering, SqlError> {
        let (first, second) = (
            Self::range_tuple(level, left.0, left.1),
            Self::range_tuple(level, right.0, right.1),
        );
        let mut ordering = Ordering::Equal;
        for key in 0..level.n_keys {
            let kind = |value: &PartitionBoundValue| match value {
                PartitionBoundValue::MinValue => 0,
                PartitionBoundValue::Value(_) => 1,
                PartitionBoundValue::MaxValue => 2,
            };
            let (a, b) = (&first[key], &second[key]);
            if kind(a) != kind(b) {
                return Ok(kind(a).cmp(&kind(b)));
            }
            let (PartitionBoundValue::Value(a), PartitionBoundValue::Value(b)) = (a, b) else {
                break;
            };
            ordering = self.compare_key(level, key, &a.as_datum(), &b.as_datum())?;
            if !ordering.is_eq() {
                return Ok(ordering);
            }
        }
        if ordering.is_eq() && left.1 != right.1 {
            ordering = if left.1 {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }
        Ok(ordering)
    }

    /// Whether two bound tuples are equal (`create_range_bounds`' distinct
    /// test: kinds, then values up to the first infinite one).
    fn same_tuple(
        &self,
        level: &Level<'a>,
        left: (usize, bool),
        right: (usize, bool),
    ) -> Result<bool, SqlError> {
        let (first, second) = (
            Self::range_tuple(level, left.0, left.1),
            Self::range_tuple(level, right.0, right.1),
        );
        for key in 0..level.n_keys {
            match (&first[key], &second[key]) {
                (PartitionBoundValue::Value(a), PartitionBoundValue::Value(b)) => {
                    if !self
                        .compare_key(level, key, &a.as_datum(), &b.as_datum())?
                        .is_eq()
                    {
                        return Ok(false);
                    }
                }
                (PartitionBoundValue::MinValue, PartitionBoundValue::MinValue)
                | (PartitionBoundValue::MaxValue, PartitionBoundValue::MaxValue) => {
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// `create_range_bounds`.
    fn range_bounds(&self, level: &mut Level<'a>) -> Result<(), SqlError> {
        let mut all = ArenaList::new(self.arena);
        for (position, bound) in level.bounds.iter().enumerate() {
            if matches!(bound, PartitionBound::Range { .. }) {
                all.push((position, true)).map_err(|_| arena_full())?;
                all.push((position, false)).map_err(|_| arena_full())?;
            }
        }
        let all = all.into_mut_slice().map_err(|_| arena_full())?;
        let mut failed = None;
        {
            let level: &Level<'a> = level;
            all.sort_unstable_by(|left, right| {
                self.compare_bounds(level, *left, *right)
                    .unwrap_or_else(|error| {
                        failed.get_or_insert(error);
                        Ordering::Equal
                    })
            });
        }
        if let Some(error) = failed {
            return Err(error);
        }
        let mut distinct = ArenaList::new(self.arena);
        let mut previous: Option<(usize, bool)> = None;
        for &bound in all.iter() {
            let repeated = match previous {
                Some(previous) => self.same_tuple(level, previous, bound)?,
                None => false,
            };
            if !repeated {
                distinct.push(bound).map_err(|_| arena_full())?;
            }
            previous = Some(bound);
        }
        let range = distinct.as_slice();
        let indexes = self
            .arena
            .alloc_slice_with(range.len() + 1, |offset| match range.get(offset) {
                Some(&(child, false)) => child as i32,
                _ => -1,
            })
            .map_err(|_| arena_full())?;
        level.range = range;
        level.indexes = &*indexes;
        Ok(())
    }

    /// `create_hash_bounds`.
    fn hash_bounds(&self, level: &mut Level<'a>) -> Result<(), SqlError> {
        let greatest = level
            .bounds
            .iter()
            .filter_map(|bound| match bound {
                PartitionBound::Hash { modulus, .. } => Some(*modulus as usize),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        let indexes = self
            .arena
            .alloc_slice_with(greatest, |_| -1i32)
            .map_err(|_| arena_full())?;
        for (position, bound) in level.bounds.iter().enumerate() {
            if let PartitionBound::Hash { modulus, remainder } = bound {
                let mut remainder = *remainder as usize;
                while remainder < greatest {
                    indexes[remainder] = position as i32;
                    remainder += *modulus as usize;
                }
            }
        }
        level.indexes = &*indexes;
        Ok(())
    }

    fn offsets(&self, len: usize, value: bool) -> Result<&'a mut [bool], SqlError> {
        self.arena
            .alloc_slice_with(len, |_| value)
            .map_err(|_| arena_full())
    }

    /// The partitions of `level` the clauses leave (`prune_append_rel_partitions`
    /// and `get_matching_partitions`), by canonical position.
    fn prune_level(
        &self,
        level: &Level<'a>,
        clauses: &[Clause<'a>],
    ) -> Result<&'a [bool], SqlError> {
        let count = level.children.len();
        if count == 0 {
            return Ok(&[]);
        }
        if clauses.is_empty() {
            return Ok(self.offsets(count, true)?);
        }
        // A partitioned partition with a default partition also prunes by
        // its own partition constraint (`gen_partprune_steps`).
        let mut list = ArenaList::new(self.arena);
        for clause in clauses.iter().chain(level.qual) {
            list.push(*clause).map_err(|_| arena_full())?;
        }
        let steps = self.steps(level, list.as_slice())?;
        let live = self.offsets(count, false)?;
        let result = match steps {
            Steps::Contradictory => return Ok(live),
            Steps::None => {
                live.fill(true);
                return Ok(live);
            }
            Steps::Result(result) => result,
        };
        let mut scan_default = result.scan_default;
        for (offset, kept) in result.offsets.iter().enumerate() {
            if !kept {
                continue;
            }
            let index = level.indexes.get(offset).copied().unwrap_or(-1);
            if index < 0 {
                // An uncovered range (or a hash remainder without a
                // partition) belongs to the default partition.
                scan_default |= level.default_index.is_some();
                continue;
            }
            live[index as usize] = true;
        }
        if result.scan_null
            && let Some(index) = level.null_index
        {
            live[index] = true;
        }
        if scan_default && let Some(index) = level.default_index {
            live[index] = true;
        }
        Ok(live)
    }

    fn all_result(&self, level: &Level<'a>) -> Result<StepResult<'a>, SqlError> {
        Ok(StepResult {
            offsets: self.offsets(level.indexes.len(), true)?,
            scan_null: level.null_index.is_some(),
            scan_default: level.default_index.is_some(),
        })
    }

    fn combine(
        &self,
        level: &Level<'a>,
        results: &[StepResult<'a>],
        intersect: bool,
    ) -> Result<StepResult<'a>, SqlError> {
        let offsets = self.offsets(level.indexes.len(), intersect)?;
        let mut scan_null = intersect;
        let mut scan_default = intersect;
        for result in results {
            for (slot, kept) in offsets.iter_mut().zip(result.offsets) {
                if intersect {
                    *slot &= *kept;
                } else {
                    *slot |= *kept;
                }
            }
            if intersect {
                scan_null &= result.scan_null;
                scan_default &= result.scan_default;
            } else {
                scan_null |= result.scan_null;
                scan_default |= result.scan_default;
            }
        }
        Ok(StepResult {
            offsets,
            scan_null,
            scan_default,
        })
    }

    /// `gen_partprune_steps_internal` followed by the steps' evaluation.
    fn steps(&self, level: &Level<'a>, clauses: &[Clause<'a>]) -> Result<Steps<'a>, SqlError> {
        // Conditions that contradict the partition constraint of a
        // partitioned partition with a default partition leave nothing.
        if !level.qual.is_empty() && self.refuted_by(level.qual, clauses, false)? {
            return Ok(Steps::Contradictory);
        }
        let mut results = ArenaList::new(self.arena);
        let mut key_clauses = ArenaList::new(self.arena);
        let mut null_keys = [false; MAX_PARTITION_KEYS];
        let mut not_null_keys = [false; MAX_PARTITION_KEYS];
        let mut has_key_clause = [false; MAX_PARTITION_KEYS];
        let mut generate = false;
        for clause in clauses {
            match clause {
                Clause::Constant(Some(true)) => continue,
                Clause::Constant(_) => return Ok(Steps::Contradictory),
                Clause::Or(arms) => {
                    let mut arm_results = ArenaList::new(self.arena);
                    let mut all_contradictory = true;
                    for arm in *arms {
                        match self.steps(level, core::slice::from_ref(arm))? {
                            Steps::Contradictory => continue,
                            Steps::None => {
                                all_contradictory = false;
                                arm_results
                                    .push(self.all_result(level)?)
                                    .map_err(|_| arena_full())?;
                            }
                            Steps::Result(result) => {
                                all_contradictory = false;
                                arm_results.push(result).map_err(|_| arena_full())?;
                            }
                        }
                    }
                    if all_contradictory {
                        return Ok(Steps::Contradictory);
                    }
                    let union = self.combine(level, arm_results.as_slice(), false)?;
                    results.push(union).map_err(|_| arena_full())?;
                    continue;
                }
                Clause::And(items) => match self.steps(level, items)? {
                    Steps::Contradictory => return Ok(Steps::Contradictory),
                    Steps::None => continue,
                    Steps::Result(result) => {
                        results.push(result).map_err(|_| arena_full())?;
                        continue;
                    }
                },
                _ => {}
            }
            for key in 0..level.n_keys {
                match self.match_key(level, clause, key)? {
                    Matched::Clause(matched) => {
                        if null_keys[key] {
                            return Ok(Steps::Contradictory);
                        }
                        generate = true;
                        has_key_clause[key] = true;
                        key_clauses.push(matched).map_err(|_| arena_full())?;
                    }
                    Matched::Nullness { not_null: false } => {
                        if not_null_keys[key] || has_key_clause[key] {
                            return Ok(Steps::Contradictory);
                        }
                        null_keys[key] = true;
                    }
                    Matched::Nullness { not_null: true } => {
                        if null_keys[key] {
                            return Ok(Steps::Contradictory);
                        }
                        not_null_keys[key] = true;
                    }
                    Matched::Steps(Steps::Contradictory) => return Ok(Steps::Contradictory),
                    Matched::Steps(Steps::Result(result)) => {
                        results.push(result).map_err(|_| arena_full())?;
                    }
                    Matched::Steps(Steps::None) | Matched::Unsupported => {}
                    Matched::NoMatch => continue,
                }
                break;
            }
        }
        let n_null = null_keys[..level.n_keys].iter().filter(|null| **null).count();
        let n_not_null = not_null_keys[..level.n_keys]
            .iter()
            .filter(|not_null| **not_null)
            .count();
        if n_null != 0
            && (level.strategy != PartitionStrategy::Hash || n_null == level.n_keys)
        {
            let result = self.base_step(level, None, &[], &null_keys)?;
            results.push(result).map_err(|_| arena_full())?;
        } else if generate {
            self.op_steps(level, key_clauses.as_slice(), &null_keys, &mut results)?;
        } else if n_not_null == level.n_keys {
            let result = self.base_step(level, None, &[], &[false; MAX_PARTITION_KEYS])?;
            results.push(result).map_err(|_| arena_full())?;
        }
        Ok(match results.len() {
            0 => Steps::None,
            1 => Steps::Result(results.as_slice()[0]),
            _ => Steps::Result(self.combine(level, results.as_slice(), true)?),
        })
    }

    /// `match_clause_to_partition_key`.
    fn match_key(
        &self,
        level: &Level<'a>,
        clause: &Clause<'a>,
        key: usize,
    ) -> Result<Matched<'a>, SqlError> {
        let column = level.keys[key];
        let key_type = level.key_columns[key].ctype.try_oid().unwrap_or(oid::UNKNOWN);
        let key_type = crate::sql::catalog::domain_root_type_oid(self.storage, self.txid, key_type);
        let hash = level.strategy == PartitionStrategy::Hash;
        match clause {
            Clause::Compare { left, op, right } => {
                let (value, op) = match (left, right) {
                    (Operand::Column(found), value) if *found == column => (value, *op),
                    (value, Operand::Column(found)) if *found == column => (value, op.commuted()),
                    _ => return Ok(Matched::NoMatch),
                };
                let value = match value {
                    Operand::Value(value) => value,
                    Operand::Other(expression, _) => {
                        if self.values.scan_value(expression) {
                            self.scan_pruned.set(true);
                        }
                        return Ok(Matched::Unsupported);
                    }
                    Operand::Column(_) => return Ok(Matched::Unsupported),
                };
                // `<>` is in no operator family; a LIST key prunes with its
                // negator.
                let not_equal = op == Cmp::Ne;
                if not_equal && level.strategy != PartitionStrategy::List {
                    return Ok(if hash {
                        Matched::NoMatch
                    } else {
                        Matched::Unsupported
                    });
                }
                if hash && op != Cmp::Eq {
                    return Ok(Matched::Unsupported);
                }
                let Some(value_type) = value.type_oid else {
                    return Ok(Matched::Unsupported);
                };
                let Some(operator_stage) = operator_stage(key_type, value_type, hash) else {
                    return Ok(Matched::NoMatch);
                };
                if value.stage.max(operator_stage) > self.stage {
                    return Ok(Matched::Unsupported);
                }
                let Some(datum) = value.datum else {
                    return Ok(Matched::Unsupported);
                };
                Ok(Matched::Clause(KeyClause {
                    key,
                    op: if not_equal { Cmp::Eq } else { op },
                    not_equal,
                    value: datum,
                }))
            }
            Clause::Array {
                operand,
                op,
                any,
                items,
            } => {
                if !matches!(operand, Operand::Column(found) if *found == column) {
                    return Ok(Matched::NoMatch);
                }
                if *op == Cmp::Ne && level.strategy != PartitionStrategy::List {
                    return Ok(Matched::NoMatch);
                }
                // Each element as `key op element`, ORed for `ANY`.
                let mut elements = ArenaList::new(self.arena);
                for item in *items {
                    if let Operand::Value(value) = item
                        && value.stage == PruneStage::Plan
                        && value.datum.is_some_and(|datum| datum.is_null())
                    {
                        if *any {
                            continue;
                        }
                        return Ok(Matched::Steps(Steps::Contradictory));
                    }
                    elements
                        .push(Clause::Compare {
                            left: *operand,
                            op: *op,
                            right: *item,
                        })
                        .map_err(|_| arena_full())?;
                }
                let elements = elements.as_slice();
                let steps = if *any && elements.len() > 1 {
                    self.steps(level, &[Clause::Or(elements)])?
                } else {
                    self.steps(level, elements)?
                };
                Ok(Matched::Steps(match steps {
                    Steps::None => return Ok(Matched::Unsupported),
                    other => other,
                }))
            }
            Clause::NullTest { operand, not_null } => {
                if matches!(operand, Operand::Column(found) if *found == column) {
                    Ok(Matched::Nullness {
                        not_null: *not_null,
                    })
                } else {
                    Ok(Matched::NoMatch)
                }
            }
            _ => Ok(Matched::Unsupported),
        }
    }

    /// `gen_prune_steps_from_opexps`, evaluating each step.
    fn op_steps(
        &self,
        level: &Level<'a>,
        clauses: &[KeyClause<'a>],
        null_keys: &[bool; MAX_PARTITION_KEYS],
        results: &mut ArenaList<'a, StepResult<'a>>,
    ) -> Result<(), SqlError> {
        // The keys whose clauses count: a RANGE key without one ends them, as
        // does a key with a strict inequality after its own clauses.
        let mut last_key = level.n_keys;
        for key in 0..level.n_keys {
            let mut of_key = clauses.iter().filter(|clause| clause.key == key);
            let count = of_key.clone().count();
            if level.strategy == PartitionStrategy::Range && count == 0 {
                last_key = key;
                break;
            }
            if level.strategy == PartitionStrategy::Hash && count == 0 && !null_keys[key] {
                return Ok(());
            }
            if of_key.any(|clause| matches!(clause.op, Cmp::Lt | Cmp::Gt)) {
                last_key = key + 1;
                break;
            }
        }
        match level.strategy {
            PartitionStrategy::List | PartitionStrategy::Range => {
                let no_nulls = [false; MAX_PARTITION_KEYS];
                for op in [Cmp::Lt, Cmp::Le, Cmp::Eq, Cmp::Ge, Cmp::Gt] {
                    // The strategy's clauses in key order.
                    'strategy: for key in 0..last_key {
                        for clause in clauses
                            .iter()
                            .filter(|clause| clause.key == key && clause.op == op)
                        {
                            if key != 0
                                && !(0..key).all(|prefix| {
                                    clauses.iter().any(|candidate| {
                                        candidate.key == prefix && prefix_usable(op, candidate.op)
                                    })
                                })
                            {
                                break 'strategy;
                            }
                            self.prefix_steps(level, op, clause, clauses, &no_nulls, results)?;
                        }
                    }
                }
            }
            PartitionStrategy::Hash => {
                let Some(last) = (0..last_key)
                    .rev()
                    .find_map(|key| clauses.iter().find(|clause| clause.key == key))
                else {
                    return Ok(());
                };
                for clause in clauses.iter().filter(|clause| clause.key == last.key) {
                    self.prefix_steps(level, Cmp::Eq, clause, clauses, null_keys, results)?;
                }
            }
        }
        Ok(())
    }

    /// `get_steps_using_prefix`: one step per combination of the earlier
    /// keys' clauses, ending with `last`. A key in `null_keys` (HASH) takes
    /// no value.
    fn prefix_steps(
        &self,
        level: &Level<'a>,
        op: Cmp,
        last: &KeyClause<'a>,
        clauses: &[KeyClause<'a>],
        null_keys: &[bool; MAX_PARTITION_KEYS],
        results: &mut ArenaList<'a, StepResult<'a>>,
    ) -> Result<(), SqlError> {
        let mut values = [Datum::Null; MAX_PARTITION_KEYS];
        self.prefix_recurse(level, op, last, clauses, null_keys, 0, &mut values, 0, results)
    }

    #[allow(clippy::too_many_arguments)]
    fn prefix_recurse(
        &self,
        level: &Level<'a>,
        op: Cmp,
        last: &KeyClause<'a>,
        clauses: &[KeyClause<'a>],
        null_keys: &[bool; MAX_PARTITION_KEYS],
        key: usize,
        values: &mut [Datum<'a>; MAX_PARTITION_KEYS],
        count: usize,
        results: &mut ArenaList<'a, StepResult<'a>>,
    ) -> Result<(), SqlError> {
        if key == last.key {
            values[count] = last.value;
            let result = self.base_step(
                level,
                Some((op, last.not_equal)),
                &values[..=count],
                null_keys,
            )?;
            return results.push(result).map_err(|_| arena_full());
        }
        if null_keys[key] {
            return self.prefix_recurse(
                level,
                op,
                last,
                clauses,
                null_keys,
                key + 1,
                values,
                count,
                results,
            );
        }
        // A key's `=` clauses come before its `<=` or `>=` ones.
        for equal in [true, false] {
            for clause in clauses.iter().filter(|clause| {
                clause.key == key && (clause.op == Cmp::Eq) == equal && prefix_usable(op, clause.op)
            }) {
                values[count] = clause.value;
                self.prefix_recurse(
                    level,
                    op,
                    last,
                    clauses,
                    null_keys,
                    key + 1,
                    values,
                    count + 1,
                    results,
                )?;
            }
        }
        Ok(())
    }

    /// `perform_pruning_base_step`: the bound offsets one lookup key
    /// matches. `op` is `None` for a step of null tests only.
    fn base_step(
        &self,
        level: &Level<'a>,
        op: Option<(Cmp, bool)>,
        values: &[Datum<'a>],
        null_keys: &[bool; MAX_PARTITION_KEYS],
    ) -> Result<StepResult<'a>, SqlError> {
        // A strict operator returns null for a null value: nothing matches.
        if values.iter().any(Datum::is_null) {
            return Ok(StepResult {
                offsets: self.offsets(level.indexes.len(), false)?,
                scan_null: false,
                scan_default: false,
            });
        }
        let has_null = null_keys[..level.n_keys].iter().any(|null| *null);
        match level.strategy {
            PartitionStrategy::Hash => self.hash_step(level, values, null_keys),
            PartitionStrategy::List => self.list_step(level, op, values.first(), has_null),
            PartitionStrategy::Range => {
                self.range_step(level, op.map_or(Cmp::Eq, |(op, _)| op), values, has_null)
            }
        }
    }

    /// `get_matching_hash_bounds`.
    fn hash_step(
        &self,
        level: &Level<'a>,
        values: &[Datum<'a>],
        null_keys: &[bool; MAX_PARTITION_KEYS],
    ) -> Result<StepResult<'a>, SqlError> {
        let greatest = level.indexes.len();
        let n_null = null_keys[..level.n_keys].iter().filter(|null| **null).count();
        if values.len() + n_null != level.n_keys {
            return Ok(StepResult {
                offsets: self.offsets(greatest, true)?,
                scan_null: false,
                scan_default: false,
            });
        }
        let mut hash = 0u64;
        let mut next = values.iter();
        for null in &null_keys[..level.n_keys] {
            if *null {
                continue;
            }
            let value = next.next().copied().unwrap_or(Datum::Null);
            hash = crate::storage::partition_hash_combine(hash, value)?;
        }
        let offsets = self.offsets(greatest, false)?;
        if greatest != 0 {
            let offset = (hash % greatest as u64) as usize;
            if level.indexes[offset] >= 0 {
                offsets[offset] = true;
            }
        }
        Ok(StepResult {
            offsets,
            scan_null: false,
            scan_default: false,
        })
    }

    /// `partition_list_bsearch`: the greatest datum at or below `value`.
    fn list_search(
        &self,
        level: &Level<'a>,
        value: &Datum<'a>,
    ) -> Result<(isize, bool), SqlError> {
        let (mut low, mut high) = (-1isize, level.list.len() as isize - 1);
        let mut equal = false;
        while low < high {
            let middle = (low + high + 1) / 2;
            let ordering = self.compare_key(level, 0, &level.list[middle as usize].0, value)?;
            if ordering.is_le() {
                low = middle;
                equal = ordering.is_eq();
                if equal {
                    break;
                }
            } else {
                high = middle - 1;
            }
        }
        Ok((low, equal))
    }

    /// `get_matching_list_bounds`.
    fn list_step(
        &self,
        level: &Level<'a>,
        op: Option<(Cmp, bool)>,
        value: Option<&Datum<'a>>,
        has_null: bool,
    ) -> Result<StepResult<'a>, SqlError> {
        let count = level.list.len();
        let offsets = self.offsets(count, false)?;
        let mut result = StepResult {
            offsets: &[],
            scan_null: false,
            scan_default: false,
        };
        let has_default = level.default_index.is_some();
        if has_null {
            if level.null_index.is_some() {
                result.scan_null = true;
            } else {
                result.scan_default = has_default;
            }
            result.offsets = offsets;
            return Ok(result);
        }
        if count == 0 {
            result.scan_default = has_default;
            result.offsets = offsets;
            return Ok(result);
        }
        let (Some(value), Some((op, not_equal))) = (value, op) else {
            offsets.fill(true);
            result.offsets = offsets;
            result.scan_default = has_default;
            return Ok(result);
        };
        if not_equal {
            offsets.fill(true);
            let (offset, equal) = self.list_search(level, value)?;
            if offset >= 0 && equal {
                offsets[offset as usize] = false;
            }
            result.offsets = offsets;
            result.scan_default = has_default;
            return Ok(result);
        }
        if op != Cmp::Eq {
            result.scan_default = has_default;
        }
        let (offset, equal) = self.list_search(level, value)?;
        let (minimum, maximum) = match op {
            Cmp::Eq => {
                if offset >= 0 && equal {
                    offsets[offset as usize] = true;
                } else {
                    result.scan_default = has_default;
                }
                result.offsets = offsets;
                return Ok(result);
            }
            Cmp::Ge | Cmp::Gt => {
                let inclusive = op == Cmp::Ge;
                let start = if offset >= 0 {
                    if !equal || !inclusive {
                        offset + 1
                    } else {
                        offset
                    }
                } else {
                    0
                };
                if start > count as isize - 1 {
                    result.offsets = offsets;
                    return Ok(result);
                }
                (start, count as isize - 1)
            }
            Cmp::Le | Cmp::Lt => {
                let inclusive = op == Cmp::Le;
                let end = if offset >= 0 && equal && !inclusive {
                    offset - 1
                } else {
                    offset
                };
                if end < 0 {
                    result.offsets = offsets;
                    return Ok(result);
                }
                (0, end)
            }
            Cmp::Ne => unreachable!("a <> step carries its negator"),
        };
        for slot in &mut offsets[minimum as usize..=maximum as usize] {
            *slot = true;
        }
        result.offsets = offsets;
        Ok(result)
    }

    /// `partition_rbound_datum_cmp`: bound `bound` against the lookup key.
    fn range_datum_compare(
        &self,
        level: &Level<'a>,
        bound: (usize, bool),
        values: &[Datum<'a>],
    ) -> Result<Ordering, SqlError> {
        let tuple = Self::range_tuple(level, bound.0, bound.1);
        let mut ordering = Ordering::Less;
        for (key, value) in values.iter().enumerate() {
            match &tuple[key] {
                PartitionBoundValue::MinValue => return Ok(Ordering::Less),
                PartitionBoundValue::MaxValue => return Ok(Ordering::Greater),
                PartitionBoundValue::Value(bound) => {
                    ordering = self.compare_key(level, key, &bound.as_datum(), value)?;
                    if !ordering.is_eq() {
                        break;
                    }
                }
            }
        }
        Ok(ordering)
    }

    /// `partition_range_datum_bsearch`.
    fn range_search(
        &self,
        level: &Level<'a>,
        values: &[Datum<'a>],
    ) -> Result<(isize, bool), SqlError> {
        let (mut low, mut high) = (-1isize, level.range.len() as isize - 1);
        let mut equal = false;
        while low < high {
            let middle = (low + high + 1) / 2;
            let ordering = self.range_datum_compare(level, level.range[middle as usize], values)?;
            if ordering.is_le() {
                low = middle;
                equal = ordering.is_eq();
                if equal {
                    break;
                }
            } else {
                high = middle - 1;
            }
        }
        Ok((low, equal))
    }

    fn bound_kind(level: &Level<'a>, offset: usize, key: usize) -> u8 {
        let (child, lower) = level.range[offset];
        match Self::range_tuple(level, child, lower).get(key) {
            Some(PartitionBoundValue::MinValue) => 0,
            Some(PartitionBoundValue::MaxValue) => 2,
            _ => 1,
        }
    }

    /// `get_matching_range_bounds`.
    fn range_step(
        &self,
        level: &Level<'a>,
        op: Cmp,
        values: &[Datum<'a>],
        has_null: bool,
    ) -> Result<StepResult<'a>, SqlError> {
        let datums = level.range.len();
        let offsets = self.offsets(datums + 1, false)?;
        let has_default = level.default_index.is_some();
        let mut result = StepResult {
            offsets: &[],
            scan_null: false,
            scan_default: false,
        };
        if datums == 0 || has_null {
            result.scan_default = has_default;
            result.offsets = offsets;
            return Ok(result);
        }
        if values.is_empty() {
            result.scan_default = has_default;
            offsets.fill(true);
            result.offsets = offsets;
            return Ok(result);
        }
        let n_values = values.len();
        if n_values < level.n_keys {
            result.scan_default = has_default;
        }
        let compare = |offset: usize| self.range_datum_compare(level, level.range[offset], values);
        let (mut minimum, mut maximum) = (0isize, datums as isize);
        match op {
            Cmp::Eq => {
                let (mut offset, equal) = self.range_search(level, values)?;
                if offset >= 0 && equal {
                    if n_values == level.n_keys {
                        offsets[(offset + 1) as usize] = true;
                        result.offsets = offsets;
                        return Ok(result);
                    }
                    let saved = offset;
                    while offset >= 1 && compare((offset - 1) as usize)?.is_eq() {
                        offset -= 1;
                    }
                    if Self::bound_kind(level, offset as usize, n_values) == 0 {
                        offset += 1;
                    }
                    minimum = offset;
                    offset = saved;
                    while offset < datums as isize - 1 && compare((offset + 1) as usize)?.is_eq() {
                        offset += 1;
                    }
                    maximum = offset + 1;
                    for slot in &mut offsets[minimum as usize..=maximum as usize] {
                        *slot = true;
                    }
                } else {
                    offsets[(offset + 1) as usize] = true;
                }
                result.offsets = offsets;
                return Ok(result);
            }
            Cmp::Ge | Cmp::Gt => {
                let inclusive = op == Cmp::Ge;
                let (mut offset, equal) = self.range_search(level, values)?;
                if offset < 0 {
                    minimum = 0;
                } else if equal && n_values < level.n_keys {
                    while offset >= 1 && offset < datums as isize - 1 {
                        let next = if inclusive { offset - 1 } else { offset + 1 };
                        if !compare(next as usize)?.is_eq() {
                            break;
                        }
                        offset = next;
                    }
                    minimum = if inclusive { offset } else { offset + 1 };
                } else {
                    minimum = offset + 1;
                }
            }
            Cmp::Le | Cmp::Lt => {
                let inclusive = op == Cmp::Le;
                let (mut offset, equal) = self.range_search(level, values)?;
                if offset >= 0 {
                    if equal && n_values < level.n_keys {
                        while offset >= 1 && offset < datums as isize - 1 {
                            let next = if inclusive { offset + 1 } else { offset - 1 };
                            if !compare(next as usize)?.is_eq() {
                                break;
                            }
                            offset = next;
                        }
                        maximum = if inclusive { offset + 1 } else { offset };
                    } else if !equal || inclusive {
                        maximum = offset + 1;
                    } else {
                        maximum = offset;
                    }
                } else {
                    maximum = offset + 1;
                }
            }
            Cmp::Ne => unreachable!("RANGE keys take no <> steps"),
        }
        // A smallest partition starting at MINVALUE, or a greatest ending
        // at MAXVALUE, is used instead of the default partition.
        let last_key = n_values - 1;
        if (minimum as usize) < datums
            && level.indexes[minimum as usize] < 0
            && Self::bound_kind(level, minimum as usize, last_key) == 0
        {
            minimum += 1;
        }
        if maximum >= 1
            && level.indexes[maximum as usize] < 0
            && Self::bound_kind(level, (maximum - 1) as usize, last_key) == 2
        {
            maximum -= 1;
        }
        if minimum <= maximum {
            for slot in &mut offsets[minimum as usize..=maximum as usize] {
                *slot = true;
            }
        }
        result.offsets = offsets;
        Ok(result)
    }

    /// The partition constraint of `level`'s partition at `position` added to
    /// `inherited` (`RelationGetPartitionQual`), as the clauses pruning the
    /// partition's own partitions read it.
    fn partition_qual(
        &self,
        level: &Level<'a>,
        position: usize,
        inherited: &'a [Clause<'a>],
    ) -> Result<&'a [Clause<'a>], SqlError> {
        let mut qual = ArenaList::new(self.arena);
        for clause in inherited {
            qual.push(*clause).map_err(|_| arena_full())?;
        }
        let key_value = |key: usize, value: &crate::storage::OwnedDatum| -> Result<Operand<'a>, SqlError> {
            let datum = self
                .arena
                .alloc(*value)
                .map_err(|_| arena_full())?
                .as_datum();
            Ok(Operand::Value(Value {
                expression: None,
                stage: PruneStage::Plan,
                datum: Some(datum),
                type_oid: level.key_columns[key].ctype.try_oid(),
            }))
        };
        match &level.bounds[position] {
            PartitionBound::Range {
                lower,
                upper,
                n_keys,
            } if *n_keys == 1 => {
                let column = Operand::Column(level.keys[0]);
                qual.push(Clause::NullTest {
                    operand: column,
                    not_null: true,
                })
                .map_err(|_| arena_full())?;
                if let PartitionBoundValue::Value(value) = &lower[0] {
                    qual.push(Clause::Compare {
                        left: column,
                        op: Cmp::Ge,
                        right: key_value(0, value)?,
                    })
                    .map_err(|_| arena_full())?;
                }
                if let PartitionBoundValue::Value(value) = &upper[0] {
                    qual.push(Clause::Compare {
                        left: column,
                        op: Cmp::Lt,
                        right: key_value(0, value)?,
                    })
                    .map_err(|_| arena_full())?;
                }
            }
            PartitionBound::List { values, n_values } => {
                let column = Operand::Column(level.keys[0]);
                let mut items = ArenaList::new(self.arena);
                let mut accepts_null = false;
                for value in &values[..usize::from(*n_values)] {
                    if value.as_datum().is_null() {
                        accepts_null = true;
                    } else {
                        items.push(key_value(0, value)?).map_err(|_| arena_full())?;
                    }
                }
                let any = Clause::Array {
                    operand: column,
                    op: Cmp::Eq,
                    any: true,
                    items: items.as_slice(),
                };
                let not_null = Clause::NullTest {
                    operand: column,
                    not_null: !accepts_null,
                };
                let constraint = if accepts_null {
                    Clause::Or(
                        self.arena
                            .alloc_slice_copy(&[not_null, any])
                            .map_err(|_| arena_full())?,
                    )
                } else {
                    Clause::And(
                        self.arena
                            .alloc_slice_copy(&[not_null, any])
                            .map_err(|_| arena_full())?,
                    )
                };
                qual.push(constraint).map_err(|_| arena_full())?;
            }
            _ => {
                // A multi-column range, hash or default bound: the partition's
                // partitions are pruned without it only when no key of theirs
                // is one of this bound's keys.
                let child = level.children[position];
                let scheme = self.storage.table_def(child, self.txid).partition.scheme;
                let child_level_keys = scheme.map_or(0, |scheme| usize::from(scheme.n_keys));
                if child_level_keys != 0 {
                    let child_level = self.level(child)?;
                    if child_level.default_index.is_some()
                        && child_level.keys[..child_level.n_keys]
                            .iter()
                            .any(|key| level.keys[..level.n_keys].contains(key))
                    {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "partition pruning of a partitioned partition with a default partition by a {} partition bound on its own key is not supported",
                            match level.strategy {
                                PartitionStrategy::Range => "multi-column range",
                                PartitionStrategy::List => "default",
                                PartitionStrategy::Hash => "hash",
                            }
                        ));
                    }
                }
            }
        }
        Ok(qual.as_slice())
    }
}

/// Constraint exclusion (`relation_excluded_by_constraints`) of one scanned
/// member, under `constraint_exclusion = partition`.
impl<'a> Context<'_, 'a> {
    fn excluded(&self, member: usize, clauses: &[Clause<'a>]) -> Result<bool, SqlError> {
        if clauses.is_empty() {
            return Ok(false);
        }
        if clauses
            .iter()
            .any(|clause| matches!(clause, Clause::Constant(None | Some(false))))
        {
            return Ok(true);
        }
        // Restrictions that contradict each other, from the immutable ones.
        let mut safe = ArenaList::new(self.arena);
        for clause in clauses {
            if !Self::mutable(clause) {
                safe.push(*clause).map_err(|_| arena_full())?;
            }
        }
        if self.refuted_by(safe.as_slice(), safe.as_slice(), true)? {
            return Ok(true);
        }
        // The member's own constraints: its NOT NULL columns and validated
        // immutable CHECK constraints.
        let definition = (*self.storage.table_def(member, self.txid)).clone();
        let member_columns = self
            .arena
            .alloc_slice_copy(definition.columns())
            .map_err(|_| arena_full())?;
        let mut constraints = ArenaList::new(self.arena);
        for column in definition.columns() {
            if column.not_null.is_required()
                && let Some(position) = self.root_column(column.name.as_str())
            {
                constraints
                    .push(Clause::NullTest {
                        operand: Operand::Column(position),
                        not_null: true,
                    })
                    .map_err(|_| arena_full())?;
            }
        }
        if !definition.checks().is_empty() {
            let checks = crate::sql::exec::constraints::parse_checks(
                &definition,
                self.storage,
                self.txid,
                self.arena,
            )?;
            let source = ConstraintValues {
                context: self,
                member: member_columns,
            };
            let bound = Context {
                storage: self.storage,
                txid: self.txid,
                root: self.root,
                values: &source,
                stage: self.stage,
                arena: self.arena,
                scan_pruned: core::cell::Cell::new(false),
            };
            for (check, parsed) in definition.checks().iter().zip(checks.iter()) {
                let Some(parsed) = parsed else {
                    continue;
                };
                if !check.validation.validated() {
                    continue;
                }
                let clause = bound.bind(parsed)?;
                if !Self::mutable(&clause) {
                    constraints.push(clause).map_err(|_| arena_full())?;
                }
            }
        }
        self.refuted_by(constraints.as_slice(), clauses, false)
    }

    fn root_column(&self, name: &str) -> Option<usize> {
        self.root
            .iter()
            .position(|column| column.name.as_str() == name)
    }

    /// `contain_mutable_functions`: a value only a later stage knows, or a
    /// condition calling a non-immutable function.
    fn mutable(clause: &Clause<'a>) -> bool {
        let operand = |operand: &Operand<'a>| match operand {
            Operand::Value(value) => value.stage != PruneStage::Plan,
            Operand::Column(_) => false,
            Operand::Other(_, mutable) => *mutable,
        };
        match clause {
            Clause::And(items) | Clause::Or(items) => items.iter().any(Self::mutable),
            Clause::Compare { left, right, .. } => operand(left) || operand(right),
            Clause::Array { operand: o, items, .. } => operand(o) || items.iter().any(operand),
            Clause::NullTest { operand: o, .. } => operand(o),
            Clause::Constant(_) => false,
            Clause::Opaque { mutable } => *mutable,
        }
    }
}

/// The values a member's `CHECK` constraint reads: its own columns by name,
/// and constants.
struct ConstraintValues<'c, 'x, 'a> {
    context: &'x Context<'c, 'a>,
    member: &'a [ColumnMeta],
}

impl<'a> PruneValues<'a> for ConstraintValues<'_, '_, 'a> {
    fn column(&self, expression: &'a Expr<'a>) -> Option<usize> {
        let Expr::Column { name, .. } = expression else {
            return None;
        };
        self.member
            .iter()
            .any(|column| column.name.as_str() == *name)
            .then(|| self.context.root_column(name))
            .flatten()
    }

    fn stage(&self, value: &'a Expr<'a>) -> Option<PruneStage> {
        value.is_constant(true).then_some(PruneStage::Plan)
    }

    fn scan_value(&self, _: &'a Expr<'a>) -> bool {
        false
    }

    fn evaluate(&self, value: &'a Expr<'a>) -> Result<Datum<'a>, SqlError> {
        crate::sql::eval::eval_full(
            value,
            self.context.arena,
            crate::sql::eval::NO_PARAMS,
            &crate::sql::eval::NoColumns,
            &crate::sql::eval::NO_HOOKS,
        )
    }
}

/// PostgreSQL's operator proof tables (`predtest.c`), by `Cmp::index`.
#[rustfmt::skip]
const IMPLIES_SAME: [[bool; 6]; 6] = [
    [true, true, false, false, false, true],
    [false, true, false, false, false, false],
    [false, true, true, true, false, false],
    [false, false, false, true, false, false],
    [false, false, false, true, true, true],
    [false, false, false, false, false, true],
];
#[rustfmt::skip]
const REFUTES_SAME: [[bool; 6]; 6] = [
    [false, false, true, true, true, false],
    [false, false, false, false, true, false],
    [true, false, false, false, true, true],
    [true, false, false, false, false, false],
    [true, true, true, false, false, false],
    [false, false, true, false, false, false],
];
use Cmp::{Eq as EQ, Ge as GE, Gt as GT, Le as LE, Lt as LT, Ne as NE};
#[rustfmt::skip]
const IMPLIC_TEST: [[Option<Cmp>; 6]; 6] = [
    [Some(GE), Some(GE), None, None, None, Some(GE)],
    [Some(GT), Some(GE), None, None, None, Some(GT)],
    [Some(GT), Some(GE), Some(EQ), Some(LE), Some(LT), Some(NE)],
    [None, None, None, Some(LE), Some(LT), Some(LT)],
    [None, None, None, Some(LE), Some(LE), Some(LE)],
    [None, None, None, None, None, Some(EQ)],
];
#[rustfmt::skip]
const REFUTE_TEST: [[Option<Cmp>; 6]; 6] = [
    [None, None, Some(GE), Some(GE), Some(GE), None],
    [None, None, Some(GT), Some(GT), Some(GE), None],
    [Some(LE), Some(LT), Some(NE), Some(GT), Some(GE), Some(EQ)],
    [Some(LE), Some(LT), Some(LT), None, None, None],
    [Some(LE), Some(LE), Some(LE), None, None, None],
    [None, None, Some(EQ), None, None, None],
];

/// How `predtest.c` walks a clause: an `AND`, an `OR`, or an atom.
#[derive(Clone, Copy)]
enum Class<'a> {
    And(Items<'a>),
    Or(Items<'a>),
    Atom,
}

/// The items of an `AND` or `OR`: clauses, or the elements of a short
/// array comparison.
#[derive(Clone, Copy)]
enum Items<'a> {
    List(&'a [Clause<'a>]),
    Array {
        operand: Operand<'a>,
        op: Cmp,
        items: &'a [Operand<'a>],
    },
}

impl<'a> Items<'a> {
    fn len(&self) -> usize {
        match self {
            Self::List(items) => items.len(),
            Self::Array { items, .. } => items.len(),
        }
    }

    fn get(&self, index: usize) -> Clause<'a> {
        match self {
            Self::List(items) => items[index],
            Self::Array { operand, op, items } => Clause::Compare {
                left: *operand,
                op: *op,
                right: items[index],
            },
        }
    }
}

/// `MAX_SAOP_ARRAY_SIZE`.
const MAX_ARRAY_PROOF: usize = 100;

fn classify<'a>(clause: &Clause<'a>) -> Class<'a> {
    match clause {
        Clause::And(items) => Class::And(Items::List(items)),
        Clause::Or(items) => Class::Or(Items::List(items)),
        Clause::Array {
            operand,
            op,
            any,
            items,
        } if items.len() <= MAX_ARRAY_PROOF => {
            let items = Items::Array {
                operand: *operand,
                op: *op,
                items,
            };
            if *any { Class::Or(items) } else { Class::And(items) }
        }
        _ => Class::Atom,
    }
}

impl<'a> Context<'_, 'a> {
    /// `predicate_refuted_by` of the predicate list by the clause list.
    fn refuted_by(
        &self,
        predicates: &[Clause<'a>],
        clauses: &[Clause<'a>],
        weak: bool,
    ) -> Result<bool, SqlError> {
        if predicates.is_empty() || clauses.is_empty() {
            return Ok(false);
        }
        // No restriction refutes itself, so a list compared with itself
        // needs two of them.
        let held = |list: &[Clause<'a>]| -> Result<Clause<'a>, SqlError> {
            Ok(Clause::And(self.arena.alloc_slice_copy(list).map_err(|_| arena_full())?))
        };
        if core::ptr::eq(predicates, clauses) {
            let both = held(predicates)?;
            if let Clause::And(items) = both
                && items.len() == 1
            {
                return Ok(false);
            }
            return self.refute(&both, &both, weak);
        }
        let predicate = if predicates.len() == 1 {
            predicates[0]
        } else {
            held(predicates)?
        };
        let clause = if clauses.len() == 1 {
            clauses[0]
        } else {
            held(clauses)?
        };
        self.refute(&clause, &predicate, weak)
    }

    fn refute(&self, clause: &Clause<'a>, predicate: &Clause<'a>, weak: bool) -> Result<bool, SqlError> {
        let any_predicate = |test: &dyn Fn(&Clause<'a>) -> Result<bool, SqlError>, items: Items<'a>| {
            for index in 0..items.len() {
                if test(&items.get(index))? {
                    return Ok(true);
                }
            }
            Ok::<bool, SqlError>(false)
        };
        let all = |test: &dyn Fn(&Clause<'a>) -> Result<bool, SqlError>, items: Items<'a>| {
            for index in 0..items.len() {
                if !test(&items.get(index))? {
                    return Ok(false);
                }
            }
            Ok::<bool, SqlError>(true)
        };
        match (classify(clause), classify(predicate)) {
            (Class::And(clause_items), Class::And(predicate_items)) => {
                if any_predicate(&|item| self.refute(clause, item, weak), predicate_items)? {
                    return Ok(true);
                }
                any_predicate(&|item| self.refute(item, predicate, weak), clause_items)
            }
            (Class::And(_), Class::Or(predicate_items))
            | (Class::Or(_), Class::Or(predicate_items))
            | (Class::Atom, Class::Or(predicate_items)) => {
                all(&|item| self.refute(clause, item, weak), predicate_items)
            }
            (Class::And(clause_items), Class::Atom) => {
                any_predicate(&|item| self.refute(item, predicate, weak), clause_items)
            }
            (Class::Or(clause_items), Class::And(predicate_items)) => all(
                &|citem| any_predicate(&|pitem| self.refute(citem, pitem, weak), predicate_items),
                clause_items,
            ),
            (Class::Or(clause_items), Class::Atom) => {
                all(&|item| self.refute(item, predicate, weak), clause_items)
            }
            (Class::Atom, Class::And(predicate_items)) => {
                any_predicate(&|item| self.refute(clause, item, weak), predicate_items)
            }
            (Class::Atom, Class::Atom) => self.refute_atom(predicate, clause, weak),
        }
    }

    /// `predicate_refuted_by_simple_clause`.
    fn refute_atom(
        &self,
        predicate: &Clause<'a>,
        clause: &Clause<'a>,
        weak: bool,
    ) -> Result<bool, SqlError> {
        if let Clause::NullTest {
            operand,
            not_null: false,
        } = clause
        {
            if let Clause::NullTest {
                operand: tested,
                not_null: true,
            } = predicate
                && tested.same(operand)
            {
                return Ok(true);
            }
            return Ok(weak && strict_for(predicate, operand));
        }
        if let Clause::NullTest { operand, not_null } = predicate {
            if *not_null {
                return Ok(false);
            }
            if let Clause::NullTest {
                operand: tested,
                not_null: true,
            } = clause
                && tested.same(operand)
            {
                return Ok(true);
            }
            return Ok(strict_for(clause, operand));
        }
        self.operator_proof(predicate, clause, true, weak)
    }

    /// `operator_predicate_proof` for two comparisons of the same expression.
    fn operator_proof(
        &self,
        predicate: &Clause<'a>,
        clause: &Clause<'a>,
        refute: bool,
        weak: bool,
    ) -> Result<bool, SqlError> {
        let (
            Clause::Compare {
                left: pred_left,
                op: pred_op,
                right: pred_right,
            },
            Clause::Compare {
                left: clause_left,
                op: clause_op,
                right: clause_right,
            },
        ) = (predicate, clause)
        else {
            return Ok(false);
        };
        let (mut pred_op, mut clause_op) = (*pred_op, *clause_op);
        let constant = |operand: &Operand<'a>| match operand {
            Operand::Value(value) => value.constant().map(|datum| (datum, value.type_oid)),
            _ => None,
        };
        let (pred_const, clause_const, common) = if pred_left.same(clause_left) {
            if pred_right.same(clause_right) {
                return Ok(self.same_operands(pred_op, clause_op, refute));
            }
            (constant(pred_right), constant(clause_right), pred_left)
        } else if pred_right.same(clause_right) {
            pred_op = pred_op.commuted();
            clause_op = clause_op.commuted();
            (constant(pred_left), constant(clause_left), pred_right)
        } else if pred_left.same(clause_right) {
            if pred_right.same(clause_left) {
                return Ok(self.same_operands(pred_op.commuted(), clause_op, refute));
            }
            clause_op = clause_op.commuted();
            (constant(pred_right), constant(clause_left), pred_left)
        } else if pred_right.same(clause_left) {
            pred_op = pred_op.commuted();
            (constant(pred_left), constant(clause_right), pred_right)
        } else {
            return Ok(false);
        };
        let collation = match common {
            Operand::Column(column) => self.root[*column].collation,
            _ => crate::sql::ast::Collation::Default,
        };
        let (Some((pred_value, pred_type)), Some((clause_value, clause_type))) =
            (pred_const, clause_const)
        else {
            return Ok(false);
        };
        if clause_value.is_null() {
            return Ok(!(weak && !refute) || pred_value.is_null());
        }
        if pred_value.is_null() {
            return Ok(weak);
        }
        let (Some(pred_type), Some(clause_type)) = (pred_type, clause_type) else {
            return Ok(false);
        };
        if operator_stage(pred_type, clause_type, false) != Some(PruneStage::Plan) {
            return Ok(false);
        }
        let table = if refute { &REFUTE_TEST } else { &IMPLIC_TEST };
        let Some(test) = table[clause_op.index()][pred_op.index()] else {
            return Ok(false);
        };
        let ordering =
            compare_datums_collated(self.storage, self.txid, collation, &pred_value, &clause_value)?;
        Ok(test.holds(ordering))
    }

    /// `operator_same_subexprs_proof`.
    fn same_operands(&self, pred_op: Cmp, clause_op: Cmp, refute: bool) -> bool {
        if refute {
            REFUTES_SAME[clause_op.index()][pred_op.index()]
        } else {
            IMPLIES_SAME[clause_op.index()][pred_op.index()]
        }
    }
}

/// `clause_is_strict_for`: whether `clause` cannot be true when `operand`
/// is null.
fn strict_for(clause: &Clause<'_>, operand: &Operand<'_>) -> bool {
    match clause {
        Clause::Compare { left, right, .. } => left.same(operand) || right.same(operand),
        Clause::Array {
            operand: compared,
            items,
            ..
        } => compared.same(operand) && !items.is_empty(),
        _ => false,
    }
}

/// The relation a pruned scan reads, as its query names its columns.
#[derive(Clone, Copy)]
pub(crate) enum ScanRelation<'s, 'a> {
    /// The FROM item at `table` of a query's scope.
    From {
        scope: &'s super::QueryScope<'a>,
        table: usize,
    },
    /// A write's target, exposed as `name`.
    Target {
        columns: &'s [ColumnMeta],
        name: &'s str,
    },
}

/// The values of a scan of `relation`: its columns, and the parameter
/// values the stage being pruned knows.
pub(crate) struct ScanValues<'s, 'h, 'a> {
    pub(crate) storage: &'s Storage,
    pub(crate) txid: u32,
    pub(crate) relation: ScanRelation<'s, 'a>,
    pub(crate) arena: &'a Arena,
    pub(crate) params: &'s [Datum<'a>],
    pub(crate) hooks: &'s crate::sql::eval::EvalHooks<'h, 'a>,
}

impl<'a> PruneValues<'a> for ScanValues<'_, '_, 'a> {
    fn column(&self, expression: &'a Expr<'a>) -> Option<usize> {
        let Expr::Column { qualifier, name } = expression else {
            return None;
        };
        match self.relation {
            ScanRelation::From { scope, table } => {
                match scope.find_column_unsuggested(*qualifier, name) {
                    Ok(super::ResolvedColumn::Table(found, column)) if found == table => {
                        Some(column)
                    }
                    _ => None,
                }
            }
            ScanRelation::Target {
                columns,
                name: exposed,
            } => {
                if qualifier.is_some_and(|qualifier| qualifier != exposed) {
                    return None;
                }
                columns
                    .iter()
                    .position(|column| column.name.as_str() == *name)
            }
        }
    }

    fn stage(&self, value: &'a Expr<'a>) -> Option<PruneStage> {
        let mut stage = Some(PruneStage::Plan);
        let mut raise = |to: Option<PruneStage>| {
            stage = match (stage, to) {
                (Some(held), Some(to)) => Some(held.max(to)),
                _ => None,
            };
        };
        super::walk_expression_tree(value, &mut |node| match node {
            // The query's own columns, an enclosing query's, a subplan's
            // result or a routine's variable: none of them prunes here.
            Expr::Column { .. }
            | Expr::WholeRow(_)
            | Expr::SchemaColumn { .. }
            | Expr::RecursiveState { .. }
            | Expr::RoutineParam { .. }
            | Expr::OuterAggregate { .. }
            | Expr::DefaultMarker
            | Expr::Subquery(_)
            | Expr::RowSubquery { .. }
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => raise(None),
            Expr::Param(index) => {
                if crate::sql::eval::custom_plan_binds(*index) {
                } else if (*index as usize) <= self.params.len() {
                    raise(Some(PruneStage::Startup));
                } else {
                    raise(None);
                }
            }
            Expr::Call {
                name, args, over, ..
            } => {
                let arity = args.len();
                if over.is_some()
                    || node.is_aggregate()
                    || crate::sql::ast::is_volatile_function(name)
                    || self
                        .storage
                        .has_volatile_function_routine_candidate(name, arity, self.txid)
                {
                    raise(None);
                } else if crate::sql::ast::is_nonimmutable_call(name, arity)
                    || self
                        .storage
                        .has_nonimmutable_function_routine_candidate(name, arity, self.txid)
                {
                    raise(Some(PruneStage::Startup));
                }
            }
            _ => {}
        })
        .ok()?;
        stage
    }

    fn scan_value(&self, value: &'a Expr<'a>) -> bool {
        // No column of this query's own relations, and a subquery or a
        // column only an enclosing query has.
        let mut own = false;
        let mut later = false;
        let _ = super::walk_expression_tree(value, &mut |node| match node {
            Expr::Column { qualifier, name } => {
                let resolves = match self.relation {
                    ScanRelation::From { scope, .. } => {
                        scope.find_column_unsuggested(*qualifier, name).is_ok()
                    }
                    ScanRelation::Target { columns, name: exposed } => {
                        qualifier.is_none_or(|qualifier| qualifier == exposed)
                            && columns.iter().any(|column| column.name.as_str() == *name)
                    }
                };
                if resolves {
                    own = true;
                } else {
                    later = true;
                }
            }
            Expr::Subquery(_)
            | Expr::RowSubquery { .. }
            | Expr::InSubquery { .. }
            | Expr::QuantifiedSubquery { .. }
            | Expr::Exists(_)
            | Expr::ArraySubquery(_) => later = true,
            _ => {}
        });
        later && !own
    }

    fn evaluate(&self, value: &'a Expr<'a>) -> Result<Datum<'a>, SqlError> {
        crate::sql::eval::eval_full(
            value,
            self.arena,
            self.params,
            &crate::sql::eval::NoColumns,
            self.hooks,
        )
    }
}

/// The conjuncts of `condition`, the restrictions of a write's target.
pub(crate) fn conjuncts<'a>(
    condition: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    let mut found = ArenaList::new(arena);
    let mut stack = ArenaList::new(arena);
    if let Some(condition) = condition {
        stack.push(condition).map_err(|_| arena_full())?;
    }
    while let Some(&next) = stack.borrowed_slice().last() {
        stack.truncate(stack.len() - 1);
        match next {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
                ..
            } => {
                stack.push(*right).map_err(|_| arena_full())?;
                stack.push(*left).map_err(|_| arena_full())?;
            }
            other => found.push(other).map_err(|_| arena_full())?,
        }
    }
    Ok(found.as_slice())
}

/// The conditions that restrict the scan of FROM position `table` itself
/// (its `baserestrictinfo`): the `WHERE` conjuncts unless an outer join
/// nulls the relation, and the `ON` conjuncts of the inner joins, or of the
/// outer join whose nullable side it is.
pub(crate) fn scan_conditions<'a>(
    from: &crate::sql::ast::FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    table: usize,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    use crate::sql::ast::JoinKind;
    let nullable = from.joins.iter().enumerate().any(|(index, join)| {
        let right = index + 1;
        match join.kind {
            JoinKind::Left => right == table,
            JoinKind::Right => table < right,
            JoinKind::Full => table <= right,
            _ => false,
        }
    });
    let mut conditions = ArenaList::new(arena);
    let mut add = |condition: &'a Expr<'a>| -> Result<(), SqlError> {
        for conjunct in conjuncts(Some(condition), arena)? {
            conditions.push(*conjunct).map_err(|_| arena_full())?;
        }
        Ok(())
    };
    if !nullable && let Some(condition) = where_clause {
        add(condition)?;
    }
    for (index, join) in from.joins.iter().enumerate() {
        let Some(on) = join.on else {
            continue;
        };
        let applies = match join.kind {
            JoinKind::Inner => !nullable,
            JoinKind::Left => index + 1 == table,
            JoinKind::Right => table <= index,
            _ => false,
        };
        if applies {
            add(on)?;
        }
    }
    Ok(conditions.as_slice())
}
