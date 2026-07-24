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
use crate::sql::ast::{
    BinaryOp, Expr, FromClause, MaterializedCte, TableRef, MAX_USING_COLUMNS,
};
use crate::sql::eval::{sqlstate, SqlError};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{ColumnMeta, SqlName, Storage, TableDef, MAX_COLUMNS};

use super::{
    arena_full, common_using_type, select_into_rows, synth_derived_def, table_func_def,
    table_func_rows, MAX_JOIN_TABLES,
};

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
    /// Marks a set-returning-function scan (`FROM func(args)`), whose output row
    /// type is its single scalar column — so a whole-row reference to the table
    /// alias yields that scalar, not a one-field record (which is how a
    /// subquery- or storage-derived table's whole-row reference behaves).
    pub func_scalar: [bool; MAX_JOIN_TABLES],
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
            func_scalar: [false; MAX_JOIN_TABLES],
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
            let ctype =
                crate::sql::exec::coltype_of_oid(m.column_types[i].0).unwrap_or(ColType::Text);
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
            if matches!(
                storage.resolve_relation(tref.schema, tref.table, txid),
                Some(crate::storage::ResolvedRelation::Catalog)
            ) {
                return self.add_catalog(storage, tref, arena, true);
            }
            return self.add(storage, tref.schema, tref.table, tref.alias, txid);
        };
        let exposed = tref.alias.expect("parser requires a derived-table alias");
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
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
            if matches!(
                storage.resolve_relation(tref.schema, tref.table, txid),
                Some(crate::storage::ResolvedRelation::Catalog)
            ) {
                return self.add_catalog(storage, tref, arena, false);
            }
            return self.add(storage, tref.schema, tref.table, tref.alias, txid);
        };
        let exposed = tref.alias.expect("parser requires a derived-table alias");
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
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
        let synth = crate::sql::catalog::synthesize(storage, tref.schema, tref.table, arena)?;
        let exposed = tref.alias.unwrap_or(tref.table);
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
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
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        let mut rows: &'a [&'a [u8]] =
            if materialize { table_func_rows(tref, arena, params)? } else { &[] };
        // `WITH ORDINALITY` appends a 1-based bigint to each materialized row.
        if tref.with_ordinality && materialize {
            let base_cols = def_reference.n_columns - 1;
            const EMPTY: &[u8] = &[];
            let wrapped = arena.alloc_slice_with(rows.len(), |_| EMPTY).map_err(|_| arena_full())?;
            for (i, row) in rows.iter().enumerate() {
                let mut vals = [Datum::Null; MAX_COLUMNS];
                for (c, slot) in vals[..base_cols].iter_mut().enumerate() {
                    *slot = crate::sql::exec::decode_projected_col_record(row, c, arena)?;
                }
                vals[base_cols] = Datum::Int8((i + 1) as i64);
                wrapped[i] = crate::sql::exec::encode_projected_pub(&vals[..base_cols + 1], arena)?;
            }
            rows = &*wrapped;
        }
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
        schema: Option<&str>,
        table: &str,
        alias: Option<&'d str>,
        txid: u32,
    ) -> Result<(), SqlError> {
        // `txid == 0` (schema-only / Describe) resolves against the committed
        // catalog; a real transaction sees its own uncommitted CREATE/DROP.
        let Some(crate::storage::ResolvedRelation::Table(slot)) =
            storage.resolve_relation(schema, table, txid)
        else {
            return Err(match schema {
                Some(s) => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}.{}\" does not exist",
                    s,
                    table
                ),
                None => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    table
                ),
            });
        };
        let def = &storage.table(slot).def;
        let exposed = alias.unwrap_or(def.name.as_str());
        if self.names[..self.n].contains(&exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
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
            sql_err!(sqlstate::UNDEFINED_TABLE, "missing FROM-clause entry for table \"{}\"", name)
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
                let Some(t) = self.names[..self.n].iter().position(|n| *n == q) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_TABLE,
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
