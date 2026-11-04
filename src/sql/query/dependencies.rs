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
use crate::sql::exec::{ColTypeResolver, StaticTypeMeta, intrinsic_record_field_meta};
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    DependencyClass, MAX_COLUMNS, MAX_STORED_QUERY_DEPENDENCIES, PathContext, PathEntry,
    ResolvedRelation, SqlName, Storage, StoredDependencyIdentity, StoredQueryDependencies,
    StoredQueryDependency,
};

use super::arena_full;

#[derive(Clone, Copy)]
struct CollectionContext<'a> {
    path: &'a PathContext,
    transition: Option<&'a dyn ColTypeResolver>,
    bindings: Option<crate::storage::StoredQueryDependencyView<'a>>,
}

impl<'a> CollectionContext<'a> {
    fn ordinary(
        path: &'a PathContext,
        bindings: Option<crate::storage::StoredQueryDependencyView<'a>>,
    ) -> Self {
        Self {
            path,
            transition: None,
            bindings,
        }
    }

    fn with_columns(path: &'a PathContext, transition: &'a dyn ColTypeResolver) -> Self {
        Self {
            path,
            transition: Some(transition),
            bindings: None,
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

    fn outer_column(self, qualifier: Option<&str>, name: &str) -> bool {
        self.transition
            .is_some_and(|columns| columns.column_meta(qualifier, name).is_some())
    }

    fn resolve_relation(
        self,
        storage: &Storage,
        txid: u32,
        schema: Option<&str>,
        name: &str,
    ) -> Result<Option<ResolvedRelation>, SqlError> {
        if let Some(dependency) = self.bindings.and_then(|bindings| {
            bindings.entries().iter().find(|dependency| {
                matches!(
                    dependency.class,
                    DependencyClass::Table | DependencyClass::View
                ) && dependency.referenced_schema.as_str() == schema.unwrap_or("")
                    && dependency.referenced_name.as_str() == name
            })
        }) {
            return storage
                .resolve_stored_query_relation(dependency, txid)
                .map(Some);
        }
        Ok(storage.resolve_relation_under(self.path, schema, name, txid))
    }

    fn bound_from<'arena>(
        self,
        from: &FromClause<'arena>,
        storage: &Storage,
        txid: u32,
        ctes: CteNames<'_>,
        arena: &'arena Arena,
    ) -> Result<FromClause<'arena>, SqlError> {
        if self.bindings.is_none() {
            return Ok(*from);
        }
        let bind = |table: TableRef<'arena>| -> Result<TableRef<'arena>, SqlError> {
            if table.subquery.is_some()
                || table.is_function_source()
                || table.cte.is_some()
                || (table.schema.is_none() && ctes.contains(table.table))
            {
                return Ok(table);
            }
            let (schema, name) =
                match self.resolve_relation(storage, txid, table.schema, table.table)? {
                    Some(ResolvedRelation::Table(slot)) => {
                        let definition = storage.table_def(slot, txid);
                        (definition.schema, definition.name)
                    }
                    Some(ResolvedRelation::View(slot)) => {
                        let view = storage.view(slot);
                        (view.schema_for(txid), view.name_for(txid))
                    }
                    _ => return Ok(table),
                };
            Ok(TableRef {
                schema: Some(arena.alloc_str(schema.as_str()).map_err(|_| arena_full())?),
                table: arena.alloc_str(name.as_str()).map_err(|_| arena_full())?,
                alias: Some(table.alias.unwrap_or(table.table)),
                ..table
            })
        };
        let base = bind(from.base)?;
        let mut joins = crate::mem::arena::ArenaList::new(arena);
        for join in from.joins {
            joins
                .push(Join {
                    table: bind(join.table)?,
                    ..*join
                })
                .map_err(|_| arena_full())?;
        }
        Ok(FromClause {
            base,
            joins: joins.as_slice(),
        })
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

pub(crate) fn collect(
    sql: &str,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    bindings: Option<crate::storage::StoredQueryDependencyView<'_>>,
    literal_inputs: &[crate::sql::type_inputs::Binding<'_>],
    arena: &Arena,
) -> Result<StoredQueryDependencies, SqlError> {
    let select =
        crate::sql::parser::parse_query_with_type_inputs(sql, literal_inputs, arena, |parser| {
            crate::sql::type_inputs::validate_restored_with(parser, |name| {
                if let Some(oid) = bindings
                    .and_then(|dependencies| super::cte::captured_type_oid(name, dependencies))
                {
                    return Ok(matches!(
                        crate::storage::NamedTypeKind::from_oid(oid),
                        Some((crate::storage::NamedTypeKind::Domain, _, true))
                    )
                    .then_some(oid));
                }
                Ok(crate::sql::type_inputs::target_oid(storage, txid, name))
            })
        })?;
    let mut dependencies = storage.empty_stored_query_dependencies(arena)?;
    collect_select(
        select,
        storage,
        txid,
        CteNames::EMPTY,
        &mut dependencies,
        arena,
        CollectionContext::ordinary(&path, bindings),
    )?;
    Ok(dependencies)
}

pub(crate) fn collect_runtime_statement(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    columns: Option<&dyn ColTypeResolver>,
) -> Result<(), SqlError> {
    let path = storage.path();
    collect_statement(
        statement,
        storage,
        txid,
        dependencies,
        arena,
        match columns {
            Some(columns) => CollectionContext::with_columns(&path, columns),
            None => CollectionContext::ordinary(&path, None),
        },
    )?;
    expand_runtime_view_dependencies(storage, txid, dependencies)
}

pub(crate) fn collect_runtime_expression(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    columns: &dyn ColTypeResolver,
) -> Result<(), SqlError> {
    let path = storage.path();
    let context = CollectionContext::with_columns(&path, columns);
    collect_expression(
        expression,
        storage,
        txid,
        CteNames::EMPTY,
        dependencies,
        arena,
        context,
    )?;
    collect_dml_routine_calls(
        expression,
        storage,
        txid,
        None,
        None,
        dependencies,
        context,
        arena,
    )?;
    expand_runtime_view_dependencies(storage, txid, dependencies)
}

fn expand_runtime_view_dependencies(
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    // Rewriting a view brings its selected base definitions into the caller's
    // plan. Stored definitions retain only their direct dependencies.
    let mut next = 0;
    while next < dependencies.entries().len() {
        let dependency = dependencies.entries()[next];
        next += 1;
        if let StoredDependencyIdentity::View(_) = dependency.identity {
            let view = storage.view(usize::from(dependency.slot));
            let selected = storage.rule_dependencies(usize::from(view.return_rule), txid);
            for entry in selected.view().entries() {
                dependencies.push(*entry)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn collect_routine_parameter_defaults(
    parameters: &[crate::storage::RoutineParameterDef],
    inputs: &[crate::storage::StoredInputSource<'_>],
    storage: &Storage,
    txid: u32,
    path: PathContext,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
) -> Result<(), SqlError> {
    for (index, parameter) in parameters.iter().enumerate() {
        let Some(default) = parameter.mode.default() else {
            continue;
        };
        let source = arena
            .alloc_str(default.as_str())
            .map_err(|_| arena_full())?;
        let literals = inputs
            .iter()
            .find(|input| {
                input.section == crate::storage::StoredInputSection::ParameterDefault(index as u16)
            })
            .map_or(&[][..], |input| input.bindings);
        let expression = crate::sql::parser::parse_expression_with_type_inputs(
            source,
            literals,
            arena,
            |parser| crate::sql::type_inputs::validate_restored(parser, storage, txid),
        )?;
        let items = arena
            .alloc_slice_copy(&[SelectItem::Expr {
                expression,
                alias: None,
            }])
            .map_err(|_| arena_full())?;
        let select = arena
            .alloc(Select {
                items,
                ..*crate::sql::parser::parse_query("SELECT NULL", arena)?
            })
            .map_err(|_| arena_full())?;
        collect_select(
            select,
            storage,
            txid,
            CteNames::EMPTY,
            dependencies,
            arena,
            CollectionContext::ordinary(&path, None),
        )?;
    }
    Ok(())
}

pub(super) fn collect_routine_program(
    program: &super::RoutineFunctionProgram<'_>,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
) -> Result<StoredQueryDependencies, SqlError> {
    let mut dependencies = storage.empty_stored_query_dependencies(arena)?;
    for step in program.preceding {
        if let super::RoutinePrelude::Statement(statement) = step {
            collect_statement(
                statement,
                storage,
                txid,
                &mut dependencies,
                arena,
                CollectionContext::ordinary(&path, None),
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
            CollectionContext::ordinary(&path, None),
        )?,
        super::RoutineFunctionResult::Query(super::RoutineQuery::Set(query)) => collect_set_query(
            query,
            storage,
            txid,
            &mut dependencies,
            arena,
            CollectionContext::ordinary(&path, None),
        )?,
        super::RoutineFunctionResult::DataModification(statement)
        | super::RoutineFunctionResult::Void(statement) => collect_statement(
            statement,
            storage,
            txid,
            &mut dependencies,
            arena,
            CollectionContext::ordinary(&path, None),
        )?,
        super::RoutineFunctionResult::Forbidden(_) => {}
    }
    Ok(dependencies)
}

pub(super) fn collect_rule_actions(
    actions: &[crate::sql::ast::RuleAction<'_>],
    condition: Option<&Expr<'_>>,
    storage: &Storage,
    txid: u32,
    path: PathContext,
    arena: &Arena,
    transition: &dyn ColTypeResolver,
) -> Result<StoredQueryDependencies, SqlError> {
    let mut dependencies = storage.empty_stored_query_dependencies(arena)?;
    if let Some(condition) = condition {
        collect_expression(
            condition,
            storage,
            txid,
            CteNames::EMPTY,
            &mut dependencies,
            arena,
            CollectionContext::with_columns(&path, transition),
        )?;
    }
    for action in actions {
        collect_statement(
            action.statement,
            storage,
            txid,
            &mut dependencies,
            arena,
            CollectionContext::with_columns(&path, transition),
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
    let context = CollectionContext::ordinary(&path, None);
    let mut dependencies = storage.empty_stored_query_dependencies(arena)?;
    collect_statement(statement, storage, txid, &mut dependencies, arena, context)?;

    let mut sources = storage.empty_stored_query_dependencies(arena)?;
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
    let mark = arena.mark();
    let result = (|| {
        super::cte::visit_cte_sources(
            query.with,
            storage,
            txid,
            Some(*context.path),
            context.bindings,
            arena,
            &mut |source| match source {
                super::cte::ScopedCte::Select(select) => collect_select(
                    select,
                    storage,
                    txid,
                    CteNames::EMPTY,
                    dependencies,
                    arena,
                    context,
                ),
                super::cte::ScopedCte::DataModification(statement) => {
                    collect_statement(statement, storage, txid, dependencies, arena, context)
                }
            },
        )?;
        let tree = super::cte::expand_set_tree(
            query.with,
            query.body,
            storage,
            txid,
            Some(*context.path),
            context.bindings,
            arena,
        )?;
        collect_set_tree(
            tree,
            storage,
            txid,
            CteNames::EMPTY,
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
                CteNames::EMPTY,
                dependencies,
                arena,
                context,
            )?;
        }
        Ok(())
    })();
    // Only copied selected catalog identities outlive these shared CTE shapes.
    unsafe { arena.rewind_to(mark) };
    result
}

fn record_routine_target(
    name: crate::sql::ast::QualName<'_>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let resolved = storage.resolve_relation_under(path, name.schema, name.name, txid);
    let Some((class, slot, schema, catalog_name, identity)) = (match resolved {
        Some(ResolvedRelation::Table(slot)) => {
            let identity =
                storage.capture_stored_query_relation(ResolvedRelation::Table(slot), txid)?;
            let definition = storage.table_def(slot, txid);
            Some((
                DependencyClass::Table,
                slot,
                definition.schema,
                definition.name,
                identity,
            ))
        }
        Some(ResolvedRelation::View(slot)) => {
            let identity =
                storage.capture_stored_query_relation(ResolvedRelation::View(slot), txid)?;
            let definition = storage.view(slot);
            Some((
                DependencyClass::View,
                slot,
                definition.schema,
                definition.name,
                identity,
            ))
        }
        Some(ResolvedRelation::Catalog) | None => None,
    }) else {
        return Ok(());
    };
    dependencies.push(StoredQueryDependency {
        class,
        slot: slot as u16,
        identity,
        referenced_columns: crate::storage::ColumnSet::EMPTY,
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
    let no_excluded: Option<(usize, crate::storage::RelationDefinition<'_>)> = None;
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
                arena,
            )
        }};
    }
    if matches!(
        statement,
        Stmt::Insert(_) | Stmt::Update(_) | Stmt::Merge(_)
    ) {
        let outer = DependencyTypes {
            scope: None,
            excluded: None,
            transition: context.transition,
            storage,
            txid,
            returning_output: None,
            arena,
        };
        visit_dml_assignment_types(
            statement,
            storage,
            txid,
            arena,
            Some(&outer),
            &mut |source, column| {
                let target = storage
                    .routine_type_oid(column.ctype, column.user_type, txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "assignment destination type is unavailable"
                        )
                    })?;
                record_cast_support(
                    source,
                    target,
                    crate::sql::eval::DefinedCastContext::Assignment,
                    storage,
                    txid,
                    dependencies,
                )
            },
        )?;
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
            let mark = arena.mark();
            let result = (|| {
                super::cte::visit_cte_sources(
                    ctes,
                    storage,
                    txid,
                    Some(*path),
                    context.bindings,
                    arena,
                    &mut |source| match source {
                        super::cte::ScopedCte::Select(select) => collect_select(
                            select,
                            storage,
                            txid,
                            CteNames::EMPTY,
                            dependencies,
                            arena,
                            context,
                        ),
                        super::cte::ScopedCte::DataModification(statement) => collect_statement(
                            statement,
                            storage,
                            txid,
                            dependencies,
                            arena,
                            context,
                        ),
                    },
                )?;
                let statement = super::cte::expand_dml_ctes_schema(
                    statement,
                    ctes,
                    storage,
                    txid,
                    Some(*path),
                    context.bindings,
                    arena,
                )?;
                collect_statement(statement, storage, txid, dependencies, arena, context)
            })();
            // Only copied catalog identities escape the CTE schema scope.
            unsafe { arena.rewind_to(mark) };
            result
        }
        Stmt::Insert(insert) => {
            let mark = arena.mark();
            let result = (|| {
                record_routine_target(insert.table, storage, txid, path, dependencies)?;
                let scope = dml_dependency_scope(storage, insert.table, None, None, txid, arena)?;
                let excluded = match storage.resolve_relation_under(
                    path,
                    insert.table.schema,
                    insert.table.name,
                    txid,
                ) {
                    Some(ResolvedRelation::Table(slot)) => {
                        Some((slot, scope.defs.get(0).expect("target resolved")))
                    }
                    _ => None,
                };
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
                for item in insert.returning.items {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_returning_expression(
                            expression,
                            insert.returning,
                            scope.defs.get(0).expect("target resolved"),
                            insert.table.name,
                            storage,
                            txid,
                            &scope,
                            dependencies,
                            arena,
                            context,
                        )?;
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
                for item in update.returning.items {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_returning_expression(
                            expression,
                            update.returning,
                            scope.defs.get(0).expect("target resolved"),
                            update.alias.unwrap_or(update.table.name),
                            storage,
                            txid,
                            &scope,
                            dependencies,
                            arena,
                            context,
                        )?;
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
                for item in delete.returning.items {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_returning_expression(
                            expression,
                            delete.returning,
                            scope.defs.get(0).expect("target resolved"),
                            delete.alias.unwrap_or(delete.table.name),
                            storage,
                            txid,
                            &scope,
                            dependencies,
                            arena,
                            context,
                        )?;
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
                for item in merge.returning.items {
                    if let SelectItem::Expr { expression, .. }
                    | SelectItem::RecordStar(expression) = item
                    {
                        collect_returning_expression(
                            expression,
                            merge.returning,
                            scope.defs.get(0).expect("target resolved"),
                            merge.target_alias.unwrap_or(merge.target.name),
                            storage,
                            txid,
                            &scope,
                            dependencies,
                            arena,
                            context,
                        )?;
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

#[expect(
    clippy::too_many_arguments,
    reason = "stored query analysis keeps its arena and captured identities explicit"
)]
fn collect_dml_routine_calls<'a>(
    expression: &Expr<'a>,
    storage: &Storage,
    txid: u32,
    scope: Option<&super::QueryScope<'a>>,
    excluded: Option<crate::storage::RelationDefinition<'_>>,
    dependencies: &mut StoredQueryDependencies,
    context: CollectionContext<'a>,
    arena: &Arena,
) -> Result<(), SqlError> {
    collect_dml_routine_calls_with_output(
        expression,
        storage,
        txid,
        scope,
        excluded,
        dependencies,
        context,
        None,
        arena,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "the DML dependency boundary keeps catalog, scope, and output aliases explicit"
)]
fn collect_dml_routine_calls_with_output<'a>(
    expression: &Expr<'a>,
    storage: &Storage,
    txid: u32,
    scope: Option<&super::QueryScope<'a>>,
    excluded: Option<crate::storage::RelationDefinition<'_>>,
    dependencies: &mut StoredQueryDependencies,
    context: CollectionContext<'a>,
    returning_output: Option<(crate::storage::RelationDefinition<'_>, [&'a str; 2])>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let resolver = DependencyTypes {
        scope,
        excluded,
        storage,
        txid,
        returning_output,
        transition: context.transition,
        arena,
    };
    let mut unresolved = None;
    visit_routine_expression(
        expression,
        storage,
        txid,
        &resolver,
        dependencies,
        &mut unresolved,
    )?;
    if let Some(error) = unresolved {
        return Err(error);
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "RETURNING dependency collection needs the explicit DML analysis boundary"
)]
fn collect_returning_expression<'a>(
    expression: &Expr<'a>,
    returning: crate::sql::ast::Returning<'a>,
    target: crate::storage::RelationDefinition<'_>,
    target_qualifier: &str,
    storage: &Storage,
    txid: u32,
    scope: &super::QueryScope<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'a>,
) -> Result<(), SqlError> {
    collect_expression(
        expression,
        storage,
        txid,
        CteNames::EMPTY,
        dependencies,
        arena,
        context,
    )?;
    let aliases = [returning.old_name(), returning.new_name()];
    let mut failure = None;
    expression.for_each_column_reference(&mut |qualifier, name| {
        if failure.is_some() {
            return;
        }
        let resolved = if qualifier.is_some_and(|qualifier| aliases.contains(&qualifier)) {
            scope.find_column(Some(target_qualifier), name)
        } else {
            scope.find_column(qualifier, name)
        };
        match resolved {
            Ok(resolved) => {
                if let Err(error) = mark_resolved_column(scope, resolved, dependencies) {
                    failure = Some(error);
                }
            }
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) && !qualifier.is_some_and(|qualifier| aliases.contains(&qualifier))
                    && context.outer_column(qualifier, name) => {}
            Err(error) => failure = Some(error),
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    collect_dml_routine_calls_with_output(
        expression,
        storage,
        txid,
        Some(scope),
        None,
        dependencies,
        context,
        Some((target, aliases)),
        arena,
    )
}

fn record_dml_column_references(
    expression: &Expr<'_>,
    scope: &super::QueryScope<'_>,
    excluded: Option<(usize, crate::storage::RelationDefinition<'_>)>,
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
                    "column excluded.{} does not exist",
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
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) && context.outer_column(qualifier, name) =>
            {
                return;
            }
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

/// Check declared assignment types before row selection, triggers, or defaults
/// can run. A typed NULL and an empty array still carry a cast contract.
pub(crate) fn validate_dml_assignment_casts(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    transition: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let (statement, with) = match statement {
            Stmt::With { ctes, statement } => (*statement, *ctes),
            _ => (statement, &[][..]),
        };
        for (index, cte) in with.iter().enumerate() {
            if let Some(statement) = cte.dml {
                let statement = super::cte::expand_dml_ctes_schema(
                    statement,
                    &with[..index],
                    storage,
                    txid,
                    None,
                    None,
                    arena,
                )?;
                validate_dml_assignment_casts(statement, storage, txid, arena, transition)?;
            }
        }
        let target = match statement {
            Stmt::Insert(insert) => insert.table,
            Stmt::Update(update) => update.table,
            Stmt::Merge(merge) => merge.target,
            _ => return Ok(()),
        };
        match storage.resolve_relation(target.schema, target.name, txid) {
            Some(ResolvedRelation::Table(slot)) => {
                let definition = storage.table_def(slot, txid);
                if !definition
                    .columns()
                    .iter()
                    .any(|column| geometry_assignment_type(column.ctype).is_some())
                {
                    return Ok(());
                }
            }
            Some(ResolvedRelation::View(_)) => {}
            _ => return Ok(()),
        }
        let statement =
            super::cte::expand_dml_ctes_schema(statement, with, storage, txid, None, None, arena)?;
        let (alias, extra) = match statement {
            Stmt::Insert(_) => (None, None),
            Stmt::Update(update) => (update.alias, update.from),
            Stmt::Merge(merge) => {
                let source = arena
                    .alloc(FromClause {
                        base: merge.source,
                        joins: &[],
                    })
                    .map_err(|_| arena_full())?;
                (merge.target_alias, Some(&*source))
            }
            _ => unreachable!(),
        };
        let target_name = target.name;
        let scope = dml_dependency_scope(storage, target, alias, extra, txid, arena)?;
        let target = scope.defs.get(0).expect("assignment target resolved");
        let catalog = super::storage_catalog(storage, arena, txid);
        let transition = transition.map(|row| crate::sql::eval::RuntimeColumnTypes {
            row,
            catalog: Some(&catalog),
        });
        let columns = DependencyTypes {
            scope: Some(&scope),
            excluded: Some(target),
            transition: transition
                .as_ref()
                .map(|columns| columns as &dyn ColTypeResolver),
            storage,
            txid,
            returning_output: None,
            arena,
        };
        let assign = |name: &str, expression: &Expr<'_>| -> Result<(), SqlError> {
            let index = target.column_index(name).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" of relation \"{}\" does not exist",
                    name,
                    target_name
                )
            })?;
            validate_assignment_expression(
                expression,
                &target.columns()[index],
                &columns,
                storage,
                txid,
            )
        };
        let insert_values = |names: &[&str], values: &[&Expr<'_>]| -> Result<(), SqlError> {
            for (index, expression) in values.iter().enumerate() {
                let column = if names.is_empty() {
                    target.columns().get(index)
                } else {
                    names
                        .get(index)
                        .and_then(|name| target.column_index(name))
                        .map(|index| &target.columns()[index])
                };
                if let Some(column) = column {
                    validate_assignment_expression(expression, column, &columns, storage, txid)?;
                }
            }
            Ok(())
        };
        match statement {
            Stmt::Insert(insert) => {
                for row in insert.rows {
                    insert_values(insert.columns, row)?;
                }
                if let Some(select) = insert.select {
                    validate_insert_projection(
                        select,
                        insert.columns,
                        target,
                        storage,
                        txid,
                        arena,
                        transition
                            .as_ref()
                            .map(|columns| columns as &dyn ColTypeResolver),
                        transition.as_ref().map(|columns| columns.row),
                    )?;
                }
                for (name, expression) in insert
                    .on_conflict
                    .and_then(|conflict| conflict.update)
                    .into_iter()
                    .flatten()
                {
                    assign(name, expression)?;
                }
            }
            Stmt::Update(update) => {
                for (name, expression) in update.assignments {
                    assign(name, expression)?;
                }
            }
            Stmt::Merge(merge) => {
                for when in merge.whens {
                    match when.action() {
                        crate::sql::ast::MergeActionRef::Update(assignments) => {
                            for (name, expression) in assignments {
                                assign(name, expression)?;
                            }
                        }
                        crate::sql::ast::MergeActionRef::Insert {
                            columns, values, ..
                        } => insert_values(columns, values)?,
                        _ => {}
                    }
                }
            }
            _ => unreachable!(),
        }
        Ok(())
    })();
    // Validation returns no arena-backed values or catalog owners.
    unsafe { arena.rewind_to(mark) };
    result
}

#[expect(
    clippy::too_many_arguments,
    reason = "schema-only INSERT projection assignment context"
)]
fn validate_insert_projection(
    select: &Select<'_>,
    names: &[&str],
    target: crate::storage::RelationDefinition<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    transition: Option<&dyn ColTypeResolver>,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
) -> Result<(), SqlError> {
    if let Some(body) = select.set_body {
        fn branches(
            tree: &SetTree<'_>,
            visit: &mut dyn FnMut(&Select<'_>) -> Result<(), SqlError>,
        ) -> Result<(), SqlError> {
            match tree {
                SetTree::Select(select) => visit(select),
                SetTree::Op { left, right, .. } => {
                    branches(left, visit)?;
                    branches(right, visit)
                }
            }
        }
        return branches(body, &mut |select| {
            validate_insert_projection(
                select, names, target, storage, txid, arena, transition, outer,
            )
        });
    }
    let scope = select
        .from
        .as_ref()
        .map(|from| super::QueryScope::resolve_schema(storage, from, txid, arena, outer))
        .transpose()?;
    let columns = DependencyTypes {
        scope: scope.as_ref(),
        excluded: None,
        transition,
        storage,
        txid,
        returning_output: None,
        arena,
    };
    let mut index = 0;
    for item in select.items {
        if matches!(item, SelectItem::Expr { .. }) {
            let column = if names.is_empty() {
                target.columns().get(index)
            } else {
                names
                    .get(index)
                    .and_then(|name| target.column_index(name))
                    .map(|index| &target.columns()[index])
            };
            if column.is_none_or(|column| geometry_assignment_type(column.ctype).is_none()) {
                index += 1;
                continue;
            }
        }
        let mut assign = |source_oid| -> Result<(), SqlError> {
            let column = if names.is_empty() {
                target.columns().get(index)
            } else {
                names
                    .get(index)
                    .and_then(|name| target.column_index(name))
                    .map(|index| &target.columns()[index])
            };
            index += 1;
            if let Some(column) = column {
                validate_assignment_type(source_oid, column, storage, txid)?;
            }
            Ok(())
        };
        match item {
            SelectItem::Expr { expression, .. } => assign(
                crate::sql::exec::infer_routine_argument_oid(expression, &columns)?,
            )?,
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                if let Some(scope) = &scope {
                    let count = match item {
                        SelectItem::TableWildcard(name) => scope.qualified_star_columns(name)?,
                        _ => scope.star_columns(),
                    };
                    for index in 0..count {
                        let entry = match item {
                            SelectItem::TableWildcard(name) => {
                                scope.qualified_star_entry(name, index)?
                            }
                            _ => scope.star_entry(index),
                        };
                        let metadata = super::scope_column_metadata(scope, entry, storage, txid)
                            .ok_or_else(|| {
                                sql_err!(
                                    sqlstate::UNDEFINED_OBJECT,
                                    "source column type identity is unavailable"
                                )
                            })?;
                        assign(metadata.type_oid)?;
                    }
                }
            }
            SelectItem::RecordStar(expression) => {
                let mut result = Ok(());
                let shape = crate::sql::exec::record_shape_metadata(
                    expression,
                    &columns,
                    |_, metadata| {
                        if result.is_ok() {
                            result = assign(metadata.type_oid);
                        }
                    },
                )?;
                result?;
                if shape.is_none() {
                    let type_oid =
                        crate::sql::exec::infer_routine_argument_oid(expression, &columns)?;
                    return Err(crate::sql::exec::not_composite(
                        "*",
                        ColType::from_oid(type_oid).map_or("unknown", ColType::name),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn for_each_dml_assignment<'a>(
    statement: &Stmt<'a>,
    visit: &mut dyn FnMut(
        Option<&'a str>,
        usize,
        &'a Expr<'a>,
        Option<crate::sql::type_inputs::AssignmentSite>,
    ) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    use crate::sql::type_inputs::{AssignmentNode, AssignmentSite};
    let position = match statement {
        Stmt::Insert(value) => value.source_position.get(),
        Stmt::Update(value) => value.source_position.get(),
        Stmt::Merge(value) => value.source_position.get(),
        _ => None,
    };
    let site = |node| {
        position.map(|statement_position| AssignmentSite {
            statement_position,
            node,
        })
    };
    match statement {
        Stmt::Insert(insert) => {
            for (row_index, row) in insert.rows.iter().enumerate() {
                for (index, expression) in row.iter().enumerate() {
                    visit(
                        insert.columns.get(index).copied(),
                        index,
                        expression,
                        site(AssignmentNode::Values {
                            row: row_index,
                            value: index,
                        }),
                    )?;
                }
            }
            for (index, (name, expression)) in insert
                .on_conflict
                .and_then(|value| value.update)
                .into_iter()
                .flatten()
                .enumerate()
            {
                visit(
                    Some(name),
                    index,
                    expression,
                    site(AssignmentNode::OnConflict { assignment: index }),
                )?;
            }
        }
        Stmt::Update(update) => {
            for (index, (name, expression)) in update.assignments.iter().enumerate() {
                visit(
                    Some(name),
                    index,
                    expression,
                    site(AssignmentNode::Update { assignment: index }),
                )?;
            }
        }
        Stmt::Merge(merge) => {
            for (when_index, when) in merge.whens.iter().enumerate() {
                match when.action() {
                    crate::sql::ast::MergeActionRef::Update(assignments) => {
                        for (index, (name, expression)) in assignments.iter().enumerate() {
                            visit(
                                Some(name),
                                index,
                                expression,
                                site(AssignmentNode::Merge {
                                    when: when_index,
                                    value: index,
                                }),
                            )?;
                        }
                    }
                    crate::sql::ast::MergeActionRef::Insert {
                        columns, values, ..
                    } => {
                        for (index, expression) in values.iter().enumerate() {
                            visit(
                                columns.get(index).copied(),
                                index,
                                expression,
                                site(AssignmentNode::Merge {
                                    when: when_index,
                                    value: index,
                                }),
                            )?;
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn visit_dml_assignment_types(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    visit: &mut dyn FnMut(i32, &crate::storage::ColumnMeta) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    visit_dml_assignment_contexts(
        statement,
        storage,
        txid,
        arena,
        outer,
        &mut |expression, column, columns| {
            visit(
                crate::sql::exec::infer_routine_argument_oid(expression, columns)?,
                column,
            )
        },
    )
}

type AssignmentContextVisit<'visit> = dyn FnMut(&Expr<'_>, &crate::storage::ColumnMeta, &dyn ColTypeResolver) -> Result<(), SqlError>
    + 'visit;

pub(super) fn visit_dml_assignment_contexts(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    visit: &mut AssignmentContextVisit<'_>,
) -> Result<(), SqlError> {
    visit_dml_assignment_inputs(
        statement,
        storage,
        txid,
        arena,
        outer,
        false,
        &mut |group, columns| {
            for assignment in group {
                if !matches!(assignment.expression, Expr::DefaultMarker) {
                    visit(assignment.expression, assignment.column, columns)?;
                }
            }
            Ok(())
        },
    )
}

#[derive(Clone, Copy)]
pub(super) struct AssignmentInput<'a> {
    pub expression: &'a Expr<'a>,
    pub column: &'a crate::storage::ColumnMeta,
    pub site: Option<crate::sql::type_inputs::AssignmentSite>,
    ordinal: usize,
}

type AssignmentGroupVisit<'visit> =
    dyn FnMut(&[AssignmentInput<'_>], &dyn ColTypeResolver) -> Result<(), SqlError> + 'visit;

pub(super) fn visit_dml_assignment_groups(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    visit: &mut AssignmentGroupVisit<'_>,
) -> Result<(), SqlError> {
    visit_dml_assignment_inputs(statement, storage, txid, arena, outer, true, visit)
}

fn visit_dml_assignment_inputs(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    grouped: bool,
    visit: &mut AssignmentGroupVisit<'_>,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let (statement, with) = match statement {
            Stmt::With { ctes, statement } => (*statement, *ctes),
            _ => (statement, &[][..]),
        };
        let statement =
            super::cte::expand_dml_ctes_schema(statement, with, storage, txid, None, None, arena)?;
        let (target, alias, extra) = match statement {
            Stmt::Insert(insert) => (insert.table, None, None),
            Stmt::Update(update) => (update.table, update.alias, update.from),
            Stmt::Merge(merge) => {
                let source = arena
                    .alloc(FromClause {
                        base: merge.source,
                        joins: &[],
                    })
                    .map_err(|_| arena_full())?;
                (merge.target, merge.target_alias, Some(&*source))
            }
            _ => return Ok(()),
        };
        let target_name = target.name;
        let scope = dml_dependency_scope(storage, target, alias, extra, txid, arena)?;
        let target = scope.defs.get(0).expect("assignment target resolved");
        let catalog = super::storage_catalog(storage, arena, txid);
        let transition = outer.map(|row| crate::sql::eval::RuntimeColumnTypes {
            row,
            catalog: Some(&catalog),
        });
        let columns = DependencyTypes {
            scope: Some(&scope),
            excluded: Some(target),
            transition: transition
                .as_ref()
                .map(|columns| columns as &dyn ColTypeResolver),
            storage,
            txid,
            returning_output: None,
            arena,
        };
        let mut assignments = crate::mem::arena::ArenaList::new(arena);
        for_each_dml_assignment(statement, &mut |name, index, expression, site| {
            let ordinal = index;
            let index = match name {
                Some(name) => target.column_index(name).ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" of relation \"{}\" does not exist",
                        name,
                        target_name
                    )
                })?,
                None => index,
            };
            let column = target.columns().get(index).ok_or_else(|| {
                sql_err!(
                    sqlstate::SYNTAX_ERROR,
                    "INSERT has more expressions than target columns"
                )
            })?;
            let assignment = AssignmentInput {
                expression,
                column,
                site,
                ordinal,
            };
            if grouped {
                assignments.push(assignment).map_err(|_| arena_full())
            } else {
                visit(core::slice::from_ref(&assignment), &columns)
            }
        })?;
        let assignments = assignments.as_slice();
        let mut start = 0;
        while start < assignments.len() {
            let end = assignments[start + 1..]
                .iter()
                .position(|assignment| assignment.ordinal == 0)
                .map_or(assignments.len(), |offset| start + 1 + offset);
            visit(&assignments[start..end], &columns)?;
            start = end;
        }
        Ok(())
    })();
    // Only Copy cast snapshots, allocated in the persistent tail, may escape.
    unsafe { arena.rewind_to(mark) };
    result
}

/// Visit declared output identities before a DML source can execute.
pub(crate) fn visit_projection_types(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    visit: &mut dyn FnMut(i32, crate::sql::ast::Collation) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let select = super::expand_ctes(select, storage, txid, arena)?;
        if let Some(tree) = select.set_body {
            let mut columns =
                [crate::sql::types::ColDesc::new("", 0, 0); crate::sql::exec::MAX_PROJ];
            let mut oids = [0; crate::sql::exec::MAX_PROJ];
            let count = super::setops::describe_set_body(
                storage,
                tree,
                txid,
                &mut columns,
                arena,
                Some(&mut oids),
            )?;
            for (oid, column) in oids[..count].iter().zip(&columns[..count]) {
                visit(*oid, column.collation)?;
            }
            return Ok(());
        }
        let scope = select
            .from
            .as_ref()
            .map(|from| super::QueryScope::resolve_schema(storage, from, txid, arena, outer))
            .transpose()?;
        let catalog = super::storage_catalog(storage, arena, txid);
        let transition = outer.map(|row| crate::sql::eval::RuntimeColumnTypes {
            row,
            catalog: Some(&catalog),
        });
        let columns = DependencyTypes {
            scope: scope.as_ref(),
            excluded: None,
            transition: transition
                .as_ref()
                .map(|columns| columns as &dyn ColTypeResolver),
            storage,
            txid,
            returning_output: None,
            arena,
        };
        for item in select.items {
            match item {
                SelectItem::Expr { expression, .. } => visit(
                    crate::sql::exec::infer_routine_argument_oid(expression, &columns)?,
                    crate::sql::eval::routine_input_collation(
                        &[expression],
                        &columns,
                        Some(&catalog),
                    )?,
                )?,
                SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                    if let Some(scope) = &scope {
                        let count = match item {
                            SelectItem::TableWildcard(name) => {
                                scope.qualified_star_columns(name)?
                            }
                            _ => scope.star_columns(),
                        };
                        for index in 0..count {
                            let entry = match item {
                                SelectItem::TableWildcard(name) => {
                                    scope.qualified_star_entry(name, index)?
                                }
                                _ => scope.star_entry(index),
                            };
                            let metadata =
                                super::scope_column_metadata(scope, entry, storage, txid)
                                    .ok_or_else(|| {
                                        sql_err!(
                                            sqlstate::UNDEFINED_OBJECT,
                                            "source column type identity is unavailable"
                                        )
                                    })?;
                            visit(metadata.type_oid, metadata.collation)?;
                        }
                    }
                }
                SelectItem::RecordStar(expression) => {
                    let mut result = Ok(());
                    let shape = crate::sql::exec::record_shape_metadata(
                        expression,
                        &columns,
                        |_, metadata| {
                            if result.is_ok() {
                                result = visit(metadata.type_oid, metadata.collation);
                            }
                        },
                    )?;
                    result?;
                    if shape.is_none() {
                        let oid =
                            crate::sql::exec::infer_routine_argument_oid(expression, &columns)?;
                        return Err(crate::sql::exec::not_composite(
                            "*",
                            ColType::from_oid(oid).map_or("unknown", ColType::name),
                        ));
                    }
                }
            }
        }
        Ok(())
    })();
    // Visitors receive copied type/collation facts; retained casts use the persistent tail.
    // Drop schema owners before releasing their temporary definitions and syntax.
    unsafe { arena.rewind_to(mark) };
    result
}

fn geometry_assignment_type(ctype: ColType) -> Option<ColType> {
    match ctype {
        ColType::Geometry(_) => Some(ctype),
        ColType::Array(element) => match element.to_coltype() {
            ColType::Geometry(kind) => {
                Some(ColType::Array(crate::sql::types::ArrElem::Geometry(kind)))
            }
            _ => None,
        },
        _ => None,
    }
}

fn validate_assignment_expression(
    expression: &Expr<'_>,
    column: &crate::storage::ColumnMeta,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    if matches!(expression, Expr::DefaultMarker) || geometry_assignment_type(column.ctype).is_none()
    {
        return Ok(());
    }
    validate_assignment_type(
        crate::sql::exec::infer_routine_argument_oid(expression, columns)?,
        column,
        storage,
        txid,
    )
}

fn validate_assignment_type(
    source_oid: i32,
    column: &crate::storage::ColumnMeta,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    let Some(target) = geometry_assignment_type(column.ctype) else {
        return Ok(());
    };
    let target_oid = storage
        .routine_type_oid(column.ctype, column.user_type, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "column type identity is unavailable"
            )
        })?;
    if source_oid == target_oid {
        return Ok(());
    }
    if let Some((_, cast)) =
        crate::sql::catalog::defined_cast_path(storage, txid, source_oid, target_oid)
    {
        return crate::sql::catalog::require_cast_context(
            storage,
            txid,
            source_oid,
            target_oid,
            cast.context,
            crate::sql::eval::DefinedCastContext::Assignment,
        );
    }
    let Some(source) = storage.routine_result_for_oid(source_oid, txid) else {
        return Ok(());
    };
    let source = match source.ctype {
        ColType::Array(element) => {
            geometry_assignment_type(ColType::Array(element)).unwrap_or(source.ctype)
        }
        ctype => ctype,
    };
    crate::sql::eval::validate_geometry_cast(
        source.oid(),
        target,
        crate::sql::eval::DefinedCastContext::Assignment,
    )
}

pub(super) fn dml_dependency_scope<'a>(
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
        func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
        rows_from: None,
        col_alias: None,
        inheritance: RelationInheritance::Descendants,
        sample: None,
        cte: None,
        with_ordinality: false,
        lateral: false,
        authorization_role: None,
        bound_table: None,
        view_access: None,
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
    let from = super::cte::expand_from_schema(from, storage, txid, arena)?;
    super::QueryScope::resolve_schema(storage, from, txid, arena, None)
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
    let mark = arena.mark();
    let result = (|| {
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
            let sources =
                core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table));
            let mut all_tables = true;
            for source in sources {
                if source.subquery.is_some()
                    || source.is_function_source()
                    || (source.schema.is_none() && visible_ctes.contains(source.table))
                    || !matches!(
                        context.resolve_relation(storage, txid, source.schema, source.table)?,
                        Some(ResolvedRelation::Table(_))
                    )
                {
                    all_tables = false;
                    break;
                }
            }
            if all_tables {
                let bound = context.bound_from(&from, storage, txid, visible_ctes, arena)?;
                let scope = super::QueryScope::resolve_schema(storage, &bound, txid, arena, None)?;
                for item in select.items {
                    match item {
                        SelectItem::Expr { expression, .. }
                        | SelectItem::RecordStar(expression) => {
                            record_column_references(expression, &scope, dependencies, context)?;
                        }
                        SelectItem::Wildcard => {
                            for table in 0..scope.n {
                                for column in 0..scope.defs.get(table).expect("resolved").n_columns
                                {
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
            record_relation_column_references(
                storage,
                txid,
                context,
                visible_ctes,
                &from,
                select,
                dependencies,
                arena,
            )?;
        }
        collect_routine_dependencies(
            select,
            storage,
            txid,
            dependencies,
            arena,
            context,
            visible_ctes,
        )?;
        Ok(())
    })();
    // Dependency entries own copied identities and names. Schema descriptions,
    // column-name copies and expression-discovery scratch end with this visit.
    unsafe { arena.rewind_to(mark) };
    result
}

struct DependencyTypes<'scope, 'definition, 'storage> {
    scope: Option<&'scope super::QueryScope<'definition>>,
    excluded: Option<crate::storage::RelationDefinition<'storage>>,
    transition: Option<&'scope dyn ColTypeResolver>,
    storage: &'storage Storage,
    txid: u32,
    returning_output: Option<(
        crate::storage::RelationDefinition<'storage>,
        [&'definition str; 2],
    )>,
    arena: &'storage Arena,
}

impl<'a> crate::sql::eval::ColumnLookup<'a> for DependencyTypes<'_, '_, '_> {
    fn lookup(&self, _: Option<&str>, _: &str) -> Result<crate::sql::types::Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "schema analysis cannot read row values"
        ))
    }
    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        self.resolve(qualifier, name)
    }
    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.resolve(qualifier, name).ok()
    }
    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.column_meta(qualifier, name)
            .map_or(crate::sql::ast::Collation::None, |metadata| {
                metadata.collation
            })
    }
    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        self.storage
            .routine_result_for_oid(self.column_meta(qualifier, name)?.type_oid, self.txid)?
            .user_type
    }
    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> crate::sql::ast::Collation {
        crate::sql::exec::record_field_metadata(base, field, self)
            .map_or(crate::sql::ast::Collation::None, |metadata| {
                metadata.collation
            })
    }
}

impl ColTypeResolver for DependencyTypes<'_, '_, '_> {
    fn subquery_type(&self, expression: &Expr<'_>) -> Result<Option<(i32, i16)>, SqlError> {
        let mark = self.arena.mark();
        let result =
            super::subquery_expression_type(expression, self.storage, self.txid, self.arena, self);
        // Only the declared OID escapes; nested schema descriptions are scratch.
        unsafe { self.arena.rewind_to(mark) };
        result.map(|oid| oid.map(|oid| (oid, ColType::from_oid(oid).map_or(-1, ColType::typlen))))
    }

    fn validate_enum_input_literal(&self, text: &str, target_oid: i32) -> Result<(), SqlError> {
        crate::sql::catalog::validate_enum_input_literal(
            self.storage,
            self.txid,
            text,
            target_oid,
            self.arena,
        )
    }

    fn domain_base_type(&self, type_oid: i32) -> Option<ColType> {
        self.storage.domain_base_type_for_oid(type_oid, self.txid)
    }

    fn validate_explicit_cast(
        &self,
        source_oid: i32,
        target_oid: i32,
        target: ColType,
    ) -> Result<(), SqlError> {
        crate::sql::catalog::validate_explicit_cast(
            self.storage,
            self.txid,
            source_oid,
            target_oid,
            target,
        )
    }

    fn type_name_fixed(&self, oid: i32) -> Result<Option<crate::util::StackStr<260>>, SqlError> {
        Ok(
            crate::sql::catalog::user_type_name_fixed(self.storage, self.txid, oid)?
                .or_else(|| crate::sql::exec::builtin_type_name_fixed(oid)),
        )
    }

    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        if let Some((definition, aliases)) = self.returning_output
            && qualifier.is_some_and(|qualifier| aliases.contains(&qualifier))
        {
            return definition
                .column_index(name)
                .map(|index| definition.columns()[index].ctype)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" does not exist",
                        name
                    )
                });
        }
        if qualifier.is_some_and(|qualifier| {
            qualifier.eq_ignore_ascii_case("old") || qualifier.eq_ignore_ascii_case("new")
        }) && let Some(transition) = self.transition
        {
            return transition.resolve(qualifier, name);
        }
        if qualifier == Some("excluded")
            && let Some(definition) = self.excluded
        {
            return definition
                .column_index(name)
                .map(|index| definition.columns()[index].ctype)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column excluded.{} does not exist",
                        name
                    )
                });
        }
        let resolved = match self.scope {
            Some(scope) => super::ScopeCols(scope).resolve(qualifier, name),
            None => crate::sql::exec::NoCols.resolve(qualifier, name),
        };
        if resolved.as_ref().is_err_and(|error| {
            error.sqlstate == sqlstate::UNDEFINED_COLUMN
                || error.sqlstate == sqlstate::UNDEFINED_TABLE
        }) && let Some(metadata) = self
            .transition
            .and_then(|columns| columns.column_meta(qualifier, name))
        {
            return Ok(metadata.ctype);
        }
        resolved
    }

    fn column_meta(&self, qualifier: Option<&str>, name: &str) -> Option<StaticTypeMeta> {
        if let Some((definition, aliases)) = self.returning_output
            && qualifier.is_some_and(|qualifier| aliases.contains(&qualifier))
        {
            let column = definition.columns().get(definition.column_index(name)?)?;
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
        let scoped = self.scope.and_then(|scope| {
            ColTypeResolver::column_meta(
                &super::CatalogScopeCols {
                    scope,
                    outer_scope: None,
                    storage: self.storage,
                    txid: self.txid,
                    arena: self.arena,
                },
                qualifier,
                name,
            )
        });
        scoped.or_else(|| {
            self.transition
                .and_then(|columns| columns.column_meta(qualifier, name))
        })
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        crate::sql::catalog::user_type_oid(self.storage, self.txid, type_name)
            .or_else(|| ColType::from_sql_name(type_name).map(ColType::oid))
    }

    fn routine_call_selection(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<crate::storage::RoutineCallSelection> {
        use crate::storage::RoutineCallSelection;
        Some(if kind == crate::sql::ast::CallKind::SqlSyntax {
            RoutineCallSelection::Absent
        } else if let Some(oid) = bound_oid {
            match self.storage.routine_slot_by_oid(oid, self.txid) {
                Some(slot) => RoutineCallSelection::Stored(slot),
                None => RoutineCallSelection::Absent,
            }
        } else {
            self.storage.select_routine_call(
                name,
                argument_names,
                arguments,
                variadic,
                self.txid,
                crate::storage::RoutineCallKind::Function,
            )
        })
    }

    fn routine_result(
        &self,
        name: &str,
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<StaticTypeMeta> {
        if kind == crate::sql::ast::CallKind::SqlSyntax {
            return None;
        }
        let routine = if let Some(oid) = bound_oid {
            self.storage.function_for_captured_call(
                oid,
                argument_names,
                arguments,
                variadic,
                self.txid,
            )?
        } else {
            let crate::storage::RoutineCallSelection::Stored(slot) =
                self.storage.select_routine_call(
                    name,
                    argument_names,
                    arguments,
                    variadic,
                    self.txid,
                    crate::storage::RoutineCallKind::Function,
                )
            else {
                return None;
            };
            let oid = crate::storage::routine_oid(&self.storage.routine_for(slot, self.txid));
            self.storage.function_for_captured_call(
                oid,
                argument_names,
                arguments,
                variadic,
                self.txid,
            )?
        };
        let ctype = routine.function_result_contract()?.ctype;
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
        kind: crate::sql::ast::CallKind,
        bound_oid: Option<i32>,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        if kind == crate::sql::ast::CallKind::SqlSyntax {
            return None;
        }
        let slot = if let Some(oid) = bound_oid {
            self.storage.routine_slot_by_oid(oid, self.txid)?
        } else {
            match self.storage.select_routine_call(
                name,
                argument_names,
                arguments,
                variadic,
                self.txid,
                crate::storage::RoutineCallKind::Function,
            ) {
                crate::storage::RoutineCallSelection::Stored(slot) => slot,
                crate::storage::RoutineCallSelection::Builtin { .. } => {
                    return intrinsic_record_field_meta(name, arguments, index);
                }
                crate::storage::RoutineCallSelection::Absent
                | crate::storage::RoutineCallSelection::Ambiguous
                | crate::storage::RoutineCallSelection::WrongKind { .. } => return None,
            }
        };
        crate::sql::exec::catalog_routine_record_field(
            self.storage,
            self.txid,
            slot,
            argument_names,
            variadic,
            arguments,
            index,
        )
    }

    fn visit_named_composite_fields(
        &self,
        type_name: &str,
        visit: &mut dyn FnMut(&str, crate::sql::exec::StaticTypeMeta),
    ) -> Result<Option<usize>, SqlError> {
        crate::sql::exec::visit_catalog_rowtype_fields(
            self.storage,
            self.txid,
            type_name,
            self.arena,
            visit,
        )
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
        Some(scope.defs.get(table)?.columns())
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
                Some(scope.defs.get(table)?.columns[column].type_mod)
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
    ctes: CteNames<'_>,
) -> Result<(), SqlError> {
    let resolver = DependencyTypes {
        scope: None,
        excluded: None,
        transition: context.transition,
        storage,
        txid,
        returning_output: None,
        arena,
    };
    let unresolved =
        collect_routine_dependencies_with_resolver(select, storage, txid, dependencies, &resolver)?;
    if let Some(error) = unresolved {
        let Some(from) = select.from.as_ref() else {
            return Err(error);
        };
        let mark = arena.mark();
        let result = (|| {
            let bound = context.bound_from(from, storage, txid, ctes, arena)?;
            let scope = super::QueryScope::resolve_schema(storage, &bound, txid, arena, None)?;
            let resolver = DependencyTypes {
                scope: Some(&scope),
                excluded: None,
                transition: context.transition,
                storage,
                txid,
                returning_output: None,
                arena,
            };
            if let Some(error) = collect_routine_dependencies_with_resolver(
                select,
                storage,
                txid,
                dependencies,
                &resolver,
            )? {
                return Err(error);
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

#[expect(
    clippy::too_many_arguments,
    reason = "stored query analysis keeps its arena and captured identities explicit"
)]
pub(super) fn stored_routine_dependency_for_call(
    name: &str,
    args: &[&Expr<'_>],
    argument_names: &[Option<&str>],
    variadic: bool,
    storage: &Storage,
    txid: u32,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &Arena,
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
    if candidates.next().is_none() {
        return Ok(Some(first));
    }
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
        returning_output: None,
        arena,
    };
    let mut argument_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
    for (output, argument) in argument_oids.iter_mut().zip(args.iter().copied()) {
        *output = match crate::sql::exec::infer_routine_argument_oid(argument, &resolver) {
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
    if let Some(slot) = storage.captured_routine_slot_for_call(
        dependencies,
        name,
        argument_names,
        argument_oids,
        variadic,
        txid,
    ) {
        return Ok(dependencies.entries().iter().copied().find(|dependency| {
            dependency.class == DependencyClass::Routine
                && usize::from(dependency.slot) == slot
                && dependency.referenced_schema.as_str() == referenced_schema
                && dependency.referenced_name.as_str() == referenced_name
        }));
    }
    Err(sql_err!(
        sqlstate::INVALID_FUNCTION_DEFINITION,
        "stored overloaded routine call \"{}\" no longer has its captured signature",
        name
    ))
}

/// Binds an explicit stored operator call by its captured catalog identity.
pub(super) fn stored_operator_dependency_for_call(
    name: &str,
    args: &[&Expr<'_>],
    storage: &Storage,
    txid: u32,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &Arena,
) -> Result<Option<StoredQueryDependency>, SqlError> {
    let Some((schema, operator_name)) = crate::sql::ast::catalog_operator_call(name) else {
        return Ok(None);
    };
    let mut candidates = dependencies.entries().iter().copied().filter(|dependency| {
        dependency.class == DependencyClass::Operator
            && dependency.referenced_schema.as_str() == schema.unwrap_or("")
            && dependency.referenced_name.as_str() == operator_name
    });
    let Some(first) = candidates.next() else {
        return Ok(None);
    };
    let Some(second) = candidates.next() else {
        return Ok(Some(first));
    };
    if !(1..=2).contains(&args.len()) {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "stored operator call has invalid arity"
        ));
    }
    let resolver = DependencyTypes {
        scope: None,
        excluded: None,
        transition: None,
        storage,
        txid,
        returning_output: None,
        arena,
    };
    let mut argument_oids = [0_i32; 2];
    for (output, argument) in argument_oids.iter_mut().zip(args.iter().copied()) {
        *output = match crate::sql::exec::infer_routine_argument_oid(argument, &resolver) {
            Ok(crate::sql::types::oid::UNKNOWN) if matches!(argument, Expr::Str(_)) => {
                ColType::Text.oid()
            }
            Ok(oid) => oid,
            Err(_) => {
                return Err(sql_err!(
                    sqlstate::INVALID_FUNCTION_DEFINITION,
                    "stored operator call \"{}\" cannot be rebound without typed operands",
                    operator_name
                ));
            }
        };
    }
    let (left_oid, right_oid) = if args.len() == 1 {
        (None, Some(argument_oids[0]))
    } else {
        (Some(argument_oids[0]), Some(argument_oids[1]))
    };
    for dependency in core::iter::once(first)
        .chain(core::iter::once(second))
        .chain(candidates)
    {
        let definition = storage.operator_for(dependency.slot as usize, txid);
        let resolved = storage.operator_slot_for_oids(
            Some(definition.schema.as_str()),
            definition.name.as_str(),
            left_oid,
            right_oid,
            txid,
        )?;
        if resolved == Some(dependency.slot as usize) {
            return Ok(Some(dependency));
        }
    }
    Err(sql_err!(
        sqlstate::INVALID_FUNCTION_DEFINITION,
        "stored operator call \"{}\" no longer has its captured signature",
        operator_name
    ))
}

fn record_support_routine(
    oid: i32,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let slot = storage.routine_slot_by_oid(oid, txid).ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "selected support function does not exist"
        )
    })?;
    let routine = storage.routine_for(slot, txid);
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::Routine,
        slot: slot as u16,
        identity: StoredDependencyIdentity::RoutineOid(oid),
        referenced_columns: crate::storage::ColumnSet::EMPTY,
        schema: routine.schema_for(txid),
        name: routine.name_for(txid),
        referenced_schema: routine.schema_for(txid),
        referenced_name: routine.name_for(txid),
    })
}

fn record_cast_support(
    source_oid: i32,
    target_oid: i32,
    context: crate::sql::eval::DefinedCastContext,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    if source_oid == target_oid {
        return Ok(());
    }
    let Some((_, cast)) =
        crate::sql::catalog::defined_cast_path(storage, txid, source_oid, target_oid)
    else {
        return Ok(());
    };
    crate::sql::catalog::require_cast_context(
        storage,
        txid,
        source_oid,
        target_oid,
        cast.context,
        context,
    )?;
    if let crate::storage::CastMethod::Function(oid) = cast.method {
        record_support_routine(oid, storage, txid, dependencies)?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "routine dependency binding carries call syntax and catalog context"
)]
fn record_routine_call(
    kind: crate::sql::ast::CallKind,
    name: &str,
    args: &[&Expr<'_>],
    argument_names: &[Option<&str>],
    variadic: bool,
    storage: &Storage,
    txid: u32,
    resolver: &DependencyTypes<'_, '_, '_>,
    dependencies: &mut StoredQueryDependencies,
    unresolved: &mut Option<SqlError>,
) -> Result<(), SqlError> {
    if kind == crate::sql::ast::CallKind::SqlSyntax {
        return Ok(());
    }
    if !storage.has_callable_candidate(name, args.len(), txid) {
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
            Ok(oid) => *slot = oid,
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                if unresolved.is_none() {
                    *unresolved = Some(error);
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        }
    }
    let argument_oids = &argument_oids[..args.len()];
    let slot = match storage.select_routine_call(
        name,
        argument_names,
        argument_oids,
        variadic,
        txid,
        crate::storage::RoutineCallKind::Function,
    ) {
        crate::storage::RoutineCallSelection::Stored(slot) => slot,
        crate::storage::RoutineCallSelection::Builtin { .. } => return Ok(()),
        crate::storage::RoutineCallSelection::Ambiguous => {
            return Err(sql_err!(
                sqlstate::AMBIGUOUS_FUNCTION,
                "function {} is not unique",
                name
            ));
        }
        crate::storage::RoutineCallSelection::WrongKind { is_procedure } => {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "{} {} cannot be used in a function call",
                if is_procedure { "procedure" } else { "routine" },
                name
            ));
        }
        crate::storage::RoutineCallSelection::Absent => {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "function {} does not exist for these argument types",
                name
            ));
        }
    };
    let routine = storage.routine_for(slot, txid);
    let mut bound = routine;
    let mapping = storage
        .bind_routine_call_inputs(&mut bound, argument_names, argument_oids, variadic, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "selected routine input types are unavailable"
            )
        })?;
    for (index, source) in argument_oids.iter().enumerate() {
        let input = usize::from(mapping[index]);
        let argument = bound.arguments[input];
        let expanded = !variadic
            && bound.parameter_for_input(input).is_some_and(|parameter| {
                matches!(
                    parameter.mode,
                    crate::storage::RoutineParameterMode::Variadic { .. }
                )
            });
        let target = if expanded {
            match argument.ctype {
                ColType::Array(element) => element.element_oid(),
                _ => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "variadic function input is not an array"
                    ));
                }
            }
        } else {
            storage
                .routine_type_oid(argument.ctype, argument.user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "selected function input type is unavailable"
                    )
                })?
        };
        record_cast_support(
            *source,
            target,
            crate::sql::eval::DefinedCastContext::Implicit,
            storage,
            txid,
            dependencies,
        )?;
    }
    let (referenced_schema, referenced_name) = name
        .split_once('.')
        .map_or(("", name), |(schema, name)| (schema, name));
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::Routine,
        slot: slot as u16,
        identity: StoredDependencyIdentity::RoutineOid(crate::storage::routine_oid(&routine)),
        referenced_columns: crate::storage::ColumnSet::EMPTY,
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
    unresolved: &mut Option<SqlError>,
) -> Result<(), SqlError> {
    let crate::sql::ast::QualName { schema, name } = identity;
    let has_catalog_candidate = storage.operators_visible_to(txid).any(|(_, operator)| {
        operator.name.as_str() == name
            && schema.map_or_else(
                || storage.schema_is_on_path(operator.schema, txid),
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
        *slot = match infer(operand) {
            Ok(oid) => oid,
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                if unresolved.is_none() {
                    *unresolved = Some(error);
                }
                return Ok(());
            }
            Err(error) => return Err(error),
        };
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
    if let Some(oid) = operator.implementation.routine() {
        record_support_routine(oid, storage, txid, dependencies)?;
    }
    let expected = [operator.signature.left, operator.signature.right];
    for (source, expected) in inferred.iter().zip(expected.iter().flatten()) {
        let target = storage
            .routine_type_oid(expected.ctype, expected.user_type, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "selected operator input type is unavailable"
                )
            })?;
        record_cast_support(
            *source,
            target,
            crate::sql::eval::DefinedCastContext::Implicit,
            storage,
            txid,
            dependencies,
        )?;
    }
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::Operator,
        slot: slot as u16,
        identity: StoredDependencyIdentity::OperatorOid(storage.operator(slot).oid()),
        referenced_columns: crate::storage::ColumnSet::EMPTY,
        schema: operator.schema,
        name: operator.name,
        referenced_schema: SqlName::parse(schema.unwrap_or(""))?,
        referenced_name: SqlName::parse(name)?,
    })
}

fn visit_routine_expression(
    expression: &Expr<'_>,
    storage: &Storage,
    txid: u32,
    resolver: &DependencyTypes<'_, '_, '_>,
    dependencies: &mut StoredQueryDependencies,
    unresolved: &mut Option<SqlError>,
) -> Result<(), SqlError> {
    if let Expr::Cast {
        operand,
        type_name,
        binding: crate::sql::ast::CastBinding::Resolve,
        ..
    } = expression
    {
        let target = ColType::from_sql_name(type_name)
            .map(ColType::oid)
            .or_else(|| resolver.named_type_oid(type_name));
        let array_target = target.and_then(|target| {
            crate::sql::eval::constructor_type_from_oid(target, &|oid| {
                resolver.domain_base_type(oid)
            })
        });
        if let (Expr::Array(_), Some(ColType::Array(element))) = (*operand, array_target) {
            match crate::sql::exec::visit_array_constructor_conversions(
                operand,
                element,
                resolver,
                &mut |source, target| {
                    record_cast_support(
                        source,
                        target,
                        crate::sql::eval::DefinedCastContext::Explicit,
                        storage,
                        txid,
                        dependencies,
                    )
                },
            ) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.sqlstate.as_str(),
                        sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                    ) =>
                {
                    if unresolved.is_none() {
                        *unresolved = Some(error);
                    }
                }
                Err(error) => return Err(error),
            }
        } else {
            let source = match crate::sql::exec::infer_routine_argument_oid(operand, resolver) {
                Ok(source) => Some(source),
                Err(error)
                    if matches!(
                        error.sqlstate.as_str(),
                        sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                    ) =>
                {
                    if unresolved.is_none() {
                        *unresolved = Some(error);
                    }
                    None
                }
                Err(error) => return Err(error),
            };
            if let (Some(source), Some(target)) = (source, target) {
                record_cast_support(
                    source,
                    target,
                    crate::sql::eval::DefinedCastContext::Explicit,
                    storage,
                    txid,
                    dependencies,
                )?;
            }
        }
    }
    if let Expr::Call {
        kind,
        name,
        args,
        argument_names,
        variadic,
        ..
    } = expression
    {
        if let Some((schema, operator)) = (*kind == crate::sql::ast::CallKind::SqlSyntax)
            .then(|| crate::sql::ast::catalog_operator_call(name))
            .flatten()
        {
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
                    unresolved,
                )?;
            }
        } else {
            record_routine_call(
                *kind,
                name,
                args,
                argument_names,
                *variadic,
                storage,
                txid,
                resolver,
                dependencies,
                unresolved,
            )?;
        }
    }
    if let Expr::Binary {
        operator,
        left,
        right,
        ..
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
            unresolved,
        )?;
    }
    super::walk_children(expression, &mut |child| {
        visit_routine_expression(child, storage, txid, resolver, dependencies, unresolved)
    })
}

fn collect_routine_dependencies_with_resolver(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    resolver: &DependencyTypes<'_, '_, '_>,
) -> Result<Option<SqlError>, SqlError> {
    let mut unresolved = None;

    fn record_table_call(
        table: &TableRef<'_>,
        args: &[&Expr<'_>],
        storage: &Storage,
        txid: u32,
        resolver: &DependencyTypes<'_, '_, '_>,
        dependencies: &mut StoredQueryDependencies,
        unresolved: &mut Option<SqlError>,
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
            crate::sql::ast::CallKind::Routine,
            name,
            args,
            table.func_argument_names,
            table.func_variadic,
            storage,
            txid,
            resolver,
            dependencies,
            unresolved,
        )
    }

    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                visit_routine_expression(
                    expression,
                    storage,
                    txid,
                    resolver,
                    dependencies,
                    &mut unresolved,
                )?
            }
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
        visit_routine_expression(
            expression_node,
            storage,
            txid,
            resolver,
            dependencies,
            &mut unresolved,
        )?;
    }
    if let Some(from) = select.from {
        for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
            if let Some(sample) = table.sample {
                visit_routine_expression(
                    sample.percentage,
                    storage,
                    txid,
                    resolver,
                    dependencies,
                    &mut unresolved,
                )?;
                if let Some(repeatable) = sample.repeatable {
                    visit_routine_expression(
                        repeatable,
                        storage,
                        txid,
                        resolver,
                        dependencies,
                        &mut unresolved,
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
                    &mut unresolved,
                )?;
                for argument in args {
                    visit_routine_expression(
                        argument,
                        storage,
                        txid,
                        resolver,
                        dependencies,
                        &mut unresolved,
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
                            &mut unresolved,
                        )?;
                        for argument in args {
                            visit_routine_expression(
                                argument,
                                storage,
                                txid,
                                resolver,
                                dependencies,
                                &mut unresolved,
                            )?;
                        }
                    }
                }
            }
        }
        for join in from.joins {
            if let Some(on) = join.on {
                visit_routine_expression(
                    on,
                    storage,
                    txid,
                    resolver,
                    dependencies,
                    &mut unresolved,
                )?;
            }
        }
    }
    Ok(unresolved)
}

#[derive(Clone, Copy)]
struct RelationSource<'a> {
    class: DependencyClass,
    slot: usize,
    exposed: &'a str,
    columns: [&'a str; MAX_COLUMNS],
    n_columns: usize,
}

#[expect(
    clippy::too_many_arguments,
    reason = "selected sources, CTE names and captured bindings are explicit"
)]
fn record_relation_column_references<'a>(
    storage: &Storage,
    txid: u32,
    context: CollectionContext<'_>,
    ctes: CteNames<'_>,
    from: &'a crate::sql::ast::FromClause<'a>,
    select: &'a Select<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let sources = arena
        .alloc_slice_with(from.joins.len() + 1, |_| RelationSource {
            class: DependencyClass::Table,
            slot: 0,
            exposed: "",
            columns: [""; MAX_COLUMNS],
            n_columns: 0,
        })
        .map_err(|_| arena_full())?;
    let mut n_sources = 0;
    for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
        if table.subquery.is_some()
            || table.is_function_source()
            || (table.schema.is_none() && ctes.contains(table.table))
        {
            continue;
        }
        let Some(relation) = context.resolve_relation(storage, txid, table.schema, table.table)?
        else {
            continue;
        };
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
                        .map_or_else(
                            || {
                                arena
                                    .alloc_str(definition.columns[column].name.as_str())
                                    .map_err(|_| arena_full())
                            },
                            Ok,
                        )?;
                }
            }
            ResolvedRelation::View(slot) => {
                source.class = DependencyClass::View;
                source.slot = slot;
                let view_image = storage.view(slot);
                let columns = view_image.columns_for(txid);
                source.n_columns = columns.len();
                for (column, name) in columns.names().iter().enumerate() {
                    source.columns[column] = table
                        .col_alias
                        .and_then(|aliases| aliases.get(column).copied())
                        .map_or_else(
                            || arena.alloc_str(name.as_str()).map_err(|_| arena_full()),
                            Ok,
                        )?;
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
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) && context.outer_column(qualifier, name) =>
            {
                return;
            }
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
    let mut mark = |table: usize, column| {
        let (class, slot) = match scope.view_accesses[table] {
            Some(view) => (DependencyClass::View, usize::from(view)),
            None => (DependencyClass::Table, scope.slots[table]),
        };
        dependencies.mark_referenced_column(class, slot, column)
    };
    match resolved {
        super::ResolvedColumn::Table(table, column) => mark(table, column),
        super::ResolvedColumn::Merged(merged) => {
            for &(table, column) in scope.merged[merged].parts {
                mark(table, column)?;
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
    if let Some(select) = table.subquery {
        collect_select(select, storage, txid, ctes, dependencies, arena, context)?;
    } else if !table.is_function_source()
        && table.cte.is_none()
        && !(table.schema.is_none() && ctes.contains(table.table))
    {
        match context.resolve_relation(storage, txid, table.schema, table.table)? {
            Some(ResolvedRelation::Table(slot)) => {
                let identity =
                    storage.capture_stored_query_relation(ResolvedRelation::Table(slot), txid)?;
                let definition = storage.table_def(slot, txid);
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::Table,
                    slot: slot as u16,
                    identity,
                    referenced_columns: crate::storage::ColumnSet::EMPTY,
                    schema: definition.schema,
                    name: definition.name,
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::View(slot)) => {
                let identity =
                    storage.capture_stored_query_relation(ResolvedRelation::View(slot), txid)?;
                let view = storage.view(slot);
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::View,
                    slot: slot as u16,
                    identity,
                    referenced_columns: crate::storage::ColumnSet::EMPTY,
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
    if let Expr::BoundLiteral {
        source, type_oid, ..
    } = expression
    {
        collect_type_oid(*type_oid, source, storage, txid, dependencies, arena)?;
        return Ok(());
    }
    if let Expr::Cast { type_name, .. } = expression {
        collect_type(type_name, storage, txid, path, dependencies)?;
    }
    if let Expr::Cast {
        operand,
        type_name: "regcollation",
        ..
    } = expression
        && let Some(name) = regclass_literal(operand)
    {
        collect_regcollation(name, storage, txid, dependencies)?;
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
                PathEntry::Catalog | PathEntry::InformationSchema | PathEntry::Toast => None,
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
            identity: StoredDependencyIdentity::Collation(storage.collation(slot).binding()),
            referenced_columns: crate::storage::ColumnSet::EMPTY,
            schema: definition.schema,
            name: definition.name,
            referenced_schema: SqlName::parse(name.schema.unwrap_or(""))?,
            referenced_name: SqlName::parse(name.name)?,
        })?;
    }
    if let Expr::Call {
        kind: crate::sql::ast::CallKind::Routine,
        name,
        args,
        ..
    } = expression
        && let Some(sequence_name) = bound_sequence_literal(name, args)
    {
        collect_sequence(sequence_name, storage, txid, path, dependencies)?;
    }
    if let Expr::Call {
        kind: crate::sql::ast::CallKind::Routine,
        name,
        args,
        ..
    } = expression
        && let Some(configuration_name) = text_search_config_literal(name, args)
    {
        collect_text_search_configuration(configuration_name, storage, txid, dependencies)?;
    }
    // SQL/JSON typed table sources retain their output definitions in
    // synthetic calls. Those names are every bit as durable as an ordinary
    // cast and must therefore participate in DROP/rename/rebind dependency
    // handling.
    if let Expr::Call {
        kind: crate::sql::ast::CallKind::SqlSyntax,
        name,
        args,
        ..
    } = expression
        && matches!(
            *name,
            "__json_table_value" | "__json_table_exists" | "__json_record_column"
        )
        && let Some(Expr::Str(type_name)) = args.get(1).copied()
    {
        collect_type(type_name, storage, txid, path, dependencies)?;
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
        Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. } => child(operand),
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

fn collect_type_oid(
    type_oid: i32,
    source: &Expr<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
) -> Result<(), SqlError> {
    let Some((kind, creation, _)) = crate::storage::NamedTypeKind::from_oid(type_oid) else {
        return Ok(());
    };
    let crate::storage::TypeDefinitionTarget::Bound(reference) =
        crate::storage::TypeDefinitionTarget::by_creation(storage, kind, creation)?
    else {
        unreachable!("creation lookup returns a captured source");
    };
    let written = if let Expr::Cast { type_name, .. } = source {
        let bare = type_name.strip_suffix("[]").unwrap_or(type_name);
        let (schema, name) = bare.split_once('.').unwrap_or(("", bare));
        Some((SqlName::parse(schema)?, SqlName::parse(name)?))
    } else {
        None
    };
    let mut pending = crate::mem::arena::ArenaList::new(arena);
    pending.push(reference).map_err(|_| arena_full())?;
    let mut index = 0;
    while index < pending.len() {
        let reference = pending.as_slice()[index];
        index += 1;
        let identity = StoredDependencyIdentity::Type(reference);
        let target = crate::storage::TypeDefinitionTarget::Bound(reference);
        let mut queue_child = |name: crate::storage::UserTypeName| -> Result<(), SqlError> {
            let child = crate::storage::TypeReference::from_bound_name(name)?;
            if !pending.as_slice().contains(&child) {
                pending.push(child).map_err(|_| arena_full())?;
            }
            Ok(())
        };
        let (class, schema, name) = match reference.kind() {
            crate::storage::NamedTypeKind::Domain => {
                let definition =
                    storage
                        .read_domain_definition(target, txid)?
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_OBJECT,
                                "captured domain input type no longer exists"
                            )
                        })?;
                for child in [definition.base_domain, definition.base_user_type]
                    .into_iter()
                    .flatten()
                {
                    queue_child(child)?;
                }
                (DependencyClass::Domain, definition.schema, definition.name)
            }
            crate::storage::NamedTypeKind::Enum => {
                let definition = storage.read_enum_definition(target, txid)?.ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_OBJECT,
                        "captured enum input type no longer exists"
                    )
                })?;
                (DependencyClass::Enum, definition.schema, definition.name)
            }
            crate::storage::NamedTypeKind::Composite => {
                let definition = storage
                    .read_composite_definition(target, txid)?
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "captured record input type no longer exists"
                        )
                    })?;
                for field in definition.fields() {
                    if let Some(child) = field.user_type {
                        queue_child(child)?;
                    }
                }
                (
                    DependencyClass::Composite,
                    definition.schema,
                    definition.name,
                )
            }
        };
        let (referenced_schema, referenced_name) =
            if reference.creation() == creation && reference.kind() == kind {
                written.unwrap_or((schema, name))
            } else {
                (schema, name)
            };
        if !dependencies.entries().iter().any(|dependency| {
            dependency.identity == identity
                && dependency.referenced_schema == referenced_schema
                && dependency.referenced_name == referenced_name
        }) {
            dependencies.push(StoredQueryDependency {
                class,
                slot: reference.catalog_slot(),
                identity,
                referenced_columns: crate::storage::ColumnSet::EMPTY,
                schema,
                name,
                referenced_schema,
                referenced_name,
            })?;
        }
    }
    Ok(())
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
            identity: storage.capture_type_dependency(class, slot, txid)?,
            referenced_columns: crate::storage::ColumnSet::EMPTY,
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
            PathEntry::Catalog | PathEntry::InformationSchema | PathEntry::Toast => {}
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
        Expr::Cast { operand, .. }
        | Expr::BoundDefault {
            expression: operand,
            ..
        }
        | Expr::Collate { operand, .. } => regclass_literal(operand),
        _ => None,
    }
}

fn collect_regcollation(
    written: &str,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let oid = match written.trim().parse::<i32>() {
        Ok(oid) => oid,
        Err(_) => match crate::sql::catalog::collation_oid_by_name(storage, txid, written) {
            Some(oid) => oid,
            None => return Ok(()),
        },
    };
    let Some((slot, definition)) = storage
        .collations_visible_to(txid)
        .find(|(slot, _)| storage.collation(*slot).oid() == oid)
    else {
        return Ok(());
    };
    let Some((referenced_schema, referenced_name)) =
        crate::sql::catalog::split_qualified_name(written)
    else {
        return Ok(());
    };
    dependencies.push(StoredQueryDependency {
        class: DependencyClass::Collation,
        slot: slot as u16,
        identity: StoredDependencyIdentity::Collation(storage.collation(slot).binding()),
        referenced_columns: crate::storage::ColumnSet::EMPTY,
        schema: definition.schema,
        name: definition.name,
        referenced_schema: SqlName::parse(referenced_schema.unwrap_or(""))?,
        referenced_name: SqlName::parse(referenced_name)?,
    })
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
        identity: StoredDependencyIdentity::TextSearchConfigurationOid(definition.oid()),
        referenced_columns: crate::storage::ColumnSet::EMPTY,
        schema: definition.schema(),
        name: definition.name(),
        referenced_schema: SqlName::parse(schema.unwrap_or(""))?,
        referenced_name: SqlName::parse(name)?,
    })
}

/// A text cast requests lookup at execution time, even if another input
/// names the same sequence and has a captured dependency.
pub(super) fn bound_sequence_literal<'a>(function: &str, args: &[&'a Expr<'a>]) -> Option<&'a str> {
    if !matches!(
        function.strip_prefix("pg_catalog.").unwrap_or(function),
        "nextval" | "currval" | "setval"
    ) {
        return None;
    }
    match args.first().copied()? {
        Expr::Str(value)
        | Expr::Cast {
            operand: Expr::Str(value),
            type_name: "regclass" | "pg_catalog.regclass",
            ..
        } => Some(value),
        _ => None,
    }
}

fn collect_sequence(
    sequence_name: &str,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let (slot, referenced_schema, referenced_name) =
        match crate::sql::identifiers::ObjectReference::parse(sequence_name)? {
            crate::sql::identifiers::ObjectReference::Oid(oid) => {
                let slot = (0..storage.sequence_count()).find(|slot| {
                    storage.sequence_slot_visible_to(*slot, txid)
                        && crate::sql::catalog::sequence_oid(storage, *slot) as u32 == oid
                });
                (
                    slot,
                    SqlName::EMPTY,
                    SqlName::parse(crate::stack_format!(16, "{}", oid).as_str())?,
                )
            }
            crate::sql::identifiers::ObjectReference::Name(parsed) => {
                if parsed
                    .database
                    .is_some_and(|database| database != storage.current_database_name(txid))
                {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "cross-database sequence references are not supported"
                    ));
                }
                let name = parsed.name.as_str();
                let slot = if let Some(schema) = parsed.schema {
                    if schema.as_str() == "pg_temp" {
                        storage
                            .temporary_schema()
                            .ok()
                            .and_then(|schema| storage.sequence_slot(schema.as_str(), name, txid))
                    } else if schema.as_str().starts_with("pg_temp_")
                        && !storage.is_current_temporary_schema(schema.as_str())
                    {
                        None
                    } else {
                        storage.sequence_slot(schema.as_str(), name, txid)
                    }
                } else {
                    storage
                        .temporary_schema()
                        .ok()
                        .and_then(|schema| storage.sequence_slot(schema.as_str(), name, txid))
                        .or_else(|| {
                            path.entries().iter().find_map(|entry| match entry {
                                PathEntry::Schema(schema_slot) => {
                                    let schema =
                                        storage.schema_def_for(*schema_slot as usize, txid).name;
                                    storage.sequence_slot(schema.as_str(), name, txid)
                                }
                                PathEntry::Catalog
                                | PathEntry::InformationSchema
                                | PathEntry::Toast => None,
                            })
                        })
                };
                (slot, parsed.schema.unwrap_or(SqlName::EMPTY), parsed.name)
            }
        };
    if let Some(slot) = slot {
        let sequence = storage.sequence_for(slot, txid);
        dependencies.push(StoredQueryDependency {
            class: DependencyClass::Sequence,
            slot: slot as u16,
            identity: StoredDependencyIdentity::Name,
            referenced_columns: crate::storage::ColumnSet::EMPTY,
            schema: sequence.schema,
            name: sequence.name,
            referenced_schema,
            referenced_name,
        })?;
    }
    Ok(())
}
