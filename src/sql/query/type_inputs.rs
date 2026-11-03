//! Schema-only discovery of UNKNOWN inputs whose destination runs array input.

use crate::mem::arena::Arena;
use crate::sql::{
    ast::{self, Expr, Select, SelectItem, Stmt},
    eval::{ColumnLookup, RuntimeColumnTypes, SqlError},
    exec::ColTypeResolver,
    types::oid,
};
use crate::sql_err;
use crate::storage::{ColumnMeta, NamedTypeKind, RoutineCallKind, RoutineCallSelection, Storage};

type Visit<'a> = dyn FnMut(&Expr<'_>, i32, i32) -> Result<(), SqlError> + 'a;

pub(super) fn discover(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut Visit<'_>,
) -> Result<(), SqlError> {
    let mut walker = Walker {
        storage,
        txid,
        arena,
        visit,
    };
    walker.statement(statement)
}

struct Walker<'a, 'v> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    visit: &'v mut Visit<'v>,
}

impl Walker<'_, '_> {
    fn input(&mut self, expression: &Expr<'_>, target: i32, type_mod: i32) -> Result<(), SqlError> {
        if matches!(expression, Expr::Str(_))
            && matches!(
                NamedTypeKind::from_oid(target),
                Some((NamedTypeKind::Domain, _, true))
            )
        {
            (self.visit)(expression, target, type_mod)?;
        }
        Ok(())
    }

    fn call(
        &mut self,
        name: &str,
        bound_oid: Option<i32>,
        arguments: &[&Expr<'_>],
        names: &[Option<&str>],
        variadic: bool,
        procedure: bool,
        columns: &dyn ColTypeResolver,
    ) -> Result<(), SqlError> {
        if !arguments
            .iter()
            .any(|argument| matches!(argument, Expr::Str(_)))
        {
            return Ok(());
        }
        if arguments.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
            return Err(sql_err!(
                super::sqlstate::TOO_MANY_ARGUMENTS,
                "routine call has too many arguments"
            ));
        }
        let mut types = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (ty, argument) in types.iter_mut().zip(arguments) {
            *ty = crate::sql::exec::infer_routine_argument_oid(argument, columns)?;
        }
        let selection = match bound_oid {
            Some(oid) => self
                .storage
                .function_for_captured_call(
                    oid,
                    names,
                    &types[..arguments.len()],
                    variadic,
                    self.txid,
                )
                .and_then(|routine| {
                    self.storage
                        .routine_slot_by_oid(crate::storage::routine_oid(&routine), self.txid)
                })
                .map(RoutineCallSelection::Stored),
            None => Some(self.storage.select_routine_call(
                name,
                names,
                &types[..arguments.len()],
                variadic,
                self.txid,
                if procedure {
                    RoutineCallKind::Procedure
                } else {
                    RoutineCallKind::Function
                },
            )),
        };
        let Some(RoutineCallSelection::Stored(slot)) = selection else {
            return Ok(());
        };
        let routine = self.storage.routine_for(slot, self.txid);
        let routine = if procedure {
            routine
        } else {
            self.storage
                .function_for_captured_call(
                    crate::storage::routine_oid(&routine),
                    names,
                    &types[..arguments.len()],
                    variadic,
                    self.txid,
                )
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::INTERNAL_ERROR,
                        "selected routine input contract is unavailable"
                    )
                })?
        };
        let mapping = if procedure {
            routine.procedure_call_mapping(names, arguments.len(), variadic)
        } else {
            self.storage
                .routine_call_mapping(slot, names, arguments.len(), variadic, self.txid)
                .map(|(_, mapping)| mapping)
        }
        .ok_or_else(|| {
            sql_err!(
                super::sqlstate::INTERNAL_ERROR,
                "selected routine input mapping is unavailable"
            )
        })?;
        for (written, expression) in arguments.iter().enumerate() {
            let parameter = usize::from(mapping[written]);
            if parameter == usize::from(u8::MAX) {
                continue;
            }
            let argument = routine.arguments().get(parameter).ok_or_else(|| {
                sql_err!(
                    super::sqlstate::INTERNAL_ERROR,
                    "selected routine input is unavailable"
                )
            })?;
            let mode = routine
                .parameters()
                .iter()
                .filter(|parameter| parameter.mode.is_input())
                .nth(parameter)
                .map(|parameter| parameter.mode);
            if !variadic
                && matches!(
                    mode,
                    Some(crate::storage::RoutineParameterMode::Variadic { .. })
                )
            {
                continue;
            }
            let target = self
                .storage
                .routine_type_oid(argument.ctype, argument.user_type, self.txid)
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::UNDEFINED_OBJECT,
                        "routine input type is unavailable"
                    )
                })?;
            self.input(expression, target, -1)?;
        }
        Ok(())
    }

    fn expression<'a>(
        &mut self,
        expression: &Expr<'a>,
        columns: &dyn ColTypeResolver,
        row: &dyn ColumnLookup<'a>,
    ) -> Result<(), SqlError> {
        match expression {
            Expr::Subquery(select)
            | Expr::Exists(select)
            | Expr::ArraySubquery(select)
            | Expr::RowSubquery { select, .. } => return self.select(select, Some(row), None),
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                self.expression(operand, columns, row)?;
                return self.select(select, Some(row), None);
            }
            _ => {}
        }
        super::walk_children(expression, &mut |child| {
            self.expression(child, columns, row)
        })?;
        if let Expr::Call {
            kind: ast::CallKind::Routine,
            name,
            bound_oid,
            args,
            argument_names,
            variadic,
            ..
        } = expression
        {
            self.call(
                name,
                *bound_oid,
                args,
                argument_names,
                *variadic,
                false,
                columns,
            )?;
        }
        Ok(())
    }

    fn table_ref<'a>(
        &mut self,
        table: &ast::TableRef<'a>,
        columns: &dyn ColTypeResolver,
        row: &dyn ColumnLookup<'a>,
    ) -> Result<(), SqlError> {
        if let Some(query) = table.subquery {
            self.select(query, Some(row), None)?;
        }
        if let Some(arguments) = table.func_args {
            for expression in arguments {
                self.expression(expression, columns, row)?;
            }
            let name = match table.schema {
                Some(schema) => self
                    .arena
                    .alloc_str_display(format_args!("{schema}.{}", table.table))
                    .map_err(|_| super::arena_full())?,
                None => table.table,
            };
            self.call(
                name,
                None,
                arguments,
                table.func_argument_names,
                table.func_variadic,
                false,
                columns,
            )?;
        }
        if let Some(group) = table.rows_from {
            for table in group {
                self.table_ref(table, columns, row)?;
            }
        }
        Ok(())
    }

    fn select<'a>(
        &mut self,
        select: &Select<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
        targets: Option<&[ColumnMeta]>,
    ) -> Result<(), SqlError> {
        let select = super::expand_ctes(select, self.storage, self.txid, self.arena)?;
        if let Some(tree) = select.set_body {
            self.set_tree(tree, outer)?;
        }
        let scope = match select.from.as_ref() {
            Some(from) => Some(super::QueryScope::resolve_schema(
                self.storage,
                from,
                self.txid,
                self.arena,
                outer,
            )?),
            None => None,
        };
        let schema = scope.as_ref().map(super::materialize::ScopeSchema);
        let empty = crate::sql::eval::NoColumns;
        let inner: &dyn ColumnLookup<'_> = schema
            .as_ref()
            .map_or(&empty as &dyn ColumnLookup<'_>, |schema| schema);
        let row = super::Chained { inner, outer };
        let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
        let columns = RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        for (index, item) in select.items.iter().enumerate() {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                if let Some(column) = targets.and_then(|targets| targets.get(index)) {
                    let target = self
                        .storage
                        .routine_type_oid(column.ctype, column.user_type, self.txid)
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_OBJECT,
                                "assignment input type is unavailable"
                            )
                        })?;
                    self.input(expression, target, column.type_mod)?;
                }
                self.expression(expression, &columns, &row)?;
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
        {
            self.expression(expression, &columns, &row)?;
        }
        if let Some(from) = select.from {
            self.table_ref(&from.base, &columns, &row)?;
            for join in from.joins {
                self.table_ref(&join.table, &columns, &row)?;
                if let Some(expression) = join.on {
                    self.expression(expression, &columns, &row)?;
                }
            }
        }
        Ok(())
    }

    fn set_tree<'a>(
        &mut self,
        tree: &ast::SetTree<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError> {
        match tree {
            ast::SetTree::Select(select) => self.select(select, outer, None),
            ast::SetTree::Op { left, right, .. } => {
                self.set_tree(left, outer)?;
                self.set_tree(right, outer)
            }
        }
    }

    fn statement(&mut self, statement: &Stmt<'_>) -> Result<(), SqlError> {
        match statement {
            Stmt::Select(select) => self.select(select, None, None),
            Stmt::SetQuery(query) => self.set_tree(query.body, None),
            Stmt::With { ctes, statement } => {
                for cte in *ctes {
                    self.select(cte.query, None, None)?;
                }
                let statement = super::cte::expand_dml_ctes_schema(
                    statement,
                    ctes,
                    self.storage,
                    self.txid,
                    self.arena,
                )?;
                self.statement(statement)
            }
            Stmt::Explain { statement, .. } => self.statement(statement),
            Stmt::Call {
                name,
                arguments,
                argument_names,
                variadic,
            } => {
                let catalog = crate::sql::exec::CatalogCols {
                    definition: None,
                    alias: None,
                    output_aliases: &[],
                    storage: self.storage,
                    txid: self.txid,
                    arena: self.arena,
                };
                let name = match name.schema {
                    Some(schema) => self
                        .arena
                        .alloc_str_display(format_args!("{schema}.{}", name.name))
                        .map_err(|_| super::arena_full())?,
                    None => name.name,
                };
                for argument in *arguments {
                    self.expression(argument, &catalog, &crate::sql::eval::NoColumns)?;
                }
                self.call(
                    name,
                    None,
                    arguments,
                    argument_names,
                    *variadic,
                    true,
                    &catalog,
                )
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => {
                self.dml(statement)
            }
            _ => Ok(()),
        }
    }

    fn dml(&mut self, statement: &Stmt<'_>) -> Result<(), SqlError> {
        super::dependencies::visit_dml_assignment_contexts(
            statement,
            self.storage,
            self.txid,
            self.arena,
            None,
            &mut |expression, column, _columns| {
                let target = self
                    .storage
                    .routine_type_oid(column.ctype, column.user_type, self.txid)
                    .ok_or_else(|| {
                        sql_err!(
                            super::sqlstate::UNDEFINED_OBJECT,
                            "assignment input type is unavailable"
                        )
                    })?;
                self.input(expression, target, column.type_mod)
            },
        )?;
        let (target, alias, extra, returning) = match statement {
            Stmt::Insert(insert) => (insert.table, None, None, insert.returning),
            Stmt::Update(update) => (update.table, update.alias, update.from, update.returning),
            Stmt::Delete(delete) => (delete.table, delete.alias, delete.using, delete.returning),
            Stmt::Merge(merge) => {
                let source = self
                    .arena
                    .alloc(ast::FromClause {
                        base: merge.source,
                        joins: &[],
                    })
                    .map_err(|_| super::arena_full())?;
                (
                    merge.target,
                    merge.target_alias,
                    Some(&*source),
                    merge.returning,
                )
            }
            _ => unreachable!(),
        };
        let scope = super::dependencies::dml_dependency_scope(
            self.storage,
            target,
            alias,
            extra,
            self.txid,
            self.arena,
        )?;
        let row = super::materialize::ScopeSchema(&scope);
        let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
        let columns = RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        super::for_each_dml_assignment(statement, &mut |_, _, expression| {
            self.expression(expression, &columns, &row)
        })?;
        let filter = match statement {
            Stmt::Update(update) => update.where_clause,
            Stmt::Delete(delete) => delete.where_clause,
            Stmt::Merge(merge) => Some(merge.on),
            Stmt::Insert(insert) => insert
                .on_conflict
                .and_then(|conflict| conflict.update_where),
            _ => None,
        };
        if let Some(expression) = filter {
            self.expression(expression, &columns, &row)?;
        }
        if let Some(from) = extra {
            self.table_ref(&from.base, &columns, &row)?;
            for join in from.joins {
                self.table_ref(&join.table, &columns, &row)?;
                if let Some(expression) = join.on {
                    self.expression(expression, &columns, &row)?;
                }
            }
        }
        for item in returning.items {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                self.expression(expression, &columns, &row)?;
            }
        }
        if let Stmt::Insert(insert) = statement
            && let Some(select) = insert.select
        {
            let definition = scope.defs.get(0).expect("assignment target resolved");
            let targets = if insert.columns.is_empty() {
                definition.columns()
            } else {
                let targets = self
                    .arena
                    .alloc_slice_with(insert.columns.len(), |_| ColumnMeta::EMPTY)
                    .map_err(|_| super::arena_full())?;
                for (column, name) in targets.iter_mut().zip(insert.columns) {
                    let index = definition.column_index(name).ok_or_else(|| {
                        sql_err!(
                            super::sqlstate::UNDEFINED_COLUMN,
                            "column \"{}\" does not exist",
                            name
                        )
                    })?;
                    *column = definition.columns()[index];
                }
                targets
            };
            self.select(select, None, Some(targets))?;
        }
        Ok(())
    }
}
