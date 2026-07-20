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
    BinaryOp, Cte, Expr, FrameBound, FrameUnits, FromClause, Join, JoinKind, MaterializedCte,
    OrderBy, Select, SelectItem, SetOp, SetQuery, SetTree, TableRef, WindowFrame,
    MAX_USING_COLUMNS,
};
use super::eval::{
    compare_datums, eval_full, sqlstate, ColumnLookup, EvalHooks, SqlError, SubqueryValues,
};
use super::exec::{describe_items, MAX_PROJ};
use super::types::{ColDesc, ColType, Datum};

pub const MAX_JOIN_TABLES: usize = 9; // base + 8 joins
const MAX_AGGS: usize = 16;

use core::cell::Cell;
std::thread_local! {
    /// Wall-clock deadline (micros since 2000-01-01) for the running statement;
    /// 0 means no `statement_timeout` is armed. Single-threaded per connection.
    static DEADLINE: Cell<i64> = const { Cell::new(0) };
    /// Amortizes the clock read in [`check_timeout`] to roughly 1 in 1024 calls.
    static TICK: Cell<u32> = const { Cell::new(0) };
}

/// Arms `statement_timeout` for the current statement (`timeout_ms == 0` clears
/// it). Call [`disarm_timeout`] when the statement completes.
pub fn arm_timeout(timeout_ms: u64) {
    let dl = if timeout_ms == 0 {
        0
    } else {
        super::datetime::now_micros().saturating_add(timeout_ms as i64 * 1000)
    };
    DEADLINE.with(|d| d.set(dl));
}

/// Clears any armed statement deadline.
pub fn disarm_timeout() {
    DEADLINE.with(|d| d.set(0));
}

/// Errors 57014 if the armed statement deadline has passed. Called at scan
/// boundaries; the clock is only read about once per 1024 calls.
pub fn check_timeout() -> Result<(), SqlError> {
    let dl = DEADLINE.with(|d| d.get());
    if dl == 0 {
        return Ok(());
    }
    let t = TICK.with(|c| {
        let v = c.get().wrapping_add(1);
        c.set(v);
        v
    });
    if !t.is_multiple_of(1024) {
        return Ok(());
    }
    if super::datetime::now_micros() >= dl {
        return Err(sql_err!("57014", "canceling statement due to statement timeout"));
    }
    Ok(())
}
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

    fn reloid(&self, name: &str) -> Option<i32> {
        super::catalog::reloid_of_name(self.storage, name)
    }
}

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

/// Upper bound on distinct USING/NATURAL-merged columns across a join tree
/// (chained merges of the same name allocate a fresh entry per join).
pub const MAX_MERGED_COLUMNS: usize = 32;
const MAX_OUTPUT_COLUMNS: usize = MAX_JOIN_TABLES * MAX_COLUMNS;

/// A `USING`/NATURAL join output column: the merged sides in join order. Its
/// value is the first non-null contributor (PostgreSQL's join output
/// variable — a COALESCE across the joined sides, observable with outer
/// joins).
#[derive(Clone, Copy)]
pub struct MergedColumn<'d> {
    pub name: &'d str,
    pub parts: [(usize, usize); MAX_JOIN_TABLES],
    pub n_parts: usize,
    /// The merged column's type: the common type of the contributors.
    pub ctype: ColType,
}

/// A column name resolved against a query scope: a plain table column
/// (table index, column index), or a USING/NATURAL-merged join column
/// (index into `QueryScope::merged`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvedColumn {
    Table(usize, usize),
    Merged(usize),
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
    /// USING/NATURAL-merged join columns (see `MergedColumn`).
    pub merged: [MergedColumn<'d>; MAX_MERGED_COLUMNS],
    pub n_merged: usize,
    /// The join tree's output columns in PostgreSQL's order — each
    /// USING/NATURAL join hoists its merged columns to the front and hides
    /// the per-side copies. `n_output == 0` means no merges anywhere: the
    /// output is every table's columns in scope order (the common case, kept
    /// implicit).
    output: [ResolvedColumn; MAX_OUTPUT_COLUMNS],
    n_output: usize,
    /// Synthesized USING/NATURAL equality predicates, indexed like
    /// `FromClause::joins` (whose `on` is None for such joins). Filled only
    /// on the executor path (predicate synthesis needs an arena).
    pub join_on: [Option<&'d Expr<'d>>; MAX_JOIN_TABLES - 1],
}

impl<'d> QueryScope<'d> {
    fn empty() -> Self {
        QueryScope {
            names: [""; MAX_JOIN_TABLES],
            defs: [None; MAX_JOIN_TABLES],
            slots: [0; MAX_JOIN_TABLES],
            derived: [None; MAX_JOIN_TABLES],
            n: 0,
            merged: [MergedColumn {
                name: "",
                parts: [(0, 0); MAX_JOIN_TABLES],
                n_parts: 0,
                ctype: ColType::Bool,
            }; MAX_MERGED_COLUMNS],
            n_merged: 0,
            output: [ResolvedColumn::Table(0, 0); MAX_OUTPUT_COLUMNS],
            n_output: 0,
            join_on: [None; MAX_JOIN_TABLES - 1],
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
        scope.build_merges(from, Some(arena))?;
        Ok(scope)
    }

    /// Registers a materialized recursive CTE reference: a synthesized
    /// `TableDef` from the CTE's column names/types, plus its precomputed rows.
    /// `materialize` false = schema only (Describe path).
    fn add_materialized<'a>(
        &mut self,
        tref: &'a TableRef<'a>,
        m: &'a MaterializedCte<'a>,
        arena: &'a Arena,
        materialize: bool,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let exposed = tref.alias.unwrap_or(tref.table);
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                "42712",
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let ncols = m.column_names.len();
        if ncols > MAX_COLUMNS {
            return Err(sql_err!("54011", "too many columns"));
        }
        if let Some(aliases) = tref.col_alias
            && aliases.len() > ncols
        {
            return Err(sql_err!(
                "42P10",
                "table \"{}\" has {} columns available but {} columns specified",
                exposed,
                ncols,
                aliases.len()
            ));
        }
        let mut columns = [ColumnMeta::EMPTY; MAX_COLUMNS];
        for (i, slot) in columns.iter_mut().enumerate().take(ncols) {
            let name = tref
                .col_alias
                .and_then(|a| a.get(i).copied())
                .unwrap_or(m.column_names[i]);
            let ctype =
                super::exec::coltype_of_oid(m.column_types[i].0).unwrap_or(ColType::Text);
            *slot = ColumnMeta { name: SqlName::parse(name)?, ctype, ..ColumnMeta::EMPTY };
        }
        let def = TableDef {
            name: SqlName::parse(exposed)?,
            columns,
            n_columns: ncols,
            ..TableDef::empty()
        };
        let def_reference = arena.alloc(def).map_err(|_| arena_full())?;
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(&*def_reference);
        self.derived[self.n] = Some(if materialize { m.rows } else { &[] });
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
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
        if let Some(m) = tref.cte {
            return self.add_materialized(tref, m, arena, true);
        }
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
        let def_reference = synth_derived_def(storage, sub, exposed, tref.col_alias, txid, arena)?;
        // Materialize the subquery rows, self-describing-encoded, into a
        // doubling arena vector.
        const EMPTY: &[u8] = &[];
        let mut store: *mut &[u8] = core::ptr::null_mut();
        let mut len = 0usize;
        let mut cap = 0usize;
        select_into_rows(storage, txid, sub, arena, params, None, &mut |vals| {
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
        self.defs[self.n] = Some(def_reference);
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
        scope.build_merges(from, None)?;
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
        if let Some(m) = tref.cte {
            return self.add_materialized(tref, m, arena, false);
        }
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
        let def_reference = synth_derived_def(storage, sub, exposed, tref.col_alias, txid, arena)?;
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_reference);
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
        let def_reference: &'a TableDef = arena.alloc(synth.def).map_err(|_| arena_full())?;
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
        self.defs[self.n] = Some(def_reference);
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
        let def_reference = table_func_def(tref, arena, params)?;
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
        self.defs[self.n] = Some(def_reference);
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
    /// Resolves every USING/NATURAL join in `from`: computes the merged
    /// output columns (and the join tree's output order) and synthesizes the
    /// equality predicates (when `arena` is given — the schema-only Describe
    /// path passes None and never evaluates joins).
    fn build_merges(
        &mut self,
        from: &FromClause<'d>,
        arena: Option<&'d Arena>,
    ) -> Result<(), SqlError> {
        if !from.joins.iter().any(|j| j.natural || j.using_columns.is_some()) {
            return Ok(());
        }
        // The left join tree's output columns, updated join by join.
        let mut out = [ResolvedColumn::Table(0, 0); MAX_OUTPUT_COLUMNS];
        let mut n_out = 0usize;
        for c in 0..self.defs[0].expect("resolved").n_columns {
            out[n_out] = ResolvedColumn::Table(0, c);
            n_out += 1;
        }
        for (join_index, join) in from.joins.iter().enumerate() {
            let right_t = join_index + 1;
            let right_def = self.defs[right_t].expect("resolved");
            if !(join.natural || join.using_columns.is_some()) {
                for c in 0..right_def.n_columns {
                    out[n_out] = ResolvedColumn::Table(right_t, c);
                    n_out += 1;
                }
                continue;
            }
            // The using-column list: explicit, or (NATURAL) every left-tree
            // output name the right table also has, in left output order.
            let mut using = [""; MAX_USING_COLUMNS];
            let mut n_using = 0usize;
            if let Some(cols) = join.using_columns {
                using[..cols.len()].copy_from_slice(cols);
                n_using = cols.len();
            } else {
                for entry in &out[..n_out] {
                    let name = self.output_name(*entry);
                    if right_def.column_index(name).is_some()
                        && !using[..n_using].contains(&name)
                    {
                        if n_using == MAX_USING_COLUMNS {
                            return Err(sql_err!(
                                "54000",
                                "NATURAL join merges more than {} columns",
                                MAX_USING_COLUMNS
                            ));
                        }
                        using[n_using] = name;
                        n_using += 1;
                    }
                }
            }
            let mut predicate: Option<&'d Expr<'d>> = None;
            let first_new_merge = self.n_merged;
            for &name in &using[..n_using] {
                // The name must be unique in the left tree and present on
                // the right (empirically pinned against PostgreSQL 18.4).
                let mut left_entry = None;
                for (k, entry) in out[..n_out].iter().enumerate() {
                    if self.output_name(*entry) == name {
                        if left_entry.is_some() {
                            return Err(sql_err!(
                                "42702",
                                "common column name \"{}\" appears more than once in left table",
                                name
                            ));
                        }
                        left_entry = Some((k, *entry));
                    }
                }
                let Some((left_k, left)) = left_entry else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" specified in USING clause does not exist in left table",
                        name
                    ));
                };
                let Some(right_c) = right_def.column_index(name) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" specified in USING clause does not exist in right table",
                        name
                    ));
                };
                let left_type = self.output_type(left);
                let right_type = right_def.columns()[right_c].ctype;
                let Some(ctype) = common_using_type(left_type, right_type) else {
                    // PostgreSQL fails resolving the merged column's `=`
                    // operator at parse time, even over empty tables.
                    return Err(sql_err!(
                        "42883",
                        "operator does not exist: {} = {}",
                        left_type.name(),
                        right_type.name()
                    ));
                };
                if self.n_merged == MAX_MERGED_COLUMNS {
                    return Err(sql_err!(
                        "54000",
                        "join tree merges more than {} USING columns",
                        MAX_MERGED_COLUMNS
                    ));
                }
                let mut merge = MergedColumn {
                    name,
                    parts: [(0, 0); MAX_JOIN_TABLES],
                    n_parts: 0,
                    ctype,
                };
                match left {
                    ResolvedColumn::Table(t, c) => {
                        merge.parts[0] = (t, c);
                        merge.n_parts = 1;
                    }
                    ResolvedColumn::Merged(m) => {
                        let prior = &self.merged[m];
                        merge.parts[..prior.n_parts].copy_from_slice(&prior.parts[..prior.n_parts]);
                        merge.n_parts = prior.n_parts;
                    }
                }
                merge.parts[merge.n_parts] = (right_t, right_c);
                merge.n_parts += 1;
                self.merged[self.n_merged] = merge;
                self.n_merged += 1;
                // Remove the consumed left entry; the merged column is
                // prepended to the output below, after all names resolve.
                out.copy_within(left_k + 1..n_out, left_k);
                n_out -= 1;
                if let Some(arena) = arena {
                    let left_ref = self.output_expression(left, arena)?;
                    let right_ref = arena
                        .alloc(Expr::Column {
                            qualifier: Some(self.names[right_t]),
                            name: right_def.columns()[right_c].name.as_str(),
                        })
                        .map_err(|_| arena_full())?;
                    let eq = arena
                        .alloc(Expr::Binary {
                            operator: BinaryOp::Eq,
                            left: left_ref,
                            right: right_ref,
                        })
                        .map_err(|_| arena_full())?;
                    predicate = Some(match predicate {
                        None => eq,
                        Some(prev) => arena
                            .alloc(Expr::Binary { operator: BinaryOp::And, left: prev, right: eq })
                            .map_err(|_| arena_full())?,
                    });
                }
            }
            // New output: this join's merged columns first, then the
            // remaining left-tree output, then the right table's columns
            // minus the consumed ones.
            let n_new = self.n_merged - first_new_merge;
            out.copy_within(0..n_out, n_new);
            for (k, slot) in out[..n_new].iter_mut().enumerate() {
                *slot = ResolvedColumn::Merged(first_new_merge + k);
            }
            n_out += n_new;
            for c in 0..right_def.n_columns {
                let consumed = (first_new_merge..self.n_merged)
                    .any(|m| self.merged[m].parts[self.merged[m].n_parts - 1] == (right_t, c));
                if !consumed {
                    out[n_out] = ResolvedColumn::Table(right_t, c);
                    n_out += 1;
                }
            }
            self.join_on[join_index] = predicate;
        }
        self.output[..n_out].copy_from_slice(&out[..n_out]);
        self.n_output = n_out;
        Ok(())
    }

    /// The exposed name of a join-tree output column.
    fn output_name(&self, entry: ResolvedColumn) -> &'d str {
        match entry {
            ResolvedColumn::Table(t, c) => {
                self.defs[t].expect("resolved").columns()[c].name.as_str()
            }
            ResolvedColumn::Merged(m) => self.merged[m].name,
        }
    }

    /// The type of a join-tree output column.
    pub fn output_type(&self, entry: ResolvedColumn) -> ColType {
        match entry {
            ResolvedColumn::Table(t, c) => self.defs[t].expect("resolved").columns()[c].ctype,
            ResolvedColumn::Merged(m) => self.merged[m].ctype,
        }
    }

    /// An expression reading a join-tree output column: a qualified column
    /// reference, or (merged) a COALESCE across the contributors.
    fn output_expression(
        &self,
        entry: ResolvedColumn,
        arena: &'d Arena,
    ) -> Result<&'d Expr<'d>, SqlError> {
        match entry {
            ResolvedColumn::Table(t, c) => Ok(&*arena
                .alloc(Expr::Column {
                    qualifier: Some(self.names[t]),
                    name: self.defs[t].expect("resolved").columns()[c].name.as_str(),
                })
                .map_err(|_| arena_full())?),
            ResolvedColumn::Merged(m) => {
                let mc = &self.merged[m];
                let mut args = [&Expr::Null as &'d Expr<'d>; MAX_JOIN_TABLES];
                for (i, &(t, c)) in mc.parts[..mc.n_parts].iter().enumerate() {
                    args[i] = &*arena
                        .alloc(Expr::Column {
                            qualifier: Some(self.names[t]),
                            name: self.defs[t].expect("resolved").columns()[c].name.as_str(),
                        })
                        .map_err(|_| arena_full())?;
                }
                let args =
                    arena.alloc_slice_copy(&args[..mc.n_parts]).map_err(|_| arena_full())?;
                Ok(&*arena
                    .alloc(Expr::Call {
                        name: "coalesce",
                        args,
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    })
                    .map_err(|_| arena_full())?)
            }
        }
    }

    /// Number of `SELECT *` output columns: merged join-tree output when
    /// USING/NATURAL merges exist, else every table's column count.
    pub fn star_columns(&self) -> usize {
        if self.n_output > 0 {
            self.n_output
        } else {
            self.total_columns()
        }
    }

    /// The i-th `SELECT *` output column.
    pub fn star_entry(&self, i: usize) -> ResolvedColumn {
        if self.n_output > 0 {
            return self.output[i];
        }
        let mut k = i;
        for t in 0..self.n {
            let n_cols = self.defs[t].expect("resolved").n_columns;
            if k < n_cols {
                return ResolvedColumn::Table(t, k);
            }
            k -= n_cols;
        }
        unreachable!("star_entry index out of range");
    }

    /// The scope index of the FROM item exposed as `name` (for `t.*`).
    pub fn table_index(&self, name: &str) -> Result<usize, SqlError> {
        self.names[..self.n].iter().position(|n| *n == name).ok_or_else(|| {
            sql_err!("42P01", "missing FROM-clause entry for table \"{}\"", name)
        })
    }

    pub fn find_column(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ResolvedColumn, SqlError> {
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
                    Some(c) => Ok(ResolvedColumn::Table(t, c)),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column {}.{} does not exist",
                        q,
                        name
                    )),
                }
            }
            None => {
                // Unqualified names resolve against the join tree's output
                // columns: a USING/NATURAL-merged column appears there once,
                // so referencing it is not ambiguous.
                if self.n_output > 0 {
                    let mut found = None;
                    for k in 0..self.n_output {
                        if self.output_name(self.output[k]) == name {
                            if found.is_some() {
                                return Err(sql_err!(
                                    "42702",
                                    "column reference \"{}\" is ambiguous",
                                    name
                                ));
                            }
                            found = Some(self.output[k]);
                        }
                    }
                    return found.ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "column \"{}\" does not exist",
                            name
                        )
                    });
                }
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
                        found = Some(ResolvedColumn::Table(t, c));
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

/// The aggregate hook data for a select's items: the aggregate-call node
/// addresses and their folded values.
type AggregateHookData<'a> = (&'a [*const Expr<'a>], &'a [Datum<'a>]);

/// FROM-less aggregation: PostgreSQL treats the missing FROM clause as a
/// single virtual row (zero rows when WHERE is false). Returns the aggregate
/// hook data for evaluating the select items, or None when the query yields
/// no output row at all (WHERE false under GROUP BY, or HAVING false).
fn fromless_aggregate_hooks<'a, R: ColumnLookup<'a>>(
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<AggregateHookData<'a>>, SqlError> {
    let pass = match statement.where_clause {
        Some(w) => where_passes(w, arena, params, row, hooks)?,
        None => true,
    };
    if !statement.group_by.is_empty() && !pass {
        // Zero input rows grouped: zero groups, zero output rows (a plain
        // aggregate still emits its one row over the empty input).
        return Ok(None);
    }
    let mut states = [AggState::default(); MAX_AGGS];
    for (i, (_, node)) in agg_nodes.iter().enumerate() {
        states[i].init(node)?;
    }
    if pass {
        for (i, (_, node)) in agg_nodes.iter().enumerate() {
            states[i].update(node, arena, params, row, hooks)?;
        }
    }
    let values = arena
        .alloc_slice_with(agg_nodes.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, state) in states[..agg_nodes.len()].iter_mut().enumerate() {
        values[i] = state.finish(arena)?;
    }
    let ptrs: &[*const Expr] = arena
        .alloc_slice_with(agg_nodes.len(), |i| agg_nodes[i].0)
        .map_err(|_| arena_full())?;
    if let Some(h) = statement.having {
        let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..*hooks };
        let held = matches!(
            eval_full(h, arena, params, row, &agg_hooks)?,
            Datum::Bool(true)
        );
        if !held {
            return Ok(None);
        }
    }
    Ok(Some((ptrs, values)))
}

/// A scalar (or ARRAY) subquery's output type is known only from its
/// pre-evaluated datum — static inference cannot reach into storage, so the
/// describe pass types it UNKNOWN (rendered text). The same holds for a bare
/// `$n` parameter, whose type arrives with its bound value. Where a select
/// item is one of these, override the described column type from the value.
#[allow(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn patch_subquery_column_types<'a>(
    items: &'a [SelectItem<'a>],
    scope: Option<&QueryScope<'a>>,
    subs: &SubqueryValues,
    params: &[Datum],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    columns: &mut [ColDesc],
) {
    let mut slot = 0usize;
    for item in items {
        match item {
            SelectItem::Wildcard => slot += scope.map_or(0, |s| s.star_columns()),
            SelectItem::TableWildcard(q) => {
                slot += scope
                    .and_then(|s| s.table_index(q).ok().map(|t| {
                        s.defs[t].expect("resolved").n_columns
                    }))
                    .unwrap_or(0);
            }
            SelectItem::Expr { expression, .. } => {
                if slot < columns.len()
                    && matches!(**expression, Expr::Subquery(_) | Expr::ArraySubquery(_))
                {
                    let node: *const Expr = *expression;
                    match subs.scalars.iter().find(|(p, _, _)| core::ptr::eq(*p, node)) {
                        Some((_, v, w)) => {
                            let typed = if v.is_null() { w } else { v };
                            if !typed.is_null()
                                && let Some(ct) = super::exec::coltype_of_oid(typed.type_oid())
                            {
                                columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                            }
                        }
                        // A correlated subquery has no pre-evaluated value;
                        // infer its column type from the inner select's item.
                        None => {
                            if let Expr::Subquery(sub) = &**expression
                                && let Some(SelectItem::Expr { expression: inner, .. }) =
                                    sub.items.first()
                            {
                                let inner_scope = sub.from.as_ref().and_then(|f| {
                                    QueryScope::resolve_schema(storage, f, txid, arena).ok()
                                });
                                let witness = subquery_witness(inner, inner_scope.as_ref());
                                if !witness.is_null()
                                    && let Some(ct) =
                                        super::exec::coltype_of_oid(witness.type_oid())
                                {
                                    columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                                }
                            }
                        }
                    }
                }
                if slot < columns.len()
                    && let Expr::Param(n) = **expression
                    && let Some(v) = params.get(n as usize - 1)
                    && !v.is_null()
                    && columns[slot].type_oid == super::types::oid::TEXT
                    && let Some(ct) = super::exec::coltype_of_oid(v.type_oid())
                {
                    columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                }
                slot += 1;
            }
        }
    }
}

/// ORDER BY `n` refers to the n-th *output* column: select-list stars count
/// one position per expanded column, as in PostgreSQL. A position inside a
/// star synthesizes the column reference; names and expressions delegate to
/// the select-list name-binding rules.
fn resolve_order_target<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let Expr::Int(n) = expression else {
        return super::exec::resolve_order_expr_pub(expression, items);
    };
    let index = *n;
    let position_error =
        || sql_err!("42P10", "ORDER BY position {} is not in select list", index);
    if index < 1 {
        return Err(position_error());
    }
    let column_ref = |qualifier: Option<&'a str>, name: &'a str| {
        Ok(&*arena.alloc(Expr::Column { qualifier, name }).map_err(|_| arena_full())?)
    };
    let mut remaining = index as usize - 1;
    for item in items {
        match item {
            SelectItem::Expr { expression, .. } => {
                if remaining == 0 {
                    return Ok(expression);
                }
                remaining -= 1;
            }
            SelectItem::Wildcard => {
                let width = scope.star_columns();
                if remaining < width {
                    return match scope.star_entry(remaining) {
                        ResolvedColumn::Table(t, c) => column_ref(
                            Some(scope.names[t]),
                            scope.defs[t].expect("resolved").columns()[c].name.as_str(),
                        ),
                        // Unqualified: resolves back to the merged column.
                        ResolvedColumn::Merged(m) => column_ref(None, scope.merged[m].name),
                    };
                }
                remaining -= width;
            }
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                let def = scope.defs[t].expect("resolved");
                if remaining < def.n_columns {
                    return column_ref(
                        Some(scope.names[t]),
                        def.columns()[remaining].name.as_str(),
                    );
                }
                remaining -= def.n_columns;
            }
        }
    }
    Err(position_error())
}

/// The common type of a USING/NATURAL-merged column pair, per PostgreSQL's
/// `select_common_type` (the preferred type of the category wins). `None`
/// means the pair has no `=` operator — an error, as in PostgreSQL.
fn common_using_type(a: ColType, b: ColType) -> Option<ColType> {
    use ColType::*;
    if a == b {
        return Some(a);
    }
    let numeric_rank = |t: ColType| match t {
        Int2 => Some(0),
        Int4 => Some(1),
        Int8 => Some(2),
        Numeric => Some(3),
        Float4 => Some(4),
        Float8 => Some(5),
        _ => None,
    };
    if let (Some(ra), Some(rb)) = (numeric_rank(a), numeric_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    if matches!(a, Text | Varchar | Bpchar) && matches!(b, Text | Varchar | Bpchar) {
        return Some(Text);
    }
    let datetime_rank = |t: ColType| match t {
        Date => Some(0),
        Timestamp => Some(1),
        Timestamptz => Some(2),
        _ => None,
    };
    if let (Some(ra), Some(rb)) = (datetime_rank(a), datetime_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    if matches!(a, Bit { .. }) && matches!(b, Bit { .. }) {
        return Some(Bit { varying: true });
    }
    None
}

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
                "42P10",
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

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<super::types::ColType> {
        let entry = self.scope.find_column(qualifier, name).ok()?;
        Some(self.scope.output_type(entry))
    }

    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        let t = self.scope.table_index(table)?;
        match self.values[t] {
            Some([]) => Ok(false), // outer-join null row
            Some(_) => Ok(true),
            None => Err(sql_err!(
                "42P10",
                "whole-row reference to \"{}\" before its table is joined",
                table
            )),
        }
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'v Arena,
    ) -> Result<Option<&'v [super::types::RecordField<'v>]>, SqlError> {
        let t = self.scope.table_index(table)?;
        let def = self.scope.defs[t].expect("resolved");
        let vals = match self.values[t] {
            Some([]) => return Ok(None), // outer-join null row
            Some(vals) => vals,
            None => {
                return Err(sql_err!(
                    "42P10",
                    "whole-row reference to \"{}\" before its table is joined",
                    table
                ))
            }
        };
        // Copy field names into the arena so the record does not borrow the
        // catalog (its lifetime is unrelated to the row's `'v`).
        let cols = def.columns();
        let mut fields = [super::types::RecordField {
            name: "",
            type_oid: 0,
            value: Datum::Null,
        }; MAX_COLUMNS];
        for (i, field) in fields.iter_mut().enumerate().take(def.n_columns) {
            let name = arena.alloc_str(cols[i].name.as_str()).map_err(|_| arena_full())?;
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
struct Chained<'r, 'a> {
    inner: &'r dyn ColumnLookup<'a>,
    outer: Option<&'r dyn ColumnLookup<'a>>,
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
    ) -> Result<Option<&'a [super::types::RecordField<'a>]>, SqlError> {
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
    // Simplify plan-time-decided boolean arms, fold `col IS [NOT] NULL` on
    // NOT-NULL columns, then order the WHERE conjuncts by PostgreSQL's clause
    // cost once, up front, so the per-row leaf evaluates them cheapest-first
    // without re-sorting.
    let where_clause = match where_clause {
        Some(w) => {
            let simplified = simplify_qual(w, arena)?;
            Some(reorder_qual(fold_null(simplified, scope, arena)?, scope, arena)?)
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
                            *slot = super::exec::decode_projected_pub(bytes, c);
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
            let row = assemble(scope, bound, order, depth, &mut buffers)?;
            if let Some(w) = where_clause {
                let chained_row = Chained { inner: &row, outer };
                if !where_passes(w, arena, params, &chained_row, hooks)? {
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
                // USING/NATURAL predicates are synthesized at plan time.
                && let Some(on) = join.on.or(scope.join_on[depth - 1]) {
                    let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
                    let row = assemble(scope, bound, order, depth + 1, &mut buffers)?;
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
        let passes_pushdown = |bound: &[Option<&'a [u8]>; MAX_JOIN_TABLES]|
         -> Result<bool, SqlError> {
            if pushdown[depth].is_empty() {
                return Ok(true);
            }
            let mut pbuf = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
            let prow = assemble(scope, bound, order, depth + 1, &mut pbuf)?;
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
        if let Some(rows) = scope.derived[order[depth]] {
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
                    storage, scope, from, txid, where_clause, arena, params, hooks,
                    outer, depth + 1, bound, matched, pushdown, order, f,
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
            let table = storage.table(scope.slots[order[depth]]);
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
                check_timeout()?;
                bound[order[depth]] = Some(storage.heap.get(loc));
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched[depth] {
                    m[this].set(true);
                }
                if !level(
                    storage, scope, from, txid, where_clause, arena, params, hooks,
                    outer, depth + 1, bound, matched, pushdown, order, f,
                )? {
                    return Ok(false);
                }
            }
        } else {
            let table = storage.table(scope.slots[order[depth]]);
            let mut index = 0usize;
            for (_, state) in table.rows.iter() {
                check_timeout()?;
                let Some(loc) = state.visible_to(txid) else {
                    continue;
                };
                bound[order[depth]] = Some(storage.heap.get(loc));
                let this = index;
                index += 1;
                if !on_matches(bound)? || !passes_pushdown(bound)? {
                    continue;
                }
                matched_any = true;
                if let Some(m) = matched[depth] {
                    m[this].set(true);
                }
                if !level(
                    storage, scope, from, txid, where_clause, arena, params, hooks,
                    outer, depth + 1, bound, matched, pushdown, order, f,
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
            bound[order[depth]] = None;
            if !level(
                storage, scope, from, txid, where_clause, arena, params, hooks,
                outer, depth + 1, bound, matched, pushdown, order, f,
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
    let mut matched: [Option<&[core::cell::Cell<bool>]>; MAX_JOIN_TABLES] =
        [None; MAX_JOIN_TABLES];
    for (i, j) in from.joins.iter().enumerate() {
        if !matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            continue;
        }
        let t = i + 1;
        let n_rows = if let Some(rows) = scope.derived[t] {
            rows.len()
        } else {
            let table = storage.table(scope.slots[t]);
            table.rows.iter().filter(|(_, s)| s.visible_to(txid).is_some()).count()
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
    // to its position. Everything else keeps FROM order (identity).
    let all_cross = from.joins.iter().all(|j| matches!(j.kind, JoinKind::Cross));
    let order: [usize; MAX_JOIN_TABLES] =
        if all_cross { join_order(scope, where_clause) } else { core::array::from_fn(|i| i) };
    let mut inv_order = [0usize; MAX_JOIN_TABLES];
    for (pos, &t) in order.iter().enumerate() {
        inv_order[t] = pos;
    }
    let mut pushdown_buffers: [[&Expr; MAX_CONJUNCTS]; MAX_JOIN_TABLES] =
        [[&Expr::Null; MAX_CONJUNCTS]; MAX_JOIN_TABLES];
    let mut pd_n = [0usize; MAX_JOIN_TABLES];
    if all_inner && scope.n >= 2 && let Some(w) = where_clause {
        let mut conjunct: [&Expr; MAX_CONJUNCTS] = [w; MAX_CONJUNCTS];
        let mut n = 0;
        let conjuncts: &[&Expr] =
            if flatten_and(w, &mut conjunct, &mut n) { &conjunct[..n] } else { core::slice::from_ref(&w) };
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
    let pushdown: [&[&Expr]; MAX_JOIN_TABLES] = core::array::from_fn(|d| &pushdown_buffers[d][..pd_n[d]]);

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
                                  f: &mut dyn FnMut(
            &JoinRow<'_, 'a, '_>,
        ) -> Result<bool, SqlError>|
         -> Result<bool, SqlError> {
            let mut b = [None; MAX_JOIN_TABLES];
            b[d] = Some(bytes);
            if d + 1 == scope.n {
                // Last level: the row is complete once the left side nulls.
                let mut buffers = [[Datum::Null; MAX_COLUMNS]; MAX_JOIN_TABLES];
                let row = assemble(scope, &b, &order, scope.n, &mut buffers)?;
                if let Some(w) = where_clause {
                    let chained_row = Chained { inner: &row, outer };
                    if !where_passes(w, arena, params, &chained_row, hooks)? {
                        return Ok(true);
                    }
                }
                return f(&row);
            }
            level(
                storage, scope, from, txid, where_clause, arena, params, hooks,
                outer, d + 1, &mut b, &matched, &pushdown, &order, f,
            )
        };
        if let Some(rows) = scope.derived[d] {
            for (index, bytes) in rows.iter().enumerate() {
                if !m[index].get() && !emit_unmatched(bytes, f)? {
                    return Ok(());
                }
            }
        } else {
            let table = storage.table(scope.slots[d]);
            let mut index = 0usize;
            for (_, state) in table.rows.iter() {
                let Some(loc) = state.visible_to(txid) else {
                    continue;
                };
                let this = index;
                index += 1;
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
/// runs the whole thing. Returns the CTE-free select. A no-operator when there are
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
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<UpdatableView<'a>>, SqlError> {
    let Some(sql) = storage.find_view(name, txid) else {
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
    let mut columns = [""; MAX_PROJ];
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
                    columns[n] = arena.alloc_str(c.name.as_str()).map_err(|_| arena_full())?;
                    n += 1;
                }
            }
            // Only a plain, un-aliased base column keeps view and base names in
            // sync (so the view's/DML's WHERE resolve directly against the base).
            SelectItem::Expr { expression: Expr::Column { name: cn, .. }, alias } => {
                if alias.is_some_and(|a| a != *cn) {
                    return Err(not_updatable());
                }
                if n == MAX_PROJ {
                    return Err(not_updatable());
                }
                columns[n] = cn;
                n += 1;
            }
            _ => return Err(not_updatable()),
        }
    }
    let columns = arena.alloc_slice_copy(&columns[..n]).map_err(|_| arena_full())?;
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
            let e = Expr::Binary { operator: super::ast::BinaryOp::And, left: a, right: b };
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
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let sel = super::parser::parse_view_select(sql, arena)?;
    let sel = expand_ctes(sel, storage, txid, arena)?;
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    match &sel.from {
        Some(from) => {
            // Committed catalog: a view's referents are validated as visible now.
            let scope = QueryScope::resolve_schema(storage, from, 0, arena)?;
            describe_scope_items(sel.items, &scope, &mut columns)?;
        }
        None => {
            describe_items(sel.items, None, &mut columns)?;
        }
    }
    Ok(())
}

pub fn expand_ctes<'a>(
    sel: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
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
    let mut resolved: [(&'a str, &'a Select<'a>, &'a [&'a str]); super::parser::MAX_CTES] =
        [("", sel, &[]); super::parser::MAX_CTES];
    let mut n = 0;
    for cte in sel.with {
        if resolved[..n].iter().any(|(name, _, _)| *name == cte.name) {
            return Err(sql_err!("42712", "WITH query name \"{}\" specified more than once", cte.name));
        }
        let context = Subst { ctes: &resolved[..n], materialized: &[], storage, txid, depth: 0 };
        // A self-referencing recursive CTE cannot be inlined; this schema-only
        // path (Describe / view validation) binds its non-recursive term,
        // which carries the CTE's column shape. Execution goes through
        // `expand_ctes_exec`, which materializes the fixpoint.
        let q = if cte.recursive && select_references(cte.query, cte.name) > 0 {
            let (base, _, _) = recursive_parts(cte.query, cte.name)?;
            let wrapped = wrap_set_tree(base, arena)?;
            subst_select(wrapped, context, arena)?
        } else {
            subst_select(cte.query, context, arena)?
        };
        resolved[n] = (cte.name, q, cte.columns);
        n += 1;
    }
    // Substitute the body against all CTEs (the WITH list is dropped by
    // subst_select, which never copies it) and expand any view references.
    let context = Subst { ctes: &resolved[..n], materialized: &[], storage, txid, depth: 0 };
    subst_select(sel, context, arena)
}

/// Like [`expand_ctes`], but for execution: a self-referencing recursive CTE is
/// materialized to a fixpoint (base term, then the recursive term iterated with
/// the CTE name bound to the previous iteration's rows) and its references
/// resolve to the finished row set.
pub fn expand_ctes_exec<'a>(
    sel: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a Select<'a>, SqlError> {
    if sel.with.is_empty() && !storage.has_any_view() {
        return Ok(sel);
    }
    if sel.with.len() > super::parser::MAX_CTES {
        return Err(sql_err!("54023", "too many WITH entries"));
    }
    let mut resolved: [(&'a str, &'a Select<'a>, &'a [&'a str]); super::parser::MAX_CTES] =
        [("", sel, &[]); super::parser::MAX_CTES];
    let mut n = 0;
    let mut materialized: [(&'a str, &'a MaterializedCte<'a>); super::parser::MAX_CTES] =
        [("", &EMPTY_CTE); super::parser::MAX_CTES];
    let mut nm = 0;
    for cte in sel.with {
        if resolved[..n].iter().any(|(name, _, _)| *name == cte.name)
            || materialized[..nm].iter().any(|(name, _)| *name == cte.name)
        {
            return Err(sql_err!("42712", "WITH query name \"{}\" specified more than once", cte.name));
        }
        let context = Subst {
            ctes: &resolved[..n],
            materialized: &materialized[..nm],
            storage,
            txid,
            depth: 0,
        };
        if cte.recursive && select_references(cte.query, cte.name) > 0 {
            let m = materialize_recursive(cte, context, storage, txid, arena, params)?;
            materialized[nm] = (cte.name, m);
            nm += 1;
        } else {
            let q = subst_select(cte.query, context, arena)?;
            resolved[n] = (cte.name, q, cte.columns);
            n += 1;
        }
    }
    let context = Subst {
        ctes: &resolved[..n],
        materialized: &materialized[..nm],
        storage,
        txid,
        depth: 0,
    };
    subst_select(sel, context, arena)
}

/// Describes a whole set-operation query (Describe path): expands CTEs and
/// views schema-only, then unifies the leaf columns.
pub fn describe_set_query<'a>(
    storage: &'a Storage,
    txid: u32,
    q: &'a SetQuery<'a>,
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let body = expand_set_tree(q.with, q.body, storage, txid, arena)?;
    describe_set_body(storage, body, txid, columns, arena)
}

/// Expands WITH CTEs and view references across a whole set-operation tree
/// (schema-only: a self-referencing recursive CTE binds its non-recursive
/// term's shape, as in [`expand_ctes`]).
pub fn expand_set_tree<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty() && !storage.has_any_view() {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes(wrapper, storage, txid, arena)?;
    Ok(expanded.set_body.expect("wrapper keeps its set body"))
}

/// Like [`expand_set_tree`], but for execution: recursive CTEs materialize to
/// their fixpoint (see [`expand_ctes_exec`]).
pub fn expand_set_tree_exec<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a SetTree<'a>, SqlError> {
    if with.is_empty() && !storage.has_any_view() {
        return Ok(tree);
    }
    let wrapper = wrap_set_tree_with(with, tree, arena)?;
    let expanded = expand_ctes_exec(wrapper, storage, txid, arena, params)?;
    Ok(expanded.set_body.expect("wrapper keeps its set body"))
}

/// A synthetic Select carrying `with` and the tree as its set body, so the
/// Select-level CTE/view expansion (which already rewrites `set_body`)
/// applies to a whole set-operation query.
fn wrap_set_tree_with<'a>(
    with: &'a [Cte<'a>],
    tree: &'a SetTree<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let sel = Select {
        items: &[],
        distinct: false,
        distinct_on: &[],
        from: None,
        where_clause: None,
        group_by: &[],
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with,
        set_body: Some(tree),
    };
    Ok(&*arena.alloc(sel).map_err(|_| arena_full())?)
}

static EMPTY_CTE: MaterializedCte<'static> =
    MaterializedCte { column_names: &[], column_types: &[], rows: &[] };

type CteBindings<'a> = [(&'a str, &'a Select<'a>, &'a [&'a str])];

/// Threaded through the FROM-reference rewrite: CTE bindings in scope (query
/// plus optional column-rename list), materialized recursive CTEs, storage (to
/// resolve view names), and the current view-expansion depth (a cycle /
/// runaway-nesting guard).
#[derive(Clone, Copy)]
struct Subst<'c, 'a> {
    ctes: &'c CteBindings<'a>,
    materialized: &'c [(&'a str, &'a MaterializedCte<'a>)],
    storage: &'a Storage,
    /// The requesting transaction, for catalog visibility (a view another
    /// transaction created but has not committed is invisible here).
    txid: u32,
    depth: u32,
}

const MAX_VIEW_DEPTH: u32 = 12;

/// Number of references to the unqualified table name `name` anywhere in a
/// select — FROM items (recursing into derived-table subqueries), the set-op
/// body, and expression subqueries.
fn select_references(s: &Select, name: &str) -> usize {
    if let Some(tree) = s.set_body {
        return set_tree_references(tree, name);
    }
    let mut count = 0usize;
    if let Some(f) = &s.from {
        count += tref_references(&f.base, name);
        for j in f.joins {
            count += tref_references(&j.table, name);
            if let Some(on) = j.on {
                count += expr_references(on, name);
            }
        }
    }
    for it in s.items {
        if let SelectItem::Expr { expression, .. } = it {
            count += expr_references(expression, name);
        }
    }
    if let Some(w) = s.where_clause {
        count += expr_references(w, name);
    }
    if let Some(h) = s.having {
        count += expr_references(h, name);
    }
    for g in s.group_by {
        count += expr_references(g, name);
    }
    count
}

fn tref_references(t: &TableRef, name: &str) -> usize {
    if let Some(sub) = t.subquery {
        return select_references(sub, name);
    }
    usize::from(t.schema.is_none() && t.func_args.is_none() && t.table == name)
}

fn set_tree_references(tree: &SetTree, name: &str) -> usize {
    match tree {
        SetTree::Select(s) => select_references(s, name),
        SetTree::Op { left, right, .. } => {
            set_tree_references(left, name) + set_tree_references(right, name)
        }
    }
}

/// Number of references to `name` inside expression subqueries of `e`.
fn expr_references(e: &Expr, name: &str) -> usize {
    match e {
        Expr::Subquery(s) | Expr::Exists(s) | Expr::ArraySubquery(s) => select_references(s, name),
        Expr::InSubquery { operand, select, .. } => {
            expr_references(operand, name) + select_references(select, name)
        }
        Expr::Unary { operand, .. } | Expr::Cast { operand, .. } | Expr::IsNull { operand, .. } => {
            expr_references(operand, name)
        }
        Expr::Binary { left, right, .. } => {
            expr_references(left, name) + expr_references(right, name)
        }
        Expr::Call { args, .. } => args.iter().map(|a| expr_references(a, name)).sum(),
        Expr::InList { operand, list, .. } => {
            expr_references(operand, name)
                + list.iter().map(|x| expr_references(x, name)).sum::<usize>()
        }
        Expr::Between { operand, low, high, .. } => {
            expr_references(operand, name)
                + expr_references(low, name)
                + expr_references(high, name)
        }
        Expr::Like { operand, pattern, .. } | Expr::Match { operand, pattern, .. } => {
            expr_references(operand, name) + expr_references(pattern, name)
        }
        Expr::Case { operand, whens, otherwise } => {
            operand.map_or(0, |o| expr_references(o, name))
                + whens
                    .iter()
                    .map(|(c, r)| expr_references(c, name) + expr_references(r, name))
                    .sum::<usize>()
                + otherwise.map_or(0, |o| expr_references(o, name))
        }
        Expr::Array(items) => items.iter().map(|x| expr_references(x, name)).sum(),
        Expr::Subscript { base, index } => {
            expr_references(base, name) + expr_references(index, name)
        }
        Expr::Field { base, .. } => expr_references(base, name),
        Expr::AnyAll { operand, array, .. } => {
            expr_references(operand, name) + expr_references(array, name)
        }
        _ => 0,
    }
}

/// Number of *direct* FROM references to `name` in the top-level selects of a
/// set tree (base table or join item; a reference inside a derived-table
/// subquery or an expression subquery does not count).
fn direct_references(tree: &SetTree, name: &str) -> usize {
    let direct = |t: &TableRef| -> usize {
        usize::from(
            t.schema.is_none()
                && t.subquery.is_none()
                && t.func_args.is_none()
                && t.table == name,
        )
    };
    match tree {
        SetTree::Select(s) => {
            let mut count = 0;
            if let Some(f) = &s.from {
                count += direct(&f.base);
                for j in f.joins {
                    count += direct(&j.table);
                }
            }
            count
        }
        SetTree::Op { left, right, .. } => {
            direct_references(left, name) + direct_references(right, name)
        }
    }
}

/// Splits a recursive CTE body into `(non-recursive term, recursive term,
/// union-all)`, enforcing PostgreSQL's required shape.
fn recursive_parts<'a>(
    q: &'a Select<'a>,
    name: &str,
) -> Result<(&'a SetTree<'a>, &'a SetTree<'a>, bool), SqlError> {
    let Some(&SetTree::Op { operator: SetOp::Union, all, left, right }) = q.set_body else {
        return Err(sql_err!(
            "42P19",
            "recursive query \"{}\" does not have the form non-recursive-term UNION [ALL] recursive-term",
            name
        ));
    };
    if !q.order_by.is_empty() || q.limit.is_some() || q.offset.is_some() {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "ORDER BY/LIMIT in a recursive query body is not supported"
        ));
    }
    if set_tree_references(left, name) > 0 {
        return Err(sql_err!(
            "42P19",
            "recursive reference to query \"{}\" must not appear within its non-recursive term",
            name
        ));
    }
    Ok((left, right, all))
}

/// Wraps a set tree as a `Select` (a lone leaf is returned as-is).
fn wrap_set_tree<'a>(tree: &'a SetTree<'a>, arena: &'a Arena) -> Result<&'a Select<'a>, SqlError> {
    if let SetTree::Select(s) = tree {
        return Ok(s);
    }
    let sel = Select {
        items: &[],
        distinct: false,
        distinct_on: &[],
        from: None,
        where_clause: None,
        group_by: &[],
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with: &[],
        set_body: Some(tree),
    };
    Ok(&*arena.alloc(sel).map_err(|_| arena_full())?)
}

/// Materializes a self-referencing recursive CTE to its fixpoint: the
/// non-recursive term's rows first, then the recursive term evaluated
/// repeatedly with the CTE name bound to the previous iteration's rows,
/// accumulating until an iteration adds nothing (UNION deduplicates against
/// everything seen; UNION ALL keeps duplicates and stops on an empty
/// iteration). Row storage is arena-bounded: runaway recursion fails loudly
/// with arena exhaustion, and the statement timeout is honored per iteration.
fn materialize_recursive<'a>(
    cte: &'a Cte<'a>,
    outer: Subst<'_, 'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a MaterializedCte<'a>, SqlError> {
    let (base_tree, recursive_tree, union_all) = recursive_parts(cte.query, cte.name)?;
    // References to earlier CTEs inline now; the self-reference stays a bare
    // table name (it is not in `outer`'s bindings) for per-iteration binding.
    let base_tree = subst_set_tree(base_tree, outer, arena)?;
    let recursive_tree = subst_set_tree(recursive_tree, outer, arena)?;
    let total = set_tree_references(recursive_tree, cte.name);
    let direct = direct_references(recursive_tree, cte.name);
    if total > direct {
        return Err(sql_err!(
            "42P19",
            "recursive reference to query \"{}\" must not appear within a subquery",
            cte.name
        ));
    }
    if direct > 1 {
        return Err(sql_err!(
            "42P19",
            "recursive reference to query \"{}\" must not appear more than once",
            cte.name
        ));
    }
    // Column names and types come from the non-recursive term, with the CTE's
    // rename list applied.
    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
    let ncols = describe_set_body(storage, base_tree, txid, &mut described, arena)?;
    if cte.columns.len() > ncols {
        return Err(sql_err!(
            "42P10",
            "WITH query \"{}\" has {} columns available but {} columns specified",
            cte.name,
            ncols,
            cte.columns.len()
        ));
    }
    let column_names: &'a [&'a str] = {
        let mut names: [&str; MAX_PROJ] = [""; MAX_PROJ];
        for (i, slot) in names.iter_mut().enumerate().take(ncols) {
            *slot = cte.columns.get(i).copied().unwrap_or(described[i].name);
        }
        arena.alloc_slice_copy(&names[..ncols]).map_err(|_| arena_full())?
    };
    let column_types: &'a [(i32, i16)] = {
        let mut types = [(0i32, 0i16); MAX_PROJ];
        for (i, slot) in types.iter_mut().enumerate().take(ncols) {
            *slot = (described[i].type_oid, described[i].typlen);
        }
        arena.alloc_slice_copy(&types[..ncols]).map_err(|_| arena_full())?
    };

    // Base rows; UNION (without ALL) deduplicates them among themselves.
    // Projected-row encoding is order-preserving-for-equality, so byte equality
    // is row equality.
    let (base_rows, _, _) = materialize_set_body(storage, txid, base_tree, arena, params)?;
    const EMPTY: &[u8] = &[];
    let mut all_rows: &'a [&'a [u8]] = if union_all {
        base_rows
    } else {
        let deduped = arena
            .alloc_slice_with(base_rows.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        let mut kept = 0usize;
        for &r in base_rows.iter() {
            if !deduped[..kept].contains(&r) {
                deduped[kept] = r;
                kept += 1;
            }
        }
        &deduped[..kept]
    };
    let mut working: &'a [&'a [u8]] = all_rows;

    while !working.is_empty() {
        check_timeout()?;
        // Bind the CTE name to the previous iteration's rows and evaluate the
        // recursive term.
        let working_cte = arena
            .alloc(MaterializedCte { column_names, column_types, rows: working })
            .map_err(|_| arena_full())?;
        let binding = [(cte.name, &*working_cte)];
        let context = Subst { ctes: &[], materialized: &binding, storage, txid: outer.txid, depth: 0 };
        let step_tree = subst_set_tree(recursive_tree, context, arena)?;
        // The recursive term's column types must agree with the non-recursive
        // term's (PostgreSQL unifies them; a mismatch is a loud error).
        let mut step_desc = [ColDesc::new("", 0, 0); MAX_PROJ];
        let stepn = describe_set_body(storage, step_tree, txid, &mut step_desc, arena)?;
        if stepn != ncols {
            return Err(sql_err!(
                "42601",
                "each UNION query must have the same number of columns"
            ));
        }
        for c in 0..ncols {
            if step_desc[c].type_oid != column_types[c].0 {
                return Err(sql_err!(
                    "42804",
                    "recursive query \"{}\" column {} has type {} in non-recursive term but type {} overall",
                    cte.name,
                    c + 1,
                    column_types[c].0,
                    step_desc[c].type_oid
                ));
            }
        }
        let (step_rows, _, _) = materialize_set_body(storage, txid, step_tree, arena, params)?;
        // Keep the rows this iteration added: all of them under UNION ALL, only
        // never-seen ones under UNION.
        let fresh: &'a [&'a [u8]] = if union_all {
            step_rows
        } else {
            let kept_rows = arena
                .alloc_slice_with(step_rows.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            let mut kept = 0usize;
            for &r in step_rows.iter() {
                if !all_rows.contains(&r) && !kept_rows[..kept].contains(&r) {
                    kept_rows[kept] = r;
                    kept += 1;
                }
            }
            &kept_rows[..kept]
        };
        if fresh.is_empty() {
            break;
        }
        let combined = arena
            .alloc_slice_with(all_rows.len() + fresh.len(), |_| EMPTY)
            .map_err(|_| arena_full())?;
        combined[..all_rows.len()].copy_from_slice(all_rows);
        combined[all_rows.len()..].copy_from_slice(fresh);
        all_rows = combined;
        working = fresh;
    }

    Ok(&*arena
        .alloc(MaterializedCte { column_names, column_types, rows: all_rows })
        .map_err(|_| arena_full())?)
}

fn subst_select<'a>(
    s: &'a Select<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let from = match &s.from {
        Some(f) => Some(subst_from(f, context, arena)?),
        None => None,
    };
    let mut items = [SelectItem::Wildcard; MAX_PROJ];
    if s.items.len() > MAX_PROJ {
        return Err(sql_err!("54011", "select list too wide"));
    }
    for (i, it) in s.items.iter().enumerate() {
        items[i] = match it {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::TableWildcard(q) => SelectItem::TableWildcard(q),
            SelectItem::Expr { expression, alias } => SelectItem::Expr {
                expression: subst_expr(expression, context, arena)?,
                alias: *alias,
            },
        };
    }
    let items = arena.alloc_slice_copy(&items[..s.items.len()]).map_err(|_| arena_full())?;
    let group_by = subst_expr_slice(s.group_by, context, arena)?;
    // Grouping-set bitmasks index into `group_by`; substitution preserves the
    // column order and count, so they carry over unchanged.
    let grouping_sets = arena.alloc_slice_copy(s.grouping_sets).map_err(|_| arena_full())?;
    let mut order = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
        super::parser::MAX_LIST];
    if s.order_by.len() > super::parser::MAX_LIST {
        return Err(sql_err!("54023", "ORDER BY list too long"));
    }
    for (i, ob) in s.order_by.iter().enumerate() {
        order[i] = OrderBy { expression: subst_expr(ob.expression, context, arena)?, ..*ob };
    }
    let order_by = arena.alloc_slice_copy(&order[..s.order_by.len()]).map_err(|_| arena_full())?;
    let set_body = match s.set_body {
        Some(tree) => Some(subst_set_tree(tree, context, arena)?),
        None => None,
    };
    let new = Select {
        items,
        distinct: s.distinct,
        distinct_on: s.distinct_on,
        from,
        where_clause: opt_subst(s.where_clause, context, arena)?,
        group_by,
        grouping_sets,
        having: opt_subst(s.having, context, arena)?,
        order_by,
        limit: opt_subst(s.limit, context, arena)?,
        offset: opt_subst(s.offset, context, arena)?,
        with: &[],
        set_body,
    };
    Ok(&*arena.alloc(new).map_err(|_| arena_full())?)
}

/// Substitutes parameters through every leaf SELECT of a set-operation tree,
/// mirroring [`subst_select`] for a set-operator subquery body.
fn subst_set_tree<'a>(
    tree: &'a SetTree<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a SetTree<'a>, SqlError> {
    let out = match tree {
        SetTree::Select(s) => SetTree::Select(subst_select(s, context, arena)?),
        SetTree::Op { operator, all, left, right } => SetTree::Op {
            operator: *operator,
            all: *all,
            left: subst_set_tree(left, context, arena)?,
            right: subst_set_tree(right, context, arena)?,
        },
    };
    Ok(&*arena.alloc(out).map_err(|_| arena_full())?)
}

fn subst_from<'a>(
    f: &'a FromClause<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<FromClause<'a>, SqlError> {
    let base = subst_tableref(&f.base, context, arena)?;
    let dummy =
        Join { table: f.base, kind: JoinKind::Inner, on: None, using_columns: None, natural: false };
    let mut joins = [dummy; MAX_JOIN_TABLES - 1];
    if f.joins.len() > joins.len() {
        return Err(sql_err!("54023", "too many joins"));
    }
    for (i, j) in f.joins.iter().enumerate() {
        joins[i] = Join {
            table: subst_tableref(&j.table, context, arena)?,
            kind: j.kind,
            on: opt_subst(j.on, context, arena)?,
            using_columns: j.using_columns,
            natural: j.natural,
        };
    }
    let joins = arena.alloc_slice_copy(&joins[..f.joins.len()]).map_err(|_| arena_full())?;
    Ok(FromClause { base, joins })
}

fn subst_tableref<'a>(
    t: &TableRef<'a>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<TableRef<'a>, SqlError> {
    if let Some(sub) = t.subquery {
        return Ok(TableRef {
            subquery: Some(subst_select(sub, context, arena)?),
            ..*t
        });
    }
    // An unqualified name matching a materialized (recursive) CTE resolves to
    // its precomputed row set.
    if t.schema.is_none()
        && t.func_args.is_none()
        && let Some((_, m)) = context.materialized.iter().find(|(name, _)| *name == t.table)
    {
        return Ok(TableRef {
            schema: None,
            table: t.table,
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: None,
            func_args: None,
            col_alias: t.col_alias,
            cte: Some(m),
        });
    }
    // An unqualified name matching a CTE becomes a derived table over the
    // (already-substituted) CTE query, exposed under its alias or CTE name.
    // The CTE's own column-rename list applies unless the reference carries an
    // explicit one (`FROM t AS x(c1, ...)`).
    if t.schema.is_none()
        && let Some((_, q, columns)) = context.ctes.iter().find(|(name, _, _)| *name == t.table)
    {
        let renames = t
            .col_alias
            .or(if columns.is_empty() { None } else { Some(columns) });
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(q),
            func_args: None,
            col_alias: renames,
            cte: None,
        });
    }
    // A name matching a view (and not shadowed by a CTE or table) expands to a
    // derived table over the view's stored SELECT, recursively expanded.
    if t.schema.is_none()
        && context.storage.find_table(t.table).is_none()
        && let Some(view_sql) = context.storage.find_view(t.table, context.txid)
    {
        if context.depth >= MAX_VIEW_DEPTH {
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
            materialized: &[],
            storage: context.storage,
            txid: context.txid,
            depth: context.depth + 1,
        };
        let expanded = subst_select(vsel, inner, arena)?;
        return Ok(TableRef {
            schema: None,
            table: "",
            alias: Some(t.alias.unwrap_or(t.table)),
            subquery: Some(expanded),
            func_args: None,
            col_alias: None,
            cte: None,
        });
    }
    Ok(*t)
}

fn opt_subst<'a>(
    e: Option<&'a Expr<'a>>,
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match e {
        Some(x) => Ok(Some(subst_expr(x, context, arena)?)),
        None => Ok(None),
    }
}

fn subst_expr_slice<'a>(
    xs: &'a [&'a Expr<'a>],
    context: Subst<'_, 'a>,
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
        tmp[i] = subst_expr(x, context, arena)?;
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
                || order_by.iter().any(|o| expr_has_subquery(o.expression))
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
    context: Subst<'_, 'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if !expr_has_subquery(e) {
        return Ok(e);
    }
    let rebuilt = match e {
        Expr::Subquery(s) => Expr::Subquery(subst_select(s, context, arena)?),
        Expr::ArraySubquery(s) => Expr::ArraySubquery(subst_select(s, context, arena)?),
        Expr::Exists(s) => Expr::Exists(subst_select(s, context, arena)?),
        Expr::InSubquery { operand, select, negated } => Expr::InSubquery {
            operand: subst_expr(operand, context, arena)?,
            select: subst_select(select, context, arena)?,
            negated: *negated,
        },
        Expr::Unary { operator, operand } => Expr::Unary {
            operator: *operator,
            operand: subst_expr(operand, context, arena)?,
        },
        Expr::Binary { operator, left, right } => Expr::Binary {
            operator: *operator,
            left: subst_expr(left, context, arena)?,
            right: subst_expr(right, context, arena)?,
        },
        Expr::Cast { operand, type_name, type_mod } => Expr::Cast {
            operand: subst_expr(operand, context, arena)?,
            type_name,
            type_mod: *type_mod,
        },
        Expr::IsNull { operand, negated } => Expr::IsNull {
            operand: subst_expr(operand, context, arena)?,
            negated: *negated,
        },
        Expr::Call { name, args, star, distinct, order_by, over, filter } => {
            let mut ob = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
                super::parser::MAX_LIST];
            if order_by.len() > ob.len() {
                return Err(sql_err!("54023", "aggregate ORDER BY list too long"));
            }
            for (i, o) in order_by.iter().enumerate() {
                ob[i] = OrderBy { expression: subst_expr(o.expression, context, arena)?, ..*o };
            }
            let order_by = arena
                .alloc_slice_copy(&ob[..order_by.len()])
                .map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(w) => {
                    let mut ob2 = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
                        super::parser::MAX_LIST];
                    for (i, o) in w.order_by.iter().enumerate() {
                        ob2[i] = OrderBy { expression: subst_expr(o.expression, context, arena)?, ..*o };
                    }
                    let spec = super::ast::WindowSpec {
                        partition_by: subst_expr_slice(w.partition_by, context, arena)?,
                        order_by: arena.alloc_slice_copy(&ob2[..w.order_by.len()]).map_err(|_| arena_full())?,
                        frame: w.frame,
                    };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            let filter = match filter {
                None => None,
                Some(f) => Some(subst_expr(f, context, arena)?),
            };
            Expr::Call {
                name,
                args: subst_expr_slice(args, context, arena)?,
                star: *star,
                distinct: *distinct,
                order_by,
                over,
                filter,
            }
        }
        Expr::InList { operand, list, negated } => Expr::InList {
            operand: subst_expr(operand, context, arena)?,
            list: subst_expr_slice(list, context, arena)?,
            negated: *negated,
        },
        Expr::Between { operand, low, high, negated } => Expr::Between {
            operand: subst_expr(operand, context, arena)?,
            low: subst_expr(low, context, arena)?,
            high: subst_expr(high, context, arena)?,
            negated: *negated,
        },
        Expr::Like { operand, pattern, negated, case_insensitive } => Expr::Like {
            operand: subst_expr(operand, context, arena)?,
            pattern: subst_expr(pattern, context, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Match { operand, pattern, negated, case_insensitive } => Expr::Match {
            operand: subst_expr(operand, context, arena)?,
            pattern: subst_expr(pattern, context, arena)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Case { operand, whens, otherwise } => {
            let operand = opt_subst(*operand, context, arena)?;
            let mut ws = [(&Expr::Null, &Expr::Null); super::parser::MAX_LIST];
            if whens.len() > ws.len() {
                return Err(sql_err!("54023", "CASE has too many WHEN branches"));
            }
            for (i, (c, r)) in whens.iter().enumerate() {
                ws[i] = (subst_expr(c, context, arena)?, subst_expr(r, context, arena)?);
            }
            let whens = arena.alloc_slice_copy(&ws[..whens.len()]).map_err(|_| arena_full())?;
            Expr::Case {
                operand,
                whens,
                otherwise: opt_subst(*otherwise, context, arena)?,
            }
        }
        // Leaves never reach here (guarded by expr_has_subquery above).
        other => *other,
    };
    Ok(&*arena.alloc(rebuilt).map_err(|_| arena_full())?)
}

/// Walks an expression tree collecting aggregate call nodes.
fn collect_aggs<'a>(
    expression: &'a Expr<'a>,
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if expression.is_aggregate() {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, expression)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!("54000", "too many aggregates in one query"));
        }
        out[*n] = (expression as *const _, expression);
        *n += 1;
        return Ok(()); // aggregate arguments evaluate per input row
    }
    walk_children(expression, &mut |child| collect_aggs(child, out, n))
}

/// Collects window-function call nodes (a `Call` with an `OVER` clause).
fn collect_windows<'a>(
    expression: &'a Expr<'a>,
    out: &mut [&'a Expr<'a>; MAX_WINDOWS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if let Expr::Call { over: Some(_), .. } = expression {
        if out[..*n].iter().any(|e| core::ptr::eq(*e, expression)) {
            return Ok(());
        }
        if *n == MAX_WINDOWS {
            return Err(sql_err!("54023", "too many window functions in one query"));
        }
        out[*n] = expression;
        *n += 1;
        // The arguments and PARTITION/ORDER expressions evaluate per input row;
        // a window function nested inside another is not supported and would be
        // found by the analysis pass, not here.
        return Ok(());
    }
    walk_children(expression, &mut |child| collect_windows(child, out, n))
}

/// Builds a `JoinRow` view over one flat materialized row (all scope columns
/// concatenated, table by table).
fn window_row<'r, 'a>(
    scope: &'r QueryScope<'a>,
    flat: &'r [Datum<'a>],
    offs: &[usize],
) -> JoinRow<'r, 'a, 'a> {
    let mut values: [Option<&[Datum]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
    for (t, offset) in offs.iter().enumerate().take(scope.n) {
        let nc = scope.defs[t].expect("resolved").n_columns;
        values[t] = Some(&flat[*offset..*offset + nc]);
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

/// Collects aggregate-call nodes for a grouped window query: aggregates
/// outside window functions plus those inside window arguments and keys
/// (`sum(sum(v)) OVER (...)`: the inner sum aggregates per group).
fn collect_grouped_aggs<'a>(
    e: &'a Expr<'a>,
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if let Expr::Call { over: Some(spec), args, filter, .. } = e {
        for a in *args {
            collect_grouped_aggs(a, out, n)?;
        }
        for pk in spec.partition_by {
            collect_grouped_aggs(pk, out, n)?;
        }
        for o in spec.order_by {
            collect_grouped_aggs(o.expression, out, n)?;
        }
        if let Some(frame) = &spec.frame {
            for bound in [&frame.start, &frame.end] {
                if let FrameBound::Preceding(x) | FrameBound::Following(x) = bound {
                    collect_grouped_aggs(x, out, n)?;
                }
            }
        }
        if let Some(f) = filter {
            collect_grouped_aggs(f, out, n)?;
        }
        return Ok(());
    }
    if e.is_aggregate() {
        return collect_aggs(e, out, n);
    }
    // GROUPING() reads the current grouping-set mask, so it must evaluate in
    // the inner grouped select, like an aggregate.
    if let Expr::Call { name: "grouping", over: None, .. } = e {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, e)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!("54000", "too many aggregates in one query"));
        }
        out[*n] = (e as *const _, e);
        *n += 1;
        return Ok(());
    }
    walk_children(e, &mut |child| collect_grouped_aggs(child, out, n))
}

/// Context for [`rewrite_grouped_expr`]: the inner derived table's exposed
/// columns for each grouping key and aggregate.
struct GroupedRewrite<'a, 's> {
    group_by: &'a [&'a Expr<'a>],
    group_names: &'a [&'a str],
    aggs: &'a [(*const Expr<'a>, &'a Expr<'a>)],
    agg_names: &'a [&'a str],
    /// The source scope, to distinguish an unknown column (42703) from a
    /// known-but-ungrouped one (42803), as PostgreSQL.
    scope: Option<&'s QueryScope<'a>>,
}

/// Rewrites an outer-select expression of a grouped window query: aggregate
/// nodes and grouping-key expressions become references to the inner derived
/// table's columns; window calls keep their shape with rewritten insides. A
/// leftover bare column is the PostgreSQL grouping error (42803).
fn rewrite_grouped_expr<'a>(
    e: &'a Expr<'a>,
    context: &GroupedRewrite<'a, '_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if let Some(i) = context.aggs.iter().position(|(p, _)| core::ptr::eq(*p, e)) {
        return Ok(&*arena
            .alloc(Expr::Column { qualifier: None, name: context.agg_names[i] })
            .map_err(|_| arena_full())?);
    }
    if let Some(i) = context.group_by.iter().position(|g| **g == *e) {
        return Ok(&*arena
            .alloc(Expr::Column { qualifier: None, name: context.group_names[i] })
            .map_err(|_| arena_full())?);
    }
    let rewrite = |x: &'a Expr<'a>| rewrite_grouped_expr(x, context, arena);
    let alloc = |x: Expr<'a>| -> Result<&'a Expr<'a>, SqlError> {
        Ok(&*arena.alloc(x).map_err(|_| arena_full())?)
    };
    match e {
        Expr::WholeRow(t) => Err(sql_err!(
            "42803",
            "column \"{}.*\" must appear in the GROUP BY clause or be used in an aggregate function",
            t
        )),
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Param(_)
        | Expr::DefaultMarker
        // Subqueries evaluate through their own hooks, not the group.
        | Expr::Subquery(_)
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Ok(e),
        Expr::Column { qualifier, name } => {
            // An unknown column errors as such; a known one is ungrouped.
            if let Some(scope) = context.scope
                && scope.find_column(*qualifier, name).is_err()
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            }
            Err(sql_err!(
                "42803",
                "column \"{}{}{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                qualifier.unwrap_or(""),
                if qualifier.is_some() { "." } else { "" },
                name
            ))
        }
        Expr::Unary { operator, operand } => {
            alloc(Expr::Unary { operator: *operator, operand: rewrite(operand)? })
        }
        Expr::Binary { operator, left, right } => alloc(Expr::Binary {
            operator: *operator,
            left: rewrite(left)?,
            right: rewrite(right)?,
        }),
        Expr::Cast { operand, type_name, type_mod } => alloc(Expr::Cast {
            operand: rewrite(operand)?,
            type_name,
            type_mod: *type_mod,
        }),
        Expr::IsNull { operand, negated } => {
            alloc(Expr::IsNull { operand: rewrite(operand)?, negated: *negated })
        }
        Expr::InList { operand, list, negated } => {
            let mut items = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, x) in list.iter().enumerate() {
                items[i] = rewrite(x)?;
            }
            let list = arena.alloc_slice_copy(&items[..list.len()]).map_err(|_| arena_full())?;
            alloc(Expr::InList { operand: rewrite(operand)?, list, negated: *negated })
        }
        Expr::Between { operand, low, high, negated } => alloc(Expr::Between {
            operand: rewrite(operand)?,
            low: rewrite(low)?,
            high: rewrite(high)?,
            negated: *negated,
        }),
        Expr::Like { operand, pattern, negated, case_insensitive } => alloc(Expr::Like {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        Expr::Match { operand, pattern, negated, case_insensitive } => alloc(Expr::Match {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        Expr::Case { operand, whens, otherwise } => {
            let operand = match operand {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            let mut pairs = [(&Expr::Null as &'a Expr<'a>, &Expr::Null as &'a Expr<'a>);
                super::parser::MAX_LIST];
            for (i, (c, r)) in whens.iter().enumerate() {
                pairs[i] = (rewrite(c)?, rewrite(r)?);
            }
            let whens = arena.alloc_slice_copy(&pairs[..whens.len()]).map_err(|_| arena_full())?;
            let otherwise = match otherwise {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            alloc(Expr::Case { operand, whens, otherwise })
        }
        Expr::Call { name, args, star, distinct, order_by, over, filter } => {
            let mut rewritten = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, a) in args.iter().enumerate() {
                rewritten[i] = rewrite(a)?;
            }
            let args = arena.alloc_slice_copy(&rewritten[..args.len()]).map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(spec) => {
                    let mut parts = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
                    for (i, pk) in spec.partition_by.iter().enumerate() {
                        parts[i] = rewrite(pk)?;
                    }
                    let partition_by = arena
                        .alloc_slice_copy(&parts[..spec.partition_by.len()])
                        .map_err(|_| arena_full())?;
                    let mut obs = [OrderBy {
                        expression: &Expr::Null,
                        descending: false,
                        nulls_first: false,
                    }; super::parser::MAX_LIST];
                    for (i, o) in spec.order_by.iter().enumerate() {
                        obs[i] = OrderBy { expression: rewrite(o.expression)?, ..*o };
                    }
                    let order_by = arena
                        .alloc_slice_copy(&obs[..spec.order_by.len()])
                        .map_err(|_| arena_full())?;
                    let frame = match &spec.frame {
                        None => None,
                        Some(f) => {
                            let bound = |b: &FrameBound<'a>| -> Result<FrameBound<'a>, SqlError> {
                                Ok(match b {
                                    FrameBound::Preceding(x) => FrameBound::Preceding(rewrite(x)?),
                                    FrameBound::Following(x) => FrameBound::Following(rewrite(x)?),
                                    other => *other,
                                })
                            };
                            Some(WindowFrame {
                                units: f.units,
                                start: bound(&f.start)?,
                                end: bound(&f.end)?,
                                exclusion: f.exclusion,
                            })
                        }
                    };
                    let spec = super::ast::WindowSpec { partition_by, order_by, frame };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            let filter = match filter {
                None => None,
                Some(f) => Some(rewrite(f)?),
            };
            alloc(Expr::Call {
                name,
                args,
                star: *star,
                distinct: *distinct,
                order_by,
                over,
                filter,
            })
        }
        Expr::InSubquery { operand, select, negated } => alloc(Expr::InSubquery {
            operand: rewrite(operand)?,
            select,
            negated: *negated,
        }),
        Expr::Array(items) => {
            let mut rewritten = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, x) in items.iter().enumerate() {
                rewritten[i] = rewrite(x)?;
            }
            let items =
                arena.alloc_slice_copy(&rewritten[..items.len()]).map_err(|_| arena_full())?;
            alloc(Expr::Array(items))
        }
        Expr::Subscript { base, index } => {
            alloc(Expr::Subscript { base: rewrite(base)?, index: rewrite(index)? })
        }
        Expr::Field { base, field } => alloc(Expr::Field { base: rewrite(base)?, field }),
        Expr::AnyAll { operand, operator, array, all } => alloc(Expr::AnyAll {
            operand: rewrite(operand)?,
            operator: *operator,
            array: rewrite(array)?,
            all: *all,
        }),
    }
}

/// Windows over a grouped query: PostgreSQL evaluates window functions after
/// GROUP BY / HAVING, over the grouped rows. Rewrites the statement into the
/// equivalent two-level form — an inner grouped select exposing every
/// grouping key and aggregate as a named column, and an outer select (window
/// calls, DISTINCT, ORDER BY, LIMIT) over it as a derived table — so the
/// existing grouped and window executors compose.
fn rewrite_grouped_windows<'a>(
    statement: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_grouped_aggs(expression, &mut agg_nodes, &mut n_aggs)?;
        }
    }
    for ob in statement.order_by {
        collect_grouped_aggs(ob.expression, &mut agg_nodes, &mut n_aggs)?;
    }

    // The inner select: one named column per grouping key and per aggregate.
    let n_keys = statement.group_by.len();
    let mut inner_items = [SelectItem::Wildcard; MAX_PROJ];
    let mut group_names: [&str; MAX_PROJ] = [""; MAX_PROJ];
    let mut agg_names: [&str; MAX_AGGS] = [""; MAX_AGGS];
    for (i, g) in statement.group_by.iter().enumerate() {
        let name = arena
            .alloc_str(stack_format!(16, "?g{}", i).as_str())
            .map_err(|_| arena_full())?;
        group_names[i] = name;
        inner_items[i] = SelectItem::Expr { expression: g, alias: Some(name) };
    }
    for i in 0..n_aggs {
        let name = arena
            .alloc_str(stack_format!(16, "?a{}", i).as_str())
            .map_err(|_| arena_full())?;
        agg_names[i] = name;
        inner_items[n_keys + i] = SelectItem::Expr { expression: agg_nodes[i].1, alias: Some(name) };
    }
    let inner = Select {
        items: arena
            .alloc_slice_copy(&inner_items[..n_keys + n_aggs])
            .map_err(|_| arena_full())?,
        distinct: false,
        distinct_on: &[],
        from: statement.from,
        where_clause: statement.where_clause,
        group_by: statement.group_by,
        grouping_sets: statement.grouping_sets,
        having: statement.having,
        order_by: &[],
        limit: None,
        offset: None,
        with: &[],
        set_body: None,
    };
    let inner = arena.alloc(inner).map_err(|_| arena_full())?;

    let group_names: &[&str] =
        arena.alloc_slice_copy(&group_names[..n_keys]).map_err(|_| arena_full())?;
    let agg_names: &[&str] =
        arena.alloc_slice_copy(&agg_names[..n_aggs]).map_err(|_| arena_full())?;
    let agg_nodes: &[(*const Expr, &Expr)] =
        arena.alloc_slice_copy(&agg_nodes[..n_aggs]).map_err(|_| arena_full())?;
    let scope = statement
        .from
        .as_ref()
        .and_then(|f| QueryScope::resolve_schema(storage, f, txid, arena).ok());
    let context = GroupedRewrite {
        group_by: statement.group_by,
        group_names,
        aggs: agg_nodes,
        agg_names,
        scope: scope.as_ref(),
    };
    let mut outer_items = [SelectItem::Wildcard; MAX_PROJ];
    for (i, item) in statement.items.iter().enumerate() {
        outer_items[i] = match item {
            SelectItem::Expr { expression, alias } => SelectItem::Expr {
                expression: rewrite_grouped_expr(expression, &context, arena)?,
                // The rewritten expression would otherwise rename the output
                // column (`?g0`); pin the original name.
                alias: Some(alias.unwrap_or(super::exec::derived_name(expression))),
            },
            other => *other,
        };
    }
    let mut outer_order = [OrderBy { expression: &Expr::Null, descending: false, nulls_first: false };
        MAX_PROJ];
    for (i, ob) in statement.order_by.iter().enumerate() {
        // Ordinals resolve against the (unchanged) select list; expressions
        // rewrite like the items.
        let expression = if matches!(ob.expression, Expr::Int(_)) {
            ob.expression
        } else {
            rewrite_grouped_expr(ob.expression, &context, arena)?
        };
        outer_order[i] = OrderBy { expression, ..*ob };
    }
    let from = FromClause {
        base: TableRef {
            schema: None,
            table: "",
            alias: Some("?grouped"),
            subquery: Some(inner),
            func_args: None,
            col_alias: None,
            cte: None,
        },
        joins: &[],
    };
    let outer = Select {
        items: arena
            .alloc_slice_copy(&outer_items[..statement.items.len()])
            .map_err(|_| arena_full())?,
        distinct: statement.distinct,
        distinct_on: &[],
        from: Some(from),
        where_clause: None,
        group_by: &[],
        grouping_sets: &[],
        having: None,
        order_by: arena
            .alloc_slice_copy(&outer_order[..statement.order_by.len()])
            .map_err(|_| arena_full())?,
        limit: statement.limit,
        offset: statement.offset,
        with: &[],
        set_body: None,
    };
    Ok(&*arena.alloc(outer).map_err(|_| arena_full())?)
}

/// Evaluates a ROWS/GROUPS frame offset to a non-negative count.
#[allow(clippy::too_many_arguments)]
fn frame_offset_count<'a>(
    e: &'a Expr<'a>,
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    row_index: usize,
    starting: bool,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let r = window_row(scope, rows[row_index], offs);
    let v = eval_full(e, arena, params, &r, hooks)?;
    let n = match v {
        Datum::Int4(x) => x as i64,
        Datum::Int8(x) => x,
        _ => {
            return Err(sql_err!(
                "22023",
                "frame offset must be an integer"
            ))
        }
    };
    if n < 0 {
        return Err(sql_err!(
            "22013",
            "frame {} offset must not be negative",
            if starting { "starting" } else { "ending" }
        ));
    }
    Ok(n as usize)
}

/// Whether a RANGE offset value is negative (PostgreSQL rejects it with
/// 22013, "invalid preceding or following size in window function").
fn range_offset_negative(v: &Datum) -> bool {
    match v {
        Datum::Int4(x) => *x < 0,
        Datum::Int8(x) => *x < 0,
        Datum::Float8(x) => *x < 0.0,
        Datum::Numeric(n) => n.sign == super::numeric::Sign::Neg,
        Datum::Interval(iv) => iv.months < 0 || iv.days < 0 || iv.micros < 0,
        _ => false,
    }
}

/// The inclusive row-index range (into the sorted partition `p[..m]`) an
/// explicit frame selects for the row at sorted position `j`; None when the
/// frame is empty. Bound semantics verified against PostgreSQL 18.4.
#[allow(clippy::too_many_arguments)]
fn frame_range<'a>(
    frame: &WindowFrame<'a>,
    ord: &[OrderBy<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    p: &[usize],
    j: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<(usize, usize)>, SqlError> {
    let m = p.len();
    // Peers under the window ORDER BY (every row is a peer with no ORDER BY).
    let is_peer = |a: usize, b: usize| -> Result<bool, SqlError> {
        ord.iter().try_fold(true, |acc, o| {
            Ok::<bool, SqlError>(acc && {
                let ra = window_row(scope, rows[p[a]], offs);
                let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                let rb = window_row(scope, rows[p[b]], offs);
                let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                match (va.is_null(), vb.is_null()) {
                    (true, true) => true,
                    (true, false) | (false, true) => false,
                    (false, false) => compare_datums(&va, &vb)?.is_eq(),
                }
            })
        })
    };
    let peer_start = |from: usize| -> Result<usize, SqlError> {
        let mut s = from;
        while s > 0 && is_peer(s - 1, from)? {
            s -= 1;
        }
        Ok(s)
    };
    let peer_end = |from: usize| -> Result<usize, SqlError> {
        let mut e = from;
        while e + 1 < m && is_peer(e + 1, from)? {
            e += 1;
        }
        Ok(e)
    };

    // RANGE with a value offset compares the single ORDER BY key.
    let range_edge = |bound: &FrameBound<'a>, starting: bool| -> Result<isize, SqlError> {
        let (offset_expr, preceding) = match bound {
            FrameBound::UnboundedPreceding => return Ok(0),
            FrameBound::UnboundedFollowing => return Ok(m as isize - 1),
            FrameBound::CurrentRow => {
                return Ok(if starting { peer_start(j)? as isize } else { peer_end(j)? as isize })
            }
            FrameBound::Preceding(e) => (*e, true),
            FrameBound::Following(e) => (*e, false),
        };
        if ord.len() != 1 {
            return Err(sql_err!(
                "42P20",
                "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column"
            ));
        }
        let o = &ord[0];
        let key_of = |i: usize| -> Result<Datum<'a>, SqlError> {
            let r = window_row(scope, rows[p[i]], offs);
            eval_full(o.expression, arena, params, &r, hooks)
        };
        let r = window_row(scope, rows[p[j]], offs);
        let off = eval_full(offset_expr, arena, params, &r, hooks)?;
        if range_offset_negative(&off) {
            return Err(sql_err!(
                "22013",
                "invalid preceding or following size in window function"
            ));
        }
        let key_j = key_of(j)?;
        // A NULL current key frames its peer group (nulls are peers).
        if key_j.is_null() {
            return Ok(if starting { peer_start(j)? as isize } else { peer_end(j)? as isize });
        }
        // The frame edge value: preceding moves against the sort direction.
        let towards_smaller = preceding != o.descending;
        let op = if towards_smaller { BinaryOp::Sub } else { BinaryOp::Add };
        let edge = super::eval::arithmetic(op, key_j, off, false, false, arena)?;
        // In-frame: key between edge and key_j (inclusive), in sort order.
        let in_frame = |i: usize| -> Result<bool, SqlError> {
            let k = key_of(i)?;
            if k.is_null() {
                return Ok(false);
            }
            let c = compare_datums(&k, &edge)?;
            Ok(if towards_smaller { c.is_ge() } else { c.is_le() })
        };
        if starting {
            // First row (scanning forward) inside the frame edge.
            for i in 0..m {
                let k = key_of(i)?;
                if k.is_null() {
                    continue;
                }
                let c = compare_datums(&k, &edge)?;
                let inside = if preceding { in_frame(i)? } else {
                    // Starting FOLLOWING: first row at/after the edge in sort
                    // direction.
                    if o.descending { c.is_le() } else { c.is_ge() }
                };
                let _ = c;
                if inside {
                    return Ok(i as isize);
                }
            }
            Ok(m as isize)
        } else {
            // Last row (scanning backward) inside the frame edge.
            for i in (0..m).rev() {
                let k = key_of(i)?;
                if k.is_null() {
                    continue;
                }
                let c = compare_datums(&k, &edge)?;
                let inside = if preceding {
                    // Ending PRECEDING: last row at/before the edge.
                    if o.descending { c.is_ge() } else { c.is_le() }
                } else {
                    in_frame(i)?
                };
                let _ = c;
                if inside {
                    return Ok(i as isize);
                }
            }
            Ok(-1)
        }
    };

    let (start, end): (isize, isize) = match frame.units {
        FrameUnits::Rows => {
            let s: isize = match &frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(e) => {
                    j as isize
                        - frame_offset_count(e, scope, rows, offs, p[j], true, arena, params, hooks)?
                            as isize
                }
                FrameBound::CurrentRow => j as isize,
                FrameBound::Following(e) => {
                    j as isize
                        + frame_offset_count(e, scope, rows, offs, p[j], true, arena, params, hooks)?
                            as isize
                }
                FrameBound::UnboundedFollowing => unreachable!("rejected at parse"),
            };
            let e: isize = match &frame.end {
                FrameBound::UnboundedPreceding => unreachable!("rejected at parse"),
                FrameBound::Preceding(e) => {
                    j as isize
                        - frame_offset_count(e, scope, rows, offs, p[j], false, arena, params, hooks)?
                            as isize
                }
                FrameBound::CurrentRow => j as isize,
                FrameBound::Following(e) => {
                    j as isize
                        + frame_offset_count(e, scope, rows, offs, p[j], false, arena, params, hooks)?
                            as isize
                }
                FrameBound::UnboundedFollowing => m as isize - 1,
            };
            (s, e)
        }
        FrameUnits::Groups => {
            if ord.is_empty() {
                return Err(sql_err!("42P20", "GROUPS mode requires an ORDER BY clause"));
            }
            // This row's peer-group index (groups counted from the front).
            let gj = {
                let mut g = 0usize;
                let mut i = 0usize;
                while i < j {
                    if !is_peer(i, i + 1)? {
                        g += 1;
                    }
                    i += 1;
                }
                g
            };
            let group_start = |target: isize| -> Result<Option<usize>, SqlError> {
                if target < 0 {
                    return Ok(Some(0));
                }
                let mut g = 0usize;
                let mut i = 0usize;
                loop {
                    if g == target as usize {
                        return Ok(Some(i));
                    }
                    // advance to next group
                    let e = peer_end(i)?;
                    if e + 1 >= m {
                        return Ok(None);
                    }
                    i = e + 1;
                    g += 1;
                }
            };
            let group_end = |target: isize| -> Result<Option<usize>, SqlError> {
                if target < 0 {
                    return Ok(None);
                }
                match group_start(target)? {
                    Some(i) => Ok(Some(peer_end(i)?)),
                    None => Ok(Some(m - 1)),
                }
            };
            let s: isize = match &frame.start {
                FrameBound::UnboundedPreceding => 0,
                FrameBound::Preceding(e) => {
                    let k = frame_offset_count(e, scope, rows, offs, p[j], true, arena, params, hooks)?;
                    group_start(gj as isize - k as isize)?.map_or(0, |x| x) as isize
                }
                FrameBound::CurrentRow => peer_start(j)? as isize,
                FrameBound::Following(e) => {
                    let k = frame_offset_count(e, scope, rows, offs, p[j], true, arena, params, hooks)?;
                    match group_start(gj as isize + k as isize)? {
                        Some(x) => x as isize,
                        None => m as isize, // past the last group: empty
                    }
                }
                FrameBound::UnboundedFollowing => unreachable!("rejected at parse"),
            };
            let e: isize = match &frame.end {
                FrameBound::UnboundedPreceding => unreachable!("rejected at parse"),
                FrameBound::Preceding(e) => {
                    let k = frame_offset_count(e, scope, rows, offs, p[j], false, arena, params, hooks)?;
                    match group_end(gj as isize - k as isize)? {
                        Some(x) => x as isize,
                        None => -1, // before the first group: empty
                    }
                }
                FrameBound::CurrentRow => peer_end(j)? as isize,
                FrameBound::Following(e) => {
                    let k = frame_offset_count(e, scope, rows, offs, p[j], false, arena, params, hooks)?;
                    group_end(gj as isize + k as isize)?.map_or(m as isize - 1, |x| x as isize)
                }
                FrameBound::UnboundedFollowing => m as isize - 1,
            };
            (s, e)
        }
        FrameUnits::Range => {
            let uses_offset = matches!(
                (&frame.start, &frame.end),
                (FrameBound::Preceding(_) | FrameBound::Following(_), _)
                    | (_, FrameBound::Preceding(_) | FrameBound::Following(_))
            );
            if uses_offset && ord.is_empty() {
                return Err(sql_err!(
                    "42P20",
                    "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column"
                ));
            }
            (range_edge(&frame.start, true)?, range_edge(&frame.end, false)?)
        }
    };
    let start = start.max(0);
    let end = end.min(m as isize - 1);
    if start > end || start >= m as isize || end < 0 {
        return Ok(None);
    }
    Ok(Some((start as usize, end as usize)))
}

/// The current row's peer-group bounds (sorted-partition indices) under the
/// window ORDER BY; the row alone when there is no ORDER BY.
#[allow(clippy::too_many_arguments)]
fn peer_bounds<'a>(
    ord: &[OrderBy<'a>],
    scope: &QueryScope<'a>,
    rows: &[&'a [Datum<'a>]],
    offs: &[usize],
    p: &[usize],
    j: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(usize, usize), SqlError> {
    if ord.is_empty() {
        // No ORDER BY: every partition row is a peer.
        return Ok((0, p.len() - 1));
    }
    let is_peer = |a: usize, b: usize| -> Result<bool, SqlError> {
        ord.iter().try_fold(true, |acc, o| {
            Ok::<bool, SqlError>(acc && {
                let ra = window_row(scope, rows[p[a]], offs);
                let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                let rb = window_row(scope, rows[p[b]], offs);
                let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
                match (va.is_null(), vb.is_null()) {
                    (true, true) => true,
                    (true, false) | (false, true) => false,
                    (false, false) => compare_datums(&va, &vb)?.is_eq(),
                }
            })
        })
    };
    let mut s = j;
    while s > 0 && is_peer(s - 1, j)? {
        s -= 1;
    }
    let mut e = j;
    while e + 1 < p.len() && is_peer(e + 1, j)? {
        e += 1;
    }
    Ok((s, e))
}

/// Whether sorted-partition index `i` is removed from row `j`'s frame by the
/// frame's EXCLUDE clause (`peers` = row `j`'s peer-group bounds).
fn frame_excludes(
    exclusion: super::ast::FrameExclusion,
    j: usize,
    peers: (usize, usize),
    i: usize,
) -> bool {
    use super::ast::FrameExclusion::*;
    match exclusion {
        NoOthers => false,
        CurrentRow => i == j,
        Group => i >= peers.0 && i <= peers.1,
        Ties => i != j && i >= peers.0 && i <= peers.1,
    }
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
                            let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                            let rb = window_row(scope, rows[p[j]], offs);
                            let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
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
            let offset: isize = if args.len() >= 2 {
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
                let src = j as isize + sign * offset;
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
        } else if let Some(frame) = &spec.frame
            && matches!(*name, "first_value" | "last_value" | "nth_value")
        {
            // Value functions over an explicit frame: per row, the value at
            // the frame's start / end / nth position (NULL on an empty or
            // too-short frame).
            for j in 0..m {
                let range = frame_range(
                    frame, spec.order_by, scope, rows, offs, p, j, arena, params, hooks,
                )?;
                let peers = if frame.exclusion == super::ast::FrameExclusion::NoOthers {
                    (j, j)
                } else {
                    peer_bounds(spec.order_by, scope, rows, offs, p, j, arena, params, hooks)?
                };
                let excluded = |i: usize| frame_excludes(frame.exclusion, j, peers, i);
                out[p[j]] = match (range, *name) {
                    (None, _) => Datum::Null,
                    (Some((fs, fe)), "first_value") => {
                        match (fs..=fe).find(|&i| !excluded(i)) {
                            Some(i) => {
                                let r = window_row(scope, rows[p[i]], offs);
                                eval_full(args[0], arena, params, &r, hooks)?
                            }
                            None => Datum::Null,
                        }
                    }
                    (Some((fs, fe)), "last_value") => {
                        match (fs..=fe).rev().find(|&i| !excluded(i)) {
                            Some(i) => {
                                let r = window_row(scope, rows[p[i]], offs);
                                eval_full(args[0], arena, params, &r, hooks)?
                            }
                            None => Datum::Null,
                        }
                    }
                    (Some((fs, fe)), _) => {
                        let r = window_row(scope, rows[p[j]], offs);
                        let nth = match eval_full(args[1], arena, params, &r, hooks)? {
                            Datum::Int4(v) => v as i64,
                            Datum::Int8(v) => v,
                            _ => 1,
                        };
                        let target = if nth >= 1 {
                            (fs..=fe).filter(|&i| !excluded(i)).nth(nth as usize - 1)
                        } else {
                            None
                        };
                        match target {
                            Some(i) => {
                                let r = window_row(scope, rows[p[i]], offs);
                                eval_full(args[0], arena, params, &r, hooks)?
                            }
                            None => Datum::Null,
                        }
                    }
                };
            }
        } else if matches!(*name, "first_value" | "last_value" | "nth_value" | "ntile") {
            // Value/positional window functions over the default frame
            // (UNBOUNDED PRECEDING TO CURRENT ROW when there is an ORDER BY,
            // else the whole partition).
            let peer_end = |from: usize| -> Result<usize, SqlError> {
                // Index of the last row peered with `from` under the ORDER BY
                // (itself when there is no ORDER BY).
                if spec.order_by.is_empty() {
                    return Ok(m - 1);
                }
                let mut e = from;
                while e + 1 < m {
                    let same = spec.order_by.iter().try_fold(true, |acc, o| {
                        Ok::<bool, SqlError>(acc && {
                            let ra = window_row(scope, rows[p[e]], offs);
                            let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                            let rb = window_row(scope, rows[p[e + 1]], offs);
                            let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
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
                Ok(e)
            };
            match *name {
                "ntile" => {
                    let buckets = {
                        let r = window_row(scope, rows[p[0]], offs);
                        match eval_full(args[0], arena, params, &r, hooks)? {
                            Datum::Int4(v) => v as i64,
                            Datum::Int8(v) => v,
                            _ => 1,
                        }
                    }
                    .max(1);
                    let base = m as i64 / buckets;
                    let larger = m as i64 % buckets; // first `larger` buckets get one extra row
                    let mut index = 0usize;
                    for bucket in 1..=buckets {
                        let size = base + if bucket <= larger { 1 } else { 0 };
                        for _ in 0..size {
                            out[p[index]] = Datum::Int8(bucket);
                            index += 1;
                        }
                    }
                }
                "first_value" => {
                    // Frame start is always the partition start.
                    let r = window_row(scope, rows[p[0]], offs);
                    let value = eval_full(args[0], arena, params, &r, hooks)?;
                    for &row_index in p {
                        out[row_index] = value;
                    }
                }
                "last_value" => {
                    // Frame end is the current row's peer-group end.
                    let mut j = 0usize;
                    while j < m {
                        let end = peer_end(j)?;
                        let r = window_row(scope, rows[p[end]], offs);
                        let value = eval_full(args[0], arena, params, &r, hooks)?;
                        for &row_index in &p[j..=end] {
                            out[row_index] = value;
                        }
                        j = end + 1;
                    }
                }
                _ => {
                    // nth_value(expr, n): the nth row of the frame (1-based from
                    // the frame start); NULL until the frame has reached it.
                    let nth = {
                        let r = window_row(scope, rows[p[0]], offs);
                        match eval_full(args[1], arena, params, &r, hooks)? {
                            Datum::Int4(v) => v as usize,
                            Datum::Int8(v) => v as usize,
                            _ => 1,
                        }
                    };
                    let nth_value = if nth >= 1 && nth <= m {
                        let r = window_row(scope, rows[p[nth - 1]], offs);
                        Some(eval_full(args[0], arena, params, &r, hooks)?)
                    } else {
                        None
                    };
                    let mut j = 0usize;
                    while j < m {
                        let end = peer_end(j)?;
                        // The frame includes rows p[0..=end]; nth is present iff
                        // nth-1 <= end.
                        let value = match nth_value {
                            Some(v) if nth >= 1 && nth - 1 <= end => v,
                            _ => Datum::Null,
                        };
                        for &row_index in &p[j..=end] {
                            out[row_index] = value;
                        }
                        j = end + 1;
                    }
                }
            }
        } else if let Some(frame) = &spec.frame {
            // Aggregate over an explicit frame: computed per row (an empty
            // frame aggregates zero rows — count 0, sum NULL).
            for j in 0..m {
                let range = frame_range(
                    frame, spec.order_by, scope, rows, offs, p, j, arena, params, hooks,
                )?;
                let peers = if frame.exclusion == super::ast::FrameExclusion::NoOthers {
                    (j, j)
                } else {
                    peer_bounds(spec.order_by, scope, rows, offs, p, j, arena, params, hooks)?
                };
                let mut st = AggState::default();
                st.init(node)?;
                if let Some((fs, fe)) = range {
                    for i in fs..=fe {
                        if frame_excludes(frame.exclusion, j, peers, i) {
                            continue;
                        }
                        let r = window_row(scope, rows[p[i]], offs);
                        st.update(node, arena, params, &r, hooks)?;
                    }
                }
                out[p[j]] = st.finish(arena)?;
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
                                let va = eval_full(o.expression, arena, params, &ra, hooks)?;
                                let rb = window_row(scope, rows[p[e + 1]], offs);
                                let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
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
        let va = eval_full(o.expression, arena, params, &ra, hooks)?;
        let rb = window_row(scope, rows[b], offs);
        let vb = eval_full(o.expression, arena, params, &rb, hooks)?;
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
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<(&'a [&'a [Datum<'a>]], &'a [&'a [Datum<'a>]]), SqlError> {
    // WHERE with correlated subqueries is applied per row in the callbacks.
    let scan_where = if correlated.is_empty() { statement.where_clause } else { None };
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
        storage, scope, from, txid, scan_where, arena, params, hooks, None,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
            )? {
                return Ok(true);
            }
            count += 1;
            Ok(true)
        },
    )?;
    // Pass 2: materialize each row's columns flat in the arena.
    let empty: &[Datum] = &[];
    let rows: &mut [&[Datum]] = arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    let mut at = 0usize;
    scan_source(
        storage, scope, from, txid, scan_where, arena, params, hooks, None,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
            )? {
                return Ok(true);
            }
            let flat = arena
                .alloc_slice_with(total.max(1), |_| Datum::Null)
                .map_err(|_| arena_full())?;
            for (t, offset) in offs.iter().enumerate().take(scope.n) {
                let def = scope.defs[t].expect("resolved");
                let vals = row.values[t].expect("bound");
                for c in 0..def.n_columns {
                    flat[offset + c] = if vals.is_empty() { Datum::Null } else { vals[c] };
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
    let n_order = statement.order_by.len();
    let mut order_exprs: [Option<&Expr>; MAX_WIN_KEYS] = [None; MAX_WIN_KEYS];
    if n_order > MAX_WIN_KEYS {
        return Err(sql_err!("54023", "ORDER BY list too long"));
    }
    for (k, ob) in statement.order_by.iter().enumerate() {
        order_exprs[k] = Some(resolve_order_target(ob.expression, statement.items, scope, arena)?);
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
        let jr = window_row(scope, rows[i], &offs);
        // Correlated subqueries in the select list / ORDER BY re-evaluate per
        // output row (their outer references resolve to this window row).
        let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
        let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
        let row_subs;
        let subs = if correlated.is_empty() {
            hooks.subs
        } else {
            row_subs = merge_correlated(
                correlated,
                base,
                &jr,
                storage,
                txid,
                arena,
                params,
                &mut sc,
                &mut ls,
            )?;
            Some(&row_subs)
        };
        let win_hooks = EvalHooks {
            group: None,
            aggs: None,
            subs,
            windows: Some((win_ptrs, &wv[..win_nodes.len()])),
            catalog: hooks.catalog, srf_index: hooks.srf_index,
        };
        let mut projected = [Datum::Null; MAX_PROJ];
        let np = project_row(statement.items, scope, &jr, arena, params, &win_hooks, &mut projected)?;
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
    statement: &'a Select<'a>,
    from: &'a FromClause<'a>,
    scope: &QueryScope<'a>,
    win_nodes: &[&'a Expr<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    if !statement.group_by.is_empty() || statement.having.is_some() {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "grouped window queries reach this executor only after rewriting"
        ));
    }
    // SELECT DISTINCT: ORDER BY keys must be select-list members.
    if statement.distinct {
        for ob in statement.order_by {
            if matches!(ob.expression, Expr::Int(_)) {
                continue;
            }
            let in_list = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *ob.expression)
            });
            if !in_list {
                return sql_fail(sql_err!(
                    "42P10",
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
    }
    let (proj_rows, sort_keys) = match project_window_rows(
        storage, txid, statement, from, scope, win_nodes, hooks, correlated, base, arena, params,
    ) {
        Ok(v) => v,
        Err(e) => return sql_fail(e),
    };
    // DISTINCT dedups on the projected row (encoded order-preserving), with
    // each surviving row keeping its sort keys.
    let (proj_rows, sort_keys) = if statement.distinct {
        match dedup_window_rows(proj_rows, sort_keys, arena) {
            Ok(pair) => pair,
            Err(e) => return sql_fail(e),
        }
    } else {
        (proj_rows, sort_keys)
    };
    let count = proj_rows.len();

    // Sort output rows by the ORDER BY keys.
    let order = match arena.alloc_slice_with(count, |i| i) {
        Ok(o) => o,
        Err(_) => return sql_fail(arena_full()),
    };
    if !statement.order_by.is_empty() {
        for x in 1..count {
            let mut y = x;
            while y > 0 {
                let c = cmp_key_rows(sort_keys[order[y - 1]], sort_keys[order[y]], statement.order_by);
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
        responder.data_row(proj_rows[i])?;
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Dedups window-projected rows on the projected values (order-preserving
/// encoding), keeping each survivor's sort keys. Used by SELECT DISTINCT
/// with window functions.
#[allow(clippy::type_complexity)]
fn dedup_window_rows<'a>(
    proj_rows: &'a [&'a [Datum<'a>]],
    sort_keys: &'a [&'a [Datum<'a>]],
    arena: &'a Arena,
) -> Result<(&'a [&'a [Datum<'a>]], &'a [&'a [Datum<'a>]]), SqlError> {
    let n = proj_rows.len();
    let index = arena.alloc_slice_with(n, |i| i).map_err(|_| arena_full())?;
    let empty: &[u8] = &[];
    let encoded = arena.alloc_slice_with(n, |_| empty).map_err(|_| arena_full())?;
    for i in 0..n {
        encoded[i] = super::exec::encode_projected_pub(proj_rows[i], arena)?;
    }
    index.sort_unstable_by(|&a, &b| encoded[a].cmp(encoded[b]));
    let mut unique = 0usize;
    for k in 0..n {
        let same = k > 0 && encoded[index[k]] == encoded[index[k - 1]];
        if !same {
            index[unique] = index[k];
            unique += 1;
        }
    }
    let empty_row: &[Datum] = &[];
    let out_rows =
        arena.alloc_slice_with(unique, |_| empty_row).map_err(|_| arena_full())?;
    let out_keys =
        arena.alloc_slice_with(unique, |_| empty_row).map_err(|_| arena_full())?;
    for k in 0..unique {
        out_rows[k] = proj_rows[index[k]];
        out_keys[k] = sort_keys[index[k]];
    }
    Ok((&*out_rows, &*out_keys))
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
    expression: &'a Expr<'a>,
    out: &mut [Option<&'a Expr<'a>>; MAX_SUBQUERIES],
    n: &mut usize,
) -> Result<(), SqlError> {
    if matches!(
        expression,
        Expr::Subquery(_) | Expr::InSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_)
    ) {
        if out[..*n].iter().any(|e| core::ptr::eq(e.expect("set"), expression)) {
            return Ok(());
        }
        if *n == MAX_SUBQUERIES {
            return Err(sql_err!("54000", "too many subqueries in one query"));
        }
        out[*n] = Some(expression);
        *n += 1;
        // The operand of IN (SELECT ..) may itself contain subqueries.
        if let Expr::InSubquery { operand, .. } = expression {
            collect_subqueries(operand, out, n)?;
        }
        return Ok(());
    }
    walk_children(expression, &mut |child| collect_subqueries(child, out, n))
}

fn walk_children<'a>(
    expression: &'a Expr<'a>,
    f: &mut dyn FnMut(&'a Expr<'a>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match expression {
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
        // A quantified comparison's array side may be a collected subquery.
        Expr::AnyAll { operand, array, .. } => {
            f(operand)?;
            f(array)
        }
        Expr::Field { base, .. } => f(base),
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
    for expression in exprs.iter().flatten() {
        collect_subqueries(expression, &mut nodes, &mut n)?;
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
    let mut scalars_tmp: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
    let mut lists_tmp: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
    let (mut n_scalars, mut n_lists) = (0, 0);
    for node in nodes.iter().flatten() {
        match node {
            Expr::Subquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        "21000",
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                let v = values.first().copied().unwrap_or(Datum::Null);
                scalars_tmp[n_scalars] = (*node as *const _, v, witness);
                n_scalars += 1;
            }
            Expr::Exists(select) => {
                let found = subquery_exists(select, storage, txid, arena, params, depth, outer)?;
                scalars_tmp[n_scalars] = (*node as *const _, Datum::Bool(found), Datum::Bool(false));
                n_scalars += 1;
            }
            Expr::ArraySubquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, depth, outer)?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars_tmp[n_scalars] = (*node as *const _, v, v);
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
        // Grouped/DISTINCT EXISTS: the row-source executor already handles
        // grouping, HAVING, and DISTINCT — existence is whether it emits.
        let mut found = false;
        select_into_rows(storage, txid, select, arena, params, outer, &mut |_| {
            found = true;
            Ok(())
        })?;
        return Ok(found);
    }
    // The projection list of EXISTS is irrelevant (only row presence matters),
    // but its expressions may carry subqueries; prepare them for the scan.
    let mut item_exprs: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    let mut n_items = 0;
    for item in select.items {
        if let SelectItem::Expr { expression, .. } = item {
            item_exprs[n_items] = Some(expression);
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
        // FROM-less: an aggregate query yields its one output row even over
        // zero input rows (WHERE false), so EXISTS is true unless HAVING
        // filters it. A plain query yields one row when WHERE holds.
        let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_aggs = 0;
        for item in select.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_aggs(expression, &mut agg_nodes, &mut n_aggs)?;
            }
        }
        if let Some(h) = select.having {
            collect_aggs(h, &mut agg_nodes, &mut n_aggs)?;
        }
        if n_aggs > 0 || select.having.is_some() {
            let base = Chained { inner: &super::eval::NoColumns, outer };
            let hook_data = fromless_aggregate_hooks(
                select, &agg_nodes[..n_aggs], arena, params, &base, &hooks,
            )?;
            return Ok(hook_data.is_some());
        }
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
    if select.having.is_some_and(|h| expr_has_outer_ref(h, chain, storage, arena)) {
        return true;
    }
    if select.group_by.iter().any(|g| expr_has_outer_ref(g, chain, storage, arena)) {
        return true;
    }
    select.items.iter().any(|it| match it {
        SelectItem::Expr { expression, .. } => expr_has_outer_ref(expression, chain, storage, arena),
        _ => false,
    })
}

/// Whether any column reference in `expression` resolves only in an enclosing scope
/// beyond `chain`. Nested subqueries push their own scope onto the chain, so a
/// column they provide themselves does not count as an outer reference.
fn expr_has_outer_ref<'a>(
    expression: &'a Expr<'a>,
    chain: &ScopeChain,
    storage: &'a Storage,
    arena: &'a Arena,
) -> bool {
    match expression {
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
            let _ = walk_children(expression, &mut |c| {
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
    for expression in exprs.iter().flatten() {
        collect_subqueries(expression, &mut nodes, &mut n)?;
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
    scalars: &'b mut [(*const Expr<'a>, Datum<'a>, Datum<'a>); MAX_SUBQUERIES],
    lists: &'b mut [(*const Expr<'a>, &'a [Datum<'a>], bool, Datum<'a>); MAX_SUBQUERIES],
) -> Result<SubqueryValues<'b, 'a>, SqlError> {
    let mut ns = 0;
    for (p, v, w) in base.scalars {
        scalars[ns] = (*p, *v, *w);
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
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                if values.len() > 1 {
                    return Err(sql_err!(
                        "21000",
                        "more than one row returned by a subquery used as an expression"
                    ));
                }
                scalars[ns] = (*node as *const _, values.first().copied().unwrap_or(Datum::Null), witness);
                ns += 1;
            }
            Expr::Exists(select) => {
                let found =
                    subquery_exists(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                scalars[ns] = (*node as *const _, Datum::Bool(found), Datum::Bool(false));
                ns += 1;
            }
            Expr::ArraySubquery(select) => {
                let (values, _, witness) =
                    run_subquery(select, storage, txid, arena, params, SUBQUERY_DEPTH, Some(outer))?;
                let v = build_array_scalar(values, &witness, arena)?;
                scalars[ns] = (*node as *const _, v, v);
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
        ColType::Array(element) => Datum::Array { element, raw: &[0, 0] },
        ColType::Float4 | ColType::Float8 => Datum::Float8(0.0),
        ColType::Date => Datum::Date(0),
        ColType::Timestamp => Datum::Timestamp(0),
        ColType::Timestamptz => Datum::Timestamptz(0),
        ColType::Uuid => Datum::Uuid([0; 16]),
        ColType::Text | ColType::Varchar | ColType::Bpchar | ColType::Bytea | ColType::Numeric => {
            Datum::Text("")
        }
        ColType::Range(kind) => Datum::Range { text: "empty", kind },
        ColType::Bit { varying } => Datum::Bit { bits: "", varying },
        ColType::Multirange(kind) => Datum::Multirange { text: "{}", kind },
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
    let table_star = match &select.items[0] {
        SelectItem::TableWildcard(q) => Some(*q),
        _ => None,
    };
    let item: &Expr = match &select.items[0] {
        SelectItem::Expr { expression, .. } => expression,
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => &Expr::Null,
    };
    if !select.group_by.is_empty() || select.having.is_some() || select.distinct {
        // Grouped/DISTINCT subquery: the row-source executor already handles
        // grouping, HAVING, and DISTINCT; collect its single output column.
        let mut count = 0usize;
        select_into_rows(storage, txid, select, arena, params, outer, &mut |_| {
            count += 1;
            Ok(())
        })?;
        let out = arena.alloc_slice_with(count, |_| Datum::Null).map_err(|_| arena_full())?;
        let mut at = 0usize;
        let mut any_null = false;
        select_into_rows(storage, txid, select, arena, params, outer, &mut |vals| {
            if vals.len() != 1 {
                return Err(sql_err!("42601", "subquery must return only one column"));
            }
            out[at] = vals[0];
            any_null |= vals[0].is_null();
            at += 1;
            Ok(())
        })?;
        let own_scope = select
            .from
            .as_ref()
            .and_then(|f| QueryScope::resolve_schema(storage, f, txid, arena).ok());
        let witness = match own_scope {
            Some(ref s) if !wildcard && table_star.is_none() => subquery_witness(item, Some(s)),
            _ => out.first().copied().unwrap_or(Datum::Null),
        };
        return Ok((&*out, any_null, witness));
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
        // Aggregates fold over that single virtual row (zero when WHERE is
        // false) and still yield their one output row.
        let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_aggs = 0;
        collect_aggs(item, &mut agg_nodes, &mut n_aggs)?;
        if n_aggs > 0 {
            let Some((ptrs, values)) =
                fromless_aggregate_hooks(select, &agg_nodes[..n_aggs], arena, params, &Chained { inner: &super::eval::NoColumns, outer }, &hooks)?
            else {
                return Ok((&[], false, subquery_witness(item, None)));
            };
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let base = Chained { inner: &super::eval::NoColumns, outer };
            let v = eval_full(item, arena, params, &base, &agg_hooks)?;
            let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
            return Ok((&*out, v.is_null(), subquery_witness(item, None)));
        }
        let base = Chained { inner: &super::eval::NoColumns, outer };
        if let Some(w) = select.where_clause
            && !where_passes(w, arena, params, &base, &hooks)?
        {
            return Ok((&[], false, subquery_witness(item, None)));
        }
        let v = eval_full(item, arena, params, &base, &hooks)?;
        let out = arena.alloc_slice_copy(&[v]).map_err(|_| arena_full())?;
        return Ok((&*out, v.is_null(), subquery_witness(item, None)));
    };
    let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;

    // `SELECT *` is a single-column subquery only if the source is exactly one
    // column; expand it to that column so the row-value path below applies.
    let item: &Expr = if wildcard {
        if scope.star_columns() != 1 {
            return Err(sql_err!("42601", "subquery must return only one column"));
        }
        let name = scope.output_name(scope.star_entry(0));
        arena
            .alloc(Expr::Column { qualifier: None, name })
            .map_err(|_| arena_full())?
    } else if let Some(q) = table_star {
        let t = scope.table_index(q)?;
        let def = scope.defs[t].expect("resolved");
        if def.n_columns != 1 {
            return Err(sql_err!("42601", "subquery must return only one column"));
        }
        arena
            .alloc(Expr::Column { qualifier: Some(q), name: def.columns()[0].name.as_str() })
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

    // Plain scan: collect item values (and ORDER BY keys). Two passes (count
    // then fill), then sort and apply OFFSET/LIMIT so a subquery's own ORDER BY
    // / LIMIT is honored (element order matters for ARRAY(...) and scalar).
    let n_keys = select.order_by.len();
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
    let vals = arena.alloc_slice_with(count, |_| Datum::Null).map_err(|_| arena_full())?;
    let keys = arena.alloc_slice_with(count * n_keys, |_| Datum::Null).map_err(|_| arena_full())?;
    let mut at = 0usize;
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
            let chained_row = Chained { inner: row, outer };
            vals[at] = eval_full(item, arena, params, &chained_row, &hooks)?;
            for (k, o) in select.order_by.iter().enumerate() {
                // A positional `ORDER BY 1` sorts by the single output column.
                let key = match o.expression {
                    Expr::Int(_) => vals[at],
                    e => eval_full(e, arena, params, &chained_row, &hooks)?,
                };
                keys[at * n_keys + k] = key;
            }
            at += 1;
            Ok(true)
        },
    )?;

    // Stable insertion sort of row indices by the ORDER BY keys.
    let order = arena.alloc_slice_with(count, |i| i).map_err(|_| arena_full())?;
    if n_keys > 0 {
        for x in 1..count {
            let mut y = x;
            while y > 0 {
                let a = &keys[order[y - 1] * n_keys..order[y - 1] * n_keys + n_keys];
                let b = &keys[order[y] * n_keys..order[y] * n_keys + n_keys];
                if cmp_key_rows(a, b, select.order_by) == core::cmp::Ordering::Greater {
                    order.swap(y - 1, y);
                    y -= 1;
                } else {
                    break;
                }
            }
        }
    }

    // Apply OFFSET/LIMIT over the ordered rows.
    let offset = super::exec::eval_offset_pub(select.offset, arena, params)? as usize;
    let limit = super::exec::eval_limit_pub(select.limit, arena, params)?;
    let start = offset.min(count);
    let n = ((count - start) as u64).min(limit) as usize;
    let mut saw_null = false;
    let out = arena
        .alloc_slice_with(n, |i| {
            let v = vals[order[start + i]];
            if v.is_null() {
                saw_null = true;
            }
            v
        })
        .map_err(|_| arena_full())?;
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
    let element = super::types::ArrElem::from_datum(witness)
        .or_else(|| values.iter().find_map(super::types::ArrElem::from_datum))
        .unwrap_or(super::types::ArrElem::Text);
    let ct = element.to_coltype();
    let buffer = arena
        .alloc_slice_with(values.len(), |i| values[i])
        .map_err(|_| arena_full())?;
    for v in buffer.iter_mut() {
        if !v.is_null() {
            *v = super::eval::cast_to(*v, ct, arena)?;
        }
    }
    Ok(Datum::Array { element, raw: super::array::build(buffer, arena)? })
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
            let chained_row = Chained { inner: row, outer: outer_arg };
            for (i, (_, node)) in agg_nodes.iter().enumerate() {
                states[i].update(node, arena, params, &chained_row, hooks)?;
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
    /// Ordered-set aggregates: the aggregated values come from `WITHIN GROUP
    /// (ORDER BY ...)` and are buffered (in `vals`), sorted, then reduced in
    /// `finish`. `sum_float` holds the percentile fraction.
    PercentileCont,
    PercentileDisc,
    Mode,
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
        let Expr::Call { name, star, distinct, order_by, args, .. } = node else {
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
            "percentile_cont" => AggKind::PercentileCont,
            "percentile_disc" => AggKind::PercentileDisc,
            "mode" => AggKind::Mode,
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
                // With DISTINCT, PostgreSQL permits ORDER BY only on the
                // aggregated expression itself.
                let sorts_by_argument =
                    order_by.len() == 1 && args.first().is_some_and(|a| **a == *order_by[0].expression);
                if !sorts_by_argument {
                    return Err(sql_err!(
                        "42P10",
                        "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list"
                    ));
                }
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
        let Expr::Call { args, filter, .. } = node else {
            unreachable!("validated in init");
        };
        // `FILTER (WHERE cond)` excludes rows where the condition is not true.
        if let Some(cond) = filter
            && !matches!(eval_full(cond, arena, params, row, hooks)?, Datum::Bool(true))
        {
            return Ok(());
        }
        if self.star {
            self.count += 1;
            return Ok(());
        }
        if self.kind == AggKind::StringAgg {
            return self.update_string_agg(args, arena, params, row, hooks);
        }
        // Ordered-set aggregates buffer their `WITHIN GROUP (ORDER BY expr)`
        // values (reduced in `finish`); `args[0]` is the percentile fraction.
        if matches!(
            self.kind,
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode
        ) {
            let Expr::Call { order_by, .. } = node else {
                unreachable!("validated in init");
            };
            let Some(item) = order_by.first() else {
                return Err(sql_err!("42809", "an ordered-set aggregate requires WITHIN GROUP"));
            };
            if matches!(self.kind, AggKind::PercentileCont | AggKind::PercentileDisc)
                && let Some(fraction) = args.first()
            {
                self.sum_float = match eval_full(fraction, arena, params, row, hooks)? {
                    Datum::Float8(f) => f,
                    Datum::Numeric(n) => n.to_f64(),
                    Datum::Int4(v) => f64::from(v),
                    Datum::Int8(v) => v as f64,
                    _ => return Err(sql_err!("2202E", "percentile value must be numeric")),
                };
            }
            let value = eval_full(item.expression, arena, params, row, hooks)?;
            if value.is_null() {
                return Ok(());
            }
            return self.push_distinct(value, arena);
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
            // Ordered-set aggregates buffer their values and reduce in `finish`;
            // they never fold through `accumulate`.
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode => {}
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
        let value = eval_full(args[0], arena, params, row, hooks)?;
        if value.is_null() {
            return Ok(());
        }
        let Datum::Text(val_str) = value else {
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
                tuple[1 + i] = eval_full(o.expression, arena, params, row, hooks)?;
            }
            let enc =
                super::exec::encode_projected_pub(&tuple[..1 + self.ord_spec.len()], arena)?;
            // DISTINCT (the sort key is the value itself, enforced in init):
            // encoded-tuple equality is value equality, so skip duplicates.
            if self.distinct && self.ord_len > 0 {
                let seen = unsafe { core::slice::from_raw_parts(self.ord, self.ord_len) };
                if seen.contains(&enc) {
                    return Ok(());
                }
            }
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

    /// Append `value` to the string_agg buffer, prefixing `sep` for every element
    /// after the first (first = buffer still empty).
    fn append_str_elem(&mut self, sep: &str, value: &str, arena: &'a Arena) -> Result<(), SqlError> {
        if self.str_len > 0 {
            self.push_bytes(sep.as_bytes(), arena)?;
        }
        self.push_bytes(value.as_bytes(), arena)?;
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
    /// Reduces the buffered `WITHIN GROUP` values for an ordered-set aggregate.
    fn finish_ordered_set(&mut self, arena: &'a Arena) -> Result<Datum<'a>, SqlError> {
        let n = self.vals_len;
        if n == 0 {
            return Ok(Datum::Null);
        }
        let values: &mut [Datum<'a>] = unsafe { core::slice::from_raw_parts_mut(self.vals, n) };
        // Stable insertion sort (compare_datums is fallible, so no library sort).
        for i in 1..n {
            let mut j = i;
            while j > 0 && compare_datums(&values[j - 1], &values[j])?.is_gt() {
                values.swap(j - 1, j);
                j -= 1;
            }
        }
        match self.kind {
            AggKind::Mode => {
                // Most frequent value; ties resolve to the smallest (first).
                let (mut best_index, mut best_run) = (0usize, 0usize);
                let mut i = 0;
                while i < n {
                    let mut end = i;
                    while end + 1 < n && compare_datums(&values[end], &values[end + 1])?.is_eq() {
                        end += 1;
                    }
                    if end - i + 1 > best_run {
                        best_run = end - i + 1;
                        best_index = i;
                    }
                    i = end + 1;
                }
                Ok(values[best_index])
            }
            AggKind::PercentileDisc => {
                let fraction = self.sum_float.clamp(0.0, 1.0);
                let index = if fraction <= 0.0 {
                    0
                } else {
                    ((fraction * n as f64).ceil() as usize).saturating_sub(1).min(n - 1)
                };
                Ok(values[index])
            }
            _ => {
                // PercentileCont: linear interpolation between the two nearest
                // ranks. Numeric input yields numeric; int/float yield double
                // precision (PostgreSQL's signatures).
                let fraction = self.sum_float.clamp(0.0, 1.0);
                let position = fraction * (n as f64 - 1.0);
                let low = position.floor() as usize;
                let high = position.ceil() as usize;
                let weight = position - low as f64;
                let to_f64 = |d: &Datum<'a>| -> f64 {
                    match d {
                        Datum::Int4(v) => f64::from(*v),
                        Datum::Int8(v) => *v as f64,
                        Datum::Float8(v) => *v,
                        Datum::Numeric(v) => v.to_f64(),
                        _ => 0.0,
                    }
                };
                let interpolated = to_f64(&values[low]) + (to_f64(&values[high]) - to_f64(&values[low])) * weight;
                match values[low] {
                    Datum::Numeric(_) => {
                        let text = crate::stack_format!(48, "{}", interpolated);
                        Ok(Datum::Numeric(super::numeric::Numeric::parse(text.as_str(), arena)?))
                    }
                    _ => Ok(Datum::Float8(interpolated)),
                }
            }
        }
    }

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
    /// values through `accumulate` (bumping `count` per unique value). A no-operator
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
        // Ordered-set aggregates reduce their buffered values directly.
        if matches!(
            self.kind,
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode
        ) {
            return self.finish_ordered_set(arena);
        }
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
            // Handled by `finish_ordered_set` before this match.
            AggKind::PercentileCont | AggKind::PercentileDisc | AggKind::Mode => Datum::Null,
        })
    }
}

/// The SELECT entry point (FROM present; FROM-less selects stay in the
/// engine).
/// Surfaces plan-time constant errors (e.g. `1/0`) across every expression
/// of a SELECT, matching PostgreSQL's constant folding.
pub fn check_select_constants<'a>(statement: &Select<'a>, arena: &'a Arena) -> Result<(), SqlError> {
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            super::eval::check_constant_errors(expression, arena)?;
        }
    }
    if let Some(w) = statement.where_clause {
        super::eval::check_constant_errors(w, arena)?;
    }
    for g in statement.group_by {
        super::eval::check_constant_errors(g, arena)?;
    }
    if let Some(h) = statement.having {
        super::eval::check_constant_errors(h, arena)?;
    }
    for ob in statement.order_by {
        super::eval::check_constant_errors(ob.expression, arena)?;
    }
    Ok(())
}

pub fn select_query<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    responder: &mut Responder,
) -> Outcome {
    let from = statement.from.as_ref().expect("FROM-less handled by caller");
    // Windows over a grouped query evaluate after GROUP BY / HAVING: rewrite
    // to the two-level form up front (before the result is described) and run
    // the rewritten statement instead.
    {
        let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
        let mut n_win_probe = 0;
        let mut grouped_aggs: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_grouped_aggs = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                if let Err(e) = collect_windows(expression, &mut win_probe, &mut n_win_probe) {
                    return sql_fail(e);
                }
                if let Err(e) =
                    collect_grouped_aggs(expression, &mut grouped_aggs, &mut n_grouped_aggs)
                {
                    return sql_fail(e);
                }
            }
        }
        let has_srf = find_srf(statement.items).is_some();
        if (n_win_probe > 0 || has_srf)
            && (!statement.group_by.is_empty()
                || statement.having.is_some()
                || n_grouped_aggs > 0)
        {
            let rewritten = match rewrite_grouped_windows(statement, storage, txid, arena) {
                Ok(r) => r,
                Err(e) => return sql_fail(e),
            };
            return select_query(storage, txid, rewritten, arena, params, responder);
        }
    }
    // Catalog relations (pg_catalog / information_schema) are synthesized and
    // registered as derived tables by resolve_exec, so they flow through the
    // general executor — joins, subqueries, aggregates, and ORDER BY included.
    let scope = match QueryScope::resolve_exec(storage, from, txid, arena, params) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };

    // Subqueries first (uncorrelated, evaluated once).
    let mut sub_exprs: [Option<&Expr>; 4 + 2 * super::parser::MAX_LIST] =
        [None; 4 + 2 * super::parser::MAX_LIST];
    sub_exprs[0] = statement.where_clause;
    sub_exprs[1] = statement.having;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[4 + i] = Some(expression);
        }
    }
    // ORDER BY expressions may carry (correlated) subqueries too; a bare
    // ordinal resolves to a select item already covered above.
    for (i, ob) in statement.order_by.iter().enumerate() {
        if !matches!(ob.expression, Expr::Int(_)) {
            sub_exprs[4 + super::parser::MAX_LIST + i] = Some(ob.expression);
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
        let columns = ScopeCols(&scope);
        let check = |e: &Expr| -> Result<(), SqlError> {
            super::exec::infer_type_res(e, &columns).map(|_| ())
        };
        let analyze = || -> Result<(), SqlError> {
            // SELECT-list items first: PostgreSQL analyzes types before it folds
            // constants, so an invalid aggregate/operator (e.g. `min(boolean)`)
            // errors ahead of a constant division elsewhere in the query.
            for item in statement.items {
                if let SelectItem::Expr { expression, .. } = item {
                    check(expression)?;
                }
            }
            if let Some(w) = statement.where_clause {
                check(w)?;
            }
            for g in statement.group_by {
                check(g)?;
            }
            if let Some(h) = statement.having {
                check(h)?;
            }
            for ob in statement.order_by {
                check(resolve_order_target(ob.expression, statement.items, &scope, arena)?)?;
            }
            Ok(())
        };
        if let Err(e) = analyze() {
            return sql_fail(e);
        }
    }

    // Constant folding runs after type analysis, matching PostgreSQL's
    // analyze-then-plan order: `min(boolean)` errors before `1/0` folds.
    if let Err(e) = check_select_constants(statement, arena) {
        return sql_fail(e);
    }

    // Result description.
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_scope_items(statement.items, &scope, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    patch_subquery_column_types(statement.items, Some(&scope), &outer_subs.base, params, storage, txid, arena, &mut columns[..n_cols]);
    responder.row_description(&columns[..n_cols])?;

    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };

    // LIMIT 0 returns no rows without scanning or projecting anything, as
    // PostgreSQL does — so a per-row error in an unreturned row does not
    // surface (constant errors already surfaced via the plan-time check).
    if limit == 0 {
        responder.command_complete("SELECT 0")?;
        return sql_ok();
    }

    // Window functions? They run over materialized rows before ORDER BY/LIMIT.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_windows(expression, &mut win_nodes, &mut n_win)
        {
            return sql_fail(e);
        }
    }
    if n_win > 0 {
        return window_select(
            storage, txid, statement, from, &scope, &win_nodes[..n_win], &hooks, correlated,
            &outer_subs.base, arena, params, limit, offset, responder,
        );
    }

    // Aggregates / GROUP BY?
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, &mut n_aggs) {
                return sql_fail(e);
            }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs) {
            return sql_fail(e);
        }
    if n_aggs > 0 || !statement.group_by.is_empty() {
        return grouped_select(
            storage,
            &scope,
            from,
            txid,
            statement,
            &agg_nodes[..n_aggs],
            arena,
            params,
            &hooks,
            correlated,
            limit,
            offset,
            responder,
        );
    }

    let needs_materialize = statement.distinct || !statement.order_by.is_empty();
    if !needs_materialize {
        // Stream.
        let mut emitted = 0u64;
        let mut skipped = 0u64;
        let mut wire_full = false;
        let mut wire_result: Result<(), WireFull> = Ok(());
        // With correlated subqueries, WHERE is applied per row against merged
        // hooks (which include the correlated results); otherwise the scan
        // applies WHERE directly for the common, faster path.
        let where_in_scan = if correlated.is_empty() { statement.where_clause } else { None };
        // A set-returning `_pg_expandarray(array)` expands each row into one output
        // row per array element.
        let srf_call = find_srf(statement.items);
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
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
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
                    && let Some(w) = statement.where_clause {
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
                let count = match srf_call {
                    None => 1,
                    Some(c) => srf_count(c, arena, params, row, row_hooks)?,
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
                    let use_hooks: &EvalHooks = if srf_call.is_some() {
                        srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        &srf_hooks
                    } else {
                        row_hooks
                    };
                    let mut projected = [Datum::Null; MAX_PROJ];
                    let n = project_row(statement.items, &scope, row, arena, params, use_hooks, &mut projected)?;
                    if let Err(w) = responder.data_row(&projected[..n]) {
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
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // Materialize: visible columns + hidden ORDER BY keys (set-returning
    // functions expand inside the materializer).
    materialized_select(
        storage, &scope, from, txid, statement, arena, params, &hooks, correlated, &outer_subs.base,
        limit, offset, responder,
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
/// The set of table indices (as a bitmask) an expression references. `None` if
/// it contains a construct not analyzable for pushdown (subquery, aggregate, …).
fn expr_tables(expression: &Expr, scope: &QueryScope) -> Option<u16> {
    use Expr::*;
    match expression {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | Param(_) => Some(0),
        Column { qualifier, name } => match scope.find_column(*qualifier, name).ok()? {
            ResolvedColumn::Table(t, _) => Some(1 << t),
            // Merged USING/NATURAL column: reads every contributing table.
            ResolvedColumn::Merged(m) => {
                let mc = &scope.merged[m];
                Some(mc.parts[..mc.n_parts].iter().fold(0u16, |mask, &(t, _)| mask | (1 << t)))
            }
        },
        Unary { operand, .. } | IsNull { operand, .. } | Cast { operand, .. } => {
            expr_tables(operand, scope)
        }
        Binary { left, right, .. } => Some(expr_tables(left, scope)? | expr_tables(right, scope)?),
        Between { operand, low, high, .. } => {
            Some(expr_tables(operand, scope)? | expr_tables(low, scope)? | expr_tables(high, scope)?)
        }
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            Some(expr_tables(operand, scope)? | expr_tables(pattern, scope)?)
        }
        InList { operand, list, .. } => {
            let mut m = expr_tables(operand, scope)?;
            for e in *list {
                m |= expr_tables(e, scope)?;
            }
            Some(m)
        }
        Call { args, over: None, .. } if !expression.is_aggregate() => {
            let mut m = 0;
            for a in *args {
                m |= expr_tables(a, scope)?;
            }
            Some(m)
        }
        _ => None,
    }
}

/// A cost-based execution order for a cross-join's tables (an identity order is
/// returned when reordering does not apply). PostgreSQL reorders joins by
/// selectivity; pos3ql's nested loop otherwise follows FROM order, so a table
/// with no predicate binding it to the already-joined tables (e.g. an
/// unconstrained table in the middle of the FROM list) multiplies the
/// intermediate product and turns a k-way join O(N^k). The greedy heuristic
/// picks, at each step, the remaining table that "unlocks" the most WHERE
/// conjuncts (its columns, together with the already-chosen tables, fully bind a
/// conjunct so pushdown can prune there), breaking ties by FROM order. This
/// keeps selective and equi-joined tables early and pushes unconstrained tables
/// last, without changing results (join order is free for inner/cross joins).
fn join_order(scope: &QueryScope, where_clause: Option<&Expr>) -> [usize; MAX_JOIN_TABLES] {
    let mut order = core::array::from_fn(|i| i);
    let n = scope.n;
    if n < 3 {
        return order;
    }
    // Collect the WHERE conjuncts' table masks (only analyzable ones).
    let mut masks = [0u16; MAX_CONJUNCTS];
    let mut n_masks = 0;
    if let Some(w) = where_clause {
        let mut conjunct: [&Expr; MAX_CONJUNCTS] = [w; MAX_CONJUNCTS];
        let mut nc = 0;
        let conjuncts: &[&Expr] =
            if flatten_and(w, &mut conjunct, &mut nc) { &conjunct[..nc] } else { core::slice::from_ref(&w) };
        for &c in conjuncts {
            if let Some(m) = expr_tables(c, scope)
                && n_masks < MAX_CONJUNCTS
            {
                masks[n_masks] = m;
                n_masks += 1;
            }
        }
    }
    let mut chosen_mask = 0u16;
    for slot in order.iter_mut().take(n) {
        // Among not-yet-chosen tables, pick the one unlocking the most conjuncts
        // (a conjunct is unlocked when the table is its last unbound one).
        let mut best = usize::MAX;
        let mut best_score = -1i32;
        for t in 0..n {
            if chosen_mask & (1 << t) != 0 {
                continue;
            }
            let after = chosen_mask | (1 << t);
            let mut score = 0i32;
            for &m in &masks[..n_masks] {
                if m & !chosen_mask == (1 << t) {
                    score += 1;
                }
                // Slight preference for being connected at all (bounds growth).
                if m & !after == 0 && m & (1 << t) != 0 {
                    score += 1;
                }
            }
            if score > best_score {
                best_score = score;
                best = t;
            }
        }
        *slot = best;
        chosen_mask |= 1 << best;
    }
    order
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
    if let Expr::Binary { operator: super::ast::BinaryOp::And, left, right } = e {
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
        Expr::Binary { operator, left, right } => match operator {
            Add | Sub | Mul | Div | Mod => false,
            _ => is_error_safe(left) && is_error_safe(right),
        },
        Expr::Unary { operator, operand } => matches!(operator, UnaryOp::Not) && is_error_safe(operand),
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
                .and_then(|entry| match entry {
                    ResolvedColumn::Table(t, c) => {
                        scope.defs[t].map(|d| d.columns()[c].not_null)
                    }
                    // A merged USING/NATURAL column can be null even over
                    // NOT NULL parts (outer-join null rows) — never fold.
                    ResolvedColumn::Merged(_) => None,
                })
                .unwrap_or(false) =>
        {
            Ok(&*arena.alloc(Expr::Bool(*negated)).map_err(|_| arena_full())?)
        }
        Expr::Binary { operator: operator @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let (l, r) = (fold_null(left, scope, arena)?, fold_null(right, scope, arena)?);
            if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Binary { operator: *operator, left: l, right: r })
                    .map_err(|_| arena_full())?)
            }
        }
        Expr::Unary { operator: UnaryOp::Not, operand } => {
            let o = fold_null(operand, scope, arena)?;
            if core::ptr::eq(o, *operand) {
                Ok(e)
            } else {
                Ok(&*arena
                    .alloc(Expr::Unary { operator: UnaryOp::Not, operand: o })
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
    let mut conjunct: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut n = 0;
    if !flatten_and(pred, &mut conjunct, &mut n) || n == 0 {
        return Ok(pred);
    }
    // PostgreSQL rewrites `x BETWEEN a AND b` at parse time into `x >= a AND
    // x <= b` — two *independent* top-level conjuncts that order separately
    // (each is one comparison, cheaper than a compound clause).
    let mut expanded: [&Expr; MAX_CONJUNCTS] = [pred; MAX_CONJUNCTS];
    let mut m = 0usize;
    for &c in &conjunct[..n] {
        if let Expr::Between { operand, low, high, negated: false } = c {
            if m + 2 > MAX_CONJUNCTS {
                return Ok(pred);
            }
            expanded[m] = arena
                .alloc(Expr::Binary { operator: super::ast::BinaryOp::GtEq, left: operand, right: low })
                .map_err(|_| arena_full())?;
            expanded[m + 1] = arena
                .alloc(Expr::Binary { operator: super::ast::BinaryOp::LtEq, left: operand, right: high })
                .map_err(|_| arena_full())?;
            m += 2;
        } else {
            if m + 1 > MAX_CONJUNCTS {
                return Ok(pred);
            }
            expanded[m] = c;
            m += 1;
        }
    }
    let conjunct = expanded;
    let n = m;
    if n <= 1 {
        return Ok(conjunct[0]);
    }
    // PostgreSQL routes top-level *equality* conjuncts through its
    // equivalence-class machinery, which re-appends them to the qual list
    // AFTER every other conjunct; only then does `order_qual_clauses` run its
    // stable per-tuple-cost insertion sort (verified against the PostgreSQL 18
    // source and pinned empirically — `(a%a)=a AND (…OR…)` evaluates the OR
    // first on an exact cost tie, while `0 <> (…) AND (…OR…)` keeps written
    // order). The same calibrated cost model drives projection postponement.
    let is_equality = |c: &Expr| -> bool {
        matches!(c, Expr::Binary { operator: super::ast::BinaryOp::Eq, .. })
            || matches!(c, Expr::InList { list, negated: false, .. } if list.len() == 1)
    };
    let mut order = [0usize; MAX_CONJUNCTS];
    let mut at = 0usize;
    for (i, c) in conjunct[..n].iter().enumerate() {
        if !is_equality(c) {
            order[at] = i;
            at += 1;
        }
    }
    for (i, c) in conjunct[..n].iter().enumerate() {
        if is_equality(c) {
            order[at] = i;
            at += 1;
        }
    }
    let mut cost = [0u32; MAX_CONJUNCTS];
    for (i, c) in conjunct[..n].iter().enumerate() {
        cost[i] = postpone_cost(c, scope, arena);
    }
    for i in 1..n {
        let mut j = i;
        while j > 0 && cost[order[j - 1]] > cost[order[j]] {
            order.swap(j - 1, j);
            j -= 1;
        }
    }
    // Rebuild a left-deep AND in cost order.
    let mut acc = conjunct[order[0]];
    for &i in &order[1..n] {
        acc = arena
            .alloc(Expr::Binary { operator: super::ast::BinaryOp::And, left: acc, right: conjunct[i] })
            .map_err(|_| arena_full())?;
    }
    Ok(acc)
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
    responder: &mut Responder,
) -> Outcome {
    // WITH CTEs and view references expand across the whole tree first.
    let body = match expand_set_tree_exec(q.with, q.body, storage, txid, arena, params) {
        Ok(b) => b,
        Err(e) => return sql_fail(e),
    };
    // Column names + types from the first leaf, unified across every leaf.
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_set_body(storage, body, txid, &mut columns, arena) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    let mut target = [ColType::Bool; MAX_PROJ];
    for (c, col) in columns[..n_cols].iter().enumerate() {
        target[c] = super::exec::coltype_of_oid(col.type_oid).unwrap_or(ColType::Text);
    }

    // Materialize and combine the tree.
    let rows = match eval_set_tree(body, storage, txid, arena, params, &target[..n_cols]) {
        Ok(r) => r,
        Err(e) => return sql_fail(e),
    };

    // ORDER BY (by output column position or name), then LIMIT/OFFSET.
    if let Err(e) = sort_set_rows(rows, q.order_by, &columns[..n_cols]) {
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

    responder.row_description(&columns[..n_cols])?;
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
        if responder.data_row(&out[..n_cols]).is_err() {
            return Err(WireFull);
        }
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
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
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    match &s.from {
        None => super::exec::describe_items(s.items, None, columns),
        Some(from) => {
            let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
            describe_scope_items(s.items, &scope, columns)
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
        SetTree::Op { operator, all, left, right } => {
            let l = eval_set_tree(left, storage, txid, arena, params, target)?;
            let r = eval_set_tree(right, storage, txid, arena, params, target)?;
            combine_sets(*operator, *all, l, r, arena)
        }
    }
}

/// Describes a set-operation body: column names/types come from the first leaf,
/// then each column's type is unified across every leaf (same count required).
/// On success `columns[..n]` carries the final unified OIDs/lengths. Shared by the
/// derived-table, subquery, and INSERT-source paths.
fn describe_set_body<'a>(
    storage: &'a Storage,
    tree: &'a SetTree<'a>,
    txid: u32,
    columns: &mut [ColDesc<'a>],
    arena: &'a Arena,
) -> Result<usize, SqlError> {
    let mut leaves: [Option<&Select>; MAX_SET_LEAVES] = [None; MAX_SET_LEAVES];
    let mut n_leaves = 0;
    collect_set_leaves(tree, &mut leaves, &mut n_leaves)?;
    let n_cols = describe_leaf(storage, leaves[0].expect(">=1 leaf"), txid, columns, arena)?;
    let mut target = [ColType::Bool; MAX_PROJ];
    for (c, col) in columns[..n_cols].iter().enumerate() {
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
    for (c, col) in columns[..n_cols].iter_mut().enumerate() {
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
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = describe_set_body(storage, tree, txid, &mut columns, arena)?;
    let mut tgt = [ColType::Bool; MAX_PROJ];
    for c in 0..n {
        tgt[c] = super::exec::coltype_of_oid(columns[c].type_oid).unwrap_or(ColType::Text);
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
    select_into_rows(storage, txid, s, arena, params, None, &mut |_| {
        count += 1;
        Ok(())
    })?;
    let empty: &[u8] = &[];
    let rows = arena.alloc_slice_with(count, |_| empty).map_err(|_| arena_full())?;
    let n = target.len();
    let mut at = 0usize;
    select_into_rows(storage, txid, s, arena, params, None, &mut |vals| {
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
    operator: SetOp,
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
    match operator {
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
                let mut chained_row = 0;
                while j < r.len() && r[j] == row {
                    chained_row += 1;
                    j += 1;
                }
                let times = match (operator, all) {
                    (SetOp::Intersect, true) => cl.min(chained_row),
                    (SetOp::Intersect, false) => usize::from(chained_row > 0),
                    (SetOp::Except, true) => cl.saturating_sub(chained_row),
                    (SetOp::Except, false) => usize::from(chained_row == 0),
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
    columns: &[ColDesc],
) -> Result<(), SqlError> {
    if order_by.is_empty() {
        return Ok(());
    }
    // Resolve each key to an output column index.
    let mut keys: [(usize, bool, bool); MAX_PROJ] = [(0, false, false); MAX_PROJ];
    let mut nk = 0;
    for ob in order_by {
        let index = match ob.expression {
            Expr::Int(n) if *n >= 1 && (*n as usize) <= columns.len() => (*n as usize) - 1,
            Expr::Column { name, qualifier: None } => {
                match columns.iter().position(|c| c.name == *name) {
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
        keys[nk] = (index, ob.descending, ob.nulls_first);
        nk += 1;
    }
    let keys = &keys[..nk];
    let mut err: Option<SqlError> = None;
    rows.sort_by(|a, b| {
        if err.is_some() {
            return core::cmp::Ordering::Equal;
        }
        for &(index, descending, nulls_first) in keys {
            let va = super::exec::decode_projected_pub(a, index);
            let vb = super::exec::decode_projected_pub(b, index);
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
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    responder: &mut Responder,
) -> Outcome {
    if let Err(e) = check_select_constants(statement, arena) {
        return sql_fail(e);
    }
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = match describe_items(statement.items, None, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };

    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[1 + i] = Some(expression);
        }
    }
    let subs = match prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)
    {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    patch_subquery_column_types(statement.items, None, &subs, params, storage, txid, arena, &mut columns[..n]);
    let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };

    // Aggregates (or GROUP BY / HAVING) without FROM: PostgreSQL aggregates
    // over one virtual input row (zero when WHERE is false) and emits at most
    // one output row.
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, &mut n_aggs) {
                return sql_fail(e);
            }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs)
    {
        return sql_fail(e);
    }
    if n_aggs > 0 || statement.having.is_some() || !statement.group_by.is_empty() {
        if find_srf(statement.items).is_some() {
            // The set-returning function expands after aggregation: rewrite
            // to the two-level form (aggregates in a derived table) and run
            // through the FROM executor.
            let rewritten = match rewrite_grouped_windows(statement, storage, txid, arena) {
                Ok(r) => r,
                Err(e) => return sql_fail(e),
            };
            return select_query(storage, txid, rewritten, arena, params, responder);
        }
        responder.row_description(&columns[..n])?;
        let hook_data =
            match fromless_aggregate_hooks(statement, &agg_nodes[..n_aggs], arena, params, &super::eval::NoColumns, &hooks)
            {
                Ok(d) => d,
                Err(e) => return sql_fail(e),
            };
        let mut rows = 0u64;
        if let Some((ptrs, values)) = hook_data {
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let mut vals = [Datum::Null; MAX_PROJ];
            for (i, item) in statement.items.iter().enumerate() {
                let SelectItem::Expr { expression, .. } = item else {
                    unreachable!("wildcard rejected by describe_items");
                };
                match eval_full(expression, arena, params, &super::eval::NoColumns, &agg_hooks) {
                    Ok(v) => vals[i] = v,
                    Err(e) => return sql_fail(e),
                }
            }
            responder.data_row(&vals[..statement.items.len()])?;
            rows = 1;
        }
        let tag = stack_format!(48, "SELECT {}", rows);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // A set-returning function in the select list expands the single virtual
    // row into one output row per element/value.
    let srf_call = find_srf(statement.items);
    let count = match srf_call {
        None => 1,
        Some(c) => match srf_count(c, arena, params, &super::eval::NoColumns, &hooks) {
            Ok(n) => n,
            Err(e) => return sql_fail(e),
        },
    };
    responder.row_description(&columns[..n])?;
    // Resolve ORDER BY targets against the select list: ordinals and output
    // names/expressions bind to an item (whose computed value is the key —
    // a set-returning item cannot re-evaluate outside its hook); anything
    // else evaluates per output row.
    let width = statement.items.len();
    let n_order = statement.order_by.len();
    let mut order_item: [Option<usize>; MAX_PROJ] = [None; MAX_PROJ];
    for (j, ob) in statement.order_by.iter().enumerate() {
        if let Expr::Int(pos) = ob.expression {
            if *pos < 1 || *pos as usize > width {
                return sql_fail(sql_err!(
                    "42P10",
                    "ORDER BY position {} is not in select list",
                    pos
                ));
            }
            order_item[j] = Some(*pos as usize - 1);
            continue;
        }
        order_item[j] = statement.items.iter().position(|item| {
            matches!(item, SelectItem::Expr { expression, alias }
                if **expression == *ob.expression
                    || matches!(ob.expression, Expr::Column { qualifier: None, name }
                        if *name == alias.unwrap_or(super::exec::derived_name(expression))))
        });
        if statement.distinct && order_item[j].is_none() {
            return sql_fail(sql_err!(
                "42P10",
                "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
            ));
        }
    }

    // Materialize every output row (visible values + hidden sort keys).
    let mut n_rows = 0usize;
    let max_rows = count;
    let empty: &[u8] = &[];
    let encoded = match arena.alloc_slice_with(max_rows, |_| empty) {
        Ok(e) => e,
        Err(_) => return sql_fail(arena_full()),
    };
    for k in 1..=count {
        let khooks = if srf_call.is_some() {
            EvalHooks { srf_index: Some(k), ..hooks }
        } else {
            hooks
        };
        let mut values = [Datum::Null; MAX_PROJ + MAX_PROJ];
        for (i, item) in statement.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else {
                unreachable!("wildcard rejected by describe_items");
            };
            match eval_full(expression, arena, params, &super::eval::NoColumns, &khooks) {
                Ok(v) => values[i] = v,
                Err(e) => return sql_fail(e),
            }
        }
        if let Some(w) = statement.where_clause {
            match where_passes(w, arena, params, &super::eval::NoColumns, &khooks) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(e) => return sql_fail(e),
            }
        }
        for (j, ob) in statement.order_by.iter().enumerate() {
            values[width + j] = match order_item[j] {
                Some(i) => values[i],
                None => {
                    match eval_full(ob.expression, arena, params, &super::eval::NoColumns, &khooks)
                    {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    }
                }
            };
        }
        encoded[n_rows] = match super::exec::encode_projected_pub(&values[..width + n_order], arena)
        {
            Ok(b) => b,
            Err(e) => return sql_fail(e),
        };
        n_rows += 1;
    }
    let out_rows = &mut encoded[..n_rows];

    let mut live = out_rows.len();
    if statement.distinct {
        out_rows.sort_unstable();
        let mut unique = 0usize;
        for i in 0..out_rows.len() {
            let same = i > 0
                && visible_prefix(out_rows[i], width) == visible_prefix(out_rows[i - 1], width);
            if !same {
                out_rows[unique] = out_rows[i];
                unique += 1;
            }
        }
        live = unique;
    }
    let out_rows = &mut out_rows[..live];
    if n_order > 0 {
        out_rows.sort_unstable_by(|a, b| {
            for (j, ob) in statement.order_by.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, width + j);
                let kb = super::exec::decode_projected_pub(b, width + j);
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

    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };
    let start = (offset as usize).min(out_rows.len());
    let take = ((out_rows.len() - start) as u64).min(limit) as usize;
    let mut rows = 0u64;
    for row in &out_rows[start..start + take] {
        let mut values = [Datum::Null; MAX_PROJ];
        for (i, slot) in values.iter_mut().take(width).enumerate() {
            *slot = super::exec::decode_projected_pub(row, i);
        }
        responder.data_row(&values[..width])?;
        rows += 1;
    }
    let tag = stack_format!(32, "SELECT {}", rows);
    responder.command_complete(tag.as_str())?;
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
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    emit: &mut dyn FnMut(&[Datum<'a>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if let Some(tree) = statement.set_body {
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
    check_select_constants(statement, arena)?;
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_aggs(expression, &mut agg_nodes, &mut n_aggs)?;
        }
    }
    if let Some(h) = statement.having {
        collect_aggs(h, &mut agg_nodes, &mut n_aggs)?;
    }
    // GROUP BY or aggregates: run the grouped executor (which sorts by any
    // ORDER BY and dedups DISTINCT) and emit each output row, honoring
    // LIMIT/OFFSET. A set-returning function expands after aggregation —
    // rewrite to the two-level form first.
    if (!statement.group_by.is_empty() || n_aggs > 0) && find_srf(statement.items).is_some() {
        let rewritten = rewrite_grouped_windows(statement, storage, txid, arena)?;
        return select_into_rows(storage, txid, rewritten, arena, params, outer, emit);
    }
    if !statement.group_by.is_empty() || n_aggs > 0 {
        let Some(from) = &statement.from else {
            // FROM-less aggregate: one virtual input row.
            let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
            sub_exprs[0] = statement.where_clause;
            sub_exprs[1] = statement.having;
            for (i, item) in statement.items.iter().enumerate() {
                if let SelectItem::Expr { expression, .. } = item {
                    sub_exprs[2 + i] = Some(expression);
                }
            }
            let subs =
                prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
            let hooks = EvalHooks {
                group: None,
                aggs: None,
                subs: Some(&subs),
                windows: None,
                catalog: None,
                srf_index: None,
            };
            let Some((ptrs, values)) =
                fromless_aggregate_hooks(statement, &agg_nodes[..n_aggs], arena, params, &super::eval::NoColumns, &hooks)?
            else {
                return Ok(());
            };
            let agg_hooks = EvalHooks { aggs: Some((ptrs, values)), ..hooks };
            let mut vals = [Datum::Null; MAX_PROJ];
            let mut n = 0;
            for item in statement.items {
                let SelectItem::Expr { expression, .. } = item else {
                    return Err(sql_err!(
                        "42601",
                        "SELECT * with no tables specified is not valid"
                    ));
                };
                vals[n] =
                    eval_full(expression, arena, params, &super::eval::NoColumns, &agg_hooks)?;
                n += 1;
            }
            emit(&vals[..n])?;
            return Ok(());
        };
        let scope = QueryScope::resolve_exec(storage, from, txid, arena, params)?;
        let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
        sub_exprs[0] = statement.where_clause;
        sub_exprs[1] = statement.having;
        for (i, item) in statement.items.iter().enumerate() {
            if let SelectItem::Expr { expression, .. } = item {
                sub_exprs[2 + i] = Some(expression);
            }
        }
        let outer_subs = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
        let hooks = EvalHooks { group: None, aggs: None, subs: Some(&outer_subs.base) , windows: None, catalog: None, srf_index: None };
        let (rows, width) = grouped_rows(
            storage, &scope, from, txid, statement, &agg_nodes[..n_aggs], arena, params, &hooks,
            outer_subs.correlated, outer,
        )?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        let start = (offset as usize).min(rows.len());
        let n = ((rows.len() - start) as u64).min(limit) as usize;
        for row in &rows[start..start + n] {
            let mut out = [Datum::Null; MAX_PROJ];
            for (i, slot) in out.iter_mut().take(width).enumerate() {
                *slot = super::exec::decode_projected_pub(row, i);
            }
            emit(&out[..width])?;
        }
        return Ok(());
    }
    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[1 + i] = Some(expression);
        }
    }

    let Some(from) = &statement.from else {
        // FROM-less: one row (or zero, when WHERE is false), unless a
        // set-returning function in the list expands it to several.
        let subs =
            prepare_subqueries(&sub_exprs, storage, txid, arena, params, SUBQUERY_DEPTH, None)?;
        let hooks = EvalHooks { group: None, aggs: None, subs: Some(&subs) , windows: None, catalog: None, srf_index: None };
        let srf_call = find_srf(statement.items);
        let count = match srf_call {
            None => 1,
            Some(c) => srf_count(c, arena, params, &super::eval::NoColumns, &hooks)?,
        };
        for k in 1..=count {
            let khooks = if srf_call.is_some() {
                EvalHooks { srf_index: Some(k), ..hooks }
            } else {
                hooks
            };
            if let Some(w) = statement.where_clause
                && !where_passes(w, arena, params, &super::eval::NoColumns, &khooks)?
            {
                continue;
            }
            let mut vals = [Datum::Null; MAX_PROJ];
            let mut n = 0;
            for item in statement.items {
                let SelectItem::Expr { expression, .. } = item else {
                    return Err(sql_err!("42601", "SELECT * with no tables specified is not valid"));
                };
                vals[n] = eval_full(expression, arena, params, &super::eval::NoColumns, &khooks)?;
                n += 1;
            }
            emit(&vals[..n])?;
        }
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
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_windows(expression, &mut win_nodes, &mut n_win)?;
        }
    }
    if n_win > 0 {
        // Windows over a grouped query: rewrite to the two-level form.
        let mut grouped_aggs: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_grouped_aggs = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_grouped_aggs(expression, &mut grouped_aggs, &mut n_grouped_aggs)?;
            }
        }
        if !statement.group_by.is_empty() || statement.having.is_some() || n_grouped_aggs > 0 {
            let rewritten = rewrite_grouped_windows(statement, storage, txid, arena)?;
            return select_into_rows(storage, txid, rewritten, arena, params, outer, emit);
        }
        let (proj_rows, sort_keys) = project_window_rows(
            storage, txid, statement, from, &scope, &win_nodes[..n_win], &hooks, correlated,
            &outer_subs.base, arena, params,
        )?;
        // DISTINCT dedups on the projected values; ORDER BY and LIMIT/OFFSET
        // apply here too (a derived table keeps its inner LIMIT).
        let (proj_rows, sort_keys) = if statement.distinct {
            dedup_window_rows(proj_rows, sort_keys, arena)?
        } else {
            (proj_rows, sort_keys)
        };
        let count = proj_rows.len();
        let order = arena.alloc_slice_with(count, |i| i).map_err(|_| arena_full())?;
        if !statement.order_by.is_empty() {
            for x in 1..count {
                let mut y = x;
                while y > 0 {
                    let c = cmp_key_rows(
                        sort_keys[order[y - 1]],
                        sort_keys[order[y]],
                        statement.order_by,
                    );
                    if c == core::cmp::Ordering::Greater {
                        order.swap(y - 1, y);
                        y -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        let start = (offset as usize).min(count);
        let n = ((count - start) as u64).min(limit) as usize;
        for &i in &order[start..start + n] {
            emit(proj_rows[i])?;
        }
        return Ok(());
    }

    // DISTINCT / ORDER BY / LIMIT / OFFSET need the whole set materialized
    // (so top-N and dedup are correct), then paged.
    if statement.distinct || !statement.order_by.is_empty() || statement.limit.is_some() || statement.offset.is_some() {
        let (rows, width, deferred) = materialized_rows(
            storage, &scope, from, txid, statement, arena, params, &hooks, correlated, &outer_subs.base, outer,
        )?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        // OFFSET rows flow through PostgreSQL's projection before Limit
        // discards them, so deferred items are evaluated for them too (their
        // errors surface); only rows past the offset are emitted.
        let window = offset.saturating_add(limit).min(usize::MAX as u64) as usize;
        for (index, row) in rows.iter().take(window).enumerate() {
            let mut out = [Datum::Null; MAX_PROJ];
            finalize_projected_row(
                row, width, deferred.as_ref(), statement, &scope, arena, params, &hooks, &mut out,
            )?;
            if (index as u64) >= offset {
                emit(&out[..width])?;
            }
        }
        return Ok(());
    }
    let where_in_scan = if correlated.is_empty() { statement.where_clause } else { None };

    // A set-returning `_pg_expandarray(array)` in the projection expands each
    // source row into one output row per array element.
    let srf_call = find_srf(statement.items);
    scan_source(
        storage, &scope, from, txid, where_in_scan, arena, params, &hooks, None,
        &mut |row| {
            let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
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
                if let Some(w) = statement.where_clause
                    && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                        return Ok(true);
                    }
                &row_hooks_owned
            };
            let mut projected = [Datum::Null; MAX_PROJ];
            match srf_call {
                None => {
                    let n = project_row(statement.items, &scope, row, arena, params, row_hooks, &mut projected)?;
                    emit(&projected[..n])?;
                }
                Some(c) => {
                    let count = srf_count(c, arena, params, row, row_hooks)?;
                    for k in 1..=count {
                        let srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        let n = project_row(statement.items, &scope, row, arena, params, &srf_hooks, &mut projected)?;
                        emit(&projected[..n])?;
                    }
                }
            }
            Ok(true)
        },
    )
}

/// Whether `name` is one of the supported set-returning functions.
fn is_srf_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("_pg_expandarray")
        || name.eq_ignore_ascii_case("unnest")
        || name.eq_ignore_ascii_case("generate_series")
        || name.eq_ignore_ascii_case("regexp_matches")
}

/// Finds a set-returning function call among the SELECT items (the whole call
/// node, so the caller can compute its row count), or None for a single row.
fn find_srf<'a>(items: &[SelectItem<'a>]) -> Option<&'a Expr<'a>> {
    fn walk<'a>(e: &'a Expr<'a>) -> Option<&'a Expr<'a>> {
        match e {
            Expr::Call { name, .. } if is_srf_name(name) => Some(e),
            Expr::Field { base, .. } => walk(base),
            Expr::Cast { operand, .. } => walk(operand),
            Expr::Unary { operand, .. } => walk(operand),
            Expr::Binary { left, right, .. } => walk(left).or_else(|| walk(right)),
            Expr::Call { args, .. } => args.iter().find_map(|a| walk(a)),
            _ => None,
        }
    }
    items.iter().find_map(|it| match it {
        SelectItem::Expr { expression, .. } => walk(expression),
        SelectItem::Wildcard | SelectItem::TableWildcard(_) => None,
    })
}

/// Number of output rows a set-returning call yields for the current source row.
fn srf_count<'a, R: ColumnLookup<'a>>(
    call: &'a Expr<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<usize, SqlError> {
    let Expr::Call { name, args, .. } = call else {
        return Ok(1);
    };
    let as_i64 = |d: &Datum| -> Option<i64> {
        match d {
            Datum::Int4(v) => Some(*v as i64),
            Datum::Int8(v) => Some(*v),
            _ => None,
        }
    };
    if name.eq_ignore_ascii_case("generate_series") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!("42883", "generate_series(...) argument count"));
        }
        let start = eval_full(args[0], arena, params, row, hooks)?;
        let stop = eval_full(args[1], arena, params, row, hooks)?;
        let step = if args.len() == 3 {
            eval_full(args[2], arena, params, row, hooks)?
        } else {
            Datum::Int4(1)
        };
        if start.is_null() || stop.is_null() || step.is_null() {
            return Ok(0);
        }
        let (Some(s), Some(e), Some(st)) = (as_i64(&start), as_i64(&stop), as_i64(&step)) else {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "generate_series is supported for integer arguments"
            ));
        };
        if st == 0 {
            return Err(sql_err!("22023", "step size cannot equal zero"));
        }
        let n = if st > 0 {
            if e < s { 0 } else { (e - s) / st + 1 }
        } else if e > s {
            0
        } else {
            (s - e) / (-st) + 1
        };
        Ok(n as usize)
    } else if name.eq_ignore_ascii_case("regexp_matches") {
        // Number of matches: 0/1 without the `g` flag, else all non-overlapping.
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!("42883", "regexp_matches(...) argument count"));
        }
        let string = eval_full(args[0], arena, params, row, hooks)?;
        let pattern = eval_full(args[1], arena, params, row, hooks)?;
        let (Datum::Text(string), Datum::Text(pattern)) = (string, pattern) else {
            return Ok(0);
        };
        let flags = if args.len() == 3 {
            match eval_full(args[2], arena, params, row, hooks)? {
                Datum::Text(f) => f,
                Datum::Null => return Ok(0),
                _ => "",
            }
        } else {
            ""
        };
        let (global, ci) = super::eval::regexp_flags(flags)?;
        let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
        let mut from = 0usize;
        let mut n = 0usize;
        while let Some(((mstart, mend), _)) =
            super::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        Ok(n)
    } else {
        // unnest / _pg_expandarray over an array.
        match eval_full(args[0], arena, params, row, hooks)? {
            Datum::Array { raw, .. } => Ok(super::array::len(raw)),
            Datum::Null => Ok(0),
            _ => Ok(1),
        }
    }
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
    project_row_skipping(items, None, scope, row, arena, params, hooks, out)
}

/// [`project_row`], with `skip` marking items whose evaluation is deferred
/// until after the sort (their slots stay NULL placeholders).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn project_row_skipping<'a>(
    items: &[SelectItem<'a>],
    skip: Option<&[bool; MAX_PROJ]>,
    scope: &QueryScope,
    row: &JoinRow<'_, 'a, '_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
) -> Result<usize, SqlError> {
    let mut n = 0;
    for (item_index, item) in items.iter().enumerate() {
        if skip.is_some_and(|s| s[item_index]) {
            // A postponed item occupies one slot (wildcards are never skipped).
            if n == MAX_PROJ {
                return Err(sql_err!(
                    "54000",
                    "select list expands past {} columns",
                    MAX_PROJ
                ));
            }
            out[n] = Datum::Null;
            n += 1;
            continue;
        }
        match item {
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                let vals = row.values[t].expect("bound");
                for c in 0..scope.defs[t].expect("resolved").n_columns {
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
            SelectItem::Wildcard => {
                let value_of = |t: usize, c: usize| {
                    let vals = row.values[t].expect("bound");
                    if vals.is_empty() { Datum::Null } else { vals[c] }
                };
                for k in 0..scope.star_columns() {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            "54000",
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = match scope.star_entry(k) {
                        ResolvedColumn::Table(t, c) => value_of(t, c),
                        // Merged USING/NATURAL column: first non-null side.
                        ResolvedColumn::Merged(m) => {
                            let mc = &scope.merged[m];
                            mc.parts[..mc.n_parts]
                                .iter()
                                .map(|&(t, c)| value_of(t, c))
                                .find(|v| !v.is_null())
                                .unwrap_or(Datum::Null)
                        }
                    };
                    n += 1;
                }
            }
            SelectItem::Expr { expression, .. } => {
                if n == MAX_PROJ {
                    return Err(sql_err!(
                        "54000",
                        "select list expands past {} columns",
                        MAX_PROJ
                    ));
                }
                out[n] = eval_full(expression, arena, params, row, hooks)?;
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
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                for c in scope.defs[t].expect("resolved").columns() {
                    if n == out.len() {
                        return Err(sql_err!("54000", "select list too wide"));
                    }
                    out[n] = ColDesc::of_type(c.name.as_str(), c.ctype);
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                for k in 0..scope.star_columns() {
                    if n == out.len() {
                        return Err(sql_err!("54000", "select list too wide"));
                    }
                    let entry = scope.star_entry(k);
                    out[n] =
                        ColDesc::of_type(scope.output_name(entry), scope.output_type(entry));
                    n += 1;
                }
            }
            SelectItem::Expr { expression, alias } => {
                if n == out.len() {
                    return Err(sql_err!("54000", "select list too wide"));
                }
                // Multi-table type inference: columns resolve via scope.
                let (oid, typlen) = infer_scope_type(expression, scope)?;
                let name = alias.unwrap_or(super::exec::derived_name(expression));
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
        let entry = self.0.find_column(qualifier, name)?;
        Ok(self.0.output_type(entry))
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.0.table_index(name).is_ok()
    }
}

fn infer_scope_type(expression: &Expr, scope: &QueryScope) -> Result<(i32, i16), SqlError> {
    let (oid, typlen) = super::exec::infer_type_res(expression, &ScopeCols(scope))?;
    if oid == super::types::oid::UNKNOWN {
        Ok((super::types::oid::TEXT, -1))
    } else {
        Ok((oid, typlen))
    }
}

/// Whether `target` occurs within `e` (pointer identity, expression-level
/// walk — nested subquery bodies evaluate their own subqueries).
fn expr_contains_node<'a>(e: &'a Expr<'a>, target: *const Expr<'a>) -> bool {
    if core::ptr::eq(e, target) {
        return true;
    }
    let mut found = false;
    let _ = walk_children(e, &mut |c| {
        if expr_contains_node(c, target) {
            found = true;
        }
        Ok(())
    });
    found
}

/// With correlated subqueries in WHERE, the filter cannot run inside
/// `scan_source` (their values change per row): re-evaluate the correlated
/// nodes against this row, then apply WHERE under the merged hooks. With no
/// correlated subqueries this is a no-op (the scan already filtered).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn row_passes_correlated_where<'a>(
    correlated: &'a [&'a Expr<'a>],
    where_clause: Option<&'a Expr<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    row: &JoinRow<'_, 'a, '_>,
) -> Result<bool, SqlError> {
    if correlated.is_empty() {
        return Ok(true);
    }
    let Some(w) = where_clause else {
        return Ok(true);
    };
    let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
    let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
        [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
    let base = hooks.subs.expect("outer subqueries prepared");
    let row_subs =
        merge_correlated(correlated, base, row, storage, txid, arena, params, &mut sc, &mut ls)?;
    let h = EvalHooks { subs: Some(&row_subs), ..*hooks };
    where_passes(w, arena, params, row, &h)
}

/// The outer-row lookup for a correlated subquery re-evaluated per GROUP:
/// a reference resolves to a grouping key's value for this group (a key
/// collapsed by the current grouping set reads NULL). Anything else is
/// PostgreSQL's ungrouped-column error (42803).
struct GroupRow<'g, 'a> {
    group_by: &'a [&'a Expr<'a>],
    keys: &'g [Datum<'a>],
}

impl<'a> ColumnLookup<'a> for GroupRow<'_, 'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        for (g, v) in self.group_by.iter().zip(self.keys) {
            if let Expr::Column { qualifier: gq, name: gn } = g
                && *gn == name
                && (qualifier.is_none() || gq.is_none() || *gq == qualifier)
            {
                return Ok(*v);
            }
        }
        Err(sql_err!(
            "42803",
            "subquery uses ungrouped column \"{}{}{}\" from outer query",
            qualifier.unwrap_or(""),
            if qualifier.is_some() { "." } else { "" },
            name
        ))
    }
}

/// Aggregates and emits the output rows for a single grouping-set `mask` (bit
/// *i* set = `group_by[i]` participates; a cleared bit collapses that column to
/// NULL so every row shares one group and the output column reads NULL). Returns
/// the surviving rows (visible columns followed by hidden ORDER BY key columns),
/// unsorted — the caller concatenates the sets and sorts once.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn groups_for_mask<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    mask: u64,
    row_count: usize,
    agg_ptrs: &'a [*const Expr<'a>],
    order_exprs: &[Option<&'a Expr<'a>>],
    width: usize,
    n_order: usize,
) -> Result<&'a [&'a [u8]], SqlError> {
    let n_keys = statement.group_by.len();
    // WHERE with correlated subqueries is applied per row in the callbacks.
    let scan_where = if correlated.is_empty() { statement.where_clause } else { None };

    // Pass 2: encode group keys per row (columns outside this set → NULL).
    let empty: &[u8] = &[];
    let keys: &mut [(&[u8], u32)] = arena
        .alloc_slice_with(row_count, |_| (empty, 0u32))
        .map_err(|_| arena_full())?;
    {
        let mut at = 0usize;
        scan_source(
            storage, scope, from, txid, scan_where, arena, params, hooks,
            outer,
            &mut |row| {
                if !row_passes_correlated_where(
                    correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
                )? {
                    return Ok(true);
                }
                // Correlated subqueries inside the grouping keys re-evaluate
                // against each input row.
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_store;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    hooks
                } else {
                    row_subs = merge_correlated(
                        correlated,
                        hooks.subs.expect("outer subqueries prepared"),
                        row,
                        storage,
                        txid,
                        arena,
                        params,
                        &mut sc,
                        &mut ls,
                    )?;
                    row_hooks_store = EvalHooks { subs: Some(&row_subs), ..*hooks };
                    &row_hooks_store
                };
                let mut key_vals = [Datum::Null; MAX_PROJ];
                for (k, g) in statement.group_by.iter().enumerate() {
                    if mask & (1u64 << k) != 0 {
                        key_vals[k] = eval_full(g, arena, params, row, row_hooks)?;
                    }
                }
                keys[at].0 = super::exec::encode_projected_pub(&key_vals[..n_keys], arena)?;
                keys[at].1 = at as u32;
                at += 1;
                Ok(true)
            },
        )?;
    }
    keys.sort_unstable();

    let n_groups = {
        let mut g = 0usize;
        for i in 0..keys.len() {
            if i == 0 || keys[i].0 != keys[i - 1].0 {
                g += 1;
            }
        }
        if keys.is_empty() && mask == 0 {
            1 // grand total (no active grouping columns): one row even over zero input rows
        } else {
            g
        }
    };
    let group_of: &mut [u32] = arena
        .alloc_slice_with(row_count, |_| 0u32)
        .map_err(|_| arena_full())?;
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

    // Correlated subquery nodes appearing in the select list, HAVING, or the
    // ORDER BY keys re-evaluate per group (their outer references resolve to
    // the group's keys); the rest were WHERE-level and already applied.
    let mut group_correlated_buffer: [&Expr; MAX_SUBQUERIES] = [&Expr::Null; MAX_SUBQUERIES];
    let mut n_group_correlated = 0usize;
    for &node in correlated {
        let in_group_clauses = statement.items.iter().any(|item| {
            matches!(item, SelectItem::Expr { expression, .. }
                if expr_contains_node(expression, node as *const Expr))
        }) || statement.having.is_some_and(|h| expr_contains_node(h, node as *const Expr))
            || order_exprs.iter().take(n_order).any(|oe| {
                oe.is_some_and(|o| expr_contains_node(o, node as *const Expr))
            });
        if in_group_clauses && n_group_correlated < MAX_SUBQUERIES {
            group_correlated_buffer[n_group_correlated] = node;
            n_group_correlated += 1;
        }
    }
    let group_correlated = &group_correlated_buffer[..n_group_correlated];

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
            storage, scope, from, txid, scan_where, arena, params, hooks,
            outer,
            &mut |row| {
                if !row_passes_correlated_where(
                    correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
                )? {
                    return Ok(true);
                }
                // Correlated subqueries inside aggregate arguments re-evaluate
                // against each input row.
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
                let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
                let row_subs;
                let row_hooks_store;
                let row_hooks: &EvalHooks = if correlated.is_empty() {
                    hooks
                } else {
                    row_subs = merge_correlated(
                        correlated,
                        hooks.subs.expect("outer subqueries prepared"),
                        row,
                        storage,
                        txid,
                        arena,
                        params,
                        &mut sc,
                        &mut ls,
                    )?;
                    row_hooks_store = EvalHooks { subs: Some(&row_subs), ..*hooks };
                    &row_hooks_store
                };
                let g = group_of.get(at).copied().unwrap_or(0) as usize;
                for (i, (_, node)) in agg_nodes.iter().enumerate() {
                    states[g * n_aggs + i].update(node, arena, params, row, row_hooks)?;
                }
                at += 1;
                Ok(true)
            },
        )?;
    }

    let out_rows: &mut [&[u8]] = arena
        .alloc_slice_with(n_groups, |_| empty)
        .map_err(|_| arena_full())?;
    let mut survivors = 0usize;
    for g in 0..n_groups {
        let mut key_vals = [Datum::Null; MAX_PROJ];
        if !keys.is_empty() {
            let rep = keys
                .iter()
                .find(|(_, index)| group_of[*index as usize] as usize == g)
                .expect("group non-empty");
            for (k, slot) in key_vals.iter_mut().enumerate().take(n_keys) {
                *slot = super::exec::decode_projected_pub(rep.0, k);
            }
        }
        let mut agg_vals = [Datum::Null; MAX_AGGS];
        for i in 0..n_aggs {
            agg_vals[i] = states[g * n_aggs.max(1) + i].finish(arena)?;
        }
        let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
        let mut ls: [(*const Expr, &[Datum], bool, Datum); MAX_SUBQUERIES] =
            [(core::ptr::null(), &[], false, Datum::Null); MAX_SUBQUERIES];
        let merged_subs;
        let group_subs = if group_correlated.is_empty() {
            hooks.subs
        } else {
            let group_row = GroupRow { group_by: statement.group_by, keys: &key_vals[..n_keys] };
            merged_subs = merge_correlated(
                group_correlated,
                hooks.subs.expect("outer subqueries prepared"),
                &group_row,
                storage,
                txid,
                arena,
                params,
                &mut sc,
                &mut ls,
            )?;
            Some(&merged_subs)
        };
        let group_hooks = EvalHooks {
            group: Some((statement.group_by, &key_vals[..n_keys], mask)),
            aggs: Some((agg_ptrs, &agg_vals[..n_aggs])),
            subs: group_subs,
        windows: None, catalog: None, srf_index: None };
        if let Some(h) = statement.having {
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
        for (n, item) in statement.items.iter().enumerate() {
            let SelectItem::Expr { expression, .. } = item else { unreachable!() };
            full[n] = eval_full(expression, arena, params, &super::eval::NoColumns, &group_hooks)?;
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
    Ok(&out_rows[..survivors])
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
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<(&'a [&'a [u8]], usize), SqlError> {
    // Validate: non-aggregate select items must be GROUP BY expressions.
    for item in statement.items {
        let SelectItem::Expr { expression, .. } = item else {
            return Err(sql_err!(
                "42803",
                "SELECT * must appear in the GROUP BY clause or be used in an aggregate function"
            ));
        };
        if !expr_is_grouped(expression, statement.group_by) {
            return Err(sql_err!(
                "42803",
                "column must appear in the GROUP BY clause or be used in an aggregate function"
            ));
        }
    }

    // Pass 1: count rows, so group storage can be arena-allocated. WHERE
    // with correlated subqueries is applied per row here too, so every pass
    // sees the same filtered sequence.
    let scan_where = if correlated.is_empty() { statement.where_clause } else { None };
    let mut row_count = 0usize;
    scan_source(
        storage, scope, from, txid, scan_where, arena, params, hooks,
        outer,
        &mut |row| {
            if !row_passes_correlated_where(
                correlated, statement.where_clause, storage, txid, arena, params, hooks, row,
            )? {
                return Ok(true);
            }
            row_count += 1;
            Ok(true)
        },
    )?;

    let n_keys = statement.group_by.len();
    let width = statement.items.len();
    let n_order = statement.order_by.len();
    let n_aggs = agg_nodes.len();
    // Aggregate-call node addresses, resolved once (shared by every set).
    let agg_ptrs: &[*const Expr] = arena
        .alloc_slice_with(n_aggs, |i| agg_nodes[i].0)
        .map_err(|_| arena_full())?;
    // ORDER BY over groups: ordinals resolve to select items; keys evaluate
    // under the group hooks (so aggregates work). Resolved once.
    let mut order_arr: [Option<&Expr>; MAX_PROJ] = [None; MAX_PROJ];
    for (k, ob) in statement.order_by.iter().enumerate() {
        order_arr[k] = Some(resolve_order_target(ob.expression, statement.items, scope, arena)?);
    }
    let order_exprs = &order_arr[..n_order];

    // Grouping sets: the explicit mask list, or a single implicit set of all
    // grouping columns for a plain GROUP BY / plain aggregate.
    let all_mask = if n_keys >= 64 { u64::MAX } else { (1u64 << n_keys) - 1 };
    let single = [all_mask];
    let masks: &[u64] = if statement.grouping_sets.is_empty() {
        &single[..]
    } else {
        statement.grouping_sets
    };
    if masks.len() > super::parser::MAX_GROUPING_SETS {
        return Err(sql_err!("54000", "too many grouping sets"));
    }

    // Aggregate each set independently, then concatenate (a single set is a
    // straight copy). ORDER BY applies across the combined result, so it is
    // deferred until after concatenation.
    let empty_rows: &[&[u8]] = &[];
    let mut per_set: [&[&[u8]]; super::parser::MAX_GROUPING_SETS] =
        [empty_rows; super::parser::MAX_GROUPING_SETS];
    let mut total = 0usize;
    for (si, &mask) in masks.iter().enumerate() {
        let rows = groups_for_mask(
            storage, scope, from, txid, statement, agg_nodes, arena, params, hooks, correlated,
            outer, mask, row_count, agg_ptrs, order_exprs, width, n_order,
        )?;
        per_set[si] = rows;
        total += rows.len();
    }

    let empty: &[u8] = &[];
    let out_rows: &mut [&[u8]] = arena
        .alloc_slice_with(total, |_| empty)
        .map_err(|_| arena_full())?;
    let mut at = 0usize;
    for rows in &per_set[..masks.len()] {
        for &r in rows.iter() {
            out_rows[at] = r;
            at += 1;
        }
    }

    let mut live = out_rows.len();
    if statement.distinct {
        // DISTINCT over the grouped output. ORDER BY keys must be select-list
        // members (as in PostgreSQL); then dedupe on the visible prefix.
        for oe in order_exprs.iter() {
            let target = oe.expect("resolved");
            let in_list = statement.items.iter().any(|item| {
                matches!(item, SelectItem::Expr { expression, .. } if **expression == *target)
            });
            if !in_list {
                return Err(sql_err!(
                    "42P10",
                    "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
                ));
            }
        }
        out_rows.sort_unstable();
        let mut unique = 0usize;
        for i in 0..out_rows.len() {
            let same = i > 0
                && visible_prefix(out_rows[i], width) == visible_prefix(out_rows[i - 1], width);
            if !same {
                out_rows[unique] = out_rows[i];
                unique += 1;
            }
        }
        live = unique;
    }
    let out_rows = &mut out_rows[..live];

    if n_order > 0 {
        out_rows.sort_unstable_by(|a, b| {
            for (k, ob) in statement.order_by.iter().enumerate() {
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
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    correlated: &'a [&'a Expr<'a>],
    limit: u64,
    offset: u64,
    responder: &mut Responder,
) -> Outcome {
    let (out_rows, width) = match grouped_rows(
        storage, scope, from, txid, statement, agg_nodes, arena, params, hooks, correlated, None,
    ) {
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
        responder.data_row(&out[..width])?;
        emitted += 1;
    }
    let tag = stack_format!(48, "SELECT {}", emitted);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Does this item expression consist only of grouped expressions,
/// aggregates, and constants?
fn expr_is_grouped(expression: &Expr, group_by: &[&Expr]) -> bool {
    if group_by.iter().any(|g| **g == *expression) || expression.is_aggregate() {
        return true;
    }
    match expression {
        Expr::Column { .. } | Expr::WholeRow(_) => false,
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::NumericLit(_) | Expr::Str(_)
        | Expr::BitLit(_) | Expr::Param(_) | Expr::DefaultMarker | Expr::Subquery(_) | Expr::Exists(_)
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
/// Evaluation cost of a select-list expression in half-operator units,
/// approximating PostgreSQL's `cost_qual_eval`: each operator or function
/// application costs 2, an implicit numeric-family coercion costs 2, an IN
/// list costs 1 per element (PostgreSQL charges half an operator per element),
/// and AND/OR/IS NULL cost nothing. Used for the sort/limit projection
/// postponement decision (threshold empirically pinned against PostgreSQL 18.4:
/// items costing more than 10 operators are projected above the Sort + Limit).
/// PostgreSQL's `find_duplicate_ors` canonicalization: AND terms common to
/// every arm of an OR are factored out in front — `(A AND B) OR (A AND C)`
/// becomes `A AND (B OR C)`, and when an arm consists *only* of common terms
/// the whole OR collapses to them (`(A AND B) OR A` ≡ `A`), so the dropped
/// arms' other conjuncts are never evaluated.
fn factor_common_or_terms<'a>(
    e: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    use super::ast::BinaryOp;
    const MAX_PARTS: usize = 16;
    fn flatten_or<'a>(x: &'a Expr<'a>, out: &mut [&'a Expr<'a>; MAX_PARTS], n: &mut usize) -> bool {
        if let Expr::Binary { operator: BinaryOp::Or, left, right } = x {
            return flatten_or(left, out, n) && flatten_or(right, out, n);
        }
        if *n == MAX_PARTS {
            return false;
        }
        out[*n] = x;
        *n += 1;
        true
    }
    fn and_terms<'a>(x: &'a Expr<'a>, out: &mut [&'a Expr<'a>; MAX_PARTS], n: &mut usize) -> bool {
        if let Expr::Binary { operator: BinaryOp::And, left, right } = x {
            return and_terms(left, out, n) && and_terms(right, out, n);
        }
        if *n == MAX_PARTS {
            return false;
        }
        out[*n] = x;
        *n += 1;
        true
    }
    let dummy = e;
    let mut arms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_arms = 0;
    if !flatten_or(e, &mut arms, &mut n_arms) || n_arms < 2 {
        return Ok(e);
    }
    // Terms of the first arm that appear in every other arm.
    let mut common: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_common = 0;
    if !and_terms(arms[0], &mut common, &mut n_common) {
        return Ok(e);
    }
    let mut kept = 0usize;
    'term: for i in 0..n_common {
        for arm in &arms[1..n_arms] {
            let mut terms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
            let mut nt = 0;
            if !and_terms(arm, &mut terms, &mut nt) {
                return Ok(e);
            }
            if !terms[..nt].iter().any(|t| **t == *common[i]) {
                continue 'term;
            }
        }
        common[kept] = common[i];
        kept += 1;
    }
    if kept == 0 {
        return Ok(e);
    }
    // Residue of each arm (its terms minus the common ones). An empty residue
    // means that arm is implied by the common terms: the OR collapses.
    let mut residues: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
    let mut n_res = 0;
    for arm in &arms[..n_arms] {
        let mut terms: [&Expr; MAX_PARTS] = [dummy; MAX_PARTS];
        let mut nt = 0;
        let _ = and_terms(arm, &mut terms, &mut nt);
        let mut residue: Option<&Expr> = None;
        for &t in &terms[..nt] {
            if common[..kept].iter().any(|c| **c == *t) {
                continue;
            }
            residue = Some(match residue {
                None => t,
                Some(acc) => arena
                    .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: t })
                    .map_err(|_| arena_full())?,
            });
        }
        match residue {
            None => {
                // This arm is exactly the common terms: OR collapses to them.
                let mut acc = common[0];
                for &c in &common[1..kept] {
                    acc = arena
                        .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: c })
                        .map_err(|_| arena_full())?;
                }
                return Ok(acc);
            }
            Some(x) => {
                residues[n_res] = x;
                n_res += 1;
            }
        }
    }
    // AND(common..., OR(residues...)).
    let mut or_acc = residues[0];
    for &x in &residues[1..n_res] {
        or_acc = arena
            .alloc(Expr::Binary { operator: BinaryOp::Or, left: or_acc, right: x })
            .map_err(|_| arena_full())?;
    }
    let mut acc = common[0];
    for &c in &common[1..kept] {
        acc = arena
            .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: c })
            .map_err(|_| arena_full())?;
    }
    Ok(&*arena
        .alloc(Expr::Binary { operator: BinaryOp::And, left: acc, right: or_acc })
        .map_err(|_| arena_full())?)
}

/// The plan-time boolean value of a condition, when PostgreSQL's
/// `eval_const_expressions` can decide it: a constant subtree, or an AND/OR
/// settled by one constant side. `None` = not decidable at plan time.
fn plan_time_bool(e: &Expr, arena: &Arena) -> Option<bool> {
    use super::ast::BinaryOp;
    if e.is_constant() {
        return match super::eval::eval(e, arena, super::eval::NO_PARAMS, &super::eval::NoColumns) {
            Ok(Datum::Bool(b)) => Some(b),
            _ => None,
        };
    }
    match e {
        Expr::Binary { operator: BinaryOp::And, left, right } => {
            if plan_time_bool(left, arena) == Some(false)
                || plan_time_bool(right, arena) == Some(false)
            {
                Some(false)
            } else {
                None
            }
        }
        Expr::Binary { operator: BinaryOp::Or, left, right } => {
            if plan_time_bool(left, arena) == Some(true)
                || plan_time_bool(right, arena) == Some(true)
            {
                Some(true)
            } else {
                None
            }
        }
        Expr::Unary { operator: super::ast::UnaryOp::Not, operand } => {
            plan_time_bool(operand, arena).map(|b| !b)
        }
        _ => None,
    }
}

/// PostgreSQL's plan-time boolean simplification applied to a qual: an AND arm
/// folding TRUE (or an OR arm folding FALSE) is dropped, and a decided
/// connective collapses to its constant. This exposes a nested AND to the
/// top-level conjunct ordering — `(a AND b) OR const-false` orders `a`/`b` by
/// cost just as PostgreSQL does after simplifying the OR away.
fn simplify_qual<'a>(e: &'a Expr<'a>, arena: &'a Arena) -> Result<&'a Expr<'a>, SqlError> {
    use super::ast::BinaryOp;
    if let Some(b) = plan_time_bool(e, arena) {
        return Ok(if b { &Expr::Bool(true) } else { &Expr::Bool(false) });
    }
    match e {
        Expr::Binary { operator: operator @ (BinaryOp::And | BinaryOp::Or), left, right } => {
            let keep_true = matches!(operator, BinaryOp::And);
            let l = simplify_qual(left, arena)?;
            let r = simplify_qual(right, arena)?;
            // The decided-connective cases returned above, so at most one side
            // is the droppable constant here.
            if *l == Expr::Bool(keep_true) {
                return Ok(r);
            }
            if *r == Expr::Bool(keep_true) {
                return Ok(l);
            }
            let rebuilt: &Expr = if core::ptr::eq(l, *left) && core::ptr::eq(r, *right) {
                e
            } else {
                arena
                    .alloc(Expr::Binary { operator: *operator, left: l, right: r })
                    .map_err(|_| arena_full())?
            };
            if matches!(operator, BinaryOp::Or) {
                return factor_common_or_terms(rebuilt, arena);
            }
            Ok(rebuilt)
        }
        // NOT pushes through the connectives (De Morgan), exposing the pieces
        // to top-level conjunct ordering exactly as PostgreSQL's
        // `canonicalize_qual` does: `NOT (x OR y IS NOT NULL)` becomes
        // `NOT x AND y IS NULL`, so the cheap null test can filter first.
        Expr::Unary { operator: super::ast::UnaryOp::Not, operand } => {
            let negated: &Expr = match *operand {
                Expr::Binary { operator: BinaryOp::Or, left, right } => {
                    let nl = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: left })
                        .map_err(|_| arena_full())?;
                    let nr = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: right })
                        .map_err(|_| arena_full())?;
                    arena
                        .alloc(Expr::Binary { operator: BinaryOp::And, left: nl, right: nr })
                        .map_err(|_| arena_full())?
                }
                Expr::Binary { operator: BinaryOp::And, left, right } => {
                    let nl = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: left })
                        .map_err(|_| arena_full())?;
                    let nr = arena
                        .alloc(Expr::Unary { operator: super::ast::UnaryOp::Not, operand: right })
                        .map_err(|_| arena_full())?;
                    arena
                        .alloc(Expr::Binary { operator: BinaryOp::Or, left: nl, right: nr })
                        .map_err(|_| arena_full())?
                }
                Expr::Unary { operator: super::ast::UnaryOp::Not, operand: inner } => inner,
                Expr::IsNull { operand: inner, negated } => arena
                    .alloc(Expr::IsNull { operand: inner, negated: !negated })
                    .map_err(|_| arena_full())?,
                _ => return Ok(e),
            };
            simplify_qual(negated, arena)
        }
        _ => Ok(e),
    }
}

fn postpone_cost(e: &Expr, scope: &QueryScope, arena: &Arena) -> u32 {
    use Expr::*;
    // PostgreSQL costs the *plan-time-folded* expression: a fully-constant
    // subtree has become a Const by then and costs nothing.
    if e.is_constant() {
        return 0;
    }
    let oid_of = |x: &Expr| -> Option<i32> {
        super::exec::infer_type_res(x, &ScopeCols(scope)).ok().map(|t| t.0)
    };
    // Numeric-family promotion rank: the lower-ranked operand is the one
    // PostgreSQL casts (int → numeric → float8).
    let rank = |o: i32| -> Option<u32> {
        use super::types::oid;
        Some(match o {
            oid::INT2 => 0,
            oid::INT4 => 1,
            oid::INT8 => 2,
            oid::NUMERIC => 3,
            oid::FLOAT4 => 4,
            oid::FLOAT8 => 5,
            _ => return None,
        })
    };
    // One implicit cast (2 half-ops) when a numeric-family pair mixes types —
    // free when the coerced side is a constant, which PostgreSQL folds into a
    // pre-cast Const at plan time.
    let coercion = |l: &Expr, r: &Expr| -> u32 {
        match (oid_of(l).and_then(rank), oid_of(r).and_then(rank)) {
            (Some(a), Some(b)) if a != b => {
                let coerced = if a < b { l } else { r };
                if coerced.is_constant() { 0 } else { 2 }
            }
            _ => 0,
        }
    };
    match e {
        Null | Bool(_) | Int(_) | Float(_) | NumericLit(_) | Str(_) | BitLit(_) | Param(_)
        | DefaultMarker | Column { .. } | WholeRow(_) => 0,
        Unary { operator: super::ast::UnaryOp::Not, operand } => postpone_cost(operand, scope, arena),
        Unary { operand, .. } => postpone_cost(operand, scope, arena) + 2,
        IsNull { operand, .. } => postpone_cost(operand, scope, arena),
        Cast { operand, .. } => postpone_cost(operand, scope, arena) + 2,
        Binary { operator: super::ast::BinaryOp::And | super::ast::BinaryOp::Or, left, right } => {
            postpone_cost(left, scope, arena) + postpone_cost(right, scope, arena)
        }
        Binary { left, right, .. } => {
            postpone_cost(left, scope, arena) + postpone_cost(right, scope, arena) + 2 + coercion(left, right)
        }
        Between { operand, low, high, .. } => {
            postpone_cost(operand, scope, arena)
                + postpone_cost(low, scope, arena)
                + postpone_cost(high, scope, arena)
                + 4
                + coercion(operand, low)
                + coercion(operand, high)
        }
        InList { operand, list, .. } => {
            // PostgreSQL rewrites a one-element IN to plain `=` (one operator);
            // longer lists cost half an operator per element (= ANY(array)).
            let applications = if list.len() <= 1 { 2 } else { list.len() as u32 };
            postpone_cost(operand, scope, arena)
                + list.iter().map(|x| postpone_cost(x, scope, arena)).sum::<u32>()
                + applications
        }
        Like { operand, pattern, .. } | Match { operand, pattern, .. } => {
            postpone_cost(operand, scope, arena) + postpone_cost(pattern, scope, arena) + 2
        }
        Call { args, .. } => args.iter().map(|a| postpone_cost(a, scope, arena)).sum::<u32>() + 2,
        Case { operand, whens, otherwise } => {
            let mut c = operand.map_or(0, |o| postpone_cost(o, scope, arena));
            // Non-constant branch results whose type differs from the CASE's
            // unified result type carry an implicit cast, which PostgreSQL
            // counts (a constant result is pre-cast at plan time).
            let case_rank = oid_of(e).and_then(rank);
            let result_cast = |result: &Expr| -> u32 {
                if result.is_constant() {
                    return 0;
                }
                match (oid_of(result).and_then(rank), case_rank) {
                    (Some(a), Some(b)) if a != b => 2,
                    _ => 0,
                }
            };
            for (cond, result) in whens.iter() {
                // PostgreSQL's plan-time simplification drops a WHEN whose
                // condition folds to constant FALSE, and truncates the CASE at
                // one folding to constant TRUE.
                match plan_time_bool(cond, arena) {
                    Some(false) => continue,
                    Some(true) => {
                        c += postpone_cost(result, scope, arena) + result_cast(result);
                        return c;
                    }
                    None => {}
                }
                c += postpone_cost(cond, scope, arena) + postpone_cost(result, scope, arena);
                c += result_cast(result);
                // The simple form compares the operand per WHEN.
                if operand.is_some() {
                    c += 2;
                }
            }
            if let Some(o) = otherwise {
                c += postpone_cost(o, scope, arena) + result_cast(o);
            }
            c
        }
        Array(items) => items.iter().map(|x| postpone_cost(x, scope, arena)).sum(),
        Subscript { base, index } => postpone_cost(base, scope, arena) + postpone_cost(index, scope, arena),
        Field { base, .. } => postpone_cost(base, scope, arena),
        AnyAll { operand, array, .. } => {
            let elements = if let Array(items) = array { items.len() as u32 } else { 20 };
            postpone_cost(operand, scope, arena) + postpone_cost(array, scope, arena) + elements
        }
        // Subqueries carry a subplan's cost in PostgreSQL and are postponed.
        Subquery(_) | Exists(_) | ArraySubquery(_) | InSubquery { .. } => 1000,
    }
}

/// A flat decoded source row (every column of every scope table, in scope
/// order) resolvable by name, for evaluating postponed projection items after
/// the sort.
struct RawRow<'s, 'd, 'a> {
    scope: &'s QueryScope<'d>,
    values: &'s [Datum<'a>],
}

impl<'a> ColumnLookup<'a> for RawRow<'_, '_, 'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        // The raw row stores every table's columns concatenated in scope
        // order (merges hide nothing here).
        let flat_of = |t: usize, c: usize| -> usize {
            (0..t).map(|i| self.scope.defs[i].expect("resolved").n_columns).sum::<usize>() + c
        };
        match self.scope.find_column(qualifier, name)? {
            ResolvedColumn::Table(t, c) => Ok(self.values[flat_of(t, c)]),
            // Merged USING/NATURAL column: the first non-null contributor.
            ResolvedColumn::Merged(m) => {
                let mc = &self.scope.merged[m];
                for &(t, c) in &mc.parts[..mc.n_parts] {
                    let v = self.values[flat_of(t, c)];
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

/// Which projection items an ORDER BY + LIMIT query defers until after the
/// sort: `postponed` flags the deferred items, and the raw source columns are
/// appended to each encoded row starting at `raw_at`.
struct PostponedProjection {
    postponed: [bool; MAX_PROJ],
    raw_at: usize,
    n_raw: usize,
}

/// Fills `out` with an encoded row's visible columns, evaluating any postponed
/// projection items from the row's appended raw source columns. Only rows that
/// survive LIMIT/OFFSET reach this, in sorted order — the whole point of the
/// postponement.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn finalize_projected_row<'a>(
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
        *slot = super::exec::decode_projected_pub(bytes, i);
    }
    let Some(d) = deferred else { return Ok(()) };
    let mut raw = [Datum::Null; MAX_COLUMNS * MAX_JOIN_TABLES];
    for (k, slot) in raw.iter_mut().enumerate().take(d.n_raw) {
        *slot = super::exec::decode_projected_pub(bytes, d.raw_at + k);
    }
    let raw_row = RawRow { scope, values: &raw[..d.n_raw] };
    // `postponed` is indexed by item; wildcards (never postponed) advance the
    // output slot by their column count.
    let mut slot = 0usize;
    for (i, item) in statement.items.iter().enumerate() {
        match item {
            SelectItem::Wildcard => slot += scope.star_columns(),
            SelectItem::TableWildcard(q) => {
                slot += scope.defs[scope.table_index(q)?].expect("resolved").n_columns;
            }
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

#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn materialized_rows<'a>(
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
    // With correlated subqueries WHERE is applied per row (against merged
    // hooks); otherwise the scan applies it directly.
    let where_in_scan = if correlated.is_empty() { statement.where_clause } else { None };

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
                "42P10",
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
                    "42P10",
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
            let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
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
                if let Some(w) = statement.where_clause
                    && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                        return Ok(true);
                    }
                &row_hooks_owned
            };
            let expansions = match srf_call {
                None => 1,
                Some(c) => srf_count(c, arena, params, row, row_hooks)?,
            };
            for k in 1..=expansions {
                let srf_hooks;
                let use_hooks: &EvalHooks = if srf_call.is_some() {
                    srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                    &srf_hooks
                } else {
                    row_hooks
                };
                let mut projected = [Datum::Null; MAX_PROJ];
                project_row_skipping(
                    statement.items, if any_postponed { Some(&postponed) } else { None },
                    scope, row, arena, params, use_hooks, &mut projected,
                )?;
                for oe in order_exprs.iter().take(n_keys) {
                    eval_full(oe.expect("resolved"), arena, params, row, use_hooks)?;
                }
                count += 1;
            }
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
                let mut sc: [(*const Expr, Datum, Datum); MAX_SUBQUERIES] =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
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
                    if let Some(w) = statement.where_clause
                        && !where_passes(w, arena, params, row, &row_hooks_owned)? {
                            return Ok(true);
                        }
                    &row_hooks_owned
                };
                let expansions = match srf_call {
                    None => 1,
                    Some(c) => srf_count(c, arena, params, row, row_hooks)?,
                };
                for k in 1..=expansions {
                    let srf_hooks;
                    let use_hooks: &EvalHooks = if srf_call.is_some() {
                        srf_hooks = EvalHooks { srf_index: Some(k), ..*row_hooks };
                        &srf_hooks
                    } else {
                        row_hooks
                    };
                    let mut projected = [Datum::Null; MAX_PROJ];
                    let n = project_row_skipping(
                        statement.items, if any_postponed { Some(&postponed) } else { None },
                        scope, row, arena, params, use_hooks, &mut projected,
                    )?;
                    debug_assert_eq!(n, width);
                    let mut full =
                        [Datum::Null; MAX_PROJ + MAX_PROJ + MAX_COLUMNS * MAX_JOIN_TABLES];
                    full[..width].copy_from_slice(&projected[..width]);
                    for (key, oe) in order_exprs.iter().take(n_keys).enumerate() {
                        full[width + key] =
                            eval_full(oe.expect("resolved"), arena, params, row, use_hooks)?;
                    }
                    // Raw source columns for deferred projection after the sort.
                    if any_postponed {
                        let mut flat = width + n_keys;
                        for t in 0..scope.n {
                            let def = scope.defs[t].expect("resolved");
                            let vals = row.values[t].expect("bound");
                            for c in 0..def.n_columns {
                                full[flat] = if vals.is_empty() { Datum::Null } else { vals[c] };
                                flat += 1;
                            }
                        }
                    }
                    rows[at] = super::exec::encode_projected_pub(
                        &full[..width + n_keys + n_raw],
                        arena,
                    )?;
                    at += 1;
                }
                Ok(true)
            },
        )?;
    }

    // Plain DISTINCT dedups on the visible prefix before the ORDER BY sort.
    // DISTINCT ON dedups on its keys *after* the sort (below), so the first
    // row per key in ORDER BY order survives.
    let mut live = rows.len();
    if statement.distinct && n_on == 0 {
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

    // Sort by ORDER BY (a stable sort so DISTINCT ON without ORDER BY keeps
    // the first-scanned row per key). For DISTINCT ON the ON keys are appended
    // ascending as a tiebreak — a no-op when ORDER BY already begins with them
    // (as PostgreSQL requires), but it groups equal keys when ORDER BY is
    // absent so the run dedup below works.
    if n_order > 0 || n_on > 0 {
        rows.sort_by(|a, b| {
            for (k, ob) in statement.order_by.iter().enumerate() {
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
            for j in 0..n_on {
                let ka = super::exec::decode_projected_pub(a, width + n_order + j);
                let kb = super::exec::decode_projected_pub(b, width + n_order + j);
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
        });
    }

    // DISTINCT ON: keep the first row of each run of equal ON keys.
    let rows: &mut [&[u8]] = if n_on > 0 {
        let mut unique = 0usize;
        for i in 0..rows.len() {
            let same = i > 0
                && (0..n_on).all(|j| {
                    let ka = super::exec::decode_projected_pub(rows[i], width + n_order + j);
                    let kb = super::exec::decode_projected_pub(rows[i - 1], width + n_order + j);
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
        n_raw,
    });
    Ok((rows, width, deferred))
}

/// DISTINCT / ORDER BY execution to the wire: materialize the rows, then page
/// with LIMIT/OFFSET and emit.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn materialized_select<'a>(
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
    let window = offset.saturating_add(limit).min(usize::MAX as u64) as usize;
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
            let chained_row = Chained { inner: jr, outer: Some(target) };
            on_match(&chained_row)?;
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
    col_alias: Option<&'a [&'a str]>,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a TableDef, SqlError> {
    let mut descriptors = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match sub.set_body {
        Some(tree) => describe_set_body(storage, tree, txid, &mut descriptors, arena)?,
        None => match &sub.from {
            Some(f) => {
                let ss = QueryScope::resolve_schema(storage, f, txid, arena)?;
                let n = describe_scope_items(sub.items, &ss, &mut descriptors)?;
                // A bare scalar/array subquery item (possibly correlated) has
                // no static type from the scope and describes as text; infer
                // its real type from the inner select's projection so the
                // derived-table column is typed correctly.
                let mut slot = 0usize;
                for item in sub.items {
                    match item {
                        SelectItem::Wildcard => slot += ss.star_columns(),
                        SelectItem::TableWildcard(q) => {
                            slot += ss.defs[ss.table_index(q)?].expect("resolved").n_columns;
                        }
                        SelectItem::Expr { expression, .. } => {
                            if slot < n
                                && descriptors[slot].type_oid == super::types::oid::TEXT
                                && let Expr::Subquery(inner_sub) = &**expression
                                && let Some(SelectItem::Expr { expression: inner, .. }) =
                                    inner_sub.items.first()
                            {
                                let inner_scope = inner_sub.from.as_ref().and_then(|inf| {
                                    QueryScope::resolve_schema(storage, inf, txid, arena).ok()
                                });
                                let witness = subquery_witness(
                                    inner,
                                    inner_scope.as_ref().or(Some(&ss)),
                                );
                                if !witness.is_null() {
                                    descriptors[slot] = ColDesc::new(
                                        descriptors[slot].name,
                                        witness.type_oid(),
                                        -1,
                                    );
                                }
                            }
                            slot += 1;
                        }
                    }
                }
                n
            }
            None => describe_items(sub.items, None, &mut descriptors)?,
        },
    };
    if n_cols > MAX_COLUMNS {
        return Err(sql_err!(
            "54011",
            "derived table \"{}\" has too many columns",
            exposed
        ));
    }
    // A column-alias list renames the output columns; PostgreSQL requires it to
    // supply no more names than the derived table has columns.
    if let Some(aliases) = col_alias {
        if aliases.len() > n_cols {
            return Err(sql_err!(
                "42P10",
                "table \"{}\" has {} columns available but {} columns specified",
                exposed,
                n_cols,
                aliases.len()
            ));
        }
        for (i, alias) in aliases.iter().enumerate() {
            descriptors[i].name = alias;
        }
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
        let ct = super::exec::coltype_of_oid(descriptors[i].type_oid).ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "derived table column \"{}\" type (oid {}) is not supported",
                descriptors[i].name,
                descriptors[i].type_oid
            )
        })?;
        columns[i] = ColumnMeta {
            name: SqlName::parse(descriptors[i].name)?,
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
fn table_func_def<'a>(
    tref: &'a TableRef<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a TableDef, SqlError> {
    let is_gs = tref.table.eq_ignore_ascii_case("generate_series");
    let is_unnest = tref.table.eq_ignore_ascii_case("unnest");
    let is_re = tref.table.eq_ignore_ascii_case("regexp_matches");
    if !is_gs && !is_unnest && !is_re {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "table function \"{}\" is not supported",
            tref.table
        ));
    }
    let name = tref.alias.unwrap_or(if is_gs {
        "generate_series"
    } else if is_re {
        "regexp_matches"
    } else {
        "unnest"
    });
    // A table function has a single output column, so at most one alias.
    if let Some(aliases) = tref.col_alias
        && aliases.len() > 1
    {
        return Err(sql_err!(
            "42P10",
            "table \"{}\" has 1 columns available but {} columns specified",
            name,
            aliases.len()
        ));
    }
    // The column takes the explicit column-alias if given, else the table name.
    let col_name = tref.col_alias.and_then(|a| a.first().copied()).unwrap_or(name);
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
    // generate_series yields int8; regexp_matches yields text[]; unnest yields
    // the array's element type.
    let ctype = if is_gs {
        ColType::Int8
    } else if is_re {
        ColType::Array(super::types::ArrElem::Text)
    } else {
        let args = tref.func_args.unwrap_or(&[]);
        match args.first() {
            Some(e) => match super::eval::eval(e, arena, params, &super::eval::NoColumns)? {
                Datum::Array { element, .. } => element.to_coltype(),
                _ => ColType::Text,
            },
            None => ColType::Text,
        }
    };
    let mut columns = [blank; MAX_COLUMNS];
    columns[0] = ColumnMeta { name: SqlName::parse(col_name)?, ctype, ..blank };
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
    // regexp_matches(string, pattern [, flags]): one row per match, each a
    // text[] of the capture groups (or the whole match when there are no groups).
    if tref.table.eq_ignore_ascii_case("regexp_matches") {
        if !(2..=3).contains(&args.len()) {
            return Err(sql_err!("42883", "regexp_matches(...) argument count"));
        }
        let string = super::eval::eval(args[0], arena, params, &super::eval::NoColumns)?;
        let pattern = super::eval::eval(args[1], arena, params, &super::eval::NoColumns)?;
        let (Datum::Text(string), Datum::Text(pattern)) = (string, pattern) else {
            return Ok(&[]);
        };
        let flags = if args.len() == 3 {
            match super::eval::eval(args[2], arena, params, &super::eval::NoColumns)? {
                Datum::Text(f) => f,
                Datum::Null => return Ok(&[]),
                _ => "",
            }
        } else {
            ""
        };
        let (global, ci) = super::eval::regexp_flags(flags)?;
        // Collect each match's encoded text[] row.
        const EMPTY: &[u8] = &[];
        let mut rows = [EMPTY; super::parser::MAX_LIST];
        let mut n = 0usize;
        let mut spans = [(-1i64, -1i64); super::regex::MAX_GROUPS];
        let mut from = 0usize;
        while let Some(((mstart, mend), ng)) =
            super::regex::find_captures(pattern, string, from, ci, &mut spans)?
        {
            let mut elems = [Datum::Null; super::regex::MAX_GROUPS];
            let count = if ng == 0 {
                elems[0] = Datum::Text(&string[mstart..mend]);
                1
            } else {
                for (i, span) in spans[..ng].iter().enumerate() {
                    elems[i] = if span.0 < 0 {
                        Datum::Null
                    } else {
                        Datum::Text(&string[span.0 as usize..span.1 as usize])
                    };
                }
                ng
            };
            let arr = Datum::Array {
                element: super::types::ArrElem::Text,
                raw: super::array::build(&elems[..count], arena)?,
            };
            if n == super::parser::MAX_LIST {
                return Err(sql_err!("54000", "too many regexp_matches rows"));
            }
            rows[n] = super::exec::encode_projected_pub(&[arr], arena)?;
            n += 1;
            if !global {
                break;
            }
            from = if mend > mstart { mend } else { mend + 1 };
            if from > string.len() {
                break;
            }
        }
        let out = arena.alloc_slice_with(n, |i| rows[i]).map_err(|_| arena_full())?;
        return Ok(&*out);
    }
    // unnest(array): one row per element.
    if tref.table.eq_ignore_ascii_case("unnest") {
        let (element, raw) = match super::eval::eval(args[0], arena, params, &super::eval::NoColumns)? {
            Datum::Array { element, raw } => (element, raw),
            Datum::Null => return Ok(&[]),
            _ => return Err(sql_err!("42883", "unnest requires an array argument")),
        };
        let count = super::array::len(raw);
        const EMPTY: &[u8] = &[];
        let rows = arena.alloc_slice_with(count, |_| EMPTY).map_err(|_| arena_full())?;
        for (i, slot) in rows.iter_mut().enumerate() {
            let v = super::array::get(raw, element, i).unwrap_or(Datum::Null);
            *slot = super::exec::encode_projected_pub(&[v], arena)?;
        }
        return Ok(&*rows);
    }
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
