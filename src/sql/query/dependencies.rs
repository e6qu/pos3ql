//! Bind catalog dependencies for stored definitions and runtime query plans.
//!
//! Views keep their SQL text for execution, but dependency decisions must not
//! reparse that text after catalog objects have moved. Definition creation and
//! runtime plan admission record the selected relations, types and sequences.
//! Executor-owned transition rows contribute their schema without acquiring
//! fictitious catalog identities.

use crate::mem::arena::Arena;
use crate::sql::ast::{
    Expr, FrameBound, FromClause, Join, JoinKind, QualName, Select, SelectItem, SetQuery, SetTree,
    Stmt, TableRef,
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
    /// The query levels enclosing a subquery, which its outer references name.
    enclosing: Option<&'a dyn EnclosingColumns>,
    /// A rewrite rule's action, whose `RETURNING` list names the rule's OLD
    /// and NEW rather than the written row's images.
    rule_action: bool,
}

/// The columns of the query levels around a subquery.
trait EnclosingColumns {
    /// Marks the enclosing column `qualifier.name` names; Ok(false) when no
    /// level resolves it.
    fn mark(
        &self,
        qualifier: Option<&str>,
        name: &str,
        dependencies: &mut StoredQueryDependencies,
    ) -> Result<bool, SqlError>;
}

/// A query level around a subquery. A level without a schema (one reading a
/// derived table, function or view) records its own references by relation,
/// so a reference that resolves nowhere may belong to it.
struct EnclosingLevel<'s, 'd, 'a> {
    scope: Option<&'s super::QueryScope<'d>>,
    parent: Option<&'s dyn EnclosingColumns>,
    /// Columns its subqueries read before the level's own relations are
    /// recorded, which the level marks once they are.
    deferred:
        core::cell::RefCell<crate::mem::arena::ArenaList<'a, (DependencyClass, usize, usize)>>,
}

impl EnclosingColumns for EnclosingLevel<'_, '_, '_> {
    fn mark(
        &self,
        qualifier: Option<&str>,
        name: &str,
        dependencies: &mut StoredQueryDependencies,
    ) -> Result<bool, SqlError> {
        let Some(scope) = self.scope else {
            return Ok(true);
        };
        if let Ok(resolved) =
            crate::sql::error_position::preserving_pending(|| scope.find_column(qualifier, name))
        {
            visit_resolved_column(scope, resolved, &mut |class, slot, column| {
                if dependencies.depends_on(class, slot) {
                    dependencies.mark_referenced_column(class, slot, column)
                } else {
                    self.deferred
                        .borrow_mut()
                        .push((class, slot, column))
                        .map_err(|_| arena_full())
                }
            })?;
            return Ok(true);
        }
        match self.parent {
            Some(parent) => parent.mark(qualifier, name, dependencies),
            None => Ok(false),
        }
    }
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
            enclosing: None,
            rule_action: false,
        }
    }

    fn with_columns(path: &'a PathContext, transition: &'a dyn ColTypeResolver) -> Self {
        Self {
            path,
            transition: Some(transition),
            bindings: None,
            enclosing: None,
            rule_action: false,
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
    /// Each visible `WITH` query, so a level reading one can describe it.
    definitions: [Option<&'a crate::sql::ast::Cte<'a>>; MAX_STORED_QUERY_DEPENDENCIES],
    len: usize,
}

impl<'a> CteNames<'a> {
    const EMPTY: Self = Self {
        names: [""; MAX_STORED_QUERY_DEPENDENCIES],
        definitions: [None; MAX_STORED_QUERY_DEPENDENCIES],
        len: 0,
    };

    fn contains(&self, name: &str) -> bool {
        self.names[..self.len].contains(&name)
    }

    fn push(&mut self, cte: &'a crate::sql::ast::Cte<'a>) -> Result<(), SqlError> {
        if self.len == self.names.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "stored query nests more than {} CTE names",
                self.names.len()
            ));
        }
        self.names[self.len] = cte.name;
        self.definitions[self.len] = Some(cte);
        self.len += 1;
        Ok(())
    }

    /// The visible `WITH` queries in order, the latest definition of a
    /// repeated name last.
    fn definitions_in(&self, arena: &'a Arena) -> Result<&'a [crate::sql::ast::Cte<'a>], SqlError> {
        let mut definitions = crate::mem::arena::ArenaList::new(arena);
        for (index, definition) in self.definitions[..self.len].iter().enumerate() {
            let Some(definition) = definition else {
                continue;
            };
            // An inner level's definition hides an outer one of the same name.
            if self.names[index + 1..self.len].contains(&definition.name) {
                continue;
            }
            definitions.push(**definition).map_err(|_| arena_full())?;
        }
        Ok(definitions.as_slice())
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
        super::RoutineFunctionResult::Forbidden(_) | super::RoutineFunctionResult::Empty => {}
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
            CollectionContext {
                rule_action: true,
                ..CollectionContext::with_columns(&path, transition)
            },
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

/// The table or view a stored `INSERT` or `UPDATE` writes.
fn written_relation(
    name: crate::sql::ast::QualName<'_>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
) -> Option<(DependencyClass, usize)> {
    match storage.resolve_relation_under(path, name.schema, name.name, txid)? {
        ResolvedRelation::Table(slot) => Some((DependencyClass::Table, slot)),
        ResolvedRelation::View(slot) => Some((DependencyClass::View, slot)),
        ResolvedRelation::Catalog => None,
    }
}

/// Records an `INSERT`'s target columns, as PostgreSQL's target list names
/// them: the column list, or the leading columns its values fill, and every
/// column for `ON CONFLICT DO UPDATE`.
#[allow(clippy::too_many_arguments)]
fn record_insert_targets(
    insert: &crate::sql::ast::Insert<'_>,
    scope: &super::QueryScope<'_>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<(), SqlError> {
    let Some((class, slot)) = written_relation(insert.table, storage, txid, path) else {
        return Ok(());
    };
    let target = scope.defs.get(0).expect("target resolved");
    let width = target.columns().len();
    if insert
        .on_conflict
        .is_some_and(|conflict| conflict.update.is_some())
    {
        for column in 0..width {
            dependencies.mark_written_column(class, slot, column)?;
        }
        return Ok(());
    }
    if !insert.columns.is_empty() {
        for name in insert.columns {
            if let Some(column) = target.column_index(name) {
                dependencies.mark_written_column(class, slot, column)?;
            }
        }
        return Ok(());
    }
    let values = match (insert.rows.first(), insert.select) {
        (Some(row), _) => row.len(),
        (None, Some(select)) => select_output_width(select, storage, txid, arena, context)?,
        (None, None) => 0,
    };
    for column in 0..values.min(width) {
        dependencies.mark_written_column(class, slot, column)?;
    }
    Ok(())
}

/// The number of output columns of a stored `SELECT` (its first set-operation
/// branch), with `*` expanded.
fn select_output_width(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    context: CollectionContext<'_>,
) -> Result<usize, SqlError> {
    if let Some(body) = select.set_body {
        let mut tree = body;
        loop {
            match tree {
                SetTree::Select(first) => {
                    return select_output_width(first, storage, txid, arena, context);
                }
                SetTree::Op { left, .. } => tree = left,
            }
        }
    }
    // The query's own WITH list names some of its FROM items.
    let select = if select.with.is_empty() {
        select
    } else {
        super::expand_ctes(select, storage, txid, arena)?
    };
    // A routine's variables are the levels around a stored body's query.
    let outer = context.transition.map(super::scope::ResolverColumns);
    let scope = select
        .from
        .as_ref()
        .map(|from| {
            super::QueryScope::resolve_schema(
                storage,
                from,
                txid,
                arena,
                outer
                    .as_ref()
                    .map(|outer| outer as &dyn crate::sql::eval::ColumnLookup<'_>),
            )
        })
        .transpose()?;
    let columns = DependencyTypes {
        hidden: None,
        scope: scope.as_ref(),
        excluded: None,
        transition: context.transition,
        storage,
        txid,
        returning_output: None,
        arena,
    };
    let mut width = 0usize;
    for item in select.items {
        width += match item {
            SelectItem::Expr { .. } => 1,
            SelectItem::Wildcard(_) => scope.as_ref().map_or(0, |scope| scope.star_columns()),
            SelectItem::TableWildcard(name) => match &scope {
                Some(scope) => scope.qualified_star_columns(name)?,
                None => 0,
            },
            SelectItem::RecordStar(expression) => {
                let mut fields = 0usize;
                crate::sql::exec::record_shape_metadata(expression, &columns, |_, _| fields += 1)?;
                fields
            }
        };
    }
    Ok(width)
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
        written_columns: crate::storage::ColumnSet::EMPTY,
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
                let references = $excluded.map_or(ReferenceScope::Query, |(slot, definition)| {
                    ReferenceScope::Excluded { slot, definition }
                });
                record_column_references($expression, scope, dependencies, context, references)?;
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
            hidden: None,
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
                let scope = dml_dependency_scope(
                    storage,
                    insert.table,
                    insert.alias,
                    None,
                    txid,
                    arena,
                    None,
                )?;
                record_insert_targets(
                    insert,
                    &scope,
                    storage,
                    txid,
                    path,
                    dependencies,
                    arena,
                    context,
                )?;
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
                    context
                        .transition
                        .map(super::scope::ResolverColumns)
                        .as_ref()
                        .map(|columns| columns as &dyn crate::sql::eval::ColumnLookup<'_>),
                )?;
                if let Some((class, slot)) = written_relation(update.table, storage, txid, path) {
                    let target = scope.defs.get(0).expect("target resolved");
                    for (name, _) in update.assignments {
                        if let Some(column) = target.column_index(name) {
                            dependencies.mark_written_column(class, slot, column)?;
                        }
                    }
                }
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
                        // In a rule action, OLD and NEW name the rule's rows.
                        if context.rule_action {
                            collect_expr!(expression, Some(&scope), no_excluded)?;
                            continue;
                        }
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
                    context
                        .transition
                        .map(super::scope::ResolverColumns)
                        .as_ref()
                        .map(|columns| columns as &dyn crate::sql::eval::ColumnLookup<'_>),
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
                        // In a rule action, OLD and NEW name the rule's rows.
                        if context.rule_action {
                            collect_expr!(expression, Some(&scope), no_excluded)?;
                            continue;
                        }
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
                    context
                        .transition
                        .map(super::scope::ResolverColumns)
                        .as_ref()
                        .map(|columns| columns as &dyn crate::sql::eval::ColumnLookup<'_>),
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
        hidden: None,
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
    record_column_references(
        expression,
        scope,
        dependencies,
        context,
        ReferenceScope::Returning {
            target: target_qualifier,
            aliases,
        },
    )?;
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

/// Check declared assignment types before row selection, triggers, or defaults
/// can run. A typed NULL and an empty array still carry a cast contract.
/// `SET (a, b) = (SELECT ...)`: the sub-SELECT must produce one column per
/// target, checked once at its first target before any row is read.
pub(super) fn check_multiple_assignment_source(
    expression: &Expr<'_>,
    scope: &super::QueryScope<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let Expr::RecordFieldIndex {
        base: base @ Expr::RowSubquery { select, arity },
        index: 0,
    } = expression
    else {
        return Ok(());
    };
    let mut columns = [crate::sql::types::ColDesc::new("", 0, 0); crate::sql::exec::MAX_PROJ];
    let width = super::describe_select(
        select,
        storage,
        txid,
        arena,
        &mut columns,
        crate::sql::exec::DescriptionTarget::Sql,
        Some(&super::ScopeSchema(scope)),
    )?;
    if width == usize::from(*arity) {
        return Ok(());
    }
    Err(crate::sql::error_position::at_node(
        sql_err!(
            sqlstate::SYNTAX_ERROR,
            "number of columns does not match number of values"
        ),
        base,
    ))
}

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
            Stmt::Delete(delete) => delete.table,
            Stmt::Merge(merge) => merge.target,
            _ => return Ok(()),
        };
        if !matches!(
            storage.resolve_relation(target.schema, target.name, txid),
            Some(ResolvedRelation::Table(_) | ResolvedRelation::View(_))
        ) {
            return Ok(());
        }
        let statement =
            super::cte::expand_dml_ctes_schema(statement, with, storage, txid, None, None, arena)?;
        let (alias, extra) = match statement {
            Stmt::Insert(insert) => (insert.alias, None),
            Stmt::Update(update) => (update.alias, update.from),
            Stmt::Delete(delete) => (delete.alias, delete.using),
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
        let scope = dml_dependency_scope(storage, target, alias, extra, txid, arena, transition)?;
        // Each subquery level converts a constant compared with its columns
        // before any row is read, as PostgreSQL's analysis does: the WHERE
        // clause first, then the assignments or inserted values.
        use super::subquery::check_subquery_literals as subquery_literals;
        let target = scope.defs.get(0).expect("assignment target resolved");
        let catalog = super::storage_catalog(storage, arena, txid);
        let transition = transition.map(|row| crate::sql::eval::RuntimeColumnTypes {
            row,
            catalog: Some(&catalog),
        });
        let columns = DependencyTypes {
            hidden: None,
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
        let assign =
            |name: &str, expression: &Expr<'_>, alias: Option<&str>| -> Result<(), SqlError> {
                let index = target
                    .column_index(name)
                    .ok_or_else(|| undefined_set_target(name, target_name, alias, expression))?;
                validate_assignment_expression(
                    expression,
                    Some(name),
                    AssignmentTarget::Update,
                    &target.columns()[index],
                    &columns,
                    storage,
                    txid,
                )?;
                assigned_literal(expression, &target.columns()[index], arena)
            };
        // An INSERT's VALUES rows read no relation, only the enclosing
        // routine's variables and trigger rows.
        let values_columns = DependencyTypes {
            scope: None,
            excluded: None,
            hidden: Some((target_name, target)),
            ..columns
        };
        let merge_columns = &columns;
        let insert_values = |names: &[&str],
                             values: &[&Expr<'_>],
                             columns: &DependencyTypes<'_, '_, '_>|
         -> Result<(), SqlError> {
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
                    validate_assignment_expression(
                        expression,
                        names.get(index).copied(),
                        AssignmentTarget::Insert,
                        column,
                        columns,
                        storage,
                        txid,
                    )?;
                    assigned_literal(expression, column, arena)?;
                }
            }
            Ok(())
        };
        match statement {
            Stmt::Insert(insert) => {
                for name in insert.columns {
                    if target.column_index(name).is_none() {
                        return Err(crate::sql::error_position::at_reference(
                            sql_err!(
                                sqlstate::UNDEFINED_COLUMN,
                                "column \"{}\" of relation \"{}\" does not exist",
                                name,
                                target_name
                            ),
                            name,
                        ));
                    }
                }
                for row in insert.rows {
                    for expression in row.iter() {
                        subquery_literals(expression, None, storage, txid, arena)?;
                    }
                    insert_values(insert.columns, row, &values_columns)?;
                }
                if let Some(select) = insert.select {
                    // A rule action's trailing VALUES row number is not assigned.
                    let assigned;
                    let select = if insert.values_defaults.is_empty() {
                        select
                    } else {
                        assigned = Select {
                            items: &select.items[..select.items.len().saturating_sub(1)],
                            ..*select
                        };
                        &assigned
                    };
                    check_insert_counts(
                        select,
                        insert.columns,
                        target.n_columns,
                        storage,
                        txid,
                        arena,
                        transition.as_ref().map(|columns| columns.row),
                    )?;
                    super::subquery::check_select_literals(select, None, storage, txid, arena)?;
                    validate_insert_projection(
                        select,
                        insert.columns,
                        target,
                        storage,
                        txid,
                        arena,
                        transition.as_ref().map(|columns| columns.row),
                    )?;
                }
                for (name, expression) in insert
                    .on_conflict
                    .and_then(|conflict| conflict.update)
                    .into_iter()
                    .flatten()
                {
                    assign(name, expression, insert.alias)?;
                }
                // A target aliased `excluded` makes the name ambiguous where
                // the proposed row is also in scope.
                if let Some(alias) = insert.alias
                    && alias == "excluded"
                    && let Some(conflict) = insert.on_conflict
                    && let Some(assignments) = conflict.update
                {
                    let mut ambiguous = None;
                    for expression in assignments
                        .iter()
                        .map(|(_, expression)| *expression)
                        .chain(conflict.update_where)
                    {
                        expression.for_each_column_reference(&mut |qualifier, _| {
                            if ambiguous.is_none()
                                && let Some(qualifier) = qualifier
                                && qualifier == "excluded"
                            {
                                ambiguous = Some(crate::sql::error_position::at_subject(
                                    sql_err!(
                                        sqlstate::AMBIGUOUS_ALIAS,
                                        "table reference \"excluded\" is ambiguous"
                                    ),
                                    qualifier,
                                ));
                            }
                        });
                    }
                    if let Some(error) = ambiguous {
                        return Err(error);
                    }
                }
            }
            Stmt::Update(update) => {
                if let Some(predicate) = update.where_clause {
                    subquery_literals(predicate, Some(&scope), storage, txid, arena)?;
                }
                for (name, expression) in update.assignments {
                    check_multiple_assignment_source(expression, &scope, storage, txid, arena)?;
                    subquery_literals(expression, Some(&scope), storage, txid, arena)?;
                    assign(name, expression, update.alias)?;
                }
            }
            Stmt::Delete(delete) => {
                if let Some(predicate) = delete.where_clause {
                    subquery_literals(predicate, Some(&scope), storage, txid, arena)?;
                }
            }
            Stmt::Merge(merge) => {
                for when in merge.whens {
                    match when.action() {
                        crate::sql::ast::MergeActionRef::Update(assignments) => {
                            for (name, expression) in assignments {
                                check_multiple_assignment_source(
                                    expression, &scope, storage, txid, arena,
                                )?;
                                assign(name, expression, merge.target_alias)?;
                            }
                        }
                        crate::sql::ast::MergeActionRef::Insert {
                            columns, values, ..
                        } => insert_values(columns, values, merge_columns)?,
                        _ => {}
                    }
                }
            }
            _ => unreachable!(),
        }
        let returning = match statement {
            Stmt::Insert(insert) => insert.returning.items,
            Stmt::Update(update) => update.returning.items,
            Stmt::Delete(delete) => delete.returning.items,
            _ => &[],
        };
        for item in returning {
            if let SelectItem::Expr { expression, .. } = item {
                subquery_literals(expression, Some(&scope), storage, txid, arena)?;
            }
        }
        Ok(())
    })();
    // Validation returns no arena-backed values or catalog owners.
    unsafe { arena.rewind_to(mark) };
    result
}

/// PostgreSQL checks `EXECUTE` on every function a data-changing plan calls
/// when its executor starts, so a revoked function fails even when no row
/// matches: assignments, conditions, `ON CONFLICT` and `MERGE` actions and
/// `RETURNING` lists. Rows reaching evaluation are checked again there.
pub(crate) fn require_dml_builtin_execute<'a, 'o: 'a>(
    statement: &Stmt<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'o>>,
) -> Result<(), SqlError> {
    let outer = outer.map(crate::sql::eval::EnclosingRow);
    let outer = outer
        .as_ref()
        .map(|row| row as &dyn crate::sql::eval::ColumnLookup<'a>);
    let catalog = super::storage_catalog(storage, arena, txid);
    if !crate::sql::eval::CatalogAccess::routine_privileges_present(&catalog) {
        return Ok(());
    }
    let (target, alias) = match statement {
        Stmt::Update(update) => (update.table, update.alias),
        Stmt::Delete(delete) => (delete.table, delete.alias),
        // `VALUES` and `SELECT` sources are checked as they are evaluated.
        Stmt::Insert(insert)
            if insert.returning.items.is_empty()
                && insert
                    .on_conflict
                    .is_none_or(|conflict| conflict.update.is_none()) =>
        {
            return Ok(());
        }
        Stmt::Insert(insert) => (insert.table, None),
        Stmt::Merge(merge) => (merge.target, merge.target_alias),
        _ => return Ok(()),
    };
    if !matches!(
        storage.resolve_relation(target.schema, target.name, txid),
        Some(ResolvedRelation::Table(_))
    ) {
        return Ok(());
    }
    let mark = arena.mark();
    let result = (|| {
        let extra = match statement {
            Stmt::Update(update) => update.from,
            Stmt::Delete(delete) => delete.using,
            // `ON CONFLICT DO UPDATE` reads the proposed row as `excluded`.
            Stmt::Insert(insert)
                if insert
                    .on_conflict
                    .is_some_and(|conflict| conflict.update.is_some()) =>
            {
                Some(
                    &*arena
                        .alloc(FromClause {
                            base: super::dml_target_ref(target, Some("excluded")),
                            joins: &[],
                        })
                        .map_err(|_| super::arena_full())?,
                )
            }
            Stmt::Merge(merge) => Some(
                &*arena
                    .alloc(FromClause {
                        base: merge.source,
                        joins: &[],
                    })
                    .map_err(|_| super::arena_full())?,
            ),
            _ => None,
        };
        let scope = dml_dependency_scope(storage, target, alias, extra, txid, arena, outer)?;
        let hooks = crate::sql::eval::EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        let schema = super::ScopeSchema(&scope);
        // A routine's variables read in the statement type its expressions.
        let row = super::Chained {
            inner: &schema,
            outer,
        };
        let check = |expression: &Expr<'a>| {
            crate::sql::eval::require_expression_builtin_execute(expression, &row, &hooks)
        };
        let assignments = |assignments: &[(&str, &'a Expr<'a>)]| {
            assignments
                .iter()
                .try_for_each(|(_, expression)| check(expression))
        };
        let returning = |returning: &crate::sql::ast::Returning<'a>| {
            returning.items.iter().try_for_each(|item| match item {
                SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                    check(expression)
                }
                _ => Ok(()),
            })
        };
        match statement {
            Stmt::Update(update) => {
                assignments(update.assignments)?;
                update.where_clause.map_or(Ok(()), check)?;
                returning(&update.returning)
            }
            Stmt::Delete(delete) => {
                delete.where_clause.map_or(Ok(()), check)?;
                returning(&delete.returning)
            }
            Stmt::Insert(insert) => {
                if let Some(conflict) = insert.on_conflict {
                    assignments(conflict.update.unwrap_or(&[]))?;
                    conflict.update_where.map_or(Ok(()), check)?;
                }
                returning(&insert.returning)
            }
            Stmt::Merge(merge) => {
                check(merge.on)?;
                for when in merge.whens {
                    when.condition().map_or(Ok(()), check)?;
                    match when.action() {
                        crate::sql::ast::MergeActionRef::Update(list) => assignments(list)?,
                        crate::sql::ast::MergeActionRef::Insert { values, .. } => {
                            values.iter().try_for_each(|value| check(value))?
                        }
                        crate::sql::ast::MergeActionRef::Delete
                        | crate::sql::ast::MergeActionRef::DoNothing => {}
                    }
                }
                returning(&merge.returning)
            }
            _ => Ok(()),
        }
    })();
    // The check returns no arena-backed values.
    unsafe { arena.rewind_to(mark) };
    result
}

/// PostgreSQL compares the numbers of target columns and query outputs of an
/// `INSERT ... SELECT` before converting any value, positioning the error at
/// the first target or expression without a partner. Targets with subscripts
/// or fields are counted as written.
fn check_insert_counts(
    select: &Select<'_>,
    names: &[&str],
    table_columns: usize,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
) -> Result<(), SqlError> {
    let (source, targets) = match select.from {
        Some(FromClause {
            base:
                TableRef {
                    alias: Some(crate::sql::ast::INSERT_SUBSCRIPT_SOURCE),
                    subquery: Some(written),
                    col_alias: Some(written_targets),
                    ..
                },
            ..
        }) => (written, written_targets.len()),
        _ if names.is_empty() => (select, table_columns),
        _ => (select, names.len()),
    };
    let mut width = 0usize;
    visit_projection_types(source, storage, txid, arena, outer, &mut |_, _| {
        width += 1;
        Ok(())
    })?;
    if width > targets {
        let error = sql_err!(
            sqlstate::SYNTAX_ERROR,
            "INSERT has more expressions than target columns"
        );
        return Err(
            match crate::sql::exec::select_output_expression(source, targets) {
                Some(expression) => crate::sql::error_position::at_expression(error, expression),
                None => error,
            },
        );
    }
    if width < targets && !names.is_empty() {
        let error = sql_err!(
            sqlstate::SYNTAX_ERROR,
            "INSERT has more target columns than expressions"
        );
        return Err(match names.get(width).filter(|_| names.len() == targets) {
            Some(column) => crate::sql::error_position::at_reference(error, column),
            None => error,
        });
    }
    Ok(())
}

fn validate_insert_projection(
    select: &Select<'_>,
    names: &[&str],
    target: crate::storage::RelationDefinition<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
) -> Result<(), SqlError> {
    let column = |index: usize| {
        if names.is_empty() {
            target.columns().get(index)
        } else {
            names
                .get(index)
                .and_then(|name| target.column_index(name))
                .map(|index| &target.columns()[index])
        }
    };
    // Subscripted targets assign from the written query's output columns.
    if let Some(FromClause {
        base:
            TableRef {
                alias: Some(crate::sql::ast::INSERT_SUBSCRIPT_SOURCE),
                subquery: Some(written),
                ..
            },
        ..
    }) = select.from
    {
        let mut sources: [(i32, Option<&Expr<'_>>); crate::sql::exec::MAX_PROJ] =
            [(crate::sql::types::oid::UNKNOWN, None); crate::sql::exec::MAX_PROJ];
        let mut count = 0;
        visit_projection_sources(
            written,
            storage,
            txid,
            arena,
            outer,
            &mut |source, expression| {
                if let Some(slot) = sources.get_mut(count) {
                    *slot = (source, expression);
                }
                count += 1;
                Ok(())
            },
        )?;
        for (index, item) in select.items.iter().enumerate() {
            let (SelectItem::Expr { expression, .. }, Some(column)) = (item, column(index)) else {
                continue;
            };
            let Some(&(source, written_expression)) = sources.get(index).filter(|_| index < count)
            else {
                continue;
            };
            // A merged target reads each written column it assigns.
            let mut source_of = |value: &Expr<'_>| {
                Ok(match value {
                    Expr::Column {
                        qualifier: Some(crate::sql::ast::INSERT_SUBSCRIPT_SOURCE),
                        name,
                    } => name
                        .strip_prefix("column")
                        .and_then(|ordinal| ordinal.parse::<usize>().ok())
                        .and_then(|ordinal| sources[..count.min(sources.len())].get(ordinal - 1))
                        .map_or(source, |(source, _)| *source),
                    _ => source,
                })
            };
            let target_name = names.get(index).copied();
            if validate_subscript_assignment(
                expression,
                target_name,
                AssignmentTarget::Insert,
                column,
                &mut source_of,
                storage,
                txid,
            )? {
                continue;
            }
            validate_assignment_type(source, column, storage, txid).map_err(|error| {
                match written_expression {
                    Some(expression) => {
                        crate::sql::error_position::at_expression(error, expression)
                    }
                    // A column a star expanded is reported at the star.
                    None => match star_item(written) {
                        Some(item) => crate::sql::error_position::at_item(error, item),
                        None => error,
                    },
                }
            })?;
        }
        return Ok(());
    }
    // An INSERT's VALUES rows convert to the targets one row at a time.
    if let Some(tree) = select.set_body
        && values_rows(tree)
    {
        fn leaves<'t>(
            tree: &'t SetTree<'t>,
            visit: &mut dyn FnMut(&'t Select<'t>) -> Result<(), SqlError>,
        ) -> Result<(), SqlError> {
            match tree {
                SetTree::Select(select) => visit(select),
                SetTree::Op { left, right, .. } => {
                    leaves(left, visit)?;
                    leaves(right, visit)
                }
            }
        }
        return leaves(tree, &mut |row| {
            validate_insert_projection(row, names, target, storage, txid, arena, outer)
        });
    }
    let mut index = 0;
    visit_projection_sources(
        select,
        storage,
        txid,
        arena,
        outer,
        &mut |source, expression| {
            let target = column(index);
            index += 1;
            let Some(column) = target else {
                return Ok(());
            };
            validate_assignment_type(source, column, storage, txid).map_err(|error| {
                match expression {
                    Some(expression) => {
                        crate::sql::error_position::at_expression(error, expression)
                    }
                    // A column a star expanded is reported at the star.
                    None => match star_item(select) {
                        Some(item) => crate::sql::error_position::at_item(error, item),
                        None => error,
                    },
                }
            })?;
            // An unknown-type literal converts to its target as analysed.
            match expression {
                Some(Expr::Str(literal)) => {
                    super::convert_literal(literal, Some(column.ctype), arena)
                }
                _ => Ok(()),
            }
        },
    )
}

/// Whether `tree` is a plain `VALUES` list (or one row), whose rows an INSERT
/// converts separately rather than through their common type.
fn values_rows(tree: &SetTree<'_>) -> bool {
    match tree {
        SetTree::Select(_) => true,
        SetTree::Op {
            syntax: crate::sql::ast::SetOperationSyntax::Values,
            left,
            right,
            ..
        } => values_rows(left) && values_rows(right),
        SetTree::Op { .. } => false,
    }
}

/// Each output column's type as PostgreSQL's INSERT analysis sees it: a
/// plain SELECT keeps an untyped literal UNKNOWN, while a set operation's
/// columns take their resolved common type. The written expression, when
/// there is one, positions an error (the first branch's for a set operation).
fn visit_projection_sources<'s>(
    select: &'s Select<'s>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    visit: &mut dyn FnMut(i32, Option<&'s Expr<'s>>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mut index = 0;
    visit_projection_types(select, storage, txid, arena, outer, &mut |source, _| {
        let expression = crate::sql::exec::select_output_expression(select, index);
        index += 1;
        let source = match expression {
            Some(Expr::Str(_) | Expr::Null) if select.set_body.is_none() => {
                crate::sql::types::oid::UNKNOWN
            }
            _ => source,
        };
        visit(source, expression)
    })
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

fn visit_dml_assignment_inputs(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
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
            Stmt::Insert(insert) => (insert.table, insert.alias, None),
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
        let scope = dml_dependency_scope(storage, target, alias, extra, txid, arena, outer)?;
        visit_resolved_dml_assignment_inputs(
            statement, storage, txid, arena, outer, &scope, false, visit,
        )
    })();
    // Only Copy cast snapshots, allocated in the persistent tail, may escape.
    unsafe { arena.rewind_to(mark) };
    result
}

// Assignment discovery borrows the target already selected by its enclosing
// walk. Resolving it again would require another image while that scope is live.
#[expect(
    clippy::too_many_arguments,
    reason = "assignment discovery carries its selected scope, row context and grouping policy"
)]
pub(super) fn visit_resolved_dml_assignment_inputs(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
    scope: &super::QueryScope<'_>,
    grouped: bool,
    visit: &mut AssignmentGroupVisit<'_>,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let (target_name, alias) = match statement {
            Stmt::Insert(insert) => (insert.table.name, insert.alias),
            Stmt::Update(update) => (update.table.name, update.alias),
            Stmt::Merge(merge) => (merge.target.name, merge.target_alias),
            _ => return Ok(()),
        };
        let target = scope.defs.get(0).expect("assignment target resolved");
        let catalog = super::storage_catalog(storage, arena, txid);
        let transition = outer.map(|row| crate::sql::eval::RuntimeColumnTypes {
            row,
            catalog: Some(&catalog),
        });
        let columns = DependencyTypes {
            hidden: None,
            scope: Some(scope),
            excluded: Some(target),
            transition: transition
                .as_ref()
                .map(|columns| columns as &dyn ColTypeResolver),
            storage,
            txid,
            returning_output: None,
            arena,
        };
        // An INSERT's VALUES rows, visited first, read no relation, only the
        // enclosing routine's variables and trigger rows.
        let values_columns = DependencyTypes {
            scope: None,
            excluded: None,
            hidden: Some((target_name, target)),
            ..columns
        };
        let values = match statement {
            Stmt::Insert(insert) => insert.rows.iter().map(|row| row.len()).sum::<usize>(),
            _ => 0,
        };
        let resolver = |visited: usize| -> &dyn ColTypeResolver {
            if visited < values {
                &values_columns
            } else {
                &columns
            }
        };
        let mut visited = 0usize;
        let mut assignments = crate::mem::arena::ArenaList::new(arena);
        for_each_dml_assignment(statement, &mut |name, index, expression, site| {
            let ordinal = index;
            let index = match name {
                Some(name) => target.column_index(name).ok_or_else(|| {
                    if matches!(statement, Stmt::Insert(_)) {
                        crate::sql::error_position::at_reference(
                            sql_err!(
                                sqlstate::UNDEFINED_COLUMN,
                                "column \"{}\" of relation \"{}\" does not exist",
                                name,
                                target_name
                            ),
                            name,
                        )
                    } else {
                        undefined_set_target(name, target_name, alias, expression)
                    }
                })?,
                None => index,
            };
            let column = target.columns().get(index).ok_or_else(|| {
                crate::sql::error_position::at_expression(
                    sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "INSERT has more expressions than target columns"
                    ),
                    expression,
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
                visited += 1;
                visit(core::slice::from_ref(&assignment), resolver(visited - 1))
            }
        })?;
        let assignments = assignments.as_slice();
        let mut start = 0;
        while start < assignments.len() {
            let end = assignments[start + 1..]
                .iter()
                .position(|assignment| assignment.ordinal == 0)
                .map_or(assignments.len(), |offset| start + 1 + offset);
            visit(&assignments[start..end], resolver(start))?;
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
        let catalog = super::storage_catalog(storage, arena, txid);
        let transition = outer.map(|row| crate::sql::eval::RuntimeColumnTypes {
            row,
            catalog: Some(&catalog),
        });
        if let Some(tree) = select.set_body {
            let metadata = outer
                .zip(transition.as_ref())
                .map(|(row, columns)| super::type_inputs::MetadataOnly { row, columns });
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
                crate::sql::exec::DescriptionTarget::Sql,
                metadata
                    .as_ref()
                    .map(|row| row as &dyn crate::sql::eval::ColumnLookup<'_>),
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
        let columns = DependencyTypes {
            hidden: None,
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
                SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
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

/// PostgreSQL's assignment analysis (`transformAssignedExpr`) of one target:
/// the value must coerce to the column's type in assignment context. A
/// subscripted target checks each assigned value against the element type
/// (or the container type for a slice or jsonb), positioned at the target.
/// An unknown-type literal assigned to a column converts to the column's
/// type while the statement is analysed.
fn assigned_literal(
    expression: &Expr<'_>,
    column: &crate::storage::ColumnMeta,
    arena: &Arena,
) -> Result<(), SqlError> {
    match expression {
        Expr::Str(literal) => super::convert_literal(literal, Some(column.ctype), arena),
        _ => Ok(()),
    }
}

fn validate_assignment_expression(
    expression: &Expr<'_>,
    target_name: Option<&str>,
    target: AssignmentTarget,
    column: &crate::storage::ColumnMeta,
    columns: &dyn ColTypeResolver,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    if matches!(expression, Expr::DefaultMarker) {
        return Ok(());
    }
    let mut source_of =
        |value: &Expr<'_>| crate::sql::exec::infer_routine_argument_oid(value, columns);
    if validate_subscript_assignment(
        expression,
        target_name,
        target,
        column,
        &mut source_of,
        storage,
        txid,
    )? {
        return Ok(());
    }
    validate_assignment_type(
        crate::sql::exec::infer_routine_argument_oid(expression, columns)?,
        column,
        storage,
        txid,
    )
    .map_err(|error| crate::sql::error_position::at_expression(error, expression))
}

/// Which statement assigns a column. PostgreSQL positions an error about the
/// assigned column's own container only for `UPDATE`, whose target is a
/// column reference; an `INSERT` target is not.
#[derive(Clone, Copy)]
enum AssignmentTarget {
    Insert,
    Update,
}

/// Checks the values of a parsed subscripted assignment; false when
/// `expression` is not one.
fn validate_subscript_assignment(
    expression: &Expr<'_>,
    target_name: Option<&str>,
    target: AssignmentTarget,
    column: &crate::storage::ColumnMeta,
    source_of: &mut dyn FnMut(&Expr<'_>) -> Result<i32, SqlError>,
    storage: &Storage,
    txid: u32,
) -> Result<bool, SqlError> {
    if let Expr::Call {
        name: "__assign_path",
        args: [base, value, _, Expr::Str(kinds), operands @ ..],
        ..
    } = expression
    {
        validate_subscript_assignment(base, target_name, target, column, source_of, storage, txid)?;
        validate_assignment_path(
            value,
            kinds,
            operands,
            target_name,
            column,
            source_of,
            storage,
            txid,
        )?;
        return Ok(true);
    }
    let Expr::Call {
        name: name @ ("__jsonb_subscript_set" | "__array_subscript_set"),
        args: [base, value, ..],
        ..
    } = expression
    else {
        return Ok(false);
    };
    validate_subscript_assignment(base, target_name, target, column, source_of, storage, txid)?;
    // A domain over a container assigns into its base type.
    let declared = storage.routine_type_oid(column.ctype, column.user_type, txid);
    let (container, container_oid) =
        match declared.and_then(|type_oid| storage.domain_base_type_for_oid(type_oid, txid)) {
            Some(base) => (base, storage.routine_type_oid(base, None, txid)),
            None => (column.ctype, declared),
        };
    let assigned = match (container, *name) {
        (ColType::Array(element), "__jsonb_subscript_set") => element.element_oid(),
        (ColType::Array(_) | ColType::Jsonb, _) => container_oid.unwrap_or_else(|| container.oid()),
        (
            ColType::Int2Vector
            | ColType::OidVector
            | ColType::Name
            | ColType::Geometry(
                crate::sql::types::GeometryKind::Point
                | crate::sql::types::GeometryKind::Line
                | crate::sql::types::GeometryKind::Box
                | crate::sql::types::GeometryKind::Lseg,
            ),
            _,
        ) => return Ok(true),
        _ => {
            let error = sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "cannot subscript type {} because it does not support subscripting",
                crate::sql::catalog::format_type_name(
                    storage,
                    txid,
                    container_oid.unwrap_or_else(|| container.oid())
                )?
                .as_str()
            );
            return Err(match (target, target_name) {
                (AssignmentTarget::Update, Some(name)) => {
                    crate::sql::error_position::at_reference(error, name)
                }
                _ => error,
            });
        }
    };
    let source = source_of(value)?;
    if crate::sql::catalog::can_coerce_assignment(storage, txid, source, assigned) {
        return Ok(true);
    }
    let error = crate::sql::error_position::with_hint(
        sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "subscripted assignment to \"{}\" requires type {} but expression is of type {}",
            column.name.as_str(),
            crate::sql::catalog::format_type_name(storage, txid, assigned)?.as_str(),
            crate::sql::catalog::format_type_name(storage, txid, source)?.as_str()
        ),
        "You will need to rewrite or cast the expression.",
    );
    Err(match target_name {
        Some(name) => crate::sql::error_position::at_reference(error, name),
        None => error,
    })
}

/// PostgreSQL's unknown `SET` target: `SET t.a = 1` reads as column `t`,
/// field `a`, so a target named like the relation gets a hint.
pub(crate) fn undefined_set_target(
    name: &str,
    relation: &str,
    alias: Option<&str>,
    expression: &Expr<'_>,
) -> SqlError {
    let error = crate::sql::error_position::at_reference(
        sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" of relation \"{}\" does not exist",
            name,
            relation
        ),
        name,
    );
    if is_indirect_assignment(expression) && name == alias.unwrap_or(relation) {
        crate::sql::error_position::with_hint(
            error,
            "SET target columns cannot be qualified with the relation name.",
        )
    } else {
        error
    }
}

/// The first `*` or `t.*` item of a query's first select list, which every
/// output column without its own expression in a common `SELECT *` comes
/// from.
fn star_item<'s>(select: &'s Select<'s>) -> Option<&'s SelectItem<'s>> {
    let mut select = select;
    while let Some(body) = select.set_body {
        let mut tree = body;
        while let crate::sql::ast::SetTree::Op { left, .. } = tree {
            tree = left;
        }
        let crate::sql::ast::SetTree::Select(first) = tree else {
            return None;
        };
        select = first;
    }
    select
        .items
        .iter()
        .find(|item| matches!(item, SelectItem::Wildcard(_) | SelectItem::TableWildcard(_)))
}

/// Whether an assigned value writes through a field or subscript.
pub(crate) fn is_indirect_assignment(expression: &Expr<'_>) -> bool {
    matches!(
        expression,
        Expr::Call {
            name: "__assign_path" | "__jsonb_subscript_set" | "__array_subscript_set",
            ..
        }
    )
}

/// PostgreSQL's `transformAssignmentIndirection` for a target through
/// fields and element subscripts: each field must exist in a composite, and
/// the value must coerce to the last field or element in assignment context.
/// Errors are positioned at the target column.
#[allow(clippy::too_many_arguments)]
fn validate_assignment_path(
    value: &Expr<'_>,
    kinds: &str,
    operands: &[&Expr<'_>],
    target_name: Option<&str>,
    column: &crate::storage::ColumnMeta,
    source_of: &mut dyn FnMut(&Expr<'_>) -> Result<i32, SqlError>,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    let at_target = |error| match target_name {
        Some(name) => crate::sql::error_position::at_reference(error, name),
        None => error,
    };
    let type_name = |type_oid| crate::sql::catalog::format_type_name(storage, txid, type_oid);
    let Some(mut container) = storage.routine_type_oid(column.ctype, column.user_type, txid) else {
        return Ok(());
    };
    let mut target = crate::util::StackStr::<64>::from_str(column.name.as_str());
    let mut subscripted = false;
    let mut operands = operands.iter();
    for kind in kinds.bytes() {
        if kind != b'f' {
            for _ in 0..match kind {
                b'e' | b'l' | b'u' => 1,
                b's' => 2,
                _ => 0,
            } {
                operands.next();
            }
            let Some(element) = crate::sql::assignment_path::element_type_oid(container) else {
                return Err(at_target(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "cannot subscript type {} because it does not support subscripting",
                    type_name(container)?.as_str()
                )));
            };
            // A slice assigns an array of the container's type.
            if kind == b'e' {
                container = element;
            }
            subscripted = true;
            continue;
        }
        let Some(Expr::Str(field)) = operands.next().copied() else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "assignment field step lacks its name"
            ));
        };
        let creation = match crate::storage::NamedTypeKind::from_oid(container) {
            Some((crate::storage::NamedTypeKind::Composite, creation, false)) => Some(creation),
            _ => None,
        };
        let mut found = None;
        let fields = crate::sql::exec::visit_catalog_composite_fields(
            storage,
            txid,
            creation,
            &mut |name, metadata| {
                if found.is_none() && name == *field {
                    found = Some(metadata.type_oid);
                }
            },
        );
        if fields.is_none() {
            return Err(at_target(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "cannot assign to field \"{}\" of column \"{}\" because its type {} is not a composite type",
                field,
                target.as_str(),
                type_name(container)?.as_str()
            )));
        }
        let Some(field_type) = found else {
            return Err(at_target(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "cannot assign to field \"{}\" of column \"{}\" because there is no such column in data type {}",
                field,
                target.as_str(),
                type_name(container)?.as_str()
            )));
        };
        container = field_type;
        target = crate::util::StackStr::from_str(field);
        subscripted = false;
    }
    let source = source_of(value)?;
    if crate::sql::catalog::can_coerce_assignment(storage, txid, source, container) {
        return Ok(());
    }
    let error = if subscripted {
        sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "subscripted assignment to \"{}\" requires type {} but expression is of type {}",
            target.as_str(),
            type_name(container)?.as_str(),
            type_name(source)?.as_str()
        )
    } else {
        sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "subfield \"{}\" is of type {} but expression is of type {}",
            target.as_str(),
            type_name(container)?.as_str(),
            type_name(source)?.as_str()
        )
    };
    Err(at_target(crate::sql::error_position::with_hint(
        error,
        "You will need to rewrite or cast the expression.",
    )))
}

/// [`validate_assignment_expression`] for a value of a known type.
fn validate_assignment_type(
    source_oid: i32,
    column: &crate::storage::ColumnMeta,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    let target_oid = storage
        .routine_type_oid(column.ctype, column.user_type, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "column type identity is unavailable"
            )
        })?;
    if crate::sql::catalog::can_coerce_assignment(storage, txid, source_oid, target_oid) {
        return Ok(());
    }
    Err(crate::sql::error_position::with_hint(
        sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "column \"{}\" is of type {} but expression is of type {}",
            column.name.as_str(),
            crate::sql::catalog::format_type_name(storage, txid, target_oid)?.as_str(),
            crate::sql::catalog::format_type_name(storage, txid, source_oid)?.as_str()
        ),
        "You will need to rewrite or cast the expression.",
    ))
}

pub(crate) fn dml_dependency_scope<'a>(
    storage: &'a Storage,
    target: QualName<'a>,
    alias: Option<&'a str>,
    extra: Option<&'a FromClause<'a>>,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
) -> Result<super::QueryScope<'a>, SqlError> {
    dml_join_scope(storage, target, alias, extra, txid, arena, outer).map(|(scope, _)| scope)
}

/// The schema scope of [`dml_dependency_scope`] with the FROM clause it
/// resolves: the target first, then each extra FROM item joined by a comma.
pub(crate) fn dml_join_scope<'a>(
    storage: &'a Storage,
    target: QualName<'a>,
    alias: Option<&'a str>,
    extra: Option<&'a FromClause<'a>>,
    txid: u32,
    arena: &'a Arena,
    outer: Option<&dyn crate::sql::eval::ColumnLookup<'_>>,
) -> Result<(super::QueryScope<'a>, &'a FromClause<'a>), SqlError> {
    let base = super::dml_target_ref(target, alias);
    let joins = match extra {
        None => &[][..],
        Some(extra) => {
            let joins = arena
                .alloc_slice_with(extra.joins.len() + 1, |index| {
                    if index == 0 {
                        Join {
                            table: extra.base,
                            kind: JoinKind::Comma,
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
    let scope = super::QueryScope::resolve_dml_schema(storage, from, txid, arena, outer)?;
    Ok((scope, from))
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
                scope.push(cte)?;
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
            visible_ctes.push(cte)?;
        }
        // A level reading only tables has a schema its subqueries' outer
        // references resolve in.
        let level_scope = match select.from.as_ref() {
            Some(from) => {
                let mut all_tables = true;
                for source in
                    core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table))
                {
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
                    let bound = context.bound_from(from, storage, txid, visible_ctes, arena)?;
                    let bound = arena.alloc(bound).map_err(|_| arena_full())?;
                    Some(super::QueryScope::resolve_schema(
                        storage, bound, txid, arena, None,
                    )?)
                } else {
                    None
                }
            }
            None => None,
        };
        let level = EnclosingLevel {
            scope: level_scope.as_ref(),
            parent: context.enclosing,
            deferred: core::cell::RefCell::new(crate::mem::arena::ArenaList::new(arena)),
        };
        let nested = CollectionContext {
            enclosing: Some(&level),
            ..context
        };

        // The level's relations come first: a subquery's outer references
        // mark columns of relations the query already depends on.
        if let Some(from) = select.from {
            // A LATERAL item's outer references name this level's items.
            collect_table_ref(
                &from.base,
                storage,
                txid,
                visible_ctes,
                dependencies,
                arena,
                if from.base.lateral { nested } else { context },
            )?;
            for join in from.joins {
                collect_table_ref(
                    &join.table,
                    storage,
                    txid,
                    visible_ctes,
                    dependencies,
                    arena,
                    if join.table.lateral { nested } else { context },
                )?;
                if let Some(on) = join.on {
                    collect_expression(
                        on,
                        storage,
                        txid,
                        visible_ctes,
                        dependencies,
                        arena,
                        nested,
                    )?;
                }
            }
            for &(class, slot, column) in level.deferred.borrow().as_slice() {
                dependencies.mark_referenced_column(class, slot, column)?;
            }
            if let Some(scope) = level_scope.as_ref() {
                for item in select.items {
                    match item {
                        SelectItem::Expr { expression, .. }
                        | SelectItem::RecordStar(expression) => {
                            record_column_references(
                                expression,
                                scope,
                                dependencies,
                                context,
                                ReferenceScope::Query,
                            )?;
                        }
                        SelectItem::Wildcard(_) => {
                            for table in 0..scope.n {
                                for column in 0..scope.defs.get(table).expect("resolved").n_columns
                                {
                                    mark_resolved_column(
                                        scope,
                                        super::ResolvedColumn::Table(table, column),
                                        dependencies,
                                    )?;
                                }
                            }
                        }
                        SelectItem::TableWildcard(name) => {
                            for index in 0..scope.qualified_star_columns(name)? {
                                mark_resolved_column(
                                    scope,
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
                    // `GROUP BY name` and `ORDER BY name` may name an output
                    // column, whose expression the target list recorded.
                    if let Expr::Column {
                        qualifier: None,
                        name,
                    } = expression
                        && select.items.iter().any(|item| {
                            matches!(item, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
                        })
                        && crate::sql::error_position::preserving_pending(|| {
                            scope.find_column(None, name)
                        })
                        .is_err()
                    {
                        continue;
                    }
                    record_column_references(
                        expression,
                        scope,
                        dependencies,
                        context,
                        ReferenceScope::Query,
                    )?;
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
                        nested,
                    )?;
                }
                SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {}
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
                nested,
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
                nested,
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
    /// The INSERT target, by name and definition, that its `VALUES` rows
    /// cannot reference; they read only `transition`.
    hidden: Option<(
        &'definition str,
        crate::storage::RelationDefinition<'storage>,
    )>,
    arena: &'storage Arena,
}

impl<'storage> DependencyTypes<'_, '_, 'storage> {
    fn output_definition(
        &self,
        name: &str,
    ) -> Option<crate::storage::RelationDefinition<'storage>> {
        if let Some((definition, aliases)) = self.returning_output
            && aliases.contains(&name)
        {
            return Some(definition);
        }
        if name == "excluded" {
            return self.excluded;
        }
        None
    }
}

impl<'a> crate::sql::eval::ColumnLookup<'a> for DependencyTypes<'_, '_, '_> {
    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        _catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        ColTypeResolver::whole_row_field(self, table, index)
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        ColTypeResolver::whole_row_scalar_type(self, table)
            .or_else(|| ColTypeResolver::is_whole_row(self, table).then_some(ColType::Record))
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        ColTypeResolver::whole_row_scalar_type(self, table).is_some()
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        self.column_meta(qualifier, name)
            .map_or(-1, |column| column.type_mod)
    }

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
        if let Some((hidden, definition)) = self.hidden {
            if let Some(metadata) = self
                .transition
                .and_then(|columns| columns.column_meta(qualifier, name))
            {
                return Ok(metadata.ctype);
            }
            // PostgreSQL names the target the rows cannot reference.
            return Err(match qualifier {
                Some(qualifier) if qualifier == hidden => {
                    super::scope::unreachable_entry(qualifier)
                }
                None if definition.column_index(name).is_some() => {
                    crate::sql::error_position::with_detail(
                        crate::sql::eval::empty_scope_column_error(None, name),
                        format_args!(
                            "There is a column named \"{}\" in table \"{}\", but it cannot be referenced from this part of the query.",
                            name, hidden
                        ),
                    )
                }
                _ => crate::sql::eval::empty_scope_column_error(qualifier, name),
            });
        }
        if qualifier == Some("excluded")
            && let Some(definition) = self.excluded
        {
            return definition
                .column_index(name)
                .map(|index| definition.columns()[index].ctype)
                .ok_or_else(|| {
                    let mut suggestion =
                        crate::sql::column_suggestion::ColumnSuggestion::new(qualifier, name);
                    if let Some(scope) = self.scope {
                        scope.suggest_columns(&mut suggestion);
                    }
                    suggestion.relation(
                        "excluded",
                        definition
                            .columns()
                            .iter()
                            .map(|column| column.name.as_str()),
                    );
                    crate::sql::error_position::with_column_suggestion(
                        sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "column excluded.{} does not exist",
                            name
                        ),
                        &suggestion,
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
        if self.hidden.is_some() {
            return self
                .transition
                .and_then(|columns| columns.column_meta(qualifier, name));
        }
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
                    outer: None,
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
        self.output_definition(name).is_some()
            || self
                .transition
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

    fn whole_row_composite_type(&self, name: &str) -> Option<ColType> {
        // RETURNING's OLD/NEW rows and ON CONFLICT's EXCLUDED row have the
        // target relation's row type.
        if let Some(definition) = self.output_definition(name) {
            let slot = self.storage.find_visible(
                definition.schema.as_str(),
                definition.name.as_str(),
                self.txid,
            )?;
            return Some(ColType::Composite(Some(
                self.storage
                    .relation_row_type_creation(crate::storage::RowTypeRelation::Table(slot)),
            )));
        }
        // A trigger's OLD/NEW rows, or a table expression's own row.
        if let Some(composite) = self
            .transition
            .and_then(|transition| transition.whole_row_composite_type(name))
        {
            return Some(composite);
        }
        self.scope?
            .whole_row_type(name)
            .filter(|ctype| matches!(ctype, ColType::Composite(_)))
    }

    fn table_columns(&self, name: &str) -> Option<&[crate::storage::ColumnMeta]> {
        if let Some(definition) = self.output_definition(name) {
            return Some(definition.columns());
        }
        if let Some(columns) = self
            .transition
            .and_then(|transition| transition.table_columns(name))
        {
            return Some(columns);
        }
        let scope = self.scope?;
        let source = scope.source(name).ok()?;
        Some(scope.source_columns(source))
    }

    fn whole_row_field(
        &self,
        name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, StaticTypeMeta)> {
        if let Some(definition) = self.output_definition(name) {
            let column = definition.columns().get(index)?;
            return Some((
                crate::util::StackStr::from_str(column.name.as_str()),
                self.column_meta(Some(name), column.name.as_str())?,
            ));
        }
        self.transition
            .and_then(|transition| transition.whole_row_field(name, index))
            .or_else(|| {
                super::CatalogScopeCols {
                    scope: self.scope?,
                    outer: None,
                    storage: self.storage,
                    txid: self.txid,
                    arena: self.arena,
                }
                .whole_row_field(name, index)
            })
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

fn from_reads_cte(from: &FromClause<'_>, ctes: CteNames<'_>) -> bool {
    core::iter::once(&from.base)
        .chain(from.joins.iter().map(|join| &join.table))
        .any(|table| table.schema.is_none() && table.cte.is_none() && ctes.contains(table.table))
}

fn collect_routine_dependencies<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    arena: &'a Arena,
    context: CollectionContext<'_>,
    ctes: CteNames<'a>,
) -> Result<(), SqlError> {
    let resolver = DependencyTypes {
        hidden: None,
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
            let from = if from_reads_cte(from, ctes) {
                // A level reading `WITH` queries describes them in place.
                let level = arena
                    .alloc(Select {
                        with: ctes.definitions_in(arena)?,
                        from: Some(*from),
                        ..*select
                    })
                    .map_err(|_| arena_full())?;
                super::expand_ctes(level, storage, txid, arena)?
                    .from
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "WITH expansion removed a FROM clause"
                        )
                    })?
            } else {
                *from
            };
            let bound = context.bound_from(&from, storage, txid, ctes, arena)?;
            let scope = super::QueryScope::resolve_schema(storage, &bound, txid, arena, None)?;
            let resolver = DependencyTypes {
                hidden: None,
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
        hidden: None,
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
        hidden: None,
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
        written_columns: crate::storage::ColumnSet::EMPTY,
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
        // Without the FROM scope a whole-row reference has no row type yet;
        // the caller retries with the scope instead of selecting `record`.
        if let Expr::WholeRow(table) = argument
            && resolver.scope.is_none()
            && !ColTypeResolver::is_whole_row(resolver, table)
        {
            if unresolved.is_none() {
                *unresolved = Some(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "missing FROM-clause entry for table \"{}\"",
                    table
                ));
            }
            return Ok(());
        }
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
        written_columns: crate::storage::ColumnSet::EMPTY,
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
        written_columns: crate::storage::ColumnSet::EMPTY,
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
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {}
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

/// The columns of a sequence read as a relation, in attribute order.
const SEQUENCE_RELATION_COLUMNS: [&str; 3] = ["last_value", "log_cnt", "is_called"];

#[derive(Clone, Copy)]
struct RelationSource<'a> {
    class: DependencyClass,
    slot: usize,
    exposed: &'a str,
    qualified: Option<&'a str>,
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
            qualified: None,
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
            // A sequence read as a relation records the columns it reads.
            let name = crate::sql::types::acl_identifier(table.table);
            let reference = match table.schema {
                Some(schema) => crate::stack_format!(
                    264,
                    "{}.{}",
                    crate::sql::types::acl_identifier(schema).as_str(),
                    name.as_str()
                ),
                None => crate::stack_format!(264, "{}", name.as_str()),
            };
            let (Some(slot), _, _) =
                resolve_sequence_reference(reference.as_str(), storage, txid, context.path)?
            else {
                continue;
            };
            let source = &mut sources[n_sources];
            source.exposed = table.alias.unwrap_or(table.table);
            source.class = DependencyClass::Sequence;
            source.slot = slot;
            source.n_columns = SEQUENCE_RELATION_COLUMNS.len();
            for (column, name) in SEQUENCE_RELATION_COLUMNS.iter().enumerate() {
                source.columns[column] = table
                    .col_alias
                    .and_then(|aliases| aliases.get(column).copied())
                    .unwrap_or(name);
            }
            if table.alias.is_none() {
                let schema = storage.sequence_for(slot, txid).schema;
                let name = crate::stack_format!(
                    130,
                    "{}.{}",
                    table.schema.unwrap_or(schema.as_str()),
                    table.table
                );
                source.qualified = Some(arena.alloc_str(name.as_str()).map_err(|_| arena_full())?);
            }
            n_sources += 1;
            continue;
        };
        let source = &mut sources[n_sources];
        source.exposed = table.alias.unwrap_or(table.table);
        match relation {
            ResolvedRelation::Table(slot) => {
                let definition = storage.table_def(slot, txid);
                if table.alias.is_none() {
                    let name = crate::stack_format!(
                        130,
                        "{}.{}",
                        table.schema.unwrap_or(definition.schema.as_str()),
                        table.table
                    );
                    source.qualified =
                        Some(arena.alloc_str(name.as_str()).map_err(|_| arena_full())?);
                }
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
                if table.alias.is_none() {
                    let schema = view_image.schema_for(txid);
                    let name = crate::stack_format!(
                        130,
                        "{}.{}",
                        table.schema.unwrap_or(schema.as_str()),
                        table.table
                    );
                    source.qualified =
                        Some(arena.alloc_str(name.as_str()).map_err(|_| arena_full())?);
                }
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
    let merged = |name: &str| {
        from.joins.iter().any(|join| {
            join.natural
                || join
                    .using
                    .is_some_and(|using| using.columns.contains(&name))
        })
    };
    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                record_relation_expression(expression, sources, &merged, dependencies)?
            }
            SelectItem::TableWildcard(name) => {
                for source in sources.iter().filter(|source| source.exposed == *name) {
                    for column in 0..source.n_columns {
                        dependencies.mark_referenced_column(source.class, source.slot, column)?;
                    }
                }
            }
            SelectItem::Wildcard(_) => {
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
        record_relation_expression(expression, sources, &merged, dependencies)?;
    }
    Ok(())
}

/// Preserve reference kinds until the selected namespace resolves them. A
/// whole-row alias is not a column, and `(row).field` may name just one column.
fn visit_column_dependencies<'a>(
    expression: &Expr<'a>,
    visit: &mut impl FnMut(&Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match expression {
        Expr::Column { .. } | Expr::SchemaColumn { .. } | Expr::WholeRow(_) => visit(expression),
        Expr::Field { base, field: "*" } => visit_column_dependencies(base, visit),
        Expr::Field { base, .. }
            if matches!(
                **base,
                Expr::WholeRow(_)
                    | Expr::Column {
                        qualifier: None,
                        ..
                    }
            ) =>
        {
            visit(expression)
        }
        Expr::Subquery(_) | Expr::RowSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_) => {
            Ok(())
        }
        _ => super::walk_children(expression, &mut |child| {
            visit_column_dependencies(child, visit)
        }),
    }
}

/// `merged` says whether a `USING` or `NATURAL` join merged the sources'
/// columns of an unqualified name into one, which the first source supplies.
fn find_relation_column(
    sources: &[RelationSource<'_>],
    qualifier: Option<&str>,
    name: &str,
    merged: &dyn Fn(&str) -> bool,
) -> Result<Option<(usize, usize)>, SqlError> {
    let mut found = None;
    for (source_index, source) in sources.iter().enumerate() {
        if qualifier.is_some_and(|qualifier| {
            qualifier != source.exposed && source.qualified != Some(qualifier)
        }) {
            continue;
        }
        if let Some(column) = source.columns[..source.n_columns]
            .iter()
            .position(|column| *column == name)
        {
            if found.is_some() {
                if qualifier.is_none() && merged(name) {
                    continue;
                }
                return Err(sql_err!(
                    sqlstate::AMBIGUOUS_COLUMN,
                    "column reference \"{}\" is ambiguous",
                    name
                ));
            }
            found = Some((source_index, column));
        }
    }
    Ok(found)
}

fn record_relation_expression(
    expression: &Expr<'_>,
    sources: &[RelationSource<'_>],
    merged: &dyn Fn(&str) -> bool,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    visit_column_dependencies(expression, &mut |reference| {
        let qualified;
        let (qualifier, name) = match reference {
            Expr::Column { qualifier, name } => (*qualifier, *name),
            Expr::SchemaColumn {
                schema,
                table,
                name,
            } => {
                qualified = crate::stack_format!(130, "{}.{}", schema, table);
                (Some(qualified.as_str()), *name)
            }
            Expr::WholeRow(table) => {
                for source in sources
                    .iter()
                    .filter(|source| source.exposed == *table || source.qualified == Some(*table))
                {
                    for column in 0..source.n_columns {
                        dependencies.mark_referenced_column(source.class, source.slot, column)?;
                    }
                }
                return Ok(());
            }
            Expr::Field { base, field } => match &**base {
                Expr::WholeRow(table) => (Some(*table), *field),
                Expr::Column {
                    qualifier: None,
                    name,
                } if find_relation_column(sources, None, name, merged)?.is_some() => (None, *name),
                Expr::Column {
                    qualifier: None,
                    name,
                } => (Some(*name), *field),
                _ => unreachable!("dependency walk selects simple record bases"),
            },
            _ => unreachable!("dependency walk selects references"),
        };
        if let Some((source_index, column)) =
            find_relation_column(sources, qualifier, name, merged)?
        {
            let source = &sources[source_index];
            return dependencies.mark_referenced_column(source.class, source.slot, column);
        }
        if qualifier.is_none() {
            for source in sources.iter().filter(|source| source.exposed == name) {
                for column in 0..source.n_columns {
                    dependencies.mark_referenced_column(source.class, source.slot, column)?;
                }
            }
        }
        Ok(())
    })
}

#[derive(Clone, Copy)]
enum ReferenceScope<'a> {
    Query,
    Returning {
        target: &'a str,
        aliases: [&'a str; 2],
    },
    Excluded {
        slot: usize,
        definition: crate::storage::RelationDefinition<'a>,
    },
}

impl<'a> ReferenceScope<'a> {
    fn qualifier<'name>(self, name: &'name str) -> &'name str
    where
        'a: 'name,
    {
        match self {
            Self::Returning { target, aliases } if aliases.contains(&name) => target,
            _ => name,
        }
    }
}

fn record_column_references(
    expression: &Expr<'_>,
    scope: &super::QueryScope<'_>,
    dependencies: &mut StoredQueryDependencies,
    context: CollectionContext<'_>,
    references: ReferenceScope<'_>,
) -> Result<(), SqlError> {
    visit_column_dependencies(expression, &mut |reference| {
        let qualified;
        let (qualifier, name) = match reference {
            Expr::Column { qualifier, name } => (*qualifier, *name),
            Expr::SchemaColumn {
                schema,
                table,
                name,
            } => {
                qualified = crate::stack_format!(130, "{}.{}", schema, table);
                (Some(qualified.as_str()), *name)
            }
            Expr::WholeRow(table) => {
                let table = references.qualifier(table);
                if table == "excluded"
                    && let ReferenceScope::Excluded { slot, definition } = references
                {
                    for column in 0..definition.n_columns {
                        dependencies.mark_referenced_column(
                            DependencyClass::Table,
                            slot,
                            column,
                        )?;
                    }
                    return Ok(());
                }
                let count = match scope.qualified_star_columns(table) {
                    Ok(count) => count,
                    Err(_)
                        if context
                            .transition
                            .is_some_and(|columns| columns.is_whole_row(table)) =>
                    {
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                for index in 0..count {
                    mark_resolved_column(
                        scope,
                        scope.qualified_star_entry(table, index)?,
                        dependencies,
                    )?;
                }
                return Ok(());
            }
            Expr::Field { base, field } => match &**base {
                Expr::WholeRow(table) => (Some(references.qualifier(table)), *field),
                Expr::Column {
                    qualifier: None,
                    name,
                } if scope
                    .find_column(None, name)
                    .is_err_and(|error| error.sqlstate == sqlstate::UNDEFINED_COLUMN)
                    && (scope
                        .qualified_star_columns(references.qualifier(name))
                        .is_ok()
                        || (matches!(references, ReferenceScope::Excluded { .. })
                            && *name == "excluded")) =>
                {
                    (Some(references.qualifier(name)), *field)
                }
                Expr::Column {
                    qualifier: None,
                    name,
                } => (None, *name),
                _ => unreachable!("dependency walk selects simple record bases"),
            },
            _ => unreachable!("dependency walk selects references"),
        };
        let qualifier = qualifier.map(|name| references.qualifier(name));
        if qualifier == Some("excluded")
            && let ReferenceScope::Excluded { slot, definition } = references
        {
            let column = definition.column_index(name).ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column excluded.{} does not exist",
                    name
                )
            })?;
            return dependencies.mark_referenced_column(DependencyClass::Table, slot, column);
        }
        if context.transition_column(qualifier, name)? {
            return Ok(());
        }
        match scope.find_column(qualifier, name) {
            Ok(resolved) => mark_resolved_column(scope, resolved, dependencies),
            Err(error)
                if qualifier.is_none()
                    && error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    && matches!(references, ReferenceScope::Excluded { .. })
                    && name == "excluded" =>
            {
                let ReferenceScope::Excluded { slot, definition } = references else {
                    unreachable!("excluded namespace was checked");
                };
                for column in 0..definition.n_columns {
                    dependencies.mark_referenced_column(DependencyClass::Table, slot, column)?;
                }
                Ok(())
            }
            Err(error)
                if qualifier.is_none()
                    && error.sqlstate == sqlstate::UNDEFINED_COLUMN
                    && scope
                        .qualified_star_columns(references.qualifier(name))
                        .is_ok() =>
            {
                let table = references.qualifier(name);
                for index in 0..scope.qualified_star_columns(table)? {
                    mark_resolved_column(
                        scope,
                        scope.qualified_star_entry(table, index)?,
                        dependencies,
                    )?;
                }
                Ok(())
            }
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) && context.outer_column(qualifier, name) =>
            {
                Ok(())
            }
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                match context.enclosing {
                    Some(enclosing) if enclosing.mark(qualifier, name, dependencies)? => Ok(()),
                    _ => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    })
}

fn mark_resolved_column(
    scope: &super::QueryScope<'_>,
    resolved: super::ResolvedColumn,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    visit_resolved_column(scope, resolved, &mut |class, slot, column| {
        dependencies.mark_referenced_column(class, slot, column)
    })
}

/// Each catalog relation column `resolved` reads.
fn visit_resolved_column(
    scope: &super::QueryScope<'_>,
    resolved: super::ResolvedColumn,
    visit: &mut dyn FnMut(DependencyClass, usize, usize) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let mut mark = |table: usize, column| {
        let (class, slot) = match scope.view_accesses[table] {
            Some(view) => (DependencyClass::View, usize::from(view)),
            // Materialized rows have an execution owner, not a catalog slot.
            None if scope.slots[table] == usize::MAX => return Ok(()),
            None => (DependencyClass::Table, scope.slots[table]),
        };
        visit(class, slot, column)
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
                    written_columns: crate::storage::ColumnSet::EMPTY,
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
                    written_columns: crate::storage::ColumnSet::EMPTY,
                    schema: view.schema_for(txid),
                    name: view.name_for(txid),
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::Catalog) => {}
            // Reading a sequence as a relation depends on it, as PostgreSQL
            // records.
            None => {
                let name = crate::sql::types::acl_identifier(table.table);
                let reference = match table.schema {
                    Some(schema) => crate::stack_format!(
                        264,
                        "{}.{}",
                        crate::sql::types::acl_identifier(schema).as_str(),
                        name.as_str()
                    ),
                    None => crate::stack_format!(264, "{}", name.as_str()),
                };
                collect_sequence(
                    reference.as_str(),
                    storage,
                    txid,
                    context.path,
                    dependencies,
                )?;
            }
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
            written_columns: crate::storage::ColumnSet::EMPTY,
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

/// A routine whose declared parameter or result type is a relation's row type
/// depends on that relation, as PostgreSQL records for the row type.
pub(crate) fn collect_routine_signature_row_types(
    types: impl IntoIterator<Item = crate::storage::RoutineResult>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    for result in types {
        let Some(identity) = result.user_type else {
            continue;
        };
        let Some(creation) = identity.relation_row_type_creation() else {
            continue;
        };
        let relation = storage.row_type_relation(creation, txid).ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "type \"{}\" does not exist",
                identity.name.as_str()
            )
        })?;
        record_relation_row_type(
            relation,
            identity.schema,
            identity.name,
            storage,
            txid,
            dependencies,
        )?;
    }
    Ok(())
}

/// Records a dependency on the relation that owns a referenced row type.
fn record_relation_row_type(
    relation: crate::storage::RowTypeRelation,
    referenced_schema: SqlName,
    referenced_name: SqlName,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
) -> Result<(), SqlError> {
    let (class, resolved, slot) = match relation {
        crate::storage::RowTypeRelation::Table(slot) => {
            (DependencyClass::Table, ResolvedRelation::Table(slot), slot)
        }
        crate::storage::RowTypeRelation::View(slot) => {
            (DependencyClass::View, ResolvedRelation::View(slot), slot)
        }
    };
    let identity = storage.capture_stored_query_relation(resolved, txid)?;
    if dependencies
        .entries()
        .iter()
        .any(|dependency| dependency.identity == identity)
    {
        return Ok(());
    }
    let (schema, name) = storage.row_type_relation_name(relation, txid);
    dependencies.push(StoredQueryDependency {
        class,
        slot: slot as u16,
        identity,
        referenced_columns: crate::storage::ColumnSet::EMPTY,
        written_columns: crate::storage::ColumnSet::EMPTY,
        schema,
        name,
        referenced_schema,
        referenced_name,
    })
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
    if kind == crate::storage::NamedTypeKind::Composite
        && let Some(relation) = storage.row_type_relation(creation, txid)
    {
        let (schema, name) = storage.row_type_relation_name(relation, txid);
        return record_relation_row_type(relation, schema, name, storage, txid, dependencies);
    }
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
                written_columns: crate::storage::ColumnSet::EMPTY,
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
            written_columns: crate::storage::ColumnSet::EMPTY,
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
        if let Some(relation) = storage.resolve_relation_row_type(bare, txid) {
            return record_relation_row_type(
                relation,
                referenced_schema,
                referenced_name,
                storage,
                txid,
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
    if let Some(relation) = storage.resolve_relation_row_type(bare, txid) {
        return record_relation_row_type(
            relation,
            referenced_schema,
            referenced_name,
            storage,
            txid,
            dependencies,
        );
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
        written_columns: crate::storage::ColumnSet::EMPTY,
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
        written_columns: crate::storage::ColumnSet::EMPTY,
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
        resolve_sequence_reference(sequence_name, storage, txid, path)?;
    if let Some(slot) = slot {
        let sequence = storage.sequence_for(slot, txid);
        dependencies.push(StoredQueryDependency {
            class: DependencyClass::Sequence,
            slot: slot as u16,
            identity: StoredDependencyIdentity::Name,
            referenced_columns: crate::storage::ColumnSet::EMPTY,
            written_columns: crate::storage::ColumnSet::EMPTY,
            schema: sequence.schema,
            name: sequence.name,
            referenced_schema,
            referenced_name,
        })?;
    }
    Ok(())
}

/// The sequence slot a `regclass` text or a relation name selects, with the
/// schema and name as written.
fn resolve_sequence_reference(
    sequence_name: &str,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
) -> Result<(Option<usize>, SqlName, SqlName), SqlError> {
    Ok(
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
        },
    )
}
