//! Owned physical input positions for cursors and suspended statement rows.

use super::*;
use crate::mem::arena::ArenaMark;
use crate::storage::{
    CursorRow, MAX_COLUMNS, RowCursor, RowCursorBuffers, SpilledRowRepresentation, rowenc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainedMode {
    Cursor,
    Statement,
}

struct PendingRow<'a> {
    row: CursorRow<'a>,
    mark: ArenaMark,
    counted: bool,
}

impl PendingRow<'_> {
    fn decode<'row>(
        &'row self,
        definition: &TableDef,
        values: &mut [Datum<'row>; MAX_COLUMNS],
    ) -> Result<(), SqlError> {
        match self.row.representation() {
            SpilledRowRepresentation::Encoded(bytes) => {
                let mut schema = [ColType::Bool; MAX_COLUMNS];
                definition.schema(&mut schema);
                rowenc::decode(bytes, &schema[..definition.n_columns], values)
            }
            SpilledRowRepresentation::Values(decoded) => {
                if decoded.len() > definition.n_columns {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "retained row exceeds its source definition"
                    ));
                }
                values[..decoded.len()].copy_from_slice(decoded);
                Ok(())
            }
        }
    }
}

enum ReaderState {
    Unstarted,
    Reading,
    Complete,
    Eliminated,
}

enum RowBounds<'a> {
    Unstarted {
        limit: Option<&'a Expr<'a>>,
        offset: Option<&'a Expr<'a>>,
    },
    Active {
        skip: u64,
        remaining: u64,
    },
}

pub(crate) struct RetainedSelect<'a> {
    items: &'a [SelectItem<'a>],
    predicate: Option<&'a Expr<'a>>,
    bounds: RowBounds<'a>,
    scope: QueryScope<'a>,
    source: RowCursor,
    decoded_columns: Option<[bool; MAX_COLUMNS]>,
    pending: Option<PendingRow<'a>>,
    state: ReaderState,
}

fn retained_expression(
    expression: &Expr<'_>,
    catalog: &dyn super::super::eval::CatalogAccess,
    statement: Option<(&Storage, u32)>,
) -> bool {
    use crate::sql::ast::UnaryOp;
    match expression {
        Expr::Column { .. }
        | Expr::SchemaColumn { .. }
        | Expr::Param(_)
        | Expr::RoutineParam { .. }
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BoundLiteral { .. }
        | Expr::BitLit(_) => true,
        Expr::Unary {
            operator: UnaryOp::Neg | UnaryOp::Not | UnaryOp::BitNot,
            operand,
            ..
        }
        | Expr::IsNull { operand, .. } => retained_expression(operand, catalog, statement),
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => {
            operator
                .operator_name()
                .is_none_or(|name| !catalog.has_operator_candidate(name))
                && retained_expression(left, catalog, statement)
                && retained_expression(right, catalog, statement)
        }
        Expr::Case {
            operand: None,
            whens,
            otherwise,
            ..
        } => {
            whens.iter().all(|(condition, value)| {
                retained_expression(condition, catalog, statement)
                    && retained_expression(value, catalog, statement)
            }) && otherwise.is_none_or(|value| retained_expression(value, catalog, statement))
        }
        Expr::Array(items) => items
            .iter()
            .all(|item| retained_expression(item, catalog, statement)),
        Expr::Subscript { base, index } => {
            retained_expression(base, catalog, statement)
                && retained_expression(index, catalog, statement)
        }
        Expr::Slice { base, lower, upper } => {
            retained_expression(base, catalog, statement)
                && lower.is_none_or(|bound| retained_expression(bound, catalog, statement))
                && upper.is_none_or(|bound| retained_expression(bound, catalog, statement))
        }
        Expr::Cast {
            operand, type_name, ..
        } if statement.is_some() => {
            ColType::from_sql_name(type_name).is_some()
                && retained_expression(operand, catalog, statement)
        }
        Expr::Call {
            name,
            args,
            star: false,
            distinct: false,
            order_by,
            over: None,
            filter: None,
            ..
        } if statement.is_some() && order_by.is_empty() => {
            let (storage, txid) = statement.expect("statement expression mode");
            !expression.is_aggregate_use()
                && !storage.has_aggregate_candidate(name, args.len(), txid)
                && !is_builtin_set_routine(name)
                && !storage.has_set_routine_candidate(name, args.len(), txid)
                && args
                    .iter()
                    .all(|argument| retained_expression(argument, catalog, statement))
        }
        _ => false,
    }
}

fn row_independent(expression: &Expr<'_>, scope: &QueryScope<'_>) -> bool {
    match expression {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BoundLiteral { .. }
        | Expr::BitLit(_)
        | Expr::Param(_) => true,
        Expr::RoutineParam {
            qualifier, name, ..
        } => scope.find_column(*qualifier, name).is_err_and(|error| {
            error.sqlstate == sqlstate::UNDEFINED_COLUMN
                || error.sqlstate == sqlstate::UNDEFINED_TABLE
        }),
        Expr::Unary { operand, .. } | Expr::IsNull { operand, .. } => {
            row_independent(operand, scope)
        }
        Expr::Binary { left, right, .. } => {
            row_independent(left, scope) && row_independent(right, scope)
        }
        Expr::Case {
            operand: None,
            whens,
            otherwise,
            ..
        } => {
            whens.iter().all(|(condition, value)| {
                row_independent(condition, scope) && row_independent(value, scope)
            }) && otherwise.is_none_or(|value| row_independent(value, scope))
        }
        Expr::Array(items) => items.iter().all(|item| row_independent(item, scope)),
        Expr::Subscript { base, index } => {
            row_independent(base, scope) && row_independent(index, scope)
        }
        Expr::Slice { base, lower, upper } => {
            row_independent(base, scope)
                && lower.is_none_or(|bound| row_independent(bound, scope))
                && upper.is_none_or(|bound| row_independent(bound, scope))
        }
        _ => false,
    }
}

fn retained_bound(expression: &Expr<'_>, catalog: &dyn super::super::eval::CatalogAccess) -> bool {
    match expression {
        Expr::Int(_) | Expr::Null | Expr::Param(_) | Expr::RoutineParam { .. } => true,
        Expr::Unary {
            operator: crate::sql::ast::UnaryOp::Neg,
            operand,
            ..
        } => retained_bound(operand, catalog),
        Expr::Binary { left, right, .. } => {
            retained_expression(expression, catalog, None)
                && retained_bound(left, catalog)
                && retained_bound(right, catalog)
        }
        _ => false,
    }
}

fn validate_statement_casts(
    expression: &Expr<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    safe: &mut bool,
) -> Result<(), SqlError> {
    if let Expr::Cast {
        operand, type_name, ..
    } = expression
    {
        let target = ColType::from_sql_name(type_name).expect("admitted builtin cast");
        // An explicit array cast supplies the constructor's element type,
        // including ARRAY[], before whole-value cast lookup.
        let source = if matches!(operand, Expr::Array(_)) && matches!(target, ColType::Array(_)) {
            target.oid()
        } else {
            super::super::exec::infer_routine_argument_oid(operand, columns)?
        };
        *safe &= crate::sql::catalog::defined_cast_path(
            columns.storage,
            columns.txid,
            source,
            target.oid(),
        )
        .is_none();
        if let ColType::Array(element) = target {
            crate::sql::exec::visit_array_constructor_conversions(
                operand,
                element,
                columns,
                &mut |source, target| {
                    *safe &= crate::sql::catalog::defined_cast_path(
                        columns.storage,
                        columns.txid,
                        source,
                        target,
                    )
                    .is_none();
                    Ok(())
                },
            )?;
        }
    }
    walk_children(expression, &mut |child| {
        validate_statement_casts(child, columns, safe)
    })
}

struct DefaultExpansion<'parent> {
    oid: i32,
    parent: Option<&'parent DefaultExpansion<'parent>>,
}

struct DefaultPath<'storage> {
    storage: &'storage Storage,
    previous: crate::storage::PathContext,
}

impl Drop for DefaultPath<'_> {
    fn drop(&mut self) {
        self.storage.swap_path(self.previous);
    }
}

// Bind stored calls and builtin operators before reading any source row.
// Cursor mode retains its original scalar-expression subset.
fn bind_expression<'a>(
    expression: &'a Expr<'a>,
    arena: &'a Arena,
    columns: Option<&CatalogScopeCols<'_, 'a, '_, '_>>,
    defaults: Option<&DefaultExpansion<'_>>,
) -> Result<&'a Expr<'a>, SqlError> {
    let full = |error: crate::mem::arena::ArenaFull| {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "retained query plan allocation failed: {}",
            error
        )
    };
    let bound = match expression {
        Expr::Unary {
            operator, operand, ..
        } => Expr::Unary {
            source_position: expression.source_position(),
            operator: *operator,
            operand: bind_expression(operand, arena, columns, defaults)?,
        },
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: bind_expression(operand, arena, columns, defaults)?,
            negated: *negated,
        },
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => {
            let left = bind_expression(left, arena, columns, defaults)?;
            let right = bind_expression(right, arena, columns, defaults)?;
            if let Some(symbol) = operator.operator_name() {
                let encoded = crate::stack_format!(
                    260,
                    "{}pg_catalog\u{1f}{}",
                    crate::sql::ast::CATALOG_OPERATOR_CALL_PREFIX,
                    symbol
                );
                let name = arena.alloc_str(encoded.as_str()).map_err(full)?;
                let args = arena.alloc_slice_copy(&[left, right]).map_err(full)?;
                Expr::Call {
                    kind: crate::sql::ast::CallKind::SqlSyntax,
                    bound_oid: None,
                    bound_strict: None,
                    source_position: expression.source_position(),
                    name,
                    args,
                    argument_names: &[],
                    variadic: false,
                    star: false,
                    distinct: false,
                    order_by: &[],
                    over: None,
                    filter: None,
                }
            } else {
                Expr::Binary {
                    source_position: expression.source_position(),
                    operator: *operator,
                    left,
                    right,
                }
            }
        }
        Expr::Case {
            operand: None,
            whens,
            otherwise,
            synthetic,
        } => {
            let whens = arena.alloc_slice_copy(whens).map_err(full)?;
            for (condition, value) in whens.iter_mut() {
                *condition = bind_expression(condition, arena, columns, defaults)?;
                *value = bind_expression(value, arena, columns, defaults)?;
            }
            Expr::Case {
                operand: None,
                whens,
                otherwise: otherwise
                    .map(|value| bind_expression(value, arena, columns, defaults))
                    .transpose()?,
                synthetic: *synthetic,
            }
        }
        Expr::Array(items) => {
            let items = arena.alloc_slice_copy(items).map_err(full)?;
            for item in items.iter_mut() {
                *item = bind_expression(item, arena, columns, defaults)?;
            }
            Expr::Array(items)
        }
        Expr::Subscript { base, index } => Expr::Subscript {
            base: bind_expression(base, arena, columns, defaults)?,
            index: bind_expression(index, arena, columns, defaults)?,
        },
        Expr::Slice { base, lower, upper } => Expr::Slice {
            base: bind_expression(base, arena, columns, defaults)?,
            lower: lower
                .map(|value| bind_expression(value, arena, columns, defaults))
                .transpose()?,
            upper: upper
                .map(|value| bind_expression(value, arena, columns, defaults))
                .transpose()?,
        },
        Expr::BoundDefault {
            expression,
            type_oid,
        } => Expr::BoundDefault {
            expression: bind_expression(expression, arena, columns, defaults)?,
            type_oid: *type_oid,
        },
        Expr::Cast {
            operand,
            type_name,
            type_mod,
            binding,
            ..
        } if columns.is_some() => {
            let operand = bind_expression(operand, arena, columns, defaults)?;
            let columns = columns.expect("statement type resolver");
            let binding = match ColType::from_sql_name(type_name) {
                Some(target) => {
                    let source = if matches!(operand, Expr::Array(_))
                        && matches!(target, ColType::Array(_))
                    {
                        target.oid()
                    } else {
                        super::super::exec::infer_routine_argument_oid(operand, columns)?
                    };
                    let mut defined = crate::sql::catalog::defined_cast_path(
                        columns.storage,
                        columns.txid,
                        source,
                        target.oid(),
                    )
                    .is_some();
                    if let ColType::Array(element) = target {
                        crate::sql::exec::visit_array_constructor_conversions(
                            operand,
                            element,
                            columns,
                            &mut |source, target| {
                                defined |= crate::sql::catalog::defined_cast_path(
                                    columns.storage,
                                    columns.txid,
                                    source,
                                    target,
                                )
                                .is_some();
                                Ok(())
                            },
                        )?;
                    }
                    if !defined {
                        crate::sql::ast::CastBinding::Builtin
                    } else {
                        *binding
                    }
                }
                None => *binding,
            };
            Expr::Cast {
                source_position: expression.source_position(),
                operand,
                type_name,
                type_mod: *type_mod,
                binding,
            }
        }
        Expr::Call {
            kind,
            name,
            bound_oid,
            bound_strict,
            source_position,
            args,
            argument_names,
            variadic,
            star,
            distinct,
            order_by,
            over,
            filter,
        } if columns.is_some() => {
            let columns = columns.expect("statement type resolver");
            let catalog = storage_catalog(columns.storage, arena, columns.txid);
            let args = arena.alloc_slice_copy(args).map_err(full)?;
            for argument in args.iter_mut() {
                *argument = bind_expression(argument, arena, Some(columns), defaults)?;
            }
            let argument_oids = arena
                .alloc_slice_with(args.len(), |_| super::super::types::oid::UNKNOWN)
                .map_err(full)?;
            for (output, argument) in argument_oids.iter_mut().zip(args.iter()) {
                *output = super::super::exec::infer_routine_argument_oid(argument, columns)?;
            }
            let target = catalog.resolve_routine_call(
                name,
                *kind,
                *bound_oid,
                argument_names,
                *variadic,
                argument_oids,
            )?;
            let builtin_schema = match &target {
                Some(super::super::eval::RoutineCallTarget::Builtin { schema, .. }) => {
                    Some(*schema)
                }
                _ => None,
            };
            let (bound_oid, bound_strict, args, argument_names, variadic) = match target {
                Some(super::super::eval::RoutineCallTarget::Stored {
                    oid,
                    argument_mapping,
                    ..
                }) => {
                    let selected_strict = match *bound_strict {
                        Some(strict) => strict,
                        None => {
                            let slot = columns
                                .storage
                                .routine_slot_by_oid(oid, columns.txid)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::UNDEFINED_FUNCTION,
                                        "captured function with OID {} does not exist",
                                        oid
                                    )
                                })?;
                            columns
                                .storage
                                .routine_for(slot, columns.txid)
                                .attributes
                                .strict
                        }
                    };
                    let (args, expanded_variadic, expanded) = bind_defaults(
                        oid,
                        args,
                        &argument_mapping,
                        *variadic,
                        columns,
                        arena,
                        defaults,
                    )?;
                    (
                        Some(oid),
                        Some(selected_strict),
                        args,
                        if expanded { &[][..] } else { *argument_names },
                        expanded_variadic,
                    )
                }
                _ => (
                    *bound_oid,
                    *bound_strict,
                    &*args,
                    *argument_names,
                    *variadic,
                ),
            };
            let name = if let Some(schema) = builtin_schema
                && !name.contains('.')
            {
                arena
                    .alloc_str(crate::stack_format!(260, "{}.{}", schema, name).as_str())
                    .map_err(full)?
            } else {
                name
            };
            Expr::Call {
                kind: *kind,
                name,
                bound_oid,
                bound_strict,
                source_position: *source_position,
                args,
                argument_names,
                variadic,
                star: *star,
                distinct: *distinct,
                order_by,
                over: *over,
                filter: *filter,
            }
        }
        _ => return Ok(expression),
    };
    Ok(arena.alloc(bound).map_err(full)?)
}

fn bind_defaults<'a>(
    oid: i32,
    arguments: &'a [&'a Expr<'a>],
    mapping: &[u8; crate::storage::MAX_ROUTINE_ARGUMENTS],
    variadic: bool,
    columns: &CatalogScopeCols<'_, 'a, '_, '_>,
    arena: &'a Arena,
    parent: Option<&DefaultExpansion<'_>>,
) -> Result<(&'a [&'a Expr<'a>], bool, bool), SqlError> {
    let storage = columns.storage;
    let txid = columns.txid;
    let slot = storage.routine_slot_by_oid(oid, txid).ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "captured function with OID {} does not exist",
            oid
        )
    })?;
    let declared = storage.routine_for(slot, txid);
    let missing = (0..declared.argument_count)
        .any(|index| !mapping[..arguments.len()].contains(&(index as u8)));
    if !missing {
        return Ok((arguments, variadic, false));
    }
    let mut ancestor = parent;
    while let Some(expansion) = ancestor {
        if expansion.oid == oid {
            return Err(sql_err!(
                sqlstate::STATEMENT_TOO_COMPLEX,
                "recursive function defaults cannot be expanded"
            ));
        }
        ancestor = expansion.parent;
    }
    let expansion = DefaultExpansion { oid, parent };
    let dependencies = storage.snapshot_routine_dependencies_for(slot, txid, arena)?;
    let user = super::super::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(declared.creation_path.as_str(), user.as_str(), txid);
    let _path = DefaultPath {
        storage,
        previous: storage.swap_path(path),
    };
    let mut bound = ArenaList::new(arena);
    let mut explicit_variadic = variadic;
    for index in 0..declared.argument_count {
        let mut supplied = false;
        for (argument, mapped) in arguments.iter().zip(mapping) {
            if usize::from(*mapped) == index {
                bound.push(*argument).map_err(|error| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "retained function inputs allocation failed: {}",
                        error
                    )
                })?;
                supplied = true;
            }
        }
        if supplied {
            continue;
        }
        let parameter = declared
            .parameter_for_input(index)
            .expect("selected input parameter");
        let source = parameter.mode.default().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "selected function has no default for an omitted argument"
            )
        })?;
        let expression = bind_routine_parameter_default(
            source.as_str(),
            declared
                .parameters()
                .iter()
                .enumerate()
                .filter(|(_, parameter)| parameter.mode.is_input())
                .nth(index)
                .expect("selected input parameter")
                .0,
            storage,
            txid,
            dependencies.view(),
            arena,
        )?;
        let expression = bind_expression(expression, arena, Some(columns), Some(&expansion))?;
        let type_oid = storage
            .routine_type_oid(parameter.ctype, parameter.user_type, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "selected default argument type identity is unavailable"
                )
            })?;
        let expression = arena
            .alloc(Expr::BoundDefault {
                expression,
                type_oid,
            })
            .map_err(|error| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained default expression allocation failed: {}",
                    error
                )
            })?;
        bound.push(&*expression).map_err(|error| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained function inputs allocation failed: {}",
                error
            )
        })?;
        if matches!(
            parameter.mode,
            crate::storage::RoutineParameterMode::Variadic { .. }
        ) {
            // A declared variadic default is the whole array, not one element.
            explicit_variadic = true;
        }
    }
    if bound.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
        return Err(sql_err!(
            sqlstate::TOO_MANY_ARGUMENTS,
            "routine call has too many arguments after inserting defaults"
        ));
    }
    // Scalar input evaluation already follows declaration order. Positional
    // arguments preserve it after named arguments and omitted defaults merge.
    Ok((bound.as_slice(), explicit_variadic, true))
}

impl<'a> RetainedSelect<'a> {
    pub(crate) fn default_scrollable(&self) -> bool {
        !matches!(self.state, ReaderState::Eliminated)
    }

    pub(crate) fn uses_table(&self, table: usize) -> bool {
        !matches!(self.state, ReaderState::Eliminated)
            && self.scope.slots[..self.scope.n].contains(&table)
    }

    pub(crate) fn open(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        responder: &mut Responder<'_>,
    ) -> Result<Option<Self>, SqlError> {
        Self::open_mode(
            storage,
            txid,
            statement,
            params,
            arena,
            responder,
            RetainedMode::Cursor,
        )
    }

    pub(crate) fn open_statement(
        storage: &Storage,
        txid: u32,
        statement: &'a Select<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        responder: &mut Responder<'_>,
    ) -> Result<Option<Self>, SqlError> {
        let reader = RetainedSelect::open_mode(
            storage,
            txid,
            statement,
            params,
            arena,
            responder,
            RetainedMode::Statement,
        )?;
        // SAFETY: only a physical local table without derived sources, policies,
        // inheritance or joins reaches this point. Scope arrays, names and ASTs
        // borrow the supplied arena/statement; definition images and row roots
        // have owned leases. No retained field borrows Storage. Its conservative
        // schema-resolution lifetime can therefore be extended to the arena.
        Ok(
            reader
                .map(|reader| unsafe { core::mem::transmute::<RetainedSelect<'_>, Self>(reader) }),
        )
    }

    fn open_mode(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        responder: &mut Responder<'_>,
        mode: RetainedMode,
    ) -> Result<Option<Self>, SqlError> {
        let Some(from) = statement.from.as_ref() else {
            return Ok(None);
        };
        if !from.joins.is_empty()
            || from.base.is_function_source()
            || from.base.subquery.is_some()
            || from.base.cte.is_some()
            || from.base.sample.is_some()
            || !statement.with.is_empty()
            || statement.set_body.is_some()
            || statement.distinct
            || !statement.distinct_on.is_empty()
            || !statement.group_by.is_empty()
            || !statement.grouping_sets.is_empty()
            || statement.having.is_some()
            || !statement.order_by.is_empty()
            || !statement.locking.is_empty()
            || statement.with_ties
            || matches!(
                storage.resolve_relation(from.base.schema, from.base.table, txid),
                Some(crate::storage::ResolvedRelation::View(_))
            )
        {
            return Ok(None);
        }
        let catalog = storage_catalog(storage, arena, txid);
        let expression_mode = (mode == RetainedMode::Statement).then_some((storage, txid));
        if !statement.items.iter().all(|item| match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
            SelectItem::Expr { expression, .. } => {
                retained_expression(expression, &catalog, expression_mode)
            }
            SelectItem::RecordStar(_) => false,
        }) || statement
            .where_clause
            .is_some_and(|predicate| !retained_expression(predicate, &catalog, expression_mode))
        {
            return Ok(None);
        }
        if [statement.limit, statement.offset]
            .into_iter()
            .flatten()
            .any(|bound| !retained_bound(bound, &catalog))
        {
            return Ok(None);
        }
        let scope = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
        if scope.n != 1 || scope.derived[0].is_some() || scope.slots[0] == usize::MAX {
            return Ok(None);
        }
        let columns = CatalogScopeCols {
            scope: &scope,
            outer_scope: None,
            storage,
            txid,
            arena,
        };
        for bound in [statement.limit, statement.offset].into_iter().flatten() {
            let (oid, _) = super::super::exec::infer_type_res(bound, &columns)?;
            if oid != super::super::types::oid::UNKNOWN
                && !matches!(
                    catalog
                        .domain_base_type(oid)
                        .or_else(|| ColType::from_oid(oid)),
                    Some(ColType::Int2 | ColType::Int4 | ColType::Int8)
                )
            {
                return Ok(None);
            }
        }
        if mode == RetainedMode::Statement {
            let mut safe_casts = true;
            for expression in statement
                .items
                .iter()
                .filter_map(|item| match item {
                    SelectItem::Expr { expression, .. } => Some(*expression),
                    _ => None,
                })
                .chain(statement.where_clause)
            {
                validate_statement_casts(expression, &columns, &mut safe_casts)?;
            }
            if !safe_casts {
                return Ok(None);
            }
        }
        let binding_columns = (mode == RetainedMode::Statement).then_some(&columns);
        let table = scope.slots[0];
        let definition = scope.defs.stored(0).expect("resolved physical source");
        if definition.kind != crate::storage::TableKind::Local
            || definition.partition.is_partitioned()
            || (from.base.inheritance == crate::sql::ast::RelationInheritance::Descendants
                && storage.relation_has_descendants(table, txid))
        {
            return Ok(None);
        }
        let role = storage.current_role_slot(txid).ok_or_else(|| {
            sql_err!(
                sqlstate::INSUFFICIENT_PRIVILEGE,
                "current role is not present in the role catalog"
            )
        })?;
        let role = scope.authorization_roles[0].map_or(role, usize::from);
        if storage.row_security_applies(table, role, txid) {
            return Ok(None);
        }
        check_select_types(statement, &scope, storage, txid, arena, None)?;
        check_select_constants(statement, arena, storage, txid)?;
        let demand = streaming_pax_columns(&scope, from, statement.items, statement.where_clause);
        scan::admit_sources(storage, &scope, txid, demand)?;
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        let width = describe_scope_items(
            statement.items,
            &scope,
            None,
            storage,
            txid,
            arena,
            &mut columns,
        )?;
        let subqueries = SubqueryValues {
            scalars: &[],
            lists: &[],
        };
        patch_subquery_column_types(
            statement.items,
            Some(&scope),
            &subqueries,
            params,
            storage,
            txid,
            arena,
            &mut columns[..width],
        )?;
        let mut source = RowCursor::closed();
        let decoded_columns = demand.selected_mask(0).map(|mask| {
            core::array::from_fn(|column| column < u64::BITS as usize && mask & (1 << column) != 0)
        });
        let items = arena.alloc_slice_copy(statement.items).map_err(|error| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained query plan allocation failed: {}",
                error
            )
        })?;
        for item in items.iter_mut() {
            if let SelectItem::Expr { expression, .. } = item {
                *expression = bind_expression(expression, arena, binding_columns, None)?;
            }
        }
        let predicate = statement
            .where_clause
            .map(|predicate| {
                if mode == RetainedMode::Statement {
                    return Ok(predicate);
                }
                let predicate = simplify_qual(predicate, arena)?;
                let predicate = plan::fold_null(predicate, &scope, arena)?;
                reorder_qual(predicate, &scope, arena)
            })
            .transpose()?;
        let (predicate, eliminated) = match predicate {
            Some(predicate)
                if mode == RetainedMode::Cursor && row_independent(predicate, &scope) =>
            {
                let hooks = EvalHooks {
                    catalog: Some(&catalog),
                    ..super::super::eval::NO_HOOKS
                };
                let value = eval_full(
                    predicate,
                    arena,
                    params,
                    &super::super::eval::NoColumns,
                    &hooks,
                )?;
                let value = super::super::eval::boolean_argument(value, "WHERE")?;
                (None, !matches!(value, Datum::Bool(true)))
            }
            Some(predicate) => (
                Some(predicate),
                plan_time_bool(predicate, arena) == Some(false),
            ),
            None => (None, false),
        };
        let predicate = predicate
            .map(|predicate| bind_expression(predicate, arena, binding_columns, None))
            .transpose()?;
        if mode == RetainedMode::Statement
            && !items.iter().any(|item| match item {
                SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                    super::super::eval::routine_input_handoff(expression, Some(&catalog))
                }
                SelectItem::Wildcard | SelectItem::TableWildcard(_) => false,
            })
            && !predicate.is_some_and(|predicate| {
                super::super::eval::routine_input_handoff(predicate, Some(&catalog))
            })
        {
            // Binding can prove a cast/operator cannot suspend. Keep such
            // queries on the general executor's existing index access paths.
            return Ok(None);
        }
        let bounds = RowBounds::Unstarted {
            limit: statement
                .limit
                .map(|bound| bind_expression(bound, arena, binding_columns, None))
                .transpose()?,
            offset: statement
                .offset
                .map(|bound| bind_expression(bound, arena, binding_columns, None))
                .transpose()?,
        };
        let bounds = if mode == RetainedMode::Statement {
            RowBounds::Active {
                skip: super::super::exec::eval_offset_pub(
                    statement.offset,
                    arena,
                    params,
                    &super::super::eval::NO_HOOKS,
                )?,
                remaining: super::super::exec::eval_limit_pub(
                    statement.limit,
                    arena,
                    params,
                    &super::super::eval::NO_HOOKS,
                )?,
            }
        } else {
            bounds
        };
        if mode == RetainedMode::Statement {
            storage.lock_table(
                txid,
                table,
                crate::sql::ast::TableLockMode::AccessShare,
                false,
            )?;
        }
        if !eliminated {
            scan::validate_materialized_view_reads(storage, &scope, None, arena, txid)?;
            source.open(storage, table, txid, arena)?;
        }
        responder
            .row_description(&columns[..width])
            .map_err(|_| match mode {
                RetainedMode::Cursor => super::super::cursor_description_too_large(),
                RetainedMode::Statement => sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "retained statement row description exceeds output capacity"
                ),
            })?;
        Ok(Some(Self {
            items,
            predicate,
            bounds,
            scope,
            source,
            decoded_columns,
            pending: None,
            state: if eliminated {
                ReaderState::Eliminated
            } else {
                ReaderState::Unstarted
            },
        }))
    }

    /// Captures input and records scan statistics before any row expression
    /// runs. The caller owns predicate and projection progress while this row
    /// remains pending, including across mutable function execution.
    pub(crate) fn prepare_statement_row(
        &mut self,
        storage: &Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
    ) -> Result<bool, SqlError> {
        assert!(
            matches!(self.bounds, RowBounds::Active { .. }),
            "statement bounds must be prepared"
        );
        if matches!(self.bounds, RowBounds::Active { remaining: 0, .. }) {
            self.source.close();
            if !matches!(self.state, ReaderState::Eliminated) {
                self.state = ReaderState::Complete;
            }
        }
        match self.state {
            ReaderState::Complete | ReaderState::Eliminated => return Ok(false),
            ReaderState::Unstarted => {
                storage.record_relation_scan(txid, self.scope.slots[0], None, 0, observed_at)?;
                self.state = ReaderState::Reading;
            }
            ReaderState::Reading => {}
        }
        check_timeout()?;
        if self.pending.is_none() {
            let mark = state_arena.mark();
            let row = match self.source.next(
                buffers,
                state_arena,
                state_arena,
                self.decoded_columns.as_ref(),
            ) {
                Ok(row) => row,
                Err(error) => {
                    // Failed reads retain navigation state, not front scratch.
                    unsafe { state_arena.rewind_to(mark) };
                    return Err(error);
                }
            };
            let Some(row) = row else {
                unsafe { state_arena.rewind_to(mark) };
                self.state = ReaderState::Complete;
                self.source.close();
                return Ok(false);
            };
            self.pending = Some(PendingRow {
                row,
                mark,
                counted: false,
            });
        }
        let pending = self.pending.as_mut().expect("captured statement row");
        if !pending.counted {
            storage.record_relation_tuple_read(txid, self.scope.slots[0], None)?;
            pending.counted = true;
        }
        Ok(true)
    }

    fn with_statement_row<'work, R>(
        &'work self,
        each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let pending = self.pending.as_ref().expect("prepared statement row");
        let definition = self
            .scope
            .defs
            .stored(0)
            .expect("retained source definition");
        let mut values = [Datum::Null; MAX_COLUMNS];
        pending.decode(definition, &mut values)?;
        let rowids = [Some(pending.row.rowid)];
        each(&scan::JoinRow {
            scope: &self.scope,
            values: scan::JoinRowValues::Flat {
                values: &values,
                offsets: &[0],
            },
            rowids: &rowids,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "retained statement expression execution"
    )]
    pub(crate) fn evaluate_statement_predicate<'work, 'frame>(
        &'work self,
        storage: &'work Storage,
        txid: u32,
        work: &'work Arena,
        params: &[Datum<'work>],
        sequence: Option<&dyn SequenceAccess>,
        invocations: Option<&RoutineInvocationState<'frame>>,
        frame_arena: &'frame Arena,
    ) -> Result<bool, SqlError> {
        let Some(predicate) = self.predicate else {
            return Ok(true);
        };
        let catalog = StorageCatalog {
            storage,
            routine_workspace: work,
            txid,
            invocations,
            statement_arena: Some(frame_arena),
        };
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            sequences: sequence,
            ..super::super::eval::NO_HOOKS
        };
        self.with_statement_row(|row| where_passes(predicate, work, params, row, &hooks))
    }

    pub(crate) fn statement_items_len(&self) -> usize {
        self.items.len()
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "retained statement expression execution"
    )]
    pub(crate) fn evaluate_statement_item<'work, 'frame>(
        &'work self,
        index: usize,
        storage: &'work Storage,
        txid: u32,
        work: &'work Arena,
        params: &[Datum<'work>],
        sequence: Option<&dyn SequenceAccess>,
        invocations: Option<&RoutineInvocationState<'frame>>,
        frame_arena: &'frame Arena,
    ) -> Result<&'frame [Datum<'frame>], SqlError> {
        let catalog = StorageCatalog {
            storage,
            routine_workspace: work,
            txid,
            invocations,
            statement_arena: Some(frame_arena),
        };
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            sequences: sequence,
            ..super::super::eval::NO_HOOKS
        };
        self.with_statement_row(|row| {
            let mut projected = [Datum::Null; MAX_PROJ];
            let width = project_row(
                &self.items[index..index + 1],
                &self.scope,
                row,
                work,
                params,
                &hooks,
                &mut projected,
                None,
            )?;
            let detached = frame_arena
                .alloc_slice_with(width, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (output, value) in detached.iter_mut().zip(projected[..width].iter()) {
                *output = super::super::exec::detach_routine_datum(*value, frame_arena)?;
            }
            Ok(&*detached)
        })
    }

    /// OFFSET consumes projected rows while the caller suppresses emission.
    pub(crate) fn skip_statement_row(&mut self) -> bool {
        let RowBounds::Active { skip, .. } = &mut self.bounds else {
            unreachable!("statement bounds must be prepared")
        };
        if *skip == 0 {
            return false;
        }
        *skip -= 1;
        true
    }

    pub(crate) fn finish_statement_row(&mut self, state_arena: &Arena, emitted: bool) {
        if emitted {
            let RowBounds::Active { remaining, .. } = &mut self.bounds else {
                unreachable!("statement bounds must be prepared")
            };
            *remaining = remaining.checked_sub(1).expect("prepared remaining row");
        }
        let mark = self
            .pending
            .take()
            .expect("finished statement input row")
            .mark;
        // The caller has emitted/dropped all scoped evaluation values. Taking
        // pending first releases its owned bytes before reclaiming row scratch.
        unsafe { state_arena.rewind_to(mark) };
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "retained query pipeline plumbing"
    )]
    pub(crate) fn advance<'work>(
        &mut self,
        storage: &'work Storage,
        txid: u32,
        observed_at: i64,
        params: &[Datum<'work>],
        state_arena: &'a Arena,
        work: &'work Arena,
        buffers: &mut RowCursorBuffers,
        responder: &mut Responder<'_>,
    ) -> Result<bool, SqlError> {
        if let RowBounds::Unstarted { limit, offset } = self.bounds {
            self.bounds = RowBounds::Active {
                skip: super::super::exec::eval_offset_pub(
                    offset,
                    work,
                    params,
                    &crate::sql::eval::NO_HOOKS,
                )?,
                remaining: super::super::exec::eval_limit_pub(
                    limit,
                    work,
                    params,
                    &crate::sql::eval::NO_HOOKS,
                )?,
            };
        }
        loop {
            let work_mark = work.mark();
            if !self.prepare_statement_row(storage, txid, observed_at, state_arena, buffers)? {
                return Ok(false);
            }
            let pending = self.pending.as_ref().expect("captured cursor input row");
            let definition = self
                .scope
                .defs
                .stored(0)
                .expect("retained source definition");
            let mut values = [Datum::Null; MAX_COLUMNS];
            pending.decode(definition, &mut values)?;
            let rowids = [Some(pending.row.rowid)];
            let row = scan::JoinRow {
                scope: &self.scope,
                values: scan::JoinRowValues::Flat {
                    values: &values,
                    offsets: &[0],
                },
                rowids: &rowids,
            };
            let catalog = storage_catalog(storage, work, txid);
            let hooks = EvalHooks {
                catalog: Some(&catalog),
                ..super::super::eval::NO_HOOKS
            };
            if let Some(predicate) = self.predicate
                && !where_passes(predicate, work, params, &row, &hooks)?
            {
                let mark = self.pending.take().expect("filtered input row").mark;
                unsafe {
                    state_arena.rewind_to(mark);
                    work.rewind_to(work_mark);
                }
                continue;
            }
            let mut projected = [Datum::Null; MAX_PROJ];
            let width = project_row(
                self.items,
                &self.scope,
                &row,
                work,
                params,
                &hooks,
                &mut projected,
                None,
            )?;
            let RowBounds::Active { skip, remaining } = &mut self.bounds else {
                unreachable!("row bounds initialized before requesting input")
            };
            if *skip != 0 {
                *skip -= 1;
                let mark = self.pending.take().expect("skipped input row").mark;
                unsafe {
                    state_arena.rewind_to(mark);
                    work.rewind_to(work_mark);
                }
                continue;
            }
            emit_data_row(storage, txid, work, responder, &projected[..width])
                .map_err(|_| super::super::cursor_result_too_large())??;
            *remaining -= 1;
            let mark = self.pending.take().expect("consumed input row").mark;
            // The typed result is copied into the cursor buffer before this rewind.
            unsafe { state_arena.rewind_to(mark) };
            return Ok(true);
        }
    }
}
