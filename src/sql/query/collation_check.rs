//! PostgreSQL's `assign_collations` conflict check for explicit `COLLATE`
//! clauses, run while a query level is analysed. Two explicit collations that
//! meet in one expression fail with `42P21` at the later clause, whether or
//! not any row ever reaches the expression. Implicit derivations do not fail
//! here: an indeterminate collation fails only when an operation needs it.

use crate::mem::arena::Arena;
use crate::sql::ast::{Collation, Expr, ParsedCollation, Select, SelectItem, SourcePosition};
use crate::sql::eval::{CatalogAccess, SqlError, sqlstate};
use crate::sql::exec::ColTypeResolver;
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::Storage;

/// An explicit collation that reaches a node, with its `COLLATE` clause.
#[derive(Clone, Copy)]
struct Explicit {
    value: Collation,
    at: SourcePosition,
}

/// Checks every expression of one query level, and each subquery level
/// inside them, for conflicting explicit collations.
pub(crate) fn check_level(
    statement: &Select<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    if level_expressions(statement)
        .map(count_collate_clauses)
        .sum::<usize>()
        < 2
    {
        return Ok(());
    }
    check_expressions(level_expressions(statement), storage, txid, arena, || {
        statement
            .from
            .as_ref()
            .map(|from| super::QueryScope::resolve_schema(storage, from, txid, arena, None))
            .transpose()
    })
}

/// Checks `expressions`, typed by the sources `resolve` gives, when two or
/// more `COLLATE` clauses could meet in them.
pub(crate) fn check_expressions<'s, 'e: 's, 'a>(
    expressions: impl Iterator<Item = &'s Expr<'e>> + Clone,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    resolve: impl FnOnce() -> Result<Option<super::QueryScope<'a>>, SqlError>,
) -> Result<(), SqlError> {
    if expressions
        .clone()
        .map(count_collate_clauses)
        .sum::<usize>()
        < 2
    {
        return Ok(());
    }
    let catalog = super::storage_catalog(storage, arena, txid);
    let mark = arena.mark();
    let scope = match resolve() {
        Ok(scope) => scope,
        Err(error) if error.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED => return Err(error),
        // Sources this analysis cannot type report their own errors.
        Err(_) => {
            // SAFETY: the failed resolution's scratch is not referenced.
            unsafe { arena.rewind_to(mark) };
            return Ok(());
        }
    };
    super::with_level_columns(scope.as_ref(), &catalog, |columns| {
        let checker = Checker {
            columns,
            catalog: &catalog,
            storage,
            txid,
            arena,
        };
        for expression in expressions {
            checker.check(expression)?;
        }
        Ok(())
    })
}

/// The expressions of a level PostgreSQL assigns collations to, each in its
/// own context.
fn level_expressions<'s, 'a>(
    statement: &'s Select<'a>,
) -> impl Iterator<Item = &'a Expr<'a>> + Clone + 's {
    let items = statement.items.iter().filter_map(|item| match item {
        SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
            Some(*expression)
        }
        SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => None,
    });
    let joins = statement
        .from
        .iter()
        .flat_map(|from| from.joins.iter().filter_map(|join| join.on));
    // `assign_query_collations` walks the target list, which holds the
    // ORDER BY, GROUP BY and DISTINCT ON keys, then the join tree's
    // conditions, then HAVING.
    items
        .chain(statement.order_by.iter().map(|key| key.expression))
        .chain(statement.group_by.iter().copied())
        .chain(statement.distinct_on.iter().copied())
        .chain(joins)
        .chain(statement.where_clause)
        .chain(statement.having)
}

/// The `COLLATE` clauses of an expression and its subqueries, counted to
/// skip the check where no two clauses can meet.
fn count_collate_clauses(expression: &Expr<'_>) -> usize {
    let own = usize::from(matches!(expression, Expr::Collate { .. }));
    let nested = match expression {
        Expr::Subquery(query) | Expr::Exists(query) | Expr::ArraySubquery(query) => {
            level_expressions(query).map(count_collate_clauses).sum()
        }
        Expr::RowSubquery { select, .. }
        | Expr::InSubquery { select, .. }
        | Expr::QuantifiedSubquery { select, .. } => {
            level_expressions(select).map(count_collate_clauses).sum()
        }
        _ => 0,
    };
    let mut children = 0;
    let _ = super::walk_children(expression, &mut |child| {
        children += count_collate_clauses(child);
        Ok(())
    });
    own + nested + children
}

struct Checker<'c> {
    columns: &'c dyn ColTypeResolver,
    catalog: &'c dyn CatalogAccess,
    storage: &'c Storage,
    txid: u32,
    arena: &'c Arena,
}

impl Checker<'_> {
    /// Checks `expression` in a context of its own.
    fn check(&self, expression: &Expr<'_>) -> Result<(), SqlError> {
        self.state(expression).map(|_| ())
    }

    /// The explicit collation `expression` passes to its parent, raising a
    /// conflict met inside it.
    fn state(&self, expression: &Expr<'_>) -> Result<Option<Explicit>, SqlError> {
        match expression {
            Expr::Collate {
                source_position,
                operand,
                collation,
            } => {
                self.check(operand)?;
                let value = match *collation {
                    ParsedCollation::Builtin(value) => Some(value),
                    ParsedCollation::Named(name) => {
                        self.catalog.resolve_collation(name.schema, name.name)
                    }
                };
                // An unknown collation fails where the clause is resolved.
                Ok(value.map(|value| Explicit {
                    value,
                    at: *source_position,
                }))
            }
            // A subquery's output has its own level's collation, implicitly.
            Expr::Subquery(query) | Expr::Exists(query) | Expr::ArraySubquery(query) => {
                check_level(query, self.storage, self.txid, self.arena)?;
                Ok(None)
            }
            Expr::RowSubquery { select, .. } => {
                check_level(select, self.storage, self.txid, self.arena)?;
                Ok(None)
            }
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                self.check(operand)?;
                check_level(select, self.storage, self.txid, self.arena)?;
                Ok(None)
            }
            // A row's fields keep separate collations.
            expression if expression.is_row_constructor() => {
                let Expr::Call { args, .. } = expression else {
                    unreachable!("a row constructor is a call");
                };
                for field in *args {
                    self.check(field)?;
                }
                Ok(None)
            }
            // A row comparison merges each pair of columns separately.
            Expr::Binary {
                left:
                    left @ Expr::Call {
                        args: left_fields, ..
                    },
                right:
                    right @ Expr::Call {
                        args: right_fields, ..
                    },
                ..
            } if left.is_row_constructor()
                && right.is_row_constructor()
                && left_fields.len() == right_fields.len() =>
            {
                for (left, right) in left_fields.iter().zip(*right_fields) {
                    let mut merged = None;
                    merge(&mut merged, self.state(left)?)?;
                    merge(&mut merged, self.state(right)?)?;
                }
                Ok(None)
            }
            // The WHEN conditions yield booleans; only the results merge. A
            // simple CASE compares its operand with each value, which reads
            // the operand's collation implicitly.
            Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } => {
                if let Some(operand) = operand {
                    self.check(operand)?;
                }
                let mut merged = None;
                for (condition, result) in *whens {
                    self.check(condition)?;
                    merge(&mut merged, self.state(result)?)?;
                }
                if let Some(otherwise) = otherwise {
                    merge(&mut merged, self.state(otherwise)?)?;
                }
                self.result(expression, merged)
            }
            // An aggregate merges its arguments; its ORDER BY keys, FILTER and
            // window are separate.
            Expr::Call {
                args,
                order_by,
                over,
                filter,
                ..
            } => {
                let mut merged = None;
                for argument in *args {
                    merge(&mut merged, self.state(argument)?)?;
                }
                for key in *order_by {
                    self.check(key.expression)?;
                }
                if let Some(filter) = filter {
                    self.check(filter)?;
                }
                if let Some(window) = over {
                    for key in window.partition_by {
                        self.check(key)?;
                    }
                    for key in window.order_by {
                        self.check(key.expression)?;
                    }
                }
                self.result(expression, merged)
            }
            // `x IN (a, b)` compares `x` with an array of the list.
            Expr::InList { operand, list, .. } => {
                let mut elements = None;
                for element in *list {
                    merge(&mut elements, self.state(element)?)?;
                }
                let mut merged = self.state(operand)?;
                merge(&mut merged, elements)?;
                Ok(None)
            }
            _ => {
                let mut merged = None;
                let mut failure = None;
                super::walk_children(expression, &mut |child| {
                    if failure.is_none() {
                        match self
                            .state(child)
                            .and_then(|state| merge(&mut merged, state))
                        {
                            Ok(()) => {}
                            Err(error) => failure = Some(error),
                        }
                    }
                    Ok(())
                })?;
                if let Some(error) = failure {
                    return Err(error);
                }
                self.result(expression, merged)
            }
        }
    }

    /// What a node passes up: its inputs' explicit collation when its own
    /// result is collatable.
    fn result(
        &self,
        expression: &Expr<'_>,
        merged: Option<Explicit>,
    ) -> Result<Option<Explicit>, SqlError> {
        if merged.is_none() {
            return Ok(None);
        }
        // A result whose type cannot be inferred here passes nothing up, so
        // the check never reports a conflict PostgreSQL would not.
        let collatable = crate::sql::error_position::preserving_pending(|| {
            crate::sql::exec::infer_type_res(expression, self.columns)
        })
        .ok()
        .and_then(|(oid, _)| ColType::from_oid(oid).or_else(|| self.catalog.domain_base_type(oid)))
        .is_some_and(ColType::is_collatable);
        Ok(if collatable { merged } else { None })
    }
}

/// `merge_collation_state` for explicit collations: the first one met wins,
/// and a different second one fails at its clause.
fn merge(merged: &mut Option<Explicit>, next: Option<Explicit>) -> Result<(), SqlError> {
    match (*merged, next) {
        (Some(first), Some(second)) if first.value != second.value => {
            Err(crate::sql::error_position::at_source_position(
                sql_err!(
                    sqlstate::COLLATION_MISMATCH,
                    "collation mismatch between explicit collations \"{}\" and \"{}\"",
                    first.value.name(),
                    second.value.name()
                ),
                second.at,
            ))
        }
        (None, Some(next)) => {
            *merged = Some(next);
            Ok(())
        }
        _ => Ok(()),
    }
}
