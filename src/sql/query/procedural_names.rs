//! The `plpgsql.variable_conflict` rule for a statement a PL/pgSQL body runs.
//!
//! PostgreSQL's parse analysis asks PL/pgSQL about every column reference of
//! such a statement, at every query level. Under the default rule (`error`)
//! a name that is both a table column and a PL/pgSQL variable is reported as
//! ambiguous before any row is read; under `use_variable` the variable wins.
//! Execution here resolves names as rows arrive, so this pass walks the
//! statement once, in PostgreSQL's analysis order, against each level's FROM
//! schema. A level whose FROM clause does not resolve is left to the
//! statement's own analysis, which reports that error.
//!
//! Under `use_variable` each colliding reference is replaced by the
//! variable's current value as a typed constant, so aggregate inputs,
//! grouping keys and every other site that binds names to columns see the
//! variable. `use_column` matches execution's own precedence.
//!
//! A `WITH` query or a derived table in FROM runs apart from the row its
//! statement reads, so every variable it names is bound to its value here,
//! as PostgreSQL passes variables to the whole statement as parameters.

use crate::mem::arena::{Arena, ArenaFull};
use crate::sql::ast::{
    Cte, Delete, Expr, FromClause, Insert, Join, OnConflict, Returning, Select, SelectItem,
    SetTree, Stmt, TableRef, Update,
};
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
    /// The statement level, outside any FROM clause.
    const fn root() -> Self {
        Self {
            scope: None,
            parent: None,
        }
    }

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

/// A failure while rebuilding: an SQL error or an exhausted statement arena.
struct Failure(SqlError);

impl From<SqlError> for Failure {
    fn from(error: SqlError) -> Self {
        Self(error)
    }
}

impl From<ArenaFull> for Failure {
    fn from(_: ArenaFull) -> Self {
        Self(super::arena_full())
    }
}

type Bound<T> = Result<T, Failure>;

/// Rebuilds statements in `'a`; storage, variables and each level's FROM
/// schema are only consulted, for the shorter `'s`.
struct Check<'a, 's> {
    storage: &'s Storage,
    txid: u32,
    arena: &'a Arena,
    variables: &'s dyn ColumnLookup<'a>,
    /// Inside a query that runs apart from the statement's row (a `WITH`
    /// query or a derived table): the earlier `WITH` queries its FROM items
    /// may name. `None` where the statement's row is in scope.
    detached: core::cell::Cell<Option<&'a [Cte<'a>]>>,
}

/// Applies `plpgsql.variable_conflict` to `statement`: reports the first
/// reference that names both a column and a variable whose rule is `error`
/// (PostgreSQL's ambiguity error), and returns the statement with each such
/// reference under `use_variable` replaced by the variable's value. An
/// unchanged statement is returned as given.
pub(crate) fn bind_statement<'a>(
    statement: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    variables: &dyn ColumnLookup<'a>,
) -> Result<&'a Stmt<'a>, SqlError> {
    Check {
        storage,
        txid,
        arena,
        variables,
        detached: core::cell::Cell::new(None),
    }
    .statement(statement)
    .map_err(|Failure(error)| error)
}

/// [`bind_statement`] for a PL/pgSQL expression, whose only query levels
/// are the subqueries it contains.
pub(crate) fn bind_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    variables: &dyn ColumnLookup<'a>,
) -> Result<&'a Expr<'a>, SqlError> {
    Check {
        storage,
        txid,
        arena,
        variables,
        detached: core::cell::Cell::new(None),
    }
    .expression(expression, &Level::root())
    .map_err(|Failure(error)| error)
}

/// The rebuilt value, or the original when nothing inside it changed.
fn keep<'a, T: Copy>(original: &'a T, rebuilt: T, changed: bool, arena: &'a Arena) -> Bound<&'a T> {
    Ok(if changed {
        arena.alloc(rebuilt)?
    } else {
        original
    })
}

impl<'a: 's, 's> Check<'a, 's> {
    /// A schema whose resolution fails is left to the statement's analysis;
    /// its error and any position it recorded are discarded here.
    fn quietly<T>(&self, resolve: impl FnOnce() -> Result<T, SqlError>) -> Option<T> {
        crate::sql::error_position::preserving_pending(|| resolve().ok())
    }

    fn statement(&self, statement: &'a Stmt<'a>) -> Bound<&'a Stmt<'a>> {
        Ok(match statement {
            Stmt::Select(select) => {
                let rebuilt = self.select(select, &Level::root())?;
                if core::ptr::eq(rebuilt, select) {
                    statement
                } else {
                    self.arena.alloc(Stmt::Select(*rebuilt))?
                }
            }
            Stmt::Insert(insert) => {
                let rebuilt = self.insert(insert)?;
                if core::ptr::eq(rebuilt, insert) {
                    statement
                } else {
                    self.arena.alloc(Stmt::Insert(*rebuilt))?
                }
            }
            Stmt::Update(update) => {
                let rebuilt = self.update(update)?;
                if core::ptr::eq(rebuilt, update) {
                    statement
                } else {
                    self.arena.alloc(Stmt::Update(*rebuilt))?
                }
            }
            Stmt::Delete(delete) => {
                let rebuilt = self.delete(delete)?;
                if core::ptr::eq(rebuilt, delete) {
                    statement
                } else {
                    self.arena.alloc(Stmt::Delete(*rebuilt))?
                }
            }
            Stmt::Merge(merge) => {
                let rebuilt = self.merge(merge)?;
                if core::ptr::eq(rebuilt, merge) {
                    statement
                } else {
                    self.arena.alloc(Stmt::Merge(*rebuilt))?
                }
            }
            Stmt::With {
                ctes,
                statement: main,
            } => {
                let mut changed = false;
                let rebuilt_ctes = self.ctes(ctes, &Level::root(), &mut changed)?;
                let rebuilt_main = self.statement(main)?;
                if !changed && core::ptr::eq(rebuilt_main, *main) {
                    statement
                } else {
                    self.arena.alloc(Stmt::With {
                        ctes: rebuilt_ctes,
                        statement: rebuilt_main,
                    })?
                }
            }
            _ => statement,
        })
    }

    fn insert(&self, insert: &'a Insert<'a>) -> Bound<&'a Insert<'a>> {
        let mut rebuilt = *insert;
        let mut changed = false;
        if let Some(select) = insert.select {
            rebuilt.select = Some(self.select(select, &Level::root())?);
            changed |= !core::ptr::eq(rebuilt.select.unwrap(), select);
        }
        let mut rows: Option<&'a mut [&'a [&'a Expr<'a>]]> = None;
        for (index, row) in insert.rows.iter().enumerate() {
            let expanded = self.expand_row_variables(row)?;
            changed |= !core::ptr::eq(expanded, *row);
            let values = self.expressions(expanded, &Level::root(), &mut changed)?;
            if !core::ptr::eq(values, *row) && rows.is_none() {
                rows = Some(self.arena.alloc_slice_copy(insert.rows)?);
            }
            if let Some(copy) = rows.as_deref_mut() {
                copy[index] = values;
            }
        }
        if let Some(copy) = rows {
            rebuilt.rows = copy;
        }
        if let Some(scope) = self.dml_scope(insert.table, None, None) {
            let target = Level {
                scope: Some(&scope),
                parent: None,
            };
            if let Some(conflict) = insert.on_conflict {
                let mut conflict_changed = false;
                let update = conflict
                    .update
                    .map(|assignments| {
                        self.assignments(assignments, &target, &mut conflict_changed)
                    })
                    .transpose()?;
                let update_where =
                    self.optional(conflict.update_where, &target, &mut conflict_changed)?;
                if conflict_changed {
                    changed = true;
                    rebuilt.on_conflict = Some(OnConflict {
                        update,
                        update_where,
                        ..conflict
                    });
                }
            }
            rebuilt.returning = self.returning(insert.returning, &target, &mut changed)?;
        }
        keep(insert, rebuilt, changed, self.arena)
    }

    /// Expands each `r.*` of a VALUES row whose `r` is a row or record
    /// variable into its fields, as PostgreSQL expands a VALUES list.
    fn expand_row_variables(&self, row: &'a [&'a Expr<'a>]) -> Bound<&'a [&'a Expr<'a>]> {
        let variable = |expression: &Expr<'a>| match *expression {
            Expr::WholeRow(name) if self.variables.procedural_variable(None, name).is_some() => {
                Some(name)
            }
            _ => None,
        };
        if !row.iter().any(|expression| variable(expression).is_some()) {
            return Ok(row);
        }
        let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
        let hooks = crate::sql::eval::EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        let mut expanded = crate::mem::arena::ArenaList::new(self.arena);
        for &expression in row {
            let Some(name) = variable(expression) else {
                expanded.push(expression)?;
                continue;
            };
            let base = &*self.arena.alloc(Expr::Column {
                qualifier: None,
                name,
            })?;
            let fields = match self.variables.lookup(None, name)? {
                crate::sql::types::Datum::Record(fields)
                | crate::sql::types::Datum::Composite { fields, .. } => fields,
                _ => {
                    let type_oid =
                        crate::sql::eval::expression_type_identity(base, self.variables, &hooks)?
                            .record_field_oid();
                    catalog_fields(&catalog, type_oid, name, self.arena)?
                }
            };
            for field in fields {
                expanded.push(&*self.arena.alloc(Expr::Column {
                    qualifier: Some(name),
                    name: field.name,
                })?)?;
            }
        }
        Ok(expanded.as_slice())
    }

    fn update(&self, update: &'a Update<'a>) -> Bound<&'a Update<'a>> {
        let Some(scope) = self.dml_scope(update.table, update.alias, update.from) else {
            return Ok(update);
        };
        let target = Level {
            scope: Some(&scope),
            parent: None,
        };
        let mut rebuilt = *update;
        let mut changed = false;
        if let Some(from) = update.from {
            rebuilt.from = Some(self.relation_sources(from, &Level::root(), &mut changed)?);
        }
        rebuilt.assignments = self.assignments(update.assignments, &target, &mut changed)?;
        rebuilt.where_clause = self.optional(update.where_clause, &target, &mut changed)?;
        rebuilt.returning = self.returning(update.returning, &target, &mut changed)?;
        keep(update, rebuilt, changed, self.arena)
    }

    fn delete(&self, delete: &'a Delete<'a>) -> Bound<&'a Delete<'a>> {
        let Some(scope) = self.dml_scope(delete.table, delete.alias, delete.using) else {
            return Ok(delete);
        };
        let target = Level {
            scope: Some(&scope),
            parent: None,
        };
        let mut rebuilt = *delete;
        let mut changed = false;
        if let Some(using) = delete.using {
            rebuilt.using = Some(self.relation_sources(using, &Level::root(), &mut changed)?);
        }
        rebuilt.where_clause = self.optional(delete.where_clause, &target, &mut changed)?;
        rebuilt.returning = self.returning(delete.returning, &target, &mut changed)?;
        keep(delete, rebuilt, changed, self.arena)
    }

    /// A `MERGE` as PostgreSQL's `transformMergeStmt` analyses it: the
    /// source, the join condition over target and source, each `WHEN`
    /// clause with the relations its kind can see (`NOT MATCHED [BY TARGET]`
    /// sees only the source, `NOT MATCHED BY SOURCE` only the target), and
    /// `RETURNING`.
    fn merge(
        &self,
        merge: &'a crate::sql::ast::Merge<'a>,
    ) -> Bound<&'a crate::sql::ast::Merge<'a>> {
        use crate::sql::ast::{MergeSourceAction, MergeTargetAction, MergeWhen};
        let mut changed = false;
        let source = self.table(merge.source, &Level::root(), &mut changed)?;
        let source_from: &'a FromClause<'a> = self.arena.alloc(FromClause {
            base: source,
            joins: &[],
        })?;
        let (Some(both), Some(target_only), Some(source_only)) = (
            self.dml_scope(merge.target, merge.target_alias, Some(source_from)),
            self.dml_scope(merge.target, merge.target_alias, None),
            self.quietly(|| {
                QueryScope::resolve_schema(
                    self.storage,
                    source_from,
                    self.txid,
                    self.arena,
                    Some(self.variables),
                )
            }),
        ) else {
            return Ok(merge);
        };
        let level = |scope| Level {
            scope: Some(scope),
            parent: None,
        };
        let (both, target_only, source_only) =
            (level(&both), level(&target_only), level(&source_only));
        let mut rebuilt = *merge;
        rebuilt.source = source;
        rebuilt.on = self.expression(merge.on, &both)?;
        changed |= !core::ptr::eq(rebuilt.on, merge.on);
        let mut whens: Option<&'a mut [MergeWhen<'a>]> = None;
        for (index, when) in merge.whens.iter().enumerate() {
            let mut when_changed = false;
            let target_action = |action: MergeTargetAction<'a>,
                                 visible: &Level<'_, 's>,
                                 changed: &mut bool|
             -> Bound<MergeTargetAction<'a>> {
                Ok(match action {
                    MergeTargetAction::Update(assignments) => MergeTargetAction::Update(
                        self.assignments(assignments, visible, changed)?,
                    ),
                    other => other,
                })
            };
            let new = match *when {
                MergeWhen::Matched { cond, action } => MergeWhen::Matched {
                    cond: self.optional(cond, &both, &mut when_changed)?,
                    action: target_action(action, &both, &mut when_changed)?,
                },
                MergeWhen::NotMatchedBySource { cond, action } => MergeWhen::NotMatchedBySource {
                    cond: self.optional(cond, &target_only, &mut when_changed)?,
                    action: target_action(action, &target_only, &mut when_changed)?,
                },
                MergeWhen::NotMatchedByTarget { cond, action } => MergeWhen::NotMatchedByTarget {
                    cond: self.optional(cond, &source_only, &mut when_changed)?,
                    action: match action {
                        MergeSourceAction::Insert {
                            columns,
                            values,
                            default_values,
                            overriding,
                        } => MergeSourceAction::Insert {
                            columns,
                            values: self.expressions(values, &source_only, &mut when_changed)?,
                            default_values,
                            overriding,
                        },
                        MergeSourceAction::DoNothing => MergeSourceAction::DoNothing,
                    },
                },
            };
            if when_changed {
                let copy = match whens.as_deref_mut() {
                    Some(copy) => copy,
                    None => whens.insert(self.arena.alloc_slice_copy(merge.whens)?),
                };
                copy[index] = new;
                changed = true;
            }
        }
        if let Some(copy) = whens {
            rebuilt.whens = copy;
        }
        rebuilt.returning = self.returning(merge.returning, &both, &mut changed)?;
        keep(merge, rebuilt, changed, self.arena)
    }

    fn dml_scope(
        &self,
        target: crate::sql::ast::QualName<'a>,
        alias: Option<&'a str>,
        extra: Option<&'a FromClause<'a>>,
    ) -> Option<QueryScope<'s>> {
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

    /// The replacement for a reference: PostgreSQL's ambiguity error under
    /// `error`, the variable's value under `use_variable`.
    fn reference(
        &self,
        reference: &'a Expr<'a>,
        qualifier: Option<&'a str>,
        name: &'a str,
        level: &Level<'_, 's>,
    ) -> Bound<Option<&'a Expr<'a>>> {
        match self.variables.procedural_variable(qualifier, name) {
            Some(VariableConflict::Error) if level.has_column(qualifier, name) => Err(Failure(
                crate::sql::eval::ambiguous_procedural_reference(qualifier, name),
            )),
            Some(VariableConflict::UseVariable) if level.has_column(qualifier, name) => {
                self.variable_value(reference, qualifier, name).map(Some)
            }
            Some(_) if self.detached.get().is_some() && !level.has_column(qualifier, name) => {
                self.variable_value(reference, qualifier, name).map(Some)
            }
            // A trigger's `NEW` and `OLD` rows are variables too.
            None if self.detached.get().is_some()
                && matches!(qualifier, Some("new" | "old"))
                && self.variables.col_type(qualifier, name).is_some()
                && !level.has_column(qualifier, name) =>
            {
                self.variable_value(reference, qualifier, name).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// The variable's current value as a constant of its declared type,
    /// typmod and collation; the reference stays its source.
    fn variable_value(
        &self,
        reference: &'a Expr<'a>,
        qualifier: Option<&'a str>,
        name: &'a str,
    ) -> Bound<&'a Expr<'a>> {
        let value = self.variables.lookup(qualifier, name)?;
        let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
        let hooks = crate::sql::eval::EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        let type_oid =
            crate::sql::eval::expression_type_identity(reference, self.variables, &hooks)?
                .record_field_oid();
        let collation = self.variables.collation(qualifier, name);
        Ok(self.arena.alloc(Expr::BoundLiteral {
            value: crate::sql::ast::BoundLiteralValue::CachedDefault(
                crate::sql::exec::encode_projected_pub(&[value], self.arena)?,
            ),
            type_oid,
            type_mod: self.variables.column_type_mod(qualifier, name),
            collation: (collation != crate::sql::ast::Collation::None).then_some(collation),
            source: reference,
        })?)
    }

    fn expression(&self, expression: &'a Expr<'a>, level: &Level<'_, 's>) -> Bound<&'a Expr<'a>> {
        crate::sql::ast::rewrite_expression(expression, self.arena, &mut |node| match node {
            Expr::Column { qualifier, name } => self.reference(node, *qualifier, name, level),
            Expr::Subquery(select) => self.subquery(select, level, Expr::Subquery),
            Expr::Exists(select) => self.subquery(select, level, Expr::Exists),
            Expr::ArraySubquery(select) => self.subquery(select, level, Expr::ArraySubquery),
            Expr::RowSubquery { select, arity } => {
                let arity = *arity;
                self.subquery(select, level, |select| Expr::RowSubquery { select, arity })
            }
            Expr::InSubquery {
                operand,
                select,
                negated,
            } => {
                let new_operand = self.expression(operand, level)?;
                let new_select = self.select(select, level)?;
                Ok(
                    (!core::ptr::eq(new_operand, *operand) || !core::ptr::eq(new_select, *select))
                        .then(|| {
                            self.arena.alloc(Expr::InSubquery {
                                operand: new_operand,
                                select: new_select,
                                negated: *negated,
                            })
                        })
                        .transpose()?
                        .map(|node| &*node),
                )
            }
            Expr::QuantifiedSubquery {
                operand,
                operator,
                select,
                all,
            } => {
                let new_operand = self.expression(operand, level)?;
                let new_select = self.select(select, level)?;
                Ok(
                    (!core::ptr::eq(new_operand, *operand) || !core::ptr::eq(new_select, *select))
                        .then(|| {
                            self.arena.alloc(Expr::QuantifiedSubquery {
                                operand: new_operand,
                                operator: *operator,
                                select: new_select,
                                all: *all,
                            })
                        })
                        .transpose()?
                        .map(|node| &*node),
                )
            }
            _ => Ok(None),
        })
    }

    fn subquery(
        &self,
        select: &'a Select<'a>,
        level: &Level<'_, 's>,
        node: impl FnOnce(&'a Select<'a>) -> Expr<'a>,
    ) -> Bound<Option<&'a Expr<'a>>> {
        let rebuilt = self.select(select, level)?;
        if core::ptr::eq(rebuilt, select) {
            return Ok(None);
        }
        Ok(Some(self.arena.alloc(node(rebuilt))?))
    }

    fn expressions(
        &self,
        expressions: &'a [&'a Expr<'a>],
        level: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<&'a [&'a Expr<'a>]> {
        crate::sql::ast::rewrite_expressions(
            expressions,
            self.arena,
            &mut |node| {
                let rebuilt = self.expression(node, level)?;
                Ok((!core::ptr::eq(rebuilt, node)).then_some(rebuilt))
            },
            changed,
        )
    }

    fn optional(
        &self,
        expression: Option<&'a Expr<'a>>,
        level: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<Option<&'a Expr<'a>>> {
        let Some(expression) = expression else {
            return Ok(None);
        };
        let rebuilt = self.expression(expression, level)?;
        *changed |= !core::ptr::eq(rebuilt, expression);
        Ok(Some(rebuilt))
    }

    fn assignments(
        &self,
        assignments: &'a [(&'a str, &'a Expr<'a>)],
        level: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<&'a [(&'a str, &'a Expr<'a>)]> {
        let mut rebuilt: Option<&'a mut [(&'a str, &'a Expr<'a>)]> = None;
        for (index, (_, value)) in assignments.iter().enumerate() {
            let new = self.expression(value, level)?;
            if !core::ptr::eq(new, *value) && rebuilt.is_none() {
                rebuilt = Some(self.arena.alloc_slice_copy(assignments)?);
            }
            if let Some(copy) = rebuilt.as_deref_mut() {
                copy[index].1 = new;
            }
        }
        Ok(match rebuilt {
            Some(copy) => {
                *changed = true;
                copy
            }
            None => assignments,
        })
    }

    fn items(
        &self,
        items: &'a [SelectItem<'a>],
        level: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<&'a [SelectItem<'a>]> {
        let mut rebuilt: Option<&'a mut [SelectItem<'a>]> = None;
        for (index, item) in items.iter().enumerate() {
            let new = match item {
                SelectItem::Expr { expression, alias } => {
                    let new = self.expression(expression, level)?;
                    (!core::ptr::eq(new, *expression)).then_some(SelectItem::Expr {
                        expression: new,
                        alias: *alias,
                    })
                }
                SelectItem::RecordStar(expression) => {
                    let new = self.expression(expression, level)?;
                    (!core::ptr::eq(new, *expression)).then_some(SelectItem::RecordStar(new))
                }
                SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => None,
            };
            if let Some(new) = new {
                let copy = match rebuilt.as_deref_mut() {
                    Some(copy) => copy,
                    None => rebuilt.insert(self.arena.alloc_slice_copy(items)?),
                };
                copy[index] = new;
            }
        }
        Ok(match rebuilt {
            Some(copy) => {
                *changed = true;
                copy
            }
            None => items,
        })
    }

    fn returning(
        &self,
        returning: Returning<'a>,
        level: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<Returning<'a>> {
        Ok(Returning {
            items: self.items(returning.items, level, changed)?,
            ..returning
        })
    }

    fn ctes(
        &self,
        ctes: &'a [Cte<'a>],
        parent: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<&'a [Cte<'a>]> {
        let mut rebuilt: Option<&'a mut [Cte<'a>]> = None;
        let outer = self.detached.get();
        for (index, cte) in ctes.iter().enumerate() {
            self.detached.set(Some(&ctes[..index]));
            let new = match cte.dml {
                Some(statement) => {
                    let new = self.statement(statement)?;
                    (!core::ptr::eq(new, statement)).then_some(Cte {
                        dml: Some(new),
                        ..*cte
                    })
                }
                None => {
                    let new = self.select(cte.query, parent)?;
                    (!core::ptr::eq(new, cte.query)).then_some(Cte { query: new, ..*cte })
                }
            };
            if let Some(new) = new {
                let copy = match rebuilt.as_deref_mut() {
                    Some(copy) => copy,
                    None => rebuilt.insert(self.arena.alloc_slice_copy(ctes)?),
                };
                copy[index] = new;
            }
        }
        self.detached.set(outer);
        Ok(match rebuilt {
            Some(copy) => {
                *changed = true;
                copy
            }
            None => ctes,
        })
    }

    fn set_tree(&self, tree: &'a SetTree<'a>, parent: &Level<'_, 's>) -> Bound<&'a SetTree<'a>> {
        match tree {
            SetTree::Select(select) => {
                let new = self.select(select, parent)?;
                if core::ptr::eq(new, *select) {
                    return Ok(tree);
                }
                Ok(self.arena.alloc(SetTree::Select(new))?)
            }
            SetTree::Op {
                syntax,
                operator,
                all,
                left,
                right,
            } => {
                let new_left = self.set_tree(left, parent)?;
                let new_right = self.set_tree(right, parent)?;
                if core::ptr::eq(new_left, *left) && core::ptr::eq(new_right, *right) {
                    return Ok(tree);
                }
                Ok(self.arena.alloc(SetTree::Op {
                    syntax: *syntax,
                    operator: *operator,
                    all: *all,
                    left: new_left,
                    right: new_right,
                })?)
            }
        }
    }

    fn table(
        &self,
        table: TableRef<'a>,
        parent: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<TableRef<'a>> {
        let mut rebuilt = table;
        if let Some(subquery) = table.subquery {
            // A LATERAL subquery may name the FROM items beside it, which
            // this pass does not resolve; it keeps reading the row.
            let outer = self.detached.get();
            if !table.lateral {
                self.detached.set(Some(outer.unwrap_or(&[])));
            }
            let new = self.select(subquery, parent);
            self.detached.set(outer);
            let new = new?;
            *changed |= !core::ptr::eq(new, subquery);
            rebuilt.subquery = Some(new);
        }
        if let Some(arguments) = table.func_args {
            rebuilt.func_args = Some(self.expressions(arguments, parent, changed)?);
        }
        Ok(rebuilt)
    }

    /// The subqueries and function arguments of FROM items, which see only
    /// the enclosing levels.
    fn relation_sources(
        &self,
        from: &'a FromClause<'a>,
        parent: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<&'a FromClause<'a>> {
        let mut from_changed = false;
        let rebuilt = self.join_sources(*from, parent, &mut from_changed)?;
        *changed |= from_changed;
        keep(from, rebuilt, from_changed, self.arena)
    }

    fn join_sources(
        &self,
        from: FromClause<'a>,
        parent: &Level<'_, 's>,
        changed: &mut bool,
    ) -> Bound<FromClause<'a>> {
        let base = self.table(from.base, parent, changed)?;
        let mut joins: Option<&'a mut [Join<'a>]> = None;
        for (index, join) in from.joins.iter().enumerate() {
            let mut join_changed = false;
            let table = self.table(join.table, parent, &mut join_changed)?;
            if join_changed {
                let copy = match joins.as_deref_mut() {
                    Some(copy) => copy,
                    None => joins.insert(self.arena.alloc_slice_copy(from.joins)?),
                };
                copy[index].table = table;
                *changed = true;
            }
        }
        Ok(FromClause {
            base,
            joins: match joins {
                Some(copy) => copy,
                None => from.joins,
            },
        })
    }

    /// One query level, in PostgreSQL's analysis order: WITH, FROM, the
    /// output list, WHERE, HAVING, ORDER BY, GROUP BY, DISTINCT ON, LIMIT
    /// and OFFSET.
    fn select(&self, select: &'a Select<'a>, parent: &Level<'_, 's>) -> Bound<&'a Select<'a>> {
        let mut rebuilt = *select;
        let mut changed = false;
        rebuilt.with = self.ctes(select.with, parent, &mut changed)?;
        if let Some(body) = select.set_body {
            // A set operation's ORDER BY names its output columns.
            let new = self.set_tree(body, parent)?;
            changed |= !core::ptr::eq(new, body);
            rebuilt.set_body = Some(new);
            return keep(select, rebuilt, changed, self.arena);
        }
        let scope = match &select.from {
            Some(from) => {
                rebuilt.from = Some(self.join_sources(*from, parent, &mut changed)?);
                let expanded = self.quietly(|| {
                    let select = match self.detached.get() {
                        Some(earlier) if !earlier.is_empty() => {
                            // The earlier queries of the enclosing clause
                            // resolve this level's FROM items.
                            let mut with = crate::mem::arena::ArenaList::new(self.arena);
                            for cte in earlier.iter().chain(select.with) {
                                with.push(*cte).map_err(|_| super::arena_full())?;
                            }
                            &*self
                                .arena
                                .alloc(Select {
                                    with: with.as_slice(),
                                    ..*select
                                })
                                .map_err(|_| super::arena_full())?
                        }
                        _ => select,
                    };
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
                    None => return keep(select, rebuilt, changed, self.arena),
                }
            }
            None => None,
        };
        let level = Level {
            scope: scope.as_ref(),
            parent: Some(parent),
        };
        if let Some(from) = rebuilt.from {
            let mut joins: Option<&'a mut [Join<'a>]> = None;
            for (index, join) in from.joins.iter().enumerate() {
                let Some(on) = join.on else {
                    continue;
                };
                let new = self.expression(on, &level)?;
                if !core::ptr::eq(new, on) {
                    let copy = match joins.as_deref_mut() {
                        Some(copy) => copy,
                        None => joins.insert(self.arena.alloc_slice_copy(from.joins)?),
                    };
                    copy[index].on = Some(new);
                }
            }
            if let Some(copy) = joins {
                changed = true;
                rebuilt.from = Some(FromClause {
                    joins: copy,
                    ..from
                });
            }
        }
        rebuilt.items = self.items(select.items, &level, &mut changed)?;
        rebuilt.where_clause = self.optional(select.where_clause, &level, &mut changed)?;
        rebuilt.having = self.optional(select.having, &level, &mut changed)?;
        let mut order: Option<&'a mut [crate::sql::ast::OrderBy<'a>]> = None;
        for (index, key) in select.order_by.iter().enumerate() {
            // A bare name that is an output column sorts by that column.
            if let Expr::Column {
                qualifier: None,
                name,
            } = key.expression
                && select.items.iter().any(|item| {
                    matches!(item, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
                })
            {
                continue;
            }
            let new = self.expression(key.expression, &level)?;
            if !core::ptr::eq(new, key.expression) {
                let copy = match order.as_deref_mut() {
                    Some(copy) => copy,
                    None => order.insert(self.arena.alloc_slice_copy(select.order_by)?),
                };
                copy[index].expression = new;
            }
        }
        if let Some(copy) = order {
            changed = true;
            rebuilt.order_by = copy;
        }
        rebuilt.group_by = self.expressions(select.group_by, &level, &mut changed)?;
        rebuilt.distinct_on = self.expressions(select.distinct_on, &level, &mut changed)?;
        rebuilt.limit = self.optional(select.limit, &level, &mut changed)?;
        rebuilt.offset = self.optional(select.offset, &level, &mut changed)?;
        keep(select, rebuilt, changed, self.arena)
    }
}

/// The declared fields of an unassigned row variable `name` of type
/// `type_oid`; a record variable without a value has none to expand.
fn catalog_fields<'a>(
    catalog: &dyn crate::sql::eval::CatalogAccess,
    type_oid: i32,
    name: &str,
    arena: &'a Arena,
) -> Result<&'a [crate::sql::types::RecordField<'a>], SqlError> {
    match catalog.null_composite_fields(type_oid, arena)? {
        Some(fields) => Ok(fields),
        None => Err(crate::sql::error_position::with_detail(
            crate::sql_err!(
                crate::sql::eval::sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "record \"{}\" is not assigned yet",
                name
            ),
            format_args!("The tuple structure of a not-yet-assigned record is indeterminate."),
        )),
    }
}
