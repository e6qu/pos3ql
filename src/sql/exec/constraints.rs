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
use crate::sql::eval::{
    ColumnLookup, EvalHooks, SqlError, compare_datums_collated, eval, eval_full, hash_key_collated,
    resolved_expression_collation, sqlstate,
};
use crate::sql::txn::TxnState;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{MAX_COLUMNS, RowHome, Storage, TableDef, rowenc};

use super::{RowCtx, check_not_null};

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
            Err(sql_err!(
                sqlstate::UNDEFINED_COLUMN,
                "column \"{}\" does not exist",
                name
            ))
        }
    }
}

/// Coerces a value to a domain's base representation, applies the base typmod,
/// then enforces the domain's own constraints. This is the one catalog-aware
/// path shared by explicit casts, domain-array elements, and table writes.
pub(crate) fn coerce_domain_value<'a>(
    storage: &Storage,
    slot: usize,
    value: Datum<'a>,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<Datum<'a>, SqlError> {
    let domain = storage.domain_for(slot, txid);
    let value = if let Some(parent_name) = domain.base_domain {
        let parent = storage
            .domain_identity_slot(parent_name.schema.as_str(), parent_name.name.as_str(), txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "base domain \"{}.{}\" does not exist",
                    parent_name.schema.as_str(),
                    parent_name.name.as_str()
                )
            })?;
        coerce_domain_value(storage, parent, value, txid, arena, params)?
    } else {
        match domain.base {
            crate::sql::types::ColType::Enum(slot) => {
                super::coerce_enum_value(value, slot, storage, txid, arena)?
            }
            crate::sql::types::ColType::Composite(slot) => match value {
                value @ Datum::CompositeText { slot: actual, .. } if actual == slot => value,
                Datum::Text(text) | Datum::Bpchar(text) => {
                    super::decode_composite_text(text, slot, storage, txid, arena)?
                }
                value => super::coerce_composite_value(value, slot, storage, txid, arena)?,
            },
            base => crate::sql::eval::cast_to(value, base, arena)?,
        }
    };
    let value = super::apply_typmod(value, domain.base, domain.base_type_mod, arena)?;
    validate_domain_value(storage, txid, &domain, value, arena, params)?;
    Ok(value)
}

fn validate_domain_value(
    storage: &Storage,
    txid: u32,
    domain: &crate::storage::DomainDef,
    value: Datum,
    arena: &Arena,
    params: &[Datum],
) -> Result<(), SqlError> {
    if value.is_null() {
        if domain.not_null {
            return Err(sql_err!(
                sqlstate::NOT_NULL_VIOLATION,
                "domain {} does not allow null values",
                domain.name.as_str()
            ));
        }
        return Ok(());
    }
    let context = ValueLookup { value };
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    for check in domain.checks() {
        let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
        if matches!(
            eval_full(expression, arena, params, &context, &hooks)?,
            Datum::Bool(false)
        ) {
            return Err(sql_err!(
                sqlstate::CHECK_VIOLATION,
                "value for domain {} violates check constraint \"{}\"",
                domain.name.as_str(),
                check.name.as_str()
            ));
        }
    }
    Ok(())
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
        let Some(user_type) = col.user_type else {
            continue;
        };
        // Enum identity also lives in `user_type`, as do enum/domain array element
        // identities. Only a scalar domain applies its constraint to the whole
        // column value; a domain array validates each element while coercing.
        if matches!(
            col.ctype,
            ColType::Enum(_)
                | ColType::Array(crate::sql::types::ArrElem::Enum(_))
                | ColType::Array(crate::sql::types::ArrElem::Domain { .. })
        ) {
            continue;
        }
        let Some(slot) =
            storage.domain_identity_slot(user_type.schema.as_str(), user_type.name.as_str(), txid)
        else {
            continue;
        };
        let value = values.get(i).copied().unwrap_or(Datum::Null);
        // The column was already coerced to the base representation. Reusing
        // the catalog-aware path applies every inherited constraint as well as
        // the leaf domain's rules.
        let _ = coerce_domain_value(storage, slot, value, txid, arena, params)?;
    }
    Ok(())
}

/// Names a uniqueness constraint for its 23505 message: a single-column flag
/// synthesizes PostgreSQL's `<table>_pkey` / `<table>_<column>_key`, while a
/// table-level key or index carries its stored name.
pub(crate) enum ConstraintName<'a> {
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

/// Whether the row `rowid`'s committed key equals `values` under this index's
/// NULL-equality rule.
#[expect(
    clippy::too_many_arguments,
    reason = "value-index probe context is explicit"
)]
fn committed_key_matches(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    nulls_not_distinct: bool,
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
        key_equal(storage, def, columns, values, &other, nulls_not_distinct)
    })
}

/// Whether both keys are equal under the index's NULL-equality rule.
fn key_equal(
    storage: &Storage,
    def: &TableDef,
    columns: &[u16],
    values: &[Datum],
    other: &[Datum],
    nulls_not_distinct: bool,
) -> Result<bool, SqlError> {
    for &column in columns {
        let index = column as usize;
        if values[index].is_null() || other[index].is_null() {
            if nulls_not_distinct && values[index].is_null() && other[index].is_null() {
                continue;
            }
            return Ok(false);
        }
        if !compare_datums_collated(
            storage,
            def.columns()[index].collation,
            &values[index],
            &other[index],
        )?
        .is_eq()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn key_values_equal(
    storage: &Storage,
    collations: &[crate::sql::ast::Collation],
    values: &[Datum],
    other: &[Datum],
    nulls_not_distinct: bool,
) -> Result<bool, SqlError> {
    for (index, (value, other_value)) in values.iter().zip(other).enumerate() {
        if value.is_null() || other_value.is_null() {
            if nulls_not_distinct && value.is_null() && other_value.is_null() {
                continue;
            }
            return Ok(false);
        }
        if !compare_datums_collated(storage, collations[index], value, other_value)?.is_eq() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn index_key_collations(
    def: &TableDef,
    columns: &[u16],
    expressions: &[Option<&Expr<'_>>],
) -> Result<[crate::sql::ast::Collation; crate::storage::MAX_INDEX_COLS], SqlError> {
    let mut collations = [crate::sql::ast::Collation::None; crate::storage::MAX_INDEX_COLS];
    let row = RowCtx {
        def,
        values: &[],
        alias: None,
    };
    for (index, expression) in expressions.iter().enumerate() {
        collations[index] = match expression {
            Some(expression) => resolved_expression_collation(expression, &row)?,
            None => def.columns()[columns[index] as usize].collation,
        };
    }
    Ok(collations)
}

pub(crate) fn index_key_values<'a>(
    def: &TableDef,
    row: &[Datum<'a>],
    columns: &[u16],
    expressions: &[Option<&'a Expr<'a>>],
    arena: &'a Arena,
) -> Result<[Datum<'a>; crate::storage::MAX_INDEX_COLS], SqlError> {
    let mut keys = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (position, expression) in expressions.iter().enumerate() {
        keys[position] = match expression {
            Some(expression) => eval(
                expression,
                arena,
                crate::sql::eval::NO_PARAMS,
                &RowCtx {
                    def,
                    values: row,
                    alias: None,
                },
            )?,
            None => row[columns[position] as usize],
        };
    }
    Ok(keys)
}

/// Enforces a unique expression index without manufacturing a column-only
/// cache key. Expressions and an optional predicate are evaluated against the
/// authoritative committed and pending images with one parser representation.
#[allow(clippy::too_many_arguments)]
fn enforce_expression_index_uniqueness<'a>(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum<'a>],
    self_rowid: Option<u64>,
    txid: u32,
    columns: &[u16],
    expressions: &[Option<&'a Expr<'a>>],
    nulls_not_distinct: bool,
    name: &ConstraintName,
    predicate: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    if let Some(predicate) = predicate
        && !index_predicate_matches(def, values, predicate, arena)?
    {
        return Ok(());
    }
    let keys = index_key_values(def, values, columns, expressions, arena)?;
    let collations = index_key_collations(def, columns, expressions)?;
    if !nulls_not_distinct && keys[..columns.len()].iter().any(Datum::is_null) {
        return Ok(());
    }
    let matches = |other: &[Datum]| -> Result<bool, SqlError> {
        if let Some(predicate) = predicate
            && !index_predicate_matches(def, other, predicate, arena)?
        {
            return Ok(false);
        }
        let other_keys = index_key_values(def, other, columns, expressions, arena)?;
        key_values_equal(
            storage,
            &collations[..columns.len()],
            &keys[..columns.len()],
            &other_keys[..columns.len()],
            nulls_not_distinct,
        )
    };
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
            matches(&other)
        })?;
        if matched {
            return Err(unique_violation(def, name));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    for (&rowid, state) in storage.table(table_index).rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        let Some(pending) = state.pending.last() else {
            continue;
        };
        let Some(location) = pending.loc else {
            continue;
        };
        let matched =
            storage.with_row_bytes(table_index, rowid, RowHome::Heap(location), |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, schema, &mut other)?;
                matches(&other)
            })?;
        if !matched {
            continue;
        }
        if pending.txid != txid {
            storage.wait_for_transaction(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a concurrent unique-key writer"
            ));
        }
        return Err(unique_violation(def, name));
    }
    Ok(())
}

/// The shared uniqueness enforcement for one key (a single-column flag, a
/// table-level key, or a unique index). Committed collisions raise 23505; a
/// collision against another transaction's pending image waits for it. The
/// committed side is served by the value index when the table carries an
/// enforcer for these columns (an O(1) probe), falling back to a full scan
/// otherwise; the pending side is always a bounded scan of the resident overlay
/// (pending rows are never evicted). The index definition controls whether a
/// NULL key is distinct.
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
    nulls_not_distinct: bool,
    name: &ConstraintName,
) -> Result<(), SqlError> {
    if !nulls_not_distinct && columns.iter().any(|&c| values[c as usize].is_null()) {
        return Ok(());
    }
    if storage.has_pending_table_def(table_index, txid) {
        return pending_scan_uniqueness(
            storage,
            table_index,
            schema,
            columns,
            nulls_not_distinct,
            values,
            self_rowid,
            txid,
            def,
            name,
        );
    }

    let mut result: Result<(), SqlError> = Ok(());
    // Value caches omit NULL keys because normal UNIQUE indexes never need to
    // compare them. NULLS NOT DISTINCT makes those keys authoritative, so scan
    // their bounded durable row set instead of treating a cache miss as proof.
    let served = if nulls_not_distinct && columns.iter().any(|&c| values[c as usize].is_null()) {
        false
    } else {
        let mut collations = [crate::sql::ast::Collation::None; crate::storage::MAX_INDEX_COLS];
        for (index, column) in columns.iter().enumerate() {
            collations[index] = def.columns()[*column as usize].collation;
        }
        let hash = hash_key_collated(values, columns, &collations[..columns.len()]);
        storage.probe_value(table_index, columns, hash, |rowid| {
            if result.is_err() || Some(rowid) == self_rowid {
                return;
            }
            match committed_key_matches(
                storage,
                table_index,
                def,
                schema,
                columns,
                values,
                nulls_not_distinct,
                rowid,
            ) {
                Ok(true) => result = Err(unique_violation(def, name)),
                Ok(false) => {}
                Err(e) => result = Err(e),
            }
        })?
    };
    result?;
    if !served {
        committed_scan_uniqueness(
            storage,
            table_index,
            schema,
            columns,
            nulls_not_distinct,
            values,
            self_rowid,
            def,
            name,
        )?;
    }
    pending_scan_uniqueness(
        storage,
        table_index,
        schema,
        columns,
        nulls_not_distinct,
        values,
        self_rowid,
        txid,
        def,
        name,
    )
}

/// The committed-image check when a key has no value-cache binding: a full
/// scan of committed rows preserves the authoritative uniqueness verdict.
#[allow(clippy::too_many_arguments)]
fn committed_scan_uniqueness(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    nulls_not_distinct: bool,
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
            key_equal(storage, def, columns, values, &other, nulls_not_distinct)
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
/// transaction parks behind its owner; from this one, it is 23505.
#[allow(clippy::too_many_arguments)]
fn pending_scan_uniqueness(
    storage: &Storage,
    table_index: usize,
    schema: &[ColType],
    columns: &[u16],
    nulls_not_distinct: bool,
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
        let Some(pending) = state.pending.last() else {
            continue;
        };
        let Some(loc) = pending.loc else {
            continue; // a pending delete has no key
        };
        let matched = storage.with_row_bytes(table_index, rowid, RowHome::Heap(loc), |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, schema, &mut other)?;
            key_equal(storage, def, columns, values, &other, nulls_not_distinct)
        })?;
        if matched {
            if pending.txid != txid {
                storage.wait_for_transaction(txid, pending.txid)?;
                return Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "statement is waiting for a concurrent unique-key writer"
                ));
            }
            return Err(unique_violation(def, name));
        }
    }
    Ok(())
}

/// Enforces a partial UNIQUE index against its predicate-defined member set.
/// A partial index deliberately has no column-only value-cache binding: a
/// cache key cannot encode arbitrary SQL membership without duplicating the
/// expression executor. Both the candidate and every authoritative competing
/// image are therefore evaluated with the same parsed predicate.
#[allow(clippy::too_many_arguments)]
pub(crate) fn enforce_partial_index_uniqueness(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    columns: &[u16],
    nulls_not_distinct: bool,
    name: &ConstraintName,
    predicate: &Expr,
    arena: &Arena,
) -> Result<(), SqlError> {
    if !index_predicate_matches(def, values, predicate, arena)?
        || (!nulls_not_distinct
            && columns
                .iter()
                .any(|&column| values[column as usize].is_null()))
    {
        return Ok(());
    }
    let matches = |other: &[Datum]| -> Result<bool, SqlError> {
        Ok(index_predicate_matches(def, other, predicate, arena)?
            && key_equal(storage, def, columns, values, other, nulls_not_distinct)?)
    };
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
            matches(&other)
        })?;
        if matched {
            return Err(unique_violation(def, name));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    for (&rowid, state) in storage.table(table_index).rows.iter() {
        if Some(rowid) == self_rowid {
            continue;
        }
        let Some(pending) = state.pending.last() else {
            continue;
        };
        let Some(location) = pending.loc else {
            continue;
        };
        let matched =
            storage.with_row_bytes(table_index, rowid, RowHome::Heap(location), |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, schema, &mut other)?;
                matches(&other)
            })?;
        if !matched {
            continue;
        }
        if pending.txid != txid {
            storage.wait_for_transaction(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a concurrent unique-key writer"
            ));
        }
        return Err(unique_violation(def, name));
    }
    Ok(())
}

pub(crate) fn index_predicate_matches(
    def: &TableDef,
    values: &[Datum],
    predicate: &Expr,
    arena: &Arena,
) -> Result<bool, SqlError> {
    match eval(
        predicate,
        arena,
        crate::sql::eval::NO_PARAMS,
        &RowCtx {
            def,
            values,
            alias: None,
        },
    )? {
        Datum::Bool(value) => Ok(value),
        Datum::Null => Ok(false),
        _ => Err(sql_err!(
            sqlstate::DATATYPE_MISMATCH,
            "argument of WHERE must be type boolean, not type {}",
            "unknown"
        )),
    }
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
            false,
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
    arena: &Arena,
) -> Result<(), SqlError> {
    check_unique(storage, table_index, def, schema, values, self_rowid, txid)?;
    check_unique_indexes(
        storage,
        table_index,
        def,
        schema,
        values,
        self_rowid,
        txid,
        arena,
    )?;
    check_unique_keys(storage, table_index, def, schema, values, self_rowid, txid)
}

/// Rejects an indexed tuple before its row or CREATE INDEX WAL can commit if
/// the immutable object generation cannot represent the key. Checkpoint is
/// never the first observer of this physical limit.
pub(crate) fn check_index_tuple_size(columns: &[u16], values: &[Datum]) -> Result<(), SqlError> {
    let mut key = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (at, column) in columns.iter().enumerate() {
        key[at] = values[*column as usize];
    }
    let encoded = rowenc::encoded_len(&key[..columns.len()]);
    if encoded > crate::store::VALUE_INDEX_KEY_MAX {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "index row size {} exceeds maximum {}",
            encoded,
            crate::store::VALUE_INDEX_KEY_MAX
        ));
    }
    Ok(())
}

/// Validates every persistent tuple shape visible on a table, including
/// non-unique named indexes (which uniqueness-only enforcement does not walk).
fn check_index_tuple_sizes(
    storage: &Storage,
    def: &TableDef,
    values: &[Datum],
    txid: u32,
) -> Result<(), SqlError> {
    for (column, metadata) in def.columns().iter().enumerate() {
        if metadata.unique {
            check_index_tuple_size(&[column as u16], values)?;
        }
    }
    for unique in def.uniques() {
        check_index_tuple_size(unique.columns(), values)?;
    }
    for index in storage.indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        if index.predicate.is_some()
            || index.expressions[..index.n_cols]
                .iter()
                .any(Option::is_some)
        {
            continue;
        }
        check_index_tuple_size(&index.columns[..index.n_cols], values)?;
    }
    Ok(())
}

/// Enforces every UNIQUE index on the table: a candidate row conflicts if some
/// other visible row has an equal key under that index's NULL-equality rule
/// (23505; a conflicting uncommitted row from another transaction waits).
/// Named indexes share the same bounded acceleration cache as table
/// constraints. CREATE INDEX validates against authoritative rows before its
/// pending cache binding is prepared, and subsequent writes may use that
/// binding while the creating transaction remains open.
#[allow(clippy::too_many_arguments)]
pub fn check_unique_indexes(
    storage: &Storage,
    table_index: usize,
    def: &TableDef,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        let icols = &index.columns[..index.n_cols];
        let index_name = index.name_for(txid);
        let name = ConstraintName::Named(index_name.as_str());
        if index.expressions[..index.n_cols]
            .iter()
            .any(Option::is_some)
        {
            let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
            for (position, source) in index.expressions.iter().enumerate().take(index.n_cols) {
                if let Some(source) = source {
                    let source = arena
                        .alloc_str(source.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?;
                    expressions[position] = Some(crate::sql::parser::parse_expr(source, arena)?);
                }
            }
            let predicate = match index.predicate {
                Some(source) => {
                    let source = arena
                        .alloc_str(source.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?;
                    Some(crate::sql::parser::parse_expr(source, arena)?)
                }
                None => None,
            };
            enforce_expression_index_uniqueness(
                storage,
                table_index,
                def,
                schema,
                values,
                self_rowid,
                txid,
                icols,
                &expressions[..index.n_cols],
                index.nulls_not_distinct,
                &name,
                predicate,
                arena,
            )?;
            continue;
        }
        if let Some(predicate) = index.predicate {
            let expression = crate::sql::parser::parse_expr(predicate.as_str(), arena)?;
            enforce_partial_index_uniqueness(
                storage,
                table_index,
                def,
                schema,
                values,
                self_rowid,
                txid,
                icols,
                index.nulls_not_distinct,
                &name,
                expression,
                arena,
            )?;
        } else {
            enforce_key_uniqueness(
                storage,
                table_index,
                def,
                schema,
                values,
                self_rowid,
                txid,
                icols,
                index.nulls_not_distinct,
                &name,
            )?;
        }
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
            false,
            &ConstraintName::Named(uk.name.as_str()),
        )?;
    }
    Ok(())
}

/// Pre-parsed CHECK predicates for a statement, aligned with `def.checks()`.
pub(crate) type ParsedChecks<'a> = [Option<&'a Expr<'a>>; crate::storage::MAX_CHECKS];

/// Re-parses every stored CHECK predicate once per statement into the arena.
pub(crate) fn parse_checks<'a>(
    def: &'a TableDef,
    arena: &'a Arena,
) -> Result<ParsedChecks<'a>, SqlError> {
    let mut out: ParsedChecks<'a> = [None; crate::storage::MAX_CHECKS];
    for (i, c) in def.checks().iter().enumerate() {
        out[i] = Some(crate::sql::parser::parse_expr(
            c.expression.as_str(),
            arena,
        )?);
    }
    Ok(out)
}

/// A column's DEFAULT source, re-parsed once per statement and indexed by
/// column. Folded constants retain their inline execution cache.
pub(crate) type ParsedDefaults<'a> = [Option<&'a Expr<'a>>; MAX_COLUMNS];

/// Re-parses every stored DEFAULT expression once per statement.
/// Generated columns are excluded — they are computed from the row by
/// [`parse_generated`], not defaulted.
pub(crate) fn parse_defaults<'a>(
    def: &'a TableDef,
    arena: &'a Arena,
) -> Result<ParsedDefaults<'a>, SqlError> {
    let mut out: ParsedDefaults<'a> = [None; MAX_COLUMNS];
    for (i, c) in def.columns().iter().enumerate() {
        if c.default.is_generated() {
            continue;
        }
        if let Some(text) = c.default.expression() {
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
        if c.default.is_generated() {
            let text = c.default.expression().expect("generated column has expr");
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
    check_index_tuple_sizes(storage, def, values, txid)?;
    check_all_unique(
        storage,
        table_index,
        def,
        schema,
        values,
        self_rowid,
        txid,
        arena,
    )?;
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
    check_index_tuple_sizes(storage, def, values, txid)?;
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
    let context = RowCtx {
        def,
        values,
        alias: None,
    };
    for (i, c) in def.checks().iter().enumerate() {
        let Some(expression) = checks[i] else {
            continue;
        };
        if matches!(
            eval(expression, arena, params, &context)?,
            Datum::Bool(false)
        ) {
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
        let Some(pi) = storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
        else {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::FOREIGN_KEY_VIOLATION,
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                def.name.as_str(),
                fk.name.as_str()
            ));
        };
        let pdef = *storage.table_def(pi, txid);
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
    let parent_definition = storage.table_def(parent_index, txid);
    let mut found = false;
    storage.for_each_row_state(parent_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = storage.visible_row_home(parent_index, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        let all_eq = storage.with_row_bytes(parent_index, rowid, home, |bytes| {
            let mut prow = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, parent_schema, &mut prow)?;
            for (&parent_column, &child_column) in parent_cols.iter().zip(child_cols) {
                let parent = &prow[parent_column as usize];
                let child = &child_values[child_column as usize];
                if parent.is_null()
                    || !compare_datums_collated(
                        storage,
                        parent_definition.columns()[parent_column as usize].collation,
                        child,
                        parent,
                    )?
                    .is_eq()
                {
                    return Ok(false);
                }
            }
            Ok(true)
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
    seq_session: &crate::sql::guc::SeqSession,
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
        let cdef = *storage.table_def(child_index, txn.txid);
        let mut cschema = [ColType::Bool; MAX_COLUMNS];
        cdef.schema(&mut cschema);
        let cschema = &cschema[..cdef.n_columns];
        for fk_index in 0..cdef.n_fkeys {
            let fk = cdef.fkeys[fk_index];
            if fk.parent_schema.as_str() != parent_schema || fk.parent.as_str() != parent_name {
                continue;
            }
            // An update triggers this key's action only when the key changed.
            if let Some(new_parent) = new_parent {
                let mut changed = false;
                for (&child_column, &parent_column) in fk.columns().iter().zip(fk.parent_cols()) {
                    let old = &old_parent[parent_column as usize];
                    let new = &new_parent[parent_column as usize];
                    if !compare_datums_collated(
                        storage,
                        cdef.columns()[child_column as usize].collation,
                        old,
                        new,
                    )?
                    .is_eq()
                    {
                        changed = true;
                        break;
                    }
                }
                if !changed {
                    continue;
                }
            }
            let action = if new_parent.is_none() {
                fk.on_delete
            } else {
                fk.on_update
            };

            // Collect the referencing rows first: the rewrites below mutate
            // the row map, so the scan must complete before them.
            let refers = |child_row: &[Datum]| -> Result<bool, SqlError> {
                for (&child_column, &parent_column) in fk.columns().iter().zip(fk.parent_cols()) {
                    let child = &child_row[child_column as usize];
                    let parent = &old_parent[parent_column as usize];
                    if child.is_null()
                        || parent.is_null()
                        || !compare_datums_collated(
                            storage,
                            cdef.columns()[child_column as usize].collation,
                            child,
                            parent,
                        )?
                        .is_eq()
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
            let mut n_match = 0usize;
            storage.for_each_row_state(child_index, &mut |rowid, state| {
                use core::ops::ControlFlow;
                let Some(home) = storage.visible_row_home(child_index, rowid, state, txn.txid)?
                else {
                    return Ok(ControlFlow::Continue(()));
                };
                let is_match = storage.with_row_bytes(child_index, rowid, home, |bytes| {
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, cschema, &mut crow)?;
                    refers(&crow[..cdef.n_columns])
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
            if matches!(
                action,
                StorageFkAction::NoAction | StorageFkAction::Restrict
            ) {
                // NO ACTION raises 23503; RESTRICT the distinct 23001, as
                // PostgreSQL (same message, different SQLSTATE).
                let code = if action == StorageFkAction::Restrict {
                    "23001"
                } else {
                    "23503"
                };
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
                    let Some(home) =
                        storage.visible_row_home(child_index, rowid, state, txn.txid)?
                    else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    // The cascade mutates storage below, so a matching row is
                    // copied into the arena wherever its bytes live.
                    let bytes = storage.row_bytes(child_index, rowid, home, arena)?;
                    let mut crow = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, cschema, &mut crow)?;
                    if refers(&crow[..cdef.n_columns])? {
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
            let defaults = parse_defaults(&cdef, arena)?;
            for &(rowid, old_bytes) in matches.iter() {
                let mut crow = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(old_bytes, cschema, &mut crow)?;
                let crow = &crow[..cdef.n_columns];
                if new_parent.is_none() && action == StorageFkAction::Cascade {
                    // Cascade the delete: grandchildren first, then this row.
                    apply_fk_parent_actions(
                        storage,
                        txn,
                        child_schema,
                        child_name,
                        crow,
                        None,
                        arena,
                        params,
                        seq_session,
                        depth - 1,
                    )?;
                    let prior = storage.write_pending(
                        child_index,
                        rowid,
                        txn.txid,
                        txn.command_id(),
                        None,
                    )?;
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
                        StorageFkAction::SetDefault => match &cdef.columns()[cc as usize].default {
                            crate::storage::ColumnDefault::None => Datum::Null,
                            crate::storage::ColumnDefault::Constant { value, .. }
                            | crate::storage::ColumnDefault::LegacyConstant(value) => {
                                value.as_datum()
                            }
                            crate::storage::ColumnDefault::Expression(_) => {
                                let expression = defaults[cc as usize].ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERNAL_ERROR,
                                        "stored column default did not parse"
                                    )
                                })?;
                                let sequence = crate::sql::sequence::SeqEval::new(
                                    storage,
                                    seq_session,
                                    txn.txid,
                                );
                                let catalog =
                                    crate::sql::query::storage_catalog(storage, arena, txn.txid);
                                let hooks = crate::sql::eval::EvalHooks {
                                    catalog: Some(&catalog),
                                    sequences: Some(&sequence),
                                    ..crate::sql::eval::NO_HOOKS
                                };
                                let value = crate::sql::eval::eval_full(
                                    expression,
                                    arena,
                                    params,
                                    &crate::sql::eval::NoColumns,
                                    &hooks,
                                )?;
                                super::coerce(
                                    value,
                                    &cdef.columns()[cc as usize],
                                    storage,
                                    txn.txid,
                                    arena,
                                )?
                            }
                            crate::storage::ColumnDefault::Generated(_) => {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "generated column cannot be a foreign-key SET DEFAULT target"
                                ));
                            }
                        },
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
                    seq_session,
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
                let prior = storage.write_pending(
                    child_index,
                    rowid,
                    txn.txid,
                    txn.command_id(),
                    Some(new_loc),
                )?;
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
pub(crate) fn table_is_referenced(storage: &Storage, schema: &str, name: &str, txid: u32) -> bool {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        if storage
            .table_def(column_index, txid)
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
) -> Result<bool, SqlError> {
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        let cdef = *storage.table_def(column_index, txid);
        for fk in cdef.fkeys() {
            if fk.parent_schema.as_str() != parent_schema || fk.parent.as_str() != parent_name {
                continue;
            }
            for (&child_column, &pc) in fk.columns().iter().zip(fk.parent_cols()) {
                let i = pc as usize;
                let (a, b) = (&old[i], &new[i]);
                if !compare_datums_collated(
                    storage,
                    cdef.columns()[child_column as usize].collation,
                    a,
                    b,
                )?
                .is_eq()
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexed_tuple_limit_is_checked_before_checkpoint() {
        assert!(check_index_tuple_size(&[0], &[Datum::Text("small")]).is_ok());
        let text = "x".repeat(crate::store::VALUE_INDEX_KEY_MAX);
        let error = check_index_tuple_size(&[0], &[Datum::Text(&text)]).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("index row size"));
    }
}
