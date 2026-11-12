//! Retained input positions for cursors and suspended statement rows.

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
    row: RetainedRow<'a>,
    mark: ArenaMark,
    counted: bool,
    /// The saved input this row was restored from.
    saved: Option<usize>,
}

enum RetainedRow<'a> {
    Physical(CursorRow<'a>),
    Literal(&'a [u8]),
    Singleton,
    /// A saved input: its row identity and version, and projected-encoded
    /// source columns.
    Saved {
        rowid: Option<u64>,
        version: Option<crate::storage::RowVersion>,
        values: &'a [u8],
    },
    /// The latest version of a locked row that changed after the cursor's
    /// snapshot, in table encoding.
    Current {
        rowid: u64,
        version: crate::storage::RowVersion,
        bytes: &'a [u8],
    },
}

/// A saved input starts with its row identity and version; `NO_ROWID` marks
/// a source without one, and a zero version tag a row without a version.
const NO_ROWID: u64 = u64::MAX;
const INPUT_HEADER: usize = 8 + crate::storage::RowVersion::ENCODED_LEN;

/// What became of a locked row since the cursor's snapshot selected it.
pub(crate) enum LockedRow {
    Unchanged,
    /// The row no longer belongs in the result.
    Skip,
    /// Another transaction deleted it and committed.
    Deleted,
    /// Another transaction replaced it; its latest version is now pending
    /// and must pass the filter again.
    Updated,
}

/// One ORDER BY key of a sorted locking cursor: a source column.
#[derive(Clone, Copy)]
pub(crate) struct RetainedOrderKey {
    column: usize,
    descending: bool,
    nulls_first: bool,
    collation: crate::sql::ast::Collation,
}

#[expect(
    clippy::large_enum_variant,
    reason = "physical cursors retain their existing inline control storage; singleton sources need no additional arena owner"
)]
enum RetainedSource<'a> {
    Physical(RowCursor),
    Literals {
        rows: &'a [&'a [u8]],
        next: usize,
        scrollable: bool,
    },
    Singleton {
        available: bool,
    },
    /// A builtin set-returning function advanced one row at a time. Its
    /// constant arguments are evaluated when fetching starts.
    Generated {
        function: &'a TableRef<'a>,
        kind: super::generator::GeneratorKind,
        generator: Option<super::generator::RowGenerator<'a>>,
    },
}

impl RetainedSource<'_> {
    fn close(&mut self) {
        match self {
            Self::Physical(cursor) => cursor.close(),
            Self::Literals { rows, next, .. } => *next = rows.len(),
            Self::Singleton { available } => *available = false,
            Self::Generated { generator, .. } => {
                *generator = Some(super::generator::RowGenerator::Elements(None));
            }
        }
    }
}

impl PendingRow<'_> {
    fn decode<'row>(
        &'row self,
        scope: &QueryScope<'_>,
        values: &mut [Datum<'row>; MAX_COLUMNS],
    ) -> Result<(), SqlError> {
        if let RetainedRow::Literal(bytes) | RetainedRow::Saved { values: bytes, .. } = self.row {
            let definition = scope.defs.get(0).expect("literal source definition");
            for (column, value) in values[..definition.n_columns].iter_mut().enumerate() {
                *value = crate::sql::exec::decode_projected_pub(bytes, column);
            }
            return Ok(());
        }
        if let RetainedRow::Current { bytes, .. } = self.row {
            let definition = scope.defs.stored(0).expect("physical source definition");
            let mut schema = [ColType::Bool; MAX_COLUMNS];
            definition.schema(&mut schema);
            return rowenc::decode(bytes, &schema[..definition.n_columns], values);
        }
        let RetainedRow::Physical(row) = &self.row else {
            return Ok(());
        };
        let definition = scope.defs.stored(0).expect("physical source definition");
        match row.representation() {
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

enum InputBytes<'a> {
    Encoded(&'a [u8]),
    Values(&'a [Datum<'a>]),
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
    source: RetainedSource<'a>,
    decoded_columns: Option<[bool; MAX_COLUMNS]>,
    pending: Option<PendingRow<'a>>,
    state: ReaderState,
    revisit: crate::sql::cursor::Revisit,
    /// Row locks each fetched row takes, as PostgreSQL's LockRows node does.
    locking: &'a [crate::sql::ast::LockClause<'a>],
    /// A locking cursor's ORDER BY: every qualifying row is saved and sorted
    /// before the first row is locked.
    order: &'a [RetainedOrderKey],
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
            args,
            star: false,
            distinct: false,
            order_by,
            over: None,
            filter: None,
            ..
        } if statement.is_some() && order_by.is_empty() => args
            .iter()
            .all(|argument| retained_expression(argument, catalog, statement)),
        _ => false,
    }
}

fn retained_scalar_calls(
    expression: &Expr<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
) -> Result<bool, SqlError> {
    if super::super::eval::is_sql_syntax_call(expression) && expression.is_aggregate_use() {
        return Ok(false);
    }
    if let Expr::Call {
        kind: crate::sql::ast::CallKind::Routine,
        name,
        bound_oid,
        args,
        argument_names,
        variadic,
        ..
    } = expression
    {
        let mut types = [super::super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if args.len() > types.len() {
            return Ok(false);
        }
        for (target, argument) in types.iter_mut().zip(*args) {
            *target = super::super::exec::infer_routine_argument_oid(argument, columns)?;
        }
        let catalog = storage_catalog(columns.storage, columns.arena, columns.txid);
        match catalog.resolve_routine_call(
            name,
            crate::sql::ast::CallKind::Routine,
            *bound_oid,
            argument_names,
            *variadic,
            &types[..args.len()],
        )? {
            Some(super::super::eval::RoutineCallTarget::Stored { oid, .. }) => {
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
                if !matches!(
                    columns.storage.routine_for(slot, columns.txid).kind,
                    crate::storage::RoutineKind::Function { .. }
                        | crate::storage::RoutineKind::RecordFunction {
                            set_returning: false
                        }
                ) {
                    return Ok(false);
                }
            }
            Some(super::super::eval::RoutineCallTarget::Builtin { oid, .. }) => {
                let signature =
                    super::super::catalog::builtin_callable_by_oid(oid).ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "selected builtin declaration is unavailable"
                        )
                    })?;
                if signature.kind != b'f' || signature.returns_set {
                    return Ok(false);
                }
            }
            Some(super::super::eval::RoutineCallTarget::Operator { .. }) | None => {
                return Ok(false);
            }
        }
    }
    let mut scalar = true;
    walk_children(expression, &mut |child| {
        scalar &= retained_scalar_calls(child, columns)?;
        Ok(())
    })?;
    Ok(scalar)
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

// VALUES is parsed as SELECT leaves joined by UNION ALL. Encoding primitive
// literals cannot execute casts, constraints or user code while opening a cursor.
fn literal_source(select: &Select<'_>) -> Option<usize> {
    if select.from.is_some()
        || select.where_clause.is_some()
        || select.distinct
        || !select.distinct_on.is_empty()
        || !select.group_by.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || !select.order_by.is_empty()
        || select.limit.is_some()
        || select.offset.is_some()
        || select.with_ties
        || !select.with.is_empty()
        || !select.locking.is_empty()
    {
        return None;
    }
    if let Some(tree) = select.set_body {
        return literal_source_tree(tree);
    }
    select
        .items
        .iter()
        .all(|item| {
            matches!(item, SelectItem::Expr { expression, .. } if matches!(expression,
            Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_)
                | Expr::NumericLit(_) | Expr::Str(_) | Expr::BitLit(_)))
        })
        .then_some(1)
}

/// Arguments a cursor evaluates once when fetching starts: literals,
/// parameters and casts of them. Calls, column references and subqueries are
/// excluded.
fn constant_function_argument(expression: &Expr<'_>) -> bool {
    match expression {
        Expr::Param(_)
        | Expr::RoutineParam { .. }
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BoundLiteral { .. }
        | Expr::BitLit(_) => true,
        Expr::Unary { operand, .. } | Expr::Cast { operand, .. } => {
            constant_function_argument(operand)
        }
        Expr::Array(items) => items.iter().all(|item| constant_function_argument(item)),
        _ => false,
    }
}

/// A table function a cursor can advance from fixed generator state: a
/// non-volatile builtin with a generator implementation and constant
/// arguments, alone in FROM. Other function sources run when declared.
fn retained_function_source(
    tref: &TableRef<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Option<super::generator::GeneratorKind> {
    let args = tref.func_args?;
    if tref.rows_from.is_some()
        || tref.lateral
        || tref.with_ordinality
        || super::srf::sql_table_constructor(tref)
        || !args
            .iter()
            .all(|argument| constant_function_argument(argument))
    {
        return None;
    }
    let (selection, _) = super::srf::select_table_func_routine(
        tref,
        storage,
        txid,
        arena,
        &super::super::eval::NoColumns,
        None,
    )
    .ok()?;
    let crate::storage::RoutineCallSelection::Builtin { oid, .. } = selection else {
        return None;
    };
    crate::sql::catalog::builtin_callable_by_oid(oid)
        .filter(|signature| signature.catalog.volatility != "v")?;
    super::generator::GeneratorKind::for_builtin(oid, args.len())
}

fn literal_source_tree(tree: &crate::sql::ast::SetTree<'_>) -> Option<usize> {
    match tree {
        crate::sql::ast::SetTree::Select(select) => literal_source(select),
        crate::sql::ast::SetTree::Op {
            operator: crate::sql::ast::SetOp::Union,
            all: true,
            left,
            right,
            ..
        } => literal_source_tree(left)?.checked_add(literal_source_tree(right)?),
        crate::sql::ast::SetTree::Op { .. } => None,
    }
}

fn literal_source_scrollable(tree: &crate::sql::ast::SetTree<'_>) -> bool {
    match tree {
        crate::sql::ast::SetTree::Select(select) => {
            select.set_body.is_some_and(literal_source_scrollable)
        }
        crate::sql::ast::SetTree::Op {
            syntax: crate::sql::ast::SetOperationSyntax::Values,
            ..
        } => true,
        crate::sql::ast::SetTree::Op { left, right, .. } => {
            literal_source_scrollable(left) && literal_source_scrollable(right)
        }
    }
}

/// A locking cursor's ORDER BY keys when every key names a source column
/// (directly, by output name or by position); `None` keeps the statement on
/// the DECLARE-time path.
fn order_keys<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    storage: &Storage,
    arena: &'a Arena,
) -> Result<Option<&'a [RetainedOrderKey]>, SqlError> {
    let keys = arena
        .alloc_slice_with(statement.order_by.len(), |_| RetainedOrderKey {
            column: 0,
            descending: false,
            nulls_first: false,
            collation: crate::sql::ast::Collation::None,
        })
        .map_err(|_| arena_full())?;
    for (key, order) in keys.iter_mut().zip(statement.order_by) {
        let mut expression =
            crate::sql::exec::resolve_order_expr_pub(order.expression, statement.items)?;
        if let Expr::Int(position) = expression {
            let item = usize::try_from(*position)
                .ok()
                .and_then(|position| position.checked_sub(1))
                .and_then(|index| statement.items.get(index));
            if statement
                .items
                .iter()
                .any(|item| !matches!(item, SelectItem::Expr { .. }))
            {
                return Ok(None);
            }
            match item {
                Some(SelectItem::Expr {
                    expression: item, ..
                }) => expression = item,
                _ => return Ok(None),
            }
        }
        let Expr::Column { qualifier, name } = expression else {
            return Ok(None);
        };
        let Ok(ResolvedColumn::Table(0, column)) = scope.find_column(*qualifier, name) else {
            return Ok(None);
        };
        *key = RetainedOrderKey {
            column,
            descending: order.descending,
            nulls_first: order.nulls_first,
            collation: scope.expression_collation(storage, arena, expression)?,
        };
    }
    Ok(Some(keys))
}

/// PostgreSQL scans a table, function result or VALUES list backward and
/// evaluates revisited rows again; other sources are materialized under a
/// scrollable cursor. Saved outputs are returned when evaluating again cannot
/// be observed.
fn cursor_revisit(
    source: &RetainedSource<'_>,
    eliminated: bool,
    statement: &Select<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    storage: &Storage,
    txid: u32,
) -> Result<crate::sql::cursor::Revisit, SqlError> {
    use crate::sql::cursor::Revisit;
    let backward = !eliminated
        && matches!(
            source,
            RetainedSource::Physical(_)
                | RetainedSource::Generated { .. }
                | RetainedSource::Literals {
                    scrollable: true,
                    ..
                }
        );
    if !backward {
        return Ok(Revisit::SavedOutput);
    }
    let volatile = |expression: &Expr<'_>| {
        super::predicate_safety::expression_properties(
            expression,
            columns,
            storage,
            txid,
            super::predicate_safety::AnalysisStage::RuntimeProjection,
        )
        .map(|properties| properties.volatile)
    };
    if let Some(predicate) = statement.where_clause
        && volatile(predicate)?
    {
        return Ok(Revisit::Rescan);
    }
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && volatile(expression)?
        {
            return Ok(Revisit::Reproject);
        }
    }
    Ok(Revisit::SavedOutput)
}

impl<'a> RetainedSelect<'a> {
    /// PostgreSQL evaluates a function scan's arguments when execution
    /// starts. They are constants, kept in the reader's own memory.
    fn start_generator(
        &mut self,
        storage: &Storage,
        txid: u32,
        state_arena: &'a Arena,
        params: &[Datum<'a>],
    ) -> Result<(), SqlError> {
        let RetainedSource::Generated {
            function,
            kind,
            generator: generator @ None,
        } = &mut self.source
        else {
            return Ok(());
        };
        let args = function.func_args.expect("retained function arguments");
        let catalog = storage_catalog(storage, state_arena, txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..super::super::eval::NO_HOOKS
        };
        let mut values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (value, argument) in values.iter_mut().zip(args) {
            *value = eval_full(
                argument,
                state_arena,
                params,
                &super::super::eval::NoColumns,
                &hooks,
            )?;
        }
        *generator = Some(super::generator::RowGenerator::start(
            *kind,
            &values[..args.len()],
            state_arena,
        )?);
        Ok(())
    }

    pub(crate) fn revisit(&self) -> crate::sql::cursor::Revisit {
        self.revisit
    }

    /// Evaluates LIMIT and OFFSET for a cursor that applies them itself, as
    /// PostgreSQL's Limit node does when it first runs. `None` means the plan
    /// has no Limit node.
    pub(crate) fn take_cursor_limits<'work>(
        &mut self,
        work: &'work Arena,
        params: &[Datum<'a>],
    ) -> Result<Option<(u64, u64)>, SqlError>
    where
        'a: 'work,
    {
        let RowBounds::Unstarted { limit, offset } = self.bounds else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor bounds were evaluated before its plan started"
            ));
        };
        let offset =
            super::super::exec::eval_offset_pub(offset, work, params, &crate::sql::eval::NO_HOOKS)?;
        let count =
            super::super::exec::eval_limit_pub(limit, work, params, &crate::sql::eval::NO_HOOKS)?;
        self.bounds = RowBounds::Active {
            skip: 0,
            remaining: u64::MAX,
        };
        Ok((offset != 0 || count != u64::MAX).then_some((offset, count)))
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// LIMIT has returned all its rows; PostgreSQL reads no further input.
    pub(crate) fn limit_reached(&self) -> bool {
        matches!(self.bounds, RowBounds::Active { remaining: 0, .. })
    }

    pub(crate) fn locks_rows(&self) -> bool {
        !self.locking.is_empty()
    }

    pub(crate) fn sorted(&self) -> bool {
        !self.order.is_empty()
    }

    /// Orders saved inputs, stored last first, so input 0 sorts first.
    pub(crate) fn sort_saved_inputs(
        &self,
        storage: &Storage,
        txid: u32,
        entries: &mut [(u32, u32)],
        bytes: impl Fn(u32, u32) -> &'a [u8],
    ) -> Result<(), SqlError>
    where
        Self: 'a,
    {
        let key = |(offset, length): (u32, u32), column: usize| {
            crate::sql::exec::ProjectedValue::column(&bytes(offset, length)[INPUT_HEADER..], column)
        };
        crate::util::try_sort_unstable_by(entries, |left, right| {
            for order in self.order {
                let (left, right) = (key(*left, order.column), key(*right, order.column));
                let ordering = match (left.is_null(), right.is_null()) {
                    (true, true) => core::cmp::Ordering::Equal,
                    (true, false) if order.nulls_first => core::cmp::Ordering::Less,
                    (true, false) => core::cmp::Ordering::Greater,
                    (false, true) if order.nulls_first => core::cmp::Ordering::Greater,
                    (false, true) => core::cmp::Ordering::Less,
                    (false, false) => {
                        let compared = super::materialize::compare_materialized_key(
                            storage,
                            txid,
                            order.collation,
                            left,
                            right,
                        )?;
                        if order.descending {
                            compared.reverse()
                        } else {
                            compared
                        }
                    }
                };
                if !ordering.is_eq() {
                    // Reversed: the slice holds the last input first.
                    return Ok(ordering.reverse());
                }
            }
            Ok(core::cmp::Ordering::Equal)
        })
    }

    fn pending_identity(&self) -> Result<(u64, crate::storage::RowVersion), SqlError> {
        let pending = self.pending.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor has no pending row to lock"
            )
        })?;
        match &pending.row {
            RetainedRow::Physical(row) => Ok((row.rowid, row.version)),
            RetainedRow::Saved {
                rowid: Some(rowid),
                version: Some(version),
                ..
            } => Ok((*rowid, *version)),
            RetainedRow::Current { rowid, version, .. } => Ok((*rowid, *version)),
            _ => Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor row has no identity to lock"
            )),
        }
    }

    /// Takes the pending row's locks. `Ok(false)` means SKIP LOCKED passed it
    /// over; a conflicting lock is a lock wait.
    pub(crate) fn lock_pending(&self, storage: &Storage, txid: u32) -> Result<bool, SqlError> {
        let (rowid, _) = self.pending_identity()?;
        super::lock_clauses_row(storage, txid, self.locking, &self.scope, &[Some(rowid)])
    }

    /// Rechecks a locked row against its latest version, as PostgreSQL's
    /// EvalPlanQual does. An update by another transaction makes its latest
    /// version the pending row.
    pub(crate) fn recheck_pending(
        &mut self,
        storage: &Storage,
        txid: u32,
        state_arena: &'a Arena,
    ) -> Result<LockedRow, SqlError> {
        let (rowid, seen) = self.pending_identity()?;
        let table = self.scope.slots[0];
        Ok(
            match storage.recheck_locked_row(table, rowid, txid, seen)? {
                crate::storage::LockedRowRecheck::Unchanged => LockedRow::Unchanged,
                crate::storage::LockedRowRecheck::OwnChange => LockedRow::Skip,
                crate::storage::LockedRowRecheck::Deleted => LockedRow::Deleted,
                crate::storage::LockedRowRecheck::Updated(home) => {
                    let version = home.snapshot().row_version();
                    let bytes = storage.row_bytes(table, rowid, home, state_arena)?;
                    let pending = self.pending.as_mut().expect("identified pending row");
                    pending.row = RetainedRow::Current {
                        rowid,
                        version,
                        bytes,
                    };
                    LockedRow::Updated
                }
            },
        )
    }

    /// Restores saved input `index` as the pending row. A wait may have
    /// left the same input pending already.
    pub(crate) fn prepare_saved_row(
        &mut self,
        index: usize,
        bytes: &[u8],
        state_arena: &'a Arena,
    ) -> Result<(), SqlError> {
        // Long walks over saved rows stay cancelable and time-limited.
        check_timeout()?;
        if let Some(pending) = &self.pending {
            if pending.saved == Some(index) {
                return Ok(());
            }
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor revisited an input while another row was pending"
            ));
        }
        let mark = state_arena.mark();
        let copied = state_arena
            .alloc_slice_copy(bytes)
            .map_err(|_| arena_full())?;
        if copied.len() < INPUT_HEADER {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "saved cursor input is torn"
            ));
        }
        let (header, values) = copied.split_at(INPUT_HEADER);
        let rowid = u64::from_le_bytes(header[..8].try_into().expect("eight-byte identity"));
        self.pending = Some(PendingRow {
            row: RetainedRow::Saved {
                rowid: (rowid != NO_ROWID).then_some(rowid),
                version: crate::storage::RowVersion::decode(&header[8..]),
                values,
            },
            mark,
            counted: true,
            saved: Some(index),
        });
        Ok(())
    }

    fn pending_input<R>(
        &self,
        each: impl FnOnce(
            Option<u64>,
            Option<crate::storage::RowVersion>,
            InputBytes<'_>,
        ) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let pending = self.pending.as_ref().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor has no pending input to save"
            )
        })?;
        let (rowid, version) = match &pending.row {
            RetainedRow::Literal(bytes) => return each(None, None, InputBytes::Encoded(bytes)),
            RetainedRow::Saved {
                rowid,
                version,
                values,
            } => return each(*rowid, *version, InputBytes::Encoded(values)),
            RetainedRow::Physical(row) => (row.rowid, row.version),
            RetainedRow::Current { rowid, version, .. } => (*rowid, *version),
            RetainedRow::Singleton => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "a FROM-less cursor saves output rows, not inputs"
                ));
            }
        };
        let mut values = [Datum::Null; MAX_COLUMNS];
        pending.decode(&self.scope, &mut values)?;
        let width = self
            .scope
            .defs
            .get(0)
            .expect("physical source definition")
            .n_columns;
        each(
            Some(rowid),
            Some(version),
            InputBytes::Values(&values[..width]),
        )
    }

    /// Bytes [`Self::encode_pending_input`] writes.
    pub(crate) fn pending_input_len(&self) -> Result<usize, SqlError> {
        self.pending_input(|_, _, bytes| {
            let length = match bytes {
                InputBytes::Encoded(bytes) => bytes.len(),
                InputBytes::Values(values) => crate::sql::exec::projected_row_len(values)?,
            };
            Ok(INPUT_HEADER + length)
        })
    }

    /// Writes the pending row as a saved input: its identity, then its
    /// source columns in projected encoding.
    pub(crate) fn encode_pending_input(&self, out: &mut [u8]) -> Result<(), SqlError> {
        self.pending_input(|rowid, version, bytes| {
            let (header, rest) = out.split_at_mut(INPUT_HEADER);
            header[..8].copy_from_slice(&rowid.unwrap_or(NO_ROWID).to_le_bytes());
            header[8..].copy_from_slice(
                &version.map_or([0; crate::storage::RowVersion::ENCODED_LEN], |version| {
                    version.encode()
                }),
            );
            match bytes {
                InputBytes::Encoded(bytes) => rest.copy_from_slice(bytes),
                InputBytes::Values(values) => {
                    crate::sql::exec::encode_projected_into(values, rest)?;
                }
            }
            Ok(())
        })
    }

    pub(crate) fn default_scrollable(&self) -> bool {
        // PostgreSQL never makes a row-locking cursor scrollable by default.
        self.locking.is_empty()
            && matches!(
                self.source,
                RetainedSource::Physical(_)
                    | RetainedSource::Generated { .. }
                    | RetainedSource::Literals {
                        scrollable: true,
                        ..
                    }
            )
            && !matches!(self.state, ReaderState::Eliminated)
    }

    pub(crate) fn uses_table(&self, table: usize) -> bool {
        !matches!(self.state, ReaderState::Eliminated)
            && self.scope.slots[..self.scope.n].contains(&table)
    }

    /// The physical table a top-level `SELECT` reads when a cursor-mode open
    /// would retain it, decided before execution and without capturing rows.
    /// Literal and `FROM`-less sources are excluded: their output is bounded
    /// by the statement itself. Scope resolution borrows `arena`; the caller
    /// rewinds it.
    pub(crate) fn stream_source_table(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        arena: &'a Arena,
    ) -> Result<Option<usize>, SqlError> {
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
            || !matches!(
                storage.resolve_relation(from.base.schema, from.base.table, txid),
                Some(crate::storage::ResolvedRelation::Table(_))
            )
        {
            return Ok(None);
        }
        let catalog = storage_catalog(storage, arena, txid);
        let expression_mode = Some((storage, txid));
        if !statement.items.iter().all(|item| match item {
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => true,
            SelectItem::Expr { expression, .. } => {
                retained_expression(expression, &catalog, expression_mode)
            }
            SelectItem::RecordStar(_) => false,
        }) || statement
            .where_clause
            .is_some_and(|predicate| !retained_expression(predicate, &catalog, expression_mode))
            || [statement.limit, statement.offset]
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
            outer: None,
            storage,
            txid,
            arena,
        };
        for expression in statement
            .items
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr { expression, .. } => Some(*expression),
                _ => None,
            })
            .chain(statement.where_clause)
        {
            if !retained_scalar_calls(expression, &columns)? {
                return Ok(None);
            }
        }
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
        let table = scope.slots[0];
        let definition = scope.defs.stored(0).expect("resolved physical source");
        if definition.kind != crate::storage::TableKind::Local
            || definition.partition.is_partitioned()
            || (from.base.inheritance == crate::sql::ast::RelationInheritance::Descendants
                && storage.relation_has_descendants(table, txid))
        {
            return Ok(None);
        }
        let Some(role) = storage.current_role_slot(txid) else {
            return Ok(None);
        };
        let role = scope.authorization_roles[0].map_or(role, usize::from);
        if storage.row_security_applies(table, role, txid) {
            return Ok(None);
        }
        Ok(Some(table))
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
        // SAFETY: only one physical local table without policies, inheritance
        // or joins, a literal VALUES list, or a builtin function source reaches
        // this point. Scope arrays, names, ASTs, literal rows and generator
        // state borrow the supplied arena/statement; definition images and row
        // roots have owned leases. No retained field borrows Storage. Its
        // conservative schema-resolution lifetime can therefore be extended to
        // the arena.
        let mut reader = reader
            .map(|reader| unsafe { core::mem::transmute::<RetainedSelect<'_>, Self>(reader) });
        if let Some(reader) = reader.as_mut() {
            reader.start_generator(storage, txid, arena, params)?;
        }
        Ok(reader)
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
        let from = statement.from.as_ref();
        let literal_count = from
            .and_then(|from| from.base.subquery)
            .and_then(literal_source);
        let literals = literal_count.is_some();
        if from.is_none() && mode == RetainedMode::Statement {
            return Ok(None);
        }
        let generated = from
            .filter(|from| from.joins.is_empty() && from.base.is_function_source())
            .and_then(|from| retained_function_source(&from.base, storage, txid, arena));
        if from.is_some_and(|from| {
            !from.joins.is_empty()
                || from.base.is_function_source() && generated.is_none()
                || from.base.subquery.is_some() && !literals
                || from.base.cte.is_some()
                || from.base.sample.is_some()
                || matches!(
                    storage.resolve_relation(from.base.schema, from.base.table, txid),
                    Some(crate::storage::ResolvedRelation::View(_))
                )
        }) || !statement.with.is_empty()
            || statement.set_body.is_some()
            || statement.distinct
            || !statement.distinct_on.is_empty()
            || !statement.group_by.is_empty()
            || !statement.grouping_sets.is_empty()
            || statement.having.is_some()
            || statement.with_ties
        {
            return Ok(None);
        }
        // A cursor locks rows of one table as it fetches them, after sorting
        // them when ordered; other ordered or locking shapes run at DECLARE.
        let locks = !statement.locking.is_empty();
        if (locks || !statement.order_by.is_empty())
            && (mode == RetainedMode::Statement
                || !locks
                || literals
                || generated.is_some()
                || from.is_none())
        {
            return Ok(None);
        }
        let catalog = storage_catalog(storage, arena, txid);
        let expression_mode = Some((storage, txid));
        if !statement.items.iter().all(|item| match item {
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => true,
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
        let scope = match from {
            Some(from) => {
                let scope = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
                if let Some(count) = literal_count {
                    let rows = arena
                        .alloc_slice_with(count, |_| &[][..])
                        .map_err(|_| arena_full())?;
                    let mut next = 0;
                    select_into_rows(
                        storage,
                        txid,
                        from.base.subquery.expect("literal source"),
                        arena,
                        params,
                        None,
                        None,
                        &mut |values| {
                            let row = rows.get_mut(next).ok_or_else(|| {
                                sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "literal source row count changed"
                                )
                            })?;
                            *row = crate::sql::exec::encode_projected_pub(values, arena)?;
                            next += 1;
                            Ok(())
                        },
                    )?;
                    if next != count {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "literal source row count changed"
                        ));
                    }
                    scope.derived[0] = Some(rows);
                }
                if scope.n != 1
                    || !literals
                        && generated.is_none()
                        && (scope.derived[0].is_some() || scope.slots[0] == usize::MAX)
                {
                    return Ok(None);
                }
                scope
            }
            None => QueryScope::singleton(storage, txid, arena)?,
        };
        let columns = CatalogScopeCols {
            scope: &scope,
            outer: None,
            storage,
            txid,
            arena,
        };
        for expression in statement
            .items
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr { expression, .. } => Some(*expression),
                _ => None,
            })
            .chain(statement.where_clause)
        {
            if !retained_scalar_calls(expression, &columns)? {
                return Ok(None);
            }
        }
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
        let binding_columns = Some(&columns);
        let table = if let Some(from) = from.filter(|_| !literals && generated.is_none()) {
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
            Some(table)
        } else {
            None
        };
        let order = match order_keys(statement, &scope, storage, arena)? {
            Some(order) => order,
            None => return Ok(None),
        };
        check_select_types(statement, &scope, storage, txid, arena, None)?;
        check_select_constants(statement, arena, storage, txid)?;
        super::check_key_types(statement, &scope, arena, storage, txid)?;
        let locking = if locks {
            if table.is_none() {
                return Ok(None);
            }
            super::validate_locking(statement, storage, txid, arena)?;
            arena
                .alloc_slice_copy(statement.locking)
                .map_err(|_| arena_full())?
        } else {
            &[][..]
        };
        let demand = from.filter(|_| table.is_some()).map(|from| {
            streaming_pax_columns(&scope, from, statement.items, statement.where_clause)
        });
        if let Some(demand) = demand {
            scan::admit_sources(storage, &scope, txid, demand)?;
        }
        let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
        let width = describe_scope_items(
            statement.items,
            &scope,
            None,
            storage,
            txid,
            arena,
            &mut columns,
            crate::sql::exec::DescriptionTarget::Sql,
        )?;
        let subqueries = SubqueryValues {
            scalars: &[],
            lists: &[],
            deferred: &[],
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
        // Sorted inputs are saved whole; their keys need not be projected.
        let decoded_columns = demand
            .filter(|_| order.is_empty())
            .and_then(|demand| demand.selected_mask(0))
            .map(|mask| {
                core::array::from_fn(|column| {
                    column < u64::BITS as usize && mask & (1 << column) != 0
                })
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
                let value = super::super::eval::boolean_argument(value, "WHERE")
                    .map_err(|error| crate::sql::error_position::at_expression(error, predicate))?;
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
                SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => false,
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
        if let Some(table) = table
            && (mode == RetainedMode::Statement || locks)
        {
            storage.lock_table(
                txid,
                table,
                if locks {
                    crate::sql::ast::TableLockMode::RowShare
                } else {
                    crate::sql::ast::TableLockMode::AccessShare
                },
                false,
            )?;
        }
        let source = if let Some(table) = table {
            let mut source = RowCursor::closed();
            if !eliminated {
                scan::validate_materialized_view_reads(storage, &scope, None, arena, txid)?;
                source.open(storage, table, txid, arena)?;
            }
            RetainedSource::Physical(source)
        } else if let (Some(kind), Some(from)) = (generated, from) {
            RetainedSource::Generated {
                function: &from.base,
                kind,
                generator: eliminated.then_some(super::generator::RowGenerator::Elements(None)),
            }
        } else if literals {
            let rows = scope.derived[0].expect("literal derived source owns encoded rows");
            let scrollable = from
                .and_then(|from| from.base.subquery)
                .and_then(|query| query.set_body)
                .is_some_and(literal_source_scrollable);
            RetainedSource::Literals {
                rows,
                next: if eliminated { rows.len() } else { 0 },
                scrollable,
            }
        } else {
            RetainedSource::Singleton {
                available: !eliminated,
            }
        };
        let revisit = cursor_revisit(
            &source,
            eliminated,
            statement,
            &CatalogScopeCols {
                scope: &scope,
                outer: None,
                storage,
                txid,
                arena,
            },
            storage,
            txid,
        )?;
        crate::sql::exec::DescriptionTarget::Wire.finish(
            &mut columns[..width],
            Some(storage),
            txid,
        )?;
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
            revisit,
            locking,
            order,
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
                if matches!(self.source, RetainedSource::Physical(_)) {
                    storage.record_relation_scan(
                        txid,
                        self.scope.slots[0],
                        None,
                        0,
                        observed_at,
                    )?;
                }
                self.state = ReaderState::Reading;
            }
            ReaderState::Reading => {}
        }
        check_timeout()?;
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.saved.is_some())
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor read its source while a saved input was pending"
            ));
        }
        if self.pending.is_none() {
            let mark = state_arena.mark();
            let next = match &mut self.source {
                RetainedSource::Physical(source) => source
                    .next(
                        buffers,
                        state_arena,
                        state_arena,
                        self.decoded_columns.as_ref(),
                    )
                    .map(|row| row.map(RetainedRow::Physical)),
                RetainedSource::Singleton { available } => {
                    Ok(core::mem::replace(available, false).then_some(RetainedRow::Singleton))
                }
                RetainedSource::Literals { rows, next, .. } => {
                    let row = rows.get(*next).copied();
                    if row.is_some() {
                        *next += 1;
                    }
                    Ok(row.map(RetainedRow::Literal))
                }
                RetainedSource::Generated { generator, .. } => generator
                    .as_mut()
                    .expect("generator starts with the cursor's first fetch")
                    .next(state_arena)
                    .and_then(|value| {
                        value
                            .map(|value| {
                                crate::sql::exec::encode_projected_pub(&[value], state_arena)
                                    .map(RetainedRow::Literal)
                            })
                            .transpose()
                    }),
            };
            let row = match next {
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
                saved: None,
            });
        }
        let pending = self.pending.as_mut().expect("captured statement row");
        if !pending.counted {
            if matches!(pending.row, RetainedRow::Physical(_)) {
                storage.record_relation_tuple_read(txid, self.scope.slots[0], None)?;
            }
            pending.counted = true;
        }
        Ok(true)
    }

    fn with_statement_row<'work, R>(
        &'work self,
        each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let pending = self.pending.as_ref().expect("prepared statement row");
        let mut values = [Datum::Null; MAX_COLUMNS];
        pending.decode(&self.scope, &mut values)?;
        let rowids = [match &pending.row {
            RetainedRow::Physical(row) => Some(row.rowid),
            RetainedRow::Saved { rowid, .. } => *rowid,
            RetainedRow::Current { rowid, .. } => Some(*rowid),
            RetainedRow::Singleton => None,
            RetainedRow::Literal(_) => None,
        }];
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
    pub(crate) fn evaluate_statement_predicate<'work>(
        &'work self,
        storage: &'work Storage,
        txid: u32,
        work: &'work Arena,
        params: &[Datum<'work>],
        sequence: Option<&dyn SequenceAccess>,
        invocations: Option<&RoutineInvocationState>,
        frame_arena: &Arena,
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
        invocations: Option<&RoutineInvocationState>,
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
        reason = "cursor bounds and physical row preparation share one explicit retained context"
    )]
    pub(crate) fn prepare_cursor_row<'work>(
        &mut self,
        storage: &'work Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &'work Arena,
        params: &[Datum<'a>],
    ) -> Result<bool, SqlError>
    where
        'a: 'work,
    {
        self.start_generator(storage, txid, state_arena, params)?;
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
        self.prepare_statement_row(storage, txid, observed_at, state_arena, buffers)
    }
}
