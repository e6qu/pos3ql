//! Retained input positions for cursors and suspended statement rows.

use super::*;
use crate::mem::arena::ArenaMark;

mod grouping;
mod join;
mod sort;
mod tree;
pub(crate) use join::MAX_RETAINED_SOURCES;
use tree::{PlanNode, PlanTree, Pull, ReaderState, ScanNode};

/// What a cursor-mode reader serves: a result stream returns its rows once,
/// in order, so it may sort them itself; a declared cursor keeps its own
/// ordered path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetainedUse {
    Stream,
    Cursor,
}
use crate::storage::{
    CursorRow, MAX_COLUMNS, RowCursor, RowCursorBuffers, SpilledRowRepresentation, rowenc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainedMode {
    Cursor,
    Statement,
    /// One query of a `UNION ALL` a stream appends: read in place, with
    /// its bounds evaluated at open and no description of its own.
    Branch,
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
    /// A LEFT JOIN's NULL-extended row at an inner level.
    Null,
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

/// What a result stream reads, as decided before its statement runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamSource {
    /// One ordinary table: its retained read position grows with its rows.
    Table(usize),
    /// A builtin set-returning function: only its next value is retained.
    Generated,
    /// A nested-loop join: the read positions of its physical tables; literal
    /// and generated items retain their rows or next value.
    Join([Option<usize>; MAX_RETAINED_SOURCES]),
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
        kinds: &'a [super::generator::GeneratorKind],
        generator: Option<super::generator::FunctionRows<'a>>,
    },
    /// The queries of a `UNION ALL`, read one after another as PostgreSQL's
    /// Append node does; each row is coerced to the combined column types.
    Append {
        branches: crate::mem::arena::OwnedArenaSlice<'a, RetainedSelect<'a>>,
        next: usize,
        targets: &'a [ColType],
    },
}

impl<'a> RetainedSource<'a> {
    /// Returns to the first row under the same snapshot; nothing is
    /// allocated.
    fn rewind(&mut self, state_arena: &Arena) -> Result<(), SqlError> {
        match self {
            Self::Physical(cursor) => cursor.rewind(state_arena)?,
            Self::Literals { next, .. } => *next = 0,
            Self::Singleton { available } => *available = true,
            Self::Generated { generator, .. } => {
                if let Some(generator) = generator {
                    generator.rewind();
                }
            }
            Self::Append { .. } => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "an appended result cannot be rewound"
                ));
            }
        }
        Ok(())
    }

    fn close(&mut self) {
        match self {
            Self::Physical(cursor) => cursor.close(),
            Self::Literals { rows, next, .. } => *next = rows.len(),
            Self::Singleton { available } => *available = false,
            Self::Generated { generator, .. } => {
                *generator = Some(super::generator::FunctionRows::finished());
            }
            Self::Append { branches, next, .. } => *next = branches.len(),
        }
    }
}

impl PendingRow<'_> {
    fn decode<'row>(
        &'row self,
        scope: &QueryScope<'_>,
        values: &mut [Datum<'row>; MAX_COLUMNS],
    ) -> Result<(), SqlError> {
        self.decode_table(scope, 0, values)
    }

    /// Decodes this row as scope source `table` into `values`.
    fn decode_table<'row>(
        &'row self,
        scope: &QueryScope<'_>,
        table: usize,
        values: &mut [Datum<'row>],
    ) -> Result<(), SqlError> {
        if let RetainedRow::Literal(bytes) | RetainedRow::Saved { values: bytes, .. } = self.row {
            let definition = scope.defs.get(table).expect("literal source definition");
            for (column, value) in values[..definition.n_columns].iter_mut().enumerate() {
                *value = crate::sql::exec::decode_projected_pub(bytes, column);
            }
            return Ok(());
        }
        if let RetainedRow::Current { bytes, .. } = self.row {
            let definition = scope
                .defs
                .stored(table)
                .expect("physical source definition");
            let mut schema = [ColType::Bool; MAX_COLUMNS];
            definition.schema(&mut schema);
            return rowenc::decode(bytes, &schema[..definition.n_columns], values);
        }
        let RetainedRow::Physical(row) = &self.row else {
            return Ok(());
        };
        let definition = scope
            .defs
            .stored(table)
            .expect("physical source definition");
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

#[derive(Clone, Copy)]
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
    scope: QueryScope<'a>,
    /// The nodes that produce the rows: see [`tree`].
    plan: PlanTree<'a>,
    revisit: crate::sql::cursor::Revisit,
    /// Row locks each fetched row takes, as PostgreSQL's LockRows node does.
    locking: &'a [crate::sql::ast::LockClause<'a>],
    /// A locking cursor's ORDER BY: every qualifying row is saved and sorted
    /// before the first row is locked.
    order: &'a [RetainedOrderKey],
    /// A scrollable cursor returns saved output rows on revisit instead of
    /// reading its input again: PostgreSQL materializes a join under a
    /// scrollable cursor, so a volatile output list is not evaluated again.
    saves_scroll_output: bool,
    /// A statement's RowDescription, sent once its first row is prepared:
    /// a sort or hash table that does not fit leaves nothing queued, and the
    /// statement runs in the general executor instead.
    description: Option<&'a [ColDesc<'a>]>,
}

fn description_too_large(mode: RetainedMode) -> SqlError {
    match mode {
        RetainedMode::Cursor => super::super::cursor_description_too_large(),
        RetainedMode::Statement | RetainedMode::Branch => sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "retained statement row description exceeds output capacity"
        ),
    }
}

/// Sends a cursor's description now; a statement keeps it to send once its
/// first row is prepared.
fn describe<'a>(
    columns: &[ColDesc<'a>],
    mode: RetainedMode,
    arena: &'a Arena,
    responder: &mut Responder<'_>,
) -> Result<Option<&'a [ColDesc<'a>]>, SqlError> {
    match mode {
        RetainedMode::Cursor => {
            responder
                .row_description(columns)
                .map_err(|_| description_too_large(mode))?;
            Ok(None)
        }
        RetainedMode::Statement => arena
            .alloc_slice_copy(columns)
            .map(|columns| Some(&*columns))
            .map_err(|_| arena_full()),
        // The appending reader describes the combined result.
        RetainedMode::Branch => Ok(None),
    }
}

fn retained_expression(
    expression: &Expr<'_>,
    catalog: &dyn super::super::eval::CatalogAccess,
    statement: Option<(&Storage, u32)>,
) -> bool {
    retained_expression_with(expression, catalog, statement, Leaves::Scalar)
}

/// Which calls [`retained_expression_with`] accepts besides scalar ones.
#[derive(Clone, Copy)]
enum Leaves<'s> {
    Scalar,
    /// Any call without `OVER`, before scope resolution tells aggregates
    /// apart; a grouped reader checks them once it does.
    Calls,
    /// These aggregate calls, whose inputs a grouped reader checks itself.
    Aggregates(&'s [*const Expr<'s>]),
}

fn same_node(pointer: *const Expr<'_>, expression: &Expr<'_>) -> bool {
    core::ptr::eq(
        pointer.cast::<u8>(),
        (expression as *const Expr<'_>).cast::<u8>(),
    )
}

fn retained_expression_with(
    expression: &Expr<'_>,
    catalog: &dyn super::super::eval::CatalogAccess,
    statement: Option<(&Storage, u32)>,
    leaves: Leaves<'_>,
) -> bool {
    use crate::sql::ast::UnaryOp;
    if let Leaves::Aggregates(pointers) = leaves
        && pointers
            .iter()
            .any(|pointer| same_node(*pointer, expression))
    {
        return true;
    }
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
        | Expr::IsNull { operand, .. } => {
            retained_expression_with(operand, catalog, statement, leaves)
        }
        Expr::Binary {
            operator,
            left,
            right,
            ..
        } => {
            operator
                .operator_name()
                .is_none_or(|name| !catalog.has_operator_candidate(name))
                && retained_expression_with(left, catalog, statement, leaves)
                && retained_expression_with(right, catalog, statement, leaves)
        }
        Expr::Case {
            operand: None,
            whens,
            otherwise,
            ..
        } => {
            whens.iter().all(|(condition, value)| {
                retained_expression_with(condition, catalog, statement, leaves)
                    && retained_expression_with(value, catalog, statement, leaves)
            }) && otherwise
                .is_none_or(|value| retained_expression_with(value, catalog, statement, leaves))
        }
        Expr::Array(items) => items
            .iter()
            .all(|item| retained_expression_with(item, catalog, statement, leaves)),
        Expr::Subscript { base, index } => {
            retained_expression_with(base, catalog, statement, leaves)
                && retained_expression_with(index, catalog, statement, leaves)
        }
        Expr::Slice { base, lower, upper } => {
            retained_expression_with(base, catalog, statement, leaves)
                && lower
                    .is_none_or(|bound| retained_expression_with(bound, catalog, statement, leaves))
                && upper
                    .is_none_or(|bound| retained_expression_with(bound, catalog, statement, leaves))
        }
        Expr::Cast {
            operand, type_name, ..
        } if statement.is_some() => {
            ColType::from_sql_name(type_name).is_some()
                && retained_expression_with(operand, catalog, statement, leaves)
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
            .all(|argument| retained_expression_with(argument, catalog, statement, leaves)),
        Expr::Call {
            args,
            order_by,
            over: None,
            filter,
            ..
        } if statement.is_some() && matches!(leaves, Leaves::Calls) => args
            .iter()
            .copied()
            .chain(order_by.iter().map(|order| order.expression))
            .chain(*filter)
            .all(|argument| retained_expression_with(argument, catalog, statement, leaves)),
        _ => false,
    }
}

fn retained_scalar_calls(
    expression: &Expr<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
) -> Result<bool, SqlError> {
    retained_scalar_calls_except(expression, columns, &[])
}

/// [`retained_scalar_calls`], skipping the aggregate calls `aggregates`
/// whose inputs a grouped reader checks itself.
fn retained_scalar_calls_except(
    expression: &Expr<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    aggregates: &[*const Expr<'_>],
) -> Result<bool, SqlError> {
    if aggregates
        .iter()
        .any(|pointer| same_node(*pointer, expression))
    {
        return Ok(true);
    }
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
        scalar &= retained_scalar_calls_except(child, columns, aggregates)?;
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

/// A table function a cursor can advance from fixed generator state: one
/// non-volatile builtin with a generator implementation and constant
/// arguments, or `ROWS FROM` of such calls, alone in FROM. Returns each
/// call's generator. Other function sources run when declared.
fn retained_function_source<'a>(
    tref: &'a TableRef<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Option<&'a [super::generator::GeneratorKind]> {
    if tref.lateral || super::srf::sql_table_constructor(tref) {
        return None;
    }
    let calls = match tref.rows_from {
        Some(calls) => calls,
        None => core::slice::from_ref(tref),
    };
    let kinds = arena
        .alloc_slice_with(calls.len(), |_| super::generator::GeneratorKind::Numeric)
        .ok()?;
    for (kind, call) in kinds.iter_mut().zip(calls) {
        let args = call.func_args?;
        if call.rows_from.is_some()
            || !args
                .iter()
                .all(|argument| constant_function_argument(argument))
        {
            return None;
        }
        let (selection, _) = super::srf::select_table_func_routine(
            call,
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
        *kind = super::generator::GeneratorKind::for_builtin(oid, args.len())?;
    }
    Some(kinds)
}

/// Clauses a retained reader of a join cannot hold: the general executor
/// groups, orders, deduplicates or locks them.
fn join_clauses_retained(statement: &Select<'_>, self_sorts: bool, groups_itself: bool) -> bool {
    (statement.order_by.is_empty() || self_sorts)
        && statement.with.is_empty()
        && statement.set_body.is_none()
        && (groups_itself || !statement.distinct)
        && (statement.distinct_on.is_empty() || self_sorts)
        && (groups_itself || statement.group_by.is_empty() && statement.having.is_none())
        && statement.grouping_sets.is_empty()
        && statement.locking.is_empty()
        && !statement.with_ties
}

/// The output, WHERE and bound expressions a retained reader evaluates
/// itself, checked before its scope is resolved.
fn expressions_retained(
    statement: &Select<'_>,
    catalog: &dyn super::super::eval::CatalogAccess,
    storage: &Storage,
    txid: u32,
    leaves: Leaves<'_>,
) -> bool {
    let expression_mode = Some((storage, txid));
    statement.items.iter().all(|item| match item {
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
        SelectItem::Expr { expression, .. } => {
            retained_expression_with(expression, catalog, expression_mode, leaves)
        }
        SelectItem::RecordStar(_) => false,
    }) && statement
        .where_clause
        .is_none_or(|predicate| retained_expression(predicate, catalog, expression_mode))
        && [statement.limit, statement.offset]
            .into_iter()
            .flatten()
            .all(|bound| retained_bound(bound, catalog))
}

/// The scope-dependent half of [`expressions_retained`]: scalar calls only,
/// and integer LIMIT and OFFSET.
fn scope_expressions_retained(
    statement: &Select<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    catalog: &dyn super::super::eval::CatalogAccess,
    grouped: bool,
) -> Result<bool, SqlError> {
    // A grouping's admission checked its output, HAVING and WHERE.
    for expression in statement
        .items
        .iter()
        .filter_map(|item| match item {
            SelectItem::Expr { expression, .. } => Some(*expression),
            _ => None,
        })
        .chain(statement.where_clause)
        .filter(|_| !grouped)
    {
        if !retained_scalar_calls(expression, columns)? {
            return Ok(false);
        }
    }
    for bound in [statement.limit, statement.offset].into_iter().flatten() {
        let (oid, _) = super::super::exec::infer_type_res(bound, columns)?;
        if oid != super::super::types::oid::UNKNOWN
            && !matches!(
                catalog
                    .domain_base_type(oid)
                    .or_else(|| ColType::from_oid(oid)),
                Some(ColType::Int2 | ColType::Int4 | ColType::Int8)
            )
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The grouping a reader computes itself, once its scope is resolved.
/// `None` leaves the statement to the general executor. Otherwise: the
/// statement, with its `GROUP BY` positions resolved when grouped, and its
/// aggregate calls when grouped.
#[allow(clippy::type_complexity)]
fn reader_grouping<'a>(
    statement: &'a Select<'a>,
    columns: &CatalogScopeCols<'_, 'a, '_, '_>,
    catalog: &dyn super::super::eval::CatalogAccess,
    groups_itself: bool,
    arena: &'a Arena,
) -> Result<Option<(&'a Select<'a>, Option<grouping::AggregateNodes<'a>>)>, SqlError> {
    if !groups_itself {
        return Ok(Some((statement, None)));
    }
    Ok(
        match grouping::admit_grouping(statement, columns, catalog, arena)? {
            None => None,
            Some(grouping::Grouping::Grouped {
                statement,
                aggregates,
            }) => Some((statement, Some(aggregates))),
            Some(grouping::Grouping::Plain) => {
                // Calls passed before scope resolution only as possible
                // aggregates.
                let mode = Some((columns.storage, columns.txid));
                statement
                    .items
                    .iter()
                    .all(|item| match item {
                        SelectItem::Expr { expression, .. } => {
                            retained_expression(expression, catalog, mode)
                        }
                        _ => true,
                    })
                    .then_some((statement, None))
            }
        },
    )
}

/// Resolves a joined FROM clause for a retained reader: its scope with
/// literal items encoded, and its admitted join shape.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn retained_join_scope<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    params: Option<&[Datum<'a>]>,
    self_sorts: bool,
    groups_itself: bool,
    arena: &'a Arena,
) -> Result<
    Option<(
        QueryScope<'a>,
        join::JoinShape<'a>,
        &'a Select<'a>,
        Option<grouping::AggregateNodes<'a>>,
    )>,
    SqlError,
> {
    let catalog = storage_catalog(storage, arena, txid);
    let leaves = if groups_itself {
        Leaves::Calls
    } else {
        Leaves::Scalar
    };
    if from.joins.len() >= MAX_RETAINED_SOURCES
        || !join_clauses_retained(statement, self_sorts, groups_itself)
        || !expressions_retained(statement, &catalog, storage, txid, leaves)
    {
        return Ok(None);
    }
    for table in 0..=from.joins.len() {
        let tref = if table == 0 {
            &from.base
        } else {
            &from.joins[table - 1].table
        };
        if join::item_shape(tref, storage, txid, arena).is_none() {
            return Ok(None);
        }
    }
    let scope = QueryScope::resolve_schema_with_join_conditions(storage, from, txid, arena)?;
    let columns = CatalogScopeCols {
        scope: &scope,
        outer_scope: None,
        storage,
        txid,
        arena,
    };
    let Some((statement, aggregates)) =
        reader_grouping(statement, &columns, &catalog, groups_itself, arena)?
    else {
        return Ok(None);
    };
    if !scope_expressions_retained(statement, &columns, &catalog, aggregates.is_some())?
        || aggregates.is_none()
            && sort::sorts(statement)
            && !sort::sort_admitted(statement, &columns, &catalog)?
    {
        return Ok(None);
    }
    let Some(shape) = join::join_shape(storage, txid, from, statement.where_clause, &scope, arena)?
    else {
        return Ok(None);
    };
    if let Some(params) = params {
        for (table, item) in shape.items.iter().enumerate().take(scope.n) {
            if let Some(join::ItemShape::Literals(count)) = item {
                let query = if table == 0 {
                    from.base.subquery
                } else {
                    from.joins[table - 1].table.subquery
                };
                scope.derived[table] = Some(literal_rows(
                    storage,
                    txid,
                    query.expect("literal join item"),
                    *count,
                    arena,
                    params,
                )?);
            }
        }
    }
    Ok(Some((scope, shape, statement, aggregates)))
}

/// Encodes a literal VALUES source's rows in the reader's memory.
fn literal_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    query: &'a Select<'a>,
    count: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a [&'a [u8]], SqlError> {
    let rows = arena
        .alloc_slice_with(count, |_| &[][..])
        .map_err(|_| arena_full())?;
    let mut next = 0;
    select_into_rows(
        storage,
        txid,
        query,
        arena,
        params,
        None,
        None,
        &mut |values| {
            let row = rows.get_mut(next).ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "literal source row count changed")
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
    Ok(rows)
}

/// PostgreSQL evaluates a function scan's arguments when execution starts.
/// They are constants, kept in the reader's own memory.
fn start_generator_source<'a>(
    source: &mut RetainedSource<'a>,
    storage: &Storage,
    txid: u32,
    state_arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(), SqlError> {
    let RetainedSource::Generated {
        function,
        kinds,
        generator: generator @ None,
    } = source
    else {
        return Ok(());
    };

    let catalog = storage_catalog(storage, state_arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..super::super::eval::NO_HOOKS
    };
    let calls_source = match function.rows_from {
        Some(calls) => calls,
        None => core::slice::from_ref(*function),
    };
    let calls = state_arena
        .alloc_slice_with(calls_source.len(), |_| {
            super::generator::RowGenerator::Elements(None)
        })
        .map_err(|_| arena_full())?;
    for ((call, source), kind) in calls.iter_mut().zip(calls_source).zip(kinds.iter()) {
        let args = source.func_args.expect("retained function arguments");
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
        *call = super::generator::RowGenerator::start(*kind, &values[..args.len()], state_arena)?;
    }
    *generator = Some(super::generator::FunctionRows::new(
        calls,
        state_arena,
        function.with_ordinality,
    )?);
    Ok(())
}

/// Reads a source's next row into `state_arena`.
fn next_source_row<'a>(
    source: &mut RetainedSource<'a>,
    state_arena: &'a Arena,
    buffers: &mut RowCursorBuffers,
    decoded_columns: Option<&[bool; MAX_COLUMNS]>,
) -> Result<Option<RetainedRow<'a>>, SqlError> {
    match source {
        RetainedSource::Physical(source) => source
            .next(buffers, state_arena, state_arena, decoded_columns)
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
            .next_encoded(state_arena)
            .map(|row| row.map(RetainedRow::Literal)),
        RetainedSource::Append { .. } => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "appended rows are read through their plan"
        )),
    }
}

/// Collects the queries of a set tree that is only explicit `UNION ALL`.
fn union_all_leaves<'a>(
    tree: &'a crate::sql::ast::SetTree<'a>,
    leaves: &mut crate::mem::arena::ArenaList<'a, &'a Select<'a>>,
) -> Result<bool, SqlError> {
    use crate::sql::ast::{SetOp, SetOperationSyntax, SetTree};
    match tree {
        SetTree::Select(select) => {
            leaves.push(*select).map_err(|_| arena_full())?;
            Ok(true)
        }
        SetTree::Op {
            syntax: SetOperationSyntax::Explicit,
            operator: SetOp::Union,
            all: true,
            left,
            right,
        } => Ok(union_all_leaves(left, leaves)? && union_all_leaves(right, leaves)?),
        SetTree::Op { .. } => Ok(false),
    }
}

/// The queries a stream appends for `statement`, a `SELECT * FROM (...)`
/// over an explicit `UNION ALL`; `None` for any other statement.
fn appended_leaves<'a>(
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a [&'a Select<'a>]>, SqlError> {
    let Some(from) = statement.from.as_ref() else {
        return Ok(None);
    };
    let Some(body) = from
        .base
        .subquery
        .filter(|_| from.joins.is_empty() && !from.base.lateral)
        .filter(|query| {
            query.with.is_empty()
                && query.order_by.is_empty()
                && query.limit.is_none()
                && query.offset.is_none()
                && query.locking.is_empty()
        })
        .and_then(|query| query.set_body)
    else {
        return Ok(None);
    };
    let mut leaves = crate::mem::arena::ArenaList::new(arena);
    if !union_all_leaves(body, &mut leaves)? {
        return Ok(None);
    }
    leaves
        .into_mut_slice()
        .map(|leaves| Some(&*leaves))
        .map_err(|_| arena_full())
}

/// Whether one appended query runs in place: its expressions are
/// non-volatile and call no routine body, since they run below the
/// statement driver, where no call can hand off.
fn branch_admitted<'a>(
    storage: &'a Storage,
    txid: u32,
    leaf: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<bool, SqlError> {
    let scope = match leaf.from.as_ref() {
        Some(from) => QueryScope::resolve_schema(storage, from, txid, arena, None)?,
        None => QueryScope::singleton(storage, txid, arena)?,
    };
    let columns = CatalogScopeCols {
        scope: &scope,
        outer_scope: None,
        storage,
        txid,
        arena,
    };
    let catalog = storage_catalog(storage, arena, txid);
    let mut expressions = crate::mem::arena::ArenaList::new(arena);
    for item in leaf.items {
        match item {
            SelectItem::Expr { expression, .. } => {
                expressions.push(*expression).map_err(|_| arena_full())?;
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {}
            SelectItem::RecordStar(_) => return Ok(false),
        }
    }
    for expression in leaf
        .where_clause
        .into_iter()
        .chain(leaf.having)
        .chain(leaf.group_by.iter().copied())
        .chain(leaf.distinct_on.iter().copied())
        .chain(leaf.order_by.iter().map(|order| order.expression))
    {
        expressions.push(expression).map_err(|_| arena_full())?;
    }
    for expression in expressions.as_slice() {
        // Binding proves which casts and operators are builtins.
        let bound = bind_expression(expression, arena, Some(&columns), None)?;
        if super::super::eval::routine_input_handoff(bound, Some(&catalog))
            || super::predicate_safety::expression_properties(
                expression,
                &columns,
                storage,
                txid,
                super::predicate_safety::AnalysisStage::RuntimeProjection,
            )?
            .volatile
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `SELECT * FROM (query)` for a set query a stream may append, with the
/// query's LIMIT and OFFSET moved to the outer reader; `None` when `query`
/// is not an explicit `UNION ALL` without ORDER BY, WITH or row locks.
fn appended_select<'a>(
    query: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    let Some(body) = query.set_body else {
        return Ok(None);
    };
    if !query.with.is_empty()
        || !query.order_by.is_empty()
        || query.with_ties
        || !query.locking.is_empty()
    {
        return Ok(None);
    }
    let mut leaves = crate::mem::arena::ArenaList::new(arena);
    if !union_all_leaves(body, &mut leaves)? {
        return Ok(None);
    }
    let inner = arena
        .alloc(Select {
            limit: None,
            offset: None,
            set_body: Some(pull_up_appended(body, storage, txid, arena)?),
            ..*query
        })
        .map_err(|_| arena_full())?;
    let items = arena
        .alloc_slice_copy(&[SelectItem::Wildcard])
        .map_err(|_| arena_full())?;
    let wrapper = arena
        .alloc(Select {
            items,
            distinct: false,
            distinct_on: &[],
            from: Some(FromClause {
                base: TableRef {
                    schema: None,
                    table: "",
                    alias: None,
                    subquery: Some(inner),
                    func_args: None,
                    func_argument_names: &[],
                    func_variadic: false,
                    func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    rows_from: None,
                    col_alias: None,
                    inheritance: crate::sql::ast::RelationInheritance::Descendants,
                    sample: None,
                    cte: None,
                    with_ordinality: false,
                    lateral: false,
                    authorization_role: None,
                    bound_table: None,
                    view_access: None,
                },
                joins: &[],
            }),
            where_clause: None,
            group_by: &[],
            grouping_set_quantifier: crate::sql::ast::GroupingSetQuantifier::All,
            grouping_sets: &[],
            having: None,
            order_by: &[],
            limit: query.limit,
            offset: query.offset,
            with_ties: false,
            with: &[],
            set_body: None,
            locking: &[],
        })
        .map_err(|_| arena_full())?;
    Ok(Some(&*wrapper))
}

/// The query a stream reads for `select`, or `None` when it cannot stream:
/// its CTEs inlined and views expanded (a CTE that must run once keeps it
/// out), then the appending form of a `UNION ALL`, else the query with its
/// simple FROM subqueries pulled up, as the general executor runs it.
pub(crate) fn stream_query<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<&'a Select<'a>>, SqlError> {
    let Some(select) = super::cte::expand_stream_ctes(select, storage, txid, arena)? else {
        return Ok(None);
    };
    match appended_select(select, storage, txid, arena)? {
        Some(appended) => Ok(Some(appended)),
        None if select.from.is_some() => {
            super::pullup::pull_up_subqueries(select, storage, txid, arena, None).map(Some)
        }
        None => Ok(Some(select)),
    }
}

/// `tree` with each query of an explicit `UNION ALL` pulled up, as the
/// general executor pulls up each query it runs.
fn pull_up_appended<'a>(
    tree: &'a crate::sql::ast::SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a crate::sql::ast::SetTree<'a>, SqlError> {
    use crate::sql::ast::SetTree;
    let rebuilt = match tree {
        SetTree::Select(select) if select.from.is_some() => SetTree::Select(
            super::pullup::pull_up_subqueries(select, storage, txid, arena, None)?,
        ),
        SetTree::Select(_) => return Ok(tree),
        SetTree::Op {
            syntax,
            operator,
            all,
            left,
            right,
        } => SetTree::Op {
            syntax: *syntax,
            operator: *operator,
            all: *all,
            left: pull_up_appended(left, storage, txid, arena)?,
            right: pull_up_appended(right, storage, txid, arena)?,
        },
    };
    arena
        .alloc(rebuilt)
        .map(|tree| &*tree)
        .map_err(|_| arena_full())
}

/// The Select form of a top-level set query, as subqueries carry one.
pub(crate) fn set_query_select<'a>(
    query: &'a crate::sql::ast::SetQuery<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    arena
        .alloc(Select {
            items: &[],
            distinct: false,
            distinct_on: &[],
            from: None,
            where_clause: None,
            group_by: &[],
            grouping_set_quantifier: crate::sql::ast::GroupingSetQuantifier::All,
            grouping_sets: &[],
            having: None,
            order_by: query.order_by,
            limit: query.limit,
            offset: query.offset,
            with_ties: query.with_ties,
            with: query.with,
            set_body: Some(query.body),
            locking: query.locking,
        })
        .map(|select| &*select)
        .map_err(|_| arena_full())
}

/// Reads the next row of an appended `UNION ALL`: the current query's
/// filter, bounds and output run here, its values are coerced to the
/// combined column types and encoded into `work`. Each query keeps its own
/// row in `state_arena` until its output is encoded, so a wait leaves that
/// row pending and the same call resumes it.
fn next_appended_row<'a, 'w>(
    branches: &mut [RetainedSelect<'a>],
    next: &mut usize,
    targets: &[ColType],
    pull: &mut Pull<'_, 'a>,
    work: &'w Arena,
) -> Result<Option<&'w [u8]>, SqlError> {
    while let Some(branch) = branches.get_mut(*next) {
        let state_arena = pull.state_arena;
        if !branch.prepare_branch_row(pull)? {
            *next += 1;
            continue;
        }
        let passes = branch.evaluate_statement_predicate(
            pull.storage,
            pull.txid,
            work,
            pull.params,
            None,
            None,
            work,
        )?;
        if !passes || branch.skip_statement_row() {
            branch.finish_statement_row(state_arena, false);
            continue;
        }
        let mut values = crate::mem::arena::ArenaList::new(work);
        for index in 0..branch.statement_items_len() {
            let item = branch.evaluate_statement_item(
                index,
                pull.storage,
                pull.txid,
                work,
                pull.params,
                None,
                None,
                work,
            )?;
            for value in item {
                let target = targets.get(values.len()).ok_or_else(|| {
                    sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "each UNION query must have the same number of columns"
                    )
                })?;
                let value = super::setops::coerce_set_value(*value, *target, work)?;
                values.push(value).map_err(|_| arena_full())?;
            }
        }
        if values.len() != targets.len() {
            return Err(sql_err!(
                sqlstate::SYNTAX_ERROR,
                "each UNION query must have the same number of columns"
            ));
        }
        let values = values.as_slice();
        let length = crate::sql::exec::projected_row_len(values)?;
        let bytes = work
            .alloc_slice_with(length, |_| 0u8)
            .map_err(|_| arena_full())?;
        crate::sql::exec::encode_projected_into(values, bytes)?;
        branch.finish_statement_row(state_arena, true);
        return Ok(Some(&*bytes));
    }
    Ok(None)
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

/// Sorts in place without allocating; the first comparison error stops it.
fn heap_sort<T: Copy>(
    items: &mut [T],
    mut less: impl FnMut(&T, &T) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let mut sift = |items: &mut [T], mut root: usize, end: usize| -> Result<(), SqlError> {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return Ok(());
            }
            if child + 1 < end && less(&items[child], &items[child + 1])? {
                child += 1;
            }
            if !less(&items[root], &items[child])? {
                return Ok(());
            }
            items.swap(root, child);
            root = child;
        }
    };
    let len = items.len();
    for root in (0..len / 2).rev() {
        sift(items, root, len)?;
    }
    for end in (1..len).rev() {
        items.swap(0, end);
        sift(items, 0, end)?;
    }
    Ok(())
}

/// PostgreSQL scans a table, function result or VALUES list backward and
/// evaluates revisited rows again; other sources are materialized under a
/// scrollable cursor. Saved outputs are returned when evaluating again cannot
/// be observed.
fn cursor_revisit(
    source: &RetainedSource<'_>,
    eliminated: bool,
    sorted: bool,
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
    // A sort keeps the rows its filter passed; only a projection above it
    // runs again, as PostgreSQL evaluates a volatile output list after
    // sorting.
    if !sorted
        && let Some(predicate) = statement.where_clause
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
        self.plan.start(storage, txid, state_arena, params)
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
        let RowBounds::Unstarted { limit, offset } = self.plan.bounds() else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "cursor bounds were evaluated before its plan started"
            ));
        };
        let offset =
            super::super::exec::eval_offset_pub(offset, work, params, &crate::sql::eval::NO_HOOKS)?;
        let count =
            super::super::exec::eval_limit_pub(limit, work, params, &crate::sql::eval::NO_HOOKS)?;
        *self.plan.bounds_mut() = RowBounds::Active {
            skip: 0,
            remaining: u64::MAX,
        };
        Ok((offset != 0 || count != u64::MAX).then_some((offset, count)))
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.plan.has_current()
    }

    /// LIMIT has returned all its rows; PostgreSQL reads no further input.
    pub(crate) fn limit_reached(&self) -> bool {
        matches!(self.plan.bounds(), RowBounds::Active { remaining: 0, .. })
    }

    pub(crate) fn locks_rows(&self) -> bool {
        !self.locking.is_empty()
    }

    pub(crate) fn sorted(&self) -> bool {
        !self.order.is_empty()
    }

    /// Whether [`Self::rewind`] can return to the first row: an unsorted,
    /// non-locking source of one table, literal rows, generated rows or a
    /// single row. The same snapshot then yields the same rows in order.
    pub(crate) fn rewindable(&self) -> bool {
        self.order.is_empty()
            && self.locking.is_empty()
            && !self.saves_scroll_output
            && self.plan.group().is_none()
            && self.branches().is_empty()
    }

    /// Keeps the input readable after its last row, so a scrollable cursor
    /// can [`Self::rewind`] it.
    pub(crate) fn keep_finished_input(&mut self) {
        self.plan.keep_finished = true;
    }

    /// Returns to the first input row, as PostgreSQL's executor rewind does
    /// for a scrollable portal. LIMIT and OFFSET apply again. No row may be
    /// pending; nothing is allocated.
    pub(crate) fn rewind(&mut self, state_arena: &Arena) -> Result<(), SqlError> {
        if !self.rewindable() || self.has_pending() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained cursor input cannot be rewound here"
            ));
        }
        if matches!(self.plan.state, ReaderState::Eliminated) {
            return Ok(());
        }
        self.plan.rewind(state_arena)
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
            crate::sql::exec::decode_projected_pub(&bytes(offset, length)[INPUT_HEADER..], column)
        };
        heap_sort(entries, |left, right| {
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
                            &left,
                            &right,
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
                    return Ok(ordering.is_gt());
                }
            }
            // Equal keys keep scan order, as the executor's stable sort
            // does: an earlier input sits at a higher offset.
            Ok(left.0 < right.0)
        })
    }

    fn pending_identity(&self) -> Result<(u64, crate::storage::RowVersion), SqlError> {
        let pending = self.pending().ok_or_else(|| {
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
                    let pending = self
                        .plan
                        .scan_mut()
                        .and_then(|scan| scan.pending.as_mut())
                        .expect("identified pending row");
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
        if let Some(pending) = self.pending() {
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
        self.plan
            .scan_mut()
            .expect("saved inputs are scanned")
            .pending = Some(PendingRow {
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
        let pending = self.pending().ok_or_else(|| {
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
            RetainedRow::Singleton | RetainedRow::Null => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "a FROM-less or joined cursor saves output rows, not inputs"
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
        // PostgreSQL never makes a row-locking cursor scrollable by default;
        // a sort can scan backward.
        if self.plan.sort().is_some() {
            return self.locking.is_empty() && !matches!(self.plan.state, ReaderState::Eliminated);
        }
        self.locking.is_empty()
            && matches!(
                self.plan.scan().map(|scan| &scan.source),
                Some(
                    RetainedSource::Physical(_)
                        | RetainedSource::Generated { .. }
                        | RetainedSource::Literals {
                            scrollable: true,
                            ..
                        }
                )
            )
            && !matches!(self.plan.state, ReaderState::Eliminated)
    }

    /// The physical tables this reader scans.
    pub(crate) fn source_tables(&self) -> [Option<usize>; MAX_RETAINED_SOURCES] {
        let mut tables = [None; MAX_RETAINED_SOURCES];
        if matches!(self.plan.state, ReaderState::Eliminated) {
            return tables;
        }
        match self.plan.join() {
            Some(joins) => {
                for (target, (table, source)) in tables.iter_mut().zip(joins.tables()) {
                    *target = matches!(source, RetainedSource::Physical(_))
                        .then(|| self.scope.slots[table]);
                }
            }
            None if !self.branches().is_empty() => {
                let mut held = self
                    .branches()
                    .iter()
                    .flat_map(|branch| branch.source_tables())
                    .flatten();
                for target in &mut tables {
                    *target = held.next();
                }
                debug_assert!(held.next().is_none(), "admission bounds appended tables");
            }
            None => {
                tables[0] = matches!(
                    self.plan.scan().map(|scan| &scan.source),
                    Some(RetainedSource::Physical(_))
                )
                .then(|| self.scope.slots[0]);
            }
        }
        tables
    }

    pub(crate) fn uses_table(&self, table: usize) -> bool {
        !matches!(self.plan.state, ReaderState::Eliminated)
            && (self.scope.slots[..self.scope.n].contains(&table)
                || self
                    .branches()
                    .iter()
                    .any(|branch| branch.uses_table(table)))
    }

    /// The queries an appending reader reads, in order.
    fn branches(&self) -> &[RetainedSelect<'a>] {
        match self.plan.scan().map(|scan| &scan.source) {
            Some(RetainedSource::Append { branches, .. }) => branches,
            _ => &[],
        }
    }

    /// The source a top-level `SELECT` reads when a cursor-mode open would
    /// retain it, decided before execution and without capturing rows: one
    /// physical table, or a builtin set-returning function advanced from
    /// fixed generator state. Literal and `FROM`-less sources are excluded:
    /// their output is bounded by the statement itself. Scope resolution
    /// borrows `arena`; the caller rewinds it.
    pub(crate) fn stream_source(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        arena: &'a Arena,
    ) -> Result<Option<StreamSource>, SqlError> {
        let Some(from) = statement.from.as_ref() else {
            return Ok(None);
        };
        if let Some(leaves) = appended_leaves(statement, arena)? {
            return Self::appended_source(storage, txid, statement, leaves, arena);
        }
        if !from.joins.is_empty() {
            let Some((scope, shape, ..)) =
                retained_join_scope(storage, txid, statement, from, None, true, true, arena)?
            else {
                return Ok(None);
            };
            let mut tables = [None; MAX_RETAINED_SOURCES];
            for (table, item) in shape.items.iter().enumerate().take(scope.n) {
                if matches!(item, Some(join::ItemShape::Table)) {
                    tables[table] = Some(scope.slots[table]);
                }
            }
            return Ok(Some(StreamSource::Join(tables)));
        }
        let generated = from.joins.is_empty()
            && from.base.is_function_source()
            && retained_function_source(&from.base, storage, txid, arena).is_some();
        if !from.joins.is_empty()
            || from.base.is_function_source() && !generated
            || from.base.subquery.is_some()
            || from.base.cte.is_some()
            || from.base.sample.is_some()
            || !statement.with.is_empty()
            || statement.set_body.is_some()
            || !statement.grouping_sets.is_empty()
            || !statement.locking.is_empty()
            || statement.with_ties
            || !generated
                && !matches!(
                    storage.resolve_relation(from.base.schema, from.base.table, txid),
                    Some(crate::storage::ResolvedRelation::Table(_))
                )
        {
            return Ok(None);
        }
        let catalog = storage_catalog(storage, arena, txid);
        let expression_mode = Some((storage, txid));
        if !statement.items.iter().all(|item| match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
            SelectItem::Expr { expression, .. } => {
                retained_expression_with(expression, &catalog, expression_mode, Leaves::Calls)
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
        if scope.n != 1
            || !generated && (scope.derived[0].is_some() || scope.slots[0] == usize::MAX)
        {
            return Ok(None);
        }
        let columns = CatalogScopeCols {
            scope: &scope,
            outer_scope: None,
            storage,
            txid,
            arena,
        };
        let Some((statement, aggregates)) =
            reader_grouping(statement, &columns, &catalog, true, arena)?
        else {
            return Ok(None);
        };
        if aggregates.is_none() {
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
            if sort::sorts(statement) && !sort::sort_admitted(statement, &columns, &catalog)? {
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
        if generated {
            return Ok(Some(StreamSource::Generated));
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
        Ok(Some(StreamSource::Table(table)))
    }

    /// [`Self::stream_source`] of a `UNION ALL` read as one appended
    /// stream: every query streams on its own and runs in place, and their
    /// physical tables together fit one retained join's.
    fn appended_source(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        leaves: &'a [&'a Select<'a>],
        arena: &'a Arena,
    ) -> Result<Option<StreamSource>, SqlError> {
        if leaves.len() > MAX_RETAINED_SOURCES {
            return Ok(None);
        }
        let catalog = storage_catalog(storage, arena, txid);
        if [statement.limit, statement.offset]
            .into_iter()
            .flatten()
            .any(|bound| !retained_bound(bound, &catalog))
        {
            return Ok(None);
        }
        let mut tables = [None; MAX_RETAINED_SOURCES];
        let mut count = 0;
        for leaf in leaves {
            if !branch_admitted(storage, txid, leaf, arena)? {
                return Ok(None);
            }
            let leaf_tables = match Self::stream_source(storage, txid, leaf, arena)? {
                Some(StreamSource::Table(table)) => {
                    [Some(table), None, None, None, None, None, None, None]
                }
                Some(StreamSource::Generated) => [None; MAX_RETAINED_SOURCES],
                Some(StreamSource::Join(tables)) => tables,
                None => return Ok(None),
            };
            for table in leaf_tables.into_iter().flatten() {
                let Some(target) = tables.get_mut(count) else {
                    return Ok(None);
                };
                *target = Some(table);
                count += 1;
            }
        }
        // The combined description and bounds resolve as the reader's will.
        let scope = QueryScope::resolve_schema(
            storage,
            statement.from.as_ref().expect("appended source"),
            txid,
            arena,
            None,
        )?;
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
        Ok(Some(StreamSource::Join(tables)))
    }

    pub(crate) fn open(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        responder: &mut Responder<'_>,
        reader_use: RetainedUse,
    ) -> Result<Option<Self>, SqlError> {
        Self::open_mode(
            storage,
            txid,
            statement,
            params,
            arena,
            responder,
            RetainedMode::Cursor,
            reader_use,
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
            RetainedUse::Stream,
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

    #[allow(clippy::too_many_arguments)]
    fn open_mode(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        responder: &mut Responder<'_>,
        mode: RetainedMode,
        reader_use: RetainedUse,
    ) -> Result<Option<Self>, SqlError> {
        // A stream or statement returns its rows once, in order: it sorts
        // them itself. A declared cursor keeps its own ordered path.
        let self_sorts = sort::sorts(statement)
            && (mode == RetainedMode::Statement || reader_use == RetainedUse::Stream);
        // Likewise it groups them itself.
        let groups_itself = mode == RetainedMode::Statement || reader_use == RetainedUse::Stream;
        if let Some(from) = statement.from.as_ref()
            && !from.joins.is_empty()
        {
            return Self::open_join(
                storage,
                txid,
                statement,
                from,
                params,
                arena,
                responder,
                mode,
                self_sorts,
                groups_itself,
            );
        }
        let from = statement.from.as_ref();
        let literal_count = from
            .and_then(|from| from.base.subquery)
            .and_then(literal_source);
        let literals = literal_count.is_some();
        // A stream appends the queries of a UNION ALL itself.
        let appended = if mode == RetainedMode::Cursor && reader_use == RetainedUse::Stream {
            appended_leaves(statement, arena)?
        } else {
            None
        };
        if from.is_none() && mode == RetainedMode::Statement {
            return Ok(None);
        }
        let generated = from
            .filter(|from| from.joins.is_empty() && from.base.is_function_source())
            .and_then(|from| retained_function_source(&from.base, storage, txid, arena));
        if from.is_some_and(|from| {
            !from.joins.is_empty()
                || from.base.is_function_source() && generated.is_none()
                || from.base.subquery.is_some() && !literals && appended.is_none()
                || from.base.cte.is_some()
                || from.base.sample.is_some()
                || matches!(
                    storage.resolve_relation(from.base.schema, from.base.table, txid),
                    Some(crate::storage::ResolvedRelation::View(_))
                )
        }) || !statement.with.is_empty()
            || statement.set_body.is_some()
            || !statement.distinct_on.is_empty() && !self_sorts
            || !groups_itself
                && (statement.distinct
                    || !statement.group_by.is_empty()
                    || statement.having.is_some())
            || !statement.grouping_sets.is_empty()
            || statement.with_ties
        {
            return Ok(None);
        }
        // A cursor over one table sorts its qualifying rows at the first
        // FETCH when ordered, and locks rows as it fetches them; other
        // ordered or locking shapes run at DECLARE.
        let locks = !statement.locking.is_empty();
        if (locks || !statement.order_by.is_empty() && !self_sorts)
            && (mode == RetainedMode::Statement
                || literals
                || generated.is_some()
                || from.is_none())
            || self_sorts && (locks || from.is_none())
        {
            return Ok(None);
        }
        let catalog = storage_catalog(storage, arena, txid);
        let expression_mode = Some((storage, txid));
        let leaves = if groups_itself {
            Leaves::Calls
        } else {
            Leaves::Scalar
        };
        if !statement.items.iter().all(|item| match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
            SelectItem::Expr { expression, .. } => {
                retained_expression_with(expression, &catalog, expression_mode, leaves)
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
                    scope.derived[0] = Some(literal_rows(
                        storage,
                        txid,
                        from.base.subquery.expect("literal source"),
                        count,
                        arena,
                        params,
                    )?);
                }
                if scope.n != 1
                    || !literals
                        && generated.is_none()
                        && appended.is_none()
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
            outer_scope: None,
            storage,
            txid,
            arena,
        };
        let Some((statement, aggregates)) =
            reader_grouping(statement, &columns, &catalog, groups_itself, arena)?
        else {
            return Ok(None);
        };
        if aggregates.is_some() && from.is_none() {
            return Ok(None);
        }
        if aggregates.is_none() {
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
        let table = if let Some(from) =
            from.filter(|_| !literals && generated.is_none() && appended.is_none())
        {
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
        if self_sorts
            && aggregates.is_none()
            && !sort::sort_admitted(statement, &columns, &catalog)?
        {
            return Ok(None);
        }
        let order = if self_sorts {
            &[][..]
        } else {
            match order_keys(statement, &scope, storage, arena)? {
                Some(order) => order,
                None => return Ok(None),
            }
        };
        check_select_types(statement, &scope, storage, txid, arena, None)?;
        check_select_constants(statement, arena, storage, txid)?;
        let locking = if locks {
            if table.is_none() {
                return Ok(None);
            }
            super::validate_locking(statement, storage, txid, arena)?;
            super::check_key_types(statement, &scope, arena, storage, txid)?;
            arena
                .alloc_slice_copy(statement.locking)
                .map_err(|_| arena_full())?
        } else {
            &[][..]
        };
        let demand = from
            .filter(|_| table.is_some())
            .map(|from| match aggregates {
                Some(_) => grouping::input_demand(&scope, from, statement, arena),
                None => Ok(streaming_pax_columns_ordered(
                    &scope,
                    from,
                    statement.items,
                    statement.where_clause,
                    statement.order_by,
                )),
            })
            .transpose()?;
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
        // A locking cursor's sorted inputs are saved whole, as a changed
        // row is filtered and projected again from its newest version.
        // Other sorted inputs keep the columns the query reads, sort keys
        // included.
        let decoded_columns = demand
            .filter(|_| order.is_empty() || !locks)
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
        // Grouped output evaluates per group as written: aggregate results
        // are found by their call's address.
        if aggregates.is_none() {
            for item in items.iter_mut() {
                if let SelectItem::Expr { expression, .. } = item {
                    *expression = bind_expression(expression, arena, binding_columns, None)?;
                }
            }
        }
        let predicate = statement
            .where_clause
            .map(|predicate| {
                // A grouping filters its input as the general executor's
                // scan does, with the same planning-time folding.
                if mode == RetainedMode::Statement && aggregates.is_none() {
                    return Ok(predicate);
                }
                let predicate = simplify_qual(predicate, arena)?;
                let predicate = plan::fold_null(predicate, &scope, arena)?;
                reorder_qual(predicate, &scope, arena)
            })
            .transpose()?;
        let (predicate, eliminated) = match predicate {
            Some(predicate)
                if mode != RetainedMode::Statement && row_independent(predicate, &scope) =>
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
                SelectItem::Wildcard | SelectItem::TableWildcard(_) => false,
            })
            && !predicate.is_some_and(|predicate| {
                super::super::eval::routine_input_handoff(predicate, Some(&catalog))
            })
            && !statement.having.is_some_and(|having| {
                super::super::eval::routine_input_handoff(having, Some(&catalog))
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
        let bounds = if mode != RetainedMode::Cursor {
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
        let source = if let Some(leaves) = appended {
            let definition = scope.defs.get(0).expect("appended result definition");
            let mut schema = [ColType::Bool; MAX_COLUMNS];
            definition.schema(&mut schema);
            let targets = arena
                .alloc_slice_copy(&schema[..definition.n_columns])
                .map_err(|_| arena_full())?;
            let mut opened: [Option<RetainedSelect<'a>>; MAX_RETAINED_SOURCES] = Default::default();
            for (slot, leaf) in opened.iter_mut().zip(leaves) {
                let Some(mut branch) = Self::open_mode(
                    storage,
                    txid,
                    leaf,
                    params,
                    arena,
                    responder,
                    RetainedMode::Branch,
                    RetainedUse::Stream,
                )?
                else {
                    return Ok(None);
                };
                branch.start_generator(storage, txid, arena, params)?;
                *slot = Some(branch);
            }
            let branches = arena
                .alloc_owned_slice_with(leaves.len(), |index| {
                    opened[index].take().expect("opened appended query")
                })
                .map_err(|_| arena_full())?;
            RetainedSource::Append {
                branches,
                next: 0,
                targets,
            }
        } else if let Some(table) = table {
            let mut source = RowCursor::closed();
            if !eliminated {
                scan::validate_materialized_view_reads(storage, &scope, None, arena, txid)?;
                source.open(storage, table, txid, arena)?;
            }
            RetainedSource::Physical(source)
        } else if let (Some(generated), Some(from)) = (generated, from) {
            RetainedSource::Generated {
                function: &from.base,
                kinds: generated,
                generator: eliminated.then(super::generator::FunctionRows::finished),
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
        let sort = if self_sorts && aggregates.is_none() {
            Some(sort::SortState::new(
                storage,
                txid,
                statement,
                items,
                &scope,
                &CatalogScopeCols {
                    scope: &scope,
                    outer_scope: None,
                    storage,
                    txid,
                    arena,
                },
                arena,
            )?)
        } else {
            None
        };
        let grouping = match (aggregates, from) {
            (Some(aggregates), Some(from)) => Some(grouping::GroupState::new(
                storage, txid, statement, from, aggregates, &scope, arena,
            )?),
            _ => None,
        };
        let revisit = cursor_revisit(
            &source,
            eliminated,
            !order.is_empty(),
            statement,
            &CatalogScopeCols {
                scope: &scope,
                outer_scope: None,
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
        let description = describe(&columns[..width], mode, arena, responder)?;
        let decoded_columns = decoded_columns
            .map(|mask| arena.alloc(mask).map(|mask| &*mask))
            .transpose()
            .map_err(|_| arena_full())?;
        let leaf = PlanNode::Scan(ScanNode {
            source,
            pending: None,
            decoded_columns,
        });
        let above = sort.map(PlanNode::Sort).or(grouping.map(PlanNode::Group));
        let plan = PlanTree::new(leaf, predicate, above, items, bounds, eliminated)?;
        Ok(Some(Self {
            scope,
            plan,
            revisit,
            locking,
            order,
            saves_scroll_output: false,
            description,
        }))
    }

    /// Opens a join as nested-loop levels, one retained source per FROM
    /// item. A cursor over it evaluates rows per FETCH; its output and WHERE
    /// run in the caller, so routine calls hand off on their row.
    #[allow(clippy::too_many_arguments)]
    fn open_join(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        from: &'a FromClause<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        responder: &mut Responder<'_>,
        mode: RetainedMode,
        self_sorts: bool,
        groups_itself: bool,
    ) -> Result<Option<Self>, SqlError> {
        let Some((scope, shape, statement, aggregates)) = retained_join_scope(
            storage,
            txid,
            statement,
            from,
            Some(params),
            self_sorts,
            groups_itself,
            arena,
        )?
        else {
            return Ok(None);
        };
        let catalog = storage_catalog(storage, arena, txid);
        let columns = CatalogScopeCols {
            scope: &scope,
            outer_scope: None,
            storage,
            txid,
            arena,
        };
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
        check_select_types(statement, &scope, storage, txid, arena, None)?;
        check_select_constants(statement, arena, storage, txid)?;
        let demand = match aggregates {
            Some(_) => grouping::input_demand(&scope, from, statement, arena)?,
            None => streaming_pax_columns_ordered(
                &scope,
                from,
                statement.items,
                statement.where_clause,
                statement.order_by,
            ),
        };
        scan::admit_sources(storage, &scope, txid, demand)?;
        let mut description = [ColDesc::new("", 0, 0); MAX_PROJ];
        let width = describe_scope_items(
            statement.items,
            &scope,
            None,
            storage,
            txid,
            arena,
            &mut description,
            crate::sql::exec::DescriptionTarget::Sql,
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
            &mut description[..width],
        )?;
        let items = arena.alloc_slice_copy(statement.items).map_err(|error| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained query plan allocation failed: {}",
                error
            )
        })?;
        if aggregates.is_none() {
            for item in items.iter_mut() {
                if let SelectItem::Expr { expression, .. } = item {
                    *expression = bind_expression(expression, arena, binding_columns, None)?;
                }
            }
        }
        let predicate = statement
            .where_clause
            .map(|predicate| {
                // A grouping filters its input as the general executor's
                // scan does, with the same planning-time folding.
                if mode == RetainedMode::Statement && aggregates.is_none() {
                    return Ok(predicate);
                }
                let predicate = simplify_qual(predicate, arena)?;
                let predicate = plan::fold_null(predicate, &scope, arena)?;
                reorder_qual(predicate, &scope, arena)
            })
            .transpose()?;
        let (predicate, eliminated) = match predicate {
            Some(predicate)
                if mode != RetainedMode::Statement && row_independent(predicate, &scope) =>
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
            && !statement.having.is_some_and(|having| {
                super::super::eval::routine_input_handoff(having, Some(&catalog))
            })
        {
            return Ok(None);
        }
        // PostgreSQL materializes a join under a scrollable cursor: revisited
        // rows return their saved output, never a volatile value again.
        let volatile = |expression: &Expr<'_>| {
            super::predicate_safety::expression_properties(
                expression,
                &columns,
                storage,
                txid,
                super::predicate_safety::AnalysisStage::RuntimeProjection,
            )
            .map(|properties| properties.volatile)
        };
        let mut saves_scroll_output = false;
        for expression in statement
            .items
            .iter()
            .filter_map(|item| match item {
                SelectItem::Expr { expression, .. } => Some(*expression),
                _ => None,
            })
            .chain(statement.where_clause)
        {
            saves_scroll_output |= volatile(expression)?;
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
        let bounds = if mode != RetainedMode::Cursor {
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
        for (table, item) in shape.items.iter().enumerate().take(scope.n) {
            if mode == RetainedMode::Statement && matches!(item, Some(join::ItemShape::Table)) {
                storage.lock_table(
                    txid,
                    scope.slots[table],
                    crate::sql::ast::TableLockMode::AccessShare,
                    false,
                )?;
            }
        }
        if !eliminated {
            scan::validate_materialized_view_reads(storage, &scope, None, arena, txid)?;
        }
        let joins = join::open_levels(
            storage, txid, from, &scope, &shape, demand, &columns, eliminated, arena,
        )?;
        let sort = if self_sorts && aggregates.is_none() {
            Some(sort::SortState::new(
                storage, txid, statement, items, &scope, &columns, arena,
            )?)
        } else {
            None
        };
        let grouping = aggregates
            .map(|aggregates| {
                grouping::GroupState::new(storage, txid, statement, from, aggregates, &scope, arena)
            })
            .transpose()?;
        crate::sql::exec::DescriptionTarget::Wire.finish(
            &mut description[..width],
            Some(storage),
            txid,
        )?;
        let description = describe(&description[..width], mode, arena, responder)?;
        let above = sort.map(PlanNode::Sort).or(grouping.map(PlanNode::Group));
        let plan = PlanTree::new(
            PlanNode::Join(joins),
            predicate,
            above,
            items,
            bounds,
            eliminated,
        )?;
        Ok(Some(Self {
            scope,
            plan,
            revisit: crate::sql::cursor::Revisit::SavedOutput,
            locking: &[],
            order: &[],
            saves_scroll_output,
            description,
        }))
    }

    /// Captures input and records scan statistics before any row expression
    /// runs. The caller owns predicate and projection progress while this row
    /// remains pending, including across mutable function execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_statement_row(
        &mut self,
        storage: &Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &Arena,
        params: &[Datum<'_>],
    ) -> Result<bool, SqlError> {
        assert!(
            matches!(self.plan.bounds(), RowBounds::Active { .. }),
            "statement bounds must be prepared"
        );
        self.plan.advance(&mut Pull {
            scope: &self.scope,
            storage,
            txid,
            observed_at,
            state_arena,
            buffers,
            work,
            params,
        })
    }

    /// Moves an appended query to its next row.
    fn prepare_branch_row(&mut self, pull: &mut Pull<'_, 'a>) -> Result<bool, SqlError> {
        self.plan.advance(&mut Pull {
            scope: &self.scope,
            storage: pull.storage,
            txid: pull.txid,
            observed_at: pull.observed_at,
            state_arena: pull.state_arena,
            buffers: pull.buffers,
            work: pull.work,
            params: pull.params,
        })
    }

    /// The single source's current row.
    fn pending(&self) -> Option<&PendingRow<'a>> {
        self.plan.scan().and_then(|scan| scan.pending.as_ref())
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
        if let Some(grouping) = self.plan.group() {
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
            return grouping.having_passes(&self.scope, work, params, &hooks);
        }
        // A sorted row passed its filter before the sort.
        let Some(predicate) = self.plan.driver_filter() else {
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
        self.plan.with_row(&self.scope, work, |row| {
            where_passes(predicate, work, params, row, &hooks)
        })
    }

    pub(crate) fn statement_items_len(&self) -> usize {
        self.plan.items().len()
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
        let items = self.plan.items();
        if let Some(sort) = self.plan.sort()
            && sort.has_current()
            && !sort.postponed(index)
        {
            return sort.saved_item(index, frame_arena);
        }
        if let Some(grouping) = self.plan.group() {
            let SelectItem::Expr { expression, .. } = items[index] else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "grouped output item is not an expression"
                ));
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
            let value = grouping.with_group(&self.scope, work, &hooks, |row, hooks| {
                eval_full(expression, work, params, row, hooks)
            })?;
            let detached = frame_arena
                .alloc_slice_with(1, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            detached[0] = super::super::exec::detach_routine_datum(value, frame_arena)?;
            return Ok(&*detached);
        }
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
        self.plan.with_row(&self.scope, work, |row| {
            let mut projected = [Datum::Null; MAX_PROJ];
            let width = project_row(
                &items[index..index + 1],
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
        let RowBounds::Active { skip, .. } = self.plan.bounds_mut() else {
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
            let RowBounds::Active { remaining, .. } = self.plan.bounds_mut() else {
                unreachable!("statement bounds must be prepared")
            };
            *remaining = remaining.checked_sub(1).expect("prepared remaining row");
        }
        // The caller has emitted or dropped every evaluation value of the
        // row.
        self.plan.release(state_arena);
    }

    /// Whether the statement's RowDescription still waits for its first
    /// row.
    pub(crate) fn description_pending(&self) -> bool {
        self.description.is_some()
    }

    /// Sends the statement's RowDescription once, before its first row or
    /// error.
    pub(crate) fn send_description(
        &mut self,
        responder: &mut Responder<'_>,
    ) -> Result<(), SqlError> {
        match self.description.take() {
            Some(columns) => responder
                .row_description(columns)
                .map_err(|_| description_too_large(RetainedMode::Statement)),
            None => Ok(()),
        }
    }

    /// Builds a join's hash tables before the first row. A declared cursor
    /// does this at `DECLARE`, where a build that does not fit
    /// `plan_build_bytes` still leaves the cursor to save its rows instead.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_tables(
        &mut self,
        storage: &Storage,
        txid: u32,
        observed_at: i64,
        state_arena: &'a Arena,
        buffers: &mut RowCursorBuffers,
        work: &Arena,
    ) -> Result<(), SqlError> {
        self.plan.build(&mut Pull {
            scope: &self.scope,
            storage,
            txid,
            observed_at,
            state_arena,
            buffers,
            work,
            params: &[],
        })
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
        if let RowBounds::Unstarted { limit, offset } = self.plan.bounds() {
            *self.plan.bounds_mut() = RowBounds::Active {
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
        self.prepare_statement_row(
            storage,
            txid,
            observed_at,
            state_arena,
            buffers,
            work,
            params,
        )
    }
}
