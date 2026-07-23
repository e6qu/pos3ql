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
use crate::sql::eval::{compare_datums, eval, sqlstate, SqlError};
use crate::sql::txn::TxnState;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{rowenc, Storage, TableDef, MAX_COLUMNS};

use super::{check_not_null, RowCtx};

/// Unique/PK enforcement: `values[col]` must not equal that column in any
/// other visible row. Committed conflicts are 23505; a conflicting
/// uncommitted row from another transaction fails fast with 40001.
pub fn check_unique(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
) -> Result<(), SqlError> {
    let any_unique = def.columns().iter().any(|c| c.unique);
    if !any_unique {
        return Ok(());
    }
    let table = storage.table(table_index);
    for (&rowid, state) in table.rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        // Check both the committed image and any pending image: a commit
        // of either would collide.
        for (home, pending_of) in [
            (state.committed, None),
            (
                state.pending.and_then(|p| p.loc).map(crate::storage::RowHome::Heap),
                state.pending.map(|p| p.txid),
            ),
        ] {
            let Some(home) = home else { continue };
            // The decoded datums borrow the fetched bytes, so the comparison
            // runs inside the fetch; only the colliding column index escapes.
            let collision = storage.with_row_bytes(table_index, rowid, home, |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, schema, &mut other)?;
                for (i, c) in def.columns().iter().enumerate() {
                    if !c.unique || values[i].is_null() || other[i].is_null() {
                        continue;
                    }
                    if compare_datums(&values[i], &other[i])
                        .map(|o| o.is_eq())
                        .unwrap_or(false)
                    {
                        return Ok(Some(i));
                    }
                }
                Ok(None)
            })?;
            if let Some(i) = collision {
                if let Some(owner) = pending_of
                    && owner != txid
                {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update"
                    ));
                }
                let c = &def.columns()[i];
                let kind = if c.primary { "pkey" } else { "key" };
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::UNIQUE_VIOLATION,
                    "duplicate key value violates unique constraint \"{}_{}_{}\"",
                    def.name.as_str(),
                    c.name.as_str(),
                    kind
                ));
            }
        }
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
/// SQL treats NULLs as distinct, so a candidate with any NULL index column is
/// never a conflict.
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
    let table_name = def.name.as_str();
    for index in storage.unique_indexes_for(table_name, txid) {
        let icols = &index.columns[..index.n_cols];
        tuple_uniqueness(
            storage, table_index, schema, icols, values, self_rowid, txid, index.name.as_str(),
        )?;
    }
    Ok(())
}

/// Enforces multi-column PRIMARY KEY / UNIQUE table constraints (single-column
/// ones ride the column flags via [`check_unique`]).
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
        tuple_uniqueness(
            storage, table_index, schema, uk.columns(), values, self_rowid, txid, uk.name.as_str(),
        )?;
    }
    Ok(())
}

/// A candidate row conflicts if some other visible row has an equal,
/// all-non-NULL tuple over `columns` (23505; 40001 if the conflicting row is
/// another transaction's uncommitted write). A NULL in any key column of the
/// candidate makes it distinct, never a conflict.
#[allow(clippy::too_many_arguments)]
fn tuple_uniqueness(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    constraint_name: &str,
) -> Result<(), SqlError> {
    if columns.iter().any(|&c| values[c as usize].is_null()) {
        return Ok(());
    }
    let table = storage.table(table_index);
    for (&rowid, state) in table.rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        for (home, pending_of) in [
            (state.committed, None),
            (
                state.pending.and_then(|p| p.loc).map(crate::storage::RowHome::Heap),
                state.pending.map(|p| p.txid),
            ),
        ] {
            let Some(home) = home else { continue };
            let all_eq = storage.with_row_bytes(table_index, rowid, home, |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, schema, &mut other)?;
                Ok(columns.iter().all(|&c| {
                    let column_index = c as usize;
                    !other[column_index].is_null()
                        && compare_datums(&values[column_index], &other[column_index])
                            .map(|o| o.is_eq())
                            .unwrap_or(false)
                }))
            })?;
            if all_eq {
                if let Some(owner) = pending_of
                    && owner != txid
                {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::SERIALIZATION_FAILURE,
                        "could not serialize access due to concurrent update"
                    ));
                }
                return Err(sql_err!(
                    crate::sql::eval::sqlstate::UNIQUE_VIOLATION,
                    "duplicate key value violates unique constraint \"{}\"",
                    constraint_name
                ));
            }
        }
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
        let Some(pi) = storage.find_visible(fk.parent.as_str(), txid) else {
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
    let table = storage.table(parent_index);
    for (&rowid, state) in table.rows.iter() {
        let Some(home) = state.visible_to(txid) else { continue };
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
            return Ok(true);
        }
    }
    Ok(false)
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
            if fk.parent.as_str() != parent_name {
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
            {
                let table = storage.table(child_index);
                for (&rowid, state) in table.rows.iter() {
                    let Some(home) = state.visible_to(txn.txid) else { continue };
                    let is_match =
                        storage.with_row_bytes(child_index, rowid, home, |bytes| {
                            let mut crow = [Datum::Null; MAX_COLUMNS];
                            rowenc::decode(bytes, cschema, &mut crow)?;
                            Ok(refers(&crow[..cdef.n_columns]))
                        })?;
                    if is_match {
                        n_match += 1;
                    }
                }
            }
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
                let table = storage.table(child_index);
                for (rowid, state) in table.rows.iter() {
                    let Some(home) = state.visible_to(txn.txid) else { continue };
                    // The cascade mutates storage below, so a matching row is
                    // copied into the arena wherever its bytes live.
                    let bytes = storage.row_bytes(child_index, *rowid, home, arena)?;
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, cschema, &mut crow)?;
                    if refers(&crow[..cdef.n_columns]) {
                        let copy = arena.alloc_slice_copy(bytes).map_err(|_| {
                            sql_err!(
                                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                                "foreign key cascade exceeds the statement arena"
                            )
                        })?;
                        matches[at] = (*rowid, &*copy);
                        at += 1;
                    }
                }
            }

            let child_name = cdef.name.as_str();
            for &(rowid, old_bytes) in matches.iter() {
                let mut crow = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(old_bytes, cschema, &mut crow)?;
                let crow = &crow[..cdef.n_columns];
                if new_parent.is_none() && action == StorageFkAction::Cascade {
                    // Cascade the delete: grandchildren first, then this row.
                    apply_fk_parent_actions(
                        storage, txn, child_name, crow, None, arena, params, depth - 1,
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
pub(crate) fn table_is_referenced(storage: &Storage, name: &str, txid: u32) -> bool {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        if storage
            .table(column_index)
            .def
            .fkeys()
            .iter()
            .any(|fk| fk.parent.as_str() == name)
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
            if fk.parent.as_str() != parent_name {
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
