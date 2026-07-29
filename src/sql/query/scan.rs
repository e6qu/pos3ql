//! Enumerating a query's source rows.
//!
//! [`scan_source`] walks the FROM clause as a nested loop — visibility-filtered
//! base rows, then each join in turn, a LEFT/RIGHT/FULL side emitting a null
//! row where it finds no match — applying ON conditions and the WHERE clause as
//! it goes, and calling back once per surviving row. [`JoinRow`] is what the
//! callback receives: the columns of every table bound so far, resolved by
//! name. [`Chained`] layers an enclosing query's row behind it, which is how a
//! correlated subquery sees the row it is correlated with.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, FromClause, JoinKind};
use crate::sql::eval::{ColumnLookup, EvalHooks, SqlError, eval_full, sqlstate};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{MAX_COLUMNS, Storage, rowenc};

use super::plan::{
    MAX_CONJUNCTS, conjunct_passes, expr_tables, flatten_and, fold_null, is_error_safe,
};
use super::{
    MAX_JOIN_TABLES, QueryScope, ResolvedColumn, arena_full, check_timeout, join_order,
    reorder_qual, simplify_qual, where_passes,
};

/// One assembled source row: per table, decoded values (empty slice =
/// LEFT-join null row; None = not yet joined).
pub struct JoinRow<'s, 'v, 'd> {
    pub scope: &'s QueryScope<'d>,
    pub values: [Option<&'s [Datum<'v>]>; MAX_JOIN_TABLES],
}

impl<'v> ColumnLookup<'v> for JoinRow<'_, 'v, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        let one = |t: usize, c: usize| match self.values[t] {
            // Empty slice = LEFT-join null row.
            Some([]) => Ok(Datum::Null),
            Some(vals) => Ok(vals[c]),
            None => Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "column \"{}\" referenced before its table is joined",
                name
            )),
        };
        match self.scope.find_column(qualifier, name)? {
            ResolvedColumn::Table(t, c) => one(t, c),
            // Merged USING/NATURAL column: the first non-null contributor.
            ResolvedColumn::Merged(m) => {
                let mc = &self.scope.merged[m];
                for &(t, c) in &mc.parts[..mc.n_parts] {
                    let v = one(t, c)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Datum::Null)
            }
        }
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<crate::sql::types::ColType> {
        let entry = self.scope.find_column(qualifier, name).ok()?;
        Some(self.scope.output_type(entry))
    }

    fn column_domain(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::SqlName> {
        match self.scope.find_column(qualifier, name).ok()? {
            ResolvedColumn::Table(t, c) => {
                self.scope.defs[t].and_then(|def| def.columns().get(c).and_then(|col| col.domain))
            }
            // A USING/NATURAL-merged column carries no single domain identity.
            ResolvedColumn::Merged(_) => None,
        }
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.scope.func_scalar_type(table).is_some()
    }

    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        let t = self.scope.table_index(table)?;
        match self.values[t] {
            Some([]) => Ok(false), // outer-join null row
            Some(_) => Ok(true),
            None => Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "whole-row reference to \"{}\" before its table is joined",
                table
            )),
        }
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'v Arena,
    ) -> Result<Option<&'v [crate::sql::types::RecordField<'v>]>, SqlError> {
        let t = self.scope.table_index(table)?;
        let def = self.scope.defs[t].expect("resolved");
        let vals = match self.values[t] {
            Some([]) => return Ok(None), // outer-join null row
            Some(vals) => vals,
            None => {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "whole-row reference to \"{}\" before its table is joined",
                    table
                ));
            }
        };
        // Copy field names into the arena so the record does not borrow the
        // catalog (its lifetime is unrelated to the row's `'v`).
        let cols = def.columns();
        let mut fields = [crate::sql::types::RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        }; MAX_COLUMNS];
        for (i, field) in fields.iter_mut().enumerate().take(def.n_columns) {
            let name = arena
                .alloc_str(cols[i].name.as_str())
                .map_err(|_| arena_full())?;
            field.name = name;
            field.type_oid = cols[i].ctype.oid();
            field.value = vals.get(i).copied().unwrap_or(Datum::Null);
        }
        let out = arena
            .alloc_slice_copy(&fields[..def.n_columns])
            .map_err(|_| arena_full())?;
        Ok(Some(&*out))
    }
}

/// Chains an inner row's column resolution to an optional outer row (for
/// correlated subqueries): a name unresolved inside the subquery falls back
/// to the enclosing query's row.
pub(crate) struct Chained<'r, 'a> {
    pub(crate) inner: &'r dyn ColumnLookup<'a>,
    pub(crate) outer: Option<&'r dyn ColumnLookup<'a>>,
}
impl<'a> ColumnLookup<'a> for Chained<'_, 'a> {
    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        match self.inner.whole_row_present(table) {
            Ok(v) => Ok(v),
            Err(e) => match self.outer {
                Some(o) => o.whole_row_present(table),
                None => Err(e),
            },
        }
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a [crate::sql::types::RecordField<'a>]>, SqlError> {
        match self.inner.whole_row_fields(table, arena) {
            Ok(v) => Ok(v),
            Err(e) => match self.outer {
                Some(o) => o.whole_row_fields(table, arena),
                None => Err(e),
            },
        }
    }

    fn lookup(&self, q: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        match self.inner.lookup(q, name) {
            Ok(v) => Ok(v),
            Err(e) => match self.outer {
                Some(o) => o.lookup(q, name),
                None => Err(e),
            },
        }
    }
    fn col_type(&self, q: Option<&str>, name: &str) -> Option<crate::sql::types::ColType> {
        self.inner
            .col_type(q, name)
            .or_else(|| self.outer.and_then(|o| o.col_type(q, name)))
    }
    fn column_domain(&self, q: Option<&str>, name: &str) -> Option<crate::storage::SqlName> {
        self.inner
            .column_domain(q, name)
            .or_else(|| self.outer.and_then(|o| o.column_domain(q, name)))
    }

    /// Forwarded like the rest: a wrapper that answered this from the trait
    /// default would report a single-column table function (`FROM
    /// json_array_elements_text(...) AS x`) as a record, so `x` would render
    /// `(p)` instead of `p`.
    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.inner.whole_row_is_scalar(table)
            || self.outer.is_some_and(|o| o.whole_row_is_scalar(table))
    }
}

/// Materializes a `LATERAL` FROM item's rows for one outer row: its body sees
/// `outer` (the tables to its left) as an outer scope. A subquery re-runs
/// through the ordinary executor; a set-returning function evaluates its
/// arguments against the outer row. Rows are projected-encoded into `arena`.
fn materialize_lateral<'a, C: ColumnLookup<'a>>(
    storage: &'a Storage,
    txid: u32,
    tref: &'a crate::sql::ast::TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: &C,
) -> Result<&'a [&'a [u8]], SqlError> {
    if let Some(sub) = tref.subquery {
        const EMPTY: &[u8] = &[];
        let mut store: *mut &[u8] = core::ptr::null_mut();
        let mut len = 0usize;
        let mut cap = 0usize;
        super::select_into_rows(
            storage,
            txid,
            sub,
            arena,
            params,
            Some(outer),
            None,
            &mut |vals| {
                let enc = crate::sql::exec::encode_projected_pub(vals, arena)?;
                if len == cap {
                    let new_cap = if cap == 0 { 8 } else { cap * 2 };
                    let fresh: &mut [&[u8]] = arena
                        .alloc_slice_with(new_cap, |_| EMPTY)
                        .map_err(|_| arena_full())?;
                    if len > 0 {
                        let old = unsafe { core::slice::from_raw_parts(store, len) };
                        fresh[..len].copy_from_slice(old);
                    }
                    store = fresh.as_mut_ptr();
                    cap = new_cap;
                }
                unsafe { store.add(len).write(enc) };
                len += 1;
                Ok(())
            },
        )?;
        return Ok(if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(store, len) }
        });
    }
    if tref.func_args.is_some() {
        // A lateral SRF (`LATERAL generate_series(1, t.n)`) evaluates its
        // arguments against the outer row.
        return super::table_func_rows_outer(tref, storage, arena, params, outer);
    }
    Err(sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "LATERAL is supported only for a subquery or a set-returning function"
    ))
}

/// Enumerates source rows (visibility-filtered, ON conditions applied,
/// WHERE applied), calling `f` per row. `f` returns false to stop early.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scan_source<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    // Simplify plan-time-decided boolean arms, fold `col IS [NOT] NULL` on
    // NOT-NULL columns, then order the WHERE conjuncts by PostgreSQL's clause
    // cost once, up front, so the per-row leaf evaluates them cheapest-first
    // without re-sorting.
    let where_clause = match where_clause {
        Some(w) => {
            let simplified = simplify_qual(w, arena)?;
            Some(reorder_qual(
                fold_null(simplified, scope, arena)?,
                scope,
                arena,
            )?)
        }
        None => None,
    };
    // Assemble a JoinRow from the currently bound row bytes. Physical rows
    // are heap-encoded (fixed schema); derived rows are self-describing.
    fn assemble<'s, 'v, 'd>(
        scope: &'s QueryScope<'d>,
        bound: &[Option<&'v [u8]>; MAX_JOIN_TABLES],
        order: &[usize; MAX_JOIN_TABLES],
        count: usize,
        buffers: &'s mut [[Datum<'v>; MAX_COLUMNS]; MAX_JOIN_TABLES],
        arena: &'v Arena,
    ) -> Result<JoinRow<'s, 'v, 'd>, SqlError> {
        let mut values: [Option<&[Datum]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
        // Split buffers so each table borrows a distinct buffer. `order` maps the
        // execution position to the scope-table index, so a reordered join still
        // fills each table's own `values` slot.
        let mut rest: &mut [[Datum<'v>; MAX_COLUMNS]] = buffers;
        for &t in order.iter().take(count) {
            let (buffer, tail) = rest.split_first_mut().expect("enough buffers");
            rest = tail;
            let def = scope.defs[t].expect("resolved");
            match bound[t] {
                Some(bytes) => {
                    if scope.derived[t].is_some() {
                        for (c, slot) in buffer.iter_mut().enumerate().take(def.n_columns) {
                            // Structural decode: a record column comes back
                            // as a `Datum::Record` (fields in the arena), so
                            // field access sees its shape.
                            *slot = crate::sql::exec::decode_projected_col_record(bytes, c, arena)?;
                        }
                    } else {
                        let mut schema = [ColType::Bool; MAX_COLUMNS];
                        def.schema(&mut schema);
                        rowenc::decode(bytes, &schema[..def.n_columns], buffer)?;
                    }
                    values[t] = Some(&buffer[..def.n_columns]);
                }
                None => values[t] = Some(&[]), // outer-join null row
            }
        }
        Ok(JoinRow { scope, values })
    }

    // Per-level decode buffers live on this stack frame.
    #[allow(clippy::too_many_arguments)]
    fn level<'a>(
        storage: &'a Storage,
        scope: &QueryScope<'a>,
        from: &'a FromClause<'a>,
        txid: u32,
        where_clause: Option<&Expr<'a>>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
        depth: usize,
        bound: &mut [Option<&'a [u8]>; MAX_JOIN_TABLES],
        // For each RIGHT/FULL join level, one flag per scanned row of that
        // level's table, marking those that found a left partner.
        matched: &[Option<&[core::cell::Cell<bool>]>; MAX_JOIN_TABLES],
        // Error-safe WHERE conjuncts to check at each depth (predicate pushdown).
        pushdown: &[&[&'a Expr<'a>]],
        // Execution order: `order[depth]` is the scope-table joined at this depth
        // (identity unless a cross join was cost-reordered).
        order: &[usize; MAX_JOIN_TABLES],
        f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        if depth == scope.n {
            let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
            let row = assemble(scope, bound, order, depth, &mut buffers, arena)?;
            if let Some(w) = where_clause {
                let chained_row = Chained { inner: &row, outer };
                if !where_passes(w, arena, params, &chained_row, hooks)? {
                    return Ok(true);
                }
            }
            return f(&row);
        }

        let join = if depth == 0 {
            None
        } else {
            Some(&from.joins[depth - 1])
        };
        let mut matched_any = false;
        // Bind one candidate row for this level, run the ON condition, then
        // recurse. `on_matches` returns false to skip the row.
        let on_matches =
            |bound: &mut [Option<&'a [u8]>; MAX_JOIN_TABLES]| -> Result<bool, SqlError> {
                if let Some(join) = join
                // USING/NATURAL predicates are synthesized at plan time.
                && let Some(on) = join.on.or(scope.join_on[depth - 1])
                {
                    let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
                    let row = assemble(scope, bound, order, depth + 1, &mut buffers, arena)?;
                    let chained_row = Chained { inner: &row, outer };
                    return match eval_full(on, arena, params, &chained_row, hooks)? {
                        Datum::Bool(true) => Ok(true),
                        Datum::Bool(false) | Datum::Null => Ok(false),
                        _ => Err(sql_err!(
                            sqlstate::DATATYPE_MISMATCH,
                            "argument of JOIN/ON must be type boolean"
                        )),
                    };
                }
                Ok(true)
            };
        // Predicate pushdown: skip a partial row that already fails an error-safe
        // WHERE conjunct fully bound at this depth.
        let passes_pushdown =
            |bound: &[Option<&'a [u8]>; MAX_JOIN_TABLES]| -> Result<bool, SqlError> {
                if pushdown[depth].is_empty() {
                    return Ok(true);
                }
                let mut pbuf = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
                let prow = assemble(scope, bound, order, depth + 1, &mut pbuf, arena)?;
                let pcr = Chained {
                    inner: &prow,
                    outer,
                };
                for &c in pushdown[depth] {
                    if !conjunct_passes(c, arena, params, &pcr, hooks)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
        // A LATERAL FROM item is re-run per outer row: assemble the row bound by
        // the tables to its left, resolve the item's body against it, and iterate
        // the resulting rows like a derived table's.
        if scope.lateral[order[depth]] {
            let t = order[depth];
            let tref = if t == 0 {
                &from.base
            } else {
                &from.joins[t - 1].table
            };
            let mut obuf = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
            let outer_row = assemble(scope, bound, order, depth, &mut obuf, arena)?;
            let chained = Chained {
                inner: &outer_row,
                outer,
            };
            let rows = materialize_lateral(storage, txid, tref, arena, params, &chained)?;
            for (index, bytes) in rows.iter().enumerate() {
                check_timeout()?;
                bound[order[depth]] = Some(bytes);
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched[depth] {
                    m[index].set(true);
                }
                if !level(
                    storage,
                    scope,
                    from,
                    txid,
                    where_clause,
                    arena,
                    params,
                    hooks,
                    outer,
                    depth + 1,
                    bound,
                    matched,
                    pushdown,
                    order,
                    f,
                )? {
                    return Ok(false);
                }
            }
        } else if let Some(rows) = scope.derived[order[depth]] {
            for (index, bytes) in rows.iter().enumerate() {
                check_timeout()?;
                bound[order[depth]] = Some(bytes);
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched[depth] {
                    m[index].set(true);
                }
                if !level(
                    storage,
                    scope,
                    from,
                    txid,
                    where_clause,
                    arena,
                    params,
                    hooks,
                    outer,
                    depth + 1,
                    bound,
                    matched,
                    pushdown,
                    order,
                    f,
                )? {
                    return Ok(false);
                }
            }
        } else if depth == 0 {
            // Outermost scan: iterate in heap-offset (insertion) order so a
            // per-row error surfaces on the same row as PostgreSQL, whose heap
            // scan is physical (insertion) order for a freshly-loaded table.
            // The rows live in a hash map (slot order), so snapshot the visible
            // locations into the per-statement arena and sort by offset. Only
            // the outermost scan is ordered — it drives output/error order, and
            // ordering an inner join scan would re-snapshot per outer row.
            let slot = scope.slots[order[depth]];
            let count = storage.visible_row_count(slot, txid)?;
            let ordered = arena
                .alloc_slice_with(count, |_| {
                    (
                        0u64,
                        crate::storage::RowHome::Heap(crate::storage::RowLoc { offset: 0, len: 0 }),
                    )
                })
                .map_err(|_| arena_full())?;
            let mut fill = 0usize;
            storage.for_each_row_state(slot, &mut |rowid, state| {
                if let Some(home) =
                    state.visible_at_lsn(txid, storage.read_snapshot(), storage.commit_snapshot())
                {
                    ordered[fill] = (rowid, home);
                    fill += 1;
                }
                Ok(core::ops::ControlFlow::Continue(()))
            })?;
            debug_assert_eq!(fill, count, "visible count is stable");
            // Spilled rows sort by rowid (their SST order — the physical order
            // they were written in); heap rows keep heap-offset order after
            // them, matching insertion order within each group.
            ordered.sort_unstable_by_key(|(rowid, home)| match home {
                crate::storage::RowHome::Spilled { .. } => (0u8, *rowid, 0u32),
                crate::storage::RowHome::Heap(loc) => (1u8, 0, loc.offset),
            });
            for (this, &(rowid, home)) in ordered.iter().enumerate() {
                check_timeout()?;
                bound[order[depth]] =
                    Some(storage.row_bytes(scope.slots[order[depth]], rowid, home, arena)?);
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched[depth] {
                    m[this].set(true);
                }
                if !level(
                    storage,
                    scope,
                    from,
                    txid,
                    where_clause,
                    arena,
                    params,
                    hooks,
                    outer,
                    depth + 1,
                    bound,
                    matched,
                    pushdown,
                    order,
                    f,
                )? {
                    return Ok(false);
                }
            }
        } else {
            let slot = scope.slots[order[depth]];
            let mut index = 0usize;
            let mut aborted = false;
            storage.for_each_row_state(slot, &mut |rowid, state| {
                use core::ops::ControlFlow;
                check_timeout()?;
                let Some(home) =
                    state.visible_at_lsn(txid, storage.read_snapshot(), storage.commit_snapshot())
                else {
                    return Ok(ControlFlow::Continue(()));
                };
                bound[order[depth]] = Some(storage.row_bytes(slot, rowid, home, arena)?);
                let this = index;
                index += 1;
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    return Ok(ControlFlow::Continue(()));
                }
                matched_any = true;
                if let Some(m) = matched[depth] {
                    m[this].set(true);
                }
                if !level(
                    storage,
                    scope,
                    from,
                    txid,
                    where_clause,
                    arena,
                    params,
                    hooks,
                    outer,
                    depth + 1,
                    bound,
                    matched,
                    pushdown,
                    order,
                    f,
                )? {
                    aborted = true;
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            })?;
            if aborted {
                return Ok(false);
            }
        }
        // LEFT/FULL join with no match at this level: emit one null row (the
        // left side preserved, this table nulled).
        if !matched_any && join.is_some_and(|j| matches!(j.kind, JoinKind::Left | JoinKind::Full)) {
            bound[order[depth]] = None;
            if !level(
                storage,
                scope,
                from,
                txid,
                where_clause,
                arena,
                params,
                hooks,
                outer,
                depth + 1,
                bound,
                matched,
                pushdown,
                order,
                f,
            )? {
                return Ok(false);
            }
        }
        bound[order[depth]] = None;
        Ok(true)
    }

    // For every RIGHT/FULL join level, one match flag per row of that
    // level's table (arena-backed, so no post-init allocation). An unmatched
    // row null-pads the tables to its left and still joins the tables to its
    // right (post-passes below, shallowest level first — so a deeper level's
    // flags also accumulate matches found during a shallower post-pass).
    let mut matched: [Option<&[core::cell::Cell<bool>]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
    for (i, j) in from.joins.iter().enumerate() {
        if !matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            continue;
        }
        let t = i + 1;
        let n_rows = if let Some(rows) = scope.derived[t] {
            rows.len()
        } else {
            storage.visible_row_count(scope.slots[t], txid)?
        };
        let flags = arena
            .alloc_slice_with(n_rows, |_| false)
            .map_err(|_| arena_full())?;
        matched[t] = Some(core::cell::Cell::from_mut(flags).as_slice_of_cells());
    }

    // Predicate pushdown (inner/cross joins only): assign each error-safe WHERE
    // conjunct to the join level at which all its tables are bound, so it can
    // prune the search early instead of being checked only after the full
    // Cartesian product is built. This turns a k-way equi-join from O(N^k)
    // toward the filtered result size. Results are identical — a partial row
    // that fails such a conjunct cannot satisfy the full WHERE (the conjunct's
    // value does not depend on the still-unbound tables), and the leaf still
    // evaluates the whole WHERE. Restricted to inner/cross joins so a
    // WHERE clause over an outer join's nullable side is never pruned early.
    let all_inner = from
        .joins
        .iter()
        .all(|j| matches!(j.kind, JoinKind::Inner | JoinKind::Cross));
    // Cost-based execution order: only cross joins (no ON clause, no nullable
    // side) may be reordered freely — an explicit JOIN ... ON's condition is tied
    // to its position. Everything else keeps FROM order (identity). A LATERAL
    // item depends on the tables to its left, so any lateral entry also pins the
    // order to FROM order (reordering could bind a dependency after it).
    let any_lateral = scope.lateral[..scope.n].iter().any(|&l| l);
    // A LATERAL item references the tables to its left, so it cannot be the
    // right side of a RIGHT/FULL join (PostgreSQL rejects this too).
    for j in from.joins {
        if j.table.lateral && matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "a LATERAL item cannot be on the right side of a RIGHT or FULL join"
            ));
        }
    }
    let all_cross = from.joins.iter().all(|j| matches!(j.kind, JoinKind::Cross));
    let order: [usize; MAX_JOIN_TABLES] = if all_cross && !any_lateral {
        join_order(scope, where_clause)
    } else {
        core::array::from_fn(|i| i)
    };
    let mut inv_order = [0usize; MAX_JOIN_TABLES];
    for (pos, &t) in order.iter().enumerate() {
        inv_order[t] = pos;
    }
    let mut pushdown_buffers: [[&Expr; MAX_CONJUNCTS]; MAX_JOIN_TABLES] =
        [[&Expr::Null; MAX_CONJUNCTS]; MAX_JOIN_TABLES];
    let mut pd_n = [0usize; MAX_JOIN_TABLES];
    if all_inner
        && scope.n >= 2
        && let Some(w) = where_clause
    {
        let mut conjunct: [&Expr; MAX_CONJUNCTS] = [w; MAX_CONJUNCTS];
        let mut n = 0;
        let conjuncts: &[&Expr] = if flatten_and(w, &mut conjunct, &mut n) {
            &conjunct[..n]
        } else {
            core::slice::from_ref(&w)
        };
        for &c in conjuncts {
            // The execution depth at which a conjunct is fully bound is the
            // latest execution position of any table it references (under
            // identity order this is just the max table index it references).
            if is_error_safe(c)
                && let Some(mask) = expr_tables(c, scope)
            {
                let d = (0..scope.n)
                    .filter(|t| mask & (1 << t) != 0)
                    .map(|t| inv_order[t])
                    .max()
                    .unwrap_or(0);
                if d < scope.n && pd_n[d] < MAX_CONJUNCTS {
                    pushdown_buffers[d][pd_n[d]] = c;
                    pd_n[d] += 1;
                }
            }
        }
    }
    let pushdown: [&[&Expr]; MAX_JOIN_TABLES] =
        core::array::from_fn(|d| &pushdown_buffers[d][..pd_n[d]]);

    let mut bound = [None; MAX_JOIN_TABLES];
    level(
        storage,
        scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        hooks,
        outer,
        0,
        &mut bound,
        &matched,
        &pushdown,
        &order,
        f,
    )?;

    // RIGHT/FULL post-passes, shallowest level first: each unmatched row of
    // that level's table binds with every table to its left nulled and then
    // joins the deeper tables normally (so its own matches mark deeper
    // levels' flags before those levels' post-passes run).
    for d in 1..scope.n {
        let Some(m) = matched[d] else { continue };
        let emit_unmatched = |bytes: &'a [u8],
                              f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>|
         -> Result<bool, SqlError> {
            let mut b = [None; MAX_JOIN_TABLES];
            b[d] = Some(bytes);
            if d + 1 == scope.n {
                // Last level: the row is complete once the left side nulls.
                let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
                let row = assemble(scope, &b, &order, scope.n, &mut buffers, arena)?;
                if let Some(w) = where_clause {
                    let chained_row = Chained { inner: &row, outer };
                    if !where_passes(w, arena, params, &chained_row, hooks)? {
                        return Ok(true);
                    }
                }
                return f(&row);
            }
            level(
                storage,
                scope,
                from,
                txid,
                where_clause,
                arena,
                params,
                hooks,
                outer,
                d + 1,
                &mut b,
                &matched,
                &pushdown,
                &order,
                f,
            )
        };
        if let Some(rows) = scope.derived[d] {
            for (index, bytes) in rows.iter().enumerate() {
                if !m[index].get() && !emit_unmatched(bytes, f)? {
                    return Ok(());
                }
            }
        } else {
            let mut index = 0usize;
            let mut done = false;
            storage.for_each_row_state(scope.slots[d], &mut |rowid, state| {
                use core::ops::ControlFlow;
                let Some(home) =
                    state.visible_at_lsn(txid, storage.read_snapshot(), storage.commit_snapshot())
                else {
                    return Ok(ControlFlow::Continue(()));
                };
                let this = index;
                index += 1;
                if !m[this].get()
                    && !emit_unmatched(storage.row_bytes(scope.slots[d], rowid, home, arena)?, f)?
                {
                    done = true;
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            })?;
            if done {
                return Ok(());
            }
        }
    }
    Ok(())
}
