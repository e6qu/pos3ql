//! A retained sort: the reader's input rows sorted in `plan_build_bytes`.
//!
//! At the first row, the sort reads its whole input (one table, generator
//! calls, a literal list or a retained join), filters it, and saves each
//! passing row's sort keys and output values in projected encoding. An
//! output item that calls a volatile function is not saved: as PostgreSQL's
//! planner postpones volatile output past the sort, it runs when its row is
//! returned, over the row's saved input columns, so `nextval` numbers rows in
//! sorted order. A storage wait during the fill keeps the rows saved so far
//! and the input position.

use super::*;
use crate::mem::lease_pool::{LeaseError, MutableSlice};
use crate::sql::ast::Collation;

#[derive(Clone, Copy)]
pub(super) struct SortKey<'a> {
    expression: &'a Expr<'a>,
    descending: bool,
    nulls_first: bool,
    collation: Collation,
}

/// Where one output item's values are, or that it runs after the sort.
#[derive(Clone, Copy)]
enum ItemPlace {
    Saved { start: usize, width: usize },
    Postponed,
}

pub(super) struct SortState<'a> {
    keys: &'a [SortKey<'a>],
    /// `DISTINCT ON`: the last `unique` keys are its expressions, and only
    /// the first sorted row of each run of equal values is returned.
    unique: usize,
    items: &'a [ItemPlace],
    /// Saved values before the input columns: keys, then saved items.
    saved_values: usize,
    /// Postponed items need each row's input columns.
    keeps_inputs: bool,
    entries: MutableSlice<(u32, u32)>,
    bytes: MutableSlice<u8>,
    filled: bool,
    next: usize,
    current: Option<usize>,
}

fn sort_full(error: LeaseError) -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "retained sort exceeds plan_build_bytes: {}",
        error
    )
    .plan_build_exhausted()
}

/// Output values one item produces; a qualified `table.*` is not admitted.
fn item_width(item: &SelectItem<'_>, scope: &QueryScope<'_>) -> usize {
    match item {
        SelectItem::Wildcard(_) => scope.star_columns(),
        _ => 1,
    }
}

/// Whether the statement's output is sorted: by its `ORDER BY`, or by its
/// `DISTINCT ON` expressions to keep each first row.
pub(super) fn sorts(statement: &Select<'_>) -> bool {
    !statement.order_by.is_empty() || !statement.distinct_on.is_empty()
}

/// Decides whether a retained reader can sort this statement itself, before
/// it runs: local mode only (object-store mode sorts in external runs), keys
/// and filter that are non-volatile and need no routine hand-off, and output
/// items that either run before the sort the same way or are volatile and
/// run after it. With `LIMIT`/`OFFSET`, PostgreSQL may also postpone costly
/// output, so only columns and constants run before such a sort.
pub(super) fn sort_admitted(
    statement: &Select<'_>,
    columns: &CatalogScopeCols<'_, '_, '_, '_>,
    catalog: &dyn crate::sql::eval::CatalogAccess,
) -> Result<bool, SqlError> {
    let storage = columns.storage;
    if !sorts(statement)
        || storage.spill_attached()
        || statement.with_ties
        || !statement.locking.is_empty()
    {
        return Ok(false);
    }
    let volatile = |expression: &Expr<'_>| {
        super::predicate_safety::expression_properties(
            expression,
            columns,
            storage,
            columns.txid,
            super::predicate_safety::AnalysisStage::RuntimeProjection,
        )
        .map(|properties| properties.volatile)
    };
    let mut safe_casts = true;
    let mut before_sort = |expression: &Expr<'_>| -> Result<bool, SqlError> {
        validate_statement_casts(expression, columns, &mut safe_casts)?;
        Ok(
            retained_expression(expression, catalog, Some((storage, columns.txid)))
                && retained_scalar_calls(expression, columns)?
                && !crate::sql::eval::routine_input_handoff(expression, Some(catalog))
                && !volatile(expression)?,
        )
    };
    let keys = statement
        .order_by
        .iter()
        .map(|order| order.expression)
        .chain(statement.distinct_on.iter().copied());
    for key in keys {
        let expression = crate::sql::exec::resolve_order_expr_pub(key, statement.items)?;
        let expression = if let Expr::Int(position) = expression {
            if statement
                .items
                .iter()
                .any(|item| !matches!(item, SelectItem::Expr { .. }))
            {
                return Ok(false);
            }
            match usize::try_from(*position)
                .ok()
                .and_then(|position| position.checked_sub(1))
                .and_then(|index| statement.items.get(index))
            {
                Some(SelectItem::Expr { expression, .. }) => expression,
                _ => return Ok(false),
            }
        } else {
            expression
        };
        if !before_sort(expression)? {
            return Ok(false);
        }
    }
    if let Some(predicate) = statement.where_clause
        && !before_sort(predicate)?
    {
        return Ok(false);
    }
    let bounded = statement.limit.is_some() || statement.offset.is_some();
    for item in statement.items {
        let expression = match item {
            SelectItem::Expr { expression, .. } => expression,
            SelectItem::Wildcard(_) => continue,
            SelectItem::TableWildcard(_) | SelectItem::RecordStar(_) => return Ok(false),
        };
        // The general executor postpones volatile output past a sort only
        // without DISTINCT ON.
        if volatile(expression)? && statement.distinct_on.is_empty() {
            continue;
        }
        if !before_sort(expression)?
            || bounded
                && !matches!(
                    expression,
                    Expr::Column { .. }
                        | Expr::Null
                        | Expr::Bool(_)
                        | Expr::Int(_)
                        | Expr::Float(_)
                        | Expr::NumericLit(_)
                        | Expr::Str(_)
                )
        {
            return Ok(false);
        }
    }
    Ok(safe_casts)
}

impl<'a> SortState<'a> {
    /// Plans the keys and item places of an admitted sort; `items` are the
    /// reader's bound output items.
    pub(super) fn new(
        storage: &Storage,
        txid: u32,
        statement: &'a Select<'a>,
        items: &'a [SelectItem<'a>],
        scope: &QueryScope<'a>,
        columns: &CatalogScopeCols<'_, 'a, '_, '_>,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
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
        let n_order = statement.order_by.len();
        let keys = arena
            .alloc_slice_with(n_order + statement.distinct_on.len(), |_| SortKey {
                expression: &Expr::Null,
                descending: false,
                nulls_first: false,
                collation: Collation::None,
            })
            .map_err(|_| arena_full())?;
        // DISTINCT ON expressions follow ORDER BY ascending, as the general
        // executor sorts them.
        let orders = statement
            .order_by
            .iter()
            .map(|order| {
                (
                    order.expression,
                    order.descending,
                    order.nulls_first,
                    "ORDER BY",
                )
            })
            .chain(
                statement
                    .distinct_on
                    .iter()
                    .map(|expression| (*expression, false, false, "DISTINCT ON")),
            );
        let mut targets = [&Expr::Null; MAX_PROJ];
        if keys.len() > targets.len() {
            return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "too many columns"));
        }
        for ((key, target), (original, descending, nulls_first, clause)) in
            keys.iter_mut().zip(&mut targets).zip(orders)
        {
            let expression = crate::sql::query::resolve_position_target(
                original,
                statement.items,
                Some(scope),
                arena,
                storage,
                txid,
                clause,
            )?;
            *target = expression;
            *key = SortKey {
                expression: bind_expression(expression, arena, Some(columns), None)?,
                descending,
                nulls_first,
                collation: scope.expression_collation(storage, arena, expression)?,
            };
        }
        super::materialize::check_distinct_on_order(
            &targets[..n_order],
            &targets[n_order..keys.len()],
            statement.distinct_on,
        )?;
        let places = arena
            .alloc_slice_with(items.len(), |_| ItemPlace::Postponed)
            .map_err(|_| arena_full())?;
        let mut saved_values = keys.len();
        let mut keeps_inputs = false;
        for ((place, item), original) in places.iter_mut().zip(items).zip(statement.items) {
            if let SelectItem::Expr { expression, .. } = original
                && volatile(expression)?
            {
                keeps_inputs = true;
                continue;
            }
            let width = item_width(item, scope);
            *place = ItemPlace::Saved {
                start: saved_values,
                width,
            };
            saved_values += width;
        }
        let pool = storage.plan_build();
        Ok(Self {
            keys,
            unique: statement.distinct_on.len(),
            items: places,
            saved_values,
            keeps_inputs,
            entries: pool.lease(0).map_err(sort_full)?,
            bytes: pool.lease(0).map_err(sort_full)?,
            filled: false,
            next: 0,
            current: None,
        })
    }

    pub(super) fn filled(&self) -> bool {
        self.filled
    }

    pub(super) fn keeps_inputs(&self) -> bool {
        self.keeps_inputs
    }

    /// Values item `index` saves before the sort.
    pub(super) fn saved_width(&self, index: usize) -> usize {
        match self.items[index] {
            ItemPlace::Saved { width, .. } => width,
            ItemPlace::Postponed => 0,
        }
    }

    /// Whether item `index` runs after the sort.
    pub(super) fn postponed(&self, index: usize) -> bool {
        matches!(self.items[index], ItemPlace::Postponed)
    }

    fn entry(&self, index: usize) -> &[u8] {
        let (start, length) = self.entries.as_slice()[index];
        &self.bytes.as_slice()[start as usize..(start + length) as usize]
    }

    fn current(&self) -> Result<&[u8], SqlError> {
        let index = self.current.ok_or_else(|| {
            sql_err!(sqlstate::INTERNAL_ERROR, "retained sort has no current row")
        })?;
        Ok(self.entry(index))
    }

    /// The key expressions, evaluated before the sort in key order.
    pub(super) fn key_expressions(&self) -> impl Iterator<Item = &'a Expr<'a>> + '_ {
        self.keys.iter().map(|key| key.expression)
    }

    /// Appends one input row, encoded as its keys, saved item values and,
    /// when items run after the sort, its input columns.
    pub(super) fn push_encoded(&mut self, row: &[u8]) -> Result<(), SqlError> {
        let start = self.bytes.len();
        let end = start + row.len();
        let (Ok(start32), Ok(length32), Ok(_)) = (
            u32::try_from(start),
            u32::try_from(row.len()),
            u32::try_from(end),
        ) else {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "retained sort exceeds 4 GiB of rows"
            )
            .plan_build_exhausted());
        };
        if end > self.bytes.capacity_entries() {
            // Grow geometrically: each move copies the rows saved so far.
            let grown = self
                .bytes
                .capacity_entries()
                .saturating_mul(2)
                .min(self.bytes.max_entries())
                .max(end);
            self.bytes.reserve_exact(grown).map_err(sort_full)?;
        }
        self.bytes.extend_from_slice(row).map_err(sort_full)?;
        self.entries.push((start32, length32)).map_err(sort_full)
    }

    /// Orders the saved rows; equal keys keep input order.
    pub(super) fn finish_fill(&mut self, storage: &Storage, txid: u32) -> Result<(), SqlError> {
        let keys = self.keys;
        let bytes = self.bytes.as_slice();
        crate::util::try_sort_unstable_by(self.entries.as_mut_slice(), |left, right| {
            let left_row = &bytes[left.0 as usize..(left.0 + left.1) as usize];
            let right_row = &bytes[right.0 as usize..(right.0 + right.1) as usize];
            for (column, key) in keys.iter().enumerate() {
                let left_value = crate::sql::exec::ProjectedValue::column(left_row, column);
                let right_value = crate::sql::exec::ProjectedValue::column(right_row, column);
                let ordering = match (left_value.is_null(), right_value.is_null()) {
                    (true, true) => core::cmp::Ordering::Equal,
                    (true, false) if key.nulls_first => core::cmp::Ordering::Less,
                    (true, false) => core::cmp::Ordering::Greater,
                    (false, true) if key.nulls_first => core::cmp::Ordering::Greater,
                    (false, true) => core::cmp::Ordering::Less,
                    (false, false) => {
                        let compared = super::materialize::compare_materialized_key(
                            storage,
                            txid,
                            key.collation,
                            left_value,
                            right_value,
                        )?;
                        if key.descending {
                            compared.reverse()
                        } else {
                            compared
                        }
                    }
                };
                if !ordering.is_eq() {
                    return Ok(ordering);
                }
            }
            Ok(left.0.cmp(&right.0))
        })?;
        self.filled = true;
        self.next = 0;
        Ok(())
    }

    /// Makes the next sorted row current; `false` at the end. Under
    /// `DISTINCT ON`, a row whose expressions equal its predecessor's is
    /// skipped.
    pub(super) fn advance(&mut self, storage: &Storage, txid: u32) -> Result<bool, SqlError> {
        while self.next < self.entries.len() {
            let index = self.next;
            self.next += 1;
            if index > 0 && self.unique > 0 && self.same_distinct(storage, txid, index)? {
                continue;
            }
            self.current = Some(index);
            return Ok(true);
        }
        self.current = None;
        Ok(false)
    }

    /// Whether sorted row `index` repeats its predecessor's `DISTINCT ON`
    /// values; NULLs are not distinct from each other.
    fn same_distinct(&self, storage: &Storage, txid: u32, index: usize) -> Result<bool, SqlError> {
        let (previous, row) = (self.entry(index - 1), self.entry(index));
        for column in self.keys.len() - self.unique..self.keys.len() {
            let left = crate::sql::exec::ProjectedValue::column(previous, column);
            let right = crate::sql::exec::ProjectedValue::column(row, column);
            let equal = match (left.is_null(), right.is_null()) {
                (true, true) => true,
                (false, false) => super::materialize::compare_materialized_key(
                    storage,
                    txid,
                    self.keys[column].collation,
                    left,
                    right,
                )?
                .is_eq(),
                _ => false,
            };
            if !equal {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn finish_row(&mut self) {
        self.current = None;
    }

    pub(super) fn has_current(&self) -> bool {
        self.current.is_some()
    }

    /// Returns to the first sorted row; the saved rows stay.
    pub(super) fn rewind(&mut self) {
        self.next = 0;
        self.current = None;
    }

    /// The current row's saved values of item `index`, detached into
    /// `frame`.
    pub(super) fn saved_item<'frame>(
        &self,
        index: usize,
        frame: &'frame Arena,
    ) -> Result<&'frame [Datum<'frame>], SqlError> {
        let ItemPlace::Saved { start, width } = self.items[index] else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "retained sort item is not saved"
            ));
        };
        let row = self.current()?;
        let values = frame
            .alloc_slice_with(width, |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (offset, value) in values.iter_mut().enumerate() {
            *value = crate::sql::exec::detach_routine_datum(
                crate::sql::exec::decode_projected_col_record(row, start + offset, frame)?,
                frame,
            )?;
        }
        Ok(values)
    }

    /// The current row's input columns as a joined row, for postponed items.
    pub(super) fn with_input_row<'work, R>(
        &'work self,
        scope: &'work QueryScope<'a>,
        work: &'work Arena,
        each: impl FnOnce(&scan::JoinRow<'_, 'work, '_>) -> Result<R, SqlError>,
    ) -> Result<R, SqlError> {
        let row = self.current()?;
        let bound = work
            .alloc_slice_with(scope.n, |_| None::<scan::BoundRow<'work>>)
            .map_err(|_| arena_full())?;
        let order = work
            .alloc_slice_with(scope.n, |index| index)
            .map_err(|_| arena_full())?;
        let rowids = work
            .alloc_slice_with(scope.n, |_| None::<u64>)
            .map_err(|_| arena_full())?;
        let mut column = self.saved_values;
        for (table, target) in bound.iter_mut().enumerate() {
            let width = scope.defs.get(table).expect("resolved").n_columns;
            let present = matches!(
                crate::sql::exec::decode_projected_pub(row, column),
                Datum::Bool(true)
            );
            column += 1;
            if present {
                let values = work
                    .alloc_slice_with(scope.row_width(table), |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                for (offset, value) in values[..width].iter_mut().enumerate() {
                    *value =
                        crate::sql::exec::decode_projected_col_record(row, column + offset, work)?;
                }
                *target = Some(scan::BoundRow::Values(values));
            }
            column += width;
        }
        each(&scan::JoinRow {
            scope,
            values: scan::JoinRowValues::Bound {
                bound,
                order,
                count: scope.n,
                buffers: &[],
                buffer_offsets: &[],
            },
            rowids,
        })
    }
}
