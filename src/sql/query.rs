//! SELECT execution: one pipeline for single tables and joins.
//!
//! Shape: resolve the FROM clause into a scope → enumerate source rows
//! (nested-loop joins, LEFT emitting a null row) → WHERE → then either
//! stream straight to the wire, or materialize projected rows as tagged
//! byte strings in the statement arena for GROUP BY / DISTINCT / ORDER BY.
//! ORDER BY keys ride along as hidden columns after the visible ones, so
//! arbitrary key expressions order both plain and joined queries.
//!
//! Subqueries are uncorrelated and pre-evaluated once per statement; their
//! results are injected into evaluation by node identity (EvalHooks).

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, SqlName, Storage, TableDef, MAX_COLUMNS};
use crate::storage::rowenc;

use super::ast::{
    Expr, FromClause, Join, JoinKind, OrderBy, Select, SelectItem, SetOp, SetQuery, SetTree,
    TableRef,
};
use super::eval::{
    compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError, SubqueryValues,
};
use super::exec::{describe_items, MAX_PROJ};
use super::types::{ColDesc, ColType, Datum};

pub const MAX_JOIN_TABLES: usize = 9; // base + 8 joins
const MAX_AGGS: usize = 16;
const MAX_WINDOWS: usize = 16;
/// Maximum ORDER BY / PARTITION BY keys in one window clause.
const MAX_WIN_KEYS: usize = 8;
const MAX_SUBQUERIES: usize = 8;
const SUBQUERY_DEPTH: u32 = 4;

type Outcome = Result<Result<(), SqlError>, WireFull>;

/// Bridges `EvalHooks`' abstract `CatalogAccess` to the concrete `Storage`, so
/// `pg_get_indexdef` can reconstruct an index's definition during evaluation.
struct StorageCatalog<'s> {
    storage: &'s Storage,
}

impl super::eval::CatalogAccess for StorageCatalog<'_> {
    fn index_def<'a>(
        &self,
        oid: i32,
        col: usize,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::index_def_text(self.storage, oid, col, arena)
    }
    fn constraint_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::constraint_def_text(self.storage, oid, arena)
    }
    fn relname<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::relname_text(self.storage, oid, arena)
    }
}

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

/// The resolved FROM clause: per table, its exposed name (alias or table
/// name), definition, and storage slot.
pub struct QueryScope<'d> {
    pub names: [&'d str; MAX_JOIN_TABLES],
    pub defs: [Option<&'d TableDef>; MAX_JOIN_TABLES],
    pub slots: [usize; MAX_JOIN_TABLES],
    /// Derived tables (`FROM (SELECT ...) alias`): the materialized rows,
    /// self-describing-encoded. `None` marks a physical table (scanned from
    /// storage by `slots`).
    pub derived: [Option<&'d [&'d [u8]]>; MAX_JOIN_TABLES],
    pub n: usize,
}

impl<'d> QueryScope<'d> {
    pub fn resolve(storage: &'d Storage, from: &FromClause<'d>) -> Result<Self, SqlError> {
        let mut scope = QueryScope::empty();
        scope.add_ref(storage, &from.base)?;
        for j in from.joins {
            scope.add_ref(storage, &j.table)?;
        }
        Ok(scope)
    }

    fn empty() -> Self {
        QueryScope {
            names: [""; MAX_JOIN_TABLES],
            defs: [None; MAX_JOIN_TABLES],
            slots: [0; MAX_JOIN_TABLES],
            derived: [None; MAX_JOIN_TABLES],
            n: 0,
        }
    }

    /// Like `resolve`, but materializes any derived table (`FROM (SELECT ...)`)
    /// by running its subquery once and synthesizing a `TableDef` for its
    /// output columns. Used by the executors that actually scan rows.
    pub fn resolve_exec<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
    ) -> Result<QueryScope<'a>, SqlError> {
        let mut scope = QueryScope::empty();
        scope.add_exec(storage, &from.base, txid, arena, params)?;
        for j in from.joins {
            scope.add_exec(storage, &j.table, txid, arena, params)?;
        }
        Ok(scope)
    }

    fn add_ref(&mut self, storage: &'d Storage, tref: &TableRef<'d>) -> Result<(), SqlError> {
        if tref.subquery.is_some() || tref.func_args.is_some() {
            // Materialized separately by the executor; the schema-only resolve
            // path (no arena) cannot synthesize derived or function columns.
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "subqueries or functions in FROM are not supported in this context"
            ));
        }
        self.add(storage, tref.table, tref.alias, 0)
    }

    /// Add one FROM item, materializing a derived table if `tref` is a subquery.
    fn add_exec<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        if tref.func_args.is_some() {
            return self.add_table_func(tref, arena, params, true);
        }
        let Some(sub) = tref.subquery else {
            if super::catalog::is_catalog_relation(tref.schema, tref.table) {
                return self.add_catalog(storage, tref, arena, true);
            }
            return self.add(storage, tref.table, tref.alias, txid);
        };
        let exposed = tref.alias.expect("parser requires a derived-table alias");
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                "42712",
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let def_ref = synth_derived_def(storage, sub, exposed, txid, arena)?;
        // Materialize the subquery rows, self-describing-encoded, into a
        // doubling arena vector.
        const EMPTY: &[u8] = &[];
        let mut store: *mut &[u8] = core::ptr::null_mut();
        let mut len = 0usize;
        let mut cap = 0usize;
        select_into_rows(storage, txid, sub, arena, params, &mut |vals| {
            let enc = super::exec::encode_projected_pub(vals, arena)?;
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
        })?;
        let rows: &'a [&'a [u8]] = if len == 0 {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(store, len) }
        };
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_ref);
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
    }

    /// Like `resolve`, but synthesizes a `TableDef` for each derived table
    /// (`FROM (SELECT ...)`) without materializing its rows. Used where only
    /// the output schema is needed (extended-protocol Describe), which has no
    /// txid or bound parameters.
    pub fn resolve_schema<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<QueryScope<'a>, SqlError> {
        let mut scope = QueryScope::empty();
        scope.add_schema(storage, &from.base, txid, arena)?;
        for j in from.joins {
            scope.add_schema(storage, &j.table, txid, arena)?;
        }
        Ok(scope)
    }

    fn add_schema<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        if tref.func_args.is_some() {
            return self.add_table_func(tref, arena, &[], false);
        }
        let Some(sub) = tref.subquery else {
            if super::catalog::is_catalog_relation(tref.schema, tref.table) {
                return self.add_catalog(storage, tref, arena, false);
            }
            return self.add(storage, tref.table, tref.alias, txid);
        };
        let exposed = tref.alias.expect("parser requires a derived-table alias");
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                "42712",
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let def_ref = synth_derived_def(storage, sub, exposed, txid, arena)?;
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_ref);
        // No rows: this scope is never scanned, only described. An empty row
        // set keeps a stray scan safe rather than reading a physical slot.
        self.derived[self.n] = Some(&[]);
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
    }

    /// Registers a `pg_catalog` / `information_schema` relation as a
    /// derived-table entry (synthesized rows), so the general executor can
    /// join it, use it in subqueries, etc. `materialize` false = schema only
    /// (Describe path).
    fn add_catalog<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        arena: &'a Arena,
        materialize: bool,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let synth = super::catalog::synthesize(storage, tref.schema, tref.table, arena)?;
        let exposed = tref.alias.unwrap_or(tref.table);
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                "42712",
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let def_ref: &'a TableDef = arena.alloc(synth.def).map_err(|_| arena_full())?;
        let rows: &'a [&'a [u8]] = if materialize {
            const EMPTY: &[u8] = &[];
            let encoded = arena
                .alloc_slice_with(synth.rows.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (i, r) in synth.rows.iter().enumerate() {
                encoded[i] = super::exec::encode_projected_pub(r, arena)?;
            }
            encoded
        } else {
            &[]
        };
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_ref);
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
    }

    /// Registers a table function (`FROM func(args) alias`) as a derived-table
    /// entry. `materialize` false = schema only (Describe / synth-def path).
    fn add_table_func<'a>(
        &mut self,
        tref: &'a TableRef<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        materialize: bool,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let def_ref = table_func_def(tref, arena)?;
        let exposed = tref.alias.unwrap_or(tref.table);
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                "42712",
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let rows: &'a [&'a [u8]] =
            if materialize { table_func_rows(tref, arena, params)? } else { &[] };
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_ref);
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
    }

    fn add(
        &mut self,
        storage: &'d Storage,
        table: &str,
        alias: Option<&'d str>,
        txid: u32,
    ) -> Result<(), SqlError> {
        // `txid == 0` (schema-only / Describe) resolves against the committed
        // catalog; a real transaction sees its own uncommitted CREATE/DROP.
        let Some(slot) = storage.find_visible(table, txid) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "relation \"{}\" does not exist",
                table
            ));
        };
        let def = &storage.table(slot).def;
        let exposed = alias.unwrap_or(def.name.as_str());
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                "42712",
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def);
        self.slots[self.n] = slot;
        self.n += 1;
        Ok(())
    }

    /// (table position, column index) for a possibly-qualified name.
    pub fn find_column(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<(usize, usize), SqlError> {
        match qualifier {
            Some(q) => {
                let Some(t) = self.names[..self.n].iter().position(|n| *n == q) else {
                    return Err(sql_err!(
                        "42P01",
                        "missing FROM-clause entry for table \"{}\"",
                        q
                    ));
                };
                match self.defs[t].expect("resolved").column_index(name) {
                    Some(c) => Ok((t, c)),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column {}.{} does not exist",
                        q,
                        name
                    )),
                }
            }
            None => {
                let mut found = None;
                for t in 0..self.n {
                    if let Some(c) = self.defs[t].expect("resolved").column_index(name) {
                        if found.is_some() {
                            return Err(sql_err!(
                                "42702",
                                "column reference \"{}\" is ambiguous",
                                name
                            ));
                        }
                        found = Some((t, c));
                    }
                }
                found.ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" does not exist",
                        name
                    )
                })
            }
        }
    }

    pub fn total_columns(&self) -> usize {
        (0..self.n)
            .map(|t| self.defs[t].expect("resolved").n_columns)
            .sum()
    }
}

/// One assembled source row: per table, decoded values (empty slice =
/// LEFT-join null row; None = not yet joined).
pub struct JoinRow<'s, 'v, 'd> {
    pub scope: &'s QueryScope<'d>,
    pub values: [Option<&'s [Datum<'v>]>; MAX_JOIN_TABLES],
}

impl<'v> ColumnLookup<'v> for JoinRow<'_, 'v, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        let (t, c) = self.scope.find_column(qualifier, name)?;
        match self.values[t] {
            // Empty slice = LEFT-join null row.
            Some([]) => Ok(Datum::Null),
            Some(vals) => Ok(vals[c]),
            None => Err(sql_err!(
                "42P10",
                "column \"{}\" referenced before its table is joined",
                name
            )),
        }
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<super::types::ColType> {
        let (t, c) = self.scope.find_column(qualifier, name).ok()?;
        self.scope.defs[t].map(|d| d.columns()[c].ctype)
    }
}

/// Chains an inner row's column resolution to an optional outer row (for
/// correlated subqueries): a name unresolved inside the subquery falls back
/// to the enclosing query's row.
struct Chained<'r, 'a> {
    inner: &'r dyn ColumnLookup<'a>,
    outer: Option<&'r dyn ColumnLookup<'a>>,
}
impl<'a> ColumnLookup<'a> for Chained<'_, 'a> {
    fn lookup(&self, q: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        match self.inner.lookup(q, name) {
            Ok(v) => Ok(v),
            Err(e) => match self.outer {
                Some(o) => o.lookup(q, name),
                None => Err(e),
            },
        }
    }
    fn col_type(&self, q: Option<&str>, name: &str) -> Option<super::types::ColType> {
        self.inner
            .col_type(q, name)
            .or_else(|| self.outer.and_then(|o| o.col_type(q, name)))
    }
}

/// Enumerates source rows (visibility-filtered, ON conditions applied,
/// WHERE applied), calling `f` per row. `f` returns false to stop early.
#[allow(clippy::too_many_arguments)]
fn scan_source<'a>(
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
    // Fold `col IS [NOT] NULL` on NOT-NULL columns, then order the WHERE
    // conjuncts by PostgreSQL's clause cost once, up front, so the per-row leaf
    // evaluates them cheapest-first without re-sorting.
    let where_clause = match where_clause {
        Some(w) => Some(reorder_qual(fold_null(w, scope, arena)?, scope, arena)?),
        None => None,
    };
    // Assemble a JoinRow from the currently bound row bytes. Physical rows
    // are heap-encoded (fixed schema); derived rows are self-describing.
    fn assemble<'s, 'v, 'd>(
        scope: &'s QueryScope<'d>,
        bound: &[Option<&'v [u8]>; MAX_JOIN_TABLES],
        depth: usize,
        buffers: &'s mut [[Datum<'v>; MAX_COLUMNS]; MAX_JOIN_TABLES],
    ) -> Result<JoinRow<'s, 'v, 'd>, SqlError> {
        let mut values: [Option<&[Datum]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
        // Split buffers so each table borrows a distinct buffer.
        let mut rest: &mut [[Datum<'v>; MAX_COLUMNS]] = buffers;
        for t in 0..depth {
            let (buf, tail) = rest.split_first_mut().expect("enough buffers");
            rest = tail;
            let def = scope.defs[t].expect("resolved");
            match bound[t] {
                Some(bytes) => {
                    if scope.derived[t].is_some() {
                        for (c, slot) in buf.iter_mut().enumerate().take(def.n_columns) {
                            *slot = super::exec::decode_projected_pub(bytes, c);
                        }
                    } else {
                        let mut schema = [ColType::Bool; MAX_COLUMNS];
                        def.schema(&mut schema);
                        rowenc::decode(bytes, &schema[..def.n_columns], buf)?;
                    }
                    values[t] = Some(&buf[..def.n_columns]);
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
        // For a RIGHT/FULL join (always the last level), one flag per scanned
        // row of the deepest table, marking those that found a left partner.
        matched: Option<&[core::cell::Cell<bool>]>,
        // Error-safe WHERE conjuncts to check at each depth (predicate pushdown).
        pushdown: &[&[&'a Expr<'a>]],
        f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        if depth == scope.n {
            let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
            let row = assemble(scope, bound, depth, &mut buffers)?;
            if let Some(w) = where_clause {
                let cr = Chained { inner: &row, outer };
                if !where_passes(w, arena, params, &cr, hooks)? {
                    return Ok(true);
                }
            }
            return f(&row);
        }

        let join = if depth == 0 { None } else { Some(&from.joins[depth - 1]) };
        let mut matched_any = false;
        // Bind one candidate row for this level, run the ON condition, then
        // recurse. `on_matches` returns false to skip the row.
        let on_matches = |bound: &mut [Option<&'a [u8]>; MAX_JOIN_TABLES]|
         -> Result<bool, SqlError> {
            if let Some(join) = join
                && let Some(on) = join.on {
                    let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
                    let row = assemble(scope, bound, depth + 1, &mut buffers)?;
                    let cr = Chained { inner: &row, outer };
                    return match eval_full(on, arena, params, &cr, hooks)? {
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
        let passes_pushdown = |bound: &[Option<&'a [u8]>; MAX_JOIN_TABLES]|
         -> Result<bool, SqlError> {
            if pushdown[depth].is_empty() {
                return Ok(true);
            }
            let mut pbuf = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
            let prow = assemble(scope, bound, depth + 1, &mut pbuf)?;
            let pcr = Chained { inner: &prow, outer };
            for &c in pushdown[depth] {
                if !conjunct_passes(c, arena, params, &pcr, hooks)? {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        // Derived tables scan their materialized rows; physical tables scan the
        // visibility-filtered heap.
        if let Some(rows) = scope.derived[depth] {
            for (idx, bytes) in rows.iter().enumerate() {
                bound[depth] = Some(bytes);
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched.filter(|_| depth + 1 == scope.n) {
                    m[idx].set(true);
                }
                if !level(
                    storage, scope, from, txid, where_clause, arena, params, hooks,
                    outer, depth + 1, bound, matched, pushdown, f,
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
            let table = storage.table(scope.slots[depth]);
            let mut count = 0usize;
            for (_, state) in table.rows.iter() {
                if state.visible_to(txid).is_some() {
                    count += 1;
                }
            }
            let mut src = table.rows.iter();
            let ordered = arena
                .alloc_slice_with(count, |_| loop {
                    let (_, state) = src.next().expect("visible count is stable");
                    if let Some(loc) = state.visible_to(txid) {
                        break loc;
                    }
                })
                .map_err(|_| arena_full())?;
            ordered.sort_unstable_by_key(|l| l.offset);
            for (this, &loc) in ordered.iter().enumerate() {
                bound[depth] = Some(storage.heap.get(loc));
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched.filter(|_| depth + 1 == scope.n) {
                    m[this].set(true);
                }
                if !level(
                    storage, scope, from, txid, where_clause, arena, params, hooks,
                    outer, depth + 1, bound, matched, pushdown, f,
                )? {
                    return Ok(false);
                }
            }
        } else {
            let table = storage.table(scope.slots[depth]);
            let mut idx = 0usize;
            for (_, state) in table.rows.iter() {
                let Some(loc) = state.visible_to(txid) else {
                    continue;
                };
                bound[depth] = Some(storage.heap.get(loc));
                let this = idx;
                idx += 1;
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched.filter(|_| depth + 1 == scope.n) {
                    m[this].set(true);
                }
                if !level(
                    storage, scope, from, txid, where_clause, arena, params, hooks,
                    outer, depth + 1, bound, matched, pushdown, f,
                )? {
                    return Ok(false);
                }
            }
        }
        // LEFT/FULL join with no match at this level: emit one null row (the
        // left side preserved, this table nulled).
        if !matched_any
            && join.is_some_and(|j| matches!(j.kind, JoinKind::Left | JoinKind::Full))
        {
            bound[depth] = None;
            if !level(
                storage, scope, from, txid, where_clause, arena, params, hooks,
                outer, depth + 1, bound, matched, pushdown, f,
            )? {
                return Ok(false);
            }
        }
        bound[depth] = None;
        Ok(true)
    }

    // RIGHT/FULL JOIN is supported only as the final join (the deepest table),
    // so an unmatched right row null-pads the whole left side with nothing
    // joined after it. A RIGHT/FULL join earlier in the chain is rejected.
    let deep = scope.n.saturating_sub(1);
    for (i, j) in from.joins[..from.joins.len().min(deep)].iter().enumerate() {
        if matches!(j.kind, JoinKind::Right | JoinKind::Full) && i + 1 != deep {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "RIGHT/FULL JOIN is only supported as the last join"
            ));
        }
    }
    let deep_kind = if deep >= 1 {
        Some(from.joins[deep - 1].kind)
    } else {
        None
    };
    let preserve_right = matches!(deep_kind, Some(JoinKind::Right | JoinKind::Full));

    // For a RIGHT/FULL last join, allocate one match flag per row of the
    // deepest table (arena-backed, so no post-init allocation).
    let matched: Option<&[core::cell::Cell<bool>]> = if preserve_right {
        let n_rows = if let Some(rows) = scope.derived[deep] {
            rows.len()
        } else {
            let table = storage.table(scope.slots[deep]);
            table.rows.iter().filter(|(_, s)| s.visible_to(txid).is_some()).count()
        };
        let flags = arena
            .alloc_slice_with(n_rows, |_| false)
            .map_err(|_| arena_full())?;
        Some(core::cell::Cell::from_mut(flags).as_slice_of_cells())
    } else {
        None
    };

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
    let mut pd_bufs: [[&Expr; MAX_CONJUNCTS]; MAX_JOIN_TABLES] =
        [[&Expr::Null; MAX_CONJUNCTS]; MAX_JOIN_TABLES];
    let mut pd_n = [0usize; MAX_JOIN_TABLES];
    if all_inner && scope.n >= 2 && let Some(w) = where_clause {
        let mut conj: [&Expr; MAX_CONJUNCTS] = [w; MAX_CONJUNCTS];
        let mut n = 0;
        let conjuncts: &[&Expr] =
            if flatten_and(w, &mut conj, &mut n) { &conj[..n] } else { core::slice::from_ref(&w) };
        for &c in conjuncts {
            if is_error_safe(c)
                && let Some(d) = expr_max_table(c, scope)
                && d < scope.n
                && pd_n[d] < MAX_CONJUNCTS
            {
                pd_bufs[d][pd_n[d]] = c;
                pd_n[d] += 1;
            }
        }
    }
    let pushdown: [&[&Expr]; MAX_JOIN_TABLES] = core::array::from_fn(|d| &pd_bufs[d][..pd_n[d]]);

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
        matched,
        &pushdown,
        f,
    )?;

    // RIGHT/FULL post-pass: emit each unmatched deepest-table row with the
    // whole left side nulled.
    if let Some(m) = matched {
        let emit_unmatched = |bytes: &'a [u8],
                              f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>|
         -> Result<bool, SqlError> {
            let mut b = [None; MAX_JOIN_TABLES];
            b[deep] = Some(bytes);
            let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
            let row = assemble(scope, &b, scope.n, &mut buffers)?;
            if let Some(w) = where_clause {
                let cr = Chained { inner: &row, outer };
                if !where_passes(w, arena, params, &cr, hooks)? {
                    return Ok(true);
                }
            }
            f(&row)
        };
        if let Some(rows) = scope.derived[deep] {
            for (idx, bytes) in rows.iter().enumerate() {
                if !m[idx].get() && !emit_unmatched(bytes, f)? {
                    return Ok(());
                }
            }
        } else {
            let table = storage.table(scope.slots[deep]);
            let mut idx = 0usize;
            for (_, state) in table.rows.iter() {
                let Some(loc) = state.visible_to(txid) else {
                    continue;
                };
                let this = idx;
                idx += 1;
                if !m[this].get() && !emit_unmatched(storage.heap.get(loc), f)? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Expands a query's non-recursive `WITH` CTEs into derived tables: every
/// FROM reference to a CTE name (in the body and in nested subqueries) is
/// rewritten to `(cte_query) alias`, so the ordinary derived-table executor
/// runs the whole thing. Returns the CTE-free select. A no-op when there are
/// no CTEs. Each CTE is substituted against the CTEs declared before it.
/// A view that PostgreSQL treats as auto-updatable: a single base table, no
/// aggregation/DISTINCT/GROUP BY/HAVING/LIMIT/joins, and every output column a
/// plain (un-aliased) base column. `where_clause` is the view's own filter, to
/// be AND-ed into any DML on the view; `columns` are the exposed base columns.
pub struct UpdatableView<'a> {
    pub base: &'a str,
    pub where_clause: Option<&'a Expr<'a>>,
    pub columns: &'a [&'a str],
}

/// If `name` is a view, resolve it for DML: `Ok(Some(..))` when auto-updatable,
/// `Err` (0A000) when it is a view but not auto-updatable, `Ok(None)` when it
/// is not a view at all (the DML then targets a table normally).
pub fn resolve_view_for_dml<'a>(
    storage: &Storage,
    name: &str,
    arena: &'a Arena,
) -> Result<Option<UpdatableView<'a>>, SqlError> {
    let Some(sql) = storage.find_view(name) else {
        return Ok(None);
    };
    // Copy the definition into the arena so the parsed AST no longer borrows
    // storage (the caller then takes a mutable storage borrow to run the DML).
    let sql = arena.alloc_str(sql).map_err(|_| arena_full())?;
    let not_updatable = || {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot change view \"{}\": it is not auto-updatable",
            name
        )
    };
    let sel = super::parser::parse_view_select(sql, arena)?;
    if sel.distinct
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || sel.limit.is_some()
        || sel.offset.is_some()
    {
        return Err(not_updatable());
    }
    let Some(from) = &sel.from else {
        return Err(not_updatable());
    };
    if !from.joins.is_empty() || from.base.subquery.is_some() || from.base.schema.is_some() {
        return Err(not_updatable());
    }
    let base = from.base.table;
    let mut cols = [""; MAX_PROJ];
    let mut n = 0;
    for it in sel.items {
        match it {
            SelectItem::Wildcard => {
                let Some(ti) = storage.find_table(base) else {
                    return Err(not_updatable());
                };
                for c in storage.table(ti).def.columns() {
                    if n == MAX_PROJ {
                        return Err(not_updatable());
                    }
                    // Copy into the arena so it does not borrow storage.
                    cols[n] = arena.alloc_str(c.name.as_str()).map_err(|_| arena_full())?;
                    n += 1;
                }
            }
            // Only a plain, un-aliased base column keeps view and base names in
            // sync (so the view's/DML's WHERE resolve directly against the base).
            SelectItem::Expr { expr: Expr::Column { name: cn, .. }, alias } => {
                if alias.is_some_and(|a| a != *cn) {
                    return Err(not_updatable());
                }
                if n == MAX_PROJ {
                    return Err(not_updatable());
                }
                cols[n] = cn;
                n += 1;
            }
            _ => return Err(not_updatable()),
        }
    }
    let columns = arena.alloc_slice_copy(&cols[..n]).map_err(|_| arena_full())?;
    Ok(Some(UpdatableView { base, where_clause: sel.where_clause, columns }))
}

/// Combines a view's filter with a DML's WHERE (AND), for view DML rewriting.
pub fn and_where<'a>(
    view_where: Option<&'a Expr<'a>>,
    dml_where: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match (view_where, dml_where) {
        (None, w) | (w, None) => Ok(w),
        (Some(a), Some(b)) => {
            let e = Expr::Binary { op: super::ast::BinaryOp::And, left: a, right: b };
            Ok(Some(&*arena.alloc(e).map_err(|_| arena_full())?))
        }
    }
}

/// Validates a view definition at CREATE VIEW time, as PostgreSQL does: the
/// SELECT must parse, its tables/views must exist, and its output columns must
/// resolve. Surfaces the same errors (42P01 / 42703) a query would.
pub fn validate_view<'a>(
    sql: &'a str,
    storage: &'a Storage,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let sel = super::parser::parse_view_select(sql, arena)?;
    let sel = expand_ctes(sel, storage, arena)?;
    let mut cols = [ColDesc::new("", 0, 0); MAX_PROJ];
    match &sel.from {
        Some(from) => {
            // Committed catalog: a view's referents are validated as visible now.
            let scope = QueryScope::resolve_schema(storage, from, 0, arena)?;
            describe_scope_items(sel.items, &scope, &mut cols)?;
        }
        None => {
            describe_items(sel.items, None, &mut cols)?;
        }
    }
    Ok(())
}

pub fn expand_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &'a Storage,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    // Fast path: nothing to rewrite (no CTEs anywhere and no views defined).
    if sel.with.is_empty() && !storage.has_any_view() {
        return Ok(sel);
    }
    if sel.with.len() > super::parser::MAX_CTES {
        return Err(sql_err!("54023", "too many WITH entries"));
    }
    // Resolve CTEs left-to-right so a CTE can reference earlier ones.
    let mut resolved: [(&'a str, &'a Select<'a>); super::parser::MAX_CTES] =
        [("", sel); super::parser::MAX_CTES];
    let mut n = 0;
    for cte in sel.with {
        if resolved[..n].iter().any(|(name, _)| *name == cte.name) {
            return Err(sql_err!("42712", "WITH query name \"{}\" specified more than once", cte.name));
        }
        let ctx = Subst { ctes: &resolved[..n], storage, depth: 0 };
        let q = subst_select(cte.query, ctx, arena)?;
        resolved[n] = (cte.name, q);
        n += 1;
    }
    // Substitute the body against all CTEs (the WITH list is dropped by
    // subst_select, which never copies it) and expand any view references.
    let ctx = Subst { ctes: &resolved[..n], storage, depth: 0 };
    subst_select(sel, ctx, arena)
}

type CteBindings<'a> = [(&'a str, &'a Select<'a>)];

/// Threaded through the FROM-reference rewrite: CTE bindings in scope, storage
/// (to resolve view names), and the current view-expansion depth (a cycle /
/// runaway-nesting guard).
#[derive(Clone, Copy)]
struct Subst<'c, 'a> {
    ctes: &'c CteBindings<'a>,
    storage: &'a Storage,
    depth: u32,
}

const MAX_VIEW_DEPTH: u32 = 12;

fn subst_select<'a>(
    s: &'a Select<'a>,
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let from = match &s.from {
        Some(f) => Some(subst_from(f, ctx, arena)?),
        None => None,
    };
    let mut items = [SelectItem::Wildcard; MAX_PROJ];
    if s.items.len() > MAX_PROJ {
        return Err(sql_err!("54011", "select list too wide"));
    }
    for (i, it) in s.items.iter().enumerate() {
        items[i] = match it {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::Expr { expr, alias } => SelectItem::Expr {
                expr: subst_expr(expr, ctx, arena)?,
                alias: *alias,
            },
        };
    }
    let items = arena.alloc_slice_copy(&items[..s.items.len()]).map_err(|_| arena_full())?;
    let group_by = subst_expr_slice(s.group_by, ctx, arena)?;
    let mut order = [OrderBy { expr: &Expr::Null, descending: false, nulls_first: false };
        super::parser::MAX_LIST];
    if s.order_by.len() > super::parser::MAX_LIST {
        return Err(sql_err!("54023", "ORDER BY list too long"));
    }
    for (i, ob) in s.order_by.iter().enumerate() {
        order[i] = OrderBy { expr: subst_expr(ob.expr, ctx, arena)?, ..*ob };
    }
    let order_by = arena.alloc_slice_copy(&order[..s.order_by.len()]).map_err(|_| arena_full())?;
    let set_body = match s.set_body {
        Some(tree) => Some(subst_set_tree(tree, ctx, arena)?),
        None => None,
    };
    let new = Select {
        items,
        distinct: s.distinct,
        from,
        where_clause: opt_subst(s.where_clause, ctx, arena)?,
        group_by,
        having: opt_subst(s.having, ctx, arena)?,
        order_by,
        limit: opt_subst(s.limit, ctx, arena)?,
        offset: opt_subst(s.offset, ctx, arena)?,
        with: &[],
        set_body,
    };
    Ok(&*arena.alloc(new).map_err(|_| arena_full())?)
}

/// Substitutes parameters through every leaf SELECT of a set-operation tree,
/// mirroring [`subst_select`] for a set-op subquery body.
fn subst_set_tree<'a>(
    tree: &'a SetTree<'a>,
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let out = match tree {
        SetTree::Select(s) => SetTree::Select(subst_select(s, ctx, arena)?),
        SetTree::Op { op, all, left, right } => SetTree::Op {
            op: *op,
            all: *all,
            left: subst_set_tree(left, ctx, arena)?,
            right: subst_set_tree(right, ctx, arena)?,
        },
    };
    Ok(&*arena.alloc(out).map_err(|_| arena_full())?)
}

fn subst_from<'a>(
    f: &'a FromClause<'a>,
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<FromClause<'a>, SqlError> {
    let base = subst_tableref(&f.base, ctx, arena)?;
    let dummy = Join { table: f.base, kind: JoinKind::Inner, on: None };
    let mut joins = [dummy; MAX_JOIN_TABLES - 1];
    if f.joins.len() > joins.len() {
        return Err(sql_err!("54023", "too many joins"));
    }
    for (i, j) in f.joins.iter().enumerate() {
        joins[i] = Join {
            table: subst_tableref(&j.table, ctx, arena)?,
            kind: j.kind,
            on: opt_subst(j.on, ctx, arena)?,
        };
    }
    let joins = arena.alloc_slice_copy(&joins[..f.joins.len()]).map_err(|_| arena_full())?;
    Ok(FromClause { base, joins })
}

fn subst_tableref<'a>(
    t: &TableRef<'a>,
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<TableRef<'a>, SqlError> {
    if let Some(sub) = t.subquery {
        return Ok(TableRef {
            subquery: Some(subst_select(sub, ctx, arena)?),
            ..*t
        });
    }
    // An unqualified name matching a CTE becomes a derived table over the
    // (already-substituted) CTE query, exposed under its alias or CTE name.
    if t.schema.is_none()
        && let Some((_, q)) = ctx.ctes.iter().find(|(name, _)| *name == t.table)
    {
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(q),
            func_args: None,
            col_alias: None,
        });
    }
    // A name matching a view (and not shadowed by a CTE or table) expands to a
    // derived table over the view's stored SELECT, recursively expanded.
    if t.schema.is_none()
        && ctx.storage.find_table(t.table).is_none()
        && let Some(view_sql) = ctx.storage.find_view(t.table)
    {
        if ctx.depth >= MAX_VIEW_DEPTH {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "view \"{}\" nests too deeply (or references itself)",
                t.table
            ));
        }
        let vsel = super::parser::parse_view_select(view_sql, arena)?;
        // The view body has its own scope: no outer CTEs, deeper view depth.
        let inner = Subst {
            ctes: &[],
            storage: ctx.storage,
            depth: ctx.depth + 1,
        };
        let expanded = subst_select(vsel, inner, arena)?;
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(expanded),
            func_args: None,
            col_alias: None,
        });
    }
    Ok(*t)
}

fn opt_subst<'a>(
    e: Option<&'a Expr<'a>>,
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match e {
        Some(x) => Ok(Some(subst_expr(x, ctx, arena)?)),
        None => Ok(None),
    }
}

fn subst_expr_slice<'a>(
    xs: &'a [&'a Expr<'a>],
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a [&'a Expr<'a>], SqlError> {
    if !xs.iter().any(|x| expr_has_subquery(x)) {
        return Ok(xs);
    }
    let mut tmp = [&Expr::Null; super::parser::MAX_LIST];
    if xs.len() > tmp.len() {
        return Err(sql_err!("54023", "expression list too long"));
    }
    for (i, x) in xs.iter().enumerate() {
        tmp[i] = subst_expr(x, ctx, arena)?;
    }
    Ok(&*arena.alloc_slice_copy(&tmp[..xs.len()]).map_err(|_| arena_full())?)
}

/// True if `e` contains a subquery anywhere (so it needs rebuilding when CTEs
/// are substituted). Leaves and subquery-free trees are returned unchanged.
fn expr_has_subquery(e: &Expr) -> bool {
    match e {
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
        | Expr::ArraySubquery(_) => true,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => expr_has_subquery(operand),
        Expr::Binary { left, right, .. } => expr_has_subquery(left) || expr_has_subquery(right),
        Expr::Call { args, order_by, .. } => {
            args.iter().any(|a| expr_has_subquery(a))
                || order_by.iter().any(|o| expr_has_subquery(o.expr))
        }
        Expr::InList { operand, list, .. } => {
            expr_has_subquery(operand) || list.iter().any(|a| expr_has_subquery(a))
        }
        Expr::Between { operand, low, high, .. } => {
            expr_has_subquery(operand) || expr_has_subquery(low) || expr_has_subquery(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            expr_has_subquery(operand) || expr_has_subquery(pattern)
        }
        Expr::Case { operand, whens, otherwise } => {
            operand.is_some_and(expr_has_subquery)
                || whens.iter().any(|(c, r)| expr_has_subquery(c) || expr_has_subquery(r))
                || otherwise.is_some_and(expr_has_subquery)
        }
        _ => false,
    }
}

fn subst_expr<'a>(
    e: &'a Expr<'a>,
    ctx: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if !expr_has_subquery(e) {
        return Ok(e);
    }
    let rebuilt = match e {
        Expr::Subquery(s) => Expr::Subquery(subst_select(s, ctx, arena)?),
        Expr::ArraySubquery(s) => Expr::ArraySubquery(subst_select(s, ctx, arena)?),
        Expr::Exists(s) => Expr::Exists(subst_select(s, ctx, arena)?),
        Expr::InSubquery { operand, select, negated } => Expr::InSubquery {
            operand: subst_expr(operand, ctx, arena)?,
            select: subst_select(select, ctx, arena)?,
            negated: *negated,
        },
        Expr::Unary { op, operand } => Expr::Unary {
            op: *op,
            operand: subst_expr(operand, ctx, arena)?,
        },
        Expr::Binary { op, left, right } => Expr::Binary {
            op: *op,
            left: subst_expr(left, ctx, arena)?,
            right: subst_expr(right, ctx, arena)?,
        },
        Expr::Cast { operand, type_name, type_mod } => Expr::Cast {
            operand: subst_expr(operand, ctx, arena)?,
            type_name,
            type_mod: *type_mod,
        },
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: subst_expr(operand, ctx, arena)?,
            negated: *negated,
        },
        Expr::Call { name, args, star, distinct, order_by, over } => {
            let mut ob = [OrderBy { expr: &Expr::Null, descending: false, nulls_first: false };
                super::parser::MAX_LIST];
            if order_by.len() > ob.len() {
                return Err(sql_err!("54023", "aggregate ORDER BY list too long"));
            }
            for (i, o) in order_by.iter().enumerate() {
                ob[i] = OrderBy { expr: subst_expr(o.expr, ctx, arena)?, ..*o };
            }
            let order_by = arena
                .alloc_slice_copy(&ob[..order_by.len()])
                .map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(w) => {
                    let mut ob2 = [OrderBy { expr: &Expr::Null, descending: false, nulls_first: false };
                        super::parser::MAX_LIST];
                    for (i, o) in w.order_by.iter().enumerate() {
                        ob2[i] = OrderBy { expr: subst_expr(o.expr, ctx, arena)?, ..*o };
                    }
                    let spec = super::ast::WindowSpec {
                        partition_by: subst_expr_slice(w.partition_by, ctx, arena)?,
                        order_by: arena.alloc_slice_copy(&ob2[..w.order_by.len()]).map_err(|_| arena_full())?,
                    };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            Expr::Call {
                name,
                args: subst_expr_slice(args, ctx, arena)?,
                star: *star,
                distinct: *distinct,
                order_by,
                over,
            }
        }
        Expr::InList { operand, list, negated } => Expr::InList {
            operand: subst_expr(operand, ctx, arena)?,
            list: subst_expr_slice(list, ctx, arena)?,
            negated: *negated,
        },
        Expr::Between { operand, low, high, negated } => Expr::Between {
            operand: subst_expr(operand, ctx, arena)?,
            low: subst_expr(low, ctx, arena)?,
            high: subst_expr(high, ctx, arena)?,
            negated: *negated,
        },
        Expr::Like { operand, pattern, negated, case_insensitive } => Expr::Like {
            operand: subst_expr(operand, ctx, arena)?,
            pattern: subst_expr(pattern, ctx, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Match { operand, pattern, negated, case_insensitive } => Expr::Match {
            operand: subst_expr(operand, ctx, arena)?,
            pattern: subst_expr(pattern, ctx, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Case { operand, whens, otherwise } => {
            let operand = opt_subst(*operand, ctx, arena)?;
            let mut ws = [(&Expr::Null, &Expr::Null); super::parser::MAX_LIST];
            if whens.len() > ws.len() {
                return Err(sql_err!("54023", "CASE has too many WHEN branches"));
            }
            for (i, (c, r)) in whens.iter().enumerate() {
                ws[i] = (subst_expr(c, ctx, arena)?, subst_expr(r, ctx, arena)?);
            }
            let whens = arena.alloc_slice_copy(&ws[..whens.len()]).map_err(|_| arena_full())?;
            Expr::Case {
                operand,
                whens,
                otherwise: opt_subst(*otherwise, ctx, arena)?,
            }
        }
        // Leaves never reach here (guarded by expr_has_subquery above).
        other => *other,
    };
    Ok(&*arena.alloc(rebuilt).map_err(|_| arena_full())?)
}

/// Walks an expression tree collecting aggregate call nodes.
fn collect_aggs<'a>(
    expr: &'a Expr<'a>,
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if expr.is_aggregate() {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, expr)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!("54000", "too many aggregates in one query"));
        }
        out[*n] = (expr as *const _, expr);
        *n += 1;
        return Ok(()); // aggregate arguments evaluate per input row
    }
    walk_children(expr, &mut |child| collect_aggs(child, out, n))
}

/// Collects window-function call nodes (a `Call` with an `OVER` clause).
fn collect_windows<'a>(
    expr: &'a Expr<'a>,
    out: &mut [&'a Expr<'a>; MAX_WINDOWS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if let Expr::Call { over: Some(_), .. } = expr {
        if out[..*n].iter().any(|e| core::ptr::eq(*e, expr)) {
            return Ok(());
        }
        if *n == MAX_WINDOWS {
            return Err(sql_err!("54023", "too many window functions in one query"));
        }
        out[*n] = expr;
        *n += 1;
        // The arguments and PARTITION/ORDER expressions evaluate per input row;
        // a window function nested inside another is not supported and would be
        // found by the analysis pass, not here.
        return Ok(());
    }
    walk_children(expr, &mut |child| collect_windows(child, out, n))
}

/// Builds a `JoinRow` view over one flat materialized row (all scope columns
/// concatenated, table by table).
fn window_row<'r, 'a>(
    scope: &'r QueryScope<'a>,
    flat: &'r [Datum<'a>],
    offs: &[usize],
) -> JoinRow<'r, 'a, 'a> {
    let mut values: [Option<&[Datum]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
    for (t, off) in offs.iter().enumerate().take(scope.n) {
        let nc = scope.defs[t].expect("resolved").n_columns;
        values[t] = Some(&flat[*off..*off + nc]);
    }
    JoinRow { scope, values }
}

/// Whether two rows have equal tuples over `keys` (NULLs compare equal, as in
/// window PARTITION BY / peer grouping).
#[allow(clippy::too_many_arguments)]
fn keys_equal<'a>(
    keys: &[&'a Expr<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    a: usize,
    b: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    for k in keys {
        let ra = window_row(scope, rows[a], offs);
        let va = eval_full(k, arena, params, &ra, hooks)?;
        let rb = window_row(scope, rows[b], offs);
        let vb = eval_full(k, arena, params, &rb, hooks)?;
        let eq = match (va.is_null(), vb.is_null()) {
            (true, true) => true,
            (true, false) | (false, true) => false,
            (false, false) => compare_datums(&va, &vb)?.is_eq(),
        };
        if !eq {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Computes one window function's value for every materialized row, returned as
/// a slice indexed by materialized-row order.
#[allow(clippy::too_many_arguments, clippy::needless_range_loop)]
fn compute_window<'a>(
    node: &'a Expr<'a>,
    rows: &[&'a [Datum<'a>]],
    scope: &QueryScope<'a>,
    offs: &[usize],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<&'a [Datum<'a>], SqlError> {
    let Expr::Call { name, args, over: Some(spec), .. } = node else {
        return Err(sql_err!("XX000", "not a window function"));
    };
    let n = rows.len();
    let out = arena.alloc_slice_with(n, |_| Datum::Null).map_err(|_| arena_full())?;

    // Assign each row a partition id by comparing PARTITION BY keys.
    let group_of = arena.alloc_slice_with(n, |_| 0usize).map_err(|_| arena_full())?;
    let reps = arena.alloc_slice_with(n, |_| 0usize).map_err(|_| arena_full())?;
    let mut n_groups = 0usize;
    for i in 0..n {
        let mut gid = None;
        for g in 0..n_groups {
            if keys_equal(spec.partition_by, scope, rows, offs, i, reps[g], arena, params, hooks)? {
                gid = Some(g);
                break;
            }
        }
        match gid {
            Some(g) => group_of[i] = g,
            None => {
                reps[n_groups] = i;
                group_of[i] = n_groups;
                n_groups += 1;
            }
        }
    }

    let is_ranking = matches!(*name, "row_number" | "rank" | "dense_rank");
    let is_offset = matches!(*name, "lag" | "lead");

    let part = arena.alloc_slice_with(n, |_| 0usize).map_err(|_| arena_full())?;
    for g in 0..n_groups {
        // Collect this partition's row indices, then sort by ORDER BY.
        let mut m = 0usize;
        for i in 0..n {
            if group_of[i] == g {
                part[m] = i;
                m += 1;
            }
        }
        let ord = &spec.order_by;
        if !ord.is_empty() {
            // Insertion sort (stable) by the ORDER BY keys — partitions are
            // small and this avoids a fallible comparator.
            for x in 1..m {
                let mut y = x;
                while y > 0 {
                    let c = cmp_order(ord, scope, rows, offs, part[y - 1], part[y], arena, params, hooks)?;
                    if c == core::cmp::Ordering::Greater {
                        part.swap(y - 1, y);
                        y -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let p = &part[..m];

        if is_ranking {
            let mut rank = 1i64;
            let mut dense = 1i64;
            for j in 0..m {
                let peer = j > 0
                    && spec.order_by.iter().try_fold(true, |acc, o| {
                        Ok::<bool, SqlError>(acc && {
                            let ra = window_row(scope, rows[p[j - 1]], offs);
                            let va = eval_full(o.expr, arena, params, &ra, hooks)?;
                            let rb = window_row(scope, rows[p[j]], offs);
                            let vb = eval_full(o.expr, arena, params, &rb, hooks)?;
                            match (va.is_null(), vb.is_null()) {
                                (true, true) => true,
                                (true, false) | (false, true) => false,
                                (false, false) => compare_datums(&va, &vb)?.is_eq(),
                            }
                        })
                    })?;
                if j > 0 && !peer {
                    rank = j as i64 + 1;
                    dense += 1;
                }
                out[p[j]] = match *name {
                    "row_number" => Datum::Int8(j as i64 + 1),
                    "rank" => Datum::Int8(rank),
                    _ => Datum::Int8(dense),
                };
            }
        } else if is_offset {
            let sign: isize = if *name == "lag" { -1 } else { 1 };
            let off: isize = if args.len() >= 2 {
                let r = window_row(scope, rows[p[0]], offs);
                match eval_full(args[1], arena, params, &r, hooks)? {
                    Datum::Int4(v) => v as isize,
                    Datum::Int8(v) => v as isize,
                    _ => 1,
                }
            } else {
                1
            };
            for j in 0..m {
                let src = j as isize + sign * off;
                out[p[j]] = if src >= 0 && (src as usize) < m {
                    let r = window_row(scope, rows[p[src as usize]], offs);
                    eval_full(args[0], arena, params, &r, hooks)?
                } else if args.len() >= 3 {
                    let r = window_row(scope, rows[p[j]], offs);
                    eval_full(args[2], arena, params, &r, hooks)?
                } else {
                    Datum::Null
                };
            }
        } else {
            // Aggregate window function. Default frame:
            //  - no ORDER BY: the whole partition (same value for every row);
            //  - with ORDER BY: RANGE UNBOUNDED PRECEDING TO CURRENT ROW, i.e.
            //    a running aggregate where peers (equal ORDER BY keys) share the
            //    value at the end of their peer group.
            if spec.order_by.is_empty() {
                let mut st = AggState::default();
                st.init(node)?;
                for &ri in p {
                    let r = window_row(scope, rows[ri], offs);
                    st.update(node, arena, params, &r, hooks)?;
                }
                let v = st.finish(arena)?;
                for &ri in p {
                    out[ri] = v;
                }
            } else {
                // Peer-group boundaries, then recompute the running aggregate at
                // each boundary and assign it to the whole peer group.
                let mut j = 0usize;
                while j < m {
                    let mut e = j;
                    while e + 1 < m {
                        let same = spec.order_by.iter().try_fold(true, |acc, o| {
                            Ok::<bool, SqlError>(acc && {
                                let ra = window_row(scope, rows[p[e]], offs);
                                let va = eval_full(o.expr, arena, params, &ra, hooks)?;
                                let rb = window_row(scope, rows[p[e + 1]], offs);
                                let vb = eval_full(o.expr, arena, params, &rb, hooks)?;
                                match (va.is_null(), vb.is_null()) {
                                    (true, true) => true,
                                    (true, false) | (false, true) => false,
                                    (false, false) => compare_datums(&va, &vb)?.is_eq(),
                                }
                            })
                        })?;
                        if same {
                            e += 1;
                        } else {
                            break;
                        }
                    }
                    // Frame is p[0..=e]; aggregate and assign to peers p[j..=e].
                    let mut st = AggState::default();
                    st.init(node)?;
                    for &ri in &p[..=e] {
                        let r = window_row(scope, rows[ri], offs);
                        st.update(node, arena, params, &r, hooks)?;
                    }
                    let v = st.finish(arena)?;
                    for &ri in &p[j..=e] {
                        out[ri] = v;
                    }
                    j = e + 1;
                }
            }
        }
    }
    Ok(&*out)
}

/// Compares two rows by a window ORDER BY spec (ASC/DESC, NULLS FIRST/LAST).
#[allow(clippy::too_many_arguments)]
fn cmp_order<'a>(
    ord: &[OrderBy<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    a: usize,
    b: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<core::cmp::Ordering, SqlError> {
    use core::cmp::Ordering;
    for o in ord {
        let ra = window_row(scope, rows[a], offs);
        let va = eval_full(o.expr, arena, params, &ra, hooks)?;
        let rb = window_row(scope, rows[b], offs);
        let vb = eval_full(o.expr, arena, params, &rb, hooks)?;
        let base = match (va.is_null(), vb.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if o.nulls_first { Ordering::Less } else { Ordering::Greater }
            }
            (false, true) => {
                if o.nulls_first { Ordering::Greater } else { Ordering::Less }
            }
            (false, false) => compare_datums(&va, &vb)?,
        };
        let c = if o.descending && !va.is_null() && !vb.is_null() {
            base.reverse()
        } else {
            base
        };
        if c != Ordering::Equal {
            return Ok(c);
        }
    }
    Ok(Ordering::Equal)
}

/// Materializes the post-WHERE source rows, computes each window function, and
/// projects every row with the window values in scope. Returns the (unsorted)
/// projected rows and their ORDER BY sort keys. Shared by the streaming
/// `window_select` and the derived-table / INSERT-source materializer.
#[allow(clippy::type_complexity, clippy::too_many_arguments, clippy::needless_range_loop)]
fn project_window_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    stmt: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(&'a [&'a [Datum<'a>]], &'a [&'a [Datum<'a>]]), SqlError> {
    // Flat-row column offsets per table.
    let mut offs = [0usize; MAX_JOIN_TABLES];
    let mut total = 0usize;
    for t in 0..scope.n {
        offs[t] = total;
        total += scope.defs[t].expect("resolved").n_columns;
    }

    // Pass 1: count source rows.
    let mut count = 0usize;
    scan_source(
        storage, scope, from, txid, stmt.where_clause, arena, params, hooks, None,
        &mut |_| {
            count += 1;
            Ok(true)
        },
    )?;
    // Pass 2: materialize each row's columns flat in the arena.
    let empty: &[Datum] = &[];
    let rows: &mut [&[Datum]] = arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    let mut at = 0usize;
    scan_source(
        storage, scope, from, txid, stmt.where_clause, arena, params, hooks, None,
        &mut |row| {
            let flat = arena
                .alloc_slice_with(total.max(1), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (t, off) in offs.iter().enumerate().take(scope.n) {
                let def = scope.defs[t].expect("resolved");
                let vals = row.values[t].expect("bound");
                for c in 0..def.n_columns {
                    flat[off + c] = if vals.is_empty() { Datum::Null } else { vals[c] };
                }
            }
            rows[at] = &flat[..total];
            at += 1;
            Ok(true)
        },
    )?;
    let rows: &[&[Datum]] = &rows[..count];

    // Compute each window function's per-row values.
    let mut win_vals: [&[Datum]; MAX_WINDOWS] = [empty; MAX_WINDOWS];
    for (wi, &node) in win_nodes.iter().enumerate() {
        win_vals[wi] = compute_window(node, rows, scope, &offs, arena, params, hooks)?;
    }
    let win_ptrs: &[*const Expr] = arena
        .alloc_slice_with(win_nodes.len(), |i| win_nodes[i] as *const Expr)
        .map_err(|_| arena_full())?;

    // Resolve ORDER BY (ordinals → select items).
    let n_order = stmt.order_by.len();
    let mut order_exprs: [Option<&Expr>; MAX_WIN_KEYS] = [None; MAX_WIN_KEYS];
    if n_order > MAX_WIN_KEYS {
        return Err(sql_err!("54023", "ORDER BY list too long"));
    }
    for (k, ob) in stmt.order_by.iter().enumerate() {
        order_exprs[k] = Some(super::exec::resolve_order_expr_pub(ob.expr, stmt.items)?);
    }

    // Project each row (with the window hook) and compute its sort keys.
    let proj_rows: &mut [&[Datum]] =
        arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    let sort_keys: &mut [&[Datum]] =
        arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    for i in 0..count {
        let mut wv = [Datum::Null; MAX_WINDOWS];
        for (w, wval) in win_vals.iter().enumerate().take(win_nodes.len()) {
            wv[w] = wval[i];
        }
        let win_hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: hooks.subs,
            windows: Some((win_ptrs, &wv[..win_nodes.len()])),
            catalog: hooks.catalog, srf_index: hooks.srf_index,
        };
        let jr = window_row(scope, rows[i], &offs);
        let mut projected = [Datum::Null; MAX_PROJ];
        let np = project_row(stmt.items, scope, &jr, arena, params, &win_hooks, &mut projected)?;
        proj_rows[i] = &*arena.alloc_slice_copy(&projected[..np]).map_err(|_| arena_full())?;
        let mut keys = [Datum::Null; MAX_WIN_KEYS];
        for (k, oe) in order_exprs.iter().enumerate().take(n_order) {
            keys[k] = eval_full(oe.expect("set"), arena, params, &jr, &win_hooks)?;
        }
        sort_keys[i] = &*arena.alloc_slice_copy(&keys[..n_order]).map_err(|_| arena_full())?;
    }
    Ok((proj_rows, sort_keys))
}

#[allow(clippy::too_many_arguments)]
fn window_select<'a>(
    storage: &'a Storage,
    txid: u32,
    stmt: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    limit: u64,
    offset: u64,
    resp: &mut Responder,
) -> Outcome {
    if !stmt.group_by.is_empty() || stmt.having.is_some() || stmt.distinct {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "window functions combined with GROUP BY/HAVING/DISTINCT are not supported yet"
        ));
    }
    let (proj_rows, sort_keys) = match project_window_rows(
        storage, txid, stmt, from, scope, win_nodes, hooks, arena, params,
    ) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    let count = proj_rows.len();

    // Sort output rows by the ORDER BY keys.
    let order = match arena.alloc_slice_with(count, |i| i) {
        Ok(o) => o,
        Err(_) => return sql_fail(arena_full()),
    };
    if !stmt.order_by.is_empty() {
        for x in 1..count {
            let mut y = x;
            while y > 0 {
                let c = cmp_key_rows(sort_keys[order[y - 1]], sort_keys[order[y]], stmt.order_by);
                if c == core::cmp::Ordering::Greater {
                    order.swap(y - 1, y);
                    y -= 1;
                } else {
                    break;
                }
            }
        }
    }

    // Emit under OFFSET/LIMIT.
    let mut emitted = 0u64;
    let mut skipped = 0u64;
    for &i in order.iter() {
        if skipped < offset {
            skipped += 1;
            continue;
        }
        if emitted >= limit {
            break;
        }
        resp.data_row(proj_rows[i])?;
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    resp.command_complete(tag.as_str())?;
    sql_ok()
}

/// Compares two precomputed sort-key tuples honoring ASC/DESC + NULLS order.
fn cmp_key_rows(a: &[Datum], b: &[Datum], ord: &[OrderBy]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    for (k, o) in ord.iter().enumerate() {
        let (va, vb) = (&a[k], &b[k]);
        let base = match (va.is_null(), vb.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => if o.nulls_first { Ordering::Less } else { Ordering::Greater },
            (false, true) => if o.nulls_first { Ordering::Greater } else { Ordering::Less },
            (false, false) => compare_datums(va, vb).unwrap_or(Ordering::Equal),
        };
        let c = if o.descending && !va.is_null() && !vb.is_null() { base.reverse() } else { base };
        if c != Ordering::Equal {
            return c;
        }
    }
    Ordering::Equal
}

/// Walks an expression tree collecting subquery nodes.
fn collect_subqueries<'a>(
    expr: &'a Expr<'a>,
    out: &mut [Option<&'a Expr<'a>>; MAX_SUBQUERIES],
    n: &mut usize,
) -> Result<(), SqlError> {
    if matches!(
        expr,
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_)
    ) {
        if out[..*n].iter().any(|e| core::ptr::eq(e.expect("set"), expr)) {
            return Ok(());
        }
        if *n == MAX_SUBQUERIES {
            return Err(sql_err!("54000", "too many subqueries in one query"));
        }
        out[*n] = Some(expr);
        *n += 1;
        // The operand of IN (SELECT ..) may itself contain subqueries.
        if let Expr::InSubquery { operand, .. } = expr {
            collect_subqueries(operand, out, n)?;
        }
        return Ok(());
    }
    walk_children(expr, &mut |child| collect_subqueries(child, out, n))
}

fn walk_children<'a>(
    expr: &'a Expr<'a>,
    f: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match expr {
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => f(operand),
        Expr::Binary { left, right, .. } => {
            f(left)?;
            f(right)
        }
        Expr::Call { args, .. } => {
            for a in *args {
                f(a)?;
            }
            Ok(())
        }
        Expr::InList { operand, list, .. } => {
            f(operand)?;
            for e in *list {
                f(e)?;
            }
            Ok(())
        }
        Expr::Between { operand, low, high, .. } => {
            f(operand)?;
            f(low)?;
            f(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            f(operand)?;
            f(pattern)
        }
        Expr::Case { operand, whens, otherwise } => {
            if let Some(o) = operand {
                f(o)?;
            }
            for (c, r) in *whens {
                f(c)?;
                f(r)?;
            }
            if let Some(o) = otherwise {
                f(o)?;
            }
            Ok(())
        }
        Expr::InSubquery { operand, .. } => f(operand),
        _ => Ok(()),
    }
}

/// Pre-evaluates every (uncorrelated) subquery in the statement and stores
/// the results in the arena for hook-based lookup during evaluation.
#[allow(clippy::too_many_arguments)]
pub fn prepare_subqueries<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let mut nodes: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n = 0;
    for expr in exprs.iter().flatten() {
        collect_subqueries(expr, &mut nodes, &mut n)?;
    }
    eval_subquery_nodes(&nodes[..n], storage, txid, arena, params, depth, outer)
}

/// Evaluates a set of already-collected subquery nodes (scalar, IN, or
/// EXISTS) into arena-backed [`SubqueryValues`] keyed by node identity.
/// EXISTS results are stored as boolean scalars.
#[allow(clippy::too_many_arguments)]
fn eval_subquery_nodes<'a>(
    nodes: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    let mut scalars_tmp: [(*const Expr, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), Datum::Null); MAX_SUBQUERIES];
    let mut lists_tmp: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
    let (mut n_scalars, mut n_lists) = (0, 0);
    for node in nodes.iter().flatten() {
        match node {
            Expr::Subquery(select) => {
                let (values, _, _) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        "21000",
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                let v = values.first().copied().unwrap_or(Datum::Null);
                scalars_tmp[n_scalars] = (*node as *const _, v);
                n_scalars += 1;
            }
            Expr::Exists(select) => {
                let found = subquery_exists(select, storage, txid, arena, params, depth, outer)?;
                scalars_tmp[n_scalars] = (*node as *const _, Datum::Bool(found));
                n_scalars += 1;
            }
            Expr::ArraySubquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars_tmp[n_scalars] = (*node as *const _, v);
                n_scalars += 1;
            }
            Expr::InSubquery { select, .. } => {
                let (values, saw_null, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                lists_tmp[n_lists] = (*node as *const _, values, saw_null, witness);
                n_lists += 1;
            }
            _ => unreachable!("collector only stores subquery nodes"),
        }
    }
    let scalars = arena
        .alloc_slice_copy(&scalars_tmp[..n_scalars])
        .map_err(|_| arena_full())?;
    let lists = arena
        .alloc_slice_copy(&lists_tmp[..n_lists])
        .map_err(|_| arena_full())?;
    Ok(SubqueryValues { scalars, lists })
}

/// Runs a subquery only to determine whether it yields any row (EXISTS).
/// Stops at the first matching row. `outer` supplies correlated columns.
#[allow(clippy::too_many_arguments)]
fn subquery_exists<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<bool, SqlError> {
    if depth == 0 {
        return Err(sql_err!("54001", "subqueries nested too deeply"));
    }
    if let Some(tree) = select.set_body {
        let (vals, _, _) = run_set_subquery(tree, select, storage, txid, arena, params)?;
        return Ok(!vals.is_empty());
    }
    if !select.group_by.is_empty() || select.having.is_some() || select.distinct {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "GROUP BY/HAVING/DISTINCT in EXISTS subqueries is not supported yet"
        ));
    }
    // The projection list of EXISTS is irrelevant (only row presence matters),
    // but its expressions may carry subqueries; prepare them for the scan.
    let mut item_exprs: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    let mut n_items = 0;
    for item in select.items {
        if let SelectItem::Expr { expr, .. } = item {
            item_exprs[n_items] = Some(expr);
            n_items += 1;
        }
    }
    let inner_subs = prepare_subqueries(
        &{
            let mut e = item_exprs;
            // WHERE joins the set of expressions whose subqueries we prepare.
            if n_items < MAX_PROJ {
                e[n_items] = select.where_clause;
            }
            e
        },
        storage,
        txid,
        arena,
        params,
        depth - 1,
        outer,
    )?;
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&inner_subs) , windows: None, catalog: None, srf_index: None };

    let Some(from) = &select.from else {
        // FROM-less SELECT yields exactly one row, so EXISTS is true whenever
        // the (optional) WHERE holds.
        if let Some(w) = select.where_clause {
            let base = Chained { inner: &super::eval::NoColumns, outer };
            return Ok(matches!(eval_full(w, arena, params, &base, &hooks)?, Datum::Bool(true)));
        }
        return Ok(true);
    };
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
    let mut found = false;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        &mut |_| {
            found = true;
            Ok(false) // stop at the first row
        },
    )?;
    Ok(found)
}

/// A chain of query scopes from innermost outward, used to decide whether a
/// subquery references a column belonging to an enclosing query.
struct ScopeChain<'s, 'd> {
    scope: Option<&'s QueryScope<'d>>,
    parent: Option<&'s ScopeChain<'s, 'd>>,
}

impl ScopeChain<'_, '_> {
    /// True if the name resolves at this scope or any enclosing scope.
    fn resolves(&self, q: Option<&str>, name: &str) -> bool {
        if self.scope.is_some_and(|s| s.find_column(q, name).is_ok()) {
            return true;
        }
        self.parent.is_some_and(|p| p.resolves(q, name))
    }
}

/// Whether a top-level subquery node references any column from the enclosing
/// query — i.e. is correlated and must be re-evaluated per outer row. A node
/// unresolvable against its own (and any nested subquery's) scope is treated
/// as correlated; false positives only cost a redundant per-row evaluation.
fn subquery_node_correlated<'a>(node: &'a Expr<'a>, storage: &'a Storage, arena: &'a Arena) -> bool {
    let select = match node {
        Expr::Subquery(s) | Expr::InSubquery { select: s, .. } | Expr::Exists(s)
        | Expr::ArraySubquery(s) => s,
        _ => return false,
    };
    let scope = select
        .from
        .as_ref()
        .and_then(|f| QueryScope::resolve_schema(storage, f, 0, arena).ok());
    let chain = ScopeChain { scope: scope.as_ref(), parent: None };
    select_has_outer_ref(select, &chain, storage, arena)
}

/// Whether any column in this select (WHERE or projection) fails to resolve
/// within `chain` (which already includes this select's own scope).
fn select_has_outer_ref<'a>(
    select: &'a Select<'a>,
    chain: &ScopeChain,
    storage: &'a Storage,
    arena: &'a Arena,
) -> bool {
    if select.where_clause.is_some_and(|w| expr_has_outer_ref(w, chain, storage, arena)) {
        return true;
    }
    select.items.iter().any(|it| match it {
        SelectItem::Expr { expr, .. } => expr_has_outer_ref(expr, chain, storage, arena),
        _ => false,
    })
}

/// Whether any column reference in `expr` resolves only in an enclosing scope
/// beyond `chain`. Nested subqueries push their own scope onto the chain, so a
/// column they provide themselves does not count as an outer reference.
fn expr_has_outer_ref<'a>(
    expr: &'a Expr<'a>,
    chain: &ScopeChain,
    storage: &'a Storage,
    arena: &'a Arena,
) -> bool {
    match expr {
        Expr::Column { qualifier, name } => !chain.resolves(*qualifier, name),
        Expr::Subquery(s) | Expr::Exists(s) => {
            let sscope = s
                .from
                .as_ref()
                .and_then(|f| QueryScope::resolve_schema(storage, f, 0, arena).ok());
            let child = ScopeChain { scope: sscope.as_ref(), parent: Some(chain) };
            select_has_outer_ref(s, &child, storage, arena)
        }
        Expr::InSubquery { operand, select, .. } => {
            let sscope = select
                .from
                .as_ref()
                .and_then(|f| QueryScope::resolve_schema(storage, f, 0, arena).ok());
            let child = ScopeChain { scope: sscope.as_ref(), parent: Some(chain) };
            select_has_outer_ref(select, &child, storage, arena)
                || expr_has_outer_ref(operand, chain, storage, arena)
        }
        _ => {
            let mut found = false;
            let _ = walk_children(expr, &mut |c| {
                if expr_has_outer_ref(c, chain, storage, arena) {
                    found = true;
                }
                Ok(())
            });
            found
        }
    }
}

/// Pre-evaluated uncorrelated subqueries plus the list of correlated subquery
/// nodes that must be re-evaluated per outer row.
struct OuterSubs<'a> {
    base: SubqueryValues<'a, 'a>,
    correlated: &'a [&'a Expr<'a>],
}

/// Splits a query's subqueries into uncorrelated (evaluated once here) and
/// correlated (deferred to per-row evaluation during the scan).
fn prepare_outer_subqueries<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<OuterSubs<'a>, SqlError> {
    let mut nodes: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n = 0;
    for expr in exprs.iter().flatten() {
        collect_subqueries(expr, &mut nodes, &mut n)?;
    }
    let mut uncorr: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n_un = 0;
    let mut corr: [Option<&Expr>; MAX_SUBQUERIES] = [None; MAX_SUBQUERIES];
    let mut n_corr = 0;
    for node in nodes[..n].iter().flatten() {
        if subquery_node_correlated(node, storage, arena) {
            corr[n_corr] = Some(*node);
            n_corr += 1;
        } else {
            uncorr[n_un] = Some(*node);
            n_un += 1;
        }
    }
    let base =
        eval_subquery_nodes(&uncorr[..n_un], storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
    let correlated = arena
        .alloc_slice_with(n_corr, |i| corr[i].expect("set"))
        .map_err(|_| arena_full())?;
    Ok(OuterSubs { base, correlated })
}

/// Builds per-outer-row [`SubqueryValues`] by merging the pre-evaluated
/// uncorrelated results with correlated subqueries evaluated against `outer`.
/// The merged arrays live in caller-provided stack scratch (no arena growth
/// for the bookkeeping; only the subquery result values themselves use the
/// arena).
#[allow(clippy::too_many_arguments)]
fn merge_correlated<'a, 'b>(
    correlated: &[&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    outer: &dyn ColumnLookup<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    scalars: &'b mut [(*const Expr<'a>, Datum<'a>); MAX_SUBQUERIES],
    lists: &'b mut [(*const Expr<'a>, &'a [Datum<'a>], bool, Datum<'a>); MAX_SUBQUERIES],
) -> Result<SubqueryValues<'b, 'a>, SqlError> {
    let mut ns = 0;
    for (p, v) in base.scalars {
        scalars[ns] = (*p, *v);
        ns += 1;
    }
    let mut nl = 0;
    for (p, l, sn, w) in base.lists {
        lists[nl] = (*p, *l, *sn, *w);
        nl += 1;
    }
    for node in correlated {
        match node {
            Expr::Subquery(select) => {
                let (values, _, _) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        "21000",
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                scalars[ns] = (*node as *const _, values.first().copied().unwrap_or(Datum::Null));
                ns += 1;
            }
            Expr::Exists(select) => {
                let found =
                    subquery_exists(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                scalars[ns] = (*node as *const _, Datum::Bool(found));
                ns += 1;
            }
            Expr::ArraySubquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                scalars[ns] = (*node as *const _, build_array_scalar(values, &witness, arena)?);
                ns += 1;
            }
            Expr::InSubquery { select, .. } => {
                let (values, saw_null, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                lists[nl] = (*node as *const _, values, saw_null, witness);
                nl += 1;
            }
            _ => unreachable!("correlated list holds only subquery nodes"),
        }
    }
    Ok(SubqueryValues { scalars: &scalars[..ns], lists: &lists[..nl] })
}

/// A representative zero value of a column type, used to coerce an IN operand
/// to the subquery's result type even over an empty or all-NULL set. Text /
/// bytea / numeric use a text witness, which `coerce_unknown` leaves untouched
/// (no spurious error), matching that these accept an unknown literal as-is.
fn type_witness(ct: ColType) -> Datum<'static> {
    match ct {
        ColType::Bool => Datum::Bool(false),
        ColType::Int2 | ColType::Int4 => Datum::Int4(0),
        ColType::Int8 => Datum::Int8(0),
        ColType::Time => Datum::Time(0),
        ColType::Interval => Datum::Interval(crate::sql::types::Interval { months: 0, days: 0, micros: 0 }),
        ColType::Json => Datum::Json { text: "null", jsonb: false },
        ColType::Jsonb => Datum::Json { text: "null", jsonb: true },
        ColType::Array(elem) => Datum::Array { elem, raw: &[0, 0] },
        ColType::Float4 | ColType::Float8 => Datum::Float8(0.0),
        ColType::Date => Datum::Date(0),
        ColType::Timestamp => Datum::Timestamp(0),
        ColType::Timestamptz => Datum::Timestamptz(0),
        ColType::Uuid => Datum::Uuid([0; 16]),
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Bytea | ColType::Numeric => {
            Datum::Text("")
        }
    }
}

/// The type witness for a subquery's single result column, inferred from its
/// projection expression. Falls back to a text witness on any inference error
/// (harmless — the real evaluation surfaces genuine errors).
fn subquery_witness(item: &Expr, scope: Option<&QueryScope>) -> Datum<'static> {
    let inferred = match scope {
        Some(s) => super::exec::infer_type_res(item, &ScopeCols(s)),
        None => super::exec::infer_type_res(item, &super::exec::NoCols),
    };
    let ct = inferred
        .ok()
        .and_then(|(o, _)| super::exec::coltype_of_oid(o))
        .unwrap_or(ColType::Text);
    type_witness(ct)
}

/// Executes a subquery to a value list: exactly one select item, full
/// WHERE/aggregate support, no grouping/ordering (irrelevant for IN, and a
/// scalar has at most one row). Also returns a type witness for the result
/// column (see [`type_witness`]); scalar callers ignore it.
#[allow(clippy::too_many_arguments)]
fn run_subquery<'a>(
    select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    depth: u32,
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    if depth == 0 {
        return Err(sql_err!("54001", "subqueries nested too deeply"));
    }
    if let Some(tree) = select.set_body {
        return run_set_subquery(tree, select, storage, txid, arena, params);
    }
    if select.items.len() != 1 {
        return Err(sql_err!("42601", "subquery must return exactly one column"));
    }
    // `SELECT *` is accepted when the source has exactly one column (resolved
    // below); until then a placeholder stands in (a wildcard carries no
    // subqueries or aggregates of its own).
    let wildcard = matches!(&select.items[0], SelectItem::Wildcard);
    let item: &Expr = match &select.items[0] {
        SelectItem::Expr { expr, .. } => expr,
        SelectItem::Wildcard => &Expr::Null,
    };
    if !select.group_by.is_empty() || select.having.is_some() || select.distinct {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "GROUP BY/HAVING/DISTINCT in subqueries is not supported yet"
        ));
    }

    // Inner subqueries first.
    let inner_subs = prepare_subqueries(
        &[Some(item), select.where_clause],
        storage,
        txid,
        arena,
        params,
        depth - 1,
        outer,
    )?;
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&inner_subs),
        windows: None, catalog: None, srf_index: None };

    let Some(from) = &select.from else {
        if wildcard {
            return Err(sql_err!("42601", "SELECT * with no tables specified is not valid"));
        }
        // FROM-less: one row (outer columns still visible if correlated).
        let base = Chained { inner: &super::eval::NoColumns, outer };
        let v = eval_full(item, arena, params, &base, &hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((&*out, v.is_null(), subquery_witness(item, None)));
    };
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;

    // `SELECT *` is a single-column subquery only if the source is exactly one
    // column; expand it to that column so the row-value path below applies.
    let item: &Expr = if wildcard {
        if scope.total_columns() != 1 {
            return Err(sql_err!("42601", "subquery must return only one column"));
        }
        let name = scope.defs[0].expect("resolved").columns()[0].name.as_str();
        arena
            .alloc(Expr::Column { qualifier: None, name })
            .map_err(|_| arena_full())?
    } else {
        item
    };

    // Aggregate subquery: one row.
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    collect_aggs(item, &mut agg_nodes, &mut n_aggs)?;
    if n_aggs > 0 {
        let agg_values = fold_aggregates(
            storage,
            &scope,
            from,
            txid,
            select.where_clause,
            &agg_nodes[..n_aggs],
            arena,
            params,
            &hooks,
            outer,
        )?;
        let ptrs = arena
            .alloc_slice_with(n_aggs, |i| agg_nodes[i].0)
            .map_err(|_| arena_full())?;
        let agg_hooks = EvalHooks {
            group: None,
            aggs: Some((&*ptrs, agg_values)),
            subs: hooks.subs,
        windows: None, catalog: None, srf_index: None };
        let base = Chained { inner: &super::eval::NoColumns, outer };
        let v = eval_full(item, arena, params, &base, &agg_hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((&*out, v.is_null(), subquery_witness(item, Some(&scope))));
    }

    // Plain scan: collect item values. Two passes (count then fill).
    let mut count = 0usize;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        &mut |_| {
            count += 1;
            Ok(true)
        },
    )?;
    let out = arena
        .alloc_slice_with(count, |_| Datum::Null)
        .map_err(|_| arena_full())?;
    let mut at = 0usize;
    let mut saw_null = false;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        select.where_clause,
        arena,
        params,
        &hooks,
        outer,
        &mut |row| {
            let cr = Chained { inner: row, outer };
            let v = eval_full(item, arena, params, &cr, &hooks)?;
            if v.is_null() {
                saw_null = true;
            }
            out[at] = v;
            at += 1;
            Ok(true)
        },
    )?;
    Ok((&*out, saw_null, subquery_witness(item, Some(&scope))))
}

/// Runs a set-operation query (UNION / INTERSECT / EXCEPT) in subquery position,
/// yielding its single output column as datums. Mirrors [`set_query`]'s type
/// unification and row combining, then decodes the lone column back to datums so
/// scalar / IN callers can consume them. Correlated columns are not visible to a
/// set-operation body (each leaf is materialized independently); an unresolved
/// reference surfaces loudly as a missing-column error from the leaf itself.
fn run_set_subquery<'a>(
    tree: &'a SetTree<'a>,
    outer_select: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(&'a [Datum<'a>], bool, Datum<'a>), SqlError> {
    let (rows, target, n_cols) = materialize_set_body(storage, txid, tree, arena, params)?;
    if n_cols != 1 {
        return Err(sql_err!("42601", "subquery must return only one column"));
    }
    let offset = super::exec::eval_offset_pub(outer_select.offset, arena, params)?;
    let limit = super::exec::eval_limit_pub(outer_select.limit, arena, params)?;
    let start = (offset as usize).min(rows.len());
    let n = ((rows.len() - start) as u64).min(limit) as usize;
    let mut saw_null = false;
    let out = arena
        .alloc_slice_with(n, |i| {
            let v = super::exec::decode_projected_pub(rows[start + i], 0);
            if v.is_null() {
                saw_null = true;
            }
            v
        })
        .map_err(|_| arena_full())?;
    Ok((&*out, saw_null, type_witness(target[0])))
}

/// Builds the `Datum::Array` for an `ARRAY(subquery)` constructor from the
/// subquery's single-column `values`. The element type comes from the column's
/// type `witness` (so an empty subquery still yields a correctly-typed empty
/// array); each value is coerced to it before encoding.
fn build_array_scalar<'a>(
    values: &[Datum<'a>],
    witness: &Datum<'a>,
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let elem = super::types::ArrElem::from_datum(witness)
        .or_else(|| values.iter().find_map(super::types::ArrElem::from_datum))
        .unwrap_or(super::types::ArrElem::Text);
    let ct = elem.to_coltype();
    let buf = arena
        .alloc_slice_with(values.len(), |i| values[i])
        .map_err(|_| arena_full())?;
    for v in buf.iter_mut() {
        if !v.is_null() {
            *v = super::eval::cast_to(*v, ct, arena)?;
        }
    }
    Ok(Datum::Array { elem, raw: super::array::build(buf, arena)? })
}

/// Streams the source once, folding every aggregate node's state.
/// Returns per-node result datums in the arena.
#[allow(clippy::too_many_arguments)]
fn fold_aggregates<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer_arg: Option<&dyn ColumnLookup<'a>>,
) -> Result<&'a mut [Datum<'a>], SqlError> {
    let mut states = [AggState::default(); MAX_AGGS];
    for (i, (_, node)) in agg_nodes.iter().enumerate() {
        states[i].init(node)?;
    }
    scan_source(
        storage,
        scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        hooks,
        outer_arg,
        &mut |row| {
            let cr = Chained { inner: row, outer: outer_arg };
            for (i, (_, node)) in agg_nodes.iter().enumerate() {
                states[i].update(node, arena, params, &cr, hooks)?;
            }
            Ok(true)
        },
    )?;
    let out = arena
        .alloc_slice_with(agg_nodes.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, state) in states[..agg_nodes.len()].iter_mut().enumerate() {
        out[i] = state.finish(arena)?;
    }
    Ok(out)
}

#[derive(Clone, Copy)]
pub struct AggState<'a> {
    kind: AggKind,
    star: bool,
    count: u64,
    sum_int: i128,
    sum_float: f64,
    sum_numeric: Option<super::numeric::Numeric<'a>>,
    arg_kind: ArgKind,
    best: Option<Datum<'a>>,
    bool_acc: Option<bool>,
    // `agg(DISTINCT x)`: non-null argument values are buffered here during the
    // scan (a doubling arena-backed vector), then sorted, deduplicated, and
    // folded in `finish`. Empty for non-distinct aggregates.
    distinct: bool,
    vals: *mut Datum<'a>,
    vals_len: usize,
    vals_cap: usize,
    // string_agg: the delimiter (captured on first input, for the DISTINCT
    // fold) and a doubling arena-backed byte buffer of the joined output.
    sep: Option<&'a str>,
    str_buf: *mut u8,
    str_len: usize,
    str_cap: usize,
    // string_agg(x ORDER BY k): each row's `[value, keys...]` tuple is buffered
    // self-describing-encoded, then sorted by the key columns and concatenated
    // in `finish`. `ordered` is only set for string_agg (ORDER BY cannot change
    // a commutative aggregate's result).
    ordered: bool,
    ord_spec: &'a [super::ast::OrderBy<'a>],
    ord: *mut &'a [u8],
    ord_len: usize,
    ord_cap: usize,
}

/// The most general numeric class seen among an aggregate's inputs, driving
/// PostgreSQL's result type (sum(int4)->int8, sum(int8)->numeric, avg(int)
/// ->numeric, etc.).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ArgKind {
    None,
    Int4,
    Int8,
    Numeric,
    Float,
}

#[derive(Clone, Copy, PartialEq)]
enum AggKind {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    BoolAnd,
    BoolOr,
    StringAgg,
}

impl Default for AggState<'_> {
    fn default() -> Self {
        Self {
            kind: AggKind::Count,
            star: false,
            count: 0,
            sum_int: 0,
            sum_float: 0.0,
            sum_numeric: None,
            arg_kind: ArgKind::None,
            best: None,
            bool_acc: None,
            distinct: false,
            vals: core::ptr::null_mut(),
            vals_len: 0,
            vals_cap: 0,
            sep: None,
            str_buf: core::ptr::null_mut(),
            str_len: 0,
            str_cap: 0,
            ordered: false,
            ord_spec: &[],
            ord: core::ptr::null_mut(),
            ord_len: 0,
            ord_cap: 0,
        }
    }
}

impl<'a> AggState<'a> {
    fn init(&mut self, node: &'a Expr<'a>) -> Result<(), SqlError> {
        let Expr::Call { name, star, distinct, order_by, .. } = node else {
            return Err(sql_err!("42803", "not an aggregate"));
        };
        self.kind = match *name {
            "count" => AggKind::Count,
            "sum" => AggKind::Sum,
            "avg" => AggKind::Avg,
            "min" => AggKind::Min,
            "max" => AggKind::Max,
            "bool_and" | "every" => AggKind::BoolAnd,
            "bool_or" => AggKind::BoolOr,
            "string_agg" => AggKind::StringAgg,
            other => {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "function {}() is not an aggregate",
                    other
                ))
            }
        };
        self.star = *star;
        self.distinct = *distinct;
        if *distinct && *star {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "DISTINCT is not implemented for count(*)"
            ));
        }
        // ORDER BY only affects string_agg (other aggregates are commutative,
        // so their result is identical regardless of input order).
        if !order_by.is_empty() && self.kind == AggKind::StringAgg {
            if *distinct {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "string_agg with both DISTINCT and ORDER BY is not supported yet"
                ));
            }
            self.ordered = true;
            self.ord_spec = order_by;
        }
        Ok(())
    }

    fn update(
        &mut self,
        node: &Expr<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &impl ColumnLookup<'a>,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<(), SqlError> {
        let Expr::Call { args, .. } = node else {
            unreachable!("validated in init");
        };
        if self.star {
            self.count += 1;
            return Ok(());
        }
        if self.kind == AggKind::StringAgg {
            return self.update_string_agg(args, arena, params, row, hooks);
        }
        let Some(arg) = args.first() else {
            return Err(sql_err!("42803", "aggregate requires an argument"));
        };
        let v = eval_full(arg, arena, params, row, hooks)?;
        if v.is_null() {
            return Ok(());
        }
        // DISTINCT defers folding until finish, so duplicate values can be
        // dropped after the whole group is seen.
        if self.distinct {
            return self.push_distinct(v, arena);
        }
        self.count += 1;
        self.accumulate(v, arena)
    }

    /// Fold one non-null value into the running aggregate (the type-specific
    /// arithmetic shared by the streaming and DISTINCT paths). Callers bump
    /// `count` themselves.
    fn accumulate(&mut self, v: Datum<'a>, arena: &'a Arena) -> Result<(), SqlError> {
        match self.kind {
            AggKind::Count => {}
            AggKind::Sum | AggKind::Avg => match v {
                Datum::Int4(x) => {
                    self.arg_kind = self.arg_kind.max(ArgKind::Int4);
                    self.sum_int += i128::from(x);
                }
                Datum::Int8(x) => {
                    self.arg_kind = self.arg_kind.max(ArgKind::Int8);
                    self.sum_int += i128::from(x);
                }
                Datum::Numeric(n) => {
                    self.arg_kind = self.arg_kind.max(ArgKind::Numeric);
                    let running = self.sum_numeric.unwrap_or(super::numeric::Numeric::ZERO);
                    self.sum_numeric = Some(super::numeric::add(&running, &n, arena)?);
                }
                Datum::Float8(x) => {
                    self.arg_kind = ArgKind::Float;
                    self.sum_float += x;
                }
                other => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "cannot sum {:?}",
                        other
                    ))
                }
            },
            AggKind::Min | AggKind::Max => {
                let replace = match &self.best {
                    None => true,
                    Some(b) => {
                        let ord = compare_datums(&v, b)?;
                        (self.kind == AggKind::Min && ord.is_lt())
                            || (self.kind == AggKind::Max && ord.is_gt())
                    }
                };
                if replace {
                    self.best = Some(v);
                }
            }
            AggKind::BoolAnd | AggKind::BoolOr => {
                let Datum::Bool(x) = v else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "bool_and/bool_or requires boolean arguments"
                    ));
                };
                let acc = self.bool_acc.get_or_insert(matches!(self.kind, AggKind::BoolAnd));
                *acc = if self.kind == AggKind::BoolAnd { *acc && x } else { *acc || x };
            }
            // Only reached through the DISTINCT fold; the streaming path handles
            // string_agg directly (it needs the per-row delimiter).
            AggKind::StringAgg => {
                let Datum::Text(s) = v else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "string_agg requires text arguments"
                    ));
                };
                let sep = self.sep.unwrap_or("");
                self.append_str_elem(sep, s, arena)?;
            }
        }
        Ok(())
    }

    /// string_agg streaming path: evaluate value + delimiter, skip NULL values,
    /// and either buffer the value (DISTINCT, folded later) or append it now.
    fn update_string_agg(
        &mut self,
        args: &[&Expr<'a>],
        arena: &'a Arena,
        params: &[Datum<'a>],
        row: &impl ColumnLookup<'a>,
        hooks: &EvalHooks<'_, 'a>,
    ) -> Result<(), SqlError> {
        if args.len() != 2 {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "string_agg requires exactly two arguments"
            ));
        }
        let val = eval_full(args[0], arena, params, row, hooks)?;
        if val.is_null() {
            return Ok(());
        }
        let Datum::Text(val_str) = val else {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "string_agg value must be text"
            ));
        };
        let sep = eval_full(args[1], arena, params, row, hooks)?;
        let sep_str = match sep {
            Datum::Text(s) => s,
            Datum::Null => "",
            _ => {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "string_agg delimiter must be text"
                ))
            }
        };
        // Stash the first delimiter so the DISTINCT/ORDER BY fold can reuse it.
        if self.sep.is_none() {
            self.sep = Some(sep_str);
        }
        if self.ordered {
            // Buffer `[value, sort-keys...]` to sort and concatenate in finish.
            let mut tuple = [Datum::Null; 1 + MAX_PROJ];
            tuple[0] = Datum::Text(val_str);
            for (i, o) in self.ord_spec.iter().enumerate() {
                tuple[1 + i] = eval_full(o.expr, arena, params, row, hooks)?;
            }
            let enc =
                super::exec::encode_projected_pub(&tuple[..1 + self.ord_spec.len()], arena)?;
            self.push_ordered(enc, arena)?;
            self.count += 1;
            return Ok(());
        }
        if self.distinct {
            return self.push_distinct(Datum::Text(val_str), arena);
        }
        self.append_str_elem(sep_str, val_str, arena)?;
        self.count += 1;
        Ok(())
    }

    /// Append an encoded `[value, keys...]` tuple to the ORDER BY buffer,
    /// growing it (doubling) in the arena when full.
    fn push_ordered(&mut self, enc: &'a [u8], arena: &'a Arena) -> Result<(), SqlError> {
        if self.ord_len == self.ord_cap {
            let new_cap = if self.ord_cap == 0 { 8 } else { self.ord_cap * 2 };
            let empty: &[u8] = &[];
            let fresh = arena
                .alloc_slice_with(new_cap, |_| empty)
                .map_err(|_| arena_full())?;
            if self.ord_len > 0 {
                let old = unsafe { core::slice::from_raw_parts(self.ord, self.ord_len) };
                fresh[..self.ord_len].copy_from_slice(old);
            }
            self.ord = fresh.as_mut_ptr();
            self.ord_cap = new_cap;
        }
        unsafe { self.ord.add(self.ord_len).write(enc) };
        self.ord_len += 1;
        Ok(())
    }

    /// Append `val` to the string_agg buffer, prefixing `sep` for every element
    /// after the first (first = buffer still empty).
    fn append_str_elem(&mut self, sep: &str, val: &str, arena: &'a Arena) -> Result<(), SqlError> {
        if self.str_len > 0 {
            self.push_bytes(sep.as_bytes(), arena)?;
        }
        self.push_bytes(val.as_bytes(), arena)?;
        Ok(())
    }

    /// Append raw bytes to the string_agg buffer, growing it (doubling) in the
    /// arena when it would overflow.
    fn push_bytes(&mut self, src: &[u8], arena: &'a Arena) -> Result<(), SqlError> {
        let need = self.str_len + src.len();
        if need > self.str_cap {
            let mut new_cap = if self.str_cap == 0 { 16 } else { self.str_cap * 2 };
            while new_cap < need {
                new_cap *= 2;
            }
            let fresh = arena
                .alloc_slice_with(new_cap, |_| 0u8)
                .map_err(|_| arena_full())?;
            if self.str_len > 0 {
                let old = unsafe { core::slice::from_raw_parts(self.str_buf, self.str_len) };
                fresh[..self.str_len].copy_from_slice(old);
            }
            self.str_buf = fresh.as_mut_ptr();
            self.str_cap = new_cap;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.str_buf.add(self.str_len), src.len());
        }
        self.str_len += src.len();
        Ok(())
    }

    /// Append a non-null value to the DISTINCT buffer, growing it (doubling)
    /// in the arena when full. The prior region becomes dead bump-arena space.
    fn push_distinct(&mut self, v: Datum<'a>, arena: &'a Arena) -> Result<(), SqlError> {
        if self.vals_len == self.vals_cap {
            let new_cap = if self.vals_cap == 0 { 8 } else { self.vals_cap * 2 };
            let fresh = arena
                .alloc_slice_with(new_cap, |_| Datum::Null)
                .map_err(|_| arena_full())?;
            if self.vals_len > 0 {
                let old = unsafe { core::slice::from_raw_parts(self.vals, self.vals_len) };
                fresh[..self.vals_len].copy_from_slice(old);
            }
            self.vals = fresh.as_mut_ptr();
            self.vals_cap = new_cap;
        }
        unsafe { self.vals.add(self.vals_len).write(v) };
        self.vals_len += 1;
        Ok(())
    }

    /// Sort the DISTINCT buffer, drop adjacent duplicates, and fold the unique
    /// values through `accumulate` (bumping `count` per unique value). A no-op
    /// for non-distinct aggregates.
    fn fold_distinct(&mut self, arena: &'a Arena) -> Result<(), SqlError> {
        if !self.distinct || self.vals_len == 0 {
            return Ok(());
        }
        let vals = unsafe { core::slice::from_raw_parts_mut(self.vals, self.vals_len) };
        let mut cmp_err: Option<SqlError> = None;
        vals.sort_unstable_by(|a, b| match compare_datums(a, b) {
            Ok(o) => o,
            Err(e) => {
                if cmp_err.is_none() {
                    cmp_err = Some(e);
                }
                core::cmp::Ordering::Equal
            }
        });
        if let Some(e) = cmp_err {
            return Err(e);
        }
        let mut prev: Option<Datum<'a>> = None;
        for &v in vals.iter() {
            let fresh = match prev {
                None => true,
                Some(p) => !compare_datums(&p, &v)?.is_eq(),
            };
            if fresh {
                self.count += 1;
                self.accumulate(v, arena)?;
                prev = Some(v);
            }
        }
        Ok(())
    }

    /// string_agg(x ORDER BY k): sort the buffered `[value, keys...]` tuples by
    /// the key columns (honoring ASC/DESC and NULLS placement) and concatenate
    /// the value column into the output buffer.
    fn fold_ordered(&mut self, arena: &'a Arena) -> Result<(), SqlError> {
        if !self.ordered || self.ord_len == 0 {
            return Ok(());
        }
        let rows = unsafe { core::slice::from_raw_parts_mut(self.ord, self.ord_len) };
        let spec = self.ord_spec;
        let mut cmp_err: Option<SqlError> = None;
        rows.sort_unstable_by(|a, b| {
            use core::cmp::Ordering;
            for (k, o) in spec.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, 1 + k);
                let kb = super::exec::decode_projected_pub(b, 1 + k);
                let ord = match (ka.is_null(), kb.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        if o.nulls_first { Ordering::Less } else { Ordering::Greater }
                    }
                    (false, true) => {
                        if o.nulls_first { Ordering::Greater } else { Ordering::Less }
                    }
                    (false, false) => match compare_datums(&ka, &kb) {
                        Ok(c) => if o.descending { c.reverse() } else { c },
                        Err(e) => {
                            if cmp_err.is_none() {
                                cmp_err = Some(e);
                            }
                            Ordering::Equal
                        }
                    },
                };
                if !ord.is_eq() {
                    return ord;
                }
            }
            Ordering::Equal
        });
        if let Some(e) = cmp_err {
            return Err(e);
        }
        let sep = self.sep.unwrap_or("");
        for &row in rows.iter() {
            let Datum::Text(s) = super::exec::decode_projected_pub(row, 0) else {
                return Err(sql_err!(
                    sqlstate::DATATYPE_MISMATCH,
                    "string_agg value must be text"
                ));
            };
            self.append_str_elem(sep, s, arena)?;
        }
        Ok(())
    }

    fn finish(&mut self, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        use super::numeric::{self as num, Numeric};
        self.fold_distinct(arena)?;
        self.fold_ordered(arena)?;
        Ok(match self.kind {
            AggKind::Count => Datum::Int8(self.count as i64),
            AggKind::Min | AggKind::Max => self.best.unwrap_or(Datum::Null),
            _ if self.count == 0 => Datum::Null,
            // SUM result type: int4->int8, int8->numeric, numeric->numeric,
            // float8->float8 (PostgreSQL's aggregate signatures).
            AggKind::Sum => match self.arg_kind {
                ArgKind::Float => Datum::Float8(self.sum_float),
                ArgKind::Int4 => Datum::Int8(
                    i64::try_from(self.sum_int)
                        .map_err(|_| sql_err!(sqlstate::NUMERIC_OUT_OF_RANGE, "bigint out of range"))?,
                ),
                ArgKind::Int8 => Datum::Numeric(Numeric::from_i128(self.sum_int, arena)?),
                ArgKind::Numeric => {
                    Datum::Numeric(self.sum_numeric.unwrap_or(Numeric::ZERO))
                }
                ArgKind::None => Datum::Null,
            },
            // AVG: numeric for int/int8/numeric, float8 for float8.
            AggKind::Avg => match self.arg_kind {
                ArgKind::Float => Datum::Float8(self.sum_float / self.count as f64),
                ArgKind::Int4 | ArgKind::Int8 => {
                    let sum = Numeric::from_i128(self.sum_int, arena)?;
                    let cnt = Numeric::from_i64(self.count as i64, arena)?;
                    Datum::Numeric(num::div(&sum, &cnt, arena)?)
                }
                ArgKind::Numeric => {
                    let sum = self.sum_numeric.unwrap_or(Numeric::ZERO);
                    let cnt = Numeric::from_i64(self.count as i64, arena)?;
                    Datum::Numeric(num::div(&sum, &cnt, arena)?)
                }
                ArgKind::None => Datum::Null,
            },
            AggKind::BoolAnd | AggKind::BoolOr => match self.bool_acc {
                Some(v) => Datum::Bool(v),
                None => Datum::Null,
            },
            AggKind::StringAgg => {
                let bytes = unsafe { core::slice::from_raw_parts(self.str_buf, self.str_len) };
                Datum::Text(unsafe { core::str::from_utf8_unchecked(bytes) })
            }
        })
    }
}

/// The SELECT entry point (FROM present; FROM-less selects stay in the
/// engine).
/// Surfaces plan-time constant errors (e.g. `1/0`) across every expression
/// of a SELECT, matching PostgreSQL's constant folding.
pub fn check_select_constants<'a>(stmt: &Select<'a>, arena: &'a Arena) -> Result<(), SqlError> {
    for item in stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            super::eval::check_constant_errors(expr, arena)?;
        }
    }
    if let Some(w) = stmt.where_clause {
        super::eval::check_constant_errors(w, arena)?;
    }
    for g in stmt.group_by {
        super::eval::check_constant_errors(g, arena)?;
    }
    if let Some(h) = stmt.having {
        super::eval::check_constant_errors(h, arena)?;
    }
    for ob in stmt.order_by {
        super::eval::check_constant_errors(ob.expr, arena)?;
    }
    Ok(())
}

pub fn select_query<'a>(
    storage: &'a Storage,
    txid: u32,
    stmt: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    resp: &mut Responder,
) -> Outcome {
    let from = stmt.from.as_ref().expect("FROM-less handled by caller");
    // Catalog relations (pg_catalog / information_schema) are synthesized and
    // registered as derived tables by resolve_exec, so they flow through the
    // general executor — joins, subqueries, aggregates, and ORDER BY included.
    let scope = match QueryScope::resolve_exec(storage, from, txid, arena, params) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };

    // Subqueries first (uncorrelated, evaluated once).
    let mut sub_exprs: [Option<&Expr>; 4 + super::parser::MAX_LIST] =
        [None; 4 + super::parser::MAX_LIST];
    sub_exprs[0] = stmt.where_clause;
    sub_exprs[1] = stmt.having;
    for (i, item) in stmt.items.iter().enumerate() {
        if let SelectItem::Expr { expr, .. } = item {
            sub_exprs[4 + i] = Some(expr);
        }
    }
    // Uncorrelated subqueries are evaluated once; correlated ones are deferred
    // and re-evaluated per outer row during the scan.
    let outer_subs = match prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let correlated = outer_subs.correlated;
    let catalog = StorageCatalog { storage };
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&outer_subs.base),
        windows: None,
        catalog: Some(&catalog), srf_index: None,
    };

    // Plan-time type analysis: validate operator/aggregate types across every
    // clause so incompatible types error before scanning (matching
    // PostgreSQL), not only when a row reaches them. SELECT items are also
    // type-checked by describe below.
    {
        let cols = ScopeCols(&scope);
        let check = |e: &Expr| -> Result<(), SqlError> {
            super::exec::infer_type_res(e, &cols).map(|_| ())
        };
        let analyze = || -> Result<(), SqlError> {
            // SELECT-list items first: PostgreSQL analyzes types before it folds
            // constants, so an invalid aggregate/operator (e.g. `min(boolean)`)
            // errors ahead of a constant division elsewhere in the query.
            for item in stmt.items {
                if let SelectItem::Expr { expr, .. } = item {
                    check(expr)?;
                }
            }
            if let Some(w) = stmt.where_clause {
                check(w)?;
            }
            for g in stmt.group_by {
                check(g)?;
            }
            if let Some(h) = stmt.having {
                check(h)?;
            }
            for ob in stmt.order_by {
                check(super::exec::resolve_order_expr_pub(ob.expr, stmt.items)?)?;
            }
            Ok(())
        };
        if let Err(e) = analyze() {
            return sql_fail(e);
        }
    }

    // Constant folding runs after type analysis, matching PostgreSQL's
    // analyze-then-plan order: `min(boolean)` errors before `1/0` folds.
    if let Err(e) = check_select_constants(stmt, arena) {
        return sql_fail(e);
    }

    // Result description.
    let mut cols = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_scope_items(stmt.items, &scope, &mut cols) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    resp.row_description(&cols[..n_cols])?;

    let limit = match super::exec::eval_limit_pub(stmt.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(stmt.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };

    // LIMIT 0 returns no rows without scanning or projecting anything, as
    // PostgreSQL does — so a per-row error in an unreturned row does not
    // surface (constant errors already surfaced via the plan-time check).
    if limit == 0 {
        resp.command_complete("SELECT 0")?;
        return sql_ok();
    }

    // Window functions? They run over materialized rows before ORDER BY/LIMIT.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && let Err(e) = collect_windows(expr, &mut win_nodes, &mut n_win)
        {
            return sql_fail(e);
        }
    }
    if n_win > 0 {
        if !correlated.is_empty() {
            return sql_fail(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "correlated subqueries with window functions are not supported yet"
            ));
        }
        return window_select(
            storage, txid, stmt, from, &scope, &win_nodes[..n_win], &hooks, arena, params,
            limit, offset, resp,
        );
    }

    // Aggregates / GROUP BY?
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in stmt.items {
        if let SelectItem::Expr { expr, .. } = item
            && let Err(e) = collect_aggs(expr, &mut agg_nodes, &mut n_aggs) {
                return sql_fail(e);
            }
    }
    if let Some(h) = stmt.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs) {
            return sql_fail(e);
        }
    if n_aggs > 0 || !stmt.group_by.is_empty() {
        if !correlated.is_empty() {
            return sql_fail(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "correlated subqueries with GROUP BY or aggregates are not supported yet"
            ));
        }
        return grouped_select(
            storage,
            &scope,
            from,
            txid,
            stmt,
            &agg_nodes[..n_aggs],
            arena,
            params,
            &hooks,
            limit,
            offset,
            resp,
        );
    }

    let needs_materialize = stmt.distinct || !stmt.order_by.is_empty();
    if !needs_materialize {
        // Stream.
        let mut emitted = 0u64;
        let mut skipped = 0u64;
        let mut wire_full = false;
        let mut wire_result: Result<(), WireFull> = Ok(());
        // With correlated subqueries, WHERE is applied per row against merged
        // hooks (which include the correlated results); otherwise the scan
        // applies WHERE directly for the common, faster path.
        let where_in_scan = if correlated.is_empty() { stmt.where_clause } else { None };
        // A set-returning `_pg_expandarray(arr)` expands each row into one output
        // row per array element.
        let srf_arg = find_expandarray_arg(stmt.items);
        let scan = scan_source(
            storage,
            &scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            &hooks,
            None,
            &mut |row| {
                if emitted >= limit {
                    return Ok(false);
                }
                // Per-row hooks for correlated subqueries; then WHERE.
                let mut sc: [(*const Expr, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_owned;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    &hooks
                } else {
                    row_subs = merge_correlated(
                        correlated, &outer_subs.base, row, storage, txid, arena, params,
                        &mut sc, &mut ls,
                    )?;
                    row_hooks_owned =
                        EvalHooks { group: None, aggs: None, subs: Some(&row_subs) , windows: None, catalog: None, srf_index: None };
                    &row_hooks_owned
                };
                if !correlated.is_empty()
                    && let Some(w) = stmt.where_clause {
                        match eval_full(w, arena, params, row, row_hooks)? {
                            Datum::Bool(true) => {}
                            Datum::Bool(false) | Datum::Null => return Ok(true),
                            _ => return Err(sql_err!(
                                sqlstate::DATATYPE_MISMATCH,
                                "argument of WHERE must be type boolean"
                            )),
                        }
                    }
                // Number of output rows this source row yields (1, unless an
                // `_pg_expandarray` expands it per array element).
                let count = match srf_arg {
                    None => 1,
                    Some(arg) => match eval_full(arg, arena, params, row, row_hooks)? {
                        Datum::Array { raw, .. } => super::array::len(raw),
                        Datum::Null => 0,
                        _ => 1,
                    },
                };
                for k in 1..=count {
                    if emitted >= limit {
                        break;
                    }
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    let srf_hooks;
                    let use_hooks: &EvalHooks = if srf_arg.is_some() {
                        srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        &srf_hooks
                    } else {
                        row_hooks
                    };
                    let mut projected = [Datum::Null; MAX_PROJ];
                    let n = project_row(stmt.items, &scope, row, arena, params, use_hooks, &mut projected)?;
                    if let Err(w) = resp.data_row(&projected[..n]) {
                        wire_full = true;
                        wire_result = Err(w);
                        return Ok(false);
                    }
                    emitted += 1;
                }
                Ok(true)
            },
        );
        if wire_full {
            return Err(WireFull);
        }
        if let Err(e) = scan {
            return sql_fail(e);
        }
        let tag = stack_format!(48, "SELECT {}", emitted);
        resp.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // A set-returning function combined with a top-level DISTINCT/ORDER BY is
    // not supported directly; wrapping it in a subquery (as JDBC does) routes it
    // through the materializer, which does expand it.
    if find_expandarray_arg(stmt.items).is_some() {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "a set-returning function with a top-level DISTINCT/ORDER BY is not supported; \
             wrap it in a subquery"
        ));
    }
    // Materialize: visible columns + hidden ORDER BY keys.
    materialized_select(
        storage, &scope, from, txid, stmt, arena, params, &hooks, correlated, &outer_subs.base,
        limit, offset, resp,
    )
}

/// Evaluates a WHERE/HAVING predicate against a row, returning whether the row
/// passes (NULL and FALSE both filter it out); errors on a non-boolean result.
const MAX_CONJUNCTS: usize = 32;

/// The highest 0-based table index in `scope` whose column this expression
/// references — i.e. the join level at which the expression becomes fully
/// bound. `None` if it references a column outside `scope` (a correlated/outer
/// reference), a subquery, an aggregate, or any construct this analysis does
/// not fully cover, in which case the conjunct is left for the final WHERE.
fn expr_max_table(expr: &Expr, scope: &QueryScope) -> Option<usize> {
    use Expr::*;
    match expr {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | Param(_) => Some(0),
        Column { qualifier, name } => scope.find_column(*qualifier, name).ok().map(|(t, _)| t),
        Unary { operand, .. } | IsNull { operand, .. } | Cast { operand, .. } => {
            expr_max_table(operand, scope)
        }
        Binary { left, right, .. } => {
            Some(expr_max_table(left, scope)?.max(expr_max_table(right, scope)?))
        }
        Between { operand, low, high, .. } => Some(
            expr_max_table(operand, scope)?
                .max(expr_max_table(low, scope)?)
                .max(expr_max_table(high, scope)?),
        ),
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            Some(expr_max_table(operand, scope)?.max(expr_max_table(pattern, scope)?))
        }
        InList { operand, list, negated: _ } => {
            let mut mx = expr_max_table(operand, scope)?;
            for e in *list {
                mx = mx.max(expr_max_table(e, scope)?);
            }
            Some(mx)
        }
        // Pure scalar function calls (not aggregates/window functions).
        Call { args, over: None, .. } if !expr.is_aggregate() => {
            let mut mx = 0;
            for a in *args {
                mx = mx.max(expr_max_table(a, scope)?);
            }
            Some(mx)
        }
        // Subqueries, aggregates, window/CASE/array/field/subscript: not pushed.
        _ => None,
    }
}

/// Evaluates one WHERE conjunct to a filter decision (NULL and FALSE both
/// exclude the row; a non-boolean is a type error).
fn conjunct_passes<'a>(
    e: &Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    match eval_full(e, arena, params, row, hooks)? {
        Datum::Bool(true) => Ok(true),
        Datum::Bool(false) | Datum::Null => Ok(false),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean"
        )),
    }
}

/// Flattens a top-level `AND` chain into `out`, returning the count, or `None`
/// if it would overflow (caller then evaluates the predicate whole).
fn flatten_and<'e, 'a>(e: &'e Expr<'a>, out: &mut [&'e Expr<'a>], n: &mut usize) -> bool {
    if let Expr::Binary { op: super::ast::BinaryOp::And, left, right } = e {
        return flatten_and(left, out, n) && flatten_and(right, out, n);
    }
    if *n == out.len() {
        return false;
    }
    out[*n] = e;
    *n += 1;
    true
}

/// Whether an expression cannot raise a runtime error (so evaluating it early
/// as a WHERE filter is always safe). Conservative: any arithmetic (which can
/// divide by zero or overflow), cast, function call, CASE, or subquery counts
/// as potentially-erroring.
fn is_error_safe(e: &Expr) -> bool {
    use super::ast::{BinaryOp::*, UnaryOp};
    // A constant subexpression cannot raise a *runtime* error: PostgreSQL folds
    // it at plan time and `check_constant_errors` surfaces any error eagerly
    // there, so by the time a row is filtered it is known good. This lets a
    // constant-false conjunct (e.g. `-2.25 <> -2.25`, whose unary minus would
    // otherwise mark it unsafe) filter the row before an erroring sibling runs.
    if e.is_constant() {
        return true;
    }
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
        | Expr::Str(_) | Expr::Column { .. } | Expr::Param(_) | Expr::DefaultMarker => true,
        Expr::Binary { op, left, right } => match op {
            Add | Sub | Mul | Div | Mod => false,
            _ => is_error_safe(left) && is_error_safe(right),
        },
        Expr::Unary { op, operand } => matches!(op, UnaryOp::Not) && is_error_safe(operand),
        Expr::IsNull { operand, .. } => is_error_safe(operand),
        Expr::InList { operand, list, .. } => {
            is_error_safe(operand) && list.iter().all(|e| is_error_safe(e))
        }
        Expr::Between { operand, low, high, .. } => {
            is_error_safe(operand) && is_error_safe(low) && is_error_safe(high)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => is_error_safe(operand) && is_error_safe(pattern),
        _ => false,
    }
}

/// Evaluates a WHERE predicate, short-circuiting a top-level AND chain
/// left-to-right. The conjuncts are already in PostgreSQL's cost order — the
/// scan reorders them once via [`reorder_qual`] before iterating rows — so a
/// cheap filtering conjunct runs before a costlier erroring one (and a cheap
/// erroring conjunct before a costlier filtering one), reproducing PostgreSQL's
/// error timing without re-sorting per row.
fn where_passes<'e, 'a>(
    predicate: &'e Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &impl ColumnLookup<'a>,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<bool, SqlError> {
    let mut conjuncts: [&'e Expr<'a>; MAX_CONJUNCTS] = [predicate; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(predicate, &mut conjuncts, &mut n) || n <= 1 {
        return conjunct_passes(predicate, arena, params, row, hooks);
    }
    for &c in &conjuncts[..n] {
        if !conjunct_passes(c, arena, params, row, hooks)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Folds `col IS NOT NULL` to TRUE and `col IS NULL` to FALSE for a column with
/// a NOT NULL constraint, as PostgreSQL does using the constraint — so
/// `WHERE x/0 = 1 OR id IS NOT NULL` (id NOT NULL) drops the erroring branch.
/// Rewrites only the boolean spine (AND/OR/NOT/IS NULL); other nodes pass
/// through, since an IS NULL test appears as a boolean operand.
fn fold_null<'a>(
    e: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    use super::ast::{BinaryOp, UnaryOp};
    match e {
        Expr::IsNull { operand: Expr::Column { qualifier, name }, negated }
            if scope
                .find_column(*qualifier, name)
                .ok()
                .and_then(|(t, c)| scope.defs[t].map(|d| d.columns()[c].not_null))
                .unwrap_or(false) =>
        {
            Ok(&*arena.alloc(Expr::Bool(*negated)).map_err(|_| arena_full())?)
        }
        Expr::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let (l, r) = (fold_null(left, scope, arena)?, fold_null(right, scope, arena)?);
            if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Binary { op: *op, left: l, right: r })
                    .map_err(|_| arena_full())?)
            }
        }
        Expr::Unary { op: UnaryOp::Not, operand } => {
            let o = fold_null(operand, scope, arena)?;
            if core::ptr::eq(o, *operand) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Unary { op: UnaryOp::Not, operand: o })
                    .map_err(|_| arena_full())?)
            }
        }
        _ => Ok(e),
    }
}

/// Reorders a WHERE predicate's top-level AND conjuncts by PostgreSQL's
/// `order_qual_clauses` cost (cheapest first, stably), returning a rebuilt
/// left-deep AND. Done once per scan (not per row), so it can afford the
/// type-aware `qual_cost`. Constants and non-AND predicates pass through
/// unchanged.
fn reorder_qual<'a>(
    pred: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let mut conj: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(pred, &mut conj, &mut n) || n <= 1 {
        return Ok(pred);
    }
    let cols = ScopeCols(scope);
    let mut cost = [0u32; MAX_CONJUNCTS];
    for (i, c) in conj[..n].iter().enumerate() {
        cost[i] = qual_cost(c, &cols);
    }
    let mut order = [0usize; MAX_CONJUNCTS];
    for (i, slot) in order[..n].iter_mut().enumerate() {
        *slot = i;
    }
    for i in 1..n {
        let mut j = i;
        while j > 0 && cost[order[j - 1]] > cost[order[j]] {
            order.swap(j - 1, j);
            j -= 1;
        }
    }
    // Rebuild a left-deep AND in cost order.
    let mut acc = conj[order[0]];
    for &i in &order[1..n] {
        acc = arena
            .alloc(Expr::Binary { op: super::ast::BinaryOp::And, left: acc, right: conj[i] })
            .map_err(|_| arena_full())?;
    }
    Ok(acc)
}

/// Per-tuple evaluation cost of a qual expression, approximating PostgreSQL's
/// `cost_qual_eval` closely enough to reproduce its clause ordering: each
/// operator, comparison, function, and cast counts one unit; the boolean
/// connectives AND/OR/NOT are control flow and cost nothing; subqueries
/// dominate. Only relative order matters.
fn qual_cost(e: &Expr, cols: &dyn super::exec::ColTypeResolver) -> u32 {
    use super::ast::{BinaryOp, UnaryOp};
    // PostgreSQL folds a constant subexpression to a single Const at plan time,
    // so it costs nothing at scan time and is evaluated first.
    if e.is_constant() {
        return 0;
    }
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_)
        | Expr::Str(_) | Expr::Column { .. } | Expr::Param(_) | Expr::DefaultMarker => 0,
        Expr::Binary { op: BinaryOp::And | BinaryOp::Or, left, right } => {
            qual_cost(left, cols) + qual_cost(right, cols)
        }
        Expr::Binary { op, left, right } => {
            // A comparison that mixes a *runtime* integer side with a
            // float/numeric side widens the integer with a cast, which
            // PostgreSQL counts (`(b % 0)::numeric < 0.21` costs more than the
            // int-only `100 = a % id`); a constant int operand is folded and
            // cast for free.
            let cast = if matches!(op, BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt
                | BinaryOp::GtEq | BinaryOp::Eq | BinaryOp::NotEq)
                && widening_cast(left, right, cols)
            {
                1
            } else {
                0
            };
            1 + cast + qual_cost(left, cols) + qual_cost(right, cols)
        }
        Expr::Unary { op: UnaryOp::Not, operand } => qual_cost(operand, cols),
        Expr::Unary { operand, .. } => 1 + qual_cost(operand, cols),
        Expr::IsNull { operand, .. } => 1 + qual_cost(operand, cols),
        Expr::Cast { operand, .. } => 1 + qual_cost(operand, cols),
        Expr::Field { base, .. } => qual_cost(base, cols),
        Expr::Subscript { base, index } => 1 + qual_cost(base, cols) + qual_cost(index, cols),
        Expr::InList { operand, list, .. } => {
            // PostgreSQL expands `x IN (a, b, ...)` to `x=a OR x=b OR ...`, one
            // comparison per element, so the cost grows with the list length.
            list.len() as u32
                + qual_cost(operand, cols)
                + list.iter().map(|e| qual_cost(e, cols)).sum::<u32>()
        }
        Expr::Between { operand, low, high, .. } => {
            2 + qual_cost(operand, cols) + qual_cost(low, cols) + qual_cost(high, cols)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            1 + qual_cost(operand, cols) + qual_cost(pattern, cols)
        }
        Expr::AnyAll { operand, array, .. } => {
            1 + qual_cost(operand, cols) + qual_cost(array, cols)
        }
        Expr::Call { args, .. } => 1 + args.iter().map(|e| qual_cost(e, cols)).sum::<u32>(),
        Expr::Array(elems) => elems.iter().map(|e| qual_cost(e, cols)).sum::<u32>(),
        Expr::Case { operand, whens, otherwise } => {
            let mut c = operand.map_or(0, |o| qual_cost(o, cols));
            for (w, t) in *whens {
                c += 1 + qual_cost(w, cols) + qual_cost(t, cols);
            }
            c + otherwise.map_or(0, |o| qual_cost(o, cols))
        }
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_)
        | Expr::ArraySubquery(_) => 1000,
    }
}

/// Whether a comparison of `l` and `r` widens a runtime integer operand to
/// float/numeric (a cast PostgreSQL charges). True when one side is an integer
/// expression that is not a compile-time constant and the other side resolves
/// to float/numeric.
fn widening_cast(l: &Expr, r: &Expr, cols: &dyn super::exec::ColTypeResolver) -> bool {
    use super::exec::infer_type_res;
    use super::types::oid;
    let ty = |e: &Expr| infer_type_res(e, cols).map(|(o, _)| o).unwrap_or(oid::UNKNOWN);
    let wide = |o: i32| matches!(o, oid::FLOAT8 | oid::FLOAT4 | oid::NUMERIC);
    let narrow = |o: i32| matches!(o, oid::INT2 | oid::INT4 | oid::INT8);
    let (lt, rt) = (ty(l), ty(r));
    (narrow(lt) && !l.is_constant() && wide(rt)) || (narrow(rt) && !r.is_constant() && wide(lt))
}

const MAX_SET_LEAVES: usize = 32;

/// Executes a set-operation query (UNION / INTERSECT / EXCEPT). Each SELECT
/// leaf is materialized to self-describing rows coerced to the columns' common
/// type; the operators combine those multisets; then the trailing ORDER BY /
/// LIMIT / OFFSET apply to the whole result. Grouped/DISTINCT/aggregate leaves
/// are rejected loudly (they flow through `select_into_rows`).
pub fn set_query<'a>(
    storage: &'a Storage,
    txid: u32,
    q: &'a SetQuery<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    resp: &mut Responder,
) -> Outcome {
    // Column names + types from the first leaf, unified across every leaf.
    let mut cols = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_set_body(storage, q.body, txid, &mut cols, arena) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let mut target = [ColType::Bool; MAX_PROJ];
    for (c, col) in cols[..n_cols].iter().enumerate() {
        target[c] = super::exec::coltype_of_oid(col.type_oid).unwrap_or(ColType::Text);
    }

    // Materialize and combine the tree.
    let rows = match eval_set_tree(q.body, storage, txid, arena, params, &target[..n_cols]) {
        Ok(r) => r,
        Err(e) => return sql_fail(e),
    };

    // ORDER BY (by output column position or name), then LIMIT/OFFSET.
    if let Err(e) = sort_set_rows(rows, q.order_by, &cols[..n_cols]) {
        return sql_fail(e);
    }
    let limit = match super::exec::eval_limit_pub(q.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(q.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };

    resp.row_description(&cols[..n_cols])?;
    let mut emitted = 0u64;
    for (i, row) in rows.iter().enumerate() {
        if (i as u64) < offset {
            continue;
        }
        if emitted >= limit {
            break;
        }
        let mut out = [Datum::Null; MAX_PROJ];
        for (c, slot) in out[..n_cols].iter_mut().enumerate() {
            *slot = super::exec::decode_projected_pub(row, c);
        }
        if resp.data_row(&out[..n_cols]).is_err() {
            return Err(WireFull);
        }
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    resp.command_complete(tag.as_str())?;
    sql_ok()
}

/// Walks a set tree collecting its SELECT leaves left-to-right.
fn collect_set_leaves<'a>(
    tree: &'a SetTree<'a>,
    out: &mut [Option<&'a Select<'a>>; MAX_SET_LEAVES],
    n: &mut usize,
) -> Result<(), SqlError> {
    match tree {
        SetTree::Select(s) => {
            if *n == MAX_SET_LEAVES {
                return Err(sql_err!("54000", "too many set-operation branches"));
            }
            out[*n] = Some(s);
            *n += 1;
            Ok(())
        }
        SetTree::Op { left, right, .. } => {
            collect_set_leaves(left, out, n)?;
            collect_set_leaves(right, out, n)
        }
    }
}

/// Column descriptions of a set-operation leaf (FROM-less or table-backed).
fn describe_leaf<'a>(
    storage: &'a Storage,
    s: &'a Select<'a>,
    txid: u32,
    cols: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    match &s.from {
        None => super::exec::describe_items(s.items, None, cols),
        Some(from) => {
            let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
            describe_scope_items(s.items, &scope, cols)
        }
    }
}

/// The common type of two set-operation columns: equal types, the numeric
/// tower, or (else) an error signalled by None.
fn unify_set_type(a: ColType, b: ColType) -> Option<ColType> {
    if a == b {
        return Some(a);
    }
    let numeric = |t| matches!(t, ColType::Int4 | ColType::Int8 | ColType::Float8 | ColType::Numeric);
    if numeric(a) && numeric(b) {
        return Some(super::exec::unify_numeric_tower(a, b));
    }
    None
}

/// Materializes a set tree to self-describing rows, coercing every leaf's rows
/// to the columns' common `target` types so the combining operators can match
/// rows by their encoded bytes.
fn eval_set_tree<'a>(
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &[ColType],
) -> Result<&'a mut [&'a [u8]], SqlError> {
    match tree {
        SetTree::Select(s) => eval_set_leaf(s, storage, txid, arena, params, target),
        SetTree::Op { op, all, left, right } => {
            let l = eval_set_tree(left, storage, txid, arena, params, target)?;
            let r = eval_set_tree(right, storage, txid, arena, params, target)?;
            combine_sets(*op, *all, l, r, arena)
        }
    }
}

/// Describes a set-operation body: column names/types come from the first leaf,
/// then each column's type is unified across every leaf (same count required).
/// On success `cols[..n]` carries the final unified OIDs/lengths. Shared by the
/// derived-table, subquery, and INSERT-source paths.
fn describe_set_body<'a>(
    storage: &'a Storage,
    tree: &'a SetTree<'a>,
    txid: u32,
    cols: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let mut leaves: [Option<&Select>; MAX_SET_LEAVES] = [None; MAX_SET_LEAVES];
    let mut n_leaves = 0;
    collect_set_leaves(tree, &mut leaves, &mut n_leaves)?;
    let n_cols = describe_leaf(storage, leaves[0].expect(">=1 leaf"), txid, cols, arena)?;
    let mut target = [ColType::Bool; MAX_PROJ];
    for (c, col) in cols[..n_cols].iter().enumerate() {
        target[c] = super::exec::coltype_of_oid(col.type_oid).unwrap_or(ColType::Text);
    }
    for leaf in leaves[1..n_leaves].iter() {
        let mut lc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let ln = describe_leaf(storage, leaf.expect("leaf"), txid, &mut lc, arena)?;
        if ln != n_cols {
            return Err(sql_err!(
                "42601",
                "each UNION query must have the same number of columns"
            ));
        }
        for c in 0..n_cols {
            let lt = super::exec::coltype_of_oid(lc[c].type_oid).unwrap_or(ColType::Text);
            match unify_set_type(target[c], lt) {
                Some(t) => target[c] = t,
                None => {
                    return Err(sql_err!(
                        "42804",
                        "UNION types {} and {} cannot be matched",
                        target[c].name(),
                        lt.name()
                    ))
                }
            }
        }
    }
    for (c, col) in cols[..n_cols].iter_mut().enumerate() {
        col.type_oid = target[c].oid();
        col.typlen = target[c].typlen();
    }
    Ok(n_cols)
}

/// The result of materializing a set-operation body: the combined encoded rows,
/// the unified per-column types, and the column count.
type MaterializedSet<'a> = (&'a [&'a [u8]], &'a [ColType], usize);

/// Materializes a set-operation body to combined encoded rows plus the unified
/// column types, ready to decode. Shared by subquery and INSERT-source paths.
fn materialize_set_body<'a>(
    storage: &'a Storage,
    txid: u32,
    tree: &'a SetTree<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<MaterializedSet<'a>, SqlError> {
    let mut cols = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = describe_set_body(storage, tree, txid, &mut cols, arena)?;
    let mut tgt = [ColType::Bool; MAX_PROJ];
    for c in 0..n {
        tgt[c] = super::exec::coltype_of_oid(cols[c].type_oid).unwrap_or(ColType::Text);
    }
    let target = arena.alloc_slice_copy(&tgt[..n]).map_err(|_| arena_full())?;
    let rows = eval_set_tree(tree, storage, txid, arena, params, target)?;
    Ok((rows, target, n))
}

fn eval_set_leaf<'a>(
    s: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &[ColType],
) -> Result<&'a mut [&'a [u8]], SqlError> {
    // Pass 1: count the rows. Pass 2: coerce to the target types and encode.
    let mut count = 0usize;
    select_into_rows(storage, txid, s, arena, params, &mut |_| {
        count += 1;
        Ok(())
    })?;
    let empty: &[u8] = &[];
    let rows = arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    let n = target.len();
    let mut at = 0usize;
    select_into_rows(storage, txid, s, arena, params, &mut |vals| {
        if vals.len() != n {
            return Err(sql_err!(
                "42601",
                "each UNION query must have the same number of columns"
            ));
        }
        let mut coerced = [Datum::Null; MAX_PROJ];
        for c in 0..n {
            coerced[c] = super::eval::cast_to(vals[c], target[c], arena)?;
        }
        rows[at] = super::exec::encode_projected_pub(&coerced[..n], arena)?;
        at += 1;
        Ok(())
    })?;
    Ok(rows)
}

/// Combines two encoded-row multisets. Both inputs are sorted here (set ops are
/// unordered until the final ORDER BY), then merged by equal runs.
fn combine_sets<'a>(
    op: SetOp,
    all: bool,
    l: &'a mut [&'a [u8]],
    r: &'a mut [&'a [u8]],
    arena: &'a Arena,
) -> Result<&'a mut [&'a [u8]], SqlError> {
    l.sort_unstable();
    r.sort_unstable();
    let empty: &[u8] = &[];
    let out = arena
        .alloc_slice_with(l.len() + r.len(), |_| empty)
        .map_err(|_| arena_full())?;
    let mut n = 0usize;
    let mut push = |row: &'a [u8], times: usize| {
        for _ in 0..times {
            out[n] = row;
            n += 1;
        }
    };
    match op {
        SetOp::Union if all => {
            for &row in l.iter().chain(r.iter()) {
                push(row, 1);
            }
        }
        SetOp::Union => {
            // Distinct merge of two sorted runs.
            let (mut i, mut j) = (0, 0);
            let mut last: Option<&[u8]> = None;
            while i < l.len() || j < r.len() {
                let take_l = j >= r.len() || (i < l.len() && l[i] <= r[j]);
                let row = if take_l {
                    i += 1;
                    l[i - 1]
                } else {
                    j += 1;
                    r[j - 1]
                };
                if last != Some(row) {
                    push(row, 1);
                    last = Some(row);
                }
            }
        }
        SetOp::Intersect | SetOp::Except => {
            let (mut i, mut j) = (0, 0);
            while i < l.len() {
                // One equal run in l.
                let row = l[i];
                let mut cl = 0;
                while i < l.len() && l[i] == row {
                    cl += 1;
                    i += 1;
                }
                // Advance r past smaller values, then count the matching run.
                while j < r.len() && r[j] < row {
                    j += 1;
                }
                let mut cr = 0;
                while j < r.len() && r[j] == row {
                    cr += 1;
                    j += 1;
                }
                let times = match (op, all) {
                    (SetOp::Intersect, true) => cl.min(cr),
                    (SetOp::Intersect, false) => usize::from(cr > 0),
                    (SetOp::Except, true) => cl.saturating_sub(cr),
                    (SetOp::Except, false) => usize::from(cr == 0),
                    _ => unreachable!(),
                };
                push(row, times);
            }
        }
    }
    Ok(&mut out[..n])
}

/// Sorts combined set-operation rows by the trailing ORDER BY, which may
/// reference an output column by 1-based position or by name (from the first
/// leaf). Other ORDER BY expressions over a set operation are unsupported.
fn sort_set_rows(
    rows: &mut [&[u8]],
    order_by: &[super::ast::OrderBy],
    cols: &[ColDesc],
) -> Result<(), SqlError> {
    if order_by.is_empty() {
        return Ok(());
    }
    // Resolve each key to an output column index.
    let mut keys: [(usize, bool, bool); MAX_PROJ] = [(0, false, false); MAX_PROJ];
    let mut nk = 0;
    for ob in order_by {
        let idx = match ob.expr {
            Expr::Int(n) if *n >= 1 && (*n as usize) <= cols.len() => (*n as usize) - 1,
            Expr::Column { name, qualifier: None } => {
                match cols.iter().position(|c| c.name == *name) {
                    Some(i) => i,
                    None => {
                        return Err(sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "ORDER BY column \"{}\" does not exist in the set-operation result",
                            name
                        ))
                    }
                }
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "ORDER BY on a set operation must name an output column or its position"
                ))
            }
        };
        keys[nk] = (idx, ob.descending, ob.nulls_first);
        nk += 1;
    }
    let keys = &keys[..nk];
    let mut err: Option<SqlError> = None;
    rows.sort_by(|a, b| {
        if err.is_some() {
            return core::cmp::Ordering::Equal;
        }
        for &(idx, descending, nulls_first) in keys {
            let va = super::exec::decode_projected_pub(a, idx);
            let vb = super::exec::decode_projected_pub(b, idx);
            let ord = match (va.is_null(), vb.is_null()) {
                (true, true) => core::cmp::Ordering::Equal,
                (true, false) => if nulls_first { core::cmp::Ordering::Less } else { core::cmp::Ordering::Greater },
                (false, true) => if nulls_first { core::cmp::Ordering::Greater } else { core::cmp::Ordering::Less },
                (false, false) => match compare_datums(&va, &vb) {
                    Ok(o) => if descending { o.reverse() } else { o },
                    Err(e) => {
                        err = Some(e);
                        core::cmp::Ordering::Equal
                    }
                },
            };
            if ord != core::cmp::Ordering::Equal {
                return ord;
            }
        }
        core::cmp::Ordering::Equal
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// FROM-less `SELECT` (one virtual row, no columns). Item and WHERE
/// expressions may still contain subqueries — always uncorrelated here, since
/// there is no outer row to reference — so they are prepared once and injected
/// by node identity, exactly as the table path does.
pub fn constant_select<'a>(
    storage: &'a Storage,
    txid: u32,
    stmt: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    resp: &mut Responder,
) -> Outcome {
    if let Err(e) = check_select_constants(stmt, arena) {
        return sql_fail(e);
    }
    let mut cols = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = match describe_items(stmt.items, None, &mut cols) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };

    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = stmt.where_clause;
    for (i, item) in stmt.items.iter().enumerate() {
        if let SelectItem::Expr { expr, .. } = item {
            sub_exprs[1 + i] = Some(expr);
        }
    }
    let subs = match prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)
    {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };

    let mut values = [Datum::Null; MAX_PROJ];
    for (i, item) in stmt.items.iter().enumerate() {
        let SelectItem::Expr { expr, .. } = item else {
            unreachable!("wildcard rejected by describe_items");
        };
        match eval_full(expr, arena, params, &super::eval::NoColumns, &hooks) {
            Ok(v) => values[i] = v,
            Err(e) => return sql_fail(e),
        }
    }
    let mut emit_row = true;
    if let Some(w) = stmt.where_clause {
        match where_passes(w, arena, params, &super::eval::NoColumns, &hooks) {
            Ok(pass) => emit_row = pass,
            Err(e) => return sql_fail(e),
        }
    }
    resp.row_description(&cols[..n])?;
    let mut rows = 0u64;
    if emit_row {
        resp.data_row(&values[..n])?;
        rows = 1;
    }
    let tag = stack_format!(32, "SELECT {}", rows);
    resp.command_complete(tag.as_str())?;
    sql_ok()
}

/// Runs a `SELECT` used as an INSERT source, invoking `emit` once per output
/// row with that row's projected datums. The resulting table is unordered, so
/// ORDER BY is ignored; DISTINCT/GROUP BY/aggregate sources are rejected loudly
/// (not yet supported). Subqueries (including correlated) in the source are
/// supported.
#[allow(clippy::too_many_arguments)]
pub fn select_into_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    stmt: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    emit: &mut dyn FnMut(&[Datum<'a>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if let Some(tree) = stmt.set_body {
        let (rows, _target, n) = materialize_set_body(storage, txid, tree, arena, params)?;
        let mut vals = [Datum::Null; MAX_PROJ];
        for row in rows.iter() {
            for (c, slot) in vals[..n].iter_mut().enumerate() {
                *slot = super::exec::decode_projected_pub(row, c);
            }
            emit(&vals[..n])?;
        }
        return Ok(());
    }
    check_select_constants(stmt, arena)?;
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_aggs(expr, &mut agg_nodes, &mut n_aggs)?;
        }
    }
    if let Some(h) = stmt.having {
        collect_aggs(h, &mut agg_nodes, &mut n_aggs)?;
    }
    // GROUP BY or aggregates: run the grouped executor and emit each output
    // row. ORDER BY is dropped (the resulting set is unordered).
    if !stmt.group_by.is_empty() || n_aggs > 0 {
        if stmt.distinct {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "SELECT DISTINCT with GROUP BY or aggregates is not supported in this context"
            ));
        }
        let Some(from) = &stmt.from else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "aggregates without a FROM clause are not supported in this context"
            ));
        };
        let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
        let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
        sub_exprs[0] = stmt.where_clause;
        sub_exprs[1] = stmt.having;
        for (i, item) in stmt.items.iter().enumerate() {
            if let SelectItem::Expr { expr, .. } = item {
                sub_exprs[2 + i] = Some(expr);
            }
        }
        let outer = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
        if !outer.correlated.is_empty() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "correlated subqueries with GROUP BY or aggregates are not supported yet"
            ));
        }
        let hooks = EvalHooks { group: None, aggs: None, subs: Some(&outer.base) , windows: None, catalog: None, srf_index: None };
        let (rows, width) =
            grouped_rows(storage, &scope, from, txid, stmt, &agg_nodes[..n_aggs], arena, params, &hooks)?;
        for row in rows {
            let mut out = [Datum::Null; MAX_PROJ];
            for (i, slot) in out.iter_mut().take(width).enumerate() {
                *slot = super::exec::decode_projected_pub(row, i);
            }
            emit(&out[..width])?;
        }
        return Ok(());
    }
    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = stmt.where_clause;
    for (i, item) in stmt.items.iter().enumerate() {
        if let SelectItem::Expr { expr, .. } = item {
            sub_exprs[1 + i] = Some(expr);
        }
    }

    let Some(from) = &stmt.from else {
        // FROM-less: one row (or zero, when WHERE is false).
        let subs =
            prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
        let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
        if let Some(w) = stmt.where_clause
            && !where_passes(w, arena, params, &super::eval::NoColumns, &hooks)? {
                return Ok(());
            }
        let mut vals = [Datum::Null; MAX_PROJ];
        let mut n = 0;
        for item in stmt.items {
            let SelectItem::Expr { expr, .. } = item else {
                return Err(sql_err!("42601", "SELECT * with no tables specified is not valid"));
            };
            vals[n] = eval_full(expr, arena, params, &super::eval::NoColumns, &hooks)?;
            n += 1;
        }
        emit(&vals[..n])?;
        return Ok(());
    };

    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
    let outer_subs = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
    let correlated = outer_subs.correlated;
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&outer_subs.base) , windows: None, catalog: None, srf_index: None };

    // Window functions (`OVER (...)`) in the projection: materialize the rows
    // with each window value computed, then emit. ORDER BY/LIMIT are handled by
    // the outer query, so the derived-table order is left unspecified.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in stmt.items {
        if let SelectItem::Expr { expr, .. } = item {
            collect_windows(expr, &mut win_nodes, &mut n_win)?;
        }
    }
    if n_win > 0 {
        let (proj_rows, _keys) = project_window_rows(
            storage, txid, stmt, from, &scope, &win_nodes[..n_win], &hooks, arena, params,
        )?;
        for row in proj_rows {
            emit(row)?;
        }
        return Ok(());
    }

    // DISTINCT / ORDER BY / LIMIT / OFFSET need the whole set materialized
    // (so top-N and dedup are correct), then paged.
    if stmt.distinct || !stmt.order_by.is_empty() || stmt.limit.is_some() || stmt.offset.is_some() {
        let (rows, width) = materialized_rows(
            storage, &scope, from, txid, stmt, arena, params, &hooks, correlated, &outer_subs.base,
        )?;
        let limit = super::exec::eval_limit_pub(stmt.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(stmt.offset, arena, params)?;
        for row in rows
            .iter()
            .skip(offset as usize)
            .take(limit.min(usize::MAX as u64) as usize)
        {
            let mut out = [Datum::Null; MAX_PROJ];
            for (i, slot) in out.iter_mut().take(width).enumerate() {
                *slot = super::exec::decode_projected_pub(row, i);
            }
            emit(&out[..width])?;
        }
        return Ok(());
    }
    let where_in_scan = if correlated.is_empty() { stmt.where_clause } else { None };

    // A set-returning `_pg_expandarray(arr)` in the projection expands each
    // source row into one output row per array element.
    let srf_arg = find_expandarray_arg(stmt.items);
    scan_source(
        storage, &scope, from, txid, where_in_scan, arena, params, &hooks, None,
        &mut |row| {
            let mut sc: [(*const Expr, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), Datum::Null); MAX_SUBQUERIES];
            let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
            let row_subs;
            let row_hooks_owned;
            let row_hooks: &EvalHooks = if correlated.is_empty() {
                &hooks
            } else {
                row_subs = merge_correlated(
                    correlated, &outer_subs.base, row, storage, txid, arena, params, &mut sc, &mut ls,
                )?;
                row_hooks_owned = EvalHooks { group: None, aggs: None, subs: Some(&row_subs) , windows: None, catalog: None, srf_index: None };
                if let Some(w) = stmt.where_clause
                    && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                        return Ok(true);
                    }
                &row_hooks_owned
            };
            let mut projected = [Datum::Null; MAX_PROJ];
            match srf_arg {
                None => {
                    let n = project_row(stmt.items, &scope, row, arena, params, row_hooks, &mut projected)?;
                    emit(&projected[..n])?;
                }
                Some(arg) => {
                    let count = match eval_full(arg, arena, params, row, row_hooks)? {
                        Datum::Array { raw, .. } => super::array::len(raw),
                        Datum::Null => 0,
                        _ => 1,
                    };
                    for k in 1..=count {
                        let srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        let n = project_row(stmt.items, &scope, row, arena, params, &srf_hooks, &mut projected)?;
                        emit(&projected[..n])?;
                    }
                }
            }
            Ok(true)
        },
    )
}

/// The single argument of the first `_pg_expandarray(arr)` call in the select
/// list, if any — the source array whose length drives row expansion.
fn find_expandarray_arg<'a>(items: &[SelectItem<'a>]) -> Option<&'a Expr<'a>> {
    fn walk<'a>(e: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
        match e {
            Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("_pg_expandarray") => {
                args.first().copied()
            }
            Expr::Field { base, .. } => walk(base),
            Expr::Cast { operand, .. } => walk(operand),
            Expr::Binary { left, right, .. } => walk(left).or_else(|| walk(right)),
            Expr::Call { args, .. } => args.iter().find_map(|a| walk(a)),
            _ => None,
        }
    }
    items.iter().find_map(|it| match it {
        SelectItem::Expr { expr, .. } => walk(expr),
        SelectItem::Wildcard => None,
    })
}

/// Projects one source row through the select items.
fn project_row<'a>(
    items: &[SelectItem<'a>],
    scope: &QueryScope,
    row: &JoinRow<'_, 'a, '_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
) -> Result<usize, SqlError> {
    let mut n = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for t in 0..scope.n {
                    let def = scope.defs[t].expect("resolved");
                    let vals = row.values[t].expect("bound");
                    for c in 0..def.n_columns {
                        if n == MAX_PROJ {
                            return Err(sql_err!(
                                "54000",
                                "select list expands past {} columns",
                                MAX_PROJ
                            ));
                        }
                        out[n] = if vals.is_empty() { Datum::Null } else { vals[c] };
                        n += 1;
                    }
                }
            }
            SelectItem::Expr { expr, .. } => {
                if n == MAX_PROJ {
                    return Err(sql_err!(
                        "54000",
                        "select list expands past {} columns",
                        MAX_PROJ
                    ));
                }
                out[n] = eval_full(expr, arena, params, row, hooks)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Column descriptions across the whole scope (wildcards expand every
/// table).
pub fn describe_scope_items<'q>(
    items: &[SelectItem<'q>],
    scope: &QueryScope<'q>,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    // The single-table fast path resolves a qualifier against the table name,
    // so only take it when the exposed name equals the table name (no alias);
    // an aliased table falls through to the alias-aware scope path below.
    if scope.n == 1 && scope.names[0] == scope.defs[0].expect("resolved").name.as_str() {
        return describe_items(items, Some(scope.defs[0].expect("resolved")), out);
    }
    let mut n = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => {
                for t in 0..scope.n {
                    let def = scope.defs[t].expect("resolved");
                    for c in def.columns() {
                        if n == out.len() {
                            return Err(sql_err!("54000", "select list too wide"));
                        }
                        out[n] = ColDesc::of_type(c.name.as_str(), c.ctype);
                        n += 1;
                    }
                }
            }
            SelectItem::Expr { expr, alias } => {
                if n == out.len() {
                    return Err(sql_err!("54000", "select list too wide"));
                }
                // Multi-table type inference: columns resolve via scope.
                let (oid, typlen) = infer_scope_type(expr, scope)?;
                let name = alias.unwrap_or(super::exec::derived_name(expr));
                out[n] = ColDesc::new(name, oid, typlen);
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Resolves column types across all tables in a join scope.
struct ScopeCols<'s, 'd>(&'s QueryScope<'d>);
impl super::exec::ColTypeResolver for ScopeCols<'_, '_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        let (t, c) = self.0.find_column(qualifier, name)?;
        Ok(self.0.defs[t].expect("resolved").columns()[c].ctype)
    }
}

fn infer_scope_type(expr: &Expr, scope: &QueryScope) -> Result<(i32, i16), SqlError> {
    let (oid, typlen) = super::exec::infer_type_res(expr, &ScopeCols(scope))?;
    if oid == super::types::oid::UNKNOWN {
        Ok((super::types::oid::TEXT, -1))
    } else {
        Ok((oid, typlen))
    }
}

/// GROUP BY / plain-aggregate execution: single scan collecting encoded
/// (key, agg-argument) pairs is avoided by running one scan per phase —
/// group keys with row-by-row aggregate folding, sort-based.
/// The row-producing half of grouped/aggregate execution: runs the scans,
/// folds aggregates per group, applies HAVING, and returns the surviving
/// output rows (self-describing-encoded, `width` visible columns followed by
/// `n_order` hidden ORDER BY key columns) sorted by any ORDER BY. The caller
/// applies LIMIT/OFFSET and emits. Shared by the wire path (`grouped_select`)
/// and the row-source path (`select_into_rows`).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn grouped_rows<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    stmt: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(&'a [&'a [u8]], usize), SqlError> {
    // Validate: non-aggregate select items must be GROUP BY expressions.
    for item in stmt.items {
        let SelectItem::Expr { expr, .. } = item else {
            return Err(sql_err!(
                "42803",
                "SELECT * must appear in the GROUP BY clause or be used in an aggregate function"
            ));
        };
        if !expr_is_grouped(expr, stmt.group_by) {
            return Err(sql_err!(
                "42803",
                "column must appear in the GROUP BY clause or be used in an aggregate function"
            ));
        }
    }

    // Pass 1: count rows, so group storage can be arena-allocated.
    let mut row_count = 0usize;
    scan_source(
        storage, scope, from, txid, stmt.where_clause, arena, params, hooks,
        None,
        &mut |_| {
            row_count += 1;
            Ok(true)
        },
    )?;

    // No rows and no GROUP BY: aggregates still yield one row.
    let n_keys = stmt.group_by.len();

    // Pass 2: encode group keys per row.
    let empty: &[u8] = &[];
    let keys: &mut [(&[u8], u32)] = arena
        .alloc_slice_with(row_count, |_| (empty, 0u32))
        .map_err(|_| arena_full())?;
    {
        let mut at = 0usize;
        scan_source(
            storage, scope, from, txid, stmt.where_clause, arena, params, hooks,
            None,
            &mut |row| {
                let mut key_vals = [Datum::Null; MAX_PROJ];
                for (k, g) in stmt.group_by.iter().enumerate() {
                    key_vals[k] = eval_full(g, arena, params, row, hooks)?;
                }
                keys[at].0 = super::exec::encode_projected_pub(&key_vals[..n_keys], arena)?;
                keys[at].1 = at as u32;
                at += 1;
                Ok(true)
            },
        )?;
    }
    keys.sort_unstable();

    // Group runs → per-group aggregate folding needs per-row agg updates;
    // rows are identified by scan order, so fold with one more scan that
    // dispatches updates to the right group.
    let n_groups = {
        let mut g = 0usize;
        for i in 0..keys.len() {
            if i == 0 || keys[i].0 != keys[i - 1].0 {
                g += 1;
            }
        }
        if keys.is_empty() && stmt.group_by.is_empty() {
            1 // plain aggregates over zero rows: one output row
        } else {
            g
        }
    };
    // row index (scan order) → group index
    let group_of: &mut [u32] = arena
        .alloc_slice_with(row_count, |_| 0u32)
        .map_err(|_| arena_full())?;
    // representative key row per group
    let rep_of: &mut [u32] = arena
        .alloc_slice_with(n_groups, |_| 0u32)
        .map_err(|_| arena_full())?;
    {
        let mut g = 0usize;
        for i in 0..keys.len() {
            if i > 0 && keys[i].0 != keys[i - 1].0 {
                g += 1;
            }
            group_of[keys[i].1 as usize] = g as u32;
            rep_of[g] = keys[i].1;
        }
    }

    // Aggregate states per group.
    let n_aggs = agg_nodes.len();
    let states: &mut [AggState] = arena
        .alloc_slice_with(n_groups * n_aggs.max(1), |_| AggState::default())
        .map_err(|_| arena_full())?;
    for g in 0..n_groups {
        for (i, (_, node)) in agg_nodes.iter().enumerate() {
            states[g * n_aggs.max(1) + i].init(node)?;
        }
    }
    if n_aggs > 0 {
        let mut at = 0usize;
        scan_source(
            storage, scope, from, txid, stmt.where_clause, arena, params, hooks,
            None,
            &mut |row| {
                let g = group_of.get(at).copied().unwrap_or(0) as usize;
                for (i, (_, node)) in agg_nodes.iter().enumerate() {
                    states[g * n_aggs + i].update(node, arena, params, row, hooks)?;
                }
                at += 1;
                Ok(true)
            },
        )?;
    }

    // Emit per group: reconstruct key values, inject hooks, evaluate
    // HAVING then items.
    let agg_ptrs: &[*const Expr] = arena
        .alloc_slice_with(n_aggs, |i| agg_nodes[i].0)
        .map_err(|_| arena_full())?;
    // ORDER BY over groups: ordinals resolve to select items; every key
    // expression evaluates under the group hooks (so aggregates work).
    let n_order = stmt.order_by.len();
    let width = stmt.items.len();
    let mut order_exprs: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    for (k, ob) in stmt.order_by.iter().enumerate() {
        order_exprs[k] = Some(super::exec::resolve_order_expr_pub(ob.expr, stmt.items)?);
    }

    // Materialize surviving groups (visible + hidden key columns).
    let empty: &[u8] = &[];
    let out_rows: &mut [&[u8]] = arena
        .alloc_slice_with(n_groups, |_| empty)
        .map_err(|_| arena_full())?;
    let mut survivors = 0usize;
    for g in 0..n_groups {
        let mut key_vals = [Datum::Null; MAX_PROJ];
        if !keys.is_empty() {
            let rep = keys
                .iter()
                .find(|(_, idx)| group_of[*idx as usize] as usize == g)
                .expect("group non-empty");
            for (k, slot) in key_vals.iter_mut().enumerate().take(n_keys) {
                *slot = super::exec::decode_projected_pub(rep.0, k);
            }
        }
        let mut agg_vals = [Datum::Null; MAX_AGGS];
        for i in 0..n_aggs {
            agg_vals[i] = states[g * n_aggs.max(1) + i].finish(arena)?;
        }
        let group_hooks = EvalHooks {
            group: Some((stmt.group_by, &key_vals[..n_keys])),
            aggs: Some((agg_ptrs, &agg_vals[..n_aggs])),
            subs: hooks.subs,
        windows: None, catalog: None, srf_index: None };
        if let Some(h) = stmt.having {
            match eval_full(h, arena, params, &super::eval::NoColumns, &group_hooks)? {
                Datum::Bool(true) => {}
                Datum::Bool(false) | Datum::Null => continue,
                _ => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "argument of HAVING must be type boolean"
                    ))
                }
            }
        }
        let mut full = [Datum::Null; MAX_PROJ];
        for (n, item) in stmt.items.iter().enumerate() {
            let SelectItem::Expr { expr, .. } = item else { unreachable!() };
            full[n] = eval_full(expr, arena, params, &super::eval::NoColumns, &group_hooks)?;
        }
        for (k, oe) in order_exprs.iter().take(n_order).enumerate() {
            full[width + k] = eval_full(
                oe.expect("resolved"),
                arena,
                params,
                &super::eval::NoColumns,
                &group_hooks,
            )?;
        }
        out_rows[survivors] =
            super::exec::encode_projected_pub(&full[..width + n_order], arena)?;
        survivors += 1;
    }
    let out_rows = &mut out_rows[..survivors];

    if n_order > 0 {
        out_rows.sort_unstable_by(|a, b| {
            for (k, ob) in stmt.order_by.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, width + k);
                let kb = super::exec::decode_projected_pub(b, width + k);
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
            core::cmp::Ordering::Equal
        });
    }

    Ok((out_rows, width))
}

/// GROUP BY / plain-aggregate execution to the wire: produce the grouped rows,
/// then page with LIMIT/OFFSET and emit.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn grouped_select<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    stmt: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    limit: u64,
    offset: u64,
    resp: &mut Responder,
) -> Outcome {
    let (out_rows, width) =
        match grouped_rows(storage, scope, from, txid, stmt, agg_nodes, arena, params, hooks) {
            Ok(x) => x,
            Err(e) => return sql_fail(e),
        };
    let mut emitted = 0u64;
    for row in out_rows.iter().skip(offset as usize) {
        if emitted >= limit {
            break;
        }
        let mut out = [Datum::Null; MAX_PROJ];
        for (i, slot) in out.iter_mut().take(width).enumerate() {
            *slot = super::exec::decode_projected_pub(row, i);
        }
        resp.data_row(&out[..width])?;
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    resp.command_complete(tag.as_str())?;
    sql_ok()
}

/// Does this item expression consist only of grouped expressions,
/// aggregates, and constants?
fn expr_is_grouped(expr: &Expr, group_by: &[&Expr]) -> bool {
    if group_by.iter().any(|g| **g == *expr) || expr.is_aggregate() {
        return true;
    }
    match expr {
        Expr::Column { .. } => false,
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_) | Expr::Str(_)
        | Expr::Param(_) | Expr::DefaultMarker | Expr::Subquery(_) | Expr::Exists(_)
        | Expr::ArraySubquery(_) => true,
        Expr::Unary { operand, .. }
        | Expr::Cast { operand, .. }
        | Expr::IsNull { operand, .. } => expr_is_grouped(operand, group_by),
        Expr::Binary { left, right, .. } => {
            expr_is_grouped(left, group_by) && expr_is_grouped(right, group_by)
        }
        Expr::Call { args, .. } => args.iter().all(|a| expr_is_grouped(a, group_by)),
        Expr::InList { operand, list, .. } => {
            expr_is_grouped(operand, group_by)
                && list.iter().all(|e| expr_is_grouped(e, group_by))
        }
        Expr::Between { operand, low, high, .. } => {
            expr_is_grouped(operand, group_by)
                && expr_is_grouped(low, group_by)
                && expr_is_grouped(high, group_by)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            expr_is_grouped(operand, group_by) && expr_is_grouped(pattern, group_by)
        }
        Expr::Case { operand, whens, otherwise } => {
            operand.is_none_or(|o| expr_is_grouped(o, group_by))
                && whens
                    .iter()
                    .all(|(c, r)| expr_is_grouped(c, group_by) && expr_is_grouped(r, group_by))
                && otherwise.is_none_or(|o| expr_is_grouped(o, group_by))
        }
        Expr::InSubquery { operand, .. } => expr_is_grouped(operand, group_by),
        Expr::Array(items) => items.iter().all(|e| expr_is_grouped(e, group_by)),
        Expr::Subscript { base, index } => {
            expr_is_grouped(base, group_by) && expr_is_grouped(index, group_by)
        }
        Expr::Field { base, .. } => expr_is_grouped(base, group_by),
        Expr::AnyAll { operand, array, .. } => {
            expr_is_grouped(operand, group_by) && expr_is_grouped(array, group_by)
        }
    }
}

/// DISTINCT / ORDER BY: materialize projected rows plus hidden key
/// columns, dedupe on the visible prefix, sort by the hidden keys, page.
/// The row-producing half of DISTINCT / ORDER BY execution: materialize
/// projected rows (with hidden ORDER BY key columns), dedupe on the visible
/// prefix for DISTINCT, and sort by the hidden keys. Returns `(rows, width)`;
/// the caller pages with LIMIT/OFFSET and emits. Shared by the wire path
/// (`materialized_select`) and the row-source path (`select_into_rows`).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn materialized_rows<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    stmt: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
) -> Result<(&'a [&'a [u8]], usize), SqlError> {
    let n_order = stmt.order_by.len();
    // With correlated subqueries WHERE is applied per row (against merged
    // hooks); otherwise the scan applies it directly.
    let where_in_scan = if correlated.is_empty() { stmt.where_clause } else { None };

    // Resolve ORDER BY ordinals to item expressions.
    let mut order_exprs: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    for (k, ob) in stmt.order_by.iter().enumerate() {
        order_exprs[k] = Some(super::exec::resolve_order_expr_pub(ob.expr, stmt.items)?);
    }
    // DISTINCT restriction: keys must be select-list members.
    if stmt.distinct {
        for oe in order_exprs.iter().take(n_order) {
            let target = oe.expect("resolved");
            let in_list = stmt.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expr, .. } if **expr == *target)
            });
            if !in_list {
                return Err(sql_err!(
                    "42P10",
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
    }

    // Visible width.
    let width = {
        let mut w = 0usize;
        for item in stmt.items {
            w += match item {
                SelectItem::Wildcard => scope.total_columns(),
                SelectItem::Expr { .. } => 1,
            };
        }
        w
    };

    // Pass 1: count — and evaluate the projection and ORDER BY keys per row
    // (discarding the values). PostgreSQL scans, filters, and projects in a
    // single per-row pass below the Sort, so an early row's projection error
    // surfaces before a later row's WHERE error. We materialize in two passes
    // for a fixed-size allocation, so the count pass must reproduce that error
    // timing rather than evaluate every WHERE before any projection.
    let mut count = 0usize;
    scan_source(
        storage, scope, from, txid, where_in_scan, arena, params, hooks,
        None,
        &mut |row| {
            let mut sc: [(*const Expr, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), Datum::Null); MAX_SUBQUERIES];
            let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
            let row_subs;
            let row_hooks_owned;
            let row_hooks: &EvalHooks = if correlated.is_empty() {
                hooks
            } else {
                row_subs = merge_correlated(
                    correlated, base, row, storage, txid, arena, params, &mut sc, &mut ls,
                )?;
                row_hooks_owned =
                    EvalHooks { group: None, aggs: None, subs: Some(&row_subs), windows: None, catalog: None, srf_index: None };
                if let Some(w) = stmt.where_clause
                    && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                        return Ok(true);
                    }
                &row_hooks_owned
            };
            let mut projected = [Datum::Null; MAX_PROJ];
            project_row(stmt.items, scope, row, arena, params, row_hooks, &mut projected)?;
            for oe in order_exprs.iter().take(n_order) {
                eval_full(oe.expect("resolved"), arena, params, row, row_hooks)?;
            }
            count += 1;
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
            None,
            &mut |row| {
                let mut sc: [(*const Expr, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_owned;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    hooks
                } else {
                    row_subs = merge_correlated(
                        correlated, base, row, storage, txid, arena, params, &mut sc, &mut ls,
                    )?;
                    row_hooks_owned =
                        EvalHooks { group: None, aggs: None, subs: Some(&row_subs) , windows: None, catalog: None, srf_index: None };
                    if let Some(w) = stmt.where_clause
                        && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                            return Ok(true);
                        }
                    &row_hooks_owned
                };
                let mut projected = [Datum::Null; MAX_PROJ];
                let n = project_row(stmt.items, scope, row, arena, params, row_hooks, &mut projected)?;
                debug_assert_eq!(n, width);
                let mut full = projected;
                for (k, oe) in order_exprs.iter().take(n_order).enumerate() {
                    full[width + k] =
                        eval_full(oe.expect("resolved"), arena, params, row, row_hooks)?;
                }
                rows[at] = super::exec::encode_projected_pub(&full[..width + n_order], arena)?;
                at += 1;
                Ok(true)
            },
        )?;
    }

    let mut live = rows.len();
    if stmt.distinct {
        // Dedupe on the visible prefix: sort whole rows (visible prefix
        // dominates the encoding), then drop adjacent equal prefixes.
        rows.sort_unstable();
        let mut unique = 0usize;
        for i in 0..rows.len() {
            let same = i > 0
                && visible_prefix(rows[i], width) == visible_prefix(rows[i - 1], width);
            if !same {
                rows[unique] = rows[i];
                unique += 1;
            }
        }
        live = unique;
    }
    let rows = &mut rows[..live];

    if n_order > 0 {
        rows.sort_unstable_by(|a, b| {
            for (k, ob) in stmt.order_by.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, width + k);
                let kb = super::exec::decode_projected_pub(b, width + k);
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
            core::cmp::Ordering::Equal
        });
    }

    Ok((rows, width))
}

/// DISTINCT / ORDER BY execution to the wire: materialize the rows, then page
/// with LIMIT/OFFSET and emit.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn materialized_select<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    stmt: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    limit: u64,
    offset: u64,
    resp: &mut Responder,
) -> Outcome {
    let (rows, width) = match materialized_rows(
        storage, scope, from, txid, stmt, arena, params, hooks, correlated, base,
    ) {
        Ok(x) => x,
        Err(e) => return sql_fail(e),
    };
    let mut emitted = 0u64;
    for row in rows.iter().skip(offset as usize) {
        if emitted >= limit {
            break;
        }
        let mut out = [Datum::Null; MAX_PROJ];
        for (i, slot) in out.iter_mut().take(width).enumerate() {
            *slot = super::exec::decode_projected_pub(row, i);
        }
        resp.data_row(&out[..width])?;
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    resp.command_complete(tag.as_str())?;
    sql_ok()
}

/// Byte span of the first `width` encoded columns.
fn visible_prefix(bytes: &[u8], width: usize) -> &[u8] {
    let mut at = 1usize;
    for _ in 0..width {
        let tag = bytes[at];
        at += 1;
        at += match tag {
            0 => 0,
            1 => 1,
            2 | 6 => 4,
            3 | 4 | 7 | 8 => 8,
            9 => 16,
            5 | 10 => {
                let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                4 + len
            }
            _ => unreachable!(),
        };
    }
    &bytes[..at]
}

/// Order helpers exported for update/delete WHERE-subquery support.
pub fn subquery_hooks<'a>(
    exprs: &[Option<&'a Expr<'a>>],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<SubqueryValues<'a, 'a>, SqlError> {
    prepare_subqueries(exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)
}


/// For `UPDATE ... FROM` / `DELETE ... USING`: enumerates the extra tables in
/// `from`, resolving the target row's columns through `target` (as the outer
/// scope), and invokes `on_match` with a combined lookup for the FIRST joined
/// row that satisfies `where_clause`. Returns whether any match was found.
#[allow(clippy::too_many_arguments)]
pub fn first_from_match<'a>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &dyn ColumnLookup<'a>,
    on_match: &mut dyn FnMut(&dyn ColumnLookup<'a>) -> Result<(), SqlError>,
) -> Result<bool, SqlError> {
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
    let subs = subquery_hooks(&[where_clause], storage, txid, arena, params)?;
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
    let mut found = false;
    scan_source(
        storage,
        &scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        &hooks,
        Some(target),
        &mut |jr| {
            let cr = Chained { inner: jr, outer: Some(target) };
            on_match(&cr)?;
            found = true;
            Ok(false) // stop at the first match (PostgreSQL uses one arbitrary row)
        },
    )?;
    Ok(found)
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "query result exceeds the statement arena"
    )
}

/// Public view-DML rewriting uses this for arena-exhaustion.
pub fn arena_full_pub() -> SqlError {
    arena_full()
}

/// Synthesizes a `TableDef` for a derived table (`FROM (SELECT ...) exposed`)
/// from the subquery's output column names and inferred types. Schema only —
/// no rows are produced, so it needs neither a txid nor bound parameters.
fn synth_derived_def<'a>(
    storage: &'a Storage,
    sub: &'a Select<'a>,
    exposed: &'a str,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a TableDef, SqlError> {
    let mut descs = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match sub.set_body {
        Some(tree) => describe_set_body(storage, tree, txid, &mut descs, arena)?,
        None => match &sub.from {
            Some(f) => {
                let ss = QueryScope::resolve_schema(storage, f, txid, arena)?;
                describe_scope_items(sub.items, &ss, &mut descs)?
            }
            None => describe_items(sub.items, None, &mut descs)?,
        },
    };
    if n_cols > MAX_COLUMNS {
        return Err(sql_err!(
            "54011",
            "derived table \"{}\" has too many columns",
            exposed
        ));
    }
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    };
    let mut columns = [blank; MAX_COLUMNS];
    for i in 0..n_cols {
        let ct = super::exec::coltype_of_oid(descs[i].type_oid).ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "derived table column \"{}\" type (oid {}) is not supported",
                descs[i].name,
                descs[i].type_oid
            )
        })?;
        columns[i] = ColumnMeta {
            name: SqlName::parse(descs[i].name)?,
            ctype: ct,
            ..blank
        };
    }
    let def = TableDef {
        name: SqlName::parse(exposed)?,
        columns,
        n_columns: n_cols,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Synthesizes the single-column `TableDef` for a supported table function
/// (`FROM func(args) alias`). The output column is named after the alias (or the
/// function name), so a bare reference to the alias resolves to the value.
fn table_func_def<'a>(tref: &'a TableRef<'a>, arena: &'a Arena) -> Result<&'a TableDef, SqlError> {
    if !tref.table.eq_ignore_ascii_case("generate_series") {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table function \"{}\" is not supported",
            tref.table
        ));
    }
    let name = tref.alias.unwrap_or("generate_series");
    // The column takes the explicit column-alias if given, else the table name.
    let col_name = tref.col_alias.unwrap_or(name);
    let blank = ColumnMeta {
        name: SqlName::parse("").expect("empty name is valid"),
        ctype: ColType::Bool,
        type_mod: -1,
        not_null: false,
        unique: false,
        primary: false,
        auto_increment: false,
        default_value: None,
    };
    let mut columns = [blank; MAX_COLUMNS];
    columns[0] = ColumnMeta { name: SqlName::parse(col_name)?, ctype: ColType::Int8, ..blank };
    let def = TableDef {
        name: SqlName::parse(name)?,
        columns,
        n_columns: 1,
        ..TableDef::empty()
    };
    Ok(&*arena.alloc(def).map_err(|_| arena_full())?)
}

/// Materializes a table function's rows. Currently `generate_series(start, stop
/// [, step])` over integers; the arguments are evaluated as constants (a lateral
/// argument referencing an outer column surfaces loudly as an unresolved
/// column). Each row is one `int8` value, projected-encoded.
fn table_func_rows<'a>(
    tref: &'a TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a [&'a [u8]], SqlError> {
    let args = tref.func_args.expect("table function carries arguments");
    if args.len() != 2 && args.len() != 3 {
        return Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "generate_series expects 2 or 3 arguments"
        ));
    }
    let as_i64 = |e: &'a Expr<'a>| -> Result<i64, SqlError> {
        match super::eval::eval(e, arena, params, &super::eval::NoColumns)? {
            Datum::Int4(v) => Ok(v as i64),
            Datum::Int8(v) => Ok(v),
            _ => Err(sql_err!("42883", "generate_series requires integer arguments")),
        }
    };
    let start = as_i64(args[0])?;
    let stop = as_i64(args[1])?;
    let step = if args.len() == 3 { as_i64(args[2])? } else { 1 };
    if step == 0 {
        return Err(sql_err!("22023", "step size cannot equal zero"));
    }
    let count = if step > 0 {
        if stop < start { 0 } else { ((stop - start) / step) as usize + 1 }
    } else if stop > start {
        0
    } else {
        ((start - stop) / (-step)) as usize + 1
    };
    const EMPTY: &[u8] = &[];
    let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
    let mut v = start;
    for slot in rows.iter_mut() {
        *slot = super::exec::encode_projected_pub(&[Datum::Int8(v)], arena)?;
        v += step;
    }
    Ok(&*rows)
}
