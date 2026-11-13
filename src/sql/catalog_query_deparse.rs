//! PostgreSQL's `get_query_def`: a stored query printed in the indented
//! layout `pg_get_viewdef` and `pg_get_ruledef` use.
//!
//! The query's FROM items become a scope of [`Joined`] items, one query level
//! after another, from which column references are qualified and typed as
//! PostgreSQL's range table and namespaces qualify them. Each level's items
//! get unique printed names (`t1_1` for a second `t1` under a query that
//! already names `t1`), as `set_rtable_names` assigns them. Shapes outside
//! this coverage mark the output unsupported.
//! Source: <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/utils/adt/ruleutils.c>
//! (`get_query_def`, `get_select_query_def`, `get_basic_select_query`,
//! `get_setop_query`, `get_with_clause`, `get_target_list`,
//! `get_from_clause_item`, `get_rule_groupingset`, `get_rule_windowspec`).

use super::{Column, Columns, Context, Joined, Output, arena_full, write_identifier};
use crate::mem::arena::{ArenaList, ArenaString};
use crate::sql::ast::{
    Expr, FrameBound, FrameExclusion, FrameUnits, FromClause, GroupingElement, JoinKind,
    LockStrength, LockWait, OrderBy, RelationInheritance, Select, SelectItem, SetOp, SetTree,
    TableRef, TableSampleMethod, WindowSpec,
};
use crate::sql::types::oid;
use crate::sql::{Arena, SqlError, Storage};

const STD: i32 = 8;
const JOIN: i32 = 4;
const VAR: i32 = 4;
const LIMIT: i32 = 40;
/// The most items one query level's merged `USING` columns can hide.
const MAX_LEVEL_ITEMS: usize = 64;

/// What a query is printed against.
#[derive(Clone, Copy)]
pub(super) struct Writer<'r> {
    pub storage: &'r Storage,
    pub txid: u32,
    pub pretty: bool,
    /// The relation identities a stored view captured at CREATE.
    pub dependencies: Option<crate::storage::StoredQueryDependencyView<'r>>,
    /// A rule's query, whose range table also holds `OLD` and `NEW`.
    pub qualify: bool,
}

/// How a query's output columns are named.
#[derive(Clone, Copy)]
pub(super) struct Naming<'n> {
    /// The names its consumer gives the columns (a view's columns).
    pub names: Option<&'n [&'n str]>,
    /// `colNamesVisible`: whether a column's own name needs no `AS`.
    pub visible: bool,
    /// An `INSERT ... SELECT` source, whose untyped literals stay unknown
    /// until the target columns coerce them.
    pub unknown: bool,
}

impl Output<'_> {
    /// `appendContextKeyword`: a new line indented to the current level.
    pub(super) fn keyword(
        &mut self,
        text: &str,
        before: i32,
        after: i32,
        plus: i32,
    ) -> Result<(), SqlError> {
        self.indent += before;
        let kept = self.text.as_str().trim_end_matches(' ').len();
        self.text.truncate(kept);
        self.push("\n")?;
        let amount = if self.indent < LIMIT {
            self.indent.max(0) + plus
        } else {
            (LIMIT + (self.indent - LIMIT) / (STD / 2)) % LIMIT + plus
        };
        self.push_spaces(amount.max(0) as usize)?;
        self.push(text)?;
        self.indent = (self.indent + after).max(0);
        Ok(())
    }

    /// A buffer for one piece whose placement depends on its length.
    fn piece(&self) -> Self {
        Output {
            arena: self.arena,
            text: ArenaString::new(self.arena),
            supported: true,
            indent: self.indent,
            wrap: self.wrap,
            layout: self.layout,
        }
    }

    /// Appends a piece built with [`Self::piece`].
    fn absorb(&mut self, piece: &Output<'_>) -> Result<(), SqlError> {
        self.supported &= piece.supported;
        self.indent = piece.indent;
        self.push(piece.text.as_str())
    }

    /// The length of the current output line.
    fn line(&self) -> usize {
        let text = self.text.as_str();
        text.rfind('\n')
            .map_or(text.len(), |at| text.len() - at - 1)
    }
}

fn full<T>(_: T) -> SqlError {
    arena_full()
}

fn push_name(output: &mut Output<'_>, name: &str) -> Result<(), SqlError> {
    let mut identifier = crate::util::StackStr::<130>::new();
    write_identifier(&mut identifier, name);
    output.push(identifier.as_str())
}

/// The scope a nested query sees: the enclosing scope one level further out.
fn outward<'x>(scope: &[Joined<'x>], arena: &'x Arena) -> Result<&'x [Joined<'x>], SqlError> {
    arena
        .alloc_slice_with(scope.len(), |index| Joined {
            level: scope[index].level.saturating_add(1),
            ..scope[index]
        })
        .map(|slice| &*slice)
        .map_err(full)
}

/// Whether `name` is already some item's printed name.
fn reserved(scope: &[Joined<'_>], taken: &[Joined<'_>], name: &str) -> bool {
    scope
        .iter()
        .chain(taken)
        .any(|joined| !joined.cte && joined.printed == name)
}

/// `set_rtable_names`: `name`, or `name_N` for the first free `N`.
fn unique<'x>(
    scope: &[Joined<'_>],
    taken: &[Joined<'_>],
    name: &'x str,
    arena: &'x Arena,
) -> Result<&'x str, SqlError> {
    if !reserved(scope, taken, name) {
        return Ok(name);
    }
    for suffix in 1.. {
        let candidate = crate::stack_format!(140, "{}_{}", name, suffix);
        if !reserved(scope, taken, candidate.as_str()) {
            return arena.alloc_str(candidate.as_str()).map_err(full);
        }
    }
    unreachable!("an unused suffix exists")
}

/// A single-row or several-row `VALUES` list: its rows.
fn values_rows<'x>(
    select: &'x Select<'x>,
    rows: &mut ArenaList<'x, &'x Select<'x>>,
) -> Result<bool, SqlError> {
    fn leaves<'x>(
        tree: &'x SetTree<'x>,
        rows: &mut ArenaList<'x, &'x Select<'x>>,
    ) -> Result<bool, SqlError> {
        match tree {
            SetTree::Select(select) if select.written.values => {
                rows.push(select).map_err(full)?;
                Ok(true)
            }
            SetTree::Select(_) => Ok(false),
            SetTree::Op {
                syntax: crate::sql::ast::SetOperationSyntax::Values,
                left,
                right,
                ..
            } => Ok(leaves(left, rows)? && leaves(right, rows)?),
            SetTree::Op { .. } => Ok(false),
        }
    }
    if !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || !select.locking.is_empty()
    {
        return Ok(false);
    }
    match select.set_body {
        Some(tree) => leaves(tree, rows),
        None if select.written.values => {
            rows.push(select).map_err(full)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// One output column of an analyzed query.
#[derive(Clone, Copy)]
struct Target<'x> {
    expression: &'x Expr<'x>,
    /// The name the query gives it (`tle->resname`).
    name: &'x str,
    /// A reference to an item's column, which needs no `AS` of its own name.
    column: Option<&'x str>,
    /// A searched `CASE` printed as an indented block.
    case_block: bool,
}

/// A query level ready to print: its scope and analyzed target list.
struct Level<'x> {
    scope: &'x [Joined<'x>],
    /// The level's own items (the front of `scope`).
    own: usize,
    targets: &'x [Target<'x>],
}

impl<'r> Writer<'r> {
    fn context<'x>(&self, scope: &'x [Joined<'x>], nested: bool, own: usize) -> Context<'x>
    where
        'r: 'x,
    {
        Context {
            storage: self.storage,
            txid: self.txid,
            table: None,
            parameters: &[],
            joined: scope,
            pretty: self.pretty,
            qualify: self.qualify || nested || own != 1,
        }
    }

    /// Binds a top-level expression's routines and types to the identities
    /// the view captured.
    fn bind<'x>(
        &self,
        expression: &'x Expr<'x>,
        arena: &'x Arena,
    ) -> Result<&'x Expr<'x>, SqlError> {
        match self.dependencies {
            Some(dependencies) if !expression.contains_subquery() => {
                crate::sql::query::bind_stored_expression(
                    expression,
                    self.storage,
                    self.txid,
                    dependencies,
                    arena,
                )
            }
            _ => Ok(expression),
        }
    }

    /// The output columns' names and types of `select`, read under `scope`.
    pub(super) fn output_columns<'x>(
        &self,
        scope: &'x [Joined<'x>],
        select: &'x Select<'x>,
        arena: &'x Arena,
    ) -> Result<Option<&'x [Column<'x>]>, SqlError>
    where
        'r: 'x,
    {
        let mut rows = ArenaList::new(arena);
        if values_rows(select, &mut rows)? {
            let rows = rows.as_slice();
            let context = self.context(scope, true, 0);
            let width = rows[0].items.len();
            let mut columns = ArenaList::new(arena);
            for column in 0..width {
                let name = arena
                    .alloc_str_display(format_args!("column{}", column + 1))
                    .map_err(full)?;
                let type_oid = values_type(&context, rows, column)?;
                columns
                    .push(Column {
                        name,
                        type_oid,
                        merged: None,
                    })
                    .map_err(full)?;
            }
            return Ok(Some(columns.as_slice()));
        }
        let mut select = select;
        while let Some(tree) = select.set_body {
            let mut leftmost = tree;
            while let SetTree::Op { left, .. } = leftmost {
                leftmost = left;
            }
            let SetTree::Select(leaf) = leftmost else {
                return Ok(None);
            };
            select = leaf;
        }
        let Some(level) = self.level(scope, select, true, arena)? else {
            return Ok(None);
        };
        let context = self.context(level.scope, true, level.own);
        let mut columns = ArenaList::new(arena);
        for target in level.targets {
            columns
                .push(Column {
                    name: target.name,
                    type_oid: context.type_of(target.expression)?,
                    merged: None,
                })
                .map_err(full)?;
        }
        Ok(Some(columns.as_slice()))
    }

    /// Builds one query level's scope and expands its target list.
    fn level<'x>(
        &self,
        around: &'x [Joined<'x>],
        select: &'x Select<'x>,
        bind: bool,
        arena: &'x Arena,
    ) -> Result<Option<Level<'x>>, SqlError>
    where
        'r: 'x,
    {
        let Some(ctes) = self.ctes(around, select, arena)? else {
            return Ok(None);
        };
        let mut builder = Builder {
            writer: *self,
            around: ctes,
            entries: ArenaList::new(arena),
            arena,
        };
        let mut parts = ArenaList::new(arena);
        if let Some(from) = &select.from {
            for segment in segments(from, arena)? {
                let Some(part) = builder.segment(segment.0, segment.1)? else {
                    return Ok(None);
                };
                parts.push(part).map_err(full)?;
            }
        }
        if builder.entries.len() > MAX_LEVEL_ITEMS {
            return Ok(None);
        }
        let own = builder.entries.len();
        let scope = arena
            .alloc_slice_with(own + ctes.len(), |index| {
                if index < own {
                    builder.entries.as_slice()[index]
                } else {
                    ctes[index - own]
                }
            })
            .map_err(full)?;
        let scope: &'x [Joined<'x>] = scope;
        let mut targets = ArenaList::new(arena);
        let star = |targets: &mut ArenaList<'x, Target<'x>>,
                    part: &[(u16, u16)]|
         -> Result<(), SqlError> {
            for &(entry, column) in part {
                let joined = &scope[usize::from(entry)];
                let name = joined.columns.name(usize::from(column));
                let qualifier = (joined.addressable
                    && joined.columns.merged(usize::from(column)).is_none())
                .then_some(joined.name);
                let expression = arena
                    .alloc(Expr::Column { qualifier, name })
                    .map_err(full)?;
                targets
                    .push(Target {
                        expression,
                        name,
                        column: Some(name),
                        case_block: false,
                    })
                    .map_err(full)?;
            }
            Ok(())
        };
        for item in select.items {
            match item {
                SelectItem::Expr { expression, alias } => {
                    let expression = if bind {
                        self.bind(expression, arena)?
                    } else {
                        expression
                    };
                    let column = match expression {
                        Expr::Column { name, .. } => Some(*name),
                        _ => None,
                    };
                    let case_block = matches!(
                        expression,
                        Expr::Case {
                            operand: None,
                            origin: crate::sql::ast::CaseOrigin::Written,
                            ..
                        }
                    );
                    targets
                        .push(Target {
                            expression,
                            name: alias
                                .unwrap_or_else(|| crate::sql::exec::derived_name(expression)),
                            column,
                            case_block,
                        })
                        .map_err(full)?;
                }
                SelectItem::Wildcard(_) => {
                    for part in parts.as_slice() {
                        star(&mut targets, part)?;
                    }
                }
                SelectItem::TableWildcard(name) => {
                    let Some(entry) = scope[..own]
                        .iter()
                        .position(|joined| joined.addressable && joined.name == *name)
                    else {
                        return Ok(None);
                    };
                    let columns = scope[entry].columns.len();
                    let part = arena
                        .alloc_slice_with(columns, |column| (entry as u16, column as u16))
                        .map_err(full)?;
                    star(&mut targets, part)?;
                }
                SelectItem::RecordStar(_) => return Ok(None),
            }
        }
        Ok(Some(Level {
            scope,
            own,
            targets: targets.as_slice(),
        }))
    }

    /// The `WITH` queries a level's FROM items can name, then `around`.
    fn ctes<'x>(
        &self,
        around: &'x [Joined<'x>],
        select: &'x Select<'x>,
        arena: &'x Arena,
    ) -> Result<Option<&'x [Joined<'x>]>, SqlError>
    where
        'r: 'x,
    {
        if select.with.is_empty() {
            return Ok(Some(around));
        }
        let count = select.with.len();
        let mut entries = ArenaList::new(arena);
        // Undefined columns until each query is analyzed in written order.
        for cte in select.with {
            entries
                .push(Joined {
                    name: cte.name,
                    printed: "",
                    columns: Columns::Derived(&[]),
                    unqualified: false,
                    addressable: false,
                    level: 0,
                    row_type: None,
                    hides: 0,
                    cte: true,
                })
                .map_err(full)?;
        }
        for entry in around {
            entries.push(*entry).map_err(full)?;
        }
        let scope = entries.into_mut_slice().map_err(full)?;
        for (index, cte) in select.with.iter().enumerate() {
            if cte.dml.is_some() || cte.search.is_some() || cte.cycle.is_some() {
                return Ok(None);
            }
            // A query sees the earlier queries; a recursive one, itself.
            let visible: &'x [Joined<'x>] = {
                let copy = arena.alloc_slice_copy(&scope[..]).map_err(full)?;
                for (later, entry) in copy.iter_mut().enumerate().take(count) {
                    if later > index || (later == index && !cte.recursive) {
                        entry.name = "";
                    }
                }
                outward(copy, arena)?
            };
            let Some(columns) = self.output_columns(visible, cte.query, arena)? else {
                return Ok(None);
            };
            let columns = rename(columns, cte.columns, arena)?;
            let Some(columns) = columns else {
                return Ok(None);
            };
            scope[index].columns = Columns::Derived(columns);
        }
        Ok(Some(scope))
    }

    /// `get_query_def`: prints `select` at the output's indentation, which
    /// it restores afterwards.
    pub(super) fn query<'x>(
        &self,
        around: &'x [Joined<'x>],
        select: &'x Select<'x>,
        naming: Naming<'_>,
        nested: bool,
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        let start = output.indent;
        self.select_query(around, select, naming, nested, arena, output)?;
        output.indent = start;
        Ok(())
    }

    fn select_query<'x>(
        &self,
        around: &'x [Joined<'x>],
        select: &'x Select<'x>,
        naming: Naming<'_>,
        nested: bool,
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        let mut rows = ArenaList::new(arena);
        if values_rows(select, &mut rows)? {
            output.indent += STD;
            output.push(" ")?;
            let context = self.context(around, nested, 0);
            return values_list(&context, rows.as_slice(), output);
        }
        if select.set_body.is_none() {
            let Some(level) = self.level(around, select, !nested, arena)? else {
                return output.unsupported();
            };
            self.with_clause(&level, select, arena, output)?;
            return self.basic(&level, select, naming, nested, arena, output);
        }
        // A set operation's leaves read their own FROM items under the
        // level's `WITH` queries.
        let Some(ctes) = self.ctes(around, select, arena)? else {
            return output.unsupported();
        };
        let level = Level {
            scope: ctes,
            own: 0,
            targets: &[],
        };
        self.with_clause(&level, select, arena, output)?;
        let tree = select.set_body.expect("set operation");
        let mut leftmost = tree;
        while let SetTree::Op { left, .. } = leftmost {
            leftmost = left;
        }
        let SetTree::Select(first) = leftmost else {
            return output.unsupported();
        };
        // A set operation's leaves read under its `WITH` queries.
        let leaves = outward(ctes, arena)?;
        let Some(columns) = self.output_columns(leaves, first, arena)? else {
            return output.unsupported();
        };
        let names = arena
            .alloc_slice_with(columns.len(), |index| columns[index].name)
            .map_err(full)?;
        self.set_operation(leaves, tree, naming, names, arena, output)?;
        // `ORDER BY` on a set operation names output columns by position.
        if !select.order_by.is_empty() {
            output.keyword(" ORDER BY ", -STD, STD, 1)?;
            for (index, order) in select.order_by.iter().enumerate() {
                if index != 0 {
                    output.push(", ")?;
                }
                let position = match order.expression {
                    Expr::Int(position)
                        if *position >= 1 && (*position as usize) <= names.len() =>
                    {
                        *position as usize
                    }
                    Expr::Column {
                        qualifier: None,
                        name,
                    } => match names.iter().position(|found| found == name) {
                        Some(found) => found + 1,
                        None => return output.unsupported(),
                    },
                    _ => return output.unsupported(),
                };
                output.push(crate::stack_format!(24, "{}", position).as_str())?;
                order_direction(order, output)?;
            }
        }
        let context = self.context(ctes, true, 0);
        self.limits(&context, select, output)
    }

    /// `get_with_clause`.
    fn with_clause<'x>(
        &self,
        level: &Level<'x>,
        select: &'x Select<'x>,
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        if select.with.is_empty() {
            return Ok(());
        }
        output.indent += STD;
        output.push(" ")?;
        let mut separator = if select.with.iter().any(|cte| cte.recursive) {
            "WITH RECURSIVE "
        } else {
            "WITH "
        };
        // A query reads under the level's own names and the queries around.
        let scope = outward(level.scope, arena)?;
        for cte in select.with {
            output.push(separator)?;
            push_name(output, cte.name)?;
            if !cte.columns.is_empty() {
                output.push("(")?;
                for (index, column) in cte.columns.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    push_name(output, column)?;
                }
                output.push(")")?;
            }
            output.push(" AS ")?;
            output.push(match cte.materialization {
                crate::sql::ast::CteMaterialization::Default => "",
                crate::sql::ast::CteMaterialization::Materialized => "MATERIALIZED ",
                crate::sql::ast::CteMaterialization::NotMaterialized => "NOT MATERIALIZED ",
            })?;
            output.push("(")?;
            output.keyword("", 0, 0, 0)?;
            self.query(
                scope,
                cte.query,
                Naming {
                    names: None,
                    visible: true,
                    unknown: false,
                },
                true,
                arena,
                output,
            )?;
            output.keyword("", 0, 0, 0)?;
            output.push(")")?;
            separator = ", ";
        }
        output.indent -= STD;
        output.keyword("", 0, 0, 0)
    }

    /// `get_setop_query`.
    fn set_operation<'x>(
        &self,
        scope: &'x [Joined<'x>],
        tree: &'x SetTree<'x>,
        naming: Naming<'_>,
        names: &[&str],
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        match tree {
            SetTree::Select(leaf) => {
                let paren = !leaf.with.is_empty()
                    || !leaf.order_by.is_empty()
                    || !leaf.locking.is_empty()
                    || leaf.limit.is_some()
                    || leaf.offset.is_some();
                if paren {
                    output.push("(")?;
                }
                self.query(scope, leaf, naming, true, arena, output)?;
                if paren {
                    output.push(")")?;
                }
                Ok(())
            }
            SetTree::Op {
                operator,
                all,
                left,
                right,
                ..
            } => {
                let paren = match left {
                    SetTree::Op {
                        operator: inner,
                        all: inner_all,
                        ..
                    } => inner != operator || inner_all != all,
                    SetTree::Select(_) => false,
                };
                let indent = if paren {
                    output.push("(")?;
                    output.keyword("", STD, 0, 0)?;
                    STD
                } else {
                    0
                };
                self.set_operation(scope, left, naming, names, arena, output)?;
                if paren {
                    output.keyword(") ", -indent, 0, 0)?;
                } else {
                    output.keyword("", -indent, 0, 0)?;
                }
                output.push(match operator {
                    SetOp::Union => "UNION ",
                    SetOp::Intersect => "INTERSECT ",
                    SetOp::Except => "EXCEPT ",
                })?;
                if *all {
                    output.push("ALL ")?;
                }
                let paren = matches!(right, SetTree::Op { .. });
                let indent = if paren {
                    output.push("(")?;
                    STD
                } else {
                    0
                };
                output.keyword("", indent, 0, 0)?;
                // A later leaf's columns take the operation's names.
                self.set_operation(
                    scope,
                    right,
                    Naming {
                        names: Some(names),
                        visible: false,
                        unknown: naming.unknown,
                    },
                    names,
                    arena,
                    output,
                )?;
                output.indent -= indent;
                if paren {
                    output.keyword(")", 0, 0, 0)?;
                }
                Ok(())
            }
        }
    }

    /// `get_basic_select_query` and the clauses `get_select_query_def`
    /// prints after it.
    fn basic<'x>(
        &self,
        level: &Level<'x>,
        select: &'x Select<'x>,
        naming: Naming<'_>,
        nested: bool,
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        if naming
            .names
            .is_some_and(|names| names.len() != level.targets.len())
        {
            return output.unsupported();
        }
        let context = self.context(level.scope, nested, level.own);
        let bind = |expression: &'x Expr<'x>| -> Result<&'x Expr<'x>, SqlError> {
            if nested {
                Ok(expression)
            } else {
                self.bind(expression, arena)
            }
        };
        output.indent += STD;
        output.push(" SELECT")?;
        // `ORDER BY 1`, `GROUP BY name` and the like name a target.
        let target = |expression: &'x Expr<'x>, group: bool| -> &'x Expr<'x> {
            match expression {
                Expr::Int(position) if *position >= 1 => level
                    .targets
                    .get((*position - 1) as usize)
                    .map_or(expression, |target| target.expression),
                Expr::Column {
                    qualifier: None,
                    name,
                } if !(group && context.column_known(None, name)) => level
                    .targets
                    .iter()
                    .find(|target| target.name == *name)
                    .map_or(expression, |target| target.expression),
                _ => expression,
            }
        };
        if select.distinct {
            if select.distinct_on.is_empty() {
                output.push(" DISTINCT")?;
            } else {
                output.push(" DISTINCT ON (")?;
                for (index, expression) in select.distinct_on.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    sort_group(&context, target(bind(expression)?, false), output)?;
                }
                output.push(")")?;
            }
        }
        self.targets(&context, level.targets, naming, output)?;
        if let Some(from) = &select.from {
            output.keyword(" FROM ", -STD, STD, 2)?;
            self.item_list(&context, level.scope, from, arena, output)?;
        }
        if let Some(condition) = select.where_clause {
            output.keyword(" WHERE ", -STD, STD, 1)?;
            context.write(bind(condition)?, Some(oid::BOOL), output)?;
        }
        if !select.group_by.is_empty() || !select.grouping_sets.is_empty() {
            output.keyword(" GROUP BY ", -STD, STD, 1)?;
            if select.grouping_set_quantifier == crate::sql::ast::GroupingSetQuantifier::Distinct {
                output.push("DISTINCT ")?;
            }
            let mut items = ArenaList::new(arena);
            for expression in select.group_by {
                items.push(target(bind(expression)?, true)).map_err(full)?;
            }
            let items = items.as_slice();
            if select.grouping_sets.is_empty() {
                for (index, expression) in items.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    sort_group(&context, expression, output)?;
                }
            } else {
                for (index, element) in select.written.grouping.iter().enumerate() {
                    if index != 0 {
                        output.push(", ")?;
                    }
                    grouping_element(&context, items, element, true, output)?;
                }
            }
        }
        if let Some(having) = select.having {
            output.keyword(" HAVING ", -STD, STD, 0)?;
            context.write(bind(having)?, Some(oid::BOOL), output)?;
        }
        if !select.windows.is_empty() {
            output.keyword(" WINDOW ", -STD, STD, 1)?;
            for (index, window) in select.windows.iter().enumerate() {
                let Some(name) = window.name else {
                    return output.unsupported();
                };
                if index != 0 {
                    output.push(", ")?;
                }
                push_name(output, name)?;
                output.push(" AS ")?;
                window_spec(&context, window, output)?;
            }
        }
        if !select.order_by.is_empty() {
            output.keyword(" ORDER BY ", -STD, STD, 1)?;
            for (index, order) in select.order_by.iter().enumerate() {
                if index != 0 {
                    output.push(", ")?;
                }
                let expression = target(bind(order.expression)?, false);
                sort_group(&context, expression, output)?;
                order_direction(order, output)?;
            }
        }
        self.limits(&context, select, output)?;
        self.locking(level, select, output)
    }

    /// `OFFSET`, then `LIMIT` or `FETCH FIRST ... WITH TIES`.
    fn limits(
        &self,
        context: &Context<'_>,
        select: &Select<'_>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        if let Some(offset) = select.offset {
            output.keyword(" OFFSET ", -STD, STD, 0)?;
            context.write(offset, None, output)?;
        }
        if select.written.limit_all {
            output.keyword(" LIMIT ", -STD, STD, 0)?;
            output.push("ALL")?;
        }
        if let Some(limit) = select.limit {
            if select.with_ties {
                output.keyword(" FETCH FIRST ", -STD, STD, 0)?;
                output.push("(")?;
                context.write(limit, None, output)?;
                output.push(") ROWS WITH TIES")?;
            } else {
                output.keyword(" LIMIT ", -STD, STD, 0)?;
                if matches!(limit, Expr::Null) {
                    output.push("ALL")?;
                } else {
                    context.write(limit, None, output)?;
                }
            }
        }
        Ok(())
    }

    /// One row mark per locked item, in the order the clauses name them.
    fn locking(
        &self,
        level: &Level<'_>,
        select: &Select<'_>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let mut marks: [(usize, LockStrength, LockWait); MAX_LEVEL_ITEMS] =
            [(0, LockStrength::Update, LockWait::Wait); MAX_LEVEL_ITEMS];
        let mut count = 0;
        let mut mark = |entry: usize, clause: &crate::sql::ast::LockClause<'_>| {
            if let Some(existing) = marks[..count].iter_mut().find(|mark| mark.0 == entry) {
                if (clause.strength as u8) < (existing.1 as u8) {
                    existing.1 = clause.strength;
                }
                return true;
            }
            if count == MAX_LEVEL_ITEMS {
                return false;
            }
            marks[count] = (entry, clause.strength, clause.wait);
            count += 1;
            true
        };
        for clause in select.locking {
            if clause.of.is_empty() {
                for (entry, joined) in level.scope[..level.own].iter().enumerate() {
                    if joined.addressable && !joined.printed.is_empty() && !mark(entry, clause) {
                        return output.unsupported();
                    }
                }
            } else {
                for name in clause.of {
                    let Some(entry) = level.scope[..level.own]
                        .iter()
                        .position(|joined| joined.addressable && joined.name == *name)
                    else {
                        return output.unsupported();
                    };
                    if !mark(entry, clause) {
                        return output.unsupported();
                    }
                }
            }
        }
        for &(entry, strength, wait) in &marks[..count] {
            output.keyword(
                match strength {
                    LockStrength::KeyShare => " FOR KEY SHARE",
                    LockStrength::Share => " FOR SHARE",
                    LockStrength::NoKeyUpdate => " FOR NO KEY UPDATE",
                    LockStrength::Update => " FOR UPDATE",
                },
                -STD,
                STD,
                0,
            )?;
            output.push(" OF ")?;
            push_name(output, level.scope[entry].printed)?;
            output.push(match wait {
                LockWait::Wait => "",
                LockWait::NoWait => " NOWAIT",
                LockWait::SkipLocked => " SKIP LOCKED",
            })?;
        }
        Ok(())
    }

    /// `get_target_list`, wrapping at the output's wrap column.
    fn targets(
        &self,
        context: &Context<'_>,
        targets: &[Target<'_>],
        naming: Naming<'_>,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let mut last_multiline = false;
        for (index, target) in targets.iter().enumerate() {
            output.push(if index == 0 { " " } else { ", " })?;
            let mut piece = output.piece();
            let attribute = if naming.unknown && matches!(target.expression, Expr::Str(_)) {
                let Expr::Str(text) = target.expression else {
                    unreachable!("checked literal");
                };
                piece.push("'")?;
                for (index, part) in text.split('\'').enumerate() {
                    if index != 0 {
                        piece.push("''")?;
                    }
                    piece.push(part)?;
                }
                piece.push("'")?;
                Some("?column?")
            } else if target.case_block {
                context.write(target.expression, None, &mut piece)?;
                None
            } else if let Some(row) = context.whole_row(target.expression) {
                // `get_variable` casts a top-level whole-row variable to its
                // row type so that it is not read back as a `*` expansion.
                context.write(target.expression, None, &mut piece)?;
                match row.row_type {
                    Some(row_type) if row_type != oid::RECORD => {
                        match super::super::user_type_name_fixed(self.storage, self.txid, row_type)?
                        {
                            Some(name) => {
                                piece.push("::")?;
                                piece.push(name.as_str())?;
                            }
                            None => piece.unsupported()?,
                        }
                    }
                    _ => piece.unsupported()?,
                }
                None
            } else {
                let natural = context.type_of(target.expression)?;
                context.write(target.expression, natural, &mut piece)?;
                match target.column {
                    Some(column) => Some(column),
                    None if naming.visible => None,
                    None => Some("?column?"),
                }
            };
            let name = naming
                .names
                .and_then(|names| names.get(index).copied())
                .unwrap_or(target.name);
            if attribute != Some(name) {
                piece.push(" AS ")?;
                push_name(&mut piece, name)?;
            }
            let text = piece.text.as_str();
            let leading_newline = text.starts_with('\n');
            if output.wrap >= 0 {
                if leading_newline {
                    let kept = output.text.as_str().trim_end_matches(' ').len();
                    output.text.truncate(kept);
                } else if index > 0
                    && (last_multiline || output.line() + text.len() > output.wrap as usize)
                {
                    output.keyword("", -STD, STD, VAR)?;
                }
                last_multiline = text[usize::from(leading_newline)..].contains('\n');
            }
            output.absorb(&piece)?;
        }
        Ok(())
    }

    /// `get_from_clause`: comma-separated items, wrapping each later one.
    fn item_list<'x>(
        &self,
        context: &Context<'x>,
        scope: &'x [Joined<'x>],
        from: &'x FromClause<'x>,
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        let mut printer = Printer {
            writer: *self,
            context,
            scope,
            next: 0,
            arena,
        };
        for (index, segment) in segments(from, arena)?.iter().enumerate() {
            if index == 0 {
                printer.segment(segment.0, segment.1, output)?;
                continue;
            }
            output.push(", ")?;
            let mut piece = output.piece();
            printer.segment(segment.0, segment.1, &mut piece)?;
            let text = piece.text.as_str();
            if output.wrap >= 0 {
                if text.starts_with('\n') {
                    let kept = output.text.as_str().trim_end_matches(' ').len();
                    output.text.truncate(kept);
                } else if output.line() + text.len() > output.wrap as usize {
                    output.keyword("", -STD, STD, VAR)?;
                }
            }
            output.absorb(&piece)?;
        }
        Ok(())
    }

    /// The scope of an `UPDATE ... FROM` or `DELETE ... USING` statement:
    /// its FROM items before `around` (the rule's rows and the target).
    pub(super) fn dml_scope<'x>(
        &self,
        around: &'x [Joined<'x>],
        from: &'x FromClause<'x>,
        arena: &'x Arena,
    ) -> Result<Option<&'x [Joined<'x>]>, SqlError>
    where
        'r: 'x,
    {
        let mut builder = Builder {
            writer: *self,
            around,
            entries: ArenaList::new(arena),
            arena,
        };
        for segment in segments(from, arena)? {
            if builder.segment(segment.0, segment.1)?.is_none() {
                return Ok(None);
            }
        }
        let own = builder.entries.len();
        let scope = arena
            .alloc_slice_with(own + around.len(), |index| {
                if index < own {
                    builder.entries.as_slice()[index]
                } else {
                    around[index - own]
                }
            })
            .map_err(full)?;
        Ok(Some(scope))
    }

    /// `get_from_clause` of an `UPDATE` or `DELETE`: `keyword`, then the
    /// items `dml_scope` built, without the target.
    pub(super) fn dml_from<'x>(
        &self,
        context: &Context<'x>,
        scope: &'x [Joined<'x>],
        from: &'x FromClause<'x>,
        keyword: &str,
        arena: &'x Arena,
        output: &mut Output<'_>,
    ) -> Result<(), SqlError>
    where
        'r: 'x,
    {
        output.keyword(keyword, -STD, STD, 2)?;
        self.item_list(context, scope, from, arena, output)
    }
}

/// Renames a derived item's leading columns by its alias column list.
fn rename<'x>(
    columns: &'x [Column<'x>],
    aliases: &[&'x str],
    arena: &'x Arena,
) -> Result<Option<&'x [Column<'x>]>, SqlError> {
    if aliases.len() > columns.len() {
        return Ok(None);
    }
    if aliases.is_empty() {
        return Ok(Some(columns));
    }
    arena
        .alloc_slice_with(columns.len(), |index| Column {
            name: aliases.get(index).copied().unwrap_or(columns[index].name),
            ..columns[index]
        })
        .map(|slice| Some(&*slice))
        .map_err(full)
}

/// The FROM list split at its commas: each item and the joins it starts.
fn segments<'x>(
    from: &'x FromClause<'x>,
    arena: &'x Arena,
) -> Result<&'x [(&'x TableRef<'x>, &'x [crate::sql::ast::Join<'x>])], SqlError> {
    let mut segments = ArenaList::new(arena);
    let mut base = &from.base;
    let mut start = 0;
    for (index, join) in from.joins.iter().enumerate() {
        if join.kind == JoinKind::Comma {
            segments
                .push((base, &from.joins[start..index]))
                .map_err(full)?;
            base = &join.table;
            start = index + 1;
        }
    }
    segments.push((base, &from.joins[start..])).map_err(full)?;
    Ok(segments.as_slice())
}

/// A join tree's output columns: (item, column) by position in the level.
type Part<'x> = &'x [(u16, u16)];

/// Builds one level's FROM items in the order the printer reads them.
struct Builder<'r, 'x> {
    writer: Writer<'r>,
    around: &'x [Joined<'x>],
    entries: ArenaList<'x, Joined<'x>>,
    arena: &'x Arena,
}

impl<'r: 'x, 'x> Builder<'r, 'x> {
    fn push(&mut self, entry: Joined<'x>) -> Result<u16, SqlError> {
        let index = self.entries.len();
        if index >= MAX_LEVEL_ITEMS {
            return Err(arena_full());
        }
        self.entries.push(entry).map_err(full)?;
        Ok(index as u16)
    }

    fn whole(&self, entry: u16) -> Result<Part<'x>, SqlError> {
        let columns = self.entries.as_slice()[usize::from(entry)].columns.len();
        self.arena
            .alloc_slice_with(columns, |column| (entry, column as u16))
            .map(|slice| &*slice)
            .map_err(full)
    }

    /// The scope a subquery or function argument of this level reads.
    fn inner_scope(&self) -> Result<&'x [Joined<'x>], SqlError> {
        let own = self.entries.as_slice();
        let joined = self
            .arena
            .alloc_slice_with(own.len() + self.around.len(), |index| {
                if index < own.len() {
                    own[index]
                } else {
                    self.around[index - own.len()]
                }
            })
            .map_err(full)?;
        outward(joined, self.arena)
    }

    fn named(&self, alias: Option<&'x str>, name: &'x str) -> Result<&'x str, SqlError> {
        unique(
            self.around,
            self.entries.as_slice(),
            alias.unwrap_or(name),
            self.arena,
        )
    }

    fn derived(
        &mut self,
        alias: Option<&'x str>,
        name: &'x str,
        columns: &'x [Column<'x>],
    ) -> Result<Option<Part<'x>>, SqlError> {
        let printed = self.named(alias, name)?;
        let entry = self.push(Joined {
            name: alias.unwrap_or(name),
            printed,
            columns: Columns::Derived(columns),
            unqualified: true,
            addressable: true,
            level: 0,
            row_type: Some(oid::RECORD),
            hides: 0,
            cte: false,
        })?;
        self.whole(entry).map(Some)
    }

    fn segment(
        &mut self,
        base: &'x TableRef<'x>,
        joins: &'x [crate::sql::ast::Join<'x>],
    ) -> Result<Option<Part<'x>>, SqlError> {
        let first = self.entries.len();
        let Some(mut left) = self.item(base)? else {
            return Ok(None);
        };
        for join in joins {
            if join.kind == JoinKind::Comma {
                return Ok(None);
            }
            let Some(right) = self.item(&join.table)? else {
                return Ok(None);
            };
            let names: &[&str] = match (join.using, join.natural) {
                (Some(using), false) => using.columns,
                (None, true) => {
                    let mut common = ArenaList::new(self.arena);
                    for &(entry, column) in left {
                        let name = self.entries.as_slice()[usize::from(entry)]
                            .columns
                            .name(usize::from(column));
                        if find(self.entries.as_slice(), right, name).is_some() {
                            common.push(name).map_err(full)?;
                        }
                    }
                    if common.is_empty() {
                        return Ok(None);
                    }
                    common.as_slice()
                }
                _ => &[],
            };
            if names.is_empty() {
                let joined = self
                    .arena
                    .alloc_slice_with(left.len() + right.len(), |index| {
                        if index < left.len() {
                            left[index]
                        } else {
                            right[index - left.len()]
                        }
                    })
                    .map_err(full)?;
                left = joined;
                continue;
            }
            let entries = self.entries.as_slice();
            let mut merged = ArenaList::new(self.arena);
            for name in names {
                let (Some(from_left), Some(from_right)) =
                    (find(entries, left, name), find(entries, right, name))
                else {
                    return Ok(None);
                };
                let target = |(entry, column): (u16, u16)| {
                    let joined = &entries[usize::from(entry)];
                    joined.columns.merged(usize::from(column)).unwrap_or((
                        Some(joined.printed),
                        joined.columns.name(usize::from(column)),
                    ))
                };
                let column_type =
                    |(entry, column): (u16, u16)| match entries[usize::from(entry)].columns {
                        Columns::Derived(columns) => columns[usize::from(column)].type_oid,
                        Columns::Table(_) => None,
                    };
                merged
                    .push(Column {
                        name,
                        type_oid: column_type(from_left).or(column_type(from_right)),
                        merged: Some(match join.kind {
                            JoinKind::Right => target(from_right),
                            JoinKind::Full => (None, name),
                            _ => target(from_left),
                        }),
                    })
                    .map_err(full)?;
            }
            let mut hides = 0u64;
            for index in first..self.entries.len() {
                hides |= 1 << index;
            }
            let alias = join.using.and_then(|using| using.alias);
            let entry = self.push(Joined {
                name: alias.unwrap_or(""),
                printed: alias.unwrap_or(""),
                columns: Columns::Derived(merged.as_slice()),
                unqualified: true,
                addressable: alias.is_some(),
                level: 0,
                row_type: Some(oid::RECORD),
                hides,
                cte: false,
            })?;
            let mut part = ArenaList::new(self.arena);
            for column in 0..names.len() {
                part.push((entry, column as u16)).map_err(full)?;
            }
            for &(item, column) in left.iter().chain(right) {
                let name = self.entries.as_slice()[usize::from(item)]
                    .columns
                    .name(usize::from(column));
                if !names.contains(&name) {
                    part.push((item, column)).map_err(full)?;
                }
            }
            left = part.as_slice();
        }
        Ok(Some(left))
    }

    fn item(&mut self, item: &'x TableRef<'x>) -> Result<Option<Part<'x>>, SqlError> {
        if item.cte.is_some() || item.bound_table.is_some() || item.view_access.is_some() {
            return Ok(None);
        }
        if item.join_members.is_some() {
            let Some(Select {
                from: Some(inner), ..
            }) = item.subquery
            else {
                return Ok(None);
            };
            let start = self.entries.len();
            let mut parts = ArenaList::new(self.arena);
            for segment in segments(inner, self.arena)? {
                let Some(part) = self.segment(segment.0, segment.1)? else {
                    return Ok(None);
                };
                parts.push(part).map_err(full)?;
            }
            let [part] = parts.as_slice() else {
                return Ok(None);
            };
            let Some(alias) = item.alias else {
                return Ok(Some(part));
            };
            // An alias hides the members; their names stay reserved.
            let entries = self.entries.as_mut_slice().map_err(full)?;
            for entry in &mut entries[start..] {
                entry.unqualified = false;
                entry.addressable = false;
            }
            let entries = self.entries.as_slice();
            let mut columns = ArenaList::new(self.arena);
            for &(entry, column) in *part {
                let joined = &entries[usize::from(entry)];
                columns
                    .push(Column {
                        name: joined.columns.name(usize::from(column)),
                        type_oid: match joined.columns {
                            Columns::Derived(columns) => columns[usize::from(column)].type_oid,
                            Columns::Table(_) => None,
                        },
                        merged: None,
                    })
                    .map_err(full)?;
            }
            let Some(columns) = rename(
                columns.as_slice(),
                item.col_alias.unwrap_or(&[]),
                self.arena,
            )?
            else {
                return Ok(None);
            };
            return self.derived(Some(alias), alias, columns);
        }
        if let Some(subquery) = item.subquery {
            let scope = self.inner_scope()?;
            let Some(columns) = self.writer.output_columns(scope, subquery, self.arena)? else {
                return Ok(None);
            };
            let Some(columns) = rename(columns, item.col_alias.unwrap_or(&[]), self.arena)? else {
                return Ok(None);
            };
            return self.derived(item.alias, "unnamed_subquery", columns);
        }
        if item.is_function_source() {
            return self.function(item);
        }
        // A `WITH` query of this level or one around it.
        if item.schema.is_none()
            && let Some(cte) = self
                .around
                .iter()
                .find(|joined| joined.cte && joined.name == item.table)
        {
            let Columns::Derived(columns) = cte.columns else {
                return Ok(None);
            };
            let Some(columns) = rename(columns, item.col_alias.unwrap_or(&[]), self.arena)? else {
                return Ok(None);
            };
            if item.sample.is_some() {
                return Ok(None);
            }
            return self.derived(item.alias, item.table, columns);
        }
        let Some(relation) = self.relation(item)? else {
            return Ok(None);
        };
        let Some(columns) = rename(relation.columns, item.col_alias.unwrap_or(&[]), self.arena)?
        else {
            return Ok(None);
        };
        let printed = self.named(item.alias, relation.name)?;
        let entry = self.push(Joined {
            name: item.alias.unwrap_or(item.table),
            printed,
            columns: Columns::Derived(columns),
            unqualified: true,
            addressable: true,
            level: 0,
            row_type: Some(relation.row_type),
            hides: 0,
            cte: false,
        })?;
        self.whole(entry).map(Some)
    }

    fn relation(&self, item: &TableRef<'_>) -> Result<Option<Relation<'x>>, SqlError> {
        let storage = self.writer.storage;
        let txid = self.writer.txid;
        let captured = self
            .writer
            .dependencies
            .and_then(|dependencies| {
                dependencies.entries().iter().find(|dependency| {
                    matches!(
                        dependency.class,
                        crate::storage::DependencyClass::Table
                            | crate::storage::DependencyClass::View
                    ) && dependency.referenced_schema.as_str() == item.schema.unwrap_or("")
                        && dependency.referenced_name.as_str() == item.table
                })
            })
            .map(|dependency| storage.resolve_stored_query_relation(dependency, txid))
            .transpose()?;
        let resolved = match captured {
            Some(resolved) => Some(resolved),
            None => storage.resolve_relation(item.schema, item.table, txid),
        };
        let arena = self.arena;
        let alloc = |text: &str| arena.alloc_str(text).map_err(full);
        Ok(match resolved {
            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                let definition = storage.table_def(slot, txid);
                let mut columns = ArenaList::new(arena);
                for column in definition.columns() {
                    columns
                        .push(Column {
                            name: alloc(column.name.as_str())?,
                            type_oid: Some(super::super::catalog_column_type_oid(
                                storage, column, txid,
                            )?),
                            merged: None,
                        })
                        .map_err(full)?;
                }
                Some(Relation {
                    schema: alloc(definition.schema.as_str())?,
                    name: alloc(definition.name.as_str())?,
                    columns: columns.as_slice(),
                    row_type: super::super::table_row_type_oid(storage, slot, false),
                    schema_visible: storage.schema_is_on_path(definition.schema, txid),
                })
            }
            Some(crate::storage::ResolvedRelation::View(slot)) => {
                let view = storage.view(slot);
                let names = view.columns_for(txid).names();
                let types = view.columns_for(txid).types();
                let mut columns = ArenaList::new(arena);
                for (index, name) in names.iter().enumerate() {
                    columns
                        .push(Column {
                            name: alloc(name.as_str())?,
                            type_oid: types.get(index).map(|column| column.oid()),
                            merged: None,
                        })
                        .map_err(full)?;
                }
                let schema = view.schema_for(txid);
                Some(Relation {
                    schema: alloc(schema.as_str())?,
                    name: alloc(view.name_for(txid).as_str())?,
                    columns: columns.as_slice(),
                    row_type: super::super::view_row_type_oid(storage, slot, false),
                    schema_visible: storage.schema_is_on_path(schema, txid),
                })
            }
            _ => None,
        })
    }

    /// A function in FROM whose result is one scalar column per function.
    fn function(&mut self, item: &'x TableRef<'x>) -> Result<Option<Part<'x>>, SqlError> {
        let scope = self.inner_scope()?;
        let context = self.writer.context(scope, true, 0);
        let mut columns = ArenaList::new(self.arena);
        let functions: &[TableRef<'x>] = match item.rows_from {
            Some(functions) => functions,
            None => core::slice::from_ref(item),
        };
        for function in functions {
            let call = function_call(function, self.arena)?;
            let type_oid = context.type_of(call)?;
            if type_oid.is_none_or(|type_oid| {
                type_oid == oid::RECORD
                    || crate::storage::NamedTypeKind::from_oid(type_oid)
                        .is_some_and(|(kind, ..)| kind == crate::storage::NamedTypeKind::Composite)
            }) {
                return Ok(None);
            }
            let name = if item.rows_from.is_none() {
                item.alias.unwrap_or(function.table)
            } else {
                function.table
            };
            columns
                .push(Column {
                    name: unqualified_name(name),
                    type_oid,
                    merged: None,
                })
                .map_err(full)?;
        }
        if item.with_ordinality {
            columns
                .push(Column {
                    name: "ordinality",
                    type_oid: Some(oid::INT8),
                    merged: None,
                })
                .map_err(full)?;
        }
        let Some(columns) = rename(
            columns.as_slice(),
            item.col_alias.unwrap_or(&[]),
            self.arena,
        )?
        else {
            return Ok(None);
        };
        let name = match item.rows_from {
            Some(_) => "rows_from",
            None => unqualified_name(item.table),
        };
        self.derived(item.alias, name, columns)
    }
}

fn unqualified_name(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(_, name)| name)
}

struct Relation<'x> {
    schema: &'x str,
    name: &'x str,
    columns: &'x [Column<'x>],
    row_type: i32,
    schema_visible: bool,
}

/// The part column named `name`, when exactly one has that name.
fn find(entries: &[Joined<'_>], part: Part<'_>, name: &str) -> Option<(u16, u16)> {
    let mut found = None;
    for &(entry, column) in part {
        if entries[usize::from(entry)]
            .columns
            .name(usize::from(column))
            == name
        {
            if found.is_some() {
                return None;
            }
            found = Some((entry, column));
        }
    }
    found
}

/// A FROM function as the call it makes.
fn function_call<'x>(function: &TableRef<'x>, arena: &'x Arena) -> Result<&'x Expr<'x>, SqlError> {
    let args = function.func_args.unwrap_or(&[]);
    arena
        .alloc(Expr::Call {
            kind: if crate::sql::eval::sql_syntax_call_name(function.table) {
                crate::sql::ast::CallKind::SqlSyntax
            } else {
                crate::sql::ast::CallKind::Routine
            },
            name: function.table,
            bound_oid: None,
            bound_strict: None,
            source_position: function.func_source_position,
            args,
            argument_names: function.func_argument_names,
            variadic: function.func_variadic,
            star: false,
            distinct: false,
            order_by: &[],
            over: None,
            filter: None,
        })
        .map(|call| &*call)
        .map_err(full)
}

/// Prints a level's FROM items, reading their entries in build order.
struct Printer<'r, 'c, 'x> {
    writer: Writer<'r>,
    context: &'c Context<'x>,
    scope: &'x [Joined<'x>],
    next: usize,
    arena: &'x Arena,
}

impl<'r: 'x, 'c, 'x> Printer<'r, 'c, 'x> {
    fn take(&mut self) -> Joined<'x> {
        let entry = self.scope[self.next];
        self.next += 1;
        entry
    }

    fn segment(
        &mut self,
        base: &'x TableRef<'x>,
        joins: &'x [crate::sql::ast::Join<'x>],
        output: &mut Output<'_>,
    ) -> Result<(), SqlError> {
        let pretty = self.writer.pretty;
        if !pretty {
            for _ in joins {
                output.push("(")?;
            }
        }
        self.item(base, output)?;
        for join in joins {
            let keyword = match join.kind {
                JoinKind::Inner if join.on.is_none() && join.using.is_none() && !join.natural => {
                    " CROSS JOIN "
                }
                JoinKind::Inner => " JOIN ",
                JoinKind::Cross => " CROSS JOIN ",
                JoinKind::Left => " LEFT JOIN ",
                JoinKind::Right => " RIGHT JOIN ",
                JoinKind::Full => " FULL JOIN ",
                JoinKind::Comma => return output.unsupported(),
            };
            output.keyword(keyword, -STD, STD, JOIN)?;
            // A pretty right side that is itself an unaliased join needs
            // parentheses the unpretty form prints on its own.
            let paren = pretty && join.table.join_members.is_some() && join.table.alias.is_none();
            if paren {
                output.push("(")?;
            }
            self.item(&join.table, output)?;
            if paren {
                output.push(")")?;
            }
            let names = join.using.map(|using| (using.columns, using.alias));
            if join.using.is_some() || join.natural {
                let merged = self.take();
                output.push(" USING (")?;
                let columns = names.map_or(&[][..], |(columns, _)| columns);
                if join.natural {
                    for index in 0..merged.columns.len() {
                        if index != 0 {
                            output.push(", ")?;
                        }
                        push_name(output, merged.columns.name(index))?;
                    }
                } else {
                    for (index, column) in columns.iter().enumerate() {
                        if index != 0 {
                            output.push(", ")?;
                        }
                        push_name(output, column)?;
                    }
                }
                output.push(")")?;
                if let Some((_, Some(alias))) = names {
                    output.push(" AS ")?;
                    push_name(output, alias)?;
                }
            } else if let Some(condition) = join.on {
                output.push(" ON ")?;
                if !pretty {
                    output.push("(")?;
                }
                self.context.write(condition, Some(oid::BOOL), output)?;
                if !pretty {
                    output.push(")")?;
                }
            } else if join.kind != JoinKind::Inner && join.kind != JoinKind::Cross {
                output.push(" ON TRUE")?;
            }
            if !pretty {
                output.push(")")?;
            }
        }
        Ok(())
    }

    /// Whether a function's arguments reference this level's items.
    fn reads_level(&self, item: &TableRef<'_>) -> bool {
        fn reads(context: &Context<'_>, expression: &Expr<'_>) -> bool {
            let own = match expression {
                Expr::Column { qualifier, name } => context
                    .joined_source(*qualifier, name)
                    .is_some_and(|(source, _)| source.level == 0),
                Expr::WholeRow(name) => context
                    .joined
                    .iter()
                    .any(|joined| joined.level == 0 && joined.addressable && joined.name == *name),
                _ => false,
            };
            let mut found = own;
            if !found {
                let _ = crate::sql::query::walk_children(expression, &mut |child| {
                    found |= reads(context, child);
                    Ok(())
                });
            }
            found
        }
        let functions: &[TableRef<'_>] = match item.rows_from {
            Some(functions) => functions,
            None => core::slice::from_ref(item),
        };
        functions.iter().any(|function| {
            function
                .func_args
                .unwrap_or(&[])
                .iter()
                .any(|argument| reads(self.context, argument))
        })
    }

    fn column_aliases(&self, entry: &Joined<'_>, output: &mut Output<'_>) -> Result<(), SqlError> {
        output.push("(")?;
        for index in 0..entry.columns.len() {
            if index != 0 {
                output.push(", ")?;
            }
            push_name(output, entry.columns.name(index))?;
        }
        output.push(")")
    }

    fn item(&mut self, item: &'x TableRef<'x>, output: &mut Output<'_>) -> Result<(), SqlError> {
        if item.join_members.is_some() {
            let Some(Select {
                from: Some(inner), ..
            }) = item.subquery
            else {
                return output.unsupported();
            };
            let [(base, joins)] = segments(inner, self.arena)? else {
                return output.unsupported();
            };
            let aliased = item.alias.is_some();
            if aliased && self.writer.pretty {
                output.push("(")?;
            }
            self.segment(base, joins, output)?;
            if aliased && self.writer.pretty {
                output.push(")")?;
            }
            if aliased {
                let entry = self.take();
                output.push(" ")?;
                push_name(output, entry.printed)?;
                if item.col_alias.is_some() {
                    self.column_aliases(&entry, output)?;
                }
            }
            return Ok(());
        }
        let entry = self.take();
        // A function reading this level's items is implicitly LATERAL.
        if (item.lateral && (item.subquery.is_some() || item.is_function_source()))
            || (item.is_function_source() && self.reads_level(item))
        {
            output.push("LATERAL ")?;
        }
        if let Some(subquery) = item.subquery {
            output.push("(")?;
            let scope = outward(self.scope, self.arena)?;
            self.writer.query(
                scope,
                subquery,
                Naming {
                    names: None,
                    visible: true,
                    unknown: false,
                },
                true,
                self.arena,
                output,
            )?;
            output.push(") ")?;
            push_name(output, entry.printed)?;
            // A derived item's column list prints when it renamed columns.
            if item.col_alias.is_some_and(|aliases| !aliases.is_empty()) {
                self.column_aliases(&entry, output)?;
            }
            return Ok(());
        }
        if item.is_function_source() {
            match item.rows_from {
                Some(functions) => {
                    output.push("ROWS FROM(")?;
                    for (index, function) in functions.iter().enumerate() {
                        if index != 0 {
                            output.push(", ")?;
                        }
                        let call = function_call(function, self.arena)?;
                        self.context.write(call, None, output)?;
                    }
                    output.push(")")?;
                }
                None => {
                    let call = function_call(item, self.arena)?;
                    self.context.write(call, None, output)?;
                }
            }
            if item.with_ordinality {
                output.push(" WITH ORDINALITY")?;
            }
            output.push(" ")?;
            push_name(output, entry.printed)?;
            // A function's column list is always printed.
            return self.column_aliases(&entry, output);
        }
        if item.schema.is_none()
            && self.scope[self.next..]
                .iter()
                .any(|joined| joined.cte && joined.name == item.table)
        {
            push_name(output, item.table)?;
            if entry.printed != item.table {
                output.push(" ")?;
                push_name(output, entry.printed)?;
            }
            if item.col_alias.is_some() {
                self.column_aliases(&entry, output)?;
            }
            return Ok(());
        }
        let Some(relation) = (Builder {
            writer: self.writer,
            around: &[],
            entries: ArenaList::new(self.arena),
            arena: self.arena,
        })
        .relation(item)?
        else {
            return output.unsupported();
        };
        if item.inheritance == RelationInheritance::Only {
            output.push("ONLY ")?;
        }
        if !relation.schema_visible {
            push_name(output, relation.schema)?;
            output.push(".")?;
        }
        push_name(output, relation.name)?;
        if item.alias.is_some() || entry.printed != relation.name {
            output.push(" ")?;
            push_name(output, entry.printed)?;
        }
        if item.col_alias.is_some() {
            self.column_aliases(&entry, output)?;
        }
        if let Some(sample) = item.sample {
            output.push(match sample.method {
                TableSampleMethod::System => " TABLESAMPLE system (",
                TableSampleMethod::Bernoulli => " TABLESAMPLE bernoulli (",
            })?;
            self.context.write(sample.percentage, None, output)?;
            output.push(")")?;
            if let Some(seed) = sample.repeatable {
                output.push(" REPEATABLE (")?;
                self.context.write(seed, None, output)?;
                output.push(")")?;
            }
        }
        Ok(())
    }
}

/// The type of a `VALUES` column: the common type of its rows' values.
fn values_type(
    context: &Context<'_>,
    rows: &[&Select<'_>],
    column: usize,
) -> Result<Option<i32>, SqlError> {
    let mut found: Option<i32> = None;
    for row in rows {
        let Some(SelectItem::Expr { expression, .. }) = row.items.get(column) else {
            return Ok(None);
        };
        if matches!(expression, Expr::Str(_) | Expr::Null) {
            continue;
        }
        let Some(type_oid) = context.type_of(expression)? else {
            return Ok(None);
        };
        found = Some(match found {
            None => type_oid,
            Some(current) => match (super::numeric_rank(current), super::numeric_rank(type_oid)) {
                (Some(left), Some(right)) if right > left => type_oid,
                _ => current,
            },
        });
    }
    Ok(Some(found.unwrap_or(oid::TEXT)))
}

/// `get_values_def`.
fn values_list(
    context: &Context<'_>,
    rows: &[&Select<'_>],
    output: &mut Output<'_>,
) -> Result<(), SqlError> {
    let width = rows[0].items.len();
    output.push("VALUES ")?;
    for (index, row) in rows.iter().enumerate() {
        if row.items.len() != width {
            return output.unsupported();
        }
        output.push(if index == 0 { "(" } else { ", (" })?;
        for (column, item) in row.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else {
                return output.unsupported();
            };
            if column != 0 {
                output.push(",")?;
            }
            if matches!(expression, Expr::DefaultMarker) {
                output.push("DEFAULT")?;
            } else if matches!(expression, Expr::Str(_) | Expr::Null) {
                let type_oid = values_type(context, rows, column)?;
                context.write(expression, type_oid, output)?;
            } else {
                // `get_values_def` hides the implicit coercion to the
                // column's common type.
                context.write(expression, None, output)?;
            }
        }
        output.push(")")?;
    }
    Ok(())
}

/// `get_rule_sortgroupclause`.
fn sort_group(
    context: &Context<'_>,
    expression: &Expr<'_>,
    output: &mut Output<'_>,
) -> Result<(), SqlError> {
    match expression {
        Expr::Int(_) | Expr::Str(_) | Expr::Null | Expr::NumericLit(_) | Expr::Float(_) => {
            output.unsupported()
        }
        Expr::Column { .. } => context.write(expression, None, output),
        _ => {
            let paren = context.pretty
                || matches!(
                    expression,
                    Expr::Call {
                        kind: crate::sql::ast::CallKind::Routine,
                        ..
                    }
                );
            if paren {
                output.push("(")?;
            }
            let natural = context.type_of(expression)?;
            context.write(expression, natural, output)?;
            if paren {
                output.push(")")?;
            }
            Ok(())
        }
    }
}

/// `get_rule_orderby`'s direction: the default `<` ordering prints nothing.
fn order_direction(order: &OrderBy<'_>, output: &mut Output<'_>) -> Result<(), SqlError> {
    if order.descending {
        output.push(" DESC")?;
        if !order.nulls_first {
            output.push(" NULLS LAST")?;
        }
    } else if order.nulls_first {
        output.push(" NULLS FIRST")?;
    }
    Ok(())
}

/// `get_rule_groupingset`.
fn grouping_element(
    context: &Context<'_>,
    items: &[&Expr<'_>],
    element: &GroupingElement<'_>,
    omit_parens: bool,
    output: &mut Output<'_>,
) -> Result<(), SqlError> {
    let item = |index: u16| items.get(usize::from(index)).copied();
    let list = |members: &[u16], output: &mut Output<'_>| -> Result<(), SqlError> {
        let paren = !omit_parens || members.len() != 1;
        if paren {
            output.push("(")?;
        }
        for (position, member) in members.iter().enumerate() {
            if position != 0 {
                output.push(", ")?;
            }
            let Some(expression) = item(*member) else {
                return output.unsupported();
            };
            sort_group(context, expression, output)?;
        }
        if paren {
            output.push(")")?;
        }
        Ok(())
    };
    let (keyword, members, omit) = match element {
        GroupingElement::Expression(index) => return list(core::slice::from_ref(index), output),
        GroupingElement::List(members) => return list(members, output),
        GroupingElement::Rollup(members) => ("ROLLUP(", *members, true),
        GroupingElement::Cube(members) => ("CUBE(", *members, true),
        GroupingElement::Sets(members) => ("GROUPING SETS (", *members, false),
    };
    output.push(keyword)?;
    for (index, member) in members.iter().enumerate() {
        if index != 0 {
            output.push(", ")?;
        }
        grouping_element(context, items, member, omit, output)?;
    }
    output.push(")")
}

/// `get_rule_windowspec`.
pub(super) fn window_spec(
    context: &Context<'_>,
    window: &WindowSpec<'_>,
    output: &mut Output<'_>,
) -> Result<(), SqlError> {
    output.push("(")?;
    let mut space = false;
    if let Some(base) = window.base {
        push_name(output, base)?;
        space = true;
    }
    if !window.partition_by.is_empty() && window.base.is_none() {
        output.push("PARTITION BY ")?;
        for (index, expression) in window.partition_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            sort_group(context, expression, output)?;
        }
        space = true;
    }
    if !window.order_by.is_empty() && !window.copied_order {
        if space {
            output.push(" ")?;
        }
        output.push("ORDER BY ")?;
        for (index, order) in window.order_by.iter().enumerate() {
            if index != 0 {
                output.push(", ")?;
            }
            sort_group(context, order.expression, output)?;
            order_direction(order, output)?;
        }
        space = true;
    }
    if let Some(frame) = window.frame {
        if space {
            output.push(" ")?;
        }
        output.push(match frame.units {
            FrameUnits::Rows => "ROWS ",
            FrameUnits::Range => "RANGE ",
            FrameUnits::Groups => "GROUPS ",
        })?;
        if frame.between {
            output.push("BETWEEN ")?;
        }
        let bound = |bound: &FrameBound<'_>, output: &mut Output<'_>| -> Result<(), SqlError> {
            match bound {
                FrameBound::UnboundedPreceding => output.push("UNBOUNDED PRECEDING"),
                FrameBound::UnboundedFollowing => output.push("UNBOUNDED FOLLOWING"),
                FrameBound::CurrentRow => output.push("CURRENT ROW"),
                FrameBound::Preceding(offset) | FrameBound::Following(offset) => {
                    context.write(offset, None, output)?;
                    output.push(if matches!(bound, FrameBound::Preceding(_)) {
                        " PRECEDING"
                    } else {
                        " FOLLOWING"
                    })
                }
            }
        };
        bound(&frame.start, output)?;
        if frame.between {
            output.push(" AND ")?;
            bound(&frame.end, output)?;
        }
        output.push(match frame.exclusion {
            FrameExclusion::NoOthers => "",
            FrameExclusion::CurrentRow => " EXCLUDE CURRENT ROW",
            FrameExclusion::Group => " EXCLUDE GROUP",
            FrameExclusion::Ties => " EXCLUDE TIES",
        })?;
    }
    output.push(")")
}

/// A sublink's query (`get_sublink_expr`): printed under the scope of the
/// expression that holds it, its columns named only by their own names.
pub(super) fn sublink<'x>(
    context: &Context<'x>,
    select: &'x Select<'x>,
    output: &mut Output<'_>,
) -> Result<(), SqlError> {
    let arena = output.arena;
    let writer = Writer {
        storage: context.storage,
        txid: context.txid,
        pretty: context.pretty,
        dependencies: None,
        qualify: false,
    };
    // Expressions outside a query layout start the query at no indentation.
    let scope = outward(context.joined, arena)?;
    writer.query(
        scope,
        select,
        Naming {
            names: None,
            visible: false,
            unknown: false,
        },
        true,
        arena,
        output,
    )
}
