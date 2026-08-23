//! Name-resolution pass for durable stored-query dependencies.
//!
//! Views keep their SQL text for execution, but dependency decisions must not
//! reparse that text after catalog objects have moved. This pass runs once at
//! CREATE time and records the exact relation, user type, and sequence slots
//! selected under the creator's search path.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, FrameBound, Select, SelectItem, SetTree, TableRef};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::exec::ColTypeResolver;
use crate::sql::types::ColType;
use crate::sql_err;
use crate::storage::{
    DependencyClass, MAX_COLUMNS, MAX_STORED_QUERY_DEPENDENCIES, PathContext, PathEntry,
    ResolvedRelation, SqlName, Storage, StoredDependencyIdentity, StoredQueryDependencies,
    StoredQueryDependency,
};

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
        &path,
        CteNames::EMPTY,
        &mut dependencies,
        arena,
    )?;
    Ok(dependencies)
}

fn collect_select<'a>(
    select: &'a Select<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    inherited_ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
) -> Result<(), SqlError> {
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
            path,
            definition_scope,
            dependencies,
            arena,
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
                    path,
                    visible_ctes,
                    dependencies,
                    arena,
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
            path,
            visible_ctes,
            dependencies,
            arena,
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
            path,
            visible_ctes,
            dependencies,
            arena,
        )?;
    }
    if let Some(tree) = select.set_body {
        collect_set_tree(tree, storage, txid, path, visible_ctes, dependencies, arena)?;
    }
    if let Some(from) = select.from {
        collect_table_ref(
            &from.base,
            storage,
            txid,
            path,
            visible_ctes,
            dependencies,
            arena,
        )?;
        for join in from.joins {
            collect_table_ref(
                &join.table,
                storage,
                txid,
                path,
                visible_ctes,
                dependencies,
                arena,
            )?;
            if let Some(on) = join.on {
                collect_expression(on, storage, txid, path, visible_ctes, dependencies, arena)?;
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
                        record_column_references(expression, &scope, dependencies)?;
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
                        let table = scope.table_index(name)?;
                        for column in 0..scope.defs[table].expect("resolved").n_columns {
                            dependencies.mark_referenced_column(
                                DependencyClass::Table,
                                scope.slots[table],
                                column,
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
                record_column_references(expression, &scope, dependencies)?;
            }
        }
        record_relation_column_references(storage, txid, path, &from, select, dependencies, arena)?;
    }
    collect_routine_dependencies(select, storage, txid, dependencies, arena)?;
    Ok(())
}

struct DependencyTypes<'scope, 'definition, 'storage> {
    scope: Option<&'scope super::QueryScope<'definition>>,
    storage: &'storage Storage,
    txid: u32,
}

impl ColTypeResolver for DependencyTypes<'_, '_, '_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        match self.scope {
            Some(scope) => super::ScopeCols(scope).resolve(qualifier, name),
            None => crate::sql::exec::NoCols.resolve(qualifier, name),
        }
    }

    fn routine_result(&self, name: &str, arguments: &[i32]) -> Option<(i32, i16)> {
        let routine = self
            .storage
            .function_for_call_oids(name, arguments, self.txid)?;
        Some((
            self.storage
                .routine_function_result_oid(&routine, self.txid)?,
            routine.kind.function_result()?.typlen(),
        ))
    }

    fn routine_record_field(
        &self,
        name: &str,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, ColType)> {
        let slot = self
            .storage
            .routine_slot_for_table_call_oids(name, arguments, self.txid)?;
        let routine = self.storage.routine_for(slot, self.txid);
        let column = routine.table_columns()?.get(index)?;
        Some((
            crate::util::StackStr::from_str(column.name.as_str()),
            column.ctype,
        ))
    }

    fn named_composite_field(
        &self,
        type_name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, ColType)> {
        let slot = self.storage.resolve_composite_slot(type_name, self.txid)?;
        let definition = self.storage.composite_for(slot, self.txid);
        let field = definition.active_field(index)?;
        Some((
            crate::util::StackStr::from_str(field.name.as_str()),
            field.ctype,
        ))
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.scope
            .is_some_and(|scope| scope.table_index(name).is_ok())
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.scope.and_then(|scope| scope.func_scalar_type(name))
    }

    fn table_columns(&self, name: &str) -> Option<&[crate::storage::ColumnMeta]> {
        let scope = self.scope?;
        let table = scope.table_index(name).ok()?;
        Some(scope.defs[table]?.columns())
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
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
) -> Result<(), SqlError> {
    let resolver = DependencyTypes {
        scope: None,
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

fn collect_routine_dependencies_with_resolver(
    select: &Select<'_>,
    storage: &Storage,
    txid: u32,
    dependencies: &mut StoredQueryDependencies,
    resolver: &DependencyTypes<'_, '_, '_>,
) -> Result<bool, SqlError> {
    let mut needs_scope = false;

    fn record_call(
        name: &str,
        args: &[&Expr<'_>],
        storage: &Storage,
        txid: u32,
        resolver: &DependencyTypes<'_, '_, '_>,
        dependencies: &mut StoredQueryDependencies,
        needs_scope: &mut bool,
    ) -> Result<(), SqlError> {
        if !storage.has_function_routine_candidate(name, args.len(), txid) {
            return Ok(());
        }
        if args.len() <= crate::storage::MAX_ROUTINE_ARGUMENTS {
            let mut argument_oids = [0_i32; crate::storage::MAX_ROUTINE_ARGUMENTS];
            let mut known = true;
            for (slot, argument) in argument_oids.iter_mut().zip(args.iter().copied()) {
                match crate::sql::exec::infer_type_res(argument, resolver) {
                    Ok((crate::sql::types::oid::UNKNOWN, _))
                        if matches!(argument, Expr::Str(_)) =>
                    {
                        *slot = ColType::Text.oid();
                    }
                    Ok((oid, _)) => *slot = oid,
                    Err(_) => {
                        *needs_scope = true;
                        known = false;
                        break;
                    }
                }
            }
            if known
                && let Some(slot) = storage.routine_slot_for_function_call_oids(
                    name,
                    &argument_oids[..args.len()],
                    txid,
                )
            {
                let routine = storage.routine_for(slot, txid);
                let (referenced_schema, referenced_name) = name
                    .split_once('.')
                    .map_or(("", name), |(schema, name)| (schema, name));
                dependencies.push(StoredQueryDependency {
                    class: DependencyClass::Routine,
                    slot: slot as u16,
                    identity: StoredDependencyIdentity::RoutineOid(crate::storage::routine_oid(
                        &routine,
                    )),
                    referenced_columns: 0,
                    schema: routine.schema_for(txid),
                    name: routine.name_for(txid),
                    referenced_schema: SqlName::parse(referenced_schema)?,
                    referenced_name: SqlName::parse(referenced_name)?,
                })?;
            }
        }
        Ok(())
    }

    fn visit_expr(
        expression: &Expr<'_>,
        storage: &Storage,
        txid: u32,
        resolver: &DependencyTypes<'_, '_, '_>,
        dependencies: &mut StoredQueryDependencies,
        needs_scope: &mut bool,
    ) -> Result<(), SqlError> {
        if let Expr::Call { name, args, .. } = expression {
            record_call(
                name,
                args,
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
        record_call(
            name,
            args,
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
                    source.columns[column] = definition.columns()[column].name.as_str();
                }
            }
            ResolvedRelation::View(slot) => {
                let view = storage.view(slot);
                let user = crate::sql::eval::funcs::system::session_user_owned();
                let view_path =
                    storage.compute_path(view.creation_path.as_str(), user.as_str(), txid);
                let mut described = [crate::sql::types::ColDesc::new("", 0, 0); MAX_COLUMNS];
                source.class = DependencyClass::View;
                source.slot = slot;
                source.n_columns = super::describe_stored_query(
                    view.sql.as_str(),
                    storage,
                    txid,
                    view_path,
                    storage.view_dependencies(slot),
                    arena,
                    &mut described,
                )?;
                for (column, described) in described.iter().enumerate().take(source.n_columns) {
                    source.columns[column] = described.name;
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
) -> Result<(), SqlError> {
    let mut failure = None;
    expression.for_each_column_reference(&mut |qualifier, name| {
        if failure.is_some() {
            return;
        }
        let resolved = match scope.find_column(qualifier, name) {
            Ok(resolved) => resolved,
            Err(error) => {
                failure = Some(error);
                return;
            }
        };
        if let super::ResolvedColumn::Table(table, column) = resolved
            && let Err(error) = dependencies.mark_referenced_column(
                DependencyClass::Table,
                scope.slots[table],
                column,
            )
        {
            failure = Some(error);
        }
    });
    failure.map_or(Ok(()), Err)
}

fn collect_set_tree<'a>(
    tree: &'a SetTree<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
) -> Result<(), SqlError> {
    match tree {
        SetTree::Select(select) => {
            collect_select(select, storage, txid, path, ctes, dependencies, arena)
        }
        SetTree::Op { left, right, .. } => {
            collect_set_tree(left, storage, txid, path, ctes, dependencies, arena)?;
            collect_set_tree(right, storage, txid, path, ctes, dependencies, arena)
        }
    }
}

fn collect_table_ref<'a>(
    table: &'a TableRef<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
) -> Result<(), SqlError> {
    if let Some(select) = table.subquery {
        collect_select(select, storage, txid, path, ctes, dependencies, arena)?;
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
                    schema: view.schema,
                    name: view.name,
                    referenced_schema: SqlName::parse(table.schema.unwrap_or(""))?,
                    referenced_name: SqlName::parse(table.table)?,
                })?;
            }
            Some(ResolvedRelation::Catalog) | None => {}
        }
    }
    if let Some(arguments) = table.func_args {
        for argument in arguments {
            collect_expression(argument, storage, txid, path, ctes, dependencies, arena)?;
        }
    }
    if let Some(functions) = table.rows_from {
        for function in functions {
            collect_table_ref(function, storage, txid, path, ctes, dependencies, arena)?;
        }
    }
    Ok(())
}

fn collect_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    txid: u32,
    path: &PathContext,
    ctes: CteNames<'a>,
    dependencies: &mut StoredQueryDependencies,
    arena: &Arena,
) -> Result<(), SqlError> {
    if let Expr::Cast { type_name, .. } = expression {
        collect_type(type_name, storage, txid, path, dependencies)?;
    }
    if let Expr::Call { name, args, .. } = expression
        && matches!(*name, "nextval" | "currval" | "setval")
        && let Some(sequence_name) = args.first().and_then(|argument| regclass_literal(argument))
    {
        collect_sequence(sequence_name, storage, txid, path, dependencies)?;
    }
    let mut child =
        |expression| collect_expression(expression, storage, txid, path, ctes, dependencies, arena);
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
            collect_select(select, storage, txid, path, ctes, dependencies, arena)
        }
        Expr::InSubquery {
            operand, select, ..
        } => {
            child(operand)?;
            collect_select(select, storage, txid, path, ctes, dependencies, arena)
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
