//! Name-resolution pass for durable stored-query dependencies.
//!
//! Views keep their SQL text for execution, but dependency decisions must not
//! reparse that text after catalog objects have moved. This pass runs once at
//! CREATE time and records the exact relation, user type, and sequence slots
//! selected under the creator's search path.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    Expr, FrameBound, FromClause, Join, JoinKind, QualName, RelationInheritance, Select,
    SelectItem, SetQuery, SetTree, Stmt, TableRef,
};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::exec::{ColTypeResolver, StaticTypeMeta};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    DependencyClass, MAX_COLUMNS, MAX_STORED_QUERY_DEPENDENCIES, PathContext, PathEntry,
    ResolvedRelation, SqlName, Storage, StoredDependencyIdentity, StoredQueryDependencies,
    StoredQueryDependency,
};

#[derive(Clone, Copy)]
struct CollectionContext<'a> {
    path: &'a PathContext,
    transition: Option<&'a dyn ColTypeResolver>,
}

impl<'a> CollectionContext<'a> {
    fn ordinary(path: &'a PathContext) -> Self {
        Self {
            path,
            transition: None,
        }
    }

    fn rule(path: &'a PathContext, transition: &'a dyn ColTypeResolver) -> Self {
        Self {
            path,
            transition: Some(transition),
        }
    }

    fn transition_column(self, qualifier: Option<&str>, name: &str) -> Result<bool, SqlError> {
        let Some(transition) = self.transition else {
            return Ok(false);
        };
        if !qualifier.is_some_and(|qualifier| {
            qualifier.eq_ignore_ascii_case("old") || qualifier.eq_ignore_ascii_case("new")
        }) {
            return Ok(false);
        }
        transition.resolve(qualifier, name).map(|_| true)
    }
}

#[derive(Clone, Copy)]
struct CteNames<'a> {
    names: [&'a str; MAX_STORED_QUERY_DEPENDENCIES],
    len: usize,
}

impl<'a> CteNames<'a> {
    const EMPTY: Self = Self {
        names: [""; MAX_STORED_QUERY_DEPENDENCIES],
        len: 0,
    };

    fn contains(&self, name: &str) -> bool {
        self.names[..self.len].contains(&name)
    }

    fn push(&mut self, name: &'a str) -> Result<(), SqlError> {
        if self.len == self.names.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query nests more than {} CTE names",
                self.names.len()
            ));
        }
        self.names[self.len] = name;
        self.len += 1;
        Ok(())
    }
}

pub(super) fn collect(
    sql: &str,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
) -> Result<StoredQueryDependencies, SqlError> {
    let select = crate::sql::parser::parse_query(sql, arena)?;
    let mut dependencies = StoredQueryDependencies::EMPTY;
    collect_select(
        select,
        storage,
        txid,
        CteNames::EMPTY,
        &mut dependencies,
        arena,
        CollectionContext::ordinary(&path),
    )?;
    Ok(dependencies)
}

pub(super) fn collect_routine_program(
    program: &super::RoutineFunctionProgram<'_>,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
) -> Result<StoredQueryDependencies, SqlError> {
    let mut dependencies = StoredQueryDependencies::EMPTY;
    for step in program.preceding {
        if let super::RoutinePrelude::Statement(statement) = step {
            collect_statement(
                statement,
                storage,
                txid,
                &mut dependencies,
                arena,
                CollectionContext::ordinary(&path),
            )?;
        }
    }
    match program.result {
        super::RoutineFunctionResult::Query(super::RoutineQuery::Select(select)) => collect_select(
            select,
            storage,
            txid,
            CteNames::EMPTY,
            &mut dependencies,
            arena,
            CollectionContext::ordinary(&path),
        )?,
        super::RoutineFunctionResult::Query(super::RoutineQuery::Set(query)) => collect_set_query(
            query,
            storage,
            txid,
            &mut dependencies,
            arena,
            CollectionContext::ordinary(&path),
        )?,
        super::RoutineFunctionResult::DataModification(statement)
        | super::RoutineFunctionResult::Void(statement) => collect_statement(
            statement,
            storage,
            txid,
            &mut dependencies,
            arena,
            CollectionContext::ordinary(&path),
        )?,
        super::RoutineFunctionResult::Forbidden(_) => {}
    }
    Ok(dependencies)
}

pub(super) fn collect_rule_actions(
    actions: &[crate::sql::ast::RuleAction<'_>],
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
    transition: &dyn ColTypeResolver,
) -> Result<StoredQueryDependencies, SqlError> {
    let mut dependencies = StoredQueryDependencies::EMPTY;
    for action in actions {
        collect_statement(
            action.statement,
            storage,
            txid,
            &mut dependencies,
            arena,
            CollectionContext::rule(&path, transition),
        )?;
    }
    Ok(dependencies)
}

pub(super) fn collect_dml_input(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
) -> Result<(StoredQueryDependencies, StoredQueryDependencies), SqlError> {
    let context = CollectionContext::ordinary(&path);
    let mut dependencies = StoredQueryDependencies::EMPTY;
    collect_statement(statement, storage, txid, &mut dependencies, arena, context)?;

    let mut sources = StoredQueryDependencies::EMPTY;
    match statement {
        Stmt::Insert(insert) => {
            if let Some(select) = insert.select {
                collect_select(
                    select,
                    storage,
                    txid,
                    CteNames::EMPTY,
                    &mut sources,
                    arena,
                    context,
                )?;
            }
        }
        Stmt::Update(update) => {
            if let Some(from) = update.from {
                collect_from_sources(from, storage, txid, &mut sources, arena, context)?;
            }
        }
        Stmt::Delete(delete) => {
            if let Some(using) = delete.using {
                collect_from_sources(using, storage, txid, &mut sources, arena, context)?;
            }
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "rewrite input is not data-modifying"
            ));
        }
    }
    Ok((dependencies, sources))
}

fn collect_from_sources(
    from: &FromClause<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    collect_table_ref(
        &from.base,
        storage,
        txid,
        CteNames::EMPTY,
        dependencies,
        arena,
        context,
    )?;
    for join in from.joins {
        collect_table_ref(
            &join.table,
            storage,
            txid,
            CteNames::EMPTY,
            dependencies,
            arena,
            context,
        )?;
    }
    Ok(())
}

fn collect_set_query(
    query: &SetQuery<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let mut ctes = CteNames::EMPTY;
    for cte in query.with {
        collect_select(cte.query, storage, txid, ctes, dependencies, arena, context)?;
        ctes.push(cte.name)?;
    }
    collect_set_tree(
        query.body,
        storage,
        txid,
        ctes,
        dependencies,
        arena,
        context,
    )?;
    for expression in query
        .order_by
        .iter()
        .map(|order| order.expression)
        .chain(query.limit)
        .chain(query.offset)
    {
        collect_expression(
            expression,
            storage,
            txid,
            ctes,
            dependencies,
            arena,
            context,
        )?;
    }
    Ok(())
}

fn record_routine_target(
    name: crate::sql::ast::QualName<'_>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let resolved = storage.resolve_relation_under(path, name.schema, name.name, txid);
    let Some((class, slot, schema, catalog_name)) = (match resolved {
        Some(ResolvedRelation::Table(slot)) => {
            let definition = storage.table_def(slot, txid);
            Some((
                DependencyClass::Table,
                slot,
                definition.schema,
                definition.name,
            ))
        }
        Some(ResolvedRelation::View(slot)) => {
            let definition = storage.view(slot);
            Some((
                DependencyClass::View,
                slot,
                definition.schema,
                definition.name,
            ))
        }
        Some(ResolvedRelation::Catalog) | None => None,
    }) else {
        return Ok(());
    };
    dependencies.push(StoredQueryDependency {
        class,
        slot: slot as u16,
        identity: StoredDependencyIdentity::Name,
        referenced_columns: 0,
        schema,
        name: catalog_name,
        referenced_schema: SqlName::parse(name.schema.unwrap_or(""))?,
        referenced_name: SqlName::parse(name.name)?,
    })
}

fn collect_statement(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let path = context.path;
    let no_excluded: Option<(usize, &crate::storage::TableDef)> = None;
    macro_rules! collect_expr {
        ($expression:expr, $scope:expr, $excluded:expr) => {{
            collect_expression(
                $expression,
                storage,
                txid,
                CteNames::EMPTY,
                dependencies,
                arena,
                context,
            )?;
            if let Some(scope) = $scope {
                record_dml_column_references($expression, scope, $excluded, dependencies, context)?;
            }
            collect_dml_routine_calls(
                $expression,
                storage,
                txid,
                $scope,
                $excluded.map(|(_, definition)| definition),
                dependencies,
                context,
            )
        }};
    }
    match statement {
        Stmt::Select(select) => collect_select(
            select,
            storage,
            txid,
            CteNames::EMPTY,
            dependencies,
            arena,
            context,
        ),
        Stmt::SetQuery(query) => {
            collect_set_query(query, storage, txid, dependencies, arena, context)
        }
        Stmt::With { ctes, statement } => {
            for cte in *ctes {
                collect_select(
                    cte.query,
                    storage,
                    txid,
                    CteNames::EMPTY,
                    dependencies,
                    arena,
                    context,
                )?;
            }
            collect_statement(statement, storage, txid, dependencies, arena, context)
        }
        Stmt::Insert(insert) => {
            let mark = arena.mark();
            let result = (|| {
                record_routine_target(insert.table, storage, txid, path, dependencies)?;
                let scope = dml_dependency_scope(storage, insert.table, None, None, txid, arena)?;
                let excluded = storage
                    .resolve_relation_under(path, insert.table.schema, insert.table.name, txid)
                    .and_then(|relation| match relation {
                        ResolvedRelation::Table(slot) => {
                            Some((slot, storage.table_def(slot, txid)))
                        }
                        _ => None,
                    });
                for row in insert.rows {
                    for expression in *row {
                        collect_expr!(expression, None, no_excluded)?;
                    }
                }
                if let Some(select) = insert.select {
                    collect_select(
                        select,
                        storage,
                        txid,
                        CteNames::EMPTY,
                        dependencies,
                        arena,
                        context,
                    )?;
                }
                if let Some(conflict) = insert.on_conflict {
                    for target in conflict.target {
                        collect_expr!(target.expression, Some(&scope), no_excluded)?;
                    }
                    for (_, expression) in conflict.update.into_iter().flatten() {
                        collect_expr!(expression, Some(&scope), excluded)?;
                    }
                    if let Some(expression) = conflict.update_where {
                        collect_expr!(expression, Some(&scope), excluded)?;
                    }
                }
                for item in insert.returning {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_expr!(expression, Some(&scope), no_excluded)?;
                    }
                }
                Ok(())
            })();
            unsafe { arena.rewind_to(mark) };
            result
        }
        Stmt::Update(update) => {
            let mark = arena.mark();
            let result = (|| {
                record_routine_target(update.table, storage, txid, path, dependencies)?;
                let scope = dml_dependency_scope(
                    storage,
                    update.table,
                    update.alias,
                    update.from,
                    txid,
                    arena,
                )?;
                if let Some(from) = update.from {
                    collect_table_ref(
                        &from.base,
                        storage,
                        txid,
                        CteNames::EMPTY,
                        dependencies,
                        arena,
                        context,
                    )?;
                    for join in from.joins {
                        collect_table_ref(
                            &join.table,
                            storage,
                            txid,
                            CteNames::EMPTY,
                            dependencies,
                            arena,
                            context,
                        )?;
                        if let Some(expression) = join.on {
                            collect_expr!(expression, Some(&scope), no_excluded)?;
                        }
                    }
                }
                for (_, expression) in update.assignments {
                    collect_expr!(expression, Some(&scope), no_excluded)?;
                }
                if let Some(expression) = update.where_clause {
                    collect_expr!(expression, Some(&scope), no_excluded)?;
                }
                for item in update.returning {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_expr!(expression, Some(&scope), no_excluded)?;
                    }
                }
                Ok(())
            })();
            unsafe { arena.rewind_to(mark) };
            result
        }
        Stmt::Delete(delete) => {
            let mark = arena.mark();
            let result = (|| {
                record_routine_target(delete.table, storage, txid, path, dependencies)?;
                let scope = dml_dependency_scope(
                    storage,
                    delete.table,
                    delete.alias,
                    delete.using,
                    txid,
                    arena,
                )?;
                if let Some(using) = delete.using {
                    collect_table_ref(
                        &using.base,
                        storage,
                        txid,
                        CteNames::EMPTY,
                        dependencies,
                        arena,
                        context,
                    )?;
                    for join in using.joins {
                        collect_table_ref(
                            &join.table,
                            storage,
                            txid,
                            CteNames::EMPTY,
                            dependencies,
                            arena,
                            context,
                        )?;
                        if let Some(expression) = join.on {
                            collect_expr!(expression, Some(&scope), no_excluded)?;
                        }
                    }
                }
                if let Some(expression) = delete.where_clause {
                    collect_expr!(expression, Some(&scope), no_excluded)?;
                }
                for item in delete.returning {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_expr!(expression, Some(&scope), no_excluded)?;
                    }
                }
                Ok(())
            })();
            unsafe { arena.rewind_to(mark) };
            result
        }
        Stmt::Merge(merge) => {
            let mark = arena.mark();
            let result = (|| {
                record_routine_target(merge.target, storage, txid, path, dependencies)?;
                collect_table_ref(
                    &merge.source,
                    storage,
                    txid,
                    CteNames::EMPTY,
                    dependencies,
                    arena,
                    context,
                )?;
                let source = arena
                    .alloc(FromClause {
                        base: merge.source,
                        joins: &[],
                    })
                    .map_err(|_| super::arena_full())?;
                let scope = dml_dependency_scope(
                    storage,
                    merge.target,
                    merge.target_alias,
                    Some(source),
                    txid,
                    arena,
                )?;
                collect_expr!(merge.on, Some(&scope), no_excluded)?;
                for when in merge.whens {
                    if let Some(condition) = when.condition() {
                        collect_expr!(condition, Some(&scope), no_excluded)?;
                    }
                    match when.action() {
                        crate::sql::ast::MergeActionRef::Update(assignments) => {
                            for (_, expression) in assignments {
                                collect_expr!(expression, Some(&scope), no_excluded)?;
                            }
                        }
                        crate::sql::ast::MergeActionRef::Insert { values, .. } => {
                            for expression in values {
                                collect_expr!(expression, Some(&scope), no_excluded)?;
                            }
                        }
                        crate::sql::ast::MergeActionRef::Delete
                        | crate::sql::ast::MergeActionRef::DoNothing => {}
                    }
                }
                Ok(())
            })();
            unsafe { arena.rewind_to(mark) };
            result
        }
        _ => Ok(()),
    }
}

fn collect_dml_routine_calls(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
    scope: Option<&super::QueryScope<'_>>,
    excluded: Option<&crate::storage::TableDef>,
    dependencies: &mut StoredQueryDependencies,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let resolver = DependencyTypes {
        scope,
        excluded,
        storage,
        txid,
        transition: context.transition,
    };
    let mut needs_scope = false;
    fn visit(
        expression: &Expr<'_>,
        storage: &Storage,
        txid: u32,
        resolver: &DependencyTypes<'_, '_, '_>,
        dependencies: &mut StoredQueryDependencies,
        needs_scope: &mut bool,
    ) -> Result<(), SqlError> {
        if let Expr::Call {
            name,
            args,
            argument_names,
            variadic,
            ..
        } = expression
        {
            record_routine_call(
                name,
                args,
                argument_names,
                *variadic,
                storage,
                txid,
                resolver,
                dependencies,
                needs_scope,
            )?;
        }
        super::walk_children(expression, &mut |child| {
            visit(child, storage, txid, resolver, dependencies, needs_scope)
        })
    }
    visit(
        expression,
        storage,
        txid,
        &resolver,
        dependencies,
        &mut needs_scope,
    )?;
    if needs_scope {
        return Err(sql_err!(
            sqlstate::INVALID_FUNCTION_DEFINITION,
            "routine call in a data-modifying SQL body needs an unresolved column type"
        ));
    }
    Ok(())
}

fn record_dml_column_references(
    expression: &Expr<'_>,
    scope: &super::QueryScope<'_>,
    excluded: Option<(usize, &crate::storage::TableDef)>,
    dependencies: &mut StoredQueryDependencies,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let mut failure = None;
    expression.for_each_column_reference(&mut |qualifier, name| {
        if failure.is_some() {
            return;
        }
        match context.transition_column(qualifier, name) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                failure = Some(error);
                return;
            }
        }
        if qualifier == Some("excluded")
            && let Some((slot, definition)) = excluded
        {
            let Some(column) = definition.column_index(name) else {
                failure = Some(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
                return;
            };
            if let Err(error) =
                dependencies.mark_referenced_column(DependencyClass::Table, slot, column)
            {
                failure = Some(error);
            }
            return;
        }
        let resolved = match scope.find_column(qualifier, name) {
            Ok(resolved) => resolved,
            Err(error) => {
                failure = Some(error);
                return;
            }
        };
        if let Err(error) = mark_resolved_column(scope, resolved, dependencies) {
            failure = Some(error);
        }
    });
    failure.map_or(Ok(()), Err)
}

fn dml_dependency_scope<'a>(
    storage: &'a Storage,
    target: QualName<'a>,
    alias: Option<&'a str>,
    extra: Option<&'a FromClause<'a>>,
    txid: u32,
    arena: &'a Arena,
) -> Result<super::QueryScope<'a>, SqlError> {
    let base = TableRef {
        schema: target.schema,
        table: target.name,
        alias,
        subquery: None,
        func_args: None,
        func_argument_names: &[],
        func_variadic: false,
        rows_from: None,
        col_alias: None,
        inheritance: RelationInheritance::Descendants,
        sample: None,
        cte: None,
        with_ordinality: false,
        lateral: false,
        authorization_role: None,
    };
    let joins = match extra {
        None => &[][..],
        Some(extra) => {
            let joins = arena
                .alloc_slice_with(extra.joins.len() + 1, |index| {
                    if index == 0 {
                        Join {
                            table: extra.base,
                            kind: JoinKind::Cross,
                            on: None,
                            using: None,
                            natural: false,
                        }
                    } else {
                        extra.joins[index - 1]
                    }
                })
                .map_err(|_| super::arena_full())?;
            &*joins
        }
    };
    let from = arena
        .alloc(FromClause { base, joins })
        .map_err(|_| super::arena_full())?;
    super::QueryScope::resolve_schema(storage, from, txid, arena)
}

fn collect_select<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    inherited_ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let path = context.path;
    let mut visible_ctes = inherited_ctes;
    for cte in select.with {
        // A CTE can reference earlier entries, not itself unless recursive.
        let definition_scope = if cte.recursive {
            let mut scope = visible_ctes;
            scope.push(cte.name)?;
            scope
        } else {
            visible_ctes
        };
        collect_select(
            cte.query,
            storage,
            txid,
            definition_scope,
            dependencies,
            arena,
            context,
        )?;
        visible_ctes.push(cte.name)?;
    }

    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                collect_expression(
                    expression,
                    storage,
                    txid,
                    visible_ctes,
                    dependencies,
                    arena,
                    context,
                )?;
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {}
        }
    }
    for expression in select.distinct_on {
        collect_expression(
            expression,
            storage,
            txid,
            visible_ctes,
            dependencies,
            arena,
            context,
        )?;
    }
    for expression in select
        .where_clause
        .into_iter()
        .chain(select.group_by.iter().copied())
        .chain(select.having)
        .chain(select.order_by.iter().map(|order| order.expression))
        .chain(select.limit)
        .chain(select.offset)
    {
        collect_expression(
            expression,
            storage,
            txid,
            visible_ctes,
            dependencies,
            arena,
            context,
        )?;
    }
    if let Some(tree) = select.set_body {
        collect_set_tree(
            tree,
            storage,
            txid,
            visible_ctes,
            dependencies,
            arena,
            context,
        )?;
    }
    if let Some(from) = select.from {
        collect_table_ref(
            &from.base,
            storage,
            txid,
            visible_ctes,
            dependencies,
            arena,
            context,
        )?;
        for join in from.joins {
            collect_table_ref(
                &join.table,
                storage,
                txid,
                visible_ctes,
                dependencies,
                arena,
                context,
            )?;
            if let Some(on) = join.on {
                collect_expression(
                    on,
                    storage,
                    txid,
                    visible_ctes,
                    dependencies,
                    arena,
                    context,
                )?;
            }
        }
        let sources = core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
        if sources.clone().all(|source| {
            source.subquery.is_none()
                && !source.is_function_source()
                && matches!(
                    storage.resolve_relation_under(path, source.schema, source.table, txid),
                    Some(ResolvedRelation::Table(_))
                )
        }) {
            let scope = super::QueryScope::resolve_schema(storage, &from, txid, arena)?;
            for item in select.items {
                match item {
                    SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                        record_column_references(expression, &scope, dependencies, context)?;
                    }
                    SelectItem::Wildcard => {
                        for table in 0..scope.n {
                            for column in 0..scope.defs[table].expect("resolved").n_columns {
                                dependencies.mark_referenced_column(
                                    DependencyClass::Table,
                                    scope.slots[table],
                                    column,
                                )?;
                            }
                        }
                    }
                    SelectItem::TableWildcard(name) => {
                        for index in 0..scope.qualified_star_columns(name)? {
                            mark_resolved_column(
                                &scope,
                                scope.qualified_star_entry(name, index)?,
                                dependencies,
                            )?;
                        }
                    }
                }
            }
            for expression in select
                .distinct_on
                .iter()
                .copied()
                .chain(select.where_clause)
                .chain(select.group_by.iter().copied())
                .chain(select.having)
                .chain(select.order_by.iter().map(|order| order.expression))
                .chain(select.limit)
                .chain(select.offset)
                .chain(from.joins.iter().filter_map(|join| join.on))
            {
                record_column_references(expression, &scope, dependencies, context)?;
            }
        }
        record_relation_column_references(storage, txid, path, &from, select, dependencies, arena)?;
    }
    collect_routine_dependencies(select, storage, txid, dependencies, arena, context)?;
    Ok(())
}

struct DependencyTypes<'scope, 'definition, 'storage> {
    scope: Option<&'scope super::QueryScope<'definition>>,
    excluded: Option<&'storage crate::storage::TableDef>,
    transition: Option<&'scope dyn ColTypeResolver>,
    storage: &'storage Storage,
    txid: u32,
}

impl ColTypeResolver for DependencyTypes<'_, '_, '_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        if qualifier.is_some_and(|qualifier| {
            qualifier.eq_ignore_ascii_case("old") || qualifier.eq_ignore_ascii_case("new")
        }) && let Some(transition) = self.transition
        {
            return transition.resolve(qualifier, name);
        }
        if qualifier == Some("excluded")
            && let Some(column) = self.excluded.and_then(|definition| {
                definition
                    .columns()
                    .iter()
                    .find(|column| column.name.as_str() == name)
            })
        {
            return Ok(column.ctype);
        }
        match self.scope {
            Some(scope) => super::ScopeCols(scope).resolve(qualifier, name),
            None => crate::sql::exec::NoCols.resolve(qualifier, name),
        }
    }

    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        if qualifier.is_some_and(|qualifier| {
            qualifier.eq_ignore_ascii_case("old") || qualifier.eq_ignore_ascii_case("new")
        }) && let Some(transition) = self.transition
        {
            return transition.column_meta(qualifier, name);
        }
        if qualifier == Some("excluded") {
            let column = self
                .excluded?
                .columns()
                .iter()
                .find(|column| column.name.as_str() == name)?;
            return Some(StaticTypeMeta {
                ctype: column.ctype,
                type_oid: self.storage.routine_type_oid(
                    column.ctype,
                    column.user_type,
                    self.txid,
                )?,
                type_mod: column.type_mod,
                collation: column.collation,
            });
        }
        let scope = self.scope?;
        ColTypeResolver::column_meta(
            &super::CatalogScopeCols {
                scope,
                outer_scope: None,
                storage: self.storage,
                txid: self.txid,
            },
            qualifier,
            name,
        )
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        crate::sql::catalog::user_type_oid(self.storage, self.txid, type_name)
            .or_else(|| ColType::from_sql_name(type_name).map(ColType::oid))
    }

    fn routine_result(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<StaticTypeMeta> {
        let routine = if argument_names.is_empty() {
            self.storage
                .function_for_call_syntax_oids(name, arguments, variadic, self.txid)?
        } else {
            self.storage
                .function_for_named_call_oids(name, argument_names, arguments, self.txid)?
        };
        let ctype = routine.kind.function_result()?;
        Some(StaticTypeMeta {
            type_oid: self
                .storage
                .routine_function_result_oid(&routine, self.txid)?,
            ctype,
            type_mod: -1,
            collation: if ctype.is_collatable() {
                crate::sql::ast::Collation::Default
            } else {
                crate::sql::ast::Collation::None
            },
        })
    }

    fn routine_record_field(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        let slot = if argument_names.is_empty() {
            if variadic {
                self.storage
                    .routine_slot_for_function_call_syntax_oids(name, arguments, true, self.txid)?
            } else {
                self.storage
                    .routine_slot_for_table_call_oids(name, arguments, self.txid)?
            }
        } else {
            self.storage.routine_slot_for_named_function_call_oids(
                name,
                argument_names,
                arguments,
                self.txid,
            )?
        };
        let routine = self.storage.routine_for(slot, self.txid);
        let column = routine.record_result_columns()?.get(index)?;
        Some((
            crate::util::StackStr::from_str(column.name.as_str()),
            StaticTypeMeta {
                ctype: column.ctype,
                type_oid: self.storage.routine_type_oid(
                    column.ctype,
                    column.user_type,
                    self.txid,
                )?,
                type_mod: -1,
                collation: if column.ctype.is_collatable() {
                    crate::sql::ast::Collation::Default
                } else {
                    crate::sql::ast::Collation::None
                },
            },
        ))
    }

    fn named_composite_field(
        &self,
        type_name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        let slot = self.storage.resolve_composite_slot(type_name, self.txid)?;
        let definition = self.storage.composite_for(slot, self.txid);
        let field = definition.active_field(index)?;
        Some((
            crate::util::StackStr::from_str(field.name.as_str()),
            StaticTypeMeta {
                ctype: field.ctype,
                type_oid: self
                    .storage
                    .routine_type_oid(field.ctype, field.user_type, self.txid)?,
                type_mod: field.type_mod,
                collation: field.collation,
            },
        ))
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.transition
            .is_some_and(|transition| transition.is_whole_row(name))
            || self
                .scope
                .is_some_and(|scope| scope.qualified_star_columns(name).is_ok())
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.transition
            .and_then(|transition| transition.whole_row_scalar_type(name))
            .or_else(|| self.scope.and_then(|scope| scope.func_scalar_type(name)))
    }

    fn table_columns(&self, name: &str) -> Option<&[crate::storage::ColumnMeta]> {
        if let Some(columns) = self
            .transition
            .and_then(|transition| transition.table_columns(name))
        {
            return Some(columns);
        }
        let scope = self.scope?;
        let table = scope.table_index(name).ok()?;
        Some(scope.defs[table]?.columns())
    }

    fn whole_row_field(
        &self,
        name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        self.transition
            .and_then(|transition| transition.whole_row_field(name, index))
            .or_else(|| super::ScopeCols(self.scope?).whole_row_field(name, index))
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        if let Some(handle) = self
            .transition
            .and_then(|transition| transition.record_column_handle(qualifier, name))
        {
            return Some(handle);
        }
        let scope = self.scope?;
        let entry = scope.find_column(qualifier, name).ok()?;
        if scope.output_type(entry) != ColType::Record {
            return None;
        }
        match entry {
            super::scope::ResolvedColumn::Table(table, column) => {
                Some(scope.defs[table]?.columns()[column].type_mod)
            }
            _ => None,
        }
    }
}

fn collect_routine_dependencies(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let resolver = DependencyTypes {
        scope: None,
        excluded: None,
        transition: context.transition,
        storage,
        txid,
    };
    let needs_scope =
        collect_routine_dependencies_with_resolver(select, storage, txid, dependencies, &resolver)?;
    if needs_scope {
        let from = select.from.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored routine dependency did not resolve at its typed boundary"
            )
        })?;
        let mark = arena.mark();
        let result = (|| {
            let scope = super::QueryScope::resolve_schema(storage, from, txid, arena)?;
            let resolver = DependencyTypes {
                scope: Some(&scope),
                excluded: None,
                transition: context.transition,
                storage,
                txid,
            };
            if collect_routine_dependencies_with_resolver(
                select,
                storage,
                txid,
                dependencies,
                &resolver,
            )? {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "stored routine dependency remained unresolved after scope binding"
                ));
            }
            Ok(())
        })();
        // The dependency set owns catalog identities; the temporary scope does
        // not cross this typed boundary.
        unsafe { arena.rewind_to(mark) };
        result?;
    }
    Ok(())
}

pub(super) fn stored_routine_dependency_for_call(
    name: &str,
    args: &[&Expr<'_>],
    argument_names: &[Option<&str>],
    variadic: bool,
    storage: &Storage,
    txid: u32,
    dependencies: &StoredQueryDependencies,
) -> Result<Option<StoredQueryDependency>, SqlError> {
    let (referenced_schema, referenced_name) = name.split_once('.').unwrap_or(("", name));
    let mut candidates = dependencies.entries().iter().copied().filter(|dependency| {
        dependency.class == DependencyClass::Routine
            && dependency.referenced_schema.as_str() == referenced_schema
            && dependency.referenced_name.as_str() == referenced_name
    });
    let Some(first) = candidates.next() else {
        return Ok(None);
    };
    let Some(second) = candidates.next() else {
        return Ok(Some(first));
    };
    if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "routine call has too many arguments"
        ));
    }
    let resolver = DependencyTypes {
        scope: None,
        excluded: None,
        transition: None,
        storage,
        txid,
    };
    let mut argument_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
    for (output, argument) in argument_oids.iter_mut().zip(args.iter().copied()) {
        *output = match crate::sql::exec::infer_routine_argument_oid(argument, &resolver) {
            Ok(crate::sql::types::oid::UNKNOWN) if matches!(argument, Expr::Str(_)) => {
                ColType::Text.oid()
            }
            Ok(oid) => oid,
            Err(_) => {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "stored overloaded routine call \"{}\" cannot be rebound without a typed argument",
                    name
                ));
            }
        };
    }
    let argument_oids = &argument_oids[..args.len()];
    for dependency in core::iter::once(first)
        .chain(core::iter::once(second))
        .chain(candidates)
    {
        let routine = storage.routine_for(dependency.slot as usize, txid);
        let qualified = crate::stack_format!(
            192,
            "{}.{}",
            routine.schema_for(txid).as_str(),
            routine.name_for(txid).as_str()
        );
        let resolved = if argument_names.is_empty() {
            storage.routine_slot_for_function_call_syntax_oids(
                qualified.as_str(),
                argument_oids,
                variadic,
                txid,
            )
        } else {
            storage.routine_slot_for_named_function_call_oids(
                qualified.as_str(),
                argument_names,
                argument_oids,
                txid,
            )
        };
        if resolved == Some(dependency.slot as usize) {
            return Ok(Some(dependency));
        }
    }
    Err(sql_err!(
        sqlstate::INVALID_FUNCTION_DEFINITION,
        "stored overloaded routine call \"{}\" no longer has its captured signature",
        name
    ))
}

#[expect(
    clippy::too_many_arguments,
    reason = "routine dependency binding carries call syntax and catalog context"
)]
fn record_routine_call(
    name: &str,
    args: &[&Expr<'_>],
    argument_names: &[Option<&str>],
    variadic: bool,
    storage: &Storage,
    txid: u32,
    resolver: &DependencyTypes<'_, '_, '_>,
    dependencies: &mut StoredQueryDependencies,
    needs_scope: &mut bool,
) -> Result<(), SqlError> {
    if !storage.has_function_routine_candidate(name, args.len(), txid) {
        return Ok(());
    }
    if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "routine call has too many arguments"
        ));
    }
    let mut argument_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
    for (slot, argument) in argument_oids.iter_mut().zip(args.iter().copied()) {
        match crate::sql::exec::infer_routine_argument_oid(argument, resolver) {
            Ok(crate::sql::types::oid::UNKNOWN) if matches!(argument, Expr::Str(_)) => {
                *slot = ColType::Text.oid();
            }
            Ok(oid) => *slot = oid,
            Err(_) => {
                *needs_scope = true;
                return Ok(());
            }
        }
    }
    let argument_oids = &argument_oids[..args.len()];
    let resolved = if argument_names.is_empty() {
        storage.routine_slot_for_function_call_syntax_oids(name, argument_oids, variadic, txid)
    } else {
        storage.routine_slot_for_named_function_call_oids(name, argument_names, argument_oids, txid)
    };
    let Some(slot) = resolved else {
        return Ok(());
    };
    let routine = storage.routine_for(slot, txid);
    let (referenced_schema, referenced_name) = name
        .split_once('.')
        .map_or(("", name), |(schema, name)| (schema, name));
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::Routine,
        slot: slot as u16,
        identity: StoredDependencyIdentity::RoutineOid(crate::storage::routine_oid(&routine)),
        referenced_columns: 0,
        schema: routine.schema_for(txid),
        name: routine.name_for(txid),
        referenced_schema: SqlName::parse(referenced_schema)?,
        referenced_name: SqlName::parse(referenced_name)?,
    })
}

fn record_operator_call(
    identity: crate::sql::ast::QualName<'_>,
    operands: &[&Expr<'_>],
    storage: &Storage,
    txid: u32,
    resolver: &DependencyTypes<'_, '_, '_>,
    dependencies: &mut StoredQueryDependencies,
    needs_scope: &mut bool,
) -> Result<(), SqlError> {
    let crate::sql::ast::QualName { schema, name } = identity;
    let has_catalog_candidate = storage.operators_visible_to(txid).any(|(_, operator)| {
        operator.name.as_str() == name
            && schema.map_or_else(
                || storage.schema_is_on_path(operator.schema),
                |schema| operator.schema.as_str() == schema,
            )
    });
    if !has_catalog_candidate {
        return Ok(());
    }
    let infer = |expression: &Expr<'_>| {
        crate::sql::exec::infer_routine_argument_oid(expression, resolver).map(|oid| {
            if oid == crate::sql::types::oid::UNKNOWN && matches!(expression, Expr::Str(_)) {
                ColType::Text.oid()
            } else {
                oid
            }
        })
    };
    if !(1..=2).contains(&operands.len()) {
        return Err(sql_err!(sqlstate::INTERNAL_ERROR, "invalid operator arity"));
    }
    let mut inferred = [0; 2];
    for (slot, operand) in inferred.iter_mut().zip(operands.iter().copied()) {
        let Ok(oid) = infer(operand) else {
            *needs_scope = true;
            return Ok(());
        };
        *slot = oid;
    }
    let (left_oid, right_oid) = if operands.len() == 1 {
        (None, Some(inferred[0]))
    } else {
        (Some(inferred[0]), Some(inferred[1]))
    };
    let Some(slot) = storage.operator_slot_for_oids(schema, name, left_oid, right_oid, txid)?
    else {
        return Ok(());
    };
    let operator = storage.operator_for(slot, txid);
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::Operator,
        slot: slot as u16,
        identity: StoredDependencyIdentity::OperatorOid(storage.operator(slot).oid()),
        referenced_columns: 0,
        schema: operator.schema,
        name: operator.name,
        referenced_schema: SqlName::parse(schema.unwrap_or(""))?,
        referenced_name: SqlName::parse(name)?,
    })
}

fn collect_routine_dependencies_with_resolver(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    resolver: &DependencyTypes<'_, '_, '_>,
) -> Result<bool, SqlError> {
    let mut needs_scope = false;

    fn visit_expr(
        expression: &Expr<'_>,
        storage: &Storage,
        txid: u32,
        resolver: &DependencyTypes<'_, '_, '_>,
        dependencies: &mut StoredQueryDependencies,
        needs_scope: &mut bool,
    ) -> Result<(), SqlError> {
        if let Expr::Call {
            name,
            args,
            argument_names,
            variadic,
            ..
        } = expression
        {
            if let Some((schema, operator)) = crate::sql::ast::catalog_operator_call(name) {
                if (1..=2).contains(&args.len())
                    && !schema.is_some_and(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
                {
                    record_operator_call(
                        crate::sql::ast::QualName {
                            schema,
                            name: operator,
                        },
                        args,
                        storage,
                        txid,
                        resolver,
                        dependencies,
                        needs_scope,
                    )?;
                }
            } else {
                record_routine_call(
                    name,
                    args,
                    argument_names,
                    *variadic,
                    storage,
                    txid,
                    resolver,
                    dependencies,
                    needs_scope,
                )?;
            }
        }
        if let Expr::Binary {
            operator,
            left,
            right,
        } = expression
            && let Some(name) = operator.operator_name()
        {
            record_operator_call(
                crate::sql::ast::QualName { schema: None, name },
                &[left, right],
                storage,
                txid,
                resolver,
                dependencies,
                needs_scope,
            )?;
        }
        super::walk_children(expression, &mut |child| {
            visit_expr(child, storage, txid, resolver, dependencies, needs_scope)
        })
    }

    fn record_table_call(
        table: &TableRef<'_>,
        args: &[&Expr<'_>],
        storage: &Storage,
        txid: u32,
        resolver: &DependencyTypes<'_, '_, '_>,
        dependencies: &mut StoredQueryDependencies,
        needs_scope: &mut bool,
    ) -> Result<(), SqlError> {
        use core::fmt::Write as _;
        let mut qualified = crate::util::StackStr::<128>::new();
        let name = if let Some(schema) = table.schema {
            write!(qualified, "{schema}.{}", table.table).map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "routine dependency name is too long"
                )
            })?;
            qualified.as_str()
        } else {
            table.table
        };
        record_routine_call(
            name,
            args,
            table.func_argument_names,
            table.func_variadic,
            storage,
            txid,
            resolver,
            dependencies,
            needs_scope,
        )
    }

    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => visit_expr(
                expression,
                storage,
                txid,
                resolver,
                dependencies,
                &mut needs_scope,
            )?,
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {}
        }
    }
    for expression_node in select
        .distinct_on
        .iter()
        .copied()
        .chain(select.where_clause)
        .chain(select.group_by.iter().copied())
        .chain(select.having)
        .chain(select.order_by.iter().map(|order| order.expression))
        .chain(select.limit)
        .chain(select.offset)
    {
        visit_expr(
            expression_node,
            storage,
            txid,
            resolver,
            dependencies,
            &mut needs_scope,
        )?;
    }
    if let Some(from) = select.from {
        for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
            if let Some(sample) = table.sample {
                visit_expr(
                    sample.percentage,
                    storage,
                    txid,
                    resolver,
                    dependencies,
                    &mut needs_scope,
                )?;
                if let Some(repeatable) = sample.repeatable {
                    visit_expr(
                        repeatable,
                        storage,
                        txid,
                        resolver,
                        dependencies,
                        &mut needs_scope,
                    )?;
                }
            }
            if let Some(args) = table.func_args {
                record_table_call(
                    table,
                    args,
                    storage,
                    txid,
                    resolver,
                    dependencies,
                    &mut needs_scope,
                )?;
                for argument in args {
                    visit_expr(
                        argument,
                        storage,
                        txid,
                        resolver,
                        dependencies,
                        &mut needs_scope,
                    )?;
                }
            }
            if let Some(functions) = table.rows_from {
                for function in functions {
                    if let Some(args) = function.func_args {
                        record_table_call(
                            function,
                            args,
                            storage,
                            txid,
                            resolver,
                            dependencies,
                            &mut needs_scope,
                        )?;
                        for argument in args {
                            visit_expr(
                                argument,
                                storage,
                                txid,
                                resolver,
                                dependencies,
                                &mut needs_scope,
                            )?;
                        }
                    }
                }
            }
        }
        for join in from.joins {
            if let Some(on) = join.on {
                visit_expr(on, storage, txid, resolver, dependencies, &mut needs_scope)?;
            }
        }
    }
    Ok(needs_scope)
}

#[derive(Clone, Copy)]
struct RelationSource<'a> {
    class: DependencyClass,
    slot: usize,
    exposed: &'a str,
    columns: [&'a str; MAX_COLUMNS],
    n_columns: usize,
}

fn record_relation_column_references<'a>(
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    from: &'a crate::sql::ast::FromClause<'a>,
    select: &'a Select<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let mut sources = [RelationSource {
        class: DependencyClass::Table,
        slot: 0,
        exposed: "",
        columns: [""; MAX_COLUMNS],
        n_columns: 0,
    }; super::MAX_JOIN_TABLES];
    let mut n_sources = 0;
    for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
        if table.subquery.is_some() || table.is_function_source() {
            continue;
        }
        let Some(relation) = storage.resolve_relation_under(path, table.schema, table.table, txid)
        else {
            continue;
        };
        if n_sources == sources.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "view query exceeds static source bound"
            ));
        }
        let source = &mut sources[n_sources];
        source.exposed = table.alias.unwrap_or(table.table);
        match relation {
            ResolvedRelation::Table(slot) => {
                let definition = storage.table_def(slot, txid);
                source.class = DependencyClass::Table;
                source.slot = slot;
                source.n_columns = definition.columns().len();
                for column in 0..source.n_columns {
                    source.columns[column] = table
                        .col_alias
                        .and_then(|aliases| aliases.get(column).copied())
                        .unwrap_or(definition.columns()[column].name.as_str());
                }
            }
            ResolvedRelation::View(slot) => {
                let user = crate::sql::eval::funcs::system::session_user_owned();
                let view_path =
                    storage.compute_path(storage.view_creation_path(slot), user.as_str(), txid);
                let mut described = [crate::sql::types::ColDesc::new("", 0, 0); MAX_COLUMNS];
                source.class = DependencyClass::View;
                source.slot = slot;
                source.n_columns = super::describe_stored_query(
                    storage.view_sql(slot),
                    storage,
                    txid,
                    view_path,
                    storage.view_dependencies(slot),
                    arena,
                    &mut described,
                )?;
                for (column, described) in described.iter().enumerate().take(source.n_columns) {
                    source.columns[column] = table
                        .col_alias
                        .and_then(|aliases| aliases.get(column).copied())
                        .unwrap_or(described.name);
                }
            }
            _ => continue,
        }
        n_sources += 1;
    }
    let sources = &sources[..n_sources];
    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                record_relation_expression(expression, sources, dependencies)?
            }
            SelectItem::TableWildcard(name) => {
                for source in sources.iter().filter(|source| source.exposed == *name) {
                    for column in 0..source.n_columns {
                        dependencies.mark_referenced_column(source.class, source.slot, column)?;
                    }
                }
            }
            SelectItem::Wildcard => {
                for source in sources {
                    for column in 0..source.n_columns {
                        dependencies.mark_referenced_column(source.class, source.slot, column)?;
                    }
                }
            }
        }
    }
    for expression in select
        .distinct_on
        .iter()
        .copied()
        .chain(select.where_clause)
        .chain(select.group_by.iter().copied())
        .chain(select.having)
        .chain(select.order_by.iter().map(|order| order.expression))
        .chain(select.limit)
        .chain(select.offset)
        .chain(from.joins.iter().filter_map(|join| join.on))
    {
        record_relation_expression(expression, sources, dependencies)?;
    }
    Ok(())
}

fn record_relation_expression(
    expression: &Expr<'_>,
    sources: &[RelationSource<'_>],
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let mut failure = None;
    expression.for_each_column_reference(&mut |qualifier, name| {
        if failure.is_some() {
            return;
        }
        let mut found = None;
        for (source_index, source) in sources.iter().enumerate() {
            if qualifier.is_some_and(|qualifier| qualifier != source.exposed) {
                continue;
            }
            if let Some(column) = source.columns[..source.n_columns]
                .iter()
                .position(|column| *column == name)
            {
                if found.is_some() {
                    failure = Some(sql_err!(
                        sqlstate::AMBIGUOUS_COLUMN,
                        "column reference \"{}\" is ambiguous",
                        name
                    ));
                    return;
                }
                found = Some((source_index, column));
            }
        }
        if let Some((source_index, column)) = found
            && let Err(error) = dependencies.mark_referenced_column(
                sources[source_index].class,
                sources[source_index].slot,
                column,
            )
        {
            failure = Some(error);
        }
    });
    failure.map_or(Ok(()), Err)
}

fn record_column_references(
    expression: &Expr<'_>,
    scope: &super::QueryScope<'_>,
    dependencies: &mut StoredQueryDependencies,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let mut failure = None;
    expression.for_each_column_reference(&mut |qualifier, name| {
        if failure.is_some() {
            return;
        }
        match context.transition_column(qualifier, name) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                failure = Some(error);
                return;
            }
        }
        let resolved = match scope.find_column(qualifier, name) {
            Ok(resolved) => resolved,
            Err(error) => {
                failure = Some(error);
                return;
            }
        };
        if let Err(error) = mark_resolved_column(scope, resolved, dependencies) {
            failure = Some(error);
        }
    });
    failure.map_or(Ok(()), Err)
}

fn mark_resolved_column(
    scope: &super::QueryScope<'_>,
    resolved: super::ResolvedColumn,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    match resolved {
        super::ResolvedColumn::Table(table, column) => {
            dependencies.mark_referenced_column(DependencyClass::Table, scope.slots[table], column)
        }
        super::ResolvedColumn::Merged(merged) => {
            for &(table, column) in &scope.merged[merged].parts[..scope.merged[merged].n_parts] {
                dependencies.mark_referenced_column(
                    DependencyClass::Table,
                    scope.slots[table],
                    column,
                )?;
            }
            Ok(())
        }
    }
}

fn collect_set_tree<'a>(
    tree: &'a SetTree<'a>,
    storage: &Storage,
    txid: u32,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    match tree {
        SetTree::Select(select) => {
            collect_select(select, storage, txid, ctes, dependencies, arena, context)
        }
        SetTree::Op { left, right, .. } => {
            collect_set_tree(left, storage, txid, ctes, dependencies, arena, context)?;
            collect_set_tree(right, storage, txid, ctes, dependencies, arena, context)
        }
    }
}

fn collect_table_ref<'a>(
    table: &'a TableRef<'a>,
    storage: &Storage,
    txid: u32,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let path = context.path;
    if let Some(select) = table.subquery {
        collect_select(select, storage, txid, ctes, dependencies, arena, context)?;
    } else if !table.is_function_source()
        && table.cte.is_none()
        && !(table.schema.is_none() && ctes.contains(table.table))
    {
        match storage.resolve_relation_under(path, table.schema, table.table, txid) {
            Some(ResolvedRelation::Table(slot)) => {
                let definition = storage.table_def(slot, txid);
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::Table,
                    slot: slot as u16,
                    identity: StoredDependencyIdentity::Name,
                    referenced_columns: 0,
                    schema: definition.schema,
                    name: definition.name,
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::View(slot)) => {
                let view = storage.view(slot);
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::View,
                    slot: slot as u16,
                    identity: StoredDependencyIdentity::Name,
                    referenced_columns: 0,
                    schema: view.schema_for(txid),
                    name: view.name_for(txid),
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::Catalog) | None => {}
        }
    }
    if let Some(arguments) = table.func_args {
        for argument in arguments {
            collect_expression(argument, storage, txid, ctes, dependencies, arena, context)?;
        }
    }
    if let Some(sample) = table.sample {
        collect_expression(
            sample.percentage,
            storage,
            txid,
            ctes,
            dependencies,
            arena,
            context,
        )?;
        if let Some(repeatable) = sample.repeatable {
            collect_expression(
                repeatable,
                storage,
                txid,
                ctes,
                dependencies,
                arena,
                context,
            )?;
        }
    }
    if let Some(functions) = table.rows_from {
        for function in functions {
            collect_table_ref(function, storage, txid, ctes, dependencies, arena, context)?;
        }
    }
    Ok(())
}

fn collect_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let path = context.path;
    if let Expr::Cast { type_name, .. } = expression {
        collect_type(type_name, storage, txid, path, dependencies)?;
    }
    if let Expr::Collate {
        collation: crate::sql::ast::ParsedCollation::Named(name),
        ..
    } = expression
    {
        let found = match name.schema {
            Some(schema) => storage.collation_slot(schema, name.name, txid),
            None => path.entries().iter().find_map(|entry| match entry {
                PathEntry::Schema(schema_slot) => storage.collation_slot(
                    storage.schema_def(*schema_slot as usize).name.as_str(),
                    name.name,
                    txid,
                ),
                PathEntry::Catalog => None,
            }),
        };
        let slot = found.ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "collation \"{}\" does not exist",
                name.name
            )
        })?;
        let definition = storage.collation(slot).definition_for(txid);
        dependencies.push(StoredQueryDependency {
            class: DependencyClass::Collation,
            slot: slot as u16,
            identity: StoredDependencyIdentity::Name,
            referenced_columns: 0,
            schema: definition.schema,
            name: definition.name,
            referenced_schema: SqlName::parse(name.schema.unwrap_or(""))?,
            referenced_name: SqlName::parse(name.name)?,
        })?;
    }
    if let Expr::Call { name, args, .. } = expression
        && matches!(*name, "nextval" | "currval" | "setval")
        && let Some(sequence_name) = args.first().and_then(|argument| regclass_literal(argument))
    {
        collect_sequence(sequence_name, storage, txid, path, dependencies)?;
    }
    if let Expr::Call { name, args, .. } = expression
        && let Some(configuration_name) = text_search_config_literal(name, args)
    {
        collect_text_search_configuration(configuration_name, storage, txid, dependencies)?;
    }
    let mut child = |expression| {
        collect_expression(
            expression,
            storage,
            txid,
            ctes,
            dependencies,
            arena,
            context,
        )
    };
    match expression {
        Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => child(operand),
        Expr::Unary { operand, .. }
        | Expr::IsNull { operand, .. }
        | Expr::Field { base: operand, .. } => child(operand),
        Expr::Binary { left, right, .. }
        | Expr::Subscript {
            base: left,
            index: right,
        }
        | Expr::AnyAll {
            operand: left,
            array: right,
            ..
        } => {
            child(left)?;
            child(right)
        }
        Expr::Call {
            name,
            args,
            order_by,
            over,
            filter,
            ..
        } => {
            let _ = name;
            for argument in *args {
                child(argument)?;
            }
            for order in *order_by {
                child(order.expression)?;
            }
            if let Some(filter) = filter {
                child(filter)?;
            }
            if let Some(window) = over {
                for expression in window.partition_by {
                    child(expression)?;
                }
                for order in window.order_by {
                    child(order.expression)?;
                }
                if let Some(frame) = window.frame {
                    for bound in [frame.start, frame.end] {
                        if let FrameBound::Preceding(expression)
                        | FrameBound::Following(expression) = bound
                        {
                            child(expression)?;
                        }
                    }
                }
            }
            Ok(())
        }
        Expr::InList { operand, list, .. } => {
            child(operand)?;
            for expression in *list {
                child(expression)?;
            }
            Ok(())
        }
        Expr::Between {
            operand, low, high, ..
        } => {
            child(operand)?;
            child(low)?;
            child(high)
        }
        Expr::Like {
            operand,
            pattern,
            escape,
            ..
        } => {
            child(operand)?;
            child(pattern)?;
            if let Some(escape) = escape {
                child(escape)?;
            }
            Ok(())
        }
        Expr::Match {
            operand, pattern, ..
        } => {
            child(operand)?;
            child(pattern)
        }
        Expr::Case {
            operand,
            whens,
            otherwise,
            ..
        } => {
            if let Some(operand) = operand {
                child(operand)?;
            }
            for (condition, result) in *whens {
                child(condition)?;
                child(result)?;
            }
            if let Some(otherwise) = otherwise {
                child(otherwise)?;
            }
            Ok(())
        }
        Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
            collect_select(select, storage, txid, ctes, dependencies, arena, context)
        }
        Expr::InSubquery {
            operand, select, ..
        }
        | Expr::QuantifiedSubquery {
            operand, select, ..
        } => {
            child(operand)?;
            collect_select(select, storage, txid, ctes, dependencies, arena, context)
        }
        Expr::Array(items) => {
            for expression in *items {
                child(expression)?;
            }
            Ok(())
        }
        Expr::Slice { base, lower, upper } => {
            child(base)?;
            if let Some(lower) = lower {
                child(lower)?;
            }
            if let Some(upper) = upper {
                child(upper)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn collect_type(
    type_name: &str,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let bare = type_name.strip_suffix("[]").unwrap_or(type_name);
    let (referenced_schema, referenced_name) = bare
        .split_once('.')
        .map_or(("", bare), |(schema, name)| (schema, name));
    let referenced_schema = SqlName::parse(referenced_schema)?;
    let referenced_name = SqlName::parse(referenced_name)?;
    let record = |class, slot: usize, schema, name, dependencies: &mut StoredQueryDependencies| {
        dependencies.push(StoredQueryDependency {
            class,
            slot: slot as u16,
            identity: StoredDependencyIdentity::Name,
            referenced_columns: 0,
            schema,
            name,
            referenced_schema,
            referenced_name,
        })
    };
    if let Some((schema, name)) = bare.split_once('.') {
        if let Some(slot) = storage.domain_slot(schema, name, txid) {
            let definition = storage.domain(slot);
            return record(
                DependencyClass::Domain,
                slot,
                definition.schema,
                definition.name,
                dependencies,
            );
        }
        if let Some(slot) = storage.enum_slot(schema, name, txid) {
            let definition = storage.enum_for(slot, txid);
            return record(
                DependencyClass::Enum,
                slot,
                definition.schema,
                definition.name,
                dependencies,
            );
        }
        if let Some(slot) = storage.composite_slot(schema, name, txid) {
            let definition = storage.composite_for(slot, txid);
            return record(
                DependencyClass::Composite,
                slot,
                definition.schema,
                definition.name,
                dependencies,
            );
        }
        return Ok(());
    }
    for entry in path.entries() {
        match entry {
            PathEntry::Catalog if ColType::from_sql_name(bare).is_some() => return Ok(()),
            PathEntry::Catalog => {}
            PathEntry::Schema(schema_slot) => {
                let schema = storage.schema_def(*schema_slot as usize).name;
                if let Some(slot) = storage.domain_slot(schema.as_str(), bare, txid) {
                    let definition = storage.domain(slot);
                    return record(
                        DependencyClass::Domain,
                        slot,
                        definition.schema,
                        definition.name,
                        dependencies,
                    );
                }
                if let Some(slot) = storage.enum_slot(schema.as_str(), bare, txid) {
                    let definition = storage.enum_for(slot, txid);
                    return record(
                        DependencyClass::Enum,
                        slot,
                        definition.schema,
                        definition.name,
                        dependencies,
                    );
                }
                if let Some(slot) = storage.composite_slot(schema.as_str(), bare, txid) {
                    let definition = storage.composite_for(slot, txid);
                    return record(
                        DependencyClass::Composite,
                        slot,
                        definition.schema,
                        definition.name,
                        dependencies,
                    );
                }
            }
        }
    }
    Ok(())
}

fn regclass_literal<'a>(expression: &'a Expr<'a>) -> Option<&'a str> {
    match expression {
        Expr::Str(value) => Some(value),
        Expr::Cast { operand, .. } | Expr::Collate { operand, .. } => regclass_literal(operand),
        _ => None,
    }
}

fn text_search_config_literal<'a>(name: &str, args: &'a [&Expr<'a>]) -> Option<&'a str> {
    let explicit = match name {
        "to_tsvector"
        | "to_tsquery"
        | "plainto_tsquery"
        | "phraseto_tsquery"
        | "websearch_to_tsquery" => args.len() == 2,
        "json_to_tsvector" | "jsonb_to_tsvector" => args.len() == 3,
        "ts_headline" => matches!(args.len(), 3 | 4),
        _ => false,
    };
    explicit
        .then(|| args.first().copied())
        .flatten()
        .and_then(regclass_literal)
}

fn collect_text_search_configuration(
    configuration_name: &str,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let (schema, name) = configuration_name
        .rsplit_once('.')
        .map_or((None, configuration_name), |(schema, name)| {
            (Some(schema), name)
        });
    let slot = storage
        .text_search_slot_on_path(
            crate::sql::ast::TextSearchObjectKind::Configuration,
            schema,
            name,
            txid,
        )
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "text search configuration \"{}\" does not exist",
                configuration_name
            )
        })?;
    let definition = storage.text_search_object(slot).definition_for(txid);
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::TextSearchConfiguration,
        slot: slot as u16,
        identity: StoredDependencyIdentity::Name,
        referenced_columns: 0,
        schema: definition.schema(),
        name: definition.name(),
        referenced_schema: SqlName::parse(schema.unwrap_or(""))?,
        referenced_name: SqlName::parse(name)?,
    })
}

fn collect_sequence(
    sequence_name: &str,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let (schema, name) = sequence_name
        .rsplit_once('.')
        .map_or((None, sequence_name), |(schema, name)| (Some(schema), name));
    let slot = if let Some(schema) = schema {
        storage.sequence_slot(schema, name, txid)
    } else {
        path.entries().iter().find_map(|entry| match entry {
            PathEntry::Schema(schema_slot) => {
                let schema = storage.schema_def(*schema_slot as usize).name;
                storage.sequence_slot(schema.as_str(), name, txid)
            }
            PathEntry::Catalog => None,
        })
    };
    if let Some(slot) = slot {
        let sequence = storage.sequence_for(slot, txid);
        dependencies.push(StoredQueryDependency {
            class: DependencyClass::Sequence,
            slot: slot as u16,
            identity: StoredDependencyIdentity::Name,
            referenced_columns: 0,
            schema: sequence.schema,
            name: sequence.name,
            referenced_schema: SqlName::parse(schema.unwrap_or(""))?,
            referenced_name: SqlName::parse(name)?,
        })?;
    }
    Ok(())
}
