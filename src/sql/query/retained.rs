//! Retained physical input and projection positions for SQL cursors.

use super::*;
use crate::mem::arena::ArenaMark;
use crate::storage::{MAX_COLUMNS, RowCursor, RowCursorBuffers, SpilledRowRepresentation, rowenc};

struct PendingRow<'a> {
    rowid: u64,
    representation: SpilledRowRepresentation<'a>,
    mark: ArenaMark,
}

pub(crate) struct RetainedSelect<'a> {
    statement: &'a Select<'a>,
    scope: QueryScope<'a>,
    source: RowCursor,
    decoded_columns: Option<[bool; MAX_COLUMNS]>,
    pending: Option<PendingRow<'a>>,
    done: bool,
}

fn projection_only(items: &[SelectItem<'_>]) -> bool {
    items.iter().all(|item| match item {
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
        SelectItem::Expr { expression, .. } => matches!(
            expression,
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
                | Expr::BitLit(_)
        ),
        SelectItem::RecordStar(_) => false,
    })
}

impl<'a> RetainedSelect<'a> {
    pub(crate) fn uses_table(&self, table: usize) -> bool {
        self.scope.slots[..self.scope.n].contains(&table)
    }

    pub(crate) fn open(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        params: &[Datum<'a>],
        arena: &'a Arena,
        observed_at: i64,
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
            || statement.where_clause.is_some()
            || statement.limit.is_some()
            || statement.offset.is_some()
            || statement.with_ties
            || !projection_only(statement.items)
        {
            return Ok(None);
        }
        let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
        if scope.n != 1 || scope.derived[0].is_some() || scope.slots[0] == usize::MAX {
            return Ok(None);
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
        let demand = streaming_pax_columns(&scope, from, statement.items, None);
        scan::admit_sources(storage, &scope, txid, demand)?;
        scan::validate_materialized_view_reads(storage, &scope, None, arena, txid)?;
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
        source.open(storage, table, txid, arena)?;
        storage.record_relation_scan(txid, table, None, 0, observed_at)?;
        let decoded_columns = demand.selected_mask(0).map(|mask| {
            core::array::from_fn(|column| column < u64::BITS as usize && mask & (1 << column) != 0)
        });
        Ok(Some(Self {
            statement,
            scope,
            source,
            decoded_columns,
            pending: None,
            done: false,
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
        params: &[Datum<'work>],
        state_arena: &'a Arena,
        work: &'work Arena,
        buffers: &mut RowCursorBuffers,
        responder: &mut Responder<'_>,
    ) -> Result<bool, SqlError> {
        if self.done {
            return Ok(false);
        }
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
                self.done = true;
                self.source.close();
                return Ok(false);
            };
            self.pending = Some(PendingRow {
                rowid: row.rowid,
                representation: row.representation,
                mark,
            });
        }
        let pending = self.pending.as_ref().expect("captured input row");
        let definition = self.scope.defs.get(0).expect("retained source definition");
        let mut values = [Datum::Null; MAX_COLUMNS];
        match pending.representation {
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
        let rowids = [Some(pending.rowid)];
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
        let mut projected = [Datum::Null; MAX_PROJ];
        let width = project_row(
            self.statement.items,
            &self.scope,
            &row,
            work,
            params,
            &hooks,
            &mut projected,
            None,
        )?;
        emit_data_row(storage, txid, work, responder, &projected[..width])
            .map_err(|_| super::super::cursor_result_too_large())??;
        storage.record_relation_tuple_read(txid, self.scope.slots[0], None)?;
        let mark = self.pending.take().expect("consumed input row").mark;
        // The typed result is copied into the cursor buffer before this rewind.
        unsafe { state_arena.rewind_to(mark) };
        Ok(true)
    }
}
