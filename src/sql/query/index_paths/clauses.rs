//! A scan's restrictions as PostgreSQL's planner holds them
//! (`RestrictInfo`): each condition's form, its selectivity
//! (`clause_selectivity`) and its evaluation cost (`cost_qual_eval`).

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Expr};
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};

use super::super::scope::{QueryScope, ResolvedColumn};
use super::{Comparison, Planner};

/// `MIN_ARRAY_SIZE_FOR_HASHED_SAOP`: a constant array this long is
/// searched through a hash table.
const HASHED_ARRAY: usize = 9;
/// The deepest nesting of `AND` and `OR` the model reads.
const MAX_DEPTH: usize = 8;
/// The conditions of one `AND` list: a query's, or one arm of an `OR`.
pub(super) const MAX_CONJUNCTS: usize = 32;
/// The shape of equalities of one column with different constants, which
/// leave no row to scan.
pub(super) const CONTRADICTION: &str = "equalities of one column with different constants";
/// The arms of one `OR`.
const MAX_ARMS: usize = 32;

/// `QualCost`: a condition's one-time and per-row evaluation cost.
#[derive(Clone, Copy, Default, Debug)]
pub(super) struct QualCost {
    pub(super) startup: f64,
    pub(super) per_tuple: f64,
}

impl QualCost {
    pub(super) fn add(self, other: Self) -> Self {
        Self {
            startup: self.startup + other.startup,
            per_tuple: self.per_tuple + other.per_tuple,
        }
    }
}

/// One restriction (`RestrictInfo`).
#[derive(Clone, Copy)]
pub(crate) struct Restriction<'a> {
    pub(super) source: &'a Expr<'a>,
    pub(super) clause: Clause<'a>,
    pub(super) selectivity: f64,
    pub(super) cost: QualCost,
    /// The clause's position among the distinct conditions of the query
    /// (`find_list_position`): equal clauses share one.
    pub(super) id: usize,
}

/// A restriction's form.
#[derive(Clone, Copy)]
pub(super) enum Clause<'a> {
    /// `column op value`, the operator with the column on its left.
    Compare {
        column: usize,
        comparison: Comparison,
        /// `None` for a parameter of a generic plan, whose value is unknown.
        constant: Option<Datum<'a>>,
        key: &'a Expr<'a>,
        operand: &'a Expr<'a>,
        /// A pattern operator (`~<~` and its kin), which compares bytes
        /// and belongs to the pattern operator family only.
        pattern: bool,
        /// A boolean column tested by itself (`active`, `NOT active`),
        /// which an index applies as `= true` or `= false`.
        boolean: Option<bool>,
    },
    NullTest {
        column: usize,
        is_null: bool,
    },
    /// `column op ANY (array)` (an `IN` list) or `column op ALL (array)`
    /// (`NOT IN`), a `ScalarArrayOpExpr`.
    Array {
        column: usize,
        comparison: Comparison,
        any: bool,
        /// The array's elements, `None` when its value is unknown.
        elements: Option<&'a [Datum<'a>]>,
        /// `estimate_array_length`.
        length: f64,
    },
    /// An `OR` of its arms, each a list of conditions.
    Or(&'a [&'a [Restriction<'a>]]),
    /// A condition the model has no form for, reading the columns of the
    /// mask (columns past 63 set bit 63); `indexable` when an index on one
    /// of them might apply it.
    Other {
        columns: u64,
        shape: &'static str,
        indexable: bool,
        /// Only a pattern class could serve it (`~<~` and its kin).
        pattern_only: bool,
    },
}

impl Clause<'_> {
    /// The one column a simple clause reads.
    pub(super) fn column(&self) -> Option<usize> {
        match *self {
            Self::Compare { column, .. }
            | Self::NullTest { column, .. }
            | Self::Array { column, .. } => Some(column),
            Self::Or(_) | Self::Other { .. } => None,
        }
    }

    /// The columns the clause reads, as a mask (columns past 63 set bit
    /// 63).
    pub(super) fn columns(&self) -> u64 {
        match *self {
            Self::Compare { column, .. }
            | Self::NullTest { column, .. }
            | Self::Array { column, .. } => column_bit(column),
            Self::Or(arms) => arms
                .iter()
                .flat_map(|arm| arm.iter())
                .fold(0, |mask, restriction| mask | restriction.clause.columns()),
            Self::Other { columns, .. } => columns,
        }
    }

    /// The first shape inside the clause the model has no form for.
    pub(super) fn unmodeled(&self) -> Option<&'static str> {
        match *self {
            Self::Other { shape, .. } => Some(shape),
            Self::Or(arms) => arms
                .iter()
                .flat_map(|arm| arm.iter())
                .find_map(|restriction| restriction.clause.unmodeled()),
            _ => None,
        }
    }

    /// Whether the clause is `column = constant`, which PostgreSQL keeps in
    /// an equivalence class with the constant.
    pub(super) fn constant_equality(&self) -> Option<usize> {
        match *self {
            Self::Compare {
                column,
                comparison: Comparison::Eq,
                pattern: false,
                boolean: None,
                ..
            } => Some(column),
            _ => None,
        }
    }
}

pub(super) fn column_bit(column: usize) -> u64 {
    1u64 << column.min(63)
}

/// The value of a constant `expression` compared with `column`: an
/// untyped literal takes the column's type, as the parser resolves it.
/// `None` when the expression is not a constant the model reads.
pub(super) fn constant_value<'a>(
    scope: &QueryScope<'_>,
    expression: &'a Expr<'a>,
    column: usize,
    params: &[Datum<'a>],
    arena: &'a Arena,
) -> Option<Datum<'a>> {
    if !expression.is_constant(false) || expression.calls_routine() {
        return None;
    }
    let value =
        crate::sql::eval::eval(expression, arena, params, &crate::sql::eval::NoColumns).ok()?;
    if matches!(expression, Expr::Str(_)) && !value.is_null() {
        let ctype = scope
            .defs
            .stored(0)
            .and_then(|definition| definition.columns().get(column))
            .map(|meta| meta.ctype);
        // Only types whose literal input has no side effect (a notice) are
        // read here.
        return match ctype {
            Some(ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Name) | None => {
                Some(value)
            }
            Some(
                ctype @ (ColType::Bool
                | ColType::Int2
                | ColType::Int4
                | ColType::Int8
                | ColType::Float4
                | ColType::Float8
                | ColType::Numeric
                | ColType::Oid
                | ColType::Date
                | ColType::Time
                | ColType::Timetz
                | ColType::Timestamp
                | ColType::Timestamptz
                | ColType::Interval
                | ColType::Bytea
                | ColType::Uuid
                | ColType::Char),
            ) => crate::sql::eval::cast_to(value, ctype, arena).ok(),
            Some(_) => None,
        };
    }
    Some(value)
}

/// Reads the conditions of a query or an `OR` arm.
pub(super) struct Analyzer<'p, 's, 'a> {
    pub(super) planner: &'p Planner<'s, 'a>,
    pub(super) scope: &'p QueryScope<'p>,
    pub(super) params: &'p [Datum<'a>],
    pub(super) arena: &'a Arena,
    /// The distinct clauses seen so far, by identity.
    pub(super) seen: &'p mut [&'a Expr<'a>],
    pub(super) seen_count: usize,
}

impl<'a> Analyzer<'_, '_, 'a> {
    fn column_of(&self, expression: &Expr<'_>) -> Option<usize> {
        let Expr::Column { qualifier, name } = expression else {
            return None;
        };
        match self.scope.find_column(*qualifier, name).ok()? {
            ResolvedColumn::Table(0, column) => Some(column),
            _ => None,
        }
    }

    /// The columns of the table `expression` reads.
    fn columns_of(&self, expression: &Expr<'_>) -> u64 {
        let mut mask = 0;
        expression.for_each_column_reference(&mut |qualifier, name| {
            if let Ok(ResolvedColumn::Table(0, column)) = self.scope.find_column(qualifier, name) {
                mask |= column_bit(column);
            }
        });
        mask
    }

    /// The identity of `source` among the distinct clauses.
    fn identity(&mut self, source: &'a Expr<'a>) -> Result<usize, &'static str> {
        if let Some(at) = self.seen[..self.seen_count]
            .iter()
            .position(|seen| **seen == *source)
        {
            return Ok(at);
        }
        if self.seen_count == self.seen.len() {
            return Err("more than 64 distinct conditions");
        }
        self.seen[self.seen_count] = source;
        self.seen_count += 1;
        Ok(self.seen_count - 1)
    }

    /// Analyzes the conjuncts of an `AND` list: each restriction in
    /// PostgreSQL's order (the others as written, then each column's
    /// equality with a constant, which an equivalence class holds).
    pub(super) fn conjuncts(
        &mut self,
        conjuncts: &[&'a Expr<'a>],
        depth: usize,
    ) -> Result<Result<&'a [Restriction<'a>], &'static str>, SqlError> {
        let mut analyzed = [None::<Restriction<'a>>; MAX_CONJUNCTS];
        if conjuncts.len() > MAX_CONJUNCTS {
            return Ok(Err("more than 32 conditions"));
        }
        for (position, &conjunct) in conjuncts.iter().enumerate() {
            analyzed[position] = Some(match self.restriction(conjunct, depth)? {
                Ok(restriction) => restriction,
                Err(shape) => return Ok(Err(shape)),
            });
        }
        let analyzed = &analyzed[..conjuncts.len()];
        let ordered = self
            .arena
            .alloc_slice_with(analyzed.len(), |_| analyzed[0].expect("analyzed"))
            .map_err(|_| crate::sql::eval::arena_full())?;
        let mut count = 0;
        for pass in [false, true] {
            for restriction in analyzed.iter().flatten() {
                let equality = restriction.clause.constant_equality().is_some()
                    && matches!(restriction.clause, super::clauses::Clause::Compare { .. });
                if equality == pass {
                    ordered[count] = *restriction;
                    count += 1;
                }
            }
        }
        // Two equalities on one column merge in their equivalence class,
        // which the model does not follow.
        for (at, restriction) in ordered.iter().enumerate() {
            let Some(column) = restriction.clause.constant_equality() else {
                continue;
            };
            for other in &ordered[at + 1..] {
                if other.clause.constant_equality() != Some(column) {
                    continue;
                }
                // Different constants leave the class no row
                // (`One-Time Filter: false`).
                if let (
                    Clause::Compare {
                        constant: Some(left),
                        ..
                    },
                    Clause::Compare {
                        constant: Some(right),
                        ..
                    },
                ) = (restriction.clause, other.clause)
                {
                    let collation = self.planner.definition.columns()[column].collation;
                    if self.planner.compare(collation, &left, &right)?.is_ne() {
                        return Ok(Err(CONTRADICTION));
                    }
                }
                return Ok(Err("two equalities on one column"));
            }
        }
        Ok(Ok(ordered))
    }

    /// Splits `expression` at its top-level `AND`s and analyzes the parts.
    pub(super) fn and_list(
        &mut self,
        expression: &'a Expr<'a>,
        depth: usize,
    ) -> Result<Result<&'a [Restriction<'a>], &'static str>, SqlError> {
        let mut conjuncts = [&Expr::Null; MAX_CONJUNCTS];
        let mut count = 0;
        if !super::super::plan::flatten_and(expression, &mut conjuncts, &mut count) {
            return Ok(Err("more than 32 conditions"));
        }
        self.conjuncts(&conjuncts[..count], depth)
    }

    fn restriction(
        &mut self,
        conjunct: &'a Expr<'a>,
        depth: usize,
    ) -> Result<Result<Restriction<'a>, &'static str>, SqlError> {
        if depth > MAX_DEPTH {
            return Ok(Err("conditions nested more than eight deep"));
        }
        let clause = match self.clause(conjunct, depth)? {
            Ok(clause) => clause,
            Err(shape) => return Ok(Err(shape)),
        };
        let selectivity = match self.selectivity(&clause)? {
            Ok(selectivity) => selectivity,
            Err(shape) => return Ok(Err(shape)),
        };
        let id = match self.identity(conjunct) {
            Ok(id) => id,
            Err(shape) => return Ok(Err(shape)),
        };
        Ok(Ok(Restriction {
            source: conjunct,
            clause,
            selectivity,
            cost: self.cost(conjunct, &clause),
            id,
        }))
    }

    fn clause(
        &mut self,
        conjunct: &'a Expr<'a>,
        depth: usize,
    ) -> Result<Result<Clause<'a>, &'static str>, SqlError> {
        let other = |analyzer: &Self, shape: &'static str| -> Clause<'a> {
            Clause::Other {
                columns: analyzer.columns_of(conjunct),
                shape,
                indexable: indexable_shape(conjunct),
                pattern_only: matches!(
                    conjunct,
                    Expr::Binary {
                        operator: BinaryOp::PatternLt
                            | BinaryOp::PatternLtEq
                            | BinaryOp::PatternGt
                            | BinaryOp::PatternGtEq,
                        ..
                    }
                ),
            }
        };
        Ok(Ok(match conjunct {
            Expr::Binary {
                operator: BinaryOp::Or,
                ..
            } => {
                let mut arms = [&Expr::Null; MAX_ARMS];
                let mut count = 0;
                if !flatten_or(conjunct, &mut arms, &mut count) {
                    return Ok(Err("an OR of more than 32 conditions"));
                }
                let mut analyzed: [&'a [Restriction<'a>]; MAX_ARMS] = [&[]; MAX_ARMS];
                for (position, &arm) in arms[..count].iter().enumerate() {
                    analyzed[position] = match self.and_list(arm, depth + 1)? {
                        Ok(arm) => arm,
                        Err(shape) => return Ok(Err(shape)),
                    };
                }
                // `canonicalize_qual` pulls a condition every arm shares out
                // of the OR, which the model does not follow.
                let arms = &analyzed[..count];
                if arms[0].iter().any(|shared| {
                    arms[1..].iter().all(|arm| {
                        arm.iter()
                            .any(|restriction| restriction.source == shared.source)
                    })
                }) {
                    return Ok(Err("an OR whose arms share a condition"));
                }
                Clause::Or(
                    self.arena
                        .alloc_slice_copy(arms)
                        .map_err(|_| crate::sql::eval::arena_full())?,
                )
            }
            Expr::Binary {
                operator,
                left,
                right,
                ..
            } => {
                let (column, key, operand, on_left) =
                    match (self.column_of(left), self.column_of(right)) {
                        (Some(column), None) => (column, *left, *right, true),
                        (None, Some(column)) => (column, *right, *left, false),
                        _ => return Ok(Ok(other(self, "a comparison of two expressions"))),
                    };
                let pattern = matches!(
                    operator,
                    BinaryOp::PatternLt
                        | BinaryOp::PatternLtEq
                        | BinaryOp::PatternGt
                        | BinaryOp::PatternGtEq
                );
                let plain = match operator {
                    BinaryOp::PatternLt => BinaryOp::Lt,
                    BinaryOp::PatternLtEq => BinaryOp::LtEq,
                    BinaryOp::PatternGt => BinaryOp::Gt,
                    BinaryOp::PatternGtEq => BinaryOp::GtEq,
                    other => *other,
                };
                let Some(comparison) = Comparison::of(plain, on_left) else {
                    return Ok(Ok(other(self, "an operator other than a comparison")));
                };
                // A generic plan's parameter is a constant of unknown value.
                let constant = if matches!(operand, Expr::Param(_)) && self.params.is_empty() {
                    None
                } else {
                    match constant_value(self.scope, operand, column, self.params, self.arena) {
                        Some(value) => Some(value),
                        None => {
                            return Ok(Ok(other(
                                self,
                                "a comparison with an expression that is not a constant",
                            )));
                        }
                    }
                };
                Clause::Compare {
                    column,
                    comparison,
                    constant,
                    key,
                    operand,
                    pattern,
                    boolean: None,
                }
            }
            Expr::IsNull { operand, negated } => match self.column_of(operand) {
                Some(column) => Clause::NullTest {
                    column,
                    is_null: !negated,
                },
                None => other(self, "a NULL test of an expression"),
            },
            Expr::InList {
                operand,
                list,
                negated,
            } => {
                let Some(column) = self.column_of(operand) else {
                    return Ok(Ok(other(self, "an IN list over an expression")));
                };
                // A one-element list is a plain comparison.
                if let [element] = list {
                    let constant = if matches!(element, Expr::Param(_)) && self.params.is_empty() {
                        None
                    } else {
                        match constant_value(self.scope, element, column, self.params, self.arena) {
                            Some(value) => Some(value),
                            None => {
                                return Ok(Ok(other(
                                    self,
                                    "an IN list of an expression that is not a constant",
                                )));
                            }
                        }
                    };
                    return Ok(Ok(Clause::Compare {
                        column,
                        comparison: if *negated {
                            Comparison::Ne
                        } else {
                            Comparison::Eq
                        },
                        constant,
                        key: operand,
                        operand: element,
                        pattern: false,
                        boolean: None,
                    }));
                }
                let elements = self
                    .arena
                    .alloc_slice_with(list.len(), |_| Datum::Null)
                    .map_err(|_| crate::sql::eval::arena_full())?;
                let mut known = true;
                for (position, element) in list.iter().enumerate() {
                    if matches!(element, Expr::Param(_)) && self.params.is_empty() {
                        known = false;
                        continue;
                    }
                    match constant_value(self.scope, element, column, self.params, self.arena) {
                        Some(value) => elements[position] = value,
                        None => {
                            return Ok(Ok(other(
                                self,
                                "an IN list of expressions that are not constants",
                            )));
                        }
                    }
                }
                Clause::Array {
                    column,
                    comparison: if *negated {
                        Comparison::Ne
                    } else {
                        Comparison::Eq
                    },
                    any: !*negated,
                    elements: known.then_some(&*elements),
                    length: list.len() as f64,
                }
            }
            Expr::AnyAll {
                operand,
                operator,
                array,
                all,
            } => {
                let Some(column) = self.column_of(operand) else {
                    return Ok(Ok(other(self, "an ANY or ALL over an expression")));
                };
                let Some(comparison) = Comparison::of(*operator, true) else {
                    return Ok(Ok(other(self, "an ANY or ALL of another operator")));
                };
                if !array.is_constant(false) || array.calls_routine() {
                    return Ok(Ok(other(self, "an ANY or ALL over a non-constant array")));
                }
                let value = crate::sql::eval::eval(
                    array,
                    self.arena,
                    self.params,
                    &crate::sql::eval::NoColumns,
                );
                // An untyped literal takes the array type of the column.
                let element = crate::sql::types::ArrElem::from_coltype(
                    self.planner.definition.columns()[column].ctype,
                );
                let value = match (array, value, element) {
                    (Expr::Str(_), Ok(text @ Datum::Text(_)), Some(element)) => {
                        crate::sql::eval::cast_to(text, ColType::Array(element), self.arena)
                    }
                    (_, value, _) => value,
                };
                let Ok(Datum::Array { element, raw }) = value else {
                    return Ok(Ok(other(self, "an ANY or ALL over a non-constant array")));
                };
                let count = crate::sql::array::elements(raw, element).count();
                let elements = self
                    .arena
                    .alloc_slice_with(count, |_| Datum::Null)
                    .map_err(|_| crate::sql::eval::arena_full())?;
                for (slot, value) in elements
                    .iter_mut()
                    .zip(crate::sql::array::elements(raw, element))
                {
                    *slot = value;
                }
                Clause::Array {
                    column,
                    comparison,
                    any: !*all,
                    elements: Some(&*elements),
                    length: count as f64,
                }
            }
            Expr::Column { .. } | Expr::Unary { .. } => {
                let (column_expression, value) = match conjunct {
                    Expr::Unary {
                        operator: crate::sql::ast::UnaryOp::Not,
                        operand,
                        ..
                    } => (*operand, false),
                    _ => (conjunct, true),
                };
                match self.column_of(column_expression) {
                    Some(column)
                        if self.planner.definition.columns()[column].ctype == ColType::Bool =>
                    {
                        let operand = self
                            .arena
                            .alloc(Expr::Bool(value))
                            .map_err(|_| crate::sql::eval::arena_full())?;
                        Clause::Compare {
                            column,
                            comparison: Comparison::Eq,
                            constant: Some(Datum::Bool(value)),
                            key: column_expression,
                            operand,
                            pattern: false,
                            boolean: Some(value),
                        }
                    }
                    _ => other(
                        self,
                        "a condition other than a column compared with constants",
                    ),
                }
            }
            _ => other(
                self,
                "a condition other than a column compared with constants",
            ),
        }))
    }

    /// `clause_selectivity`, or the shape whose selectivity the model does
    /// not compute.
    fn selectivity(&self, clause: &Clause<'a>) -> Result<Result<f64, &'static str>, SqlError> {
        let planner = self.planner;
        Ok(Ok(match *clause {
            // `boolvarsel`, and `NOT` its complement.
            Clause::Compare {
                column,
                boolean: Some(value),
                ..
            } => {
                let truth = match planner.operator_selectivity(
                    column,
                    Comparison::Eq,
                    Some(Datum::Bool(true)),
                )? {
                    Ok(selectivity) => selectivity,
                    Err(shape) => return Ok(Err(shape)),
                };
                if value { truth } else { 1.0 - truth }
            }
            Clause::Compare {
                column,
                comparison,
                constant,
                pattern: false,
                ..
            } => match planner.operator_selectivity(column, comparison, constant)? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            },
            Clause::Compare {
                column,
                comparison,
                constant,
                pattern: true,
                ..
            } => match planner.pattern_selectivity(column, comparison, constant)? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            },
            Clause::NullTest {
                column, is_null, ..
            } => planner.null_selectivity(column, is_null),
            Clause::Array {
                column,
                comparison,
                any,
                elements,
                ..
            } => match planner.array_selectivity(column, comparison, any, elements)? {
                Ok(selectivity) => selectivity,
                Err(shape) => return Ok(Err(shape)),
            },
            Clause::Or(arms) => {
                // `clauselist_selectivity_or`.
                let mut selectivity = 0.0;
                for arm in arms {
                    let arm = planner.selectivity(arm);
                    selectivity = selectivity + arm - selectivity * arm;
                }
                selectivity
            }
            // Unknown; a plan that depends on it is outside the model.
            Clause::Other { .. } => f64::NAN,
        }))
    }

    /// `cost_qual_eval` of the clause.
    fn cost(&self, source: &Expr<'_>, clause: &Clause<'_>) -> QualCost {
        let operator = self.planner.costs.cpu_operator;
        match *clause {
            Clause::Or(arms) => arms
                .iter()
                .flat_map(|arm| arm.iter())
                .fold(QualCost::default(), |total, restriction| {
                    total.add(restriction.cost)
                }),
            // A long constant array is hashed once and probed once per row
            // with the hash and equality functions.
            Clause::Array {
                comparison,
                elements: Some(elements),
                length,
                ..
            } if elements.len() >= HASHED_ARRAY
                && matches!(comparison, Comparison::Eq | Comparison::Ne) =>
            {
                QualCost {
                    startup: length * operator,
                    per_tuple: 2.0 * operator,
                }
            }
            _ => {
                let definition = self.planner.definition;
                let columns = crate::sql::exec::AliasedDefCols {
                    definition: crate::storage::RelationDefinition::from_definition(definition),
                    alias: None,
                    output_aliases: &[],
                    remote_ctid: false,
                };
                QualCost {
                    startup: 0.0,
                    per_tuple: super::super::plan::qual_cost(
                        source,
                        &columns,
                        self.planner.storage,
                        self.planner.txid,
                        self.arena,
                    ) / 2.0
                        * operator,
                }
            }
        }
    }
}

/// Splits `expression` at its top-level `OR`s.
fn flatten_or<'e, 'a>(
    expression: &'e Expr<'a>,
    out: &mut [&'e Expr<'a>],
    count: &mut usize,
) -> bool {
    if let Expr::Binary {
        operator: BinaryOp::Or,
        left,
        right,
        ..
    } = expression
    {
        return flatten_or(left, out, count) && flatten_or(right, out, count);
    }
    if *count == out.len() {
        return false;
    }
    out[*count] = expression;
    *count += 1;
    true
}

/// Whether PostgreSQL might apply `condition` through an index on one of
/// its columns: no subquery, and a column (or a binary-compatible
/// relabeling of one) as an operand of its top operator, as
/// `match_clause_to_indexcol` and the support functions of `LIKE` and the
/// pattern and boolean operators look for.
fn indexable_shape(condition: &Expr<'_>) -> bool {
    fn bare(expression: &Expr<'_>) -> bool {
        match expression {
            Expr::Column { .. } => true,
            Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => bare(operand),
            _ => false,
        }
    }
    if condition.contains_subquery() {
        return false;
    }
    match condition {
        Expr::Column { .. } => true,
        Expr::Unary { operand, .. } => bare(operand),
        Expr::Binary { left, right, .. } => bare(left) || bare(right),
        Expr::Like { operand, .. } | Expr::Match { operand, .. } => bare(operand),
        Expr::IsNull { operand, .. }
        | Expr::InList { operand, .. }
        | Expr::AnyAll { operand, .. } => bare(operand),
        Expr::Call { .. } => false,
        _ => true,
    }
}
