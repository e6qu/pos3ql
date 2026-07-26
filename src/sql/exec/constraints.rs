//! Enforcing a table's constraints against the rows a statement writes.
//!
//! Uniqueness (the column flags, the multi-column keys and the secondary
//! indexes), NOT NULL, CHECK, and both sides of a foreign key: the child's
//! reference must exist, and a parent's delete or key change must be answered
//! by the referential action the constraint carries — which re-enters the DML
//! it came from, since CASCADE deletes and SET NULL updates are ordinary
//! writes on another table.

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::eval::{compare_datums, eval, hash_key, sqlstate, ColumnLookup, SqlError};
use crate::sql::txn::TxnState;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{rowenc, RowHome, Storage, TableDef, MAX_COLUMNS};

use super::{check_not_null, RowCtx};

/// A one-column lookup binding PostgreSQL's `VALUE` placeholder to a domain's
/// candidate input, for evaluating that domain's CHECK predicates.
struct ValueLookup<'v> {
    value: Datum<'v>,
}

impl<'v> ColumnLookup<'v> for ValueLookup<'v> {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'v>, SqlError> {
        if name.eq_ignore_ascii_case("value") {
            Ok(self.value)
        } else {
            Err(sql_err!(sqlstate::UNDEFINED_COLUMN, "column \"{}\" does not exist", name))
        }
    }
}

/// Enforces every domain-typed column's NOT NULL and CHECK constraints against a
/// candidate row: base coercion has already happened, so this only runs the
/// domain's own rules, binding `VALUE` to each column's value.
pub(crate) fn check_domain_constraints(
    storage: &Storage,
    def: &TableDef,
    values: &[Datum],
    txid: u32,
    arena: &Arena,
    params: &[Datum],
) -> Result<(), SqlError> {
    for (i, col) in def.columns().iter().enumerate() {
        let Some(dname) = col.domain else { continue };
        let Some(domain) = storage.domain_by_name(dname.as_str(), txid) else { continue };
        let value = values.get(i).copied().unwrap_or(Datum::Null);
        if value.is_null() {
            if domain.not_null {
                return Err(sql_err!(
                    sqlstate::NOT_NULL_VIOLATION,
                    "domain {} does not allow null values",
                    dname.as_str()
                ));
            }
            // NULL bypasses a domain's CHECK constraints (three-valued logic).
            continue;
        }
        let context = ValueLookup { value };
        for check in domain.checks() {
            let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
            if matches!(eval(expression, arena, params, &context)?, Datum::Bool(false)) {
                return Err(sql_err!(
                    sqlstate::CHECK_VIOLATION,
                    "value for domain {} violates check constraint \"{}\"",
                    dname.as_str(),
                    check.name.as_str()
                ));
            }
        }
    }
    Ok(())
}

/// Names a uniqueness constraint for its 23505 message: a single-column flag
/// synthesizes PostgreSQL's `<table>_pkey` / `<table>_<column>_key`, while a
/// table-level key or index carries its stored name.
enum ConstraintName<'a> {
    PrimaryFlag,
    UniqueFlag(&'a str),
    Named(&'a str),
}

fn unique_violation(def: &TableDef, name: &ConstraintName) -> SqlError {
    match name {
        ConstraintName::PrimaryFlag => sql_err!(
            sqlstate::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"{}_pkey\"",
            def.name.as_str()
        ),
        ConstraintName::UniqueFlag(column) => sql_err!(
            sqlstate::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"{}_{}_key\"",
            def.name.as_str(),
            column
        ),
        ConstraintName::Named(constraint) => sql_err!(
            sqlstate::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"{}\"",
            constraint
        ),
    }
}

/// Whether the row `rowid`'s committed image has an all-non-NULL key over
/// `columns` equal to `values` — the verification of an index probe candidate
/// (and of a full-scan candidate) against the authoritative row bytes.
fn committed_key_matches(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    rowid: u64,
) -> Result<bool, SqlError> {
    let Some(state) = storage.row_state(table_index, rowid)? else {
        return Ok(false);
    };
    let Some(home) = state.committed else {
        return Ok(false);
    };
    storage.with_row_bytes(table_index, rowid, home, |bytes| {
        let mut other = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(bytes, schema, &mut other)?;
        Ok(key_equal(columns, values, &other))
    })
}

/// Whether every key column is non-NULL and equal between a candidate `values`
/// tuple and a decoded `other` row (SQL treats a NULL key as distinct).
fn key_equal(columns: &[u16], values: &[Datum], other: &[Datum]) -> bool {
    columns.iter().all(|&c| {
        let i = c as usize;
        !other[i].is_null()
            && compare_datums(&values[i], &other[i]).map(|o| o.is_eq()).unwrap_or(false)
    })
}

/// The shared uniqueness enforcement for one key (a single-column flag, a
/// table-level key, or a unique index). Committed collisions raise 23505; a
/// collision against another transaction's pending image raises 40001. The
/// committed side is served by the value index when the table carries an
/// enforcer for these columns (B-169's O(1) probe), falling back to a full scan
/// otherwise; the pending side is always a bounded scan of the resident overlay
/// (pending rows are never evicted). A NULL in any key column makes the
/// candidate distinct.
#[allow(clippy::too_many_arguments)]
fn enforce_key_uniqueness(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    columns: &[u16],
    name: &ConstraintName,
) -> Result<(), SqlError> {
    if columns.iter().any(|&c| values[c as usize].is_null()) {
        return Ok(());
    }

    // A new key (an insert, not an update of the same row) past the enforcer's
    // committed-row cap is a loud error: an in-RAM value index cannot grow.
    if self_rowid.is_none() && storage.enforcer_at_capacity(table_index, columns) {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "table \"{}\" has reached its value-index row limit ({}); raise value_index_rows",
            def.name.as_str(),
            storage.value_index_cap()
        ));
    }

    let hash = hash_key(values, columns);
    let mut result: Result<(), SqlError> = Ok(());
    let served = storage.probe_unique(table_index, columns, hash, |rowid| {
        if result.is_err() || Some(rowid) == self_rowid {
            return;
        }
        match committed_key_matches(storage, table_index, schema, columns, values, rowid) {
            Ok(true) => result = Err(unique_violation(def, name)),
            Ok(false) => {}
            Err(e) => result = Err(e),
        }
    });
    result?;
    if !served {
        committed_scan_uniqueness(storage, table_index, schema, columns, values, self_rowid, def, name)?;
    }
    pending_scan_uniqueness(storage, table_index, schema, columns, values, self_rowid, txid, def, name)
}

/// The committed-image fallback when a table has no value index for `columns`
/// (an unindexed unique index, or before an enforcer is built): a full scan of
/// committed rows, matching the value index's verdict.
#[allow(clippy::too_many_arguments)]
fn committed_scan_uniqueness(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    self_rowid: Option<u64>,
    def: &TableDef,
    name: &ConstraintName,
) -> Result<(), SqlError> {
    storage.for_each_row_state(table_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        if Some(rowid) == self_rowid {
            return Ok(ControlFlow::Continue(()));
        }
        let Some(home) = state.committed else {
            return Ok(ControlFlow::Continue(()));
        };
        let matched = storage.with_row_bytes(table_index, rowid, home, |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, schema, &mut other)?;
            Ok(key_equal(columns, values, &other))
        })?;
        if matched {
            return Err(unique_violation(def, name));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// Checks a candidate against every resident pending image — the writes not yet
/// committed. All pending rows stay in the overlay map (eviction only drops
/// committed-spilled, pending-free entries), so this scan is bounded by the
/// overlay, never the spilled dataset. A pending collision from another
/// transaction is 40001; from this one, 23505.
#[allow(clippy::too_many_arguments)]
fn pending_scan_uniqueness(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    def: &TableDef,
    name: &ConstraintName,
) -> Result<(), SqlError> {
    for (&rowid, state) in storage.table(table_index).rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        let Some(pending) = state.pending else {
            continue;
        };
        let Some(loc) = pending.loc else {
            continue; // a pending delete has no key
        };
        let matched = storage.with_row_bytes(table_index, rowid, RowHome::Heap(loc), |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, schema, &mut other)?;
            Ok(key_equal(columns, values, &other))
        })?;
        if matched {
            if pending.txid != txid {
                return Err(sql_err!(
                    sqlstate::SERIALIZATION_FAILURE,
                    "could not serialize access due to concurrent update"
                ));
            }
            return Err(unique_violation(def, name));
        }
    }
    Ok(())
}

/// Unique/PK enforcement for the single-column column flags: each `unique`
/// column's value must not equal that column in any other visible row.
pub fn check_unique(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    for (i, column) in def.columns().iter().enumerate() {
        if !column.unique {
            continue;
        }
        let name = if column.primary {
            ConstraintName::PrimaryFlag
        } else {
            ConstraintName::UniqueFlag(column.name.as_str())
        };
        enforce_key_uniqueness(
            storage,
            table_index,
            def,
            schema,
            values,
            self_rowid,
            txid,
            &[i as u16],
            &name,
        )?;
    }
    Ok(())
}

/// Enforces both column-level UNIQUE/PRIMARY KEY and every UNIQUE index.
#[allow(clippy::too_many_arguments)]
pub fn check_all_unique(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    check_unique(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_unique_indexes(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_unique_keys(storage, table_index, def, schema, values, self_rowid, txid)
}

/// Enforces every UNIQUE index on the table: a candidate row conflicts if some
/// other visible row has an equal, all-non-NULL tuple over the index columns
/// (23505; a conflicting uncommitted row from another transaction is 40001).
/// A unique index carries no value index (B-169 scopes those to the PRIMARY KEY
/// / UNIQUE constraints), so this always takes the full-scan fallback inside
/// [`enforce_key_uniqueness`].
#[allow(clippy::too_many_arguments)]
pub fn check_unique_indexes(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        let icols = &index.columns[..index.n_cols];
        enforce_key_uniqueness(
            storage,
            table_index,
            def,
            schema,
            values,
            self_rowid,
            txid,
            icols,
            &ConstraintName::Named(index.name.as_str()),
        )?;
    }
    Ok(())
}

/// Enforces multi-column PRIMARY KEY / UNIQUE table constraints (single-column
/// ones ride the column flags via [`check_unique`]), served by the value index.
#[allow(clippy::too_many_arguments)]
fn check_unique_keys(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    for uk in def.uniques() {
        enforce_key_uniqueness(
            storage,
            table_index,
            def,
            schema,
            values,
            self_rowid,
            txid,
            uk.columns(),
            &ConstraintName::Named(uk.name.as_str()),
        )?;
    }
    Ok(())
}

/// Pre-parsed CHECK predicates for a statement, aligned with `def.checks()`.
pub(crate) type ParsedChecks<'a> = [Option<&'a Expr<'a>>; crate::storage::MAX_CHECKS];

/// Re-parses every stored CHECK predicate once per statement into the arena.
pub(crate) fn parse_checks<'a>(def: &'a TableDef, arena: &'a Arena) -> Result<ParsedChecks<'a>, SqlError> {
    let mut out: ParsedChecks<'a> = [None; crate::storage::MAX_CHECKS];
    for (i, c) in def.checks().iter().enumerate() {
        out[i] = Some(crate::sql::parser::parse_expr(c.expression.as_str(), arena)?);
    }
    Ok(out)
}

/// A column's non-constant DEFAULT, re-parsed once per statement (indexed by
/// column). `None` where the column has no default or a folded constant one.
pub(crate) type ParsedDefaults<'a> = [Option<&'a Expr<'a>>; MAX_COLUMNS];

/// Re-parses every stored non-constant DEFAULT expression once per statement.
/// Generated columns (whose `default_expr` is a generation expression) are
/// excluded — they are computed from the row by [`parse_generated`], not
/// defaulted.
pub(crate) fn parse_defaults<'a>(
    def: &'a TableDef,
    arena: &'a Arena,
) -> Result<ParsedDefaults<'a>, SqlError> {
    let mut out: ParsedDefaults<'a> = [None; MAX_COLUMNS];
    for (i, c) in def.columns().iter().enumerate() {
        if c.is_generated {
            continue;
        }
        if let Some(text) = &c.default_expr {
            out[i] = Some(crate::sql::parser::parse_expr(text.as_str(), arena)?);
        }
    }
    Ok(out)
}

/// Re-parses every `GENERATED ALWAYS AS (expr) STORED` expression once per
/// statement (indexed by column); `None` for non-generated columns. Evaluated
/// against the row's other columns after they are filled.
pub(crate) fn parse_generated<'a>(
    def: &'a TableDef,
    arena: &'a Arena,
) -> Result<ParsedDefaults<'a>, SqlError> {
    let mut out: ParsedDefaults<'a> = [None; MAX_COLUMNS];
    for (i, c) in def.columns().iter().enumerate() {
        if c.is_generated {
            let text = c.default_expr.as_ref().expect("generated column has expr");
            out[i] = Some(crate::sql::parser::parse_expr(text.as_str(), arena)?);
        }
    }
    Ok(out)
}

/// Enforces unique keys, CHECK predicates, and outbound foreign keys for one
/// candidate row about to be stored.
#[allow(clippy::too_many_arguments)]
pub(crate) fn enforce_row_constraints(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
) -> Result<(), SqlError> {
    check_all_unique(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_row_checks(def, checks, values, arena, params)?;
    check_domain_constraints(storage, def, values, txid, arena, params)?;
    check_fk_child(storage, def, values, txid)?;
    Ok(())
}

/// Validates a candidate row's *content* — NOT NULL, CHECK, and outbound
/// foreign keys — without the uniqueness scan. ALTER's row rewrite uses this to
/// validate freshly transformed images before any of them is journaled: the
/// uniqueness of a rewritten column against the other rewritten rows can't be
/// judged against storage (which still holds the old images), so it is checked
/// separately once the new images are in place.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_row_content(
    storage: &Storage,
    def: &TableDef,
    values: &[Datum],
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
    txid: u32,
) -> Result<(), SqlError> {
    check_not_null(def, values)?;
    check_row_checks(def, checks, values, arena, params)?;
    check_domain_constraints(storage, def, values, txid, arena, params)?;
    check_fk_child(storage, def, values, txid)?;
    Ok(())
}

/// Evaluates each CHECK predicate against the candidate row. A predicate that
/// is FALSE raises 23514; NULL and TRUE both pass, per SQL three-valued logic.
fn check_row_checks(
    def: &TableDef,
    checks: &ParsedChecks,
    values: &[Datum],
    arena: &Arena,
    params: &[Datum],
) -> Result<(), SqlError> {
    let context = RowCtx { def, values };
    for (i, c) in def.checks().iter().enumerate() {
        let Some(expression) = checks[i] else { continue };
        if matches!(eval(expression, arena, params, &context)?, Datum::Bool(false)) {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::CHECK_VIOLATION,
                "new row for relation \"{}\" violates check constraint \"{}\"",
                def.name.as_str(),
                c.name.as_str()
            ));
        }
    }
    Ok(())
}

/// Enforces this table's outbound foreign keys: each non-NULL referencing tuple
/// must match a row in the parent (MATCH SIMPLE — a NULL in any referencing
/// column satisfies the constraint). Missing referent raises 23503.
fn check_fk_child(
    storage: &Storage,
    def: &TableDef,
    values: &[Datum],
    txid: u32,
) -> Result<(), SqlError> {
    for fk in def.fkeys() {
        if fk.columns().iter().any(|&c| values[c as usize].is_null()) {
            continue;
        }
        let Some(pi) = storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid) else {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::FOREIGN_KEY_VIOLATION,
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                def.name.as_str(),
                fk.name.as_str()
            ));
        };
        let pdef = storage.table(pi).def;
        let mut pschema = [ColType::Bool; MAX_COLUMNS];
        pdef.schema(&mut pschema);
        if !parent_has_key(
            storage,
            pi,
            &pschema[..pdef.n_columns],
            fk.parent_cols(),
            fk.columns(),
            values,
            txid,
        )? {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::FOREIGN_KEY_VIOLATION,
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                def.name.as_str(),
                fk.name.as_str()
            ));
        }
    }
    Ok(())
}

/// Whether any row of the parent (visible to `txid`) has, in `parent_cols`, the
/// same tuple the child row carries in `child_cols`.
#[allow(clippy::too_many_arguments)]
fn parent_has_key(
    storage: &Storage,
    parent_index: usize,
    parent_schema: &[ColType],
    parent_cols: &[u16],
    child_cols: &[u16],
    child_values: &[Datum],
    txid: u32,
) -> Result<bool, SqlError> {
    let mut found = false;
    storage.for_each_row_state(parent_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = state.visible_to(txid) else {
            return Ok(ControlFlow::Continue(()));
        };
        let all_eq = storage.with_row_bytes(parent_index, rowid, home, |bytes| {
            let mut prow = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, parent_schema, &mut prow)?;
            Ok(parent_cols.iter().zip(child_cols).all(|(&pc, &cc)| {
                let pv = &prow[pc as usize];
                let cv = &child_values[cc as usize];
                !pv.is_null()
                    && compare_datums(cv, pv).map(|o| o.is_eq()).unwrap_or(false)
            }))
        })?;
        if all_eq {
            found = true;
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(found)
}

/// Referential-action cascades can chase foreign keys through many tables
/// (or a cycle); past this depth the statement fails loudly.
pub(crate) const MAX_FK_CASCADE_DEPTH: u32 = 32;

/// After a parent row is deleted (`new_parent` None) or its referenced key
/// updated (Some), applies every referencing foreign key's action:
/// NO ACTION / RESTRICT block (23503); CASCADE deletes or re-keys the
/// referencing rows; SET NULL / SET DEFAULT rewrite the referencing columns
/// (re-checking the child's own constraints). Rewritten or deleted child
/// rows recursively apply their own referential actions. `old_parent` /
/// `new_parent` must not borrow storage (the cascade mutates it) — decode
/// them from arena-copied row bytes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_fk_parent_actions(
    storage: &mut Storage,
    txn: &mut TxnState,
    parent_schema: &str,
    parent_name: &str,
    old_parent: &[Datum],
    new_parent: Option<&[Datum]>,
    arena: &Arena,
    params: &[Datum],
    depth: u32,
) -> Result<(), SqlError> {
    if depth == 0 {
        return Err(sql_err!(
            sqlstate::STATEMENT_TOO_COMPLEX,
            "foreign key cascade nested more than {} levels deep",
            MAX_FK_CASCADE_DEPTH
        ));
    }
    for child_index in 0..storage.table_count() {
        if !storage.table(child_index).visible_to(txn.txid) {
            continue;
        }
        let cdef = storage.table(child_index).def;
        let mut cschema = [ColType::Bool; MAX_COLUMNS];
        cdef.schema(&mut cschema);
        let cschema = &cschema[..cdef.n_columns];
        for fk_index in 0..cdef.n_fkeys {
            let fk = cdef.fkeys[fk_index];
            if fk.parent_schema.as_str() != parent_schema
                || fk.parent.as_str() != parent_name
            {
                continue;
            }
            // An update triggers this key's action only when the key changed.
            if let Some(new_parent) = new_parent {
                let changed = fk.parent_cols().iter().any(|&pc| {
                    let (a, b) = (&old_parent[pc as usize], &new_parent[pc as usize]);
                    match (a.is_null(), b.is_null()) {
                        (true, true) => false,
                        (true, false) | (false, true) => true,
                        (false, false) => {
                            !compare_datums(a, b).map(|o| o.is_eq()).unwrap_or(false)
                        }
                    }
                });
                if !changed {
                    continue;
                }
            }
            let action = if new_parent.is_none() { fk.on_delete } else { fk.on_update };

            // Collect the referencing rows first: the rewrites below mutate
            // the row map, so the scan must complete before them.
            let refers = |crow: &[Datum]| {
                !fk.columns().iter().any(|&c| crow[c as usize].is_null())
                    && fk.columns().iter().zip(fk.parent_cols()).all(|(&cc, &pc)| {
                        let (cv, pv) = (&crow[cc as usize], &old_parent[pc as usize]);
                        !pv.is_null()
                            && compare_datums(cv, pv).map(|o| o.is_eq()).unwrap_or(false)
                    })
            };
            let mut n_match = 0usize;
            storage.for_each_row_state(child_index, &mut |rowid, state| {
                use core::ops::ControlFlow;
                let Some(home) = state.visible_to(txn.txid) else {
                    return Ok(ControlFlow::Continue(()));
                };
                let is_match = storage.with_row_bytes(child_index, rowid, home, |bytes| {
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, cschema, &mut crow)?;
                    Ok(refers(&crow[..cdef.n_columns]))
                })?;
                if is_match {
                    n_match += 1;
                }
                Ok(ControlFlow::Continue(()))
            })?;
            if n_match == 0 {
                continue;
            }
            use crate::storage::FkAction as StorageFkAction;
            if matches!(action, StorageFkAction::NoAction | StorageFkAction::Restrict) {
                // NO ACTION raises 23503; RESTRICT the distinct 23001, as
                // PostgreSQL (same message, different SQLSTATE).
                let code =
                    if action == StorageFkAction::Restrict { "23001" } else { "23503" };
                return Err(sql_err!(
                    code,
                    "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                    parent_name,
                    fk.name.as_str(),
                    cdef.name.as_str()
                ));
            }
            let matches: &mut [(u64, &[u8])] = arena
                .alloc_slice_with(n_match, |_| (0u64, &[] as &[u8]))
                .map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "foreign key cascade exceeds the statement arena"
                    )
                })?;
            {
                let mut at = 0usize;
                storage.for_each_row_state(child_index, &mut |rowid, state| {
                    use core::ops::ControlFlow;
                    let Some(home) = state.visible_to(txn.txid) else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    // The cascade mutates storage below, so a matching row is
                    // copied into the arena wherever its bytes live.
                    let bytes = storage.row_bytes(child_index, rowid, home, arena)?;
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, cschema, &mut crow)?;
                    if refers(&crow[..cdef.n_columns]) {
                        let copy = arena.alloc_slice_copy(bytes).map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "foreign key cascade exceeds the statement arena"
                            )
                        })?;
                        matches[at] = (rowid, &*copy);
                        at += 1;
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
            }

            let child_schema = cdef.schema.as_str();
            let child_name = cdef.name.as_str();
            for &(rowid, old_bytes) in matches.iter() {
                let mut crow = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(old_bytes, cschema, &mut crow)?;
                let crow = &crow[..cdef.n_columns];
                if new_parent.is_none() && action == StorageFkAction::Cascade {
                    // Cascade the delete: grandchildren first, then this row.
                    apply_fk_parent_actions(
                        storage, txn, child_schema, child_name, crow, None, arena, params,
                        depth - 1,
                    )?;
                    let prior = storage.write_pending(child_index, rowid, txn.txid, None)?;
                    if let Err(e) = txn.touch(child_index as u32, rowid, prior) {
                        storage.restore_pending(child_index, rowid, txn.txid, prior);
                        return Err(e);
                    }
                    continue;
                }
                // The rewriting actions produce a new child row image.
                let mut new_child = [Datum::Null; MAX_COLUMNS];
                new_child[..cdef.n_columns].copy_from_slice(crow);
                for (&cc, &pc) in fk.columns().iter().zip(fk.parent_cols()) {
                    new_child[cc as usize] = match action {
                        StorageFkAction::Cascade => {
                            new_parent.expect("delete-cascade handled above")[pc as usize]
                        }
                        StorageFkAction::SetNull => Datum::Null,
                        StorageFkAction::SetDefault => cdef.columns()[cc as usize]
                            .default_value
                            .as_ref()
                            .map(|d| d.as_datum())
                            .unwrap_or(Datum::Null),
                        StorageFkAction::NoAction | StorageFkAction::Restrict => {
                            unreachable!("blocking actions handled above")
                        }
                    };
                }
                let new_child = &new_child[..cdef.n_columns];
                check_not_null(&cdef, new_child)?;
                let checks = parse_checks(&cdef, arena)?;
                enforce_row_constraints(
                    storage,
                    child_index,
                    &cdef,
                    cschema,
                    new_child,
                    Some(rowid),
                    txn.txid,
                    &checks,
                    arena,
                    params,
                )?;
                // The child's own referenced keys may have changed — recurse.
                apply_fk_parent_actions(
                    storage,
                    txn,
                    child_schema,
                    child_name,
                    crow,
                    Some(new_child),
                    arena,
                    params,
                    depth - 1,
                )?;
                let len = rowenc::encoded_len(new_child);
                let out = arena.alloc_slice_with(len, |_| 0u8).map_err(|_| {
                    sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "foreign key cascade exceeds the statement arena"
                    )
                })?;
                rowenc::encode(new_child, out);
                let (new_loc, slice) = storage.heap.append(out.len())?;
                slice.copy_from_slice(out);
                let prior = storage.write_pending(child_index, rowid, txn.txid, Some(new_loc))?;
                if let Err(e) = txn.touch(child_index as u32, rowid, prior) {
                    storage.restore_pending(child_index, rowid, txn.txid, prior);
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Whether any visible table has a foreign key referencing `name`.
pub(crate) fn table_is_referenced(
    storage: &Storage,
    schema: &str,
    name: &str,
    txid: u32,
) -> bool {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        if storage
            .table(column_index)
            .def
            .fkeys()
            .iter()
            .any(|fk| fk.parent_schema.as_str() == schema && fk.parent.as_str() == name)
        {
            return true;
        }
    }
    false
}

/// Whether an update to `parent_name` changed any column referenced by some
/// child foreign key (so referential integrity must be re-checked).
pub(crate) fn referenced_key_changed(
    storage: &Storage,
    parent_schema: &str,
    parent_name: &str,
    old: &[Datum],
    new: &[Datum],
    txid: u32,
) -> bool {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        let cdef = storage.table(column_index).def;
        for fk in cdef.fkeys() {
            if fk.parent_schema.as_str() != parent_schema
                || fk.parent.as_str() != parent_name
            {
                continue;
            }
            for &pc in fk.parent_cols() {
                let i = pc as usize;
                let (a, b) = (&old[i], &new[i]);
                let changed = match (a.is_null(), b.is_null()) {
                    (true, true) => false,
                    (true, false) | (false, true) => true,
                    (false, false) => !compare_datums(a, b).map(|o| o.is_eq()).unwrap_or(false),
                };
                if changed {
                    return true;
                }
            }
        }
    }
    false
}
