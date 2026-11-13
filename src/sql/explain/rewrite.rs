//! EXPLAIN of a statement that rewrite rules or an auto-updatable view
//! change: the queries PostgreSQL's rewriter produces, each as a statement
//! the planner explains.
//!
//! Each rule action reads the rewritten statement's rows as PostgreSQL's
//! `rewriteRuleAction` joins them in: an `INSERT`'s single `VALUES` row is
//! substituted for `NEW`, several rows become the `*VALUES*` list, a
//! query PostgreSQL cannot pull up is read as the `*SELECT*` subquery, and
//! an `UPDATE` or `DELETE` joins its target relation (and its own `FROM`
//! or `USING` items) into the action. Every action is rewritten again by
//! the rules of its own target, in PostgreSQL's order.

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{
    CaseOrigin, Delete, Expr, FromClause, Insert, Join, JoinKind, QualName, Select, SelectItem,
    SetOp, SetOperationSyntax, SetTree, Stmt, TableRef, Update,
};
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql::query;
use crate::sql::rule_rewrite;
use crate::sql_err;
use crate::storage::{ResolvedRelation, RewriteEvent, RewriteMode, RuleTarget, Storage};

use super::{Plan, PlanNode, RewriteFailure, plan_derived, plan_select, selects_aggregate};
use crate::util::StackStr;

/// The statements `statement` is rewritten into, in PostgreSQL's order, or
/// None when no rule or view rewrite applies.
pub(super) fn rewritten_statements<'a>(
    storage: &Storage,
    txid: u32,
    statement: &'a Stmt<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a [&'a Stmt<'a>]>, SqlError> {
    if !rule_rewrite::statement_is_rewritten(storage, txid, false, statement) {
        return Ok(None);
    }
    // The rewriter's own errors (recursion, views it cannot update, RETURNING
    // lists) come before anything is planned.
    rule_rewrite::walk_rewritten(storage, txid, false, statement, arena, &mut |_| Ok(()))?;
    let mut out = ArenaList::new(arena);
    Rewriter {
        storage,
        txid,
        arena,
    }
    .query(statement, &mut out)?;
    let out = out.as_slice();
    if let [only] = out
        && core::ptr::eq(*only, statement)
    {
        return Ok(None);
    }
    Ok(Some(out))
}

struct Rewriter<'s, 'a> {
    storage: &'s Storage,
    txid: u32,
    arena: &'a Arena,
}

/// How an `INSERT`'s `NEW` row is read.
#[derive(Clone, Copy)]
enum Inserted<'a> {
    /// A single `VALUES` row, in the statement's column order.
    Row(&'a [&'a Expr<'a>]),
    /// A query's output expressions over its own FROM items.
    Query {
        items: &'a [&'a Expr<'a>],
        from: Option<FromClause<'a>>,
        where_clause: Option<&'a Expr<'a>>,
    },
}

/// The statement a rule rewrites, as its actions read it.
struct Original<'a> {
    statement: &'a Stmt<'a>,
    event: RewriteEvent,
    /// The name the statement's target is exposed as.
    exposed: &'a str,
    /// The target relation's columns, and each one's default.
    columns: &'a [&'a str],
    defaults: &'a [Option<&'a Expr<'a>>],
    /// The INSERT's column list, in order.
    inserted_columns: &'a [&'a str],
    inserted: Option<Inserted<'a>>,
    /// UPDATE and DELETE: the target as a FROM item, its other FROM or
    /// USING items, and its qualified condition.
    target: Option<TableRef<'a>>,
    extras: Option<FromClause<'a>>,
    condition: Option<&'a Expr<'a>>,
}

/// How an UPDATE or DELETE action joins the original's target relation
/// (PostgreSQL's `keeporig` and the OLD entry the rule's parse added).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetJoin {
    /// The action reads OLD (or NEW of an UPDATE): the target joins last.
    Last,
    /// Only the conditions read it: the original's FROM list comes first.
    First,
    /// Nothing reads it.
    None,
}

fn alloc<'a, T>(arena: &'a Arena, value: T) -> Result<&'a T, SqlError> {
    arena.alloc(value).map(|value| &*value).map_err(|_| arena_full())
}

fn column<'a>(arena: &'a Arena, qualifier: &'a str, name: &'a str) -> Result<&'a Expr<'a>, SqlError> {
    alloc(
        arena,
        Expr::Column {
            qualifier: Some(qualifier),
            name,
        },
    )
}

fn unsupported(what: &str) -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "EXPLAIN of a statement a rewrite rule changes is not supported for {}",
        what
    )
}

/// The FROM list of `parts` in order, each joined to the ones before it by
/// a comma.
fn concat_from<'a>(
    parts: &[Option<FromClause<'a>>],
    arena: &'a Arena,
) -> Result<Option<FromClause<'a>>, SqlError> {
    let mut base = None;
    let mut joins = ArenaList::new(arena);
    for part in parts.iter().flatten() {
        match base {
            None => base = Some(part.base),
            Some(_) => joins
                .push(Join {
                    table: part.base,
                    kind: JoinKind::Comma,
                    on: None,
                    using: None,
                    natural: false,
                })
                .map_err(|_| arena_full())?,
        }
        for join in part.joins {
            joins.push(*join).map_err(|_| arena_full())?;
        }
    }
    Ok(base.map(|base| FromClause {
        base,
        joins: joins.as_slice(),
    }))
}

fn single_from(table: TableRef<'_>) -> FromClause<'_> {
    FromClause {
        base: table,
        joins: &[],
    }
}

/// Whether `expression` names a column of `qualifier` (any of `names`), or
/// an unqualified column in `columns` when given.
fn mentions(expression: &Expr<'_>, qualifiers: &[&str], columns: &[&str]) -> bool {
    let mut found = false;
    expression.for_each_column_reference(&mut |qualifier, name| {
        found |= match qualifier {
            Some(qualifier) => qualifiers
                .iter()
                .any(|known| known.eq_ignore_ascii_case(qualifier)),
            None => columns.contains(&name),
        };
    });
    found
}

/// Every expression of a rule action, for [`mentions`].
fn action_expressions<'a>(statement: &Stmt<'a>, visit: &mut dyn FnMut(&'a Expr<'a>)) {
    fn select<'a>(select: &Select<'a>, visit: &mut dyn FnMut(&'a Expr<'a>)) {
        for item in select.items {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                visit(expression);
            }
        }
        select.where_clause.into_iter().for_each(&mut *visit);
        select.having.into_iter().for_each(&mut *visit);
        select.group_by.iter().copied().for_each(&mut *visit);
        if let Some(from) = &select.from {
            from.joins.iter().filter_map(|join| join.on).for_each(&mut *visit);
        }
    }
    match statement {
        Stmt::Insert(insert) => {
            for row in insert.rows {
                row.iter().copied().for_each(&mut *visit);
            }
            if let Some(query) = insert.select {
                select(query, visit);
            }
        }
        Stmt::Update(update) => {
            update.assignments.iter().for_each(|(_, value)| visit(value));
            update.where_clause.into_iter().for_each(&mut *visit);
        }
        Stmt::Delete(delete) => delete.where_clause.into_iter().for_each(&mut *visit),
        Stmt::Select(query) => select(query, visit),
        _ => {}
    }
}

/// PostgreSQL's name of a query's output column (`FigureColname`).
pub(super) fn output_name<'a>(item: &SelectItem<'a>) -> &'a str {
    fn figure<'a>(expression: &Expr<'a>) -> Option<&'a str> {
        match expression {
            Expr::Column { name, .. } => Some(name),
            Expr::Call { name, .. } => Some(name),
            Expr::Cast {
                operand, type_name, ..
            } => figure(operand).or(Some(type_name)),
            _ => None,
        }
    }
    match item {
        SelectItem::Expr {
            alias: Some(alias), ..
        } => alias,
        SelectItem::Expr { expression, .. } => figure(expression).unwrap_or("?column?"),
        _ => "?column?",
    }
}

/// Whether PostgreSQL pulls `select` up into the query reading it
/// (`is_simple_subquery`), so its FROM items are read in place.
pub(super) fn pulled_up(
    storage: &Storage,
    txid: u32,
    select: &Select<'_>,
    arena: &Arena,
) -> Result<bool, SqlError> {
    Ok(!select.distinct
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
        && !selects_aggregate(select, storage, txid, arena)?)
}

/// A derived table read as `alias`, as a FROM item.
fn derived<'a>(
    body: &'a Select<'a>,
    alias: &'a str,
    columns: Option<&'a [&'a str]>,
) -> TableRef<'a> {
    TableRef {
        subquery: Some(body),
        col_alias: columns,
        ..query::rule_relation_ref(QualName { schema: None, name: alias }, Some(alias))
    }
}

/// A `VALUES` list of `rows` as a query.
fn values_query<'a>(
    rows: &[&'a [&'a Expr<'a>]],
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let mut tree: Option<&'a SetTree<'a>> = None;
    for row in rows {
        let items = arena
            .alloc_slice_with(row.len(), |index| SelectItem::Expr {
                expression: row[index],
                alias: None,
            })
            .map_err(|_| arena_full())?;
        let leaf = alloc(arena, SetTree::Select(alloc(arena, query::rule_select(items, None, None))?))?;
        tree = Some(match tree {
            None => leaf,
            Some(left) => alloc(
                arena,
                SetTree::Op {
                    syntax: SetOperationSyntax::Values,
                    operator: SetOp::Union,
                    all: true,
                    left,
                    right: leaf,
                },
            )?,
        });
    }
    let tree = tree.ok_or_else(|| {
        sql_err!(sqlstate::INTERNAL_ERROR, "a VALUES list has no rows")
    })?;
    alloc(
        arena,
        Select {
            set_body: Some(tree),
            ..query::rule_select(&[], None, None)
        },
    )
}

fn values_column_name(index: usize, arena: &Arena) -> Result<&str, SqlError> {
    let name = crate::stack_format!(16, "column{}", index + 1);
    arena.alloc_str(name.as_str()).map_err(|_| arena_full())
}

impl<'a> Rewriter<'_, 'a> {
    /// PostgreSQL's `RewriteQuery`: `statement`'s rule actions and view
    /// rewrite, each rewritten again, with the statement itself first for
    /// INSERT and last otherwise unless an INSTEAD rule replaces it.
    fn query(
        &self,
        statement: &'a Stmt<'a>,
        out: &mut ArenaList<'a, &'a Stmt<'a>>,
    ) -> Result<(), SqlError> {
        let storage = self.storage;
        let txid = self.txid;
        let push = |out: &mut ArenaList<'a, &'a Stmt<'a>>, statement| {
            out.push(statement).map_err(|_| arena_full())
        };
        let Some((name, event)) = rule_rewrite::statement_target(statement) else {
            return push(out, statement);
        };
        let (relation, target) = match storage.resolve_relation(name.schema, name.name, txid) {
            Some(ResolvedRelation::Table(slot)) => {
                (ResolvedRelation::Table(slot), RuleTarget::Table(slot as u16))
            }
            Some(ResolvedRelation::View(slot)) => {
                (ResolvedRelation::View(slot), RuleTarget::View(slot as u16))
            }
            _ => return push(out, statement),
        };
        let original = self.original(statement, relation, event)?;
        let mut products = ArenaList::new(self.arena);
        let mut instead = false;
        let mut exclusions: Option<&'a Expr<'a>> = None;
        rule_rewrite::rules_in_order(storage, target, event, false, txid, |slot, definition| {
            let condition = match definition.condition_sql() {
                Some(sql) => {
                    let sql = self.arena.alloc_str(sql).map_err(|_| arena_full())?;
                    Some(crate::sql::parser::parse_expr(sql, self.arena)?)
                }
                None => None,
            };
            let substituted = condition
                .map(|condition| self.substitute(&original, condition))
                .transpose()?;
            if definition.mode == RewriteMode::Instead {
                match substituted {
                    None => instead = true,
                    // `AddInvertedQual`: the original keeps the rows for
                    // which the condition is not true.
                    Some(condition) => {
                        let whens = self
                            .arena
                            .alloc_slice_copy(&[(condition, &Expr::Bool(false))])
                            .map_err(|_| arena_full())?;
                        let not_true = alloc(
                            self.arena,
                            Expr::Case {
                                operand: None,
                                whens,
                                otherwise: Some(&Expr::Bool(true)),
                                origin: CaseOrigin::BooleanTest {
                                    value: true,
                                    negated: true,
                                },
                            },
                        )?;
                        exclusions = query::and_where(exclusions, Some(not_true), self.arena)?;
                    }
                }
            }
            let dependencies = storage.snapshot_rule_dependencies(slot, txid, self.arena)?;
            for sql in definition.action_sql() {
                let action = query::parse_stored_rule_statement(
                    &definition,
                    sql,
                    storage,
                    txid,
                    dependencies.view(),
                    self.arena,
                )?;
                let action = self.bind_action_target(action, dependencies.view())?;
                let product = self.action(&original, action, condition, substituted)?;
                products.push(product).map_err(|_| arena_full())?;
            }
            Ok(())
        })?;
        let base = match relation {
            ResolvedRelation::View(view)
                if !instead && !rule_rewrite::view_has_instead_trigger(storage, view, event, txid) =>
            {
                instead = true;
                Some(self.view_base(statement)?)
            }
            _ => None,
        };
        let kept = if instead {
            None
        } else if let Some(exclusions) = exclusions {
            Some(self.restricted(&original, exclusions)?)
        } else {
            Some(statement)
        };
        let insert = event == RewriteEvent::Insert;
        if insert && let Some(kept) = kept {
            push(out, kept)?;
        }
        if insert && let Some(base) = base {
            self.query(base, out)?;
        }
        for product in products.as_slice() {
            self.query(product, out)?;
        }
        if !insert && let Some(base) = base {
            self.query(base, out)?;
        }
        if !insert && let Some(kept) = kept {
            push(out, kept)?;
        }
        Ok(())
    }

    /// A stored action's target under its current name, as execution
    /// binds it through the rule's dependencies.
    fn bind_action_target(
        &self,
        action: &'a Stmt<'a>,
        dependencies: crate::storage::StoredQueryDependencyView<'_>,
    ) -> Result<&'a Stmt<'a>, SqlError> {
        let Some((name, _)) = rule_rewrite::statement_target(action) else {
            return Ok(action);
        };
        let Some(relation) =
            rule_rewrite::rule_action_target(self.storage, self.txid, dependencies, name)?
        else {
            return Ok(action);
        };
        let (schema, relation) = match relation {
            ResolvedRelation::Table(slot) => {
                let definition = self.storage.table_def(slot, self.txid);
                (definition.schema, definition.name)
            }
            ResolvedRelation::View(slot) => {
                let view = self.storage.view(slot);
                (view.schema_for(self.txid), view.name_for(self.txid))
            }
            ResolvedRelation::Catalog => return Ok(action),
        };
        let bound = QualName {
            schema: Some(self.arena.alloc_str(schema.as_str()).map_err(|_| arena_full())?),
            name: self
                .arena
                .alloc_str(relation.as_str())
                .map_err(|_| arena_full())?,
        };
        // The action's own references to its target keep the written name.
        let exposed = |alias: Option<&'a str>| alias.or(Some(name.name));
        alloc(
            self.arena,
            match action {
                Stmt::Insert(insert) => Stmt::Insert(Insert {
                    table: bound,
                    ..*insert
                }),
                Stmt::Update(update) => Stmt::Update(Update {
                    table: bound,
                    alias: exposed(update.alias).filter(|alias| *alias != bound.name),
                    ..*update
                }),
                Stmt::Delete(delete) => Stmt::Delete(Delete {
                    table: bound,
                    alias: exposed(delete.alias).filter(|alias| *alias != bound.name),
                    ..*delete
                }),
                _ => return Ok(action),
            },
        )
    }

    /// The statement as its rule actions read it.
    fn original(
        &self,
        statement: &'a Stmt<'a>,
        relation: ResolvedRelation,
        event: RewriteEvent,
    ) -> Result<Original<'a>, SqlError> {
        let arena = self.arena;
        let (columns, defaults): (&'a [&'a str], &'a [Option<&'a Expr<'a>>]) = match relation {
            ResolvedRelation::Table(slot) => {
                let definition = self.storage.table_def(slot, self.txid);
                let columns = definition.columns();
                let names = arena
                    .alloc_slice_with(columns.len(), |_| "")
                    .map_err(|_| arena_full())?;
                let defaults = arena
                    .alloc_slice_with(columns.len(), |_| None)
                    .map_err(|_| arena_full())?;
                for (index, column) in columns.iter().enumerate() {
                    names[index] = arena
                        .alloc_str(column.name.as_str())
                        .map_err(|_| arena_full())?;
                    if !column.default.is_generated()
                        && let Some(sql) = column.default.expression()
                    {
                        let sql = arena.alloc_str(sql.as_str()).map_err(|_| arena_full())?;
                        defaults[index] = Some(crate::sql::parser::parse_expr(sql, arena)?);
                    }
                }
                (&*names, &*defaults)
            }
            ResolvedRelation::View(slot) => {
                let mut descriptions = [crate::sql::types::ColDesc::new("", 0, 0);
                    crate::sql::exec::MAX_PROJ];
                let count = crate::sql::catalog::describe_view(
                    &self.storage.view(slot),
                    self.txid,
                    arena,
                    &mut descriptions,
                )?;
                let names = arena
                    .alloc_slice_with(count, |index| descriptions[index].name)
                    .map_err(|_| arena_full())?;
                let defaults = arena
                    .alloc_slice_with(count, |_| None)
                    .map_err(|_| arena_full())?;
                (&*names, &*defaults)
            }
            ResolvedRelation::Catalog => unreachable!("rules target tables and views"),
        };
        let mut original = Original {
            statement,
            event,
            exposed: "",
            columns,
            defaults,
            inserted_columns: columns,
            inserted: None,
            target: None,
            extras: None,
            condition: None,
        };
        match statement {
            Stmt::Insert(insert) => {
                original.exposed = insert.alias.unwrap_or(insert.table.name);
                if !insert.columns.is_empty() {
                    original.inserted_columns = insert.columns;
                }
                original.inserted = Some(self.inserted(insert, &original)?);
            }
            Stmt::Update(update) => {
                original.exposed = update.alias.unwrap_or(update.table.name);
                original.target = Some(query::rule_relation_ref(update.table, update.alias));
                original.extras = update.from.copied();
                original.condition =
                    update.where_clause.map(|condition| self.qualify(&original, condition)).transpose()?;
            }
            Stmt::Delete(delete) => {
                original.exposed = delete.alias.unwrap_or(delete.table.name);
                original.target = Some(query::rule_relation_ref(delete.table, delete.alias));
                original.extras = delete.using.copied();
                original.condition =
                    delete.where_clause.map(|condition| self.qualify(&original, condition)).transpose()?;
            }
            _ => unreachable!("only INSERT, UPDATE and DELETE are rewritten"),
        }
        Ok(original)
    }

    /// How the INSERT's rows read as `NEW`.
    fn inserted(&self, insert: &Insert<'a>, original: &Original<'a>) -> Result<Inserted<'a>, SqlError> {
        let arena = self.arena;
        if let Some(select) = insert.select {
            if pulled_up(self.storage, self.txid, select, arena)? {
                let items = arena
                    .alloc_slice_with(select.items.len(), |index| match select.items[index] {
                        SelectItem::Expr { expression, .. } => expression,
                        _ => &Expr::Null,
                    })
                    .map_err(|_| arena_full())?;
                return Ok(Inserted::Query {
                    items,
                    from: select.from,
                    where_clause: select.where_clause,
                });
            }
            // PostgreSQL reads a query it cannot pull up as the `*SELECT*`
            // subquery; `*` items take the target's column names.
            let named = select
                .items
                .iter()
                .all(|item| matches!(item, SelectItem::Expr { .. }));
            let names: &'a [&'a str] = if named {
                arena
                    .alloc_slice_with(select.items.len(), |index| output_name(&select.items[index]))
                    .map_err(|_| arena_full())?
            } else {
                original.inserted_columns
            };
            let columns = (!named).then_some(names);
            let items = arena
                .alloc_slice_with(names.len(), |_| &Expr::Null)
                .map_err(|_| arena_full())?;
            for (item, name) in items.iter_mut().zip(names) {
                *item = column(arena, "*SELECT*", name)?;
            }
            return Ok(Inserted::Query {
                items,
                from: Some(single_from(derived(select, "*SELECT*", columns))),
                where_clause: None,
            });
        }
        let defaulted = |row: &'a [&'a Expr<'a>]| -> Result<&'a [&'a Expr<'a>], SqlError> {
            let values = arena
                .alloc_slice_with(row.len(), |index| row[index])
                .map_err(|_| arena_full())?;
            for (index, value) in values.iter_mut().enumerate() {
                if matches!(value, Expr::DefaultMarker) {
                    *value = original
                        .inserted_columns
                        .get(index)
                        .and_then(|name| self.default_of(original, name))
                        .unwrap_or(&Expr::Null);
                }
            }
            Ok(values)
        };
        match insert.rows {
            [row] => Ok(Inserted::Row(defaulted(row)?)),
            rows => {
                // Several rows are scanned from their `*VALUES*` list.
                let mut defaulted_rows = ArenaList::new(arena);
                for row in rows {
                    defaulted_rows.push(defaulted(row)?).map_err(|_| arena_full())?;
                }
                let width = rows.first().map_or(0, |row| row.len());
                let names = arena.alloc_slice_with(width, |_| "").map_err(|_| arena_full())?;
                let items = arena
                    .alloc_slice_with(width, |_| &Expr::Null)
                    .map_err(|_| arena_full())?;
                for index in 0..width {
                    names[index] = values_column_name(index, arena)?;
                    items[index] = column(arena, "*VALUES*", names[index])?;
                }
                let body = values_query(defaulted_rows.as_slice(), arena)?;
                Ok(Inserted::Query {
                    items,
                    from: Some(single_from(derived(body, "*VALUES*", Some(names)))),
                    where_clause: None,
                })
            }
        }
    }

    fn default_of(&self, original: &Original<'a>, name: &str) -> Option<&'a Expr<'a>> {
        original
            .columns
            .iter()
            .position(|column| *column == name)
            .and_then(|index| original.defaults[index])
    }

    /// The original's unqualified target columns, named through its target.
    fn qualify(&self, original: &Original<'a>, expression: &'a Expr<'a>) -> Result<&'a Expr<'a>, SqlError> {
        qualify_columns(expression, original.columns, original.exposed, self.arena)
    }

    fn old_value(&self, original: &Original<'a>, name: &'a str) -> Result<&'a Expr<'a>, SqlError> {
        column(self.arena, original.exposed, name)
    }

    fn new_value(&self, original: &Original<'a>, name: &'a str) -> Result<&'a Expr<'a>, SqlError> {
        match original.statement {
            Stmt::Update(update) => match update
                .assignments
                .iter()
                .find(|(assigned, _)| *assigned == name)
            {
                Some((_, value)) => self.qualify(original, value),
                None => self.old_value(original, name),
            },
            Stmt::Insert(_) => {
                let position = original
                    .inserted_columns
                    .iter()
                    .position(|column| *column == name);
                let value = match (original.inserted, position) {
                    (Some(Inserted::Row(row)), Some(position)) => row.get(position).copied(),
                    (Some(Inserted::Query { items, .. }), Some(position)) => {
                        items.get(position).copied()
                    }
                    _ => None,
                };
                Ok(value
                    .or_else(|| self.default_of(original, name))
                    .unwrap_or(&Expr::Null))
            }
            _ => Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a DELETE rule cannot read NEW"
            )),
        }
    }

    /// `expression` with the rule's OLD and NEW replaced by the original's
    /// row.
    fn substitute(&self, original: &Original<'a>, expression: &'a Expr<'a>) -> Result<&'a Expr<'a>, SqlError> {
        crate::sql::ast::rewrite_expression(expression, self.arena, &mut |node: &'a Expr<'a>| -> Result<
            Option<&'a Expr<'a>>,
            RewriteFailure,
        > {
            match node {
                Expr::Column {
                    qualifier: Some(qualifier),
                    name,
                } if qualifier.eq_ignore_ascii_case("old") => {
                    self.old_value(original, name).map(Some).map_err(RewriteFailure)
                }
                Expr::Column {
                    qualifier: Some(qualifier),
                    name,
                } if qualifier.eq_ignore_ascii_case("new") => {
                    self.new_value(original, name).map(Some).map_err(RewriteFailure)
                }
                _ => Ok(None),
            }
        })
        .map_err(|RewriteFailure(error)| error)
    }

    fn substitute_all(
        &self,
        original: &Original<'a>,
        values: &'a [&'a Expr<'a>],
    ) -> Result<&'a [&'a Expr<'a>], SqlError> {
        let substituted = self
            .arena
            .alloc_slice_with(values.len(), |index| values[index])
            .map_err(|_| arena_full())?;
        for value in substituted.iter_mut() {
            *value = self.substitute(original, value)?;
        }
        Ok(substituted)
    }

    fn substitute_items(
        &self,
        original: &Original<'a>,
        items: &'a [SelectItem<'a>],
    ) -> Result<&'a [SelectItem<'a>], SqlError> {
        let substituted = self
            .arena
            .alloc_slice_with(items.len(), |index| items[index])
            .map_err(|_| arena_full())?;
        for item in substituted.iter_mut() {
            if let SelectItem::Expr { expression, .. } = item {
                *expression = self.substitute(original, expression)?;
            }
        }
        Ok(substituted)
    }

    /// One rule action with the original's rows joined in, as PostgreSQL's
    /// `rewriteRuleAction` builds it.
    fn action(
        &self,
        original: &Original<'a>,
        action: &'a Stmt<'a>,
        written_condition: Option<&'a Expr<'a>>,
        condition: Option<&'a Expr<'a>>,
    ) -> Result<&'a Stmt<'a>, SqlError> {
        let arena = self.arena;
        // The original's rows: an INSERT's query, or an UPDATE's or DELETE's
        // FROM list with its target where the action reads it.
        let (prefix, suffix, original_condition): (
            Option<FromClause<'a>>,
            Option<FromClause<'a>>,
            Option<&'a Expr<'a>>,
        ) = match original.inserted {
            Some(Inserted::Query {
                from, where_clause, ..
            }) => (from, None, where_clause),
            Some(Inserted::Row(_)) => (None, None, None),
            None => {
                let transition: &[&str] = if original.event == RewriteEvent::Update {
                    &["old", "new"]
                } else {
                    &["old"]
                };
                let mut reads_target = false;
                action_expressions(action, &mut |expression| {
                    reads_target |= mentions(expression, transition, &[]);
                });
                let join = if reads_target {
                    TargetJoin::Last
                } else if written_condition.is_some_and(|condition| {
                    mentions(condition, &["old", "new"], &[])
                }) || original.condition.is_some_and(|condition| {
                    mentions(condition, &[original.exposed], &[])
                }) {
                    TargetJoin::First
                } else {
                    TargetJoin::None
                };
                let target = original.target.map(single_from);
                match join {
                    TargetJoin::Last => (original.extras, target, original.condition),
                    TargetJoin::First => (
                        concat_from(&[target, original.extras], arena)?,
                        None,
                        original.condition,
                    ),
                    TargetJoin::None => (original.extras, None, original.condition),
                }
            }
        };
        let filter = query::and_where(original_condition, condition, arena)?;
        let joined = |own: Option<FromClause<'a>>| concat_from(&[prefix, own, suffix], arena);
        let rewritten = match *action {
            Stmt::Notify { .. } => return Ok(action),
            Stmt::Insert(insert) if insert.select.is_none() => {
                let mut rows = ArenaList::new(arena);
                for row in insert.rows {
                    rows.push(self.substitute_all(original, row)?)
                        .map_err(|_| arena_full())?;
                }
                let rows = rows.as_slice();
                let from = joined(None)?;
                if from.is_none() && filter.is_none() {
                    Stmt::Insert(Insert { rows, ..insert })
                } else {
                    let [row] = rows else {
                        return Err(unsupported(
                            "an action inserting several VALUES rows over the statement's rows",
                        ));
                    };
                    let items = arena
                        .alloc_slice_with(row.len(), |index| SelectItem::Expr {
                            expression: row[index],
                            alias: None,
                        })
                        .map_err(|_| arena_full())?;
                    Stmt::Insert(Insert {
                        rows: &[],
                        select: Some(alloc(arena, query::rule_select(items, from, filter))?),
                        ..insert
                    })
                }
            }
            Stmt::Insert(insert) => {
                let select = insert.select.expect("matched above");
                Stmt::Insert(Insert {
                    select: Some(self.action_select(original, select, &joined, filter)?),
                    ..insert
                })
            }
            Stmt::Select(select) => Stmt::Select(*self.action_select(original, select, &joined, filter)?),
            Stmt::Update(update) => {
                let target = update.alias.unwrap_or(update.table.name);
                let columns = self.relation_columns(update.table)?;
                let assignments = arena
                    .alloc_slice_with(update.assignments.len(), |index| update.assignments[index])
                    .map_err(|_| arena_full())?;
                for (_, value) in assignments.iter_mut() {
                    *value = self.substitute(original, qualify_columns(value, columns, target, arena)?)?;
                }
                let own = update
                    .where_clause
                    .map(|own| self.substitute(original, qualify_columns(own, columns, target, arena)?))
                    .transpose()?;
                let from = joined(update.from.copied())?;
                Stmt::Update(Update {
                    assignments,
                    from: from.map(|from| alloc(arena, from)).transpose()?,
                    where_clause: query::and_where(own, filter, arena)?,
                    ..update
                })
            }
            Stmt::Delete(delete) => {
                let target = delete.alias.unwrap_or(delete.table.name);
                let columns = self.relation_columns(delete.table)?;
                let own = delete
                    .where_clause
                    .map(|own| self.substitute(original, qualify_columns(own, columns, target, arena)?))
                    .transpose()?;
                let using = joined(delete.using.copied())?;
                Stmt::Delete(Delete {
                    using: using.map(|using| alloc(arena, using)).transpose()?,
                    where_clause: query::and_where(own, filter, arena)?,
                    ..delete
                })
            }
            _ => return Err(unsupported("this rule action")),
        };
        alloc(arena, rewritten)
    }

    /// An action's query (a SELECT action, or an INSERT action's source)
    /// over the original's rows.
    fn action_select(
        &self,
        original: &Original<'a>,
        select: &'a Select<'a>,
        joined: &dyn Fn(Option<FromClause<'a>>) -> Result<Option<FromClause<'a>>, SqlError>,
        filter: Option<&'a Expr<'a>>,
    ) -> Result<&'a Select<'a>, SqlError> {
        let from = joined(select.from)?;
        if select.set_body.is_some() {
            if from != select.from || filter.is_some() {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "conditional UNION/INTERSECT/EXCEPT statements are not implemented"
                ));
            }
            return Ok(select);
        }
        let own = select
            .where_clause
            .map(|own| self.substitute(original, own))
            .transpose()?;
        alloc(
            self.arena,
            Select {
                items: self.substitute_items(original, select.items)?,
                from,
                where_clause: query::and_where(own, filter, self.arena)?,
                ..*select
            },
        )
    }

    fn relation_columns(&self, name: QualName<'a>) -> Result<&'a [&'a str], SqlError> {
        let arena = self.arena;
        match self.storage.resolve_relation(name.schema, name.name, self.txid) {
            Some(ResolvedRelation::Table(slot)) => {
                let definition = self.storage.table_def(slot, self.txid);
                let columns = definition.columns();
                let names = arena
                    .alloc_slice_with(columns.len(), |_| "")
                    .map_err(|_| arena_full())?;
                for (name, column) in names.iter_mut().zip(columns) {
                    *name = arena
                        .alloc_str(column.name.as_str())
                        .map_err(|_| arena_full())?;
                }
                Ok(names)
            }
            Some(ResolvedRelation::View(slot)) => {
                let mut descriptions = [crate::sql::types::ColDesc::new("", 0, 0);
                    crate::sql::exec::MAX_PROJ];
                let count = crate::sql::catalog::describe_view(
                    &self.storage.view(slot),
                    self.txid,
                    arena,
                    &mut descriptions,
                )?;
                arena
                    .alloc_slice_with(count, |index| descriptions[index].name)
                    .map(|names| &*names)
                    .map_err(|_| arena_full())
            }
            _ => Ok(&[]),
        }
    }

    /// The original restricted to the rows no qualified INSTEAD rule
    /// takes (PostgreSQL's `qual_product`).
    fn restricted(&self, original: &Original<'a>, exclusions: &'a Expr<'a>) -> Result<&'a Stmt<'a>, SqlError> {
        let arena = self.arena;
        let rewritten = match original.statement {
            Stmt::Update(update) => Stmt::Update(Update {
                where_clause: query::and_where(update.where_clause, Some(exclusions), arena)?,
                ..*update
            }),
            Stmt::Delete(delete) => Stmt::Delete(Delete {
                where_clause: query::and_where(delete.where_clause, Some(exclusions), arena)?,
                ..*delete
            }),
            Stmt::Insert(insert) => {
                let (items, from, where_clause) = match original.inserted {
                    Some(Inserted::Row(row)) => (row, None, None),
                    Some(Inserted::Query {
                        items,
                        from,
                        where_clause,
                    }) => (items, from, where_clause),
                    None => unreachable!("an INSERT reads its rows"),
                };
                let items = arena
                    .alloc_slice_with(items.len(), |index| SelectItem::Expr {
                        expression: items[index],
                        alias: None,
                    })
                    .map_err(|_| arena_full())?;
                let where_clause = query::and_where(where_clause, Some(exclusions), arena)?;
                Stmt::Insert(Insert {
                    rows: &[],
                    select: Some(alloc(arena, query::rule_select(items, from, where_clause))?),
                    ..*insert
                })
            }
            _ => unreachable!("only INSERT, UPDATE and DELETE are rewritten"),
        };
        alloc(arena, rewritten)
    }

    /// An auto-updatable view's statement on its base relation
    /// (PostgreSQL's `rewriteTargetView`).
    fn view_base(&self, statement: &'a Stmt<'a>) -> Result<&'a Stmt<'a>, SqlError> {
        let arena = self.arena;
        let Some((name, _)) = rule_rewrite::statement_target(statement) else {
            unreachable!("a view rewrite reads a data-changing statement");
        };
        let view = query::resolve_view_for_dml(self.storage, name, self.txid, arena)?
            .expect("a view target resolves as a view");
        let rewritten = query::rewrite_view_dml(
            statement,
            name.name,
            view.base.name,
            view.base.schema.expect("view base is qualified"),
            view.columns,
            view.base_columns,
            self.storage,
            self.txid,
            arena,
        )?;
        let base = match (statement, rewritten) {
            (Stmt::Insert(_), Stmt::Insert(rewritten)) => Stmt::Insert(Insert {
                table: view.base,
                columns: if rewritten.columns.is_empty() {
                    view.base_columns
                } else {
                    rewritten.columns
                },
                ..*rewritten
            }),
            (Stmt::Update(update), Stmt::Update(rewritten)) => Stmt::Update(Update {
                table: view.base,
                alias: update.alias,
                where_clause: query::and_where(view.where_clause, rewritten.where_clause, arena)?,
                ..*rewritten
            }),
            (Stmt::Delete(delete), Stmt::Delete(rewritten)) => Stmt::Delete(Delete {
                table: view.base,
                alias: delete.alias,
                where_clause: query::and_where(view.where_clause, rewritten.where_clause, arena)?,
                ..*rewritten
            }),
            _ => unreachable!("a view rewrite keeps its statement kind"),
        };
        alloc(arena, base)
    }
}

/// `expression` with its unqualified `columns` named through `exposed`.
fn qualify_columns<'a>(
    expression: &'a Expr<'a>,
    columns: &[&str],
    exposed: &'a str,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    crate::sql::ast::rewrite_expression(expression, arena, &mut |node: &'a Expr<'a>| -> Result<
        Option<&'a Expr<'a>>,
        RewriteFailure,
    > {
        Ok(match node {
            Expr::Column {
                qualifier: None,
                name,
            } if columns.contains(name) => Some(&*arena.alloc(Expr::Column {
                qualifier: Some(exposed),
                name,
            })?),
            _ => None,
        })
    })
    .map_err(|RewriteFailure(error)| error)
}

/// The plan of an `INSERT ... SELECT`'s source. PostgreSQL reads a query it
/// cannot pull up as the `*SELECT*` subquery, whose `Subquery Scan` it
/// drops when the scan only passes the subquery's columns on unchanged to
/// every target column in order.
pub(super) fn insert_source_plan<'a>(
    storage: &Storage,
    txid: u32,
    insert: &Insert<'_>,
    select: &Select<'_>,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let (plan, wrapper) = if select.set_body.is_some() {
        (plan_derived(storage, txid, select, arena)?, None)
    } else if pulled_up(storage, txid, select, arena)? {
        match lone_derived(select) {
            Some((alias, body)) => {
                let passes = passes_columns(storage, txid, insert, select, Some(body), arena)?;
                (
                    plan_derived(storage, txid, body, arena)?,
                    (!passes).then_some(alias),
                )
            }
            None => (plan_select(storage, txid, select, arena)?, None),
        }
    } else {
        let passes = passes_columns(storage, txid, insert, select, None, arena)?;
        (
            plan_derived(storage, txid, select, arena)?,
            (!passes).then_some("*SELECT*"),
        )
    };
    let Some(alias) = wrapper else {
        return Ok(plan);
    };
    let mut wrapped = Plan::new(arena, plan.count + 1)?;
    wrapped.methods = plan.methods;
    let root = plan.nodes[0];
    wrapped.push(PlanNode {
        name: StackStr::from_str("Subquery Scan"),
        alias: StackStr::from_str(alias),
        rows: root.rows,
        width: root.width,
        total_cost: root.total_cost,
        object_requests: root.object_requests,
        cache_blocks: root.cache_blocks,
        ..PlanNode::EMPTY
    })?;
    for node in &plan.nodes[..plan.count] {
        wrapped.push(PlanNode {
            depth: node.depth.saturating_add(1),
            ..*node
        })?;
    }
    Ok(wrapped)
}

/// A pulled-up query that only reads one derived table's columns: its
/// alias and query.
fn lone_derived<'s>(select: &'s Select<'s>) -> Option<(&'s str, &'s Select<'s>)> {
    let from = select.from.as_ref()?;
    let body = from.base.subquery?;
    (from.joins.is_empty()
        && select.where_clause.is_none()
        && super::values_list(body).is_none()
        && select
            .items
            .iter()
            .all(|item| matches!(item, SelectItem::Expr { expression: Expr::Column { .. }, .. })))
    .then(|| (from.base.alias.unwrap_or("unnamed_subquery"), body))
}

/// Whether the INSERT's target columns are exactly `source`'s output (or,
/// for `derived`, the derived table's output the query names in order),
/// each of the target column's own type: PostgreSQL's
/// `trivial_subqueryscan`.
fn passes_columns(
    storage: &Storage,
    txid: u32,
    insert: &Insert<'_>,
    select: &Select<'_>,
    derived: Option<&Select<'_>>,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let Some(ResolvedRelation::Table(slot)) =
        storage.resolve_relation(insert.table.schema, insert.table.name, txid)
    else {
        return Ok(false);
    };
    let definition = storage.table_def(slot, txid);
    let columns = definition.columns();
    if !insert.columns.is_empty()
        && (insert.columns.len() != columns.len()
            || insert
                .columns
                .iter()
                .zip(columns)
                .any(|(written, column)| *written != column.name.as_str()))
    {
        return Ok(false);
    }
    let source = derived.unwrap_or(select);
    if source.set_body.is_some() {
        return Ok(false);
    }
    if let Some(derived) = derived {
        // The query names the derived table's columns in order.
        if select.items.len() != derived.items.len() {
            return Ok(false);
        }
        for (item, inner) in select.items.iter().zip(derived.items) {
            let (SelectItem::Expr { expression: Expr::Column { name, .. }, .. }, SelectItem::Expr { .. }) =
                (item, inner)
            else {
                return Ok(false);
            };
            if *name != output_name(inner) {
                return Ok(false);
            }
        }
    }
    if source.items.len() != columns.len() {
        return Ok(false);
    }
    let scope = match &source.from {
        Some(from) => Some(query::QueryScope::resolve_schema(storage, from, txid, arena, None)?),
        None => None,
    };
    for (item, column) in source.items.iter().zip(columns) {
        let SelectItem::Expr { expression, .. } = item else {
            return Ok(false);
        };
        // An untyped literal is converted on the way into the target.
        if matches!(expression, Expr::Str(_) | Expr::Null) || column.type_mod != -1 {
            return Ok(false);
        }
        let inferred = match &scope {
            Some(scope) => crate::sql::exec::infer_type_res(
                expression,
                &query::CatalogScopeCols {
                    scope,
                    outer: None,
                    storage,
                    txid,
                    arena,
                },
            ),
            None => crate::sql::exec::infer_type_res(
                expression,
                &crate::sql::exec::CatalogCols {
                    definition: None,
                    alias: None,
                    output_aliases: &[],
                    storage,
                    txid,
                    arena,
                },
            ),
        };
        let Ok((type_oid, _)) = inferred else {
            return Ok(false);
        };
        if Some(type_oid) != storage.routine_type_oid(column.ctype, column.user_type, txid) {
            return Ok(false);
        }
    }
    Ok(true)
}
