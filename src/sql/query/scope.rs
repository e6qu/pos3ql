//! Resolving a FROM clause into the scope a query's columns are looked up in.
//!
//! A [`QueryScope`] names every table, derived table, table function and
//! materialized CTE a query reads, in join order, and answers what a column
//! reference means: which of them owns it, whether it is ambiguous, and whether
//! `USING`/`NATURAL` merged it with a column of the same name on the other side
//! (a [`MergedColumn`], which is what `SELECT *` shows and what an unqualified
//! reference resolves to). It is built twice over: a schema-only form for
//! describe, and an executing form that also materializes derived tables.

use crate::mem::arena::Arena;
use crate::sql::ast::{BinaryOp, Expr, FromClause, MAX_USING_COLUMNS, MaterializedCte, TableRef};
use crate::sql::eval::{ColumnLookup, SqlError, sqlstate};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{ColumnMeta, MAX_COLUMNS, SqlName, Storage, TableDef, UserTypeName};
use crate::util::StackStr;

use super::{
    Chained, MAX_AGGS, MAX_JOIN_TABLES, MAX_WINDOWS, arena_full, collect_aggs, collect_windows,
    common_using_type, select_into_rows, synth_derived_def, synth_derived_def_outer,
    table_func_def, table_func_def_outer, table_func_rows_outer,
};

/// Upper bound on distinct USING/NATURAL-merged columns across a join tree
/// (chained merges of the same name allocate a fresh entry per join).
pub const MAX_MERGED_COLUMNS: usize = 32;

fn validate_table_sample(
    storage: &Storage,
    table: &TableRef<'_>,
    txid: u32,
) -> Result<(), SqlError> {
    let Some(sample) = table.sample else {
        return Ok(());
    };
    for expression in [Some(sample.percentage), sample.repeatable]
        .into_iter()
        .flatten()
    {
        let mut reference: Option<(bool, StackStr<128>)> = None;
        expression.for_each_column_reference(&mut |qualifier, name| {
            if reference.is_none() {
                reference = Some((
                    qualifier.is_some(),
                    StackStr::from_str(qualifier.unwrap_or(name)),
                ));
            }
        });
        if let Some((qualified, name)) = reference {
            return Err(if qualified {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "invalid reference to FROM-clause entry for table \"{}\"",
                    name.as_str()
                )
            } else {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name.as_str()
                )
            });
        }
        let mut aggregates = [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut aggregate_count = 0;
        collect_aggs(
            expression,
            &mut aggregates,
            &mut aggregate_count,
            storage,
            txid,
        )?;
        if aggregate_count != 0 {
            return Err(sql_err!(
                sqlstate::GROUPING_ERROR,
                "aggregate functions are not allowed in functions in FROM"
            ));
        }
        let mut windows = [&Expr::Null; MAX_WINDOWS];
        let mut window_count = 0;
        collect_windows(expression, &mut windows, &mut window_count, storage, txid)?;
        if window_count != 0 {
            return Err(sql_err!(
                sqlstate::WINDOWING_ERROR,
                "window functions are not allowed in functions in FROM"
            ));
        }
        if super::srf::expression_has_project_set(expression, storage, txid) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "argument of TABLESAMPLE must not return a set"
            ));
        }
    }
    Ok(())
}

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
    pub names: &'d mut [&'d str],
    pub defs: &'d mut [Option<&'d TableDef>],
    pub slots: &'d mut [usize],
    /// Effective authorization role for each physical relation. Stored views
    /// populate this with their owner; ordinary references leave it empty and
    /// execute as the session's current role.
    pub authorization_roles: &'d mut [Option<u16>],
    /// Derived tables (`FROM (SELECT ...) alias`): the materialized rows,
    /// self-describing-encoded. `None` marks a physical table (scanned from
    /// storage by `slots`).
    pub derived: &'d mut [Option<&'d [&'d [u8]]>],
    /// Object-backed derived rows. `derived` remains `Some` for these entries
    /// so decoding and relation-kind checks take the derived-table path; the
    /// empty arena slice is only a marker, not the row authority.
    pub(crate) external_runs: &'d mut [Option<crate::sql::external::ExternalRun>],
    /// A `LATERAL` FROM item: its rows depend on the outer row, so they are
    /// **not** pre-materialized (`derived` holds an empty placeholder). The
    /// scan re-runs the item's body per outer row, resolving its outer column
    /// references against the tables to its left.
    pub lateral: &'d mut [bool],
    /// Marks a set-returning-function scan (`FROM func(args)`), whose output row
    /// type is its single scalar column — so a whole-row reference to the table
    /// alias yields that scalar, not a one-field record (which is how a
    /// subquery- or storage-derived table's whole-row reference behaves).
    pub func_scalar: &'d mut [bool],
    pub n: usize,
    /// USING/NATURAL-merged join columns (see `MergedColumn`).
    pub merged: &'d mut [MergedColumn<'d>],
    pub n_merged: usize,
    /// The join tree's output columns in PostgreSQL's order — each
    /// USING/NATURAL join hoists its merged columns to the front and hides
    /// the per-side copies. `n_output == 0` means no merges anywhere: the
    /// output is every table's columns in scope order (the common case, kept
    /// implicit).
    output: &'d mut [ResolvedColumn],
    n_output: usize,
    /// Synthesized USING/NATURAL equality predicates, indexed like
    /// `FromClause::joins` (whose `on` is None for such joins). Filled only
    /// on the executor path (predicate synthesis needs an arena).
    pub join_on: &'d mut [Option<&'d Expr<'d>>],
}

/// Type-only lookup over FROM items already registered in a scope. Table
/// functions are implicitly lateral in PostgreSQL, so their output definition
/// may depend on a preceding column before a concrete row has been bound.
struct ScopeTypes<'s, 'd>(&'s QueryScope<'d>);

impl<'a> ColumnLookup<'a> for ScopeTypes<'_, '_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        self.0.find_column(qualifier, name)?;
        Ok(Datum::Null)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.0
            .find_column(qualifier, name)
            .ok()
            .map(|column| self.0.output_type(column))
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.0
            .find_column(qualifier, name)
            .ok()
            .map(|column| self.0.output_collation(column))
            .unwrap_or(crate::sql::ast::Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> crate::sql::ast::Collation {
        crate::sql::exec::record_field_metadata(base, field, &super::ScopeCols(self.0))
            .map_or(crate::sql::ast::Collation::None, |meta| meta.collation)
    }

    fn column_user_type(&self, qualifier: Option<&str>, name: &str) -> Option<UserTypeName> {
        match self.0.find_column(qualifier, name).ok()? {
            ResolvedColumn::Table(table, column) => {
                self.0.defs[table]?.columns().get(column)?.user_type
            }
            ResolvedColumn::Merged(_) => None,
        }
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.0.func_scalar_type(table).is_some()
    }
}

impl<'d> QueryScope<'d> {
    fn empty(arena: &'d Arena, from: &FromClause<'d>) -> Result<Self, SqlError> {
        let table_count = from.joins.len() + 1;
        let merged_capacity = from
            .joins
            .iter()
            .map(|join| {
                if join.natural {
                    MAX_USING_COLUMNS
                } else {
                    join.using_columns.map_or(0, <[&str]>::len)
                }
            })
            .sum::<usize>()
            .min(MAX_MERGED_COLUMNS);
        let has_merges = merged_capacity != 0;
        let names = arena
            .alloc_slice_with(table_count, |_| "")
            .map_err(|_| arena_full())?;
        let defs = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let slots = arena
            .alloc_slice_with(table_count, |_| 0)
            .map_err(|_| arena_full())?;
        let authorization_roles = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let derived = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let external_runs = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let lateral = arena
            .alloc_slice_with(table_count, |_| false)
            .map_err(|_| arena_full())?;
        let func_scalar = arena
            .alloc_slice_with(table_count, |_| false)
            .map_err(|_| arena_full())?;
        let merged = arena
            .alloc_slice_with(merged_capacity.max(1), |_| MergedColumn {
                name: "",
                parts: [(0, 0); MAX_JOIN_TABLES],
                n_parts: 0,
                ctype: ColType::Bool,
            })
            .map_err(|_| arena_full())?;
        let output = arena
            .alloc_slice_with(
                if has_merges {
                    table_count * MAX_COLUMNS
                } else {
                    1
                },
                |_| ResolvedColumn::Table(0, 0),
            )
            .map_err(|_| arena_full())?;
        let join_on = arena
            .alloc_slice_with(from.joins.len().max(1), |_| None)
            .map_err(|_| arena_full())?;
        Ok(QueryScope {
            names,
            defs,
            slots,
            authorization_roles,
            derived,
            external_runs,
            lateral,
            func_scalar,
            n: 0,
            merged,
            n_merged: 0,
            output,
            n_output: 0,
            join_on,
        })
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
        Self::resolve_exec_outer(storage, from, txid, arena, params, None)
    }

    /// Execution scope with an enclosing row available while FROM items are
    /// resolved. Correlated subqueries need this for a base table function
    /// such as `unnest(outer.array)`.
    pub fn resolve_exec_outer<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<QueryScope<'a>, SqlError> {
        let mut scope = QueryScope::empty(arena, from)?;
        scope.add_exec(storage, &from.base, txid, arena, params, outer)?;
        for j in from.joins {
            scope.add_exec(storage, &j.table, txid, arena, params, outer)?;
        }
        scope.build_merges(from, arena, Some(arena))?;
        Ok(scope)
    }

    /// Registers a materialized recursive CTE reference: a synthesized
    /// `TableDef` from the CTE's column names/types, plus its precomputed rows.
    /// `materialize` false = schema only (Describe path).
    fn add_materialized<'a>(
        &mut self,
        storage: &Storage,
        tref: &'a TableRef<'a>,
        m: &'a MaterializedCte<'a>,
        txid: u32,
        arena: &'a Arena,
        materialize: bool,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let exposed = tref.alias.unwrap_or(tref.table);
        if !exposed.is_empty() && self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let ncols = m.column_names.len();
        if ncols > MAX_COLUMNS {
            return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "too many columns"));
        }
        if let Some(aliases) = tref.col_alias
            && aliases.len() > ncols
        {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
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
            let (ctype, user_type) =
                crate::sql::exec::catalog_column_type(storage, txid, m.column_types[i].0)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "CTE column \"{}\" type (oid {}) is not supported",
                            name,
                            m.column_types[i].0
                        )
                    })?;
            *slot = ColumnMeta {
                name: SqlName::parse(name)?,
                ctype,
                type_mod: m.column_types[i].2,
                collation: m.column_collations[i],
                user_type,
                ..ColumnMeta::EMPTY
            };
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
        self.derived[self.n] = Some(if materialize { m.rows() } else { &[] });
        self.external_runs[self.n] = if materialize { m.external_run() } else { None };
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
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        validate_table_sample(storage, tref, txid)?;
        if tref.sample.is_some()
            && (tref.cte.is_some()
                || tref.is_function_source()
                || tref.subquery.is_some()
                || matches!(
                    storage.resolve_relation(tref.schema, tref.table, txid),
                    Some(crate::storage::ResolvedRelation::Catalog)
                ))
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "TABLESAMPLE clause can only be applied to tables and materialized views"
            ));
        }
        if let Some(m) = tref.cte {
            return self.add_materialized(storage, tref, m, txid, arena, true);
        }
        if tref.is_function_source() {
            return self.add_table_func(storage, tref, txid, arena, params, true, outer);
        }
        let Some(sub) = tref.subquery else {
            if matches!(
                storage.resolve_relation(tref.schema, tref.table, txid),
                Some(crate::storage::ResolvedRelation::Catalog)
            ) {
                return self.add_catalog(storage, tref, txid, arena, true);
            }
            return self.add(storage, tref, txid, arena);
        };
        let exposed = tref.alias.expect("parser requires a derived-table alias");
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        // A LATERAL subquery's def is typed against the tables to its left
        // (a body may project an outer column); a plain derived table is typed
        // on its own. Its rows are materialized per outer row by the scan, so
        // register an empty placeholder here rather than running it now.
        if tref.lateral {
            let def_reference = synth_derived_def_outer(
                storage,
                sub,
                exposed,
                tref.col_alias,
                txid,
                arena,
                Some(self),
            )?;
            self.names[self.n] = exposed;
            self.defs[self.n] = Some(def_reference);
            self.derived[self.n] = Some(&[]);
            self.lateral[self.n] = true;
            self.slots[self.n] = usize::MAX;
            self.n += 1;
            return Ok(());
        }
        let def_reference = synth_derived_def(storage, sub, exposed, tref.col_alias, txid, arena)?;
        let rows: &'a [&'a [u8]];
        let external_run;
        if storage.spill_attached() {
            // A stable all-equal sort is an insertion-order spool. Resetting
            // one leased producer cannot disturb a nested child's producer.
            let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
            let mut sorter = storage.external_sorter()?;
            sorter.reset();
            select_into_rows(
                storage,
                txid,
                sub,
                arena,
                params,
                None,
                None,
                &mut |values| {
                    storage
                        .with_block_store(|blocks| {
                            sorter.push_projected_by(
                                blocks,
                                values.len(),
                                |column| values[column],
                                &mut compare,
                            )
                        })
                        .expect("spill-attached block store")
                },
            )?;
            external_run = storage
                .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
                .expect("spill-attached block store")?;
            rows = &[];
        } else {
            // Local-only mode has no authoritative external tier: retain the
            // existing arena representation and fail loudly at its bound.
            const EMPTY: &[u8] = &[];
            let mut store: *mut &[u8] = core::ptr::null_mut();
            let mut len = 0usize;
            let mut cap = 0usize;
            select_into_rows(
                storage,
                txid,
                sub,
                arena,
                params,
                None,
                None,
                &mut |values| {
                    let encoded = crate::sql::exec::encode_projected_pub(values, arena)?;
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
                    unsafe { store.add(len).write(encoded) };
                    len += 1;
                    Ok(())
                },
            )?;
            rows = if len == 0 {
                &[]
            } else {
                unsafe { core::slice::from_raw_parts(store, len) }
            };
            external_run = None;
        }
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_reference);
        self.derived[self.n] = Some(rows);
        self.external_runs[self.n] = external_run;
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
        let mut scope = QueryScope::empty(arena, from)?;
        scope.add_schema(storage, &from.base, txid, arena)?;
        for j in from.joins {
            scope.add_schema(storage, &j.table, txid, arena)?;
        }
        scope.build_merges(from, arena, None)?;
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
        validate_table_sample(storage, tref, txid)?;
        if tref.sample.is_some()
            && (tref.cte.is_some()
                || tref.is_function_source()
                || tref.subquery.is_some()
                || matches!(
                    storage.resolve_relation(tref.schema, tref.table, txid),
                    Some(crate::storage::ResolvedRelation::Catalog)
                ))
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "TABLESAMPLE clause can only be applied to tables and materialized views"
            ));
        }
        if let Some(m) = tref.cte {
            return self.add_materialized(storage, tref, m, txid, arena, false);
        }
        if tref.is_function_source() {
            return self.add_table_func(storage, tref, txid, arena, &[], false, None);
        }
        let Some(sub) = tref.subquery else {
            if matches!(
                storage.resolve_relation(tref.schema, tref.table, txid),
                Some(crate::storage::ResolvedRelation::Catalog)
            ) {
                return self.add_catalog(storage, tref, txid, arena, false);
            }
            return self.add(storage, tref, txid, arena);
        };
        let exposed = tref.alias.expect("parser requires a derived-table alias");
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let def_reference = if tref.lateral {
            synth_derived_def_outer(
                storage,
                sub,
                exposed,
                tref.col_alias,
                txid,
                arena,
                Some(self),
            )?
        } else {
            synth_derived_def(storage, sub, exposed, tref.col_alias, txid, arena)?
        };
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_reference);
        // No rows: this scope is never scanned, only described. An empty row
        // set keeps a stray scan safe rather than reading a physical slot.
        self.derived[self.n] = Some(&[]);
        self.lateral[self.n] = tref.lateral;
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
        txid: u32,
        arena: &'a Arena,
        materialize: bool,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let synth = crate::sql::catalog::synthesize(storage, tref.schema, tref.table, txid, arena)?;
        let exposed = tref.alias.unwrap_or(tref.table);
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let def_reference = if let Some(aliases) = tref.col_alias {
            if aliases.len() > synth.def.n_columns {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "table \"{}\" has {} columns available but {} columns specified",
                    exposed,
                    synth.def.n_columns,
                    aliases.len()
                ));
            }
            let mut renamed = *synth.def;
            for (column, alias) in renamed.columns.iter_mut().zip(aliases) {
                column.name = SqlName::parse(alias)?;
            }
            &*arena.alloc(renamed).map_err(|_| arena_full())?
        } else {
            synth.def
        };
        let rows: &'a [&'a [u8]] = if materialize {
            const EMPTY: &[u8] = &[];
            let encoded = arena
                .alloc_slice_with(synth.rows.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (i, r) in synth.rows.iter().enumerate() {
                encoded[i] = crate::sql::exec::encode_projected_pub(r, arena)?;
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
    #[allow(clippy::too_many_arguments)]
    fn add_table_func<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        materialize: bool,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let scope_types = ScopeTypes(self);
        let columns = Chained {
            inner: if self.n == 0 {
                &crate::sql::eval::NoColumns
            } else {
                &scope_types
            },
            outer,
        };
        let def_reference = if self.n == 0 && outer.is_none() {
            table_func_def(tref, storage, txid, arena, params)?
        } else {
            table_func_def_outer(tref, storage, txid, arena, params, &columns)?
        };
        let exposed = tref.alias.unwrap_or(tref.table);
        if !exposed.is_empty() && self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        // Every table function is implicitly lateral to preceding FROM items;
        // the keyword is optional for functions in PostgreSQL. Such rows are
        // built per left-hand row by the scan.
        if tref.lateral || self.n > 0 {
            self.names[self.n] = exposed;
            self.defs[self.n] = Some(def_reference);
            self.derived[self.n] = Some(&[]);
            self.lateral[self.n] = true;
            self.func_scalar[self.n] = true;
            self.slots[self.n] = usize::MAX;
            self.n += 1;
            return Ok(());
        }
        let rows: &'a [&'a [u8]] = if !materialize {
            &[]
        } else if outer.is_some() {
            table_func_rows_outer(
                tref, storage, txid, arena, params, &columns, None, None, None,
            )?
        } else {
            table_func_rows_outer(
                tref,
                storage,
                txid,
                arena,
                params,
                &crate::sql::eval::NoColumns,
                None,
                None,
                None,
            )?
        };
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def_reference);
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.func_scalar[self.n] = true;
        self.n += 1;
        Ok(())
    }

    pub(crate) fn add(
        &mut self,
        storage: &'d Storage,
        tref: &'d TableRef<'d>,
        txid: u32,
        arena: &'d Arena,
    ) -> Result<(), SqlError> {
        // `txid == 0` (schema-only / Describe) resolves against the committed
        // catalog; a real transaction sees its own uncommitted CREATE/DROP.
        let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(tref.schema, tref.table, txid)
        else {
            return Err(match tref.schema {
                Some(s) => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}.{}\" does not exist",
                    s,
                    tref.table
                ),
                None => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    tref.table
                ),
            });
        };
        let stored_def = storage.table_def(slot, txid);
        let exposed = tref.alias.unwrap_or(stored_def.name.as_str());
        let def = if let Some(aliases) = tref.col_alias {
            if aliases.len() > stored_def.n_columns {
                return Err(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "table \"{}\" has {} columns available but {} columns specified",
                    exposed,
                    stored_def.n_columns,
                    aliases.len()
                ));
            }
            let mut renamed = *stored_def;
            for (column, alias) in renamed.columns.iter_mut().zip(aliases) {
                column.name = SqlName::parse(alias)?;
            }
            &*arena.alloc(renamed).map_err(|_| arena_full())?
        } else {
            stored_def
        };
        // Two same-named entries coexist only when both are *unaliased base
        // tables of different schemas* (their references then need the
        // three-part spelling); any other duplicate — aliases, the same
        // table twice — is PostgreSQL's 42712.
        for t in 0..self.n {
            if self.names[t] != exposed {
                continue;
            }
            let both_distinct_tables = tref.alias.is_none()
                && self.defs[t].is_some_and(|d| {
                    self.slots[t] != usize::MAX
                        && d.name.as_str() == exposed
                        && d.schema.as_str() != def.schema.as_str()
                });
            if !both_distinct_tables {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_ALIAS,
                    "table name \"{}\" specified more than once",
                    exposed
                ));
            }
        }
        self.names[self.n] = exposed;
        self.defs[self.n] = Some(def);
        self.slots[self.n] = slot;
        self.authorization_roles[self.n] = tref.authorization_role;
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
        scratch_arena: &'d Arena,
        expression_arena: Option<&'d Arena>,
    ) -> Result<(), SqlError> {
        if !from
            .joins
            .iter()
            .any(|j| j.natural || j.using_columns.is_some())
        {
            return Ok(());
        }
        // The left join tree's output columns, updated join by join.
        let out = scratch_arena
            .alloc_slice_with(self.output.len(), |_| ResolvedColumn::Table(0, 0))
            .map_err(|_| arena_full())?;
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
                    if right_def.column_index(name).is_some() && !using[..n_using].contains(&name) {
                        if n_using == MAX_USING_COLUMNS {
                            return Err(sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
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
                                crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
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
                        sqlstate::UNDEFINED_FUNCTION,
                        "operator does not exist: {} = {}",
                        left_type.name(),
                        right_type.name()
                    ));
                };
                if self.n_merged == MAX_MERGED_COLUMNS {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
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
                if let Some(arena) = expression_arena {
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
                            .alloc(Expr::Binary {
                                operator: BinaryOp::And,
                                left: prev,
                                right: eq,
                            })
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
    pub(crate) fn output_name(&self, entry: ResolvedColumn) -> &'d str {
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

    /// The declared collation of a join-tree output column.
    pub(crate) fn output_collation(&self, entry: ResolvedColumn) -> crate::sql::ast::Collation {
        match entry {
            ResolvedColumn::Table(table, column) => {
                self.defs[table].expect("resolved").columns()[column].collation
            }
            ResolvedColumn::Merged(merged) => self.merged[merged].parts
                [..self.merged[merged].n_parts]
                .first()
                .map(|&(table, column)| {
                    self.defs[table].expect("resolved").columns()[column].collation
                })
                .unwrap_or(crate::sql::ast::Collation::None),
        }
    }

    /// Derives a projection's collation while its source scope is still
    /// available, before a derived relation turns it into column metadata.
    pub(crate) fn expression_collation(
        &self,
        expression: &Expr<'d>,
    ) -> Result<crate::sql::ast::Collation, SqlError> {
        crate::sql::eval::resolved_expression_collation(expression, &ScopeTypes(self))
    }

    pub(crate) fn described_expression_collation(
        &self,
        expression: &Expr<'d>,
    ) -> Result<
        (
            crate::sql::ast::Collation,
            crate::sql::types::CollationDerivation,
        ),
        SqlError,
    > {
        crate::sql::eval::described_expression_collation(expression, &ScopeTypes(self))
    }

    /// An expression reading a join-tree output column: a qualified column
    /// reference, or (merged) a COALESCE across the contributors.
    pub(super) fn star_expression(
        &self,
        entry: ResolvedColumn,
        arena: &'d Arena,
    ) -> Result<&'d Expr<'d>, SqlError> {
        self.output_expression(entry, arena)
    }

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
                let args = arena
                    .alloc_slice_copy(&args[..mc.n_parts])
                    .map_err(|_| arena_full())?;
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
    /// Whether entry `t` answers to qualifier `q`. A plain qualifier matches
    /// the exposed name; a composed `schema.table` qualifier (a three-part
    /// reference) matches only an unaliased base table of that schema.
    fn entry_answers_to(&self, t: usize, q: &str) -> bool {
        match q.split_once('.') {
            None => self.names[t] == q,
            Some((schema, table)) => {
                let Some(def) = self.defs[t] else {
                    return false;
                };
                self.names[t] == table
                    && def.name.as_str() == table
                    && def.schema.as_str() == schema
            }
        }
    }

    pub fn table_index(&self, name: &str) -> Result<usize, SqlError> {
        let mut found = None;
        for t in 0..self.n {
            if self.entry_answers_to(t, name) {
                if found.is_some() {
                    // Two same-named tables from different schemas: a bare
                    // reference cannot pick one, exactly as PostgreSQL.
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::AMBIGUOUS_ALIAS,
                        "table reference \"{}\" is ambiguous",
                        name
                    ));
                }
                found = Some(t);
            }
        }
        found.ok_or_else(|| match name.split_once('.') {
            // A composed qualifier whose table name matches some entry is an
            // *invalid* reference (wrong schema or an alias); one matching
            // nothing is *missing* — PostgreSQL's two wordings.
            Some((_, table)) if self.names[..self.n].contains(&table) => sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "invalid reference to FROM-clause entry for table \"{}\"",
                table
            ),
            Some((_, table)) => sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                table
            ),
            None => sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                name
            ),
        })
    }

    /// If `name` refers to a set-returning-function scan, the type of its single
    /// scalar output column — the type a whole-row reference to it carries. A
    /// storage- or subquery-derived table returns None (its whole-row reference
    /// is a record).
    pub fn func_scalar_type(&self, name: &str) -> Option<ColType> {
        let t = self.table_index(name).ok()?;
        if !self.func_scalar[t] {
            return None;
        }
        let def = self.defs[t]?;
        (def.n_columns == 1).then(|| def.columns()[0].ctype)
    }

    pub fn find_column(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ResolvedColumn, SqlError> {
        match qualifier {
            Some(q) => {
                let t = self.table_index(q)?;
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
                                    crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
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
                                crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
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
