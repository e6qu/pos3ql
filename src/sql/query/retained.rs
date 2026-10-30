//! Retained physical input and projection positions for SQL cursors.

use super::*;
use crate::mem::arena::ArenaMark;
use crate::storage::{
    CursorRow, MAX_COLUMNS, RowCursor, RowCursorBuffers, SpilledRowRepresentation, rowenc,
};

struct PendingRow<'a> {
    row: CursorRow<'a>,
    mark: ArenaMark,
    counted: bool,
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
        | Expr::BitLit(_) => true,
        Expr::Unary {
            operator: UnaryOp::Neg | UnaryOp::Not | UnaryOp::BitNot,
            operand,
        }
        | Expr::IsNull { operand, .. } => retained_expression(operand, catalog),
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            operator
                .operator_name()
                .is_none_or(|name| !catalog.has_operator_candidate(name))
                && retained_expression(left, catalog)
                && retained_expression(right, catalog)
        }
        Expr::Case {
            operand: None,
            whens,
            otherwise,
            ..
        } => {
            whens.iter().all(|(condition, value)| {
                retained_expression(condition, catalog) && retained_expression(value, catalog)
            }) && otherwise.is_none_or(|value| retained_expression(value, catalog))
        }
        Expr::Array(items) => items.iter().all(|item| retained_expression(item, catalog)),
        Expr::Subscript { base, index } => {
            retained_expression(base, catalog) && retained_expression(index, catalog)
        }
        Expr::Slice { base, lower, upper } => {
            retained_expression(base, catalog)
                && lower.is_none_or(|bound| retained_expression(bound, catalog))
                && upper.is_none_or(|bound| retained_expression(bound, catalog))
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
        } => retained_bound(operand, catalog),
        Expr::Binary { left, right, .. } => {
            retained_expression(expression, catalog)
                && retained_bound(left, catalog)
                && retained_bound(right, catalog)
        }
        _ => false,
    }
}

// Freeze builtin operator choice at DECLARE. A later CREATE OPERATOR or
// search_path change must not rebind expressions on their next FETCH.
fn bind_expression<'a>(
    expression: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let full = || {
        sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "cursor query plan exceeds cursor_query_bytes"
        )
    };
    let bound = match expression {
        Expr::Unary { operator, operand } => Expr::Unary {
            operator: *operator,
            operand: bind_expression(operand, arena)?,
        },
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: bind_expression(operand, arena)?,
            negated: *negated,
        },
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            let left = bind_expression(left, arena)?;
            let right = bind_expression(right, arena)?;
            if let Some(symbol) = operator.operator_name() {
                let encoded = crate::stack_format!(
                    260,
                    "{}pg_catalog\u{1f}{}",
                    crate::sql::ast::CATALOG_OPERATOR_CALL_PREFIX,
                    symbol
                );
                let name = arena.alloc_str(encoded.as_str()).map_err(|_| full())?;
                let args = arena.alloc_slice_copy(&[left, right]).map_err(|_| full())?;
                Expr::Call {
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
            let whens = arena.alloc_slice_copy(whens).map_err(|_| full())?;
            for (condition, value) in whens.iter_mut() {
                *condition = bind_expression(condition, arena)?;
                *value = bind_expression(value, arena)?;
            }
            Expr::Case {
                operand: None,
                whens,
                otherwise: otherwise
                    .map(|value| bind_expression(value, arena))
                    .transpose()?,
                synthetic: *synthetic,
            }
        }
        Expr::Array(items) => {
            let items = arena.alloc_slice_copy(items).map_err(|_| full())?;
            for item in items.iter_mut() {
                *item = bind_expression(item, arena)?;
            }
            Expr::Array(items)
        }
        Expr::Subscript { base, index } => Expr::Subscript {
            base: bind_expression(base, arena)?,
            index: bind_expression(index, arena)?,
        },
        Expr::Slice { base, lower, upper } => Expr::Slice {
            base: bind_expression(base, arena)?,
            lower: lower
                .map(|value| bind_expression(value, arena))
                .transpose()?,
            upper: upper
                .map(|value| bind_expression(value, arena))
                .transpose()?,
        },
        _ => return Ok(expression),
    };
    Ok(arena.alloc(bound).map_err(|_| full())?)
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
        if !statement.items.iter().all(|item| match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
            SelectItem::Expr { expression, .. } => retained_expression(expression, &catalog),
            SelectItem::RecordStar(_) => false,
        }) || statement
            .where_clause
            .is_some_and(|predicate| !retained_expression(predicate, &catalog))
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
        let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
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
        let table = scope.slots[0];
        let definition = scope.defs.get(0).expect("resolved physical source");
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
        check_select_types(statement, &scope, storage, txid, arena)?;
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
        responder
            .row_description(&columns[..width])
            .map_err(|_| super::super::cursor_description_too_large())?;
        let mut source = RowCursor::closed();
        let decoded_columns = demand.selected_mask(0).map(|mask| {
            core::array::from_fn(|column| column < u64::BITS as usize && mask & (1 << column) != 0)
        });
        let items = arena.alloc_slice_copy(statement.items).map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "cursor query plan exceeds cursor_query_bytes"
            )
        })?;
        for item in items.iter_mut() {
            if let SelectItem::Expr { expression, .. } = item {
                *expression = bind_expression(expression, arena)?;
            }
        }
        let predicate = statement
            .where_clause
            .map(|predicate| {
                let predicate = simplify_qual(predicate, arena)?;
                let predicate = plan::fold_null(predicate, &scope, arena)?;
                reorder_qual(predicate, &scope, arena)
            })
            .transpose()?;
        let (predicate, eliminated) = match predicate {
            Some(predicate) if row_independent(predicate, &scope) => {
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
            .map(|predicate| bind_expression(predicate, arena))
            .transpose()?;
        let bounds = RowBounds::Unstarted {
            limit: statement
                .limit
                .map(|bound| bind_expression(bound, arena))
                .transpose()?,
            offset: statement
                .offset
                .map(|bound| bind_expression(bound, arena))
                .transpose()?,
        };
        if !eliminated {
            scan::validate_materialized_view_reads(storage, &scope, None, arena, txid)?;
            source.open(storage, table, txid, arena)?;
        }
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
        loop {
            check_timeout()?;
            let work_mark = work.mark();
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
                        // The reader keeps positions and download buffers, never row-arena references.
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
            let pending = self.pending.as_mut().expect("captured input row");
            if !pending.counted {
                storage.record_relation_tuple_read(txid, self.scope.slots[0], None)?;
                pending.counted = true;
            }
            let definition = self.scope.defs.get(0).expect("retained source definition");
            let mut values = [Datum::Null; MAX_COLUMNS];
            match pending.row.representation() {
                SpilledRowRepresentation::Encoded(bytes) => {
                    let mut schema = [ColType::Bool; MAX_COLUMNS];
                    definition.schema(&mut schema);
                    rowenc::decode(bytes, &schema[..definition.n_columns], &mut values)?;
                }
                SpilledRowRepresentation::Values(decoded) => {
                    if decoded.len() > definition.n_columns {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "retained row exceeds its source definition"
                        ));
                    }
                    values[..decoded.len()].copy_from_slice(decoded);
                }
            }
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
