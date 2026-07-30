//! Materializing a SELECT's rows before they can be returned.
//!
//! Streaming a row straight to the wire is only possible when nothing has to
//! see the whole result first. `GROUP BY`, `DISTINCT` and `ORDER BY` all do, so
//! their rows are projected into the statement arena as self-describing byte
//! strings and sorted or deduplicated there. ORDER BY keys ride along as hidden
//! columns after the visible ones, which is how an arbitrary key expression can
//! order a result; an expensive select-list expression can also be postponed
//! past the sort, so it is evaluated only for the rows that survive a LIMIT.

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::sql::ast::{Expr, FromClause, Select, SelectItem};
use crate::sql::eval::{
    compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError, SubqueryValues,
};
use crate::sql::exec::MAX_PROJ;
use crate::sql::types::{ColType, Datum};
use crate::{sql_err, stack_format};
use crate::storage::Storage;

use super::{
    arena_full, correlated_in_expression, correlated_where_passes, find_srf, merge_correlated,
    postpone_cost, project_row_skipping, record_star_width, resolve_order_target, scan_source,
    sql_fail, sql_ok, srf_max_count, JoinRow, Outcome, QueryScope, ResolvedColumn,
    MAX_SUBQUERIES,
};

/// A flat decoded source row (every column of every scope table, in scope
/// order) resolvable by name, for evaluating postponed projection items after
/// the sort.
struct EncodedRawRow<'s, 'd, 'a> {
    scope: &'s QueryScope<'d>,
    bytes: &'a [u8],
    raw_at: usize,
    arena: &'a Arena,
}

impl<'a> ColumnLookup<'a> for EncodedRawRow<'_, '_, 'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        let flat_of = |t: usize, c: usize| -> usize {
            (0..t).map(|i| self.scope.defs[i].expect("resolved").n_columns).sum::<usize>() + c
        };
        match self.scope.find_column(qualifier, name)? {
            ResolvedColumn::Table(t, c) => crate::sql::exec::decode_projected_col_record(
                self.bytes,
                self.raw_at + flat_of(t, c),
                self.arena,
            ),
            // Merged USING/NATURAL column: the first non-null contributor.
            ResolvedColumn::Merged(m) => {
                let mc = &self.scope.merged[m];
                for &(t, c) in &mc.parts[..mc.n_parts] {
                    let v = crate::sql::exec::decode_projected_col_record(
                        self.bytes,
                        self.raw_at + flat_of(t, c),
                        self.arena,
                    )?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Datum::Null)
            }
        }
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        let entry = self.scope.find_column(qualifier, name).ok()?;
        Some(self.scope.output_type(entry))
    }
}

/// Runs the per-source-row part shared by materialization's sizing and
/// encoding passes. Both passes must evaluate predicates, correlated
/// subqueries, SRFs, projection, and sort keys in exactly the same order.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn for_each_materialized_projection<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    statement: &'a Select<'a>,
    row: &JoinRow<'_, 'a, '_>,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    where_correlated: &[&'a Expr<'a>],
    order_exprs: &[Option<&'a Expr<'a>>; MAX_PROJ],
    n_keys: usize,
    postponed: Option<&[bool; MAX_PROJ]>,
    has_srf: bool,
    consume: &mut impl FnMut(
        &JoinRow<'_, 'a, '_>,
        &[Datum<'a>],
        &[Datum<'a>],
    ) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if !where_correlated.is_empty()
        && !correlated_where_passes(
            where_correlated,
            base,
            statement.where_clause,
            row,
            storage,
            txid,
            arena,
            params,
            hooks,
        )?
    {
        return Ok(());
    }
    let mut scalars: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
    let mut lists: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
    let row_subqueries;
    let owned_hooks;
    let row_hooks: &EvalHooks = if correlated.is_empty() {
        hooks
    } else {
        row_subqueries = merge_correlated(
            correlated,
            base,
            row,
            storage,
            txid,
            arena,
            params,
            &mut scalars,
            &mut lists,
        )?;
        owned_hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: Some(&row_subqueries),
            windows: None,
            catalog: hooks.catalog,
            srf_index: None,
            sequences: hooks.sequences,
        };
        &owned_hooks
    };
    let expansions = srf_max_count(statement.items, arena, params, row, row_hooks)?;
    for expansion in 1..=expansions {
        let srf_hooks;
        let use_hooks: &EvalHooks = if has_srf {
            srf_hooks = EvalHooks { srf_index: Some(expansion), ..*row_hooks };
            &srf_hooks
        } else {
            row_hooks
        };
        let mut projected = [Datum::Null; MAX_PROJ];
        let width = project_row_skipping(
            statement.items,
            postponed,
            scope,
            row,
            arena,
            params,
            use_hooks,
            &mut projected,
            None,
        )?;
        let mut keys = [Datum::Null; MAX_PROJ];
        for (key, expression) in order_exprs.iter().take(n_keys).enumerate() {
            keys[key] =
                eval_full(expression.expect("resolved"), arena, params, row, use_hooks)?;
        }
        consume(row, &projected[..width], &keys[..n_keys])?;
    }
    Ok(())
}

/// A schema-only lookup for aggregate and grouped projections: it exposes the
/// scope's column *types* — so static type inference (e.g. `pg_typeof`) still
/// names a column's type when an aggregate over an empty or all-NULL group
/// yields NULL — while resolving no values. Value lookups fail exactly as an
/// empty row's would, so grouped columns still resolve through the group hook
/// and bare ungrouped references remain the same error.
pub(crate) struct ScopeSchema<'s, 'd>(pub(crate) &'s QueryScope<'d>);
impl<'a> ColumnLookup<'a> for ScopeSchema<'_, '_> {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name))
    }
    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        let entry = self.0.find_column(qualifier, name).ok()?;
        Some(self.0.output_type(entry))
    }
    fn column_domain(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::SqlName> {
        match self.0.find_column(qualifier, name).ok()? {
            super::scope::ResolvedColumn::Table(table, column) => {
                self.0.defs[table]?.columns().get(column)?.domain
            }
            super::scope::ResolvedColumn::Merged(_) => None,
        }
    }
    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        match self.0.find_column(qualifier, name).ok()? {
            super::scope::ResolvedColumn::Table(t, c) => Some((t as u32, c as u32)),
            super::scope::ResolvedColumn::Merged(m) => Some((u32::MAX, m as u32)),
        }
    }
}

/// Which projection items an ORDER BY + LIMIT query defers until after the
/// sort: `postponed` flags the deferred items, and the raw source columns are
/// appended to each encoded row starting at `raw_at`.
pub(crate) struct PostponedProjection {
    postponed: [bool; MAX_PROJ],
    raw_at: usize,
}

/// Fills `out` with an encoded row's visible columns, evaluating any postponed
/// projection items from the row's appended raw source columns. Only rows that
/// survive LIMIT/OFFSET reach this, in sorted order — the whole point of the
/// postponement.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn finalize_projected_row<'a>(
    bytes: &'a [u8],
    width: usize,
    deferred: Option<&PostponedProjection>,
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
) -> Result<(), SqlError> {
    for (i, slot) in out.iter_mut().take(width).enumerate() {
        *slot = crate::sql::exec::decode_projected_col_record(bytes, i, arena)?;
    }
    let Some(d) = deferred else { return Ok(()) };
    let raw_row = EncodedRawRow {
        scope,
        bytes,
        raw_at: d.raw_at,
        arena,
    };
    // `postponed` is indexed by item; wildcards (never postponed) advance the
    // output slot by their column count.
    let mut slot = 0usize;
    for (i, item) in statement.items.iter().enumerate() {
        match item {
            SelectItem::Wildcard => slot += scope.star_columns(),
            SelectItem::TableWildcard(q) => {
                slot += scope.defs[scope.table_index(q)?].expect("resolved").n_columns;
            }
            SelectItem::RecordStar(base) => slot += record_star_width(base, scope),
            SelectItem::Expr { expression, .. } => {
                if d.postponed[i] {
                    out[slot] = eval_full(expression, arena, params, &raw_row, hooks)?;
                }
                slot += 1;
            }
        }
    }
    Ok(())
}

/// Materialized rows, their visible width, and any postponed-projection plan.
type MaterializedSelect<'a> = (&'a [&'a [u8]], usize, Option<PostponedProjection>);

/// The row-producing half of DISTINCT / ORDER BY execution: materialize
/// projected rows (with hidden ORDER BY key columns), dedupe on the visible
/// prefix for DISTINCT, and sort by the hidden keys. Returns `(rows, width)`;
/// the caller pages with LIMIT/OFFSET and emits. Shared by the wire path
/// (`materialized_select`) and the row-source path (`select_into_rows`).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn materialized_rows<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<MaterializedSelect<'a>, SqlError> {
    let n_order = statement.order_by.len();
    // `DISTINCT ON (exprs)`: its keys are materialized as hidden columns
    // after the ORDER BY keys, and the result is deduped on them (keeping the
    // first row per key, in ORDER BY order — PostgreSQL requires the ON
    // expressions to match the leftmost ORDER BY).
    let n_on = statement.distinct_on.len();
    let mut where_correlated = [&Expr::Null; MAX_SUBQUERIES];
    let n_where_correlated = correlated_in_expression(
        statement.where_clause,
        correlated,
        &mut where_correlated,
    )?;
    // Select-list and ORDER BY subqueries do not prevent predicate pushdown.
    // Only a correlated subquery actually used by WHERE needs per-row hooks.
    let where_in_scan =
        if n_where_correlated == 0 { statement.where_clause } else { None };

    // Resolve ORDER BY ordinals to item expressions, then the DISTINCT ON keys.
    let mut order_exprs: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    for (k, ob) in statement.order_by.iter().enumerate() {
        order_exprs[k] = Some(resolve_order_target(ob.expression, statement.items, scope, arena)?);
    }
    for (j, on) in statement.distinct_on.iter().enumerate() {
        let resolved = resolve_order_target(on, statement.items, scope, arena)?;
        // PostgreSQL requires each DISTINCT ON expression to match the
        // ORDER BY expression at the same leftmost position (when ORDER BY is
        // present); otherwise the "first" row per key is ill-defined.
        if n_order > 0 && (j >= n_order || *order_exprs[j].expect("resolved") != *resolved) {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "SELECT DISTINCT ON expressions must match initial ORDER BY expressions"
            ));
        }
        order_exprs[n_order + j] = Some(resolved);
    }
    let n_keys = n_order + n_on;
    // DISTINCT (not DISTINCT ON) restriction: ORDER BY keys must be members.
    if statement.distinct && n_on == 0 {
        for oe in order_exprs.iter().take(n_order) {
            let target = oe.expect("resolved");
            let in_list = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *target)
            });
            if !in_list {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
    }

    // Visible width.
    let width = {
        let mut w = 0usize;
        for item in statement.items {
            w += match item {
                SelectItem::Wildcard => scope.star_columns(),
                SelectItem::TableWildcard(q) => {
                    scope.defs[scope.table_index(q)?].expect("resolved").n_columns
                }
                SelectItem::RecordStar(base) => record_star_width(base, scope),
                SelectItem::Expr { .. } => 1,
            };
        }
        w
    };

    // Projection postponement (PostgreSQL `make_sort_input_target`, behavior
    // pinned empirically against 18.4): with ORDER BY *and* LIMIT (OFFSET alone
    // does not trigger it), a select-list item costing more than 10 operators
    // is evaluated above the Sort + Limit — only on surviving rows, in sorted
    // order — unless the ORDER BY references it. Wildcards are plain columns
    // and never postponed. Each row then also carries its raw source columns
    // so the deferred items can be evaluated later.
    // A set-returning function in the list expands each source row into
    // several output rows here.
    let srf_call = find_srf(statement.items);
    let defer_allowed = n_order > 0
        && statement.limit.is_some()
        && !statement.distinct
        && correlated.is_empty()
        && srf_call.is_none()
        && statement.items.len() <= MAX_PROJ;
    let mut postponed = [false; MAX_PROJ];
    let mut any_postponed = false;
    if defer_allowed {
        for (i, item) in statement.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else { continue };
            let ordered_by = order_exprs[..n_order]
                .iter()
                .any(|oe| oe.is_some_and(|o| *o == **expression));
            if !ordered_by && postpone_cost(expression, scope, arena) > 20 {
                postponed[i] = true;
                any_postponed = true;
            }
        }
    }
    let n_raw = if any_postponed { scope.total_columns() } else { 0 };

    // Pass 1: count — and evaluate the projection and ORDER BY keys per row
    // (discarding the values). PostgreSQL scans, filters, and projects in a
    // single per-row pass below the Sort, so an early row's projection error
    // surfaces before a later row's WHERE error. We materialize in two passes
    // for a fixed-size allocation, so the count pass must reproduce that error
    // timing rather than evaluate every WHERE before any projection. Postponed
    // items are exactly the ones PostgreSQL does not evaluate below the Sort,
    // so they are skipped here too.
    let mut count = 0usize;
    scan_source(
        storage, scope, from, txid, where_in_scan, arena, params, hooks,
        outer,
        &mut |row| {
            for_each_materialized_projection(
                storage,
                scope,
                statement,
                row,
                txid,
                arena,
                params,
                hooks,
                correlated,
                base,
                &where_correlated[..n_where_correlated],
                &order_exprs,
                n_keys,
                if any_postponed { Some(&postponed) } else { None },
                srf_call.is_some(),
                &mut |_, _, _| {
                    count += 1;
                    Ok(())
                },
            )?;
            Ok(true)
        },
    )?;
    let empty: &[u8] = &[];
    let rows: &mut [&[u8]] = arena
        .alloc_slice_with(count, |_| empty)
        .map_err(|_| arena_full())?;
    // Pass 2: project + keys, encode.
    {
        let mut at = 0usize;
        scan_source(
            storage, scope, from, txid, where_in_scan, arena, params, hooks,
            outer,
            &mut |row| {
                for_each_materialized_projection(
                    storage,
                    scope,
                    statement,
                    row,
                    txid,
                    arena,
                    params,
                    hooks,
                    correlated,
                    base,
                    &where_correlated[..n_where_correlated],
                    &order_exprs,
                    n_keys,
                    if any_postponed { Some(&postponed) } else { None },
                    srf_call.is_some(),
                    &mut |row, projected, keys| {
                    debug_assert_eq!(projected.len(), width);
                    rows[at] = crate::sql::exec::encode_projected_by(
                        width + n_keys + n_raw,
                        |index| {
                            if index < width {
                                return projected[index];
                            }
                            if index < width + n_keys {
                                return keys[index - width];
                            }
                            let mut raw_index = index - width - n_keys;
                            for table in 0..scope.n {
                                let column_count =
                                    scope.defs[table].expect("resolved").n_columns;
                                if raw_index < column_count {
                                    let values = row.values[table].expect("bound");
                                    return if values.is_empty() {
                                        Datum::Null
                                    } else {
                                        values[raw_index]
                                    };
                                }
                                raw_index -= column_count;
                            }
                            unreachable!("raw projected column is within scope width")
                        },
                        arena,
                    )?;
                    at += 1;
                    Ok(())
                },
                )?;
                Ok(true)
            },
        )?;
    }

    // Plain DISTINCT dedups on the visible prefix before the ORDER BY sort.
    // DISTINCT ON dedups on its keys *after* the sort (below), so the first
    // row per key in ORDER BY order survives.
    let mut live = rows.len();
    if statement.distinct && n_on == 0 {
        live = crate::sql::exec::sort_dedup_projected(rows, width);
    }
    let rows = &mut rows[..live];

    // Sort by ORDER BY (a stable sort so DISTINCT ON without ORDER BY keeps
    // the first-scanned row per key). For DISTINCT ON the ON keys are appended
    // ascending as a tiebreak — a no-op when ORDER BY already begins with them
    // (as PostgreSQL requires), but it groups equal keys when ORDER BY is
    // absent so the run dedup below works.
    if n_order > 0 || n_on > 0 {
        crate::mem::arena::stable_sort_via(arena, rows, |a, b| {
            for (k, ob) in statement.order_by.iter().enumerate() {
                let ka = crate::sql::exec::decode_projected_pub(a, width + k);
                let kb = crate::sql::exec::decode_projected_pub(b, width + k);
                // NULL placement follows NULLS FIRST/LAST (absolute, not
                // affected by ASC/DESC); only the value comparison reverses
                // for DESC.
                let ord = match (ka.is_null(), kb.is_null()) {
                    (true, true) => core::cmp::Ordering::Equal,
                    (true, false) => {
                        if ob.nulls_first { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater }
                    }
                    (false, true) => {
                        if ob.nulls_first { core::cmp::Ordering::Greater } else { core::cmp::Ordering::Less }
                    }
                    (false, false) => {
                        let c = compare_datums(&ka, &kb).unwrap_or(core::cmp::Ordering::Equal);
                        if ob.descending { c.reverse() } else { c }
                    }
                };
                if !ord.is_eq() {
                    return ord;
                }
            }
            for j in 0..n_on {
                let ka = crate::sql::exec::decode_projected_pub(a, width + n_order + j);
                let kb = crate::sql::exec::decode_projected_pub(b, width + n_order + j);
                let ord = match (ka.is_null(), kb.is_null()) {
                    (true, true) => core::cmp::Ordering::Equal,
                    (true, false) => core::cmp::Ordering::Greater,
                    (false, true) => core::cmp::Ordering::Less,
                    (false, false) => {
                        compare_datums(&ka, &kb).unwrap_or(core::cmp::Ordering::Equal)
                    }
                };
                if !ord.is_eq() {
                    return ord;
                }
            }
            core::cmp::Ordering::Equal
        })
        .map_err(|_| arena_full())?;
    }

    // DISTINCT ON: keep the first row of each run of equal ON keys.
    let rows: &mut [&[u8]] = if n_on > 0 {
        let mut unique = 0usize;
        for i in 0..rows.len() {
            let same = i > 0
                && (0..n_on).all(|j| {
                    let ka = crate::sql::exec::decode_projected_pub(rows[i], width + n_order + j);
                    let kb = crate::sql::exec::decode_projected_pub(rows[i - 1], width + n_order + j);
                    match (ka.is_null(), kb.is_null()) {
                        (true, true) => true,
                        (true, false) | (false, true) => false,
                        (false, false) => {
                            compare_datums(&ka, &kb).map(|o| o.is_eq()).unwrap_or(false)
                        }
                    }
                });
            if !same {
                rows[unique] = rows[i];
                unique += 1;
            }
        }
        &mut rows[..unique]
    } else {
        rows
    };

    let deferred = any_postponed.then_some(PostponedProjection {
        postponed,
        raw_at: width + n_keys,
    });
    Ok((rows, width, deferred))
}

/// Whether two sorted rows tie on their `n_order` hidden ORDER BY key columns
/// (stored after `width`), with NULLs comparing equal — the `WITH TIES` peer
/// test, independent of ASC/DESC.
pub(crate) fn order_keys_equal(a: &[u8], b: &[u8], width: usize, n_order: usize) -> bool {
    for k in 0..n_order {
        let ka = crate::sql::exec::decode_projected_pub(a, width + k);
        let kb = crate::sql::exec::decode_projected_pub(b, width + k);
        let equal = match (ka.is_null(), kb.is_null()) {
            (true, true) => true,
            (false, false) => compare_datums(&ka, &kb).is_ok_and(|o| o.is_eq()),
            _ => false,
        };
        if !equal {
            return false;
        }
    }
    true
}

/// Extends an exclusive limit window to include every following row that ties
/// with `rows[window - 1]` on the ORDER BY keys (`FETCH FIRST ... WITH TIES`).
/// A `window` at or past the end (or no ORDER BY) is returned clamped.
pub(crate) fn extend_ties(rows: &[&[u8]], width: usize, n_order: usize, window: usize) -> usize {
    if n_order == 0 || window == 0 || window >= rows.len() {
        return window.min(rows.len());
    }
    let boundary = rows[window - 1];
    let mut end = window;
    while end < rows.len() && order_keys_equal(boundary, rows[end], width, n_order) {
        end += 1;
    }
    end
}

/// DISTINCT / ORDER BY execution to the wire: materialize the rows, then page
/// with LIMIT/OFFSET and emit.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(crate) fn materialized_select<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    let (rows, width, deferred) = match materialized_rows(
        storage, scope, from, txid, statement, arena, params, hooks, correlated, base, None,
    ) {
        Ok(x) => x,
        Err(e) => return sql_fail(e),
    };
    let mut emitted = 0u64;
    // OFFSET rows flow through PostgreSQL's projection before Limit discards
    // them, so deferred items are evaluated for them too (their errors
    // surface); only rows past the offset are emitted.
    let mut window = offset.saturating_add(limit).min(usize::MAX as u64) as usize;
    // FETCH FIRST ... WITH TIES: keep rows past the limit that tie with the
    // last one on the ORDER BY keys (which ride as hidden columns after
    // `width`). Only meaningful when a row was actually emitted (`limit > 0`).
    if statement.with_ties && limit > 0 {
        window = extend_ties(rows, width, statement.order_by.len(), window);
    }
    for (index, row) in rows.iter().take(window).enumerate() {
        let mut out = [Datum::Null; MAX_PROJ];
        if let Err(e) = finalize_projected_row(
            row, width, deferred.as_ref(), statement, scope, arena, params, hooks, &mut out,
        ) {
            return sql_fail(e);
        }
        if (index as u64) >= offset {
            responder.data_row(&out[..width])?;
            emitted += 1;
        }
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}
