//! Aggregate levels: which query computes each aggregate call.
//!
//! PostgreSQL gives an aggregate to the innermost query whose columns its
//! arguments read (`agglevelsup`). In `SELECT (SELECT max(t.id) FROM u) FROM t`
//! the call `max(t.id)` aggregates `t`: the outer query returns one row, and
//! the subquery returns that maximum once for each row of `u`. An aggregate
//! that reads no column belongs to the query it is written in.
//!
//! Before a query runs, [`assign_aggregate_levels`] rewrites each call that
//! belongs to an enclosing query into `Expr::OuterAggregate`, naming how many
//! levels out it belongs. The owning query folds the call as one of its own
//! aggregates ([`visit_owned_outer_aggregates`] finds them), and the nested
//! query reads the owner's value for the current group through its outer row.
//! The same pass raises PostgreSQL's placement errors, such as `aggregate
//! functions are not allowed in WHERE` or nested aggregate calls, before any
//! row is read.

use crate::mem::arena::{Arena, ArenaFull};
use crate::sql::ast::{
    Expr, FromClause, Join, OrderBy, Select, SelectItem, SetTree, TableRef, map_expression,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::storage::Storage;

use super::scope::QueryScope;
use super::subquery::walk_children;
use super::{CatalogScopeCols, arena_full, call_may_be_aggregate, selected_aggregate_call};

/// Query levels are tracked as bits of a word; deeper nesting is left as
/// written (the executor rejects it as too deeply nested first).
const MAX_DEPTH: u32 = u64::BITS - 1;

/// The clause of a query level that encloses the expression being analysed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    Target,
    Where,
    JoinOn,
    GroupBy,
    Having,
    OrderBy,
    DistinctOn,
    Limit,
    Offset,
    FunctionInFrom,
    FromSubquery,
    Filter,
}

impl Clause {
    /// PostgreSQL's name for a clause that cannot hold its query's aggregates.
    const fn forbidden(self) -> Option<&'static str> {
        match self {
            Self::Where => Some("WHERE"),
            Self::JoinOn => Some("JOIN conditions"),
            Self::GroupBy => Some("GROUP BY"),
            Self::Limit => Some("LIMIT"),
            Self::Offset => Some("OFFSET"),
            Self::FunctionInFrom => Some("functions in FROM"),
            Self::FromSubquery => Some("FROM clause of their own query level"),
            Self::Filter => Some("FILTER"),
            Self::Target | Self::Having | Self::OrderBy | Self::DistinctOn => None,
        }
    }
}

/// The columns a query level provides to the expressions inside it.
#[derive(Clone, Copy)]
enum Columns<'l, 'a> {
    /// A FROM-less query, or a query whose columns a non-`LATERAL` derived
    /// table cannot see.
    Hidden,
    Rows(&'l QueryScope<'a>),
    /// Sources this pass cannot resolve on its own (a `WITH` name, a function
    /// reading an enclosing row): every name reaching this level is taken as
    /// its own, so aggregates beneath it stay where they are written.
    Unresolved,
}

struct Level<'l, 'a> {
    columns: Columns<'l, 'a>,
    clause: Clause,
    depth: u32,
    parent: Option<&'l Level<'l, 'a>>,
}

impl<'l, 'a> Level<'l, 'a> {
    const fn in_clause(&'l self, clause: Clause) -> Self {
        Level {
            columns: self.columns,
            clause,
            depth: self.depth,
            parent: self.parent,
        }
    }

    fn at(&self, depth: u32) -> &Self {
        let mut level = self;
        while level.depth > depth {
            level = level.parent.expect("enclosing query level");
        }
        level
    }

    /// The depth of the innermost level that binds a column reference.
    fn column(&self, qualifier: Option<&str>, name: &str) -> Option<u32> {
        let mut current = Some(self);
        while let Some(level) = current {
            match level.columns {
                Columns::Hidden => {}
                Columns::Unresolved => return Some(level.depth),
                Columns::Rows(scope) => match scope.find_column(qualifier, name) {
                    Ok(_) => return Some(level.depth),
                    Err(error)
                        if !matches!(
                            error.sqlstate.as_str(),
                            sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                        ) =>
                    {
                        return Some(level.depth);
                    }
                    Err(_) => {}
                },
            }
            current = level.parent;
        }
        None
    }

    /// The depth of the innermost level that binds a whole-row reference.
    fn relation(&self, name: &str) -> Option<u32> {
        let mut current = Some(self);
        while let Some(level) = current {
            match level.columns {
                Columns::Hidden => {}
                Columns::Unresolved => return Some(level.depth),
                Columns::Rows(scope) => {
                    if scope.table_index(name).is_ok() {
                        return Some(level.depth);
                    }
                }
            }
            current = level.parent;
        }
        None
    }
}

/// Declared column types of a level and the levels enclosing it, so a
/// nested query's function arguments resolve against them. It holds no row.
struct LevelColumns<'l, 'a>(Option<&'l Level<'l, 'a>>);

impl<'l, 'a> LevelColumns<'l, 'a> {
    fn answering(&self, qualifier: Option<&str>, name: &str) -> Option<&'l QueryScope<'a>> {
        let mut current = self.0;
        while let Some(level) = current {
            if let Columns::Rows(scope) = level.columns
                && scope.find_column(qualifier, name).is_ok()
            {
                return Some(scope);
            }
            current = level.parent;
        }
        None
    }
}

impl<'v> crate::sql::eval::ColumnLookup<'v> for LevelColumns<'_, '_> {
    fn lookup(
        &self,
        _qualifier: Option<&str>,
        name: &str,
    ) -> Result<crate::sql::types::Datum<'v>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<crate::sql::types::ColType> {
        let scope = self.answering(qualifier, name)?;
        crate::sql::eval::ColumnLookup::<'v>::col_type(&super::ScopeSchema(scope), qualifier, name)
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        self.answering(qualifier, name).map_or(-1, |scope| {
            crate::sql::eval::ColumnLookup::<'v>::column_type_mod(
                &super::ScopeSchema(scope),
                qualifier,
                name,
            )
        })
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.answering(qualifier, name)
            .map_or(crate::sql::ast::Collation::None, |scope| {
                crate::sql::eval::ColumnLookup::<'v>::collation(
                    &super::ScopeSchema(scope),
                    qualifier,
                    name,
                )
            })
    }
}

const fn bit(depth: u32) -> u64 {
    1 << depth
}

/// Bits for the levels at or outside `depth`.
const fn through(depth: u32) -> u64 {
    if depth >= MAX_DEPTH {
        u64::MAX
    } else {
        (1 << (depth + 1)) - 1
    }
}

/// Levels whose first aggregate is remembered, to report a nested call where
/// PostgreSQL does.
const SITES: usize = 8;

/// The levels an expression reads columns of, and the levels owning the
/// aggregates inside it (each aggregate hides its own arguments).
#[derive(Clone, Copy, Default)]
struct Reach<'a> {
    columns: u64,
    aggregates: u64,
    /// The first aggregate each of the outermost levels owns.
    sites: [Option<&'a Expr<'a>>; SITES],
}

impl<'a> Reach<'a> {
    fn aggregate(&mut self, owner: u32, call: &'a Expr<'a>) {
        self.aggregates |= bit(owner);
        if let Some(site) = self.sites.get_mut(owner as usize)
            && site.is_none()
        {
            *site = Some(call);
        }
    }

    fn absorb(&mut self, inner: &Self) {
        self.columns |= inner.columns;
        self.aggregates |= inner.aggregates;
        for (site, inner) in self.sites.iter_mut().zip(inner.sites) {
            if site.is_none() {
                *site = inner;
            }
        }
    }
}

struct Failure(SqlError);

impl From<ArenaFull> for Failure {
    fn from(_: ArenaFull) -> Self {
        Self(arena_full())
    }
}

impl From<SqlError> for Failure {
    fn from(error: SqlError) -> Self {
        Self(error)
    }
}

type Step<T> = Result<T, Failure>;

struct Analysis<'a> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
}

/// Rewrites every aggregate call in `select`'s nested queries that belongs
/// to an enclosing level into `Expr::OuterAggregate`, and rejects aggregates
/// placed where PostgreSQL forbids them. Returns `select` itself when nothing
/// changes, without keeping any analysis allocation.
pub(super) fn assign_aggregate_levels<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let analysis = Analysis {
        storage,
        txid,
        arena,
    };
    if !analysis.select_needs_analysis(select, false) {
        return Ok(select);
    }
    let mark = arena.mark();
    let mut reach = Reach::default();
    match analysis.select(select, None, 0, &mut reach) {
        Ok(rewritten) if core::ptr::eq(rewritten, select) => {
            // SAFETY: an unchanged query references nothing allocated since
            // `mark`: the analysis scopes are discarded.
            unsafe { arena.rewind_to(mark) };
            Ok(select)
        }
        Ok(rewritten) => Ok(rewritten),
        Err(Failure(error)) => Err(error),
    }
}

/// Visits each aggregate call that a nested query of `expression` reads from
/// the query evaluating `expression` (an `Expr::OuterAggregate` naming it).
pub(super) fn visit_owned_outer_aggregates<'a>(
    expression: &'a Expr<'a>,
    visit: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    owned_in_expression(expression, 0, visit)
}

/// Whether a nested query of `select` reads an aggregate `select` owns, so
/// `select` must evaluate that nested query once per group.
pub(super) fn select_owns_nested_aggregates<'a>(select: &'a Select<'a>) -> bool {
    let mut found = false;
    let walked = owned_in_select(select, 0, &mut |_| {
        found = true;
        Ok(())
    });
    walked.is_ok() && found
}

fn owned_in_expression<'a>(
    expression: &'a Expr<'a>,
    depth: u32,
    visit: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match expression {
        Expr::OuterAggregate { call, levels_up } => {
            if *levels_up == depth {
                visit(call)?;
            }
            Ok(())
        }
        Expr::Subquery(select)
        | Expr::Exists(select)
        | Expr::ArraySubquery(select)
        | Expr::RowSubquery { select, .. } => owned_in_select(select, depth + 1, visit),
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => {
            owned_in_expression(operand, depth, visit)?;
            owned_in_select(select, depth + 1, visit)
        }
        _ => walk_children(expression, &mut |child| {
            owned_in_expression(child, depth, visit)
        }),
    }
}

fn owned_in_select<'a>(
    select: &'a Select<'a>,
    depth: u32,
    visit: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    fn owned_in_tree<'a>(
        tree: &'a SetTree<'a>,
        depth: u32,
        visit: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        match tree {
            SetTree::Select(select) => owned_in_select(select, depth, visit),
            SetTree::Op { left, right, .. } => {
                owned_in_tree(left, depth, visit)?;
                owned_in_tree(right, depth, visit)
            }
        }
    }
    fn owned_in_table<'a>(
        table: &'a TableRef<'a>,
        depth: u32,
        visit: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
    ) -> Result<(), SqlError> {
        if let Some(select) = table.subquery {
            owned_in_select(select, depth + 1, visit)?;
        }
        for argument in table.func_args.unwrap_or(&[]) {
            owned_in_expression(argument, depth, visit)?;
        }
        for function in table.rows_from.unwrap_or(&[]) {
            owned_in_table(function, depth, visit)?;
        }
        Ok(())
    }
    if let Some(body) = select.set_body {
        owned_in_tree(body, depth, visit)?;
    }
    if let Some(from) = &select.from {
        owned_in_table(&from.base, depth, visit)?;
        for join in from.joins {
            owned_in_table(&join.table, depth, visit)?;
            if let Some(on) = join.on {
                owned_in_expression(on, depth, visit)?;
            }
        }
    }
    for item in select.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            owned_in_expression(expression, depth, visit)?;
        }
    }
    let clauses = select
        .where_clause
        .into_iter()
        .chain(select.having)
        .chain(select.group_by.iter().copied())
        .chain(select.order_by.iter().map(|order| order.expression))
        .chain(select.distinct_on.iter().copied())
        .chain(select.limit)
        .chain(select.offset);
    for expression in clauses {
        owned_in_expression(expression, depth, visit)?;
    }
    Ok(())
}

impl<'a> Analysis<'a> {
    fn candidate(&self, expression: &Expr<'_>) -> bool {
        matches!(expression, Expr::Call { over: None, .. })
            && call_may_be_aggregate(expression, self.storage, self.txid)
    }

    fn contains_candidate(&self, expression: &'a Expr<'a>) -> bool {
        self.expression_needs_analysis(expression, true)
    }

    /// Whether an expression holds an aggregate this pass must place: any
    /// inside a nested query or another aggregate, or, with `restricted`,
    /// any at all.
    fn expression_needs_analysis(&self, expression: &'a Expr<'a>, restricted: bool) -> bool {
        match expression {
            Expr::Subquery(select)
            | Expr::Exists(select)
            | Expr::ArraySubquery(select)
            | Expr::RowSubquery { select, .. } => self.select_needs_analysis(select, true),
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                self.expression_needs_analysis(operand, restricted)
                    || self.select_needs_analysis(select, true)
            }
            _ => {
                if self.candidate(expression) {
                    if restricted {
                        return true;
                    }
                    let mut found = false;
                    let _ = walk_children(expression, &mut |child| {
                        found |= self.contains_candidate(child);
                        Ok(())
                    });
                    return found;
                }
                let mut found = false;
                let _ = walk_children(expression, &mut |child| {
                    found |= self.expression_needs_analysis(child, restricted);
                    Ok(())
                });
                found
            }
        }
    }

    /// Whether a query holds an aggregate this pass must place; `nested`
    /// counts every aggregate of a query inside another one.
    fn select_needs_analysis(&self, select: &'a Select<'a>, nested: bool) -> bool {
        fn tree_needs<'a>(analysis: &Analysis<'a>, tree: &'a SetTree<'a>, nested: bool) -> bool {
            match tree {
                SetTree::Select(select) => analysis.select_needs_analysis(select, nested),
                SetTree::Op { left, right, .. } => {
                    tree_needs(analysis, left, nested) || tree_needs(analysis, right, nested)
                }
            }
        }
        let table = |table: &'a TableRef<'a>| {
            table
                .subquery
                .is_some_and(|select| self.select_needs_analysis(select, true))
                || table
                    .func_args
                    .unwrap_or(&[])
                    .iter()
                    .chain(
                        table
                            .rows_from
                            .unwrap_or(&[])
                            .iter()
                            .flat_map(|function| function.func_args.unwrap_or(&[]).iter()),
                    )
                    .any(|argument| self.contains_candidate(argument))
        };
        if select
            .set_body
            .is_some_and(|body| tree_needs(self, body, nested))
        {
            return true;
        }
        if let Some(from) = &select.from
            && (table(&from.base)
                || from.joins.iter().any(|join| {
                    table(&join.table) || join.on.is_some_and(|on| self.contains_candidate(on))
                }))
        {
            return true;
        }
        let open = select
            .items
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                    Some(*expression)
                }
                _ => None,
            })
            .chain(select.having)
            .chain(select.order_by.iter().map(|order| order.expression))
            .chain(select.distinct_on.iter().copied());
        let restricted = select
            .where_clause
            .into_iter()
            .chain(select.group_by.iter().copied())
            .chain(select.limit)
            .chain(select.offset);
        open.into_iter()
            .any(|expression| self.expression_needs_analysis(expression, nested))
            || restricted
                .into_iter()
                .any(|expression| self.contains_candidate(expression))
    }

    fn select(
        &self,
        select: &'a Select<'a>,
        parent: Option<&Level<'_, 'a>>,
        depth: u32,
        reach: &mut Reach<'a>,
    ) -> Step<&'a Select<'a>> {
        if depth > MAX_DEPTH {
            return Ok(select);
        }
        if let Some(body) = select.set_body {
            let mapped = self.set_tree(body, parent, depth, reach)?;
            let level = Level {
                columns: Columns::Hidden,
                clause: Clause::Offset,
                depth,
                parent,
            };
            let offset = self.optional(select.offset, &level, reach)?;
            let limit = self.optional(select.limit, &level.in_clause(Clause::Limit), reach)?;
            if core::ptr::eq(mapped, body)
                && same(offset, select.offset)
                && same(limit, select.limit)
            {
                return Ok(select);
            }
            return Ok(&*self.arena.alloc(Select {
                set_body: Some(mapped),
                offset,
                limit,
                ..*select
            })?);
        }
        let scope;
        let columns = match &select.from {
            None => Columns::Hidden,
            Some(_) if !select.with.is_empty() => Columns::Unresolved,
            Some(from) => {
                let enclosing = LevelColumns(parent);
                match QueryScope::resolve_schema(
                    self.storage,
                    from,
                    self.txid,
                    self.arena,
                    parent.map(|_| &enclosing as &dyn crate::sql::eval::ColumnLookup<'_>),
                ) {
                    Ok(resolved) => {
                        scope = resolved;
                        Columns::Rows(&scope)
                    }
                    Err(_) => Columns::Unresolved,
                }
            }
        };
        let level = Level {
            columns,
            clause: Clause::Target,
            depth,
            parent,
        };
        let from = match &select.from {
            Some(from) => Some(self.from(from, &level, reach)?),
            None => None,
        };
        let items = self.items(select.items, &level, reach)?;
        let where_clause =
            self.optional(select.where_clause, &level.in_clause(Clause::Where), reach)?;
        let having = self.optional(select.having, &level.in_clause(Clause::Having), reach)?;
        let order_by = self.ordering(select.order_by, &level.in_clause(Clause::OrderBy), reach)?;
        let group_by =
            self.expressions(select.group_by, &level.in_clause(Clause::GroupBy), reach)?;
        let distinct_on = self.expressions(
            select.distinct_on,
            &level.in_clause(Clause::DistinctOn),
            reach,
        )?;
        let offset = self.optional(select.offset, &level.in_clause(Clause::Offset), reach)?;
        let limit = self.optional(select.limit, &level.in_clause(Clause::Limit), reach)?;
        let from_same = match (&from, &select.from) {
            (Some(mapped), Some(original)) => {
                core::ptr::eq(mapped.joins, original.joins) && mapped.base == original.base
            }
            _ => true,
        };
        if from_same
            && core::ptr::eq(items, select.items)
            && same(where_clause, select.where_clause)
            && same(having, select.having)
            && core::ptr::eq(order_by, select.order_by)
            && core::ptr::eq(group_by, select.group_by)
            && core::ptr::eq(distinct_on, select.distinct_on)
            && same(offset, select.offset)
            && same(limit, select.limit)
        {
            return Ok(select);
        }
        Ok(&*self.arena.alloc(Select {
            items,
            from,
            where_clause,
            having,
            order_by,
            group_by,
            distinct_on,
            offset,
            limit,
            ..*select
        })?)
    }

    fn set_tree(
        &self,
        tree: &'a SetTree<'a>,
        parent: Option<&Level<'_, 'a>>,
        depth: u32,
        reach: &mut Reach<'a>,
    ) -> Step<&'a SetTree<'a>> {
        match tree {
            SetTree::Select(select) => {
                let mapped = self.select(select, parent, depth, reach)?;
                if core::ptr::eq(mapped, *select) {
                    Ok(tree)
                } else {
                    Ok(&*self.arena.alloc(SetTree::Select(mapped))?)
                }
            }
            SetTree::Op {
                syntax,
                operator,
                all,
                left,
                right,
            } => {
                let mapped_left = self.set_tree(left, parent, depth, reach)?;
                let mapped_right = self.set_tree(right, parent, depth, reach)?;
                if core::ptr::eq(mapped_left, *left) && core::ptr::eq(mapped_right, *right) {
                    Ok(tree)
                } else {
                    Ok(&*self.arena.alloc(SetTree::Op {
                        syntax: *syntax,
                        operator: *operator,
                        all: *all,
                        left: mapped_left,
                        right: mapped_right,
                    })?)
                }
            }
        }
    }

    fn from(
        &self,
        from: &FromClause<'a>,
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<FromClause<'a>> {
        let base = self.table(&from.base, level, reach)?;
        let mut joins: Option<&'a mut [Join<'a>]> = None;
        for (index, join) in from.joins.iter().enumerate() {
            let table = self.table(&join.table, level, reach)?;
            let on = self.optional(join.on, &level.in_clause(Clause::JoinOn), reach)?;
            if table != join.table || !same(on, join.on) {
                let slice = match joins.take() {
                    Some(slice) => slice,
                    None => self.arena.alloc_slice_copy(from.joins)?,
                };
                slice[index].table = table;
                slice[index].on = on;
                joins = Some(slice);
            }
        }
        Ok(FromClause {
            base,
            joins: joins.map_or(from.joins, |slice| &*slice),
        })
    }

    fn table(
        &self,
        table: &TableRef<'a>,
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<TableRef<'a>> {
        let mut mapped = *table;
        if let Some(select) = table.subquery {
            // A derived table sees its query's columns only when LATERAL.
            let own = if table.lateral {
                level.in_clause(Clause::FromSubquery)
            } else {
                Level {
                    columns: Columns::Hidden,
                    clause: Clause::FromSubquery,
                    depth: level.depth,
                    parent: level.parent,
                }
            };
            mapped.subquery = Some(self.select(select, Some(&own), level.depth + 1, reach)?);
        }
        let functions = level.in_clause(Clause::FunctionInFrom);
        if let Some(arguments) = table.func_args {
            mapped.func_args = Some(self.expressions(arguments, &functions, reach)?);
        }
        if let Some(entries) = table.rows_from {
            let mut copied: Option<&'a mut [TableRef<'a>]> = None;
            for (index, entry) in entries.iter().enumerate() {
                let function = self.table(entry, level, reach)?;
                if function != *entry {
                    let slice = match copied.take() {
                        Some(slice) => slice,
                        None => self.arena.alloc_slice_copy(entries)?,
                    };
                    slice[index] = function;
                    copied = Some(slice);
                }
            }
            if let Some(slice) = copied {
                mapped.rows_from = Some(slice);
            }
        }
        Ok(mapped)
    }

    fn items(
        &self,
        items: &'a [SelectItem<'a>],
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<&'a [SelectItem<'a>]> {
        let mut copied: Option<&'a mut [SelectItem<'a>]> = None;
        for (index, item) in items.iter().enumerate() {
            let mapped = match item {
                SelectItem::Expr { expression, alias } => SelectItem::Expr {
                    expression: self.expression(expression, level, reach)?,
                    alias: *alias,
                },
                SelectItem::RecordStar(expression) => {
                    SelectItem::RecordStar(self.expression(expression, level, reach)?)
                }
                other => *other,
            };
            if mapped != *item {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self.arena.alloc_slice_copy(items)?,
                };
                slice[index] = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(items, |slice| &*slice))
    }

    fn optional(
        &self,
        expression: Option<&'a Expr<'a>>,
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<Option<&'a Expr<'a>>> {
        expression
            .map(|expression| self.expression(expression, level, reach))
            .transpose()
    }

    fn expressions(
        &self,
        expressions: &'a [&'a Expr<'a>],
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<&'a [&'a Expr<'a>]> {
        let mut copied: Option<&'a mut [&'a Expr<'a>]> = None;
        for (index, expression) in expressions.iter().enumerate() {
            let mapped = self.expression(expression, level, reach)?;
            if !core::ptr::eq(mapped, *expression) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self.arena.alloc_slice_copy(expressions)?,
                };
                slice[index] = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(expressions, |slice| &*slice))
    }

    fn ordering(
        &self,
        keys: &'a [OrderBy<'a>],
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<&'a [OrderBy<'a>]> {
        let mut copied: Option<&'a mut [OrderBy<'a>]> = None;
        for (index, key) in keys.iter().enumerate() {
            let mapped = self.expression(key.expression, level, reach)?;
            if !core::ptr::eq(mapped, key.expression) {
                let slice = match copied.take() {
                    Some(slice) => slice,
                    None => self.arena.alloc_slice_copy(keys)?,
                };
                slice[index].expression = mapped;
                copied = Some(slice);
            }
        }
        Ok(copied.map_or(keys, |slice| &*slice))
    }

    fn sublink(
        &self,
        select: &'a Select<'a>,
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<Option<&'a Select<'a>>> {
        let mapped = self.select(select, Some(level), level.depth + 1, reach)?;
        Ok((!core::ptr::eq(mapped, select)).then_some(mapped))
    }

    fn expression(
        &self,
        expression: &'a Expr<'a>,
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<&'a Expr<'a>> {
        map_expression(expression, self.arena, &mut |node| {
            let rebuilt = match *node {
                Expr::Column { qualifier, name } => {
                    if let Some(depth) = level.column(qualifier, name) {
                        reach.columns |= bit(depth);
                    }
                    return Ok(None);
                }
                Expr::WholeRow(name) => {
                    if let Some(depth) = level.relation(name) {
                        reach.columns |= bit(depth);
                    }
                    return Ok(None);
                }
                Expr::SchemaColumn {
                    schema,
                    table,
                    name,
                } => {
                    let qualifier = crate::stack_format!(130, "{}.{}", schema, table);
                    if let Some(depth) = level.column(Some(qualifier.as_str()), name) {
                        reach.columns |= bit(depth);
                    }
                    return Ok(None);
                }
                Expr::OuterAggregate { call, levels_up } => {
                    if let Some(owner) = level.depth.checked_sub(levels_up) {
                        reach.aggregate(owner, call);
                    }
                    return Ok(None);
                }
                Expr::Subquery(select) => self.sublink(select, level, reach)?.map(Expr::Subquery),
                Expr::Exists(select) => self.sublink(select, level, reach)?.map(Expr::Exists),
                Expr::ArraySubquery(select) => {
                    self.sublink(select, level, reach)?.map(Expr::ArraySubquery)
                }
                Expr::RowSubquery { select, arity } => self
                    .sublink(select, level, reach)?
                    .map(|select| Expr::RowSubquery { select, arity }),
                Expr::InSubquery {
                    operand,
                    select,
                    negated,
                } => {
                    let mapped_operand = self.expression(operand, level, reach)?;
                    let mapped = self.sublink(select, level, reach)?;
                    (mapped.is_some() || !core::ptr::eq(mapped_operand, operand)).then(|| {
                        Expr::InSubquery {
                            operand: mapped_operand,
                            select: mapped.unwrap_or(select),
                            negated,
                        }
                    })
                }
                Expr::QuantifiedSubquery {
                    operand,
                    operator,
                    select,
                    all,
                } => {
                    let mapped_operand = self.expression(operand, level, reach)?;
                    let mapped = self.sublink(select, level, reach)?;
                    (mapped.is_some() || !core::ptr::eq(mapped_operand, operand)).then(|| {
                        Expr::QuantifiedSubquery {
                            operand: mapped_operand,
                            operator,
                            select: mapped.unwrap_or(select),
                            all,
                        }
                    })
                }
                _ if self.candidate(node) => return Ok(Some(self.aggregate(node, level, reach)?)),
                _ => return Ok(None),
            };
            Ok(Some(match rebuilt {
                Some(rebuilt) => &*self.arena.alloc(rebuilt)?,
                None => node,
            }))
        })
    }

    /// Places one aggregate candidate: its level is the innermost level its
    /// arguments, `FILTER` and ordering read, through columns or nested
    /// aggregates; one that reads neither belongs to its own query.
    fn aggregate(
        &self,
        call: &'a Expr<'a>,
        level: &Level<'_, 'a>,
        reach: &mut Reach<'a>,
    ) -> Step<&'a Expr<'a>> {
        let Expr::Call {
            args,
            order_by,
            filter,
            ..
        } = *call
        else {
            unreachable!("aggregate candidates are calls");
        };
        let mut inner = Reach::default();
        let mapped_args = self.expressions(args, level, &mut inner)?;
        let mapped_order = self.ordering(order_by, level, &mut inner)?;
        let mapped_filter = self.optional(filter, &level.in_clause(Clause::Filter), &mut inner)?;
        let mapped = if core::ptr::eq(mapped_args, args)
            && core::ptr::eq(mapped_order, order_by)
            && same(mapped_filter, filter)
        {
            call
        } else {
            let mut rebuilt = *call;
            if let Expr::Call {
                args,
                order_by,
                filter,
                ..
            } = &mut rebuilt
            {
                *args = mapped_args;
                *order_by = mapped_order;
                *filter = mapped_filter;
            }
            &*self.arena.alloc(rebuilt)?
        };
        let depth = level.depth;
        let visible = (inner.columns | inner.aggregates) & through(depth);
        let owner = if visible == 0 {
            depth
        } else {
            MAX_DEPTH - visible.leading_zeros()
        };
        let nested = inner.aggregates & through(depth) & bit(owner) != 0;
        let owner_level = level.at(owner);
        let forbidden = owner_level.clause.forbidden();
        if owner == depth && !nested && forbidden.is_none() {
            reach.aggregate(owner, call);
            return Ok(mapped);
        }
        if !self.confirmed(mapped, owner_level)? {
            reach.absorb(&inner);
            return Ok(mapped);
        }
        if nested {
            // PostgreSQL points at the nested call of the same level.
            let site = inner
                .sites
                .get(owner as usize)
                .copied()
                .flatten()
                .unwrap_or(call);
            return Err(Failure(crate::sql::error_position::expression_error(
                sql_err!(
                    sqlstate::GROUPING_ERROR,
                    "aggregate function calls cannot be nested"
                ),
                site,
            )));
        }
        if let Some(clause) = forbidden {
            return Err(Failure(crate::sql::error_position::expression_error(
                sql_err!(
                    sqlstate::GROUPING_ERROR,
                    "aggregate functions are not allowed in {}",
                    clause
                ),
                call,
            )));
        }
        reach.aggregate(owner, call);
        if owner == depth {
            return Ok(mapped);
        }
        Ok(&*self.arena.alloc(Expr::OuterAggregate {
            call: mapped,
            levels_up: depth - owner,
        })?)
    }

    /// Whether a candidate selects an aggregate, resolved against the level
    /// that would own it. A level this pass cannot resolve owns nothing here:
    /// its own query places its aggregates when it runs.
    fn confirmed(&self, call: &'a Expr<'a>, owner: &Level<'_, 'a>) -> Step<bool> {
        let scoped;
        let fromless;
        let columns: &dyn crate::sql::exec::ColTypeResolver = match owner.columns {
            Columns::Rows(scope) => {
                scoped = CatalogScopeCols {
                    scope,
                    outer_scope: None,
                    storage: self.storage,
                    txid: self.txid,
                    arena: self.arena,
                };
                &scoped
            }
            Columns::Hidden => {
                fromless = crate::sql::exec::CatalogCols {
                    definition: None,
                    alias: None,
                    output_aliases: &[],
                    storage: self.storage,
                    txid: self.txid,
                    arena: self.arena,
                };
                &fromless
            }
            Columns::Unresolved => return Ok(false),
        };
        // A candidate that does not resolve here is left to the executor,
        // which reports the lookup failure where PostgreSQL does.
        Ok(selected_aggregate_call(call, self.storage, self.txid, columns).unwrap_or(false))
    }
}

fn same(left: Option<&Expr<'_>>, right: Option<&Expr<'_>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => core::ptr::eq(left, right),
        (None, None) => true,
        _ => false,
    }
}
