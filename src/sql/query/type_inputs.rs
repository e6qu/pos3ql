//! Schema-only discovery of UNKNOWN inputs whose destination runs array input.

use crate::mem::arena::Arena;
use crate::sql::type_inputs::InputKind;
use crate::sql::{
    ast::{self, Expr, Select, SelectItem, Stmt},
    eval::{ColumnLookup, RuntimeColumnTypes, SqlError},
    exec::ColTypeResolver,
    types::oid,
};
use crate::sql_err;
use crate::storage::{NamedTypeKind, RoutineCallKind, RoutineCallSelection, Storage};

type Visit<'a> = dyn FnMut(&Expr<'_>, i32, i32, InputKind) -> Result<(), SqlError> + 'a;

pub(crate) fn capture_contextual_type_inputs(
    parser: &crate::sql::parser::Parser<'_>,
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let literals = parser.string_literals();
    if literals.is_empty()
        || !(0..storage.domain_count()).any(|slot| storage.domain_slot_visible_to(slot, txid))
    {
        return Ok(());
    }
    let found = arena
        .alloc_slice_with(literals.len(), |_| None)
        .map_err(|_| super::arena_full())?;
    let mark = arena.mark();
    let mut count = 0;
    let result = discover(
        statement,
        storage,
        txid,
        arena,
        &mut |expression, target, type_mod, kind| {
            let expression = match expression {
                Expr::Cast { operand, .. } => *operand,
                expression => expression,
            };
            let Expr::Str(value) = expression else {
                return Ok(());
            };
            let Some(index) = literals
                .iter()
                .position(|(expression, _)| match expression {
                    Expr::Str(source) => {
                        value.len() == source.len()
                            && core::ptr::eq(value.as_ptr(), source.as_ptr())
                    }
                    _ => false,
                })
            else {
                // A stored source owns its own admitted inputs, rather than the
                // raw statement currently being analyzed.
                return Ok(());
            };
            if let Some(previous) =
                found[..count]
                    .iter()
                    .flatten()
                    .find(|(previous, _, _, previous_kind)| {
                        *previous == index && *previous_kind == kind
                    })
            {
                if *previous != (index, target, type_mod, kind) {
                    return Err(sql_err!(
                        super::sqlstate::DATATYPE_MISMATCH,
                        "SQL input occurrence has conflicting destination types"
                    ));
                }
                return Ok(());
            }
            found[count] = Some((index, target, type_mod, kind));
            count += 1;
            Ok(())
        },
    );
    // Only copied source ordinals and type identities escape schema scratch.
    unsafe { arena.rewind_to(mark) };
    result?;
    let keys = arena
        .alloc_slice_with(count, |_| (0usize, 0usize, InputKind::ExplicitCast))
        .map_err(|_| super::arena_full())?;
    for (key, (index, target, type_mod, kind)) in keys
        .iter_mut()
        .zip(found[..count].iter().flatten().copied())
    {
        let source = literals[index].1;
        if kind == InputKind::ImplicitLiteral {
            parser.add_contextual_type_input(literals[index].0, target, type_mod)?;
            *key = (source.start, source.end, kind);
        } else {
            let candidate = parser
                .type_input_candidates()
                .iter()
                .find(|candidate| candidate.start == source.start && candidate.kind == kind)
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::DATA_EXCEPTION,
                        "explicit SQL input occurrence is missing"
                    )
                })?;
            *key = (candidate.start, candidate.end, kind);
        }
    }
    parser.reorder_type_input_candidates(keys)?;
    Ok(())
}

fn discover(
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

struct Walker<'a, 'v, 'callback> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    visit: &'v mut Visit<'callback>,
}

impl Walker<'_, '_, '_> {
    fn input(&mut self, expression: &Expr<'_>, target: i32, type_mod: i32) -> Result<(), SqlError> {
        if matches!(expression, Expr::Str(_))
            && matches!(
                NamedTypeKind::from_oid(target),
                Some((NamedTypeKind::Domain, _, true))
            )
        {
            (self.visit)(expression, target, type_mod, InputKind::ImplicitLiteral)?;
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
        let has_stored_name = (0..self.storage.routine_count()).any(|slot| {
            let Some(identity) = self.storage.routine_identity_for(slot, self.txid) else {
                return false;
            };
            if let Some(oid) = bound_oid {
                return identity.oid == oid;
            }
            name == identity.name.as_str()
                || name
                    .strip_suffix(identity.name.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.'))
        });
        if !has_stored_name {
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
        if let Expr::Cast {
            operand: Expr::Str(_),
            type_name,
            type_mod,
            ..
        } = expression
            && let Some(target) =
                crate::sql::type_inputs::target_oid(self.storage, self.txid, type_name)
        {
            (self.visit)(expression, target, *type_mod, InputKind::ExplicitCast)?;
        }
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
        targets: Option<&[(i32, i32)]>,
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
        if let Some(from) = select.from {
            self.table_ref(&from.base, &columns, &row)?;
            for join in from.joins {
                self.table_ref(&join.table, &columns, &row)?;
                if let Some(expression) = join.on {
                    self.expression(expression, &columns, &row)?;
                }
            }
        }
        let mut output_index = 0usize;
        let mut target_inputs = crate::mem::arena::ArenaList::new(self.arena);
        for item in select.items {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                if matches!(item, SelectItem::Expr { .. })
                    && let Some((target, type_mod)) =
                        targets.and_then(|targets| targets.get(output_index))
                {
                    if matches!(expression, Expr::Str(_))
                        && matches!(
                            NamedTypeKind::from_oid(*target),
                            Some((NamedTypeKind::Domain, _, true))
                        )
                    {
                        target_inputs
                            .push((*expression, *target, *type_mod))
                            .map_err(|_| super::arena_full())?;
                    }
                }
                self.expression(expression, &columns, &row)?;
            }
            if targets.is_some() {
                let width = match item {
                    SelectItem::Expr { .. } => 1,
                    SelectItem::Wildcard => scope.as_ref().map_or(0, |scope| scope.star_columns()),
                    SelectItem::TableWildcard(name) => scope
                        .as_ref()
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_TABLE,
                                "missing FROM-clause entry for table \"{}\"",
                                name
                            )
                        })?
                        .qualified_star_columns(name)?,
                    SelectItem::RecordStar(expression) => {
                        crate::sql::exec::record_shape(expression, &columns, |_, _| {})?
                            .ok_or_else(|| {
                                sql_err!(
                                    super::sqlstate::FEATURE_NOT_SUPPORTED,
                                    "record output shape is unavailable"
                                )
                            })?
                    }
                };
                output_index = output_index
                    .checked_add(width)
                    .ok_or_else(super::arena_full)?;
            }
        }
        for expression in select
            .where_clause
            .into_iter()
            .chain(select.having)
            .chain(select.order_by.iter().map(|order| order.expression))
            .chain(select.group_by.iter().copied())
            .chain(select.distinct_on.iter().copied())
            .chain(select.offset)
            .chain(select.limit)
        {
            self.expression(expression, &columns, &row)?;
        }
        for (expression, target, type_mod) in target_inputs.as_slice() {
            self.input(expression, *target, *type_mod)?;
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
        let storage = self.storage;
        let txid = self.txid;
        let arena = self.arena;
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
        if let Some(from) = extra {
            self.table_ref(&from.base, &columns, &row)?;
            for join in from.joins {
                self.table_ref(&join.table, &columns, &row)?;
                if let Some(expression) = join.on {
                    self.expression(expression, &columns, &row)?;
                }
            }
        }
        if let Stmt::Insert(insert) = statement
            && let Some(select) = insert.select
        {
            let definition = scope.defs.get(0).expect("assignment target resolved");
            let count = if insert.columns.is_empty() {
                definition.columns().len()
            } else {
                insert.columns.len()
            };
            let targets = arena
                .alloc_slice_with(count, |_| (0, -1))
                .map_err(|_| super::arena_full())?;
            for (index, target) in targets.iter_mut().enumerate() {
                let index = if insert.columns.is_empty() {
                    index
                } else {
                    definition
                        .column_index(insert.columns[index])
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_COLUMN,
                                "column \"{}\" does not exist",
                                insert.columns[index]
                            )
                        })?
                };
                let column = &definition.columns()[index];
                *target = (
                    storage
                        .routine_type_oid(column.ctype, column.user_type, txid)
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_OBJECT,
                                "assignment input type is unavailable"
                            )
                        })?,
                    column.type_mod,
                );
            }
            self.select(select, None, Some(targets))?;
        }
        if let Stmt::Merge(merge) = statement {
            self.expression(merge.on, &columns, &row)?;
        }
        super::dependencies::visit_dml_assignment_groups(
            statement,
            storage,
            txid,
            arena,
            None,
            &mut |group, assignment_columns| {
                for assignment in group {
                    self.expression(assignment.expression, assignment_columns, &row)?;
                }
                for assignment in group {
                    let column = assignment.column;
                    let target = storage
                        .routine_type_oid(column.ctype, column.user_type, txid)
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_OBJECT,
                                "assignment input type is unavailable"
                            )
                        })?;
                    self.input(assignment.expression, target, column.type_mod)?;
                }
                Ok(())
            },
        )?;
        let filter = match statement {
            Stmt::Update(update) => update.where_clause,
            Stmt::Delete(delete) => delete.where_clause,
            Stmt::Insert(insert) => insert
                .on_conflict
                .and_then(|conflict| conflict.update_where),
            _ => None,
        };
        if let Some(expression) = filter {
            self.expression(expression, &columns, &row)?;
        }
        for item in returning.items {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                self.expression(expression, &columns, &row)?;
            }
        }
        Ok(())
    }
}
