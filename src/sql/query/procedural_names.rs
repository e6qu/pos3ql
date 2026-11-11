//! The `plpgsql.variable_conflict` check for a statement a PL/pgSQL body runs.
//!
//! PostgreSQL's parse analysis asks PL/pgSQL about every column reference of
//! such a statement, at every query level, and under the default rule
//! (`error`) reports a name that is both a table column and a PL/pgSQL
//! variable as ambiguous before any row is read. Execution here resolves
//! names as rows arrive, so this pass walks the statement once, in
//! PostgreSQL's analysis order, against each level's FROM schema. A level
//! whose FROM clause does not resolve is left to the statement's own
//! analysis, which reports that error.
//!
//! Under `use_variable` the variable must win at every evaluation site,
//! including aggregate inputs and grouping keys that execution binds to
//! columns directly, so such a name is rejected rather than read as the
//! column. `use_column` matches execution's own precedence.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, FromClause, Select, SelectItem, SetTree, Stmt};
use crate::sql::eval::{ColumnLookup, SqlError};
use crate::sql::guc::VariableConflict;
use crate::storage::Storage;

use super::QueryScope;

/// One query level's FROM schema, linked to the level enclosing it.
struct Level<'s, 'a> {
    scope: Option<&'s QueryScope<'a>>,
    parent: Option<&'s Level<'s, 'a>>,
}

impl Level<'_, '_> {
    fn has_column(&self, qualifier: Option<&str>, name: &str) -> bool {
        let mut level = Some(self);
        while let Some(current) = level {
            if current.scope.is_some_and(|scope| {
                crate::sql::error_position::preserving_pending(|| {
                    match scope.find_column(qualifier, name) {
                        Ok(_) => true,
                        Err(error) => {
                            error.sqlstate == crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN
                        }
                    }
                })
            }) {
                return true;
            }
            level = current.parent;
        }
        false
    }
}

struct Check<'a, 'v> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    variables: &'a dyn ColumnLookup<'v>,
}

/// Reports the first reference in `statement` that names both a column and
/// a PL/pgSQL variable whose rule is `error` (PostgreSQL's ambiguity error)
/// or `use_variable` (unsupported).
pub(crate) fn check_statement<'a, 'v>(
    statement: &'a Stmt<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    variables: &'a dyn ColumnLookup<'v>,
) -> Result<(), SqlError> {
    let check = Check {
        storage,
        txid,
        arena,
        variables,
    };
    let root = Level {
        scope: None,
        parent: None,
    };
    match statement {
        Stmt::Select(select) => check.select(select, &root),
        Stmt::SetQuery(query) => {
            for cte in query.with {
                check.cte(cte, &root)?;
            }
            check.set_tree(query.body, &root)
        }
        Stmt::Insert(insert) => {
            if let Some(select) = insert.select {
                check.select(select, &root)?;
            }
            for row in insert.rows {
                for value in *row {
                    check.expression(value, &root)?;
                }
            }
            let Some(scope) = check.dml_scope(insert.table, None, None) else {
                return Ok(());
            };
            let target = Level {
                scope: Some(&scope),
                parent: None,
            };
            if let Some(conflict) = insert.on_conflict {
                for (_, value) in conflict.update.unwrap_or(&[]) {
                    check.expression(value, &target)?;
                }
                if let Some(predicate) = conflict.update_where {
                    check.expression(predicate, &target)?;
                }
            }
            check.items(insert.returning.items, &target)
        }
        Stmt::Update(update) => {
            let Some(scope) = check.dml_scope(update.table, update.alias, update.from) else {
                return Ok(());
            };
            let target = Level {
                scope: Some(&scope),
                parent: None,
            };
            if let Some(from) = update.from {
                check.relation_sources(from, &root)?;
            }
            for (_, value) in update.assignments {
                check.expression(value, &target)?;
            }
            if let Some(predicate) = update.where_clause {
                check.expression(predicate, &target)?;
            }
            check.items(update.returning.items, &target)
        }
        Stmt::Delete(delete) => {
            let Some(scope) = check.dml_scope(delete.table, delete.alias, delete.using) else {
                return Ok(());
            };
            let target = Level {
                scope: Some(&scope),
                parent: None,
            };
            if let Some(using) = delete.using {
                check.relation_sources(using, &root)?;
            }
            if let Some(predicate) = delete.where_clause {
                check.expression(predicate, &target)?;
            }
            check.items(delete.returning.items, &target)
        }
        Stmt::Merge(merge) => check.merge(merge, &root),
        _ => Ok(()),
    }
}

/// [`check_statement`] for a PL/pgSQL expression, whose only query levels
/// are the subqueries it contains.
pub(crate) fn check_expression<'a, 'v>(
    expression: &'a Expr<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    variables: &'a dyn ColumnLookup<'v>,
) -> Result<(), SqlError> {
    Check {
        storage,
        txid,
        arena,
        variables,
    }
    .expression(
        expression,
        &Level {
            scope: None,
            parent: None,
        },
    )
}

impl<'a> Check<'a, '_> {
    /// A schema whose resolution fails is left to the statement's analysis;
    /// its error and any position it recorded are discarded here.
    fn quietly<T>(&self, resolve: impl FnOnce() -> Result<T, SqlError>) -> Option<T> {
        crate::sql::error_position::preserving_pending(|| resolve().ok())
    }

    fn dml_scope(
        &self,
        target: crate::sql::ast::QualName<'a>,
        alias: Option<&'a str>,
        extra: Option<&'a FromClause<'a>>,
    ) -> Option<QueryScope<'a>> {
        self.quietly(|| {
            super::dependencies::dml_dependency_scope(
                self.storage,
                target,
                alias,
                extra,
                self.txid,
                self.arena,
                Some(self.variables),
            )
        })
    }

    /// A `MERGE` as PostgreSQL's `transformMergeStmt` analyses it: the
    /// source, the join condition over target and source, each `WHEN`
    /// clause with the relations its kind can see (`NOT MATCHED [BY TARGET]`
    /// sees only the source, `NOT MATCHED BY SOURCE` only the target), and
    /// `RETURNING`.
    fn merge(
        &self,
        merge: &'a crate::sql::ast::Merge<'a>,
        root: &Level<'_, 'a>,
    ) -> Result<(), SqlError> {
        let Ok(source) = self.arena.alloc(FromClause {
            base: merge.source,
            joins: &[],
        }) else {
            return Err(super::arena_full_pub());
        };
        let source: &'a FromClause<'a> = source;
        self.relation_sources(source, root)?;
        let (Some(both), Some(target_only), Some(source_only)) = (
            self.dml_scope(merge.target, merge.target_alias, Some(source)),
            self.dml_scope(merge.target, merge.target_alias, None),
            self.quietly(|| {
                QueryScope::resolve_schema(
                    self.storage,
                    source,
                    self.txid,
                    self.arena,
                    Some(self.variables),
                )
            }),
        ) else {
            return Ok(());
        };
        let level = |scope| Level {
            scope: Some(scope),
            parent: None,
        };
        let (both, target_only, source_only) =
            (level(&both), level(&target_only), level(&source_only));
        self.expression(merge.on, &both)?;
        for when in merge.whens {
            let visible = match when.kind() {
                crate::sql::ast::MergeMatchKind::Matched => &both,
                crate::sql::ast::MergeMatchKind::NotMatchedBySource => &target_only,
                crate::sql::ast::MergeMatchKind::NotMatchedByTarget => &source_only,
            };
            if let Some(condition) = when.condition() {
                self.expression(condition, visible)?;
            }
            match when.action() {
                crate::sql::ast::MergeActionRef::Update(assignments) => {
                    for (_, value) in assignments {
                        self.expression(value, visible)?;
                    }
                }
                crate::sql::ast::MergeActionRef::Insert { values, .. } => {
                    for value in values {
                        self.expression(value, visible)?;
                    }
                }
                crate::sql::ast::MergeActionRef::Delete
                | crate::sql::ast::MergeActionRef::DoNothing => {}
            }
        }
        self.items(merge.returning.items, &both)
    }

    fn reference(
        &self,
        qualifier: Option<&str>,
        name: &str,
        level: &Level<'_, 'a>,
    ) -> Result<(), SqlError> {
        match self.variables.procedural_variable(qualifier, name) {
            Some(VariableConflict::Error) if level.has_column(qualifier, name) => Err(
                crate::sql::eval::ambiguous_procedural_reference(qualifier, name),
            ),
            Some(VariableConflict::UseVariable) if level.has_column(qualifier, name) => {
                Err(crate::sql::error_position::at_reference(
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "#variable_conflict use_variable is not supported for \"{}{}{}\", which is also a column",
                        qualifier.unwrap_or(""),
                        if qualifier.is_some() { "." } else { "" },
                        name
                    ),
                    qualifier.unwrap_or(name),
                ))
            }
            _ => Ok(()),
        }
    }

    fn expression(&self, expression: &'a Expr<'a>, level: &Level<'_, 'a>) -> Result<(), SqlError> {
        match expression {
            Expr::Column { qualifier, name } => self.reference(*qualifier, name, level),
            Expr::Subquery(select)
            | Expr::Exists(select)
            | Expr::ArraySubquery(select)
            | Expr::RowSubquery { select, .. } => self.select(select, level),
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                self.expression(operand, level)?;
                self.select(select, level)
            }
            _ => super::walk_children(expression, &mut |child| self.expression(child, level)),
        }
    }

    fn items(&self, items: &'a [SelectItem<'a>], level: &Level<'_, 'a>) -> Result<(), SqlError> {
        for item in items {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                self.expression(expression, level)?;
            }
        }
        Ok(())
    }

    fn cte(
        &self,
        cte: &'a crate::sql::ast::Cte<'a>,
        parent: &Level<'_, 'a>,
    ) -> Result<(), SqlError> {
        match cte.dml {
            Some(statement) => check_statement(
                statement,
                self.storage,
                self.txid,
                self.arena,
                self.variables,
            ),
            None => self.select(cte.query, parent),
        }
    }

    fn set_tree(&self, tree: &'a SetTree<'a>, parent: &Level<'_, 'a>) -> Result<(), SqlError> {
        match tree {
            SetTree::Select(select) => self.select(select, parent),
            SetTree::Op { left, right, .. } => {
                self.set_tree(left, parent)?;
                self.set_tree(right, parent)
            }
        }
    }

    /// The subqueries and function arguments of FROM items, which see only
    /// the enclosing levels.
    fn relation_sources(
        &self,
        from: &'a FromClause<'a>,
        parent: &Level<'_, 'a>,
    ) -> Result<(), SqlError> {
        for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
            if let Some(subquery) = table.subquery {
                self.select(subquery, parent)?;
            }
            for argument in table.func_args.unwrap_or(&[]) {
                self.expression(argument, parent)?;
            }
        }
        Ok(())
    }

    /// One query level, in PostgreSQL's analysis order: WITH, FROM, the
    /// output list, WHERE, HAVING, ORDER BY, GROUP BY, DISTINCT ON, LIMIT
    /// and OFFSET.
    fn select(&self, select: &'a Select<'a>, parent: &Level<'_, 'a>) -> Result<(), SqlError> {
        for cte in select.with {
            self.cte(cte, parent)?;
        }
        if let Some(body) = select.set_body {
            // A set operation's ORDER BY names its output columns.
            return self.set_tree(body, parent);
        }
        let scope = match &select.from {
            Some(from) => {
                self.relation_sources(from, parent)?;
                let expanded = self.quietly(|| {
                    let select = super::expand_ctes(select, self.storage, self.txid, self.arena)?;
                    let from = select.from.as_ref().expect("expansion keeps FROM");
                    let from =
                        super::cte::expand_from_schema(from, self.storage, self.txid, self.arena)?;
                    QueryScope::resolve_schema(
                        self.storage,
                        from,
                        self.txid,
                        self.arena,
                        Some(self.variables),
                    )
                });
                match expanded {
                    Some(scope) => Some(scope),
                    // The statement's own analysis reports this level.
                    None => return Ok(()),
                }
            }
            None => None,
        };
        let level = Level {
            scope: scope.as_ref(),
            parent: Some(parent),
        };
        if let Some(from) = &select.from {
            for join in from.joins {
                if let Some(on) = join.on {
                    self.expression(on, &level)?;
                }
            }
        }
        self.items(select.items, &level)?;
        for expression in select.where_clause.into_iter().chain(select.having) {
            self.expression(expression, &level)?;
        }
        for order in select.order_by {
            // A bare name that is an output column sorts by that column.
            if let Expr::Column {
                qualifier: None,
                name,
            } = order.expression
                && select.items.iter().any(|item| {
                    matches!(item, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
                })
            {
                continue;
            }
            self.expression(order.expression, &level)?;
        }
        for expression in select
            .group_by
            .iter()
            .chain(select.distinct_on)
            .copied()
            .chain(select.limit)
            .chain(select.offset)
        {
            self.expression(expression, &level)?;
        }
        Ok(())
    }
}
