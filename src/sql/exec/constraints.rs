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
    ColumnLookup, EvalHooks, SqlError, compare_datums_collated, eval_full,
    hash_key_collated_catalog, resolved_expression_collation, sqlstate,
};
use crate::sql::txn::TxnState;
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{ColumnMeta, MAX_COLUMNS, SqlName, Storage, ValidationRead, rowenc};

use super::{RowCtx, check_not_null};

/// The longest prefix of one value PostgreSQL shows in a row description
/// (`ExecBuildSlotValueDescription`'s `maxfieldlen`).
const MAX_DESCRIBED_FIELD_BYTES: usize = 64;

type DetailText = crate::util::StackStr<{ crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES }>;

fn detail_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "error detail exceeds {} bytes",
        crate::sql::eval::MAX_DIAGNOSTIC_DETAIL_BYTES
    )
}

/// Writes `value`'s output text, cut to [`MAX_DESCRIBED_FIELD_BYTES`] at a
/// character boundary and followed by `...` when longer, as PostgreSQL
/// describes a failing row.
fn write_described_value(out: &mut DetailText, value: Datum<'_>) {
    use core::fmt::Write;
    struct Clip<'o> {
        out: &'o mut DetailText,
        written: usize,
        clipped: bool,
    }
    impl Write for Clip<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            for character in text.chars() {
                if self.clipped {
                    return Ok(());
                }
                if self.written + character.len_utf8() > MAX_DESCRIBED_FIELD_BYTES {
                    self.clipped = true;
                    return Ok(());
                }
                self.written += character.len_utf8();
                self.out.write_char(character)?;
            }
            Ok(())
        }
    }
    if value.is_null() {
        let _ = out.write_str("null");
        return;
    }
    let mut clip = Clip {
        out,
        written: 0,
        clipped: false,
    };
    let _ = write!(clip, "{value}");
    if clip.clipped {
        let _ = clip.out.write_str("...");
    }
}

/// `Failing row contains (...).`, PostgreSQL's detail for a row that
/// violates a NOT NULL or CHECK constraint.
pub(crate) fn failing_row_detail(values: &[Datum<'_>]) -> Result<DetailText, SqlError> {
    use core::fmt::Write;
    let mut detail = DetailText::new();
    let _ = detail.write_str("Failing row contains (");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            let _ = detail.write_str(", ");
        }
        write_described_value(&mut detail, *value);
    }
    let _ = detail.write_str(").");
    if detail.is_truncated() {
        return Err(detail_full());
    }
    Ok(detail)
}

/// `Key (a, b)=(1, 2)`, PostgreSQL's description of an index key
/// (`BuildIndexValueDescription`), written to `detail`.
fn write_key_description(
    detail: &mut DetailText,
    label: &str,
    columns: &mut dyn Iterator<Item = &str>,
    values: &mut dyn Iterator<Item = Datum<'_>>,
) {
    use core::fmt::Write;
    let _ = detail.write_str(label);
    let _ = detail.write_str(" (");
    for (index, column) in columns.enumerate() {
        if index > 0 {
            let _ = detail.write_str(", ");
        }
        let _ = detail.write_str(column);
    }
    let _ = detail.write_str(")=(");
    for (index, value) in values.enumerate() {
        if index > 0 {
            let _ = detail.write_str(", ");
        }
        if value.is_null() {
            let _ = detail.write_str("null");
        } else {
            let _ = write!(detail, "{value}");
        }
    }
    let _ = detail.write_char(')');
}

/// A key description followed by `suffix`, such as ` already exists.`.
pub(crate) fn key_detail<'v>(
    columns: &mut dyn Iterator<Item = &str>,
    values: &mut dyn Iterator<Item = Datum<'v>>,
    suffix: core::fmt::Arguments<'_>,
) -> Result<DetailText, SqlError> {
    use core::fmt::Write;
    let mut detail = DetailText::new();
    write_key_description(&mut detail, "Key", columns, values);
    let _ = detail.write_fmt(suffix);
    if detail.is_truncated() {
        return Err(detail_full());
    }
    Ok(detail)
}

type ValidationRowVisitor<'callback> =
    dyn for<'row> FnMut(u64, &'row [Datum<'row>]) -> Result<(), SqlError> + 'callback;

/// Streams the current rows through retained byte owners and reuses validation
/// scratch between rows and tables.
///
/// # Safety
///
/// The callback must not retain front-arena allocations or row values. Its
/// expression and output state must precede this call's arena mark.
pub(super) unsafe fn for_each_validation_row(
    storage: &Storage,
    table: usize,
    txid: u32,
    read: ValidationRead,
    schema: &[ColType],
    arena: &Arena,
    each: &mut ValidationRowVisitor<'_>,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let scan = read.scan_in(storage, table, txid, arena)?;
        let mut validate = |rowid, values: &[Datum<'_>]| {
            crate::sql::query::check_timeout()?;
            let row_mark = arena.mark();
            let result = each(rowid, values);
            // SAFETY: the callback contract excludes references into this suffix.
            unsafe { arena.rewind_to(row_mark) };
            result
        };
        let flow = scan.for_each_overlay(&mut |rowid, home| {
            storage.with_row_bytes(table, rowid, home, |bytes| {
                let mut values = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, schema, &mut values)?;
                validate(rowid, &values[..schema.len()])
            })?;
            Ok(core::ops::ControlFlow::Continue(()))
        })?;
        if flow.is_break() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "constraint validation stopped before completing the scan"
            ));
        }
        scan.for_each_spilled_row_batch(arena, true, None, &mut |rows| {
            for row in rows {
                let mut decoded = [Datum::Null; MAX_COLUMNS];
                let values = match row.representation {
                    crate::storage::SpilledRowRepresentation::Encoded(bytes) => {
                        rowenc::decode(bytes, schema, &mut decoded)?;
                        &decoded[..schema.len()]
                    }
                    crate::storage::SpilledRowRepresentation::Values(values) => values,
                };
                validate(row.rowid, values)?;
            }
            Ok(core::ops::ControlFlow::Continue(()))
        })
    })();
    // SAFETY: every row callback and retained scan has ended; only owned errors
    // or success escape. Persistent tail allocations are preserved.
    unsafe { arena.rewind_to(mark) };
    result
}

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
    target: crate::storage::TypeDefinitionTarget,
    value: Datum<'a>,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Datum<'a>, SqlError> {
    let domain = storage
        .read_domain_definition(target, txid)?
        .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_OBJECT, "domain type does not exist"))?;
    coerce_selected_domain_value(storage, &domain, value, txid, arena, params, sequences)
}

fn coerce_selected_domain_value<'a>(
    storage: &Storage,
    domain: &crate::storage::DomainDef,
    value: Datum<'a>,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<Datum<'a>, SqlError> {
    let value = if let Some(parent_name) = domain.base_domain {
        let reference = crate::storage::TypeReference::from_bound_name(parent_name)?;
        let parent = storage
            .read_domain_definition(crate::storage::TypeDefinitionTarget::Bound(reference), txid)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "base domain \"{}.{}\" does not exist",
                    parent_name.schema.as_str(),
                    parent_name.name.as_str()
                )
            })?;
        coerce_selected_domain_value(storage, &parent, value, txid, arena, params, sequences)?
    } else {
        match domain.base {
            crate::sql::types::ColType::Enum(slot) => {
                super::coerce_enum_value(value, slot, storage, txid, arena)?
            }
            crate::sql::types::ColType::Composite(slot) => match value {
                value @ Datum::CompositeText { slot: actual, .. } if Some(actual) == slot => value,
                Datum::Text(text) | Datum::Bpchar(text) => super::decode_composite_text(
                    text,
                    slot.ok_or_else(|| {
                        sql_err!(sqlstate::UNDEFINED_OBJECT, "composite type has no identity")
                    })?,
                    storage,
                    txid,
                    arena,
                    sequences,
                )?,
                value => {
                    super::coerce_composite_value(value, slot, storage, txid, arena, sequences)?
                }
            },
            crate::sql::types::ColType::Array(
                element @ (crate::sql::types::ArrElem::Enum(_)
                | crate::sql::types::ArrElem::Composite(_)
                | crate::sql::types::ArrElem::Domain { .. }),
            ) => super::coerce_user_type_array(value, element, storage, txid, arena, sequences)?,
            base => crate::sql::eval::cast_to(value, base, arena)?,
        }
    };
    let value = super::apply_typmod(value, domain.base, domain.base_type_mod, arena)?;
    validate_domain_value(storage, txid, domain, value, arena, params, sequences)?;
    Ok(value)
}

fn validate_domain_value(
    storage: &Storage,
    txid: u32,
    domain: &crate::storage::DomainDef,
    value: Datum,
    arena: &Arena,
    params: &[Datum],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    validate_domain_not_null(storage, txid, domain, value)?;
    for check in domain.checks() {
        validate_domain_check(
            storage, txid, domain, check, value, arena, params, sequences,
        )?;
    }
    Ok(())
}

/// Enforces only a domain's `NOT NULL` rule. Domain DDL uses this for the
/// validation scan of `ALTER DOMAIN ... SET NOT NULL`; unrelated CHECK rules
/// must not make that transition fail.
pub(crate) fn validate_domain_not_null(
    storage: &Storage,
    txid: u32,
    domain: &crate::storage::DomainDef,
    value: Datum,
) -> Result<(), SqlError> {
    if value.is_null() && domain.not_null {
        let name = domain_display_name(storage, txid, domain)?;
        crate::sql::eval::stash_object_diagnostic(
            None,
            crate::sql::eval::ErrorObjects::datatype(
                domain.schema.as_str(),
                domain.name.as_str(),
                None,
            ),
        );
        return Err(sql_err!(
            sqlstate::NOT_NULL_VIOLATION,
            "domain {} does not allow null values",
            name.as_str()
        ));
    }
    Ok(())
}

/// A domain's name as PostgreSQL's `format_type_be` writes it: qualified
/// only when the search path does not find it unqualified.
fn domain_display_name(
    storage: &Storage,
    txid: u32,
    domain: &crate::storage::DomainDef,
) -> Result<crate::util::StackStr<260>, SqlError> {
    let oid = crate::sql::types::oid::domain_oid(domain.creation());
    crate::sql::catalog::user_type_name_fixed(storage, txid, oid)?.ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "domain {} has no catalog type",
            domain.name.as_str()
        )
    })
}

/// Enforces one named domain CHECK. This is deliberately separate from the
/// full write-time domain coercion path: `VALIDATE CONSTRAINT` scans exactly
/// the named constraint and does not validate unrelated `NOT VALID` checks.
#[expect(
    clippy::too_many_arguments,
    reason = "domain checks preserve the selected definition and explicit sequence context"
)]
pub(crate) fn validate_domain_check<'a>(
    storage: &Storage,
    txid: u32,
    domain: &crate::storage::DomainDef,
    check: &crate::storage::CheckConstraint,
    value: Datum<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    let context = ValueLookup { value };
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        sequences,
        ..crate::sql::eval::NO_HOOKS
    };
    let source = crate::sql::enum_values::checks::resolve(
        check.expression.as_str(),
        domain.base,
        storage,
        txid,
        arena,
    )?;
    let expression = crate::sql::parser::parse_expr(source, arena)?;
    if matches!(
        eval_full(expression, arena, params, &context, &hooks)
            .map_err(SqlError::input_expression_failure)?,
        Datum::Bool(false)
    ) {
        let name = domain_display_name(storage, txid, domain)?;
        crate::sql::eval::stash_object_diagnostic(
            None,
            crate::sql::eval::ErrorObjects::datatype(
                domain.schema.as_str(),
                domain.name.as_str(),
                Some(check.name.as_str()),
            ),
        );
        return Err(sql_err!(
            sqlstate::CHECK_VIOLATION,
            "value for domain {} violates check constraint \"{}\"",
            name.as_str(),
            check.name.as_str()
        ));
    }
    Ok(())
}

pub(crate) fn column_domain_target(
    storage: &Storage,
    column: &ColumnMeta,
    txid: u32,
) -> Result<Option<crate::storage::TypeDefinitionTarget>, SqlError> {
    let bound = storage.bind_routine_result(
        crate::storage::RoutineResult {
            ctype: column.ctype,
            user_type: column.user_type,
        },
        txid,
    )?;
    if bound.polymorphic_type().is_some() {
        return Ok(None);
    }
    let Some(identity) = bound.user_type else {
        return Ok(None);
    };
    let reference = crate::storage::TypeReference::from_bound_name(identity)?;
    if reference.kind() != crate::storage::NamedTypeKind::Domain
        || matches!(bound.ctype, ColType::Array(crate::sql::types::ArrElem::Domain { identity, .. })
            if identity == Some(reference.creation()))
    {
        return Ok(None);
    }
    Ok(Some(crate::storage::TypeDefinitionTarget::Bound(reference)))
}

/// Enforces every domain-typed column's NOT NULL and CHECK constraints against a
/// candidate row: base coercion has already happened, so this only runs the
/// domain's own rules, binding `VALUE` to each column's value.
pub(crate) fn check_domain_constraints(
    storage: &Storage,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    txid: u32,
    arena: &Arena,
    params: &[Datum],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    for (i, col) in def.columns().iter().enumerate() {
        let Some(slot) = column_domain_target(storage, col, txid)? else {
            continue;
        };
        let value = values.get(i).copied().unwrap_or(Datum::Null);
        // The column was already coerced to the base representation. Reusing
        // the catalog-aware path applies every inherited constraint as well as
        // the leaf domain's rules.
        let _ = coerce_domain_value(storage, slot, value, txid, arena, params, sequences)?;
    }
    Ok(())
}

/// 23505 for the key `columns` of `values`, with PostgreSQL's detail and
/// the constraint's table.
fn unique_violation(
    duplicate: DuplicateKey,
    name: SqlName,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    columns: &[u16],
    values: &[Datum],
) -> SqlError {
    let names = columns
        .iter()
        .map(|column| def.columns()[*column as usize].name.as_str());
    let keys = columns.iter().map(|column| values[*column as usize]);
    duplicate_key_error(duplicate, name, def, &mut { names }, &mut { keys })
}

/// Whether a duplicate key meets a written row or an index being built.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicateKey {
    Write,
    IndexBuild,
}

fn duplicate_key_error<'v>(
    duplicate: DuplicateKey,
    name: SqlName,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    names: &mut dyn Iterator<Item = &str>,
    keys: &mut dyn Iterator<Item = Datum<'v>>,
) -> SqlError {
    let suffix = match duplicate {
        DuplicateKey::Write => format_args!(" already exists."),
        DuplicateKey::IndexBuild => format_args!(" is duplicated."),
    };
    let detail = match key_detail(names, keys, suffix) {
        Ok(detail) => detail,
        Err(error) => return error,
    };
    crate::sql::eval::stash_object_diagnostic(
        Some(detail.as_str()),
        crate::sql::eval::ErrorObjects::table_constraint(
            def.schema.as_str(),
            def.name.as_str(),
            name.as_str(),
        ),
    );
    match duplicate {
        DuplicateKey::Write => sql_err!(
            sqlstate::UNIQUE_VIOLATION,
            "duplicate key value violates unique constraint \"{}\"",
            name.as_str()
        ),
        DuplicateKey::IndexBuild => sql_err!(
            sqlstate::UNIQUE_VIOLATION,
            "could not create unique index \"{}\"",
            name.as_str()
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
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    _schema: &[ColType],
    columns: &[u16],
    values: &[Datum],
    nulls_not_distinct: bool,
    rowid: u64,
    txid: u32,
) -> Result<bool, SqlError> {
    let decoder =
        crate::storage::RelationRowDecoder::new(def, &storage.table_def(table_index, txid))?;
    let Some(state) = storage.row_state(table_index, rowid)? else {
        return Ok(false);
    };
    let Some(home) = storage.committed_row_home(table_index, rowid, state)? else {
        return Ok(false);
    };
    storage.with_row_bytes(table_index, rowid, home, |bytes| {
        let mut other = [Datum::Null; MAX_COLUMNS];
        decoder.decode(bytes, &mut other)?;
        key_equal(
            storage,
            txid,
            def,
            columns,
            values,
            &other,
            nulls_not_distinct,
        )
    })
}

/// Whether both keys are equal under the index's NULL-equality rule.
fn key_equal(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
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
            txid,
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

/// Compares one encoded plain-column value-index key with a candidate row.
/// Durable probes already carry this key, so uniqueness need not reenter the
/// object-backed row reader while its value-index block is borrowed.
fn encoded_key_equal(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    columns: &[u16],
    values: &[Datum],
    encoded: &[u8],
    nulls_not_distinct: bool,
) -> Result<bool, SqlError> {
    let mut types = [ColType::Bool; crate::storage::MAX_INDEX_COLS];
    for (position, &column) in columns.iter().enumerate() {
        types[position] = def.columns()[column as usize].ctype;
    }
    let mut other = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    rowenc::decode(
        encoded,
        &types[..columns.len()],
        &mut other[..columns.len()],
    )?;
    for (position, &column) in columns.iter().enumerate() {
        let value = values[column as usize];
        let candidate = other[position];
        if value.is_null() || candidate.is_null() {
            if nulls_not_distinct && value.is_null() && candidate.is_null() {
                continue;
            }
            return Ok(false);
        }
        if !compare_datums_collated(
            storage,
            txid,
            def.columns()[column as usize].collation,
            &value,
            &candidate,
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
    txid: u32,
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
        if !compare_datums_collated(storage, txid, collations[index], value, other_value)?.is_eq() {
            return Ok(false);
        }
    }
    Ok(true)
}

struct IndexKeyEquality<'a> {
    storage: &'a Storage,
    txid: u32,
    collations: &'a [crate::sql::ast::Collation],
    operator_classes: &'a [crate::storage::IndexOperatorClass],
    nulls_not_distinct: bool,
    arena: &'a Arena,
}

fn index_key_values_equal(
    equality: &IndexKeyEquality<'_>,
    values: &[Datum],
    other: &[Datum],
) -> Result<bool, SqlError> {
    for (index, (value, other_value)) in values.iter().zip(other).enumerate() {
        if value.is_null() || other_value.is_null() {
            if equality.nulls_not_distinct && value.is_null() && other_value.is_null() {
                continue;
            }
            return Ok(false);
        }
        let equal = match equality.operator_classes[index] {
            crate::storage::IndexOperatorClass::Btree(_)
            | crate::storage::IndexOperatorClass::Hash(_)
            | crate::storage::IndexOperatorClass::Brin(_)
            | crate::storage::IndexOperatorClass::Gist(_)
            | crate::storage::IndexOperatorClass::Gin(_)
            | crate::storage::IndexOperatorClass::SpGist(_) => compare_datums_collated(
                equality.storage,
                equality.txid,
                equality.collations[index],
                value,
                other_value,
            )?
            .is_eq(),
            crate::storage::IndexOperatorClass::Catalog(oid) => {
                let slot = equality
                    .storage
                    .operator_class_slot_by_oid(oid, equality.txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "index operator class does not exist"
                        )
                    })?;
                let function = equality
                    .storage
                    .operator_class_for(slot, equality.txid)
                    .comparison_function()
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "missing support function 1 for index operator class"
                        )
                    })?;
                crate::sql::query::execute_index_comparison(
                    equality.storage,
                    equality.txid,
                    function.function,
                    *value,
                    *other_value,
                    equality.arena,
                )?
                .is_eq()
            }
        };
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

fn resolve_index_operator_classes(
    index: &crate::storage::IndexDef,
) -> Result<[Option<crate::storage::IndexOperatorClass>; crate::storage::MAX_INDEX_COLS], SqlError>
{
    let mut resolved = [None; crate::storage::MAX_INDEX_COLS];
    for (position, value) in resolved.iter_mut().enumerate().take(index.n_cols) {
        *value = Some(index.resolved_operator_classes[position].ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "index key has no resolved operator class"
            )
        })?);
    }
    Ok(resolved)
}

pub(crate) fn index_key_collations(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    columns: &[u16],
    expressions: &[Option<&Expr<'_>>],
    arena: &Arena,
) -> Result<[crate::sql::ast::Collation; crate::storage::MAX_INDEX_COLS], SqlError> {
    let mut collations = [crate::sql::ast::Collation::None; crate::storage::MAX_INDEX_COLS];
    let row = RowCtx {
        def: crate::storage::RelationDefinition::from_definition(def),
        values: &[],
        alias: None,
    };
    for (index, expression) in expressions.iter().enumerate() {
        collations[index] = match expression {
            Some(expression) => {
                let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
                resolved_expression_collation(expression, &row, Some(&catalog))?
            }
            None => def.columns()[columns[index] as usize].collation,
        };
    }
    Ok(collations)
}

pub(crate) fn index_key_values<'a>(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    row: &[Datum<'a>],
    columns: &[u16],
    expressions: &[Option<&'a Expr<'a>>],
    arena: &'a Arena,
) -> Result<[Datum<'a>; crate::storage::MAX_INDEX_COLS], SqlError> {
    let mut keys = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    for (position, expression) in expressions.iter().enumerate() {
        keys[position] = match expression {
            Some(expression) => eval_full(
                expression,
                arena,
                crate::sql::eval::NO_PARAMS,
                &RowCtx {
                    def: crate::storage::RelationDefinition::from_definition(def),
                    values: row,
                    alias: None,
                },
                &hooks,
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
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    _schema: &[ColType],
    values: &[Datum<'a>],
    self_rowid: Option<u64>,
    txid: u32,
    columns: &[u16],
    expressions: &[Option<&'a Expr<'a>>],
    expression_texts: &[Option<&str>],
    collations: &[crate::sql::ast::Collation],
    operator_classes: &[crate::storage::IndexOperatorClass],
    nulls_not_distinct: bool,
    name: SqlName,
    predicate: Option<&'a Expr<'a>>,
    duplicate: DuplicateKey,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let decoder =
        crate::storage::RelationRowDecoder::new(def, &storage.table_def(table_index, txid))?;
    if let Some(predicate) = predicate
        && !index_predicate_matches(storage, txid, def, values, predicate, arena)?
    {
        return Ok(());
    }
    let keys = index_key_values(storage, txid, def, values, columns, expressions, arena)?;
    if !nulls_not_distinct && keys[..columns.len()].iter().any(Datum::is_null) {
        return Ok(());
    }
    let violation = || {
        let names = columns.iter().enumerate().map(|(position, column)| {
            expression_texts[position].unwrap_or(def.columns()[*column as usize].name.as_str())
        });
        let values = keys[..columns.len()].iter().copied();
        duplicate_key_error(duplicate, name, def, &mut { names }, &mut { values })
    };
    let matches = |other: &[Datum]| -> Result<bool, SqlError> {
        if let Some(predicate) = predicate
            && !index_predicate_matches(storage, txid, def, other, predicate, arena)?
        {
            return Ok(false);
        }
        let other_keys = index_key_values(storage, txid, def, other, columns, expressions, arena)?;
        index_key_values_equal(
            &IndexKeyEquality {
                storage,
                txid,
                collations,
                operator_classes,
                nulls_not_distinct,
                arena,
            },
            &keys[..columns.len()],
            &other_keys[..columns.len()],
        )
    };
    storage
        .validation_read()
        .for_each_row_state(storage, table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            if Some(rowid) == self_rowid {
                return Ok(ControlFlow::Continue(()));
            }
            let Some(home) = storage.uniqueness_row_home(table_index, rowid, state, txid)? else {
                return Ok(ControlFlow::Continue(()));
            };
            let matched = storage.with_row_bytes(table_index, rowid, home, |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                decoder.decode(bytes, &mut other)?;
                matches(&other)
            })?;
            if matched {
                return Err(violation());
            }
            Ok(ControlFlow::Continue(()))
        })?;
    storage.for_each_pending_row_change(table_index, &mut |rowid, pending, image| {
        use core::ops::ControlFlow;
        if Some(rowid) == self_rowid {
            return Ok(ControlFlow::Continue(()));
        }
        let Some(image) = image else {
            return Ok(ControlFlow::Continue(()));
        };
        let matched = storage.with_row_bytes(table_index, rowid, image, |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            decoder.decode(bytes, &mut other)?;
            matches(&other)
        })?;
        if !matched {
            return Ok(ControlFlow::Continue(()));
        }
        if pending.txid != txid {
            storage.wait_for_transaction(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a concurrent unique-key writer"
            ));
        }
        Err(violation())
    })?;
    Ok(())
}

/// The shared uniqueness enforcement for one key (a single-column flag, a
/// table-level key, or a unique index). Committed collisions raise 23505; a
/// collision against another transaction's pending image waits for it. The
/// committed side is served by the value index when the table carries an
/// enforcer for these columns (an O(1) probe); keys without that accelerator
/// use the authoritative row scan. The pending side is always a bounded scan of the resident overlay
/// (pending rows are never evicted). The index definition controls whether a
/// NULL key is distinct.
#[allow(clippy::too_many_arguments)]
fn enforce_key_uniqueness(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    columns: &[u16],
    nulls_not_distinct: bool,
    name: SqlName,
    duplicate: DuplicateKey,
    source: UniqueKeySource,
) -> Result<(), SqlError> {
    if !nulls_not_distinct && columns.iter().any(|&c| values[c as usize].is_null()) {
        return Ok(());
    }
    if matches!(source, UniqueKeySource::Rows) {
        committed_scan_uniqueness(
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
            duplicate,
        )?;
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
            duplicate,
        );
    }
    if storage.has_pending_table_def(table_index, txid) {
        if !storage.pending_table_rows_rewritten(table_index, txid) {
            committed_scan_uniqueness(
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
                duplicate,
            )?;
        }
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
            duplicate,
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
        let hash = hash_key_collated_catalog(
            storage,
            txid,
            values,
            columns,
            &collations[..columns.len()],
        )?;
        let physical_columns =
            super::remap_columns(&storage.table_def(table_index, txid), def, columns)?;
        storage.probe_value(
            table_index,
            &physical_columns[..columns.len()],
            hash,
            |rowid, encoded_key| {
                if result.is_err() || Some(rowid) == self_rowid {
                    return Ok(());
                }
                // Own writes supersede the cached committed key. The pending
                // pass below compares their current key without reentering the
                // object reader while a durable index block is borrowed.
                if storage.row_has_own_pending(table_index, rowid, txid) {
                    return Ok(());
                }
                let matched = match encoded_key {
                    Some(encoded) => encoded_key_equal(
                        storage,
                        txid,
                        def,
                        columns,
                        values,
                        encoded,
                        nulls_not_distinct,
                    ),
                    None => committed_key_matches(
                        storage,
                        table_index,
                        def,
                        schema,
                        columns,
                        values,
                        nulls_not_distinct,
                        rowid,
                        txid,
                    ),
                };
                match matched {
                    Ok(true) => {
                        result = Err(unique_violation(duplicate, name, def, columns, values))
                    }
                    Ok(false) => {}
                    Err(e) => result = Err(e),
                }
                Ok(())
            },
        )?
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
            txid,
            def,
            name,
            duplicate,
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
        duplicate,
    )
}

#[derive(Clone, Copy)]
enum UniqueKeySource {
    Cache,
    Rows,
}

/// The committed-image check when a key has no value-cache binding: a full
/// scan of committed rows preserves the authoritative uniqueness verdict.
#[allow(clippy::too_many_arguments)]
fn committed_scan_uniqueness(
    storage: &Storage,
    table_index: usize,
    _schema: &[ColType],
    columns: &[u16],
    nulls_not_distinct: bool,
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    name: SqlName,
    duplicate: DuplicateKey,
) -> Result<(), SqlError> {
    let decoder =
        crate::storage::RelationRowDecoder::new(def, &storage.table_def(table_index, txid))?;
    storage
        .validation_read()
        .for_each_row_state(storage, table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            if Some(rowid) == self_rowid {
                return Ok(ControlFlow::Continue(()));
            }
            let Some(home) = storage.uniqueness_row_home(table_index, rowid, state, txid)? else {
                return Ok(ControlFlow::Continue(()));
            };
            let matched = storage.with_row_bytes(table_index, rowid, home, |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                decoder.decode(bytes, &mut other)?;
                key_equal(
                    storage,
                    txid,
                    def,
                    columns,
                    values,
                    &other,
                    nulls_not_distinct,
                )
            })?;
            if matched {
                return Err(unique_violation(duplicate, name, def, columns, values));
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
    _schema: &[ColType],
    columns: &[u16],
    nulls_not_distinct: bool,
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    name: SqlName,
    duplicate: DuplicateKey,
) -> Result<(), SqlError> {
    let decoder =
        crate::storage::RelationRowDecoder::new(def, &storage.table_def(table_index, txid))?;
    storage.for_each_pending_row_change(table_index, &mut |rowid, pending, image| {
        use core::ops::ControlFlow;
        if Some(rowid) == self_rowid {
            return Ok(ControlFlow::Continue(()));
        }
        let Some(image) = image else {
            return Ok(ControlFlow::Continue(())); // a pending delete has no key
        };
        let matched = storage.with_row_bytes(table_index, rowid, image, |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            decoder.decode(bytes, &mut other)?;
            key_equal(
                storage,
                txid,
                def,
                columns,
                values,
                &other,
                nulls_not_distinct,
            )
        })?;
        if matched {
            if pending.txid != txid {
                storage.wait_for_transaction(txid, pending.txid)?;
                return Err(sql_err!(
                    sqlstate::INTERNAL_LOCK_WAIT,
                    "statement is waiting for a concurrent unique-key writer"
                ));
            }
            return Err(unique_violation(duplicate, name, def, columns, values));
        }
        Ok(ControlFlow::Continue(()))
    })?;
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
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    _schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    columns: &[u16],
    nulls_not_distinct: bool,
    name: SqlName,
    duplicate: DuplicateKey,
    predicate: &Expr,
    arena: &Arena,
) -> Result<(), SqlError> {
    let decoder =
        crate::storage::RelationRowDecoder::new(def, &storage.table_def(table_index, txid))?;
    if !index_predicate_matches(storage, txid, def, values, predicate, arena)?
        || (!nulls_not_distinct
            && columns
                .iter()
                .any(|&column| values[column as usize].is_null()))
    {
        return Ok(());
    }
    let matches = |other: &[Datum]| -> Result<bool, SqlError> {
        Ok(
            index_predicate_matches(storage, txid, def, other, predicate, arena)?
                && key_equal(
                    storage,
                    txid,
                    def,
                    columns,
                    values,
                    other,
                    nulls_not_distinct,
                )?,
        )
    };
    storage
        .validation_read()
        .for_each_row_state(storage, table_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            if Some(rowid) == self_rowid {
                return Ok(ControlFlow::Continue(()));
            }
            let Some(home) = storage.uniqueness_row_home(table_index, rowid, state, txid)? else {
                return Ok(ControlFlow::Continue(()));
            };
            let matched = storage.with_row_bytes(table_index, rowid, home, |bytes| {
                let mut other = [Datum::Null; MAX_COLUMNS];
                decoder.decode(bytes, &mut other)?;
                matches(&other)
            })?;
            if matched {
                return Err(unique_violation(duplicate, name, def, columns, values));
            }
            Ok(ControlFlow::Continue(()))
        })?;
    storage.for_each_pending_row_change(table_index, &mut |rowid, pending, image| {
        use core::ops::ControlFlow;
        if Some(rowid) == self_rowid {
            return Ok(ControlFlow::Continue(()));
        }
        let Some(image) = image else {
            return Ok(ControlFlow::Continue(()));
        };
        let matched = storage.with_row_bytes(table_index, rowid, image, |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            decoder.decode(bytes, &mut other)?;
            matches(&other)
        })?;
        if !matched {
            return Ok(ControlFlow::Continue(()));
        }
        if pending.txid != txid {
            storage.wait_for_transaction(txid, pending.txid)?;
            return Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for a concurrent unique-key writer"
            ));
        }
        Err(unique_violation(duplicate, name, def, columns, values))
    })?;
    Ok(())
}

pub(crate) fn index_predicate_matches(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    predicate: &Expr,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    match eval_full(
        predicate,
        arena,
        crate::sql::eval::NO_PARAMS,
        &RowCtx {
            def: crate::storage::RelationDefinition::from_definition(def),
            values,
            alias: None,
        },
        &hooks,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_unique(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    duplicate: DuplicateKey,
) -> Result<(), SqlError> {
    for (i, column) in def.columns().iter().enumerate() {
        if !column.unique {
            continue;
        }
        let name = def.column_key_name(i).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "inline unique key has no admitted name"
            )
        })?;
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
            name,
            duplicate,
            UniqueKeySource::Cache,
        )?;
    }
    Ok(())
}

/// Enforces both column-level UNIQUE/PRIMARY KEY and every UNIQUE index.
#[allow(clippy::too_many_arguments)]
pub(crate) fn check_all_unique(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    arena: &Arena,
    duplicate: DuplicateKey,
) -> Result<(), SqlError> {
    check_unique(
        storage,
        table_index,
        def,
        schema,
        values,
        self_rowid,
        txid,
        duplicate,
    )?;
    check_unique_indexes(
        storage,
        table_index,
        def,
        schema,
        values,
        self_rowid,
        txid,
        arena,
        UniqueIndexCheck::Cache,
    )
}

/// Rejects an indexed tuple before its row or CREATE INDEX WAL can commit if
/// the immutable object generation cannot represent the key. Checkpoint is
/// never the first observer of this physical limit.
pub(crate) fn check_index_tuple_size(columns: &[u16], values: &[Datum]) -> Result<(), SqlError> {
    check_index_tuple_size_with_include(columns, &[], values)
}

pub(crate) fn check_index_tuple_size_with_include(
    columns: &[u16],
    include_columns: &[u16],
    values: &[Datum],
) -> Result<(), SqlError> {
    if columns.len() > crate::storage::MAX_INDEX_COLS {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "index has {} columns, exceeding the internal limit {}",
            columns.len(),
            crate::storage::MAX_INDEX_COLS
        ));
    }
    if include_columns.len() > crate::storage::MAX_COLUMNS {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "index has {} included columns, exceeding the internal limit {}",
            include_columns.len(),
            crate::storage::MAX_COLUMNS
        ));
    }
    let mut key = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    for (at, column) in columns.iter().enumerate() {
        key[at] = *values.get(*column as usize).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "index column position {} is outside row width {}",
                column,
                values.len()
            )
        })?;
    }
    check_index_key_tuple_size(&key[..columns.len()], include_columns, values)
}

fn check_index_key_tuple_size(
    key: &[Datum],
    include_columns: &[u16],
    values: &[Datum],
) -> Result<(), SqlError> {
    let encoded = rowenc::encoded_len(key);
    if encoded > crate::store::VALUE_INDEX_KEY_MAX {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "index row size {} exceeds maximum {}",
            encoded,
            crate::store::VALUE_INDEX_KEY_MAX
        ));
    }
    if !include_columns.is_empty() {
        let mut payload = [Datum::Null; crate::storage::MAX_COLUMNS];
        for (at, column) in include_columns.iter().enumerate() {
            payload[at] = *values.get(*column as usize).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "included column position {} is outside row width {}",
                    column,
                    values.len()
                )
            })?;
        }
        let physical =
            encoded.saturating_add(rowenc::encoded_len(&payload[..include_columns.len()]));
        if physical > crate::store::VALUE_INDEX_TUPLE_MAX {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "index row size {} exceeds maximum {}",
                physical,
                crate::store::VALUE_INDEX_TUPLE_MAX
            ));
        }
    }
    Ok(())
}

/// Validates every persistent tuple shape visible on a table, including
/// non-unique named indexes (which uniqueness-only enforcement does not walk).
pub(crate) fn check_index_tuple_sizes(
    storage: &Storage,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let mark = arena.mark();
    let result = check_index_tuple_sizes_inner(storage, def, values, txid, arena);
    // SAFETY: parsed definitions and evaluated keys are consumed completely
    // by the size check and never escape this call.
    unsafe { arena.rewind_to(mark) };
    result
}

fn check_index_tuple_sizes_inner(
    storage: &Storage,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    for (column, metadata) in def.columns().iter().enumerate() {
        if metadata.unique {
            check_index_tuple_size(&[column as u16], values)?;
        }
    }
    for unique in def.uniques() {
        check_index_tuple_size(unique.columns(), values)?;
    }
    let table_slot = storage
        .find_visible(def.schema.as_str(), def.name.as_str(), txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "index tuple validation cannot resolve table {}.{}",
                def.schema.as_str(),
                def.name.as_str()
            )
        })?;
    let index_source = storage.table_def(table_slot, txid);
    for index in storage.indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        if !index.mutable_for(txid).kind.live() {
            continue;
        }
        let mut columns = [0u16; crate::storage::MAX_INDEX_COLS];
        let mut include_columns = [0u16; crate::storage::MAX_INDEX_COLS];
        for (target, source) in columns.iter_mut().zip(&index.columns[..index.n_cols]) {
            let name = index_source
                .columns()
                .get(usize::from(*source))
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "index column position {} is outside table width {}",
                        source,
                        index_source.n_columns
                    )
                })?
                .name;
            *target = def.column_index(name.as_str()).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "visible index references removed column {}",
                    name.as_str()
                )
            })? as u16;
        }
        for (target, source) in include_columns
            .iter_mut()
            .zip(&index.include_columns[..index.n_include_cols])
        {
            let name = index_source
                .columns()
                .get(usize::from(*source))
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "included column position {} is outside table width {}",
                        source,
                        index_source.n_columns
                    )
                })?
                .name;
            *target = def.column_index(name.as_str()).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "visible index references removed column {}",
                    name.as_str()
                )
            })? as u16;
        }
        if let Some(source) = index.predicate {
            let source = arena
                .alloc_str(source.as_str())
                .map_err(|_| crate::sql::eval::arena_full())?;
            let predicate = crate::sql::parser::parse_expr(source, arena)?;
            if !index_predicate_matches(storage, txid, def, values, predicate, arena)? {
                continue;
            }
        }
        let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
        for (position, source) in index.expressions[..index.n_cols].iter().enumerate() {
            if let Some(source) = source {
                let source = arena
                    .alloc_str(source.as_str())
                    .map_err(|_| crate::sql::eval::arena_full())?;
                expressions[position] = Some(crate::sql::parser::parse_expr(source, arena)?);
            }
        }
        let key = index_key_values(
            storage,
            txid,
            def,
            values,
            &columns[..index.n_cols],
            &expressions[..index.n_cols],
            arena,
        )?;
        check_index_key_tuple_size(
            &key[..index.n_cols],
            &include_columns[..index.n_include_cols],
            values,
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub enum UniqueIndexCheck {
    Cache,
    Rebuild { selected: Option<i32> },
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_reindex_constraints(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    rowid: u64,
    txid: u32,
    arena: &Arena,
    selected: Option<i32>,
) -> Result<(), SqlError> {
    for (column, metadata) in def.columns().iter().enumerate() {
        if !metadata.unique
            || selected.is_some_and(|oid| {
                def.column_index_identity(column)
                    .is_none_or(|identity| identity.oid() != oid)
            })
        {
            continue;
        }
        let name = def.column_key_name(column).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "inline unique key has no admitted name"
            )
        })?;
        enforce_key_uniqueness(
            storage,
            table_index,
            def,
            schema,
            values,
            Some(rowid),
            txid,
            &[column as u16],
            false,
            name,
            DuplicateKey::IndexBuild,
            UniqueKeySource::Rows,
        )?;
    }
    for key in def.uniques() {
        if selected.is_some_and(|oid| {
            key.index_identity
                .is_none_or(|identity| identity.oid() != oid)
        }) {
            continue;
        }
        enforce_key_uniqueness(
            storage,
            table_index,
            def,
            schema,
            values,
            Some(rowid),
            txid,
            key.columns(),
            key.kind.nulls_not_distinct(),
            key.name,
            DuplicateKey::IndexBuild,
            UniqueKeySource::Rows,
        )?;
    }
    for exclusion in def.exclusions() {
        if selected.is_some_and(|oid| {
            exclusion
                .index_identity
                .is_none_or(|identity| identity.oid() != oid)
        }) {
            continue;
        }
        let predicate = exclusion
            .predicate
            .map(|source| {
                let source = arena
                    .alloc_str(source.as_str())
                    .map_err(|_| crate::sql::eval::arena_full())?;
                crate::sql::parser::parse_expr(source, arena)
            })
            .transpose()?;
        enforce_exclusion_constraint(
            storage,
            table_index,
            def,
            schema,
            values,
            Some(rowid),
            txid,
            exclusion,
            predicate,
            arena,
            Some(storage.validation_read()),
        )?;
    }
    check_unique_indexes(
        storage,
        table_index,
        def,
        schema,
        values,
        Some(rowid),
        txid,
        arena,
        UniqueIndexCheck::Rebuild { selected },
    )
}

/// Normal writes may probe the derived cache. Explicit rebuilding validates
/// only its selected indexes against authoritative rows, including own writes.
#[allow(clippy::too_many_arguments)]
pub fn check_unique_indexes(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    arena: &Arena,
    check: UniqueIndexCheck,
) -> Result<(), SqlError> {
    for index in storage.unique_indexes_for(def.schema.as_str(), def.name.as_str(), txid) {
        if matches!(check, UniqueIndexCheck::Cache) && !index.mutable_for(txid).immediate {
            // The constraint's own timing controls checks. A detached REINDEX
            // replacement has no check trigger until the atomic link swap.
            continue;
        }
        if matches!(check, UniqueIndexCheck::Rebuild { selected: Some(oid) } if oid != crate::sql::catalog::explicit_index_oid(&index))
        {
            continue;
        }
        let icols = &index.columns[..index.n_cols];
        let index_name = index.name_for(txid);
        let name = index_name;
        let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
        let mut expression_texts = [None; crate::storage::MAX_INDEX_COLS];
        for (position, source) in index.expressions.iter().enumerate().take(index.n_cols) {
            if let Some(source) = source {
                let source = arena
                    .alloc_str(source.as_str())
                    .map_err(|_| crate::sql::eval::arena_full())?;
                expression_texts[position] = Some(source);
                expressions[position] = Some(crate::sql::parser::parse_expr(source, arena)?);
            }
        }
        let resolved = resolve_index_operator_classes(&index)?;
        let custom = resolved[..index.n_cols]
            .iter()
            .any(|class| matches!(class, Some(crate::storage::IndexOperatorClass::Catalog(_))));
        let custom_collation =
            index.columns[..index.n_cols]
                .iter()
                .enumerate()
                .any(|(position, column)| {
                    index.collations[position] != def.columns()[*column as usize].collation
                });
        if matches!(check, UniqueIndexCheck::Rebuild { .. })
            || custom
            || custom_collation
            || index.expressions[..index.n_cols]
                .iter()
                .any(Option::is_some)
        {
            let predicate = index
                .predicate
                .map(|source| {
                    let source = arena
                        .alloc_str(source.as_str())
                        .map_err(|_| crate::sql::eval::arena_full())?;
                    crate::sql::parser::parse_expr(source, arena)
                })
                .transpose()?;
            let mut classes = [crate::storage::IndexOperatorClass::Btree(
                crate::sql::types::BtreeOperatorClass::Int4,
            ); crate::storage::MAX_INDEX_COLS];
            for position in 0..index.n_cols {
                classes[position] = resolved[position].expect("index operator class resolved");
            }
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
                &expression_texts[..index.n_cols],
                &index.collations[..index.n_cols],
                &classes[..index.n_cols],
                index.nulls_not_distinct,
                name,
                predicate,
                match check {
                    UniqueIndexCheck::Cache => DuplicateKey::Write,
                    UniqueIndexCheck::Rebuild { .. } => DuplicateKey::IndexBuild,
                },
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
                name,
                match check {
                    UniqueIndexCheck::Cache => DuplicateKey::Write,
                    UniqueIndexCheck::Rebuild { .. } => DuplicateKey::IndexBuild,
                },
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
                name,
                match check {
                    UniqueIndexCheck::Cache => DuplicateKey::Write,
                    UniqueIndexCheck::Rebuild { .. } => DuplicateKey::IndexBuild,
                },
                UniqueKeySource::Cache,
            )?;
        }
    }
    Ok(())
}

/// Pre-parsed CHECK predicates for a statement, aligned with `def.checks()`.
pub(crate) type ParsedChecks<'a> =
    [Option<&'a Expr<'a>>; crate::storage::MAX_CHECKS + crate::storage::MAX_EXCLUSIONS];

/// Re-parses every stored CHECK predicate once per statement into the arena.
pub(crate) fn parse_checks<'a>(
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    arena: &'a Arena,
) -> Result<ParsedChecks<'a>, SqlError> {
    let mut out: ParsedChecks<'a> =
        [None; crate::storage::MAX_CHECKS + crate::storage::MAX_EXCLUSIONS];
    for (i, c) in def.checks().iter().enumerate() {
        let source = arena
            .alloc_str(c.expression.as_str())
            .map_err(|_| crate::sql::eval::arena_full())?;
        out[i] = Some(crate::sql::parser::parse_expr(source, arena)?);
    }
    for (index, exclusion) in def.exclusions().iter().enumerate() {
        if let Some(predicate) = &exclusion.predicate {
            let source = arena
                .alloc_str(predicate.as_str())
                .map_err(|_| crate::sql::eval::arena_full())?;
            out[crate::storage::MAX_CHECKS + index] =
                Some(crate::sql::parser::parse_expr(source, arena)?);
        }
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
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    arena: &'a Arena,
    storage: &Storage,
    txid: u32,
) -> Result<ParsedDefaults<'a>, SqlError> {
    let mut out: ParsedDefaults<'a> = [None; MAX_COLUMNS];
    let input_sections = def.type_inputs.as_ref().detached_sections(arena)?;
    for (i, c) in def.columns().iter().enumerate() {
        if c.default.is_generated() || c.default.constant().is_some() {
            continue;
        }
        if let Some(text) = c.default.expression() {
            let original = crate::sql::enum_values::checks::default_display(text.as_str(), arena)?;
            let text = super::super::enum_values::checks::resolve_default(
                text.as_str(),
                c.ctype,
                storage,
                txid,
                arena,
            )?;
            let section = crate::storage::StoredInputSection::TableDefault(def.attribute_number(i));
            let inputs = input_sections
                .binary_search_by_key(&section, |(section, _)| *section)
                .ok()
                .map_or(&[][..], |index| input_sections[index].1);
            let inputs = if original == text {
                inputs
            } else {
                crate::sql::type_inputs::rebase(
                    original,
                    text,
                    crate::sql::parser::StoredSqlKind::Expression,
                    inputs,
                    |name| Ok(crate::sql::type_inputs::target_oid(storage, txid, name)),
                    arena,
                )?
            };
            out[i] = Some(crate::sql::parser::parse_expression_with_type_inputs(
                text,
                inputs,
                arena,
                |parser| {
                    crate::sql::type_inputs::validate_restored_candidates(parser, |candidate| {
                        Ok(inputs
                            .iter()
                            .find(|input| {
                                (input.start, input.end, input.kind)
                                    == (candidate.start, candidate.end, candidate.kind)
                            })
                            .map(|input| input.type_oid)
                            .or_else(|| {
                                crate::sql::type_inputs::candidate_target_oid(
                                    storage, txid, candidate,
                                )
                            }))
                    })
                },
            )?);
        }
    }
    Ok(out)
}

/// Re-parses every `GENERATED ALWAYS AS (expr) STORED` expression once per
/// statement (indexed by column); `None` for non-generated columns. Evaluated
/// against the row's other columns after they are filled.
pub(crate) fn parse_generated<'a>(
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    arena: &'a Arena,
    storage: &Storage,
    txid: u32,
) -> Result<ParsedDefaults<'a>, SqlError> {
    let mut out: ParsedDefaults<'a> = [None; MAX_COLUMNS];
    let input_sections = def.type_inputs.as_ref().detached_sections(arena)?;
    for (i, c) in def.columns().iter().enumerate() {
        if c.default.is_generated() {
            let text = c.default.expression().expect("generated column has expr");
            let source = arena
                .alloc_str(text.as_str())
                .map_err(|_| crate::sql::eval::arena_full())?;
            let section = crate::storage::StoredInputSection::TableDefault(def.attribute_number(i));
            let inputs = input_sections
                .binary_search_by_key(&section, |(section, _)| *section)
                .ok()
                .map_or(&[][..], |index| input_sections[index].1);
            out[i] = Some(crate::sql::parser::parse_expression_with_type_inputs(
                source,
                inputs,
                arena,
                |parser| {
                    crate::sql::type_inputs::validate_restored_candidates(parser, |candidate| {
                        Ok(inputs
                            .iter()
                            .find(|input| {
                                (input.start, input.end, input.kind)
                                    == (candidate.start, candidate.end, candidate.kind)
                            })
                            .map(|input| input.type_oid)
                            .or_else(|| {
                                crate::sql::type_inputs::candidate_target_oid(
                                    storage, txid, candidate,
                                )
                            }))
                    })
                },
            )?);
        }
    }
    Ok(out)
}

/// The stored identity and the row excluded from uniqueness checks can differ
/// when an update moves to another partition. Validation publishes no row.
#[derive(Clone, Copy)]
pub(crate) enum ConstraintRow {
    Stored {
        excluded: Option<u64>,
        rowid: u64,
    },
    Validation {
        excluded: Option<u64>,
    },
    Existing {
        excluded: Option<u64>,
        read: ValidationRead,
    },
}

impl ConstraintRow {
    pub(crate) fn insert(rowid: u64) -> Self {
        Self::Stored {
            excluded: None,
            rowid,
        }
    }

    pub(crate) fn update(rowid: u64) -> Self {
        Self::Stored {
            excluded: Some(rowid),
            rowid,
        }
    }

    fn excluded(self) -> Option<u64> {
        match self {
            Self::Stored { excluded, .. }
            | Self::Validation { excluded }
            | Self::Existing { excluded, .. } => excluded,
        }
    }

    pub(crate) fn stored(self) -> Option<u64> {
        match self {
            Self::Stored { rowid, .. } => Some(rowid),
            Self::Validation { .. } | Self::Existing { .. } => None,
        }
    }

    fn validation_read(self) -> Option<ValidationRead> {
        match self {
            Self::Existing { read, .. } => Some(read),
            Self::Stored { .. } | Self::Validation { .. } => None,
        }
    }
}

/// Enforces constraints using the exact identity that the caller will store,
/// in PostgreSQL's order: the row's own CHECK and domain constraints before
/// its index keys and foreign keys.
#[allow(clippy::too_many_arguments)]
pub(crate) fn enforce_row_constraints(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    row: ConstraintRow,
    txid: u32,
    txn: Option<&mut TxnState>,
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    enforce_row_checks(storage, def, values, txid, checks, arena, params, sequences)?;
    enforce_row_keys(
        storage,
        table_index,
        def,
        schema,
        values,
        row,
        txid,
        txn,
        checks,
        arena,
    )
}

/// The row's CHECK, partition bound and domain constraints, which
/// PostgreSQL's `ExecConstraints` tests before the row is stored.
#[allow(clippy::too_many_arguments)]
pub(crate) fn enforce_row_checks(
    storage: &Storage,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    txid: u32,
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    check_detached_partition_bound(def, values)?;
    check_row_checks(storage, def, checks, values, txid, arena, params, sequences)?;
    check_domain_constraints(storage, def, values, txid, arena, params, sequences)
}

/// The row's unique, exclusion and foreign key constraints, which PostgreSQL
/// tests while and after it stores the row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn enforce_row_keys(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    row: ConstraintRow,
    txid: u32,
    mut txn: Option<&mut TxnState>,
    checks: &ParsedChecks,
    arena: &Arena,
) -> Result<(), SqlError> {
    check_index_tuple_sizes(storage, def, values, txid, arena)?;
    let self_rowid = row.excluded();
    // Existing rows meeting a new key are its index being built.
    let duplicate = if matches!(row, ConstraintRow::Existing { .. }) {
        DuplicateKey::IndexBuild
    } else {
        DuplicateKey::Write
    };
    let constraint_table = storage
        .find_visible(def.schema.as_str(), def.name.as_str(), txid)
        .unwrap_or(table_index);
    if def.partition.is_partitioned() {
        for candidate in 0..storage.table_count() {
            if !storage.table(candidate).visible_to(txid)
                || storage
                    .table_def(candidate, txid)
                    .partition
                    .is_partitioned()
                || (candidate != constraint_table
                    && !storage.partition_descends_from(candidate, constraint_table, txid))
            {
                continue;
            }
            check_all_unique(
                storage, candidate, def, schema, values, self_rowid, txid, arena, duplicate,
            )?;
            check_or_defer_unique_keys(
                storage,
                candidate,
                constraint_table,
                def,
                schema,
                values,
                row,
                txid,
                txn.as_deref_mut(),
            )?;
            check_or_defer_exclusions(
                storage,
                candidate,
                constraint_table,
                def,
                schema,
                values,
                row,
                txid,
                txn.as_deref_mut(),
                checks,
                arena,
            )?;
        }
    } else {
        check_all_unique(
            storage,
            table_index,
            def,
            schema,
            values,
            self_rowid,
            txid,
            arena,
            duplicate,
        )?;
        check_or_defer_unique_keys(
            storage,
            table_index,
            constraint_table,
            def,
            schema,
            values,
            row,
            txid,
            txn.as_deref_mut(),
        )?;
        check_or_defer_exclusions(
            storage,
            table_index,
            constraint_table,
            def,
            schema,
            values,
            row,
            txid,
            txn.as_deref_mut(),
            checks,
            arena,
        )?;
    }
    check_or_defer_fk_child(
        storage,
        table_index,
        constraint_table,
        def,
        values,
        row,
        txid,
        txn,
        arena,
    )?;
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
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    checks: &ParsedChecks,
    arena: &Arena,
    params: &[Datum],
    txid: u32,
    read: ValidationRead,
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    check_not_null(def, values)?;
    check_index_tuple_sizes(storage, def, values, txid, arena)?;
    check_detached_partition_bound(def, values)?;
    check_row_checks(storage, def, checks, values, txid, arena, params, sequences)?;
    check_domain_constraints(storage, def, values, txid, arena, params, sequences)?;
    check_fk_child(storage, def, values, txid, read, arena)?;
    Ok(())
}

fn check_detached_partition_bound(
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
) -> Result<(), SqlError> {
    if let Some(detached) = def.partition.detached_bound
        && !crate::storage::partition_bound_matches(
            detached.scheme.strategy,
            detached.scheme.keys,
            detached.scheme.n_keys,
            detached.bound,
            values,
        )?
    {
        return Err(sql_err!(
            crate::sql::eval::sqlstate::CHECK_VIOLATION,
            "new row for relation \"{}\" violates partition constraint",
            def.name.as_str()
        ));
    }
    Ok(())
}

/// Evaluates each CHECK predicate against the candidate row. A predicate that
/// is FALSE raises 23514; NULL and TRUE both pass, per SQL three-valued logic.
#[expect(
    clippy::too_many_arguments,
    reason = "row checks preserve row and statement sequence contexts"
)]
fn check_row_checks(
    storage: &Storage,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    checks: &ParsedChecks,
    values: &[Datum],
    txid: u32,
    arena: &Arena,
    params: &[Datum],
    sequences: Option<&dyn crate::sql::eval::SequenceAccess>,
) -> Result<(), SqlError> {
    let context = RowCtx {
        def: crate::storage::RelationDefinition::from_definition(def),
        values,
        alias: None,
    };
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        sequences,
        ..crate::sql::eval::NO_HOOKS
    };
    for (i, c) in def.checks().iter().enumerate() {
        if !c.validation.enforced() {
            continue;
        }
        let Some(expression) = checks[i] else {
            continue;
        };
        if matches!(
            eval_full(expression, arena, params, &context, &hooks)?,
            Datum::Bool(false)
        ) {
            let detail = failing_row_detail(&values[..def.columns().len()])?;
            crate::sql::eval::stash_object_diagnostic(
                Some(detail.as_str()),
                crate::sql::eval::ErrorObjects::table_constraint(
                    def.schema.as_str(),
                    def.name.as_str(),
                    c.name.as_str(),
                ),
            );
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
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    txid: u32,
    read: ValidationRead,
    arena: &Arena,
) -> Result<(), SqlError> {
    for fk in def.fkeys() {
        if fk.validation.enforced()
            && !fk
                .columns()
                .iter()
                .any(|&column| values[column as usize].is_null())
        {
            check_fk_child_one_at(
                storage,
                def,
                fk,
                values,
                txid,
                ParentRead::Validation(read),
                arena,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn check_or_defer_unique_keys(
    storage: &Storage,
    table_index: usize,
    constraint_table: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    row: ConstraintRow,
    txid: u32,
    mut txn: Option<&mut TxnState>,
) -> Result<(), SqlError> {
    let self_rowid = row.excluded();
    for key in def.uniques() {
        if key.timing.is_deferrable()
            && let Some(rowid) = row.stored()
            && let Some(transaction) = txn.as_deref_mut()
        {
            let identity = transaction.constraint_identity(constraint_table as u32, key.name);
            transaction.defer_constraint(identity, rowid)?;
            continue;
        }
        enforce_key_uniqueness(
            storage,
            table_index,
            def,
            schema,
            values,
            self_rowid,
            txid,
            key.columns(),
            key.kind.nulls_not_distinct(),
            key.name,
            // Existing rows meeting a new key are its index being built.
            if matches!(row, ConstraintRow::Existing { .. }) {
                DuplicateKey::IndexBuild
            } else {
                DuplicateKey::Write
            },
            UniqueKeySource::Cache,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn check_or_defer_exclusions(
    storage: &Storage,
    table_index: usize,
    constraint_table: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    schema: &[ColType],
    values: &[Datum],
    row: ConstraintRow,
    txid: u32,
    mut txn: Option<&mut TxnState>,
    expressions: &ParsedChecks,
    arena: &Arena,
) -> Result<(), SqlError> {
    let self_rowid = row.excluded();
    for (index, exclusion) in def.exclusions().iter().enumerate() {
        if exclusion.timing.is_deferrable()
            && let Some(rowid) = row.stored()
            && let Some(transaction) = txn.as_deref_mut()
        {
            let identity = transaction.constraint_identity(constraint_table as u32, exclusion.name);
            transaction.defer_constraint(identity, rowid)?;
            continue;
        }
        enforce_exclusion_constraint(
            storage,
            table_index,
            def,
            schema,
            values,
            self_rowid,
            txid,
            exclusion,
            expressions[crate::storage::MAX_CHECKS + index],
            arena,
            row.validation_read(),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enforce_exclusion_constraint(
    storage: &Storage,
    table_index: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    _schema: &[ColType],
    values: &[Datum],
    self_rowid: Option<u64>,
    txid: u32,
    exclusion: &crate::storage::ExclusionConstraint,
    predicate: Option<&Expr>,
    arena: &Arena,
    read: Option<ValidationRead>,
) -> Result<(), SqlError> {
    let decoder =
        crate::storage::RelationRowDecoder::new(def, &storage.table_def(table_index, txid))?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    let predicate_holds = |row: &[Datum]| -> Result<bool, SqlError> {
        let Some(predicate) = predicate else {
            return Ok(true);
        };
        let context = RowCtx {
            def: crate::storage::RelationDefinition::from_definition(def),
            values: row,
            alias: None,
        };
        Ok(matches!(
            eval_full(predicate, arena, &[], &context, &hooks)?,
            Datum::Bool(true)
        ))
    };
    if !predicate_holds(values)? {
        return Ok(());
    }
    let mut visit = |rowid: u64, state: crate::storage::RowRead<'_>| {
        use core::ops::ControlFlow;
        if self_rowid == Some(rowid) {
            return Ok(ControlFlow::Continue(()));
        }
        let home = match read {
            Some(read) => read.visible_row_home(storage, table_index, rowid, state, txid)?,
            None => storage.visible_row_home(table_index, rowid, state, txid)?,
        };
        let Some(home) = home else {
            return Ok(ControlFlow::Continue(()));
        };
        let conflicts = storage.with_row_bytes(table_index, rowid, home, |bytes| {
            let mut other = [Datum::Null; MAX_COLUMNS];
            decoder.decode(bytes, &mut other)?;
            let other = &other[..def.n_columns];
            if !predicate_holds(other)? {
                return Ok(false);
            }
            for position in 0..exclusion.n_cols {
                let column = exclusion.columns[position] as usize;
                if values[column].is_null() || other[column].is_null() {
                    return Ok(false);
                }
                let operator = match exclusion.operators[position] {
                    crate::storage::ExclusionOperator::Equal => crate::sql::ast::BinaryOp::Eq,
                    crate::storage::ExclusionOperator::Overlaps => {
                        crate::sql::ast::BinaryOp::Overlaps
                    }
                    crate::storage::ExclusionOperator::Adjacent => {
                        crate::sql::ast::BinaryOp::Adjacent
                    }
                };
                if !crate::sql::eval::exclusion_operator(
                    operator,
                    values[column],
                    other[column],
                    arena,
                )? {
                    return Ok(false);
                }
            }
            let names = || {
                exclusion.columns[..exclusion.n_cols]
                    .iter()
                    .map(|column| def.columns()[*column as usize].name.as_str())
            };
            let mut existing_text = DetailText::new();
            write_key_description(
                &mut existing_text,
                "key",
                &mut names(),
                &mut exclusion.columns[..exclusion.n_cols]
                    .iter()
                    .map(|column| other[*column as usize]),
            );
            let detail = key_detail(
                &mut names(),
                &mut exclusion.columns[..exclusion.n_cols]
                    .iter()
                    .map(|column| values[*column as usize]),
                format_args!(" conflicts with existing {}.", existing_text.as_str()),
            )?;
            crate::sql::eval::stash_object_diagnostic(
                Some(detail.as_str()),
                crate::sql::eval::ErrorObjects::table_constraint(
                    def.schema.as_str(),
                    def.name.as_str(),
                    exclusion.name.as_str(),
                ),
            );
            Ok(true)
        })?;
        if conflicts {
            return Err(sql_err!(
                sqlstate::EXCLUSION_VIOLATION,
                "conflicting key value violates exclusion constraint \"{}\"",
                exclusion.name.as_str()
            ));
        }
        Ok(ControlFlow::Continue(()))
    };
    match read {
        Some(read) => read.for_each_row_state(storage, table_index, &mut visit)?,
        None => storage.for_each_row_state(table_index, &mut visit)?,
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// A written row's foreign keys are checked as AFTER ROW events at the end
/// of its statement (or at commit when deferred), in the trigger-name order
/// of PostgreSQL's `RI_ConstraintTrigger_c_<oid>` triggers. Rows that are not
/// written (validation) are checked at once.
#[expect(
    clippy::too_many_arguments,
    reason = "a row's foreign key checks need its storage and constraint identities"
)]
fn check_or_defer_fk_child(
    storage: &Storage,
    table_index: usize,
    constraint_table: usize,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    values: &[Datum],
    row: ConstraintRow,
    txid: u32,
    mut txn: Option<&mut TxnState>,
    arena: &Arena,
) -> Result<(), SqlError> {
    for fk in def.fkeys() {
        if !fk.validation.enforced()
            || fk
                .columns()
                .iter()
                .any(|&column| values[column as usize].is_null())
        {
            continue;
        }
        if let Some(rowid) = row.stored()
            && let Some(transaction) = txn.as_deref_mut()
        {
            let identity = transaction.constraint_identity(constraint_table as u32, fk.name);
            let (trigger, event) = if row.excluded().is_some() {
                (
                    crate::storage::ForeignKeyTrigger::CheckUpdate,
                    crate::sql::ast::TriggerEvents::UPDATE,
                )
            } else {
                (
                    crate::storage::ForeignKeyTrigger::CheckInsert,
                    crate::sql::ast::TriggerEvents::INSERT,
                )
            };
            transaction.pend_foreign_key_event(
                table_index as u16,
                foreign_key_trigger_name(fk, trigger)?,
                identity,
                crate::sql::txn::ForeignKeyCheck::Referencing,
                constraint_table as u16,
                event,
                rowid,
                None,
            )?;
            continue;
        }
        if let Some(read) = row.validation_read() {
            check_fk_child_one_at(
                storage,
                def,
                fk,
                values,
                txid,
                ParentRead::Validation(read),
                arena,
            )?;
            continue;
        }
        let transaction = txn.as_deref_mut().ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a written row's foreign key check has no transaction"
            )
        })?;
        // PostgreSQL's check query keeps the snapshot it started with while
        // it waits for the parent row's lock; the statement's next attempt
        // runs the waiting check again under that snapshot.
        let identity = transaction.constraint_identity(constraint_table as u32, fk.name);
        let current = transaction
            .take_awaited_parent_read(identity)
            .unwrap_or_else(|| storage.validation_read());
        let read = ParentRead::Write {
            isolation: transaction.isolation,
            current,
        };
        if let Err(error) = check_fk_child_one_at(storage, def, fk, values, txid, read, arena) {
            if error.sqlstate == sqlstate::INTERNAL_LOCK_WAIT {
                transaction.await_parent_read(identity, current);
            }
            return Err(error);
        }
    }
    Ok(())
}

/// How a referencing row's check reads its parent row.
#[derive(Clone, Copy)]
enum ParentRead {
    /// Validation while a relation lock keeps writers out: the latest
    /// committed rows, without row locks.
    Validation(ValidationRead),
    /// A written row. Like PostgreSQL's referential check query, it locks the
    /// parent `FOR KEY SHARE`, so a concurrent delete or key change and the
    /// referencing transaction wait for each other. Read committed reads the
    /// latest committed parent; repeatable read and serializable read their
    /// snapshot and fail with `40001` when the parent was deleted or its key
    /// changed after it. `current` is the latest committed state the check
    /// reads under read committed and for a partitioned parent.
    Write {
        isolation: crate::sql::ast::TransactionIsolation,
        current: ValidationRead,
    },
}

fn check_fk_child_one_at(
    storage: &Storage,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    fk: &crate::storage::ForeignKey,
    values: &[Datum],
    txid: u32,
    read: ParentRead,
    arena: &Arena,
) -> Result<(), SqlError> {
    let Some(parent_index) =
        storage.find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
    else {
        return Err(missing_parent_key(def, fk, values));
    };
    let parent_definition = storage.table_def(parent_index, txid).clone();
    let mut key = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    let mut key_types = [ColType::Bool; crate::storage::MAX_INDEX_COLS];
    for (position, (&child_column, &parent_column)) in
        fk.columns().iter().zip(fk.parent_cols()).enumerate()
    {
        let referencing = def.columns()[usize::from(child_column)].ctype;
        let referenced = parent_definition.columns()[usize::from(parent_column)].ctype;
        let equality = super::foreign_key_equality::stored_key_equality(referencing, referenced)?;
        key[position] = super::foreign_key_equality::comparison_value(
            equality,
            values[usize::from(child_column)],
            arena,
        )?;
        key_types[position] = match equality {
            super::foreign_key_equality::KeyEquality::Direct => referencing,
            super::foreign_key_equality::KeyEquality::Cast(target) => target,
        };
    }
    let n_key = fk.parent_cols().len();
    match parent_has_key(
        storage,
        parent_index,
        fk.parent_cols(),
        &key[..n_key],
        &key_types[..n_key],
        txid,
        read,
        arena,
    ) {
        Ok(true) => Ok(()),
        Ok(false) => Err(missing_parent_key(def, fk, values)),
        Err(error) if error.sqlstate.internal_control().is_some() => Err(error),
        // An error inside the lookup names PostgreSQL's check query.
        Err(error) => {
            let query = check_query(storage, txid, def, fk, &parent_definition, arena)?;
            let frame = crate::sql::error_context::QueryFrame::new(
                query,
                crate::sql::error_context::QueryMode::Statement,
                false,
            );
            crate::sql::error_context::within(&frame, || Err(error))
        }
    }
}

/// PostgreSQL's `RI_FKey_check` query for `fk` (`ri_triggers.c`).
fn check_query<'a>(
    storage: &Storage,
    txid: u32,
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    fk: &crate::storage::ForeignKey,
    parent: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    arena: &'a Arena,
) -> Result<&'a str, SqlError> {
    use core::fmt::Write;
    let mut text = crate::util::StackStr::<REFERENTIAL_QUERY_BYTES>::new();
    let declared_type = |column: &crate::storage::ColumnMeta| {
        storage
            .routine_type_oid(column.ctype, column.user_type, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "foreign key column type has no OID"
                )
            })
    };
    let only = if parent.partition.is_partitioned() {
        ""
    } else {
        "ONLY "
    };
    let _ = write!(text, "SELECT 1 FROM {only}");
    let _ = write_quoted(&mut text, parent.schema.as_str());
    let _ = text.write_char('.');
    let _ = write_quoted(&mut text, parent.name.as_str());
    let _ = text.write_str(" x");
    for (key, (&child_column, &parent_column)) in
        fk.columns().iter().zip(fk.parent_cols()).enumerate()
    {
        let child_column = &def.columns()[usize::from(child_column)];
        let parent_column = &parent.columns()[usize::from(parent_column)];
        let (left, right) = super::foreign_key_equality::operator_inputs(
            super::foreign_key_equality::key_operators(child_column.ctype, parent_column.ctype)[0],
        )
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "foreign key equality operator is not catalogued"
            )
        })?;
        let _ = text.write_str(if key == 0 { " WHERE " } else { " AND " });
        let _ = write_quoted(&mut text, parent_column.name.as_str());
        let _ = write_cast(&mut text, declared_type(parent_column)?, left, false);
        let _ = write!(text, " OPERATOR(pg_catalog.=) ${}", key + 1);
        let _ = write_cast(&mut text, declared_type(child_column)?, right, false);
    }
    let _ = text.write_str(" FOR KEY SHARE OF x");
    if text.is_truncated() {
        return Err(referential_query_full());
    }
    arena
        .alloc_str(text.as_str())
        .map_err(|_| crate::sql::eval::arena_full())
}

/// PostgreSQL's `quoteOneName`: always quoted, embedded quotes doubled.
fn write_quoted(out: &mut dyn core::fmt::Write, name: &str) -> core::fmt::Result {
    out.write_char('"')?;
    for character in name.chars() {
        if character == '"' {
            out.write_char('"')?;
        }
        out.write_char(character)?;
    }
    out.write_char('"')
}

/// 23503 for a referencing row whose key `values` has no parent row.
fn missing_parent_key(
    def: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    fk: &crate::storage::ForeignKey,
    values: &[Datum],
) -> SqlError {
    let detail = key_detail(
        &mut fk
            .columns()
            .iter()
            .map(|column| def.columns()[*column as usize].name.as_str()),
        &mut fk.columns().iter().map(|column| values[*column as usize]),
        format_args!(" is not present in table \"{}\".", fk.parent.as_str()),
    );
    let detail = match detail {
        Ok(detail) => detail,
        Err(error) => return error,
    };
    crate::sql::eval::stash_object_diagnostic(
        Some(detail.as_str()),
        crate::sql::eval::ErrorObjects::table_constraint(
            def.schema.as_str(),
            def.name.as_str(),
            fk.name.as_str(),
        ),
    );
    sql_err!(
        sqlstate::FOREIGN_KEY_VIOLATION,
        "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
        def.name.as_str(),
        fk.name.as_str()
    )
}

/// The name of a foreign key's RI trigger, which orders its queued check
/// among the row's AFTER triggers.
fn foreign_key_trigger_name(
    foreign_key: &crate::storage::ForeignKey,
    trigger: crate::storage::ForeignKeyTrigger,
) -> Result<SqlName, SqlError> {
    let side = match trigger {
        crate::storage::ForeignKeyTrigger::ActionDelete
        | crate::storage::ForeignKeyTrigger::ActionUpdate => 'a',
        crate::storage::ForeignKeyTrigger::CheckInsert
        | crate::storage::ForeignKeyTrigger::CheckUpdate => 'c',
    };
    SqlName::parse(
        crate::stack_format!(
            64,
            "RI_ConstraintTrigger_{}_{}",
            side,
            foreign_key.trigger_oid(trigger)
        )
        .as_str(),
    )
}

/// When a queued foreign key check fires: `None` once its constraint is gone.
/// A `RESTRICT` check and the rewriting actions are never deferred, as
/// PostgreSQL creates their triggers not deferrable.
pub(crate) fn foreign_key_event_mode(
    storage: &Storage,
    txn: &TxnState,
    identity: crate::sql::txn::ConstraintIdentity,
    check: crate::sql::txn::ForeignKeyCheck,
) -> Option<crate::sql::ast::ConstraintMode> {
    if !txn.constraint_identity_is_current(identity) {
        return None;
    }
    let timing = constraint_timing(storage, txn.current_constraint_identity(identity), txn.txid)?;
    if !check.deferrable() {
        return Some(crate::sql::ast::ConstraintMode::Immediate);
    }
    Some(txn.constraint_mode(identity, timing))
}

/// Runs the queued foreign key event `index`: a referential action or a
/// check (see [`fire_foreign_key_event`]).
pub(super) fn fire_queued_foreign_key_event(
    context: &mut super::TriggerExecContext<'_, '_, '_>,
    index: u64,
) -> Result<(), SqlError> {
    let (event, _, _) = context.txn.trigger_event(index)?;
    match event.kind {
        crate::sql::txn::DeferredTriggerKind::ForeignKey {
            check: crate::sql::txn::ForeignKeyCheck::Action(_),
            ..
        } => fire_foreign_key_action(context, index),
        _ => fire_foreign_key_event(context.host.storage(), context.txn, index, context.arena),
    }
}

/// Runs the queued foreign key check `index` and marks it fired. A check
/// that waits for a row lock runs again under the snapshot it started with.
fn fire_foreign_key_event(
    storage: &Storage,
    txn: &mut TxnState,
    index: u64,
    arena: &Arena,
) -> Result<(), SqlError> {
    let (event, _, fired) = txn.trigger_event(index)?;
    if fired {
        return Ok(());
    }
    let crate::sql::txn::DeferredTriggerKind::ForeignKey {
        identity,
        check,
        effective_table,
        rowid,
        ..
    } = event.kind
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "queued event is not a foreign key check"
        ));
    };
    if !txn.constraint_identity_is_current(identity) {
        return txn.fire_trigger_event(index);
    }
    let current = txn.current_constraint_identity(identity);
    let read = txn
        .take_awaited_parent_read(current)
        .unwrap_or_else(|| storage.validation_read());
    // A data-modifying `WITH` reads below its own command; its checks do not.
    let _own_changes = storage.own_changes_snapshot();
    let result = match check {
        crate::sql::txn::ForeignKeyCheck::Referencing => validate_constraint_obligation(
            storage,
            current,
            rowid,
            txn.txid,
            txn.isolation,
            read,
            arena,
        )
        .map(|_| ()),
        crate::sql::txn::ForeignKeyCheck::Action(_) => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a referential action was run as a check"
        )),
        crate::sql::txn::ForeignKeyCheck::Referenced { restrict } => {
            let old = event
                .old
                .map(|tuple| txn.trigger_event_tuple(tuple, arena))
                .transpose()?
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "a referenced-key check lost its old key"
                    )
                })?;
            check_referenced_key(
                storage,
                txn,
                current,
                restrict,
                usize::from(effective_table),
                old,
                read,
                arena,
            )
        }
    };
    if let Err(error) = result {
        if error.sqlstate == sqlstate::INTERNAL_LOCK_WAIT {
            txn.await_parent_read(current, read);
        }
        return Err(error);
    }
    txn.fire_trigger_event(index)
}

/// Validates the current image of the row that created a deferred obligation.
/// A deleted row has discharged its obligation. This row scope is essential
/// for NOT VALID constraints, whose pre-existing rows are not retroactively
/// subject to ordinary DML enforcement.
pub(crate) fn validate_constraint_obligation(
    storage: &Storage,
    constraint: crate::sql::txn::ConstraintIdentity,
    rowid: u64,
    txid: u32,
    isolation: crate::sql::ast::TransactionIsolation,
    current: ValidationRead,
    arena: &Arena,
) -> Result<Option<crate::storage::ConstraintTiming>, SqlError> {
    let crate::sql::txn::ConstraintIdentity::Table { table, name, .. } = constraint else {
        return Ok(None);
    };
    let table_index = table as usize;
    if table_index >= storage.table_count() || !storage.table(table_index).visible_to(txid) {
        return Ok(None);
    }
    let definition = storage.table_def(table_index, txid).clone();
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    definition.schema(&mut schema);
    let schema = &schema[..definition.n_columns];
    let key = definition
        .uniques()
        .iter()
        .find(|key| key.name == name)
        .copied();
    let foreign_key = definition
        .fkeys()
        .iter()
        .find(|foreign_key| foreign_key.name == name)
        .copied();
    let exclusion = definition
        .exclusions()
        .iter()
        .find(|exclusion| exclusion.name == name)
        .copied();
    let timing = key
        .map(|key| key.timing)
        .or_else(|| foreign_key.map(|foreign_key| foreign_key.timing))
        .or_else(|| exclusion.map(|exclusion| exclusion.timing));
    let Some(timing) = timing else {
        return Ok(None);
    };
    let predicate = match exclusion.and_then(|constraint| constraint.predicate) {
        Some(source) => {
            let source = arena
                .alloc_str(source.as_str())
                .map_err(|_| crate::sql::eval::arena_full())?;
            Some(crate::sql::parser::parse_expr(source, arena)?)
        }
        None => None,
    };
    constraint_row_tables(storage, table_index, txid, |row_table| {
        let Some(state) = storage.row_state(row_table, rowid)? else {
            return Ok(());
        };
        let Some(home) = storage.visible_row_home(row_table, rowid, state, txid)? else {
            return Ok(());
        };
        let decoder = crate::storage::RelationRowDecoder::new(
            &definition,
            &storage.table_def(row_table, txid),
        )?;
        storage.with_row_bytes(row_table, rowid, home, |bytes| {
            let mut values = [Datum::Null; MAX_COLUMNS];
            decoder.decode(bytes, &mut values)?;
            let values = &values[..definition.n_columns];
            if let Some(key) = key {
                enforce_key_uniqueness(
                    storage,
                    row_table,
                    &definition,
                    schema,
                    values,
                    Some(rowid),
                    txid,
                    key.columns(),
                    key.kind.nulls_not_distinct(),
                    key.name,
                    DuplicateKey::Write,
                    UniqueKeySource::Cache,
                )?;
            } else if let Some(foreign_key) = foreign_key {
                if foreign_key.validation.enforced()
                    && !foreign_key
                        .columns()
                        .iter()
                        .any(|&column| values[column as usize].is_null())
                {
                    check_fk_child_one_at(
                        storage,
                        &definition,
                        &foreign_key,
                        values,
                        txid,
                        ParentRead::Write { isolation, current },
                        arena,
                    )?;
                }
            } else if let Some(exclusion) = exclusion {
                enforce_exclusion_constraint(
                    storage,
                    row_table,
                    &definition,
                    schema,
                    values,
                    Some(rowid),
                    txid,
                    &exclusion,
                    predicate,
                    arena,
                    None,
                )?;
            }
            Ok(())
        })
    })?;
    Ok(Some(timing))
}

fn constraint_row_tables(
    storage: &Storage,
    table_index: usize,
    txid: u32,
    mut visit: impl FnMut(usize) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let definition = storage.table_def(table_index, txid);
    if !definition.partition.is_partitioned() {
        return visit(table_index);
    }
    for candidate in 0..storage.table_count() {
        if storage.table(candidate).visible_to(txid)
            && !storage
                .table_def(candidate, txid)
                .partition
                .is_partitioned()
            && storage.partition_descends_from(candidate, table_index, txid)
        {
            visit(candidate)?;
        }
    }
    Ok(())
}

pub(crate) fn validate_foreign_key_constraint(
    storage: &Storage,
    table_index: usize,
    definition: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    foreign_key: &crate::storage::ForeignKey,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let read = storage.validation_read();
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    definition.schema(&mut schema);
    constraint_row_tables(storage, table_index, txid, |row_table| {
        // SAFETY: the callback returns only an owned validation result.
        unsafe {
            for_each_validation_row(
                storage,
                row_table,
                txid,
                read,
                &schema[..definition.n_columns],
                arena,
                &mut |_, values| {
                    if foreign_key
                        .columns()
                        .iter()
                        .any(|&column| values[column as usize].is_null())
                    {
                        return Ok(());
                    }
                    check_fk_child_one_at(
                        storage,
                        definition,
                        foreign_key,
                        values,
                        txid,
                        ParentRead::Validation(read),
                        arena,
                    )
                },
            )
        }
    })
}

pub(crate) fn validate_check_constraint(
    storage: &Storage,
    table_index: usize,
    definition: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    check: &crate::storage::CheckConstraint,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let read = storage.validation_read();
    let expression = crate::sql::parser::parse_expr(check.expression.as_str(), arena)?;
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    definition.schema(&mut schema);
    constraint_row_tables(storage, table_index, txid, |row_table| {
        // SAFETY: the expression and hooks precede the scan. Its callback
        // consumes every evaluated value before returning an owned result.
        unsafe {
            for_each_validation_row(
                storage,
                row_table,
                txid,
                read,
                &schema[..definition.n_columns],
                arena,
                &mut |_, values| {
                    let context = RowCtx {
                        def: crate::storage::RelationDefinition::from_definition(definition),
                        values,
                        alias: None,
                    };
                    if matches!(
                        eval_full(expression, arena, &[], &context, &hooks)?,
                        Datum::Bool(false)
                    ) {
                        return Err(sql_err!(
                            sqlstate::CHECK_VIOLATION,
                            "check constraint \"{}\" of relation \"{}\" is violated by some row",
                            check.name.as_str(),
                            definition.name.as_str()
                        ));
                    }
                    Ok(())
                },
            )
        }
    })
}

/// Checks and removes obligations whose current mode requires this boundary.
/// `immediate_only` is the statement boundary; `false` is transaction commit.
pub(crate) fn validate_deferred_constraints(
    storage: &Storage,
    txn: &mut TxnState,
    immediate_only: bool,
    arena: &Arena,
) -> Result<(), SqlError> {
    let mut index = 0;
    while index < txn.deferred_constraints().len() {
        if txn.deferred_constraint_is_complete(index) {
            index += 1;
            continue;
        }
        let obligation = txn.deferred_constraints()[index];
        let current = txn.current_constraint_identity(obligation.constraint);
        if !txn.constraint_identity_is_current(obligation.constraint) {
            txn.complete_deferred_constraint(index)?;
            index += 1;
            continue;
        }
        let Some(timing) = constraint_timing(storage, current, txn.txid) else {
            txn.complete_deferred_constraint(index)?;
            index += 1;
            continue;
        };
        if immediate_only
            && txn.constraint_mode(obligation.constraint, timing)
                == crate::sql::ast::ConstraintMode::Deferred
        {
            index += 1;
            continue;
        }
        // A check that waited for its parent row's lock runs again under
        // the snapshot it started with, as in-row checks do.
        let read = txn
            .take_awaited_parent_read(current)
            .unwrap_or_else(|| storage.validation_read());
        if let Err(error) = validate_constraint_obligation(
            storage,
            current,
            obligation.rowid,
            txn.txid,
            txn.isolation,
            read,
            arena,
        ) {
            if error.sqlstate == sqlstate::INTERNAL_LOCK_WAIT {
                txn.await_parent_read(current, read);
            }
            return Err(error);
        }
        txn.complete_deferred_constraint(index)?;
        index += 1;
    }
    Ok(())
}

pub(crate) fn constraint_timing(
    storage: &Storage,
    constraint: crate::sql::txn::ConstraintIdentity,
    txid: u32,
) -> Option<crate::storage::ConstraintTiming> {
    let crate::sql::txn::ConstraintIdentity::Table { table, name, .. } = constraint else {
        let crate::sql::txn::ConstraintIdentity::Trigger { slot, .. } = constraint else {
            unreachable!()
        };
        let trigger = storage.trigger(usize::from(slot));
        return trigger.visible_to(txid).then(|| trigger.kind.timing());
    };
    let table_index = table as usize;
    if table_index >= storage.table_count() || !storage.table(table_index).visible_to(txid) {
        return None;
    }
    let definition = storage.table_def(table_index, txid);
    definition
        .uniques()
        .iter()
        .find(|key| key.name == name)
        .map(|key| key.timing)
        .or_else(|| {
            definition
                .fkeys()
                .iter()
                .find(|foreign_key| foreign_key.name == name)
                .map(|foreign_key| foreign_key.timing)
        })
        .or_else(|| {
            definition
                .exclusions()
                .iter()
                .find(|exclusion| exclusion.name == name)
                .map(|exclusion| exclusion.timing)
        })
}

fn named_constraint_timing(
    definition: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    name: &str,
) -> Option<crate::storage::ConstraintTiming> {
    if let Some(key) = definition
        .uniques()
        .iter()
        .find(|key| key.name.as_str() == name)
    {
        return Some(key.timing);
    }
    if let Some(foreign_key) = definition
        .fkeys()
        .iter()
        .find(|foreign_key| foreign_key.name.as_str() == name)
    {
        return Some(foreign_key.timing);
    }
    if let Some(exclusion) = definition
        .exclusions()
        .iter()
        .find(|exclusion| exclusion.name.as_str() == name)
    {
        return Some(exclusion.timing);
    }
    if definition
        .checks()
        .iter()
        .any(|check| check.name.as_str() == name)
    {
        return Some(crate::storage::ConstraintTiming::NotDeferrable);
    }
    for (column, metadata) in definition.columns().iter().enumerate() {
        if (metadata.primary || metadata.unique)
            && definition
                .column_key_name(column)
                .is_some_and(|stored| stored.as_str() == name)
        {
            return Some(crate::storage::ConstraintTiming::NotDeferrable);
        }
    }
    None
}

/// Resolves a `SET CONSTRAINTS` name with PostgreSQL's first-schema search
/// rule while retaining every same-schema match.
pub(crate) fn resolve_constraint_name(
    storage: &Storage,
    written: &crate::sql::ast::QualName<'_>,
    mode: crate::sql::ast::ConstraintMode,
    txid: u32,
    output: &mut [crate::sql::txn::ConstraintIdentity],
) -> Result<usize, SqlError> {
    let mut collect_schema = |schema: &str| -> Result<(bool, usize), SqlError> {
        let mut found = false;
        let mut count = 0;
        for table in 0..storage.table_count() {
            if !storage.table(table).visible_to(txid) {
                continue;
            }
            let definition = storage.table_def(table, txid);
            if definition.schema.as_str() != schema {
                continue;
            }
            if let Some(timing) = named_constraint_timing(&definition, written.name) {
                found = true;
                if !timing.is_deferrable() && mode == crate::sql::ast::ConstraintMode::Deferred {
                    return Err(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "constraint \"{}\" is not deferrable",
                        written.name
                    ));
                }
                if timing.is_deferrable() {
                    if count == output.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "SET CONSTRAINTS matches more than {} constraints",
                            output.len()
                        ));
                    }
                    output[count] = crate::sql::txn::ConstraintIdentity::Table {
                        table: table as u32,
                        name: crate::storage::SqlName::parse(written.name)?,
                        generation: 0,
                    };
                    count += 1;
                }
            }
            for (slot, trigger) in storage.triggers_for_table(table, txid) {
                if trigger.name_to(txid).as_str() != written.name {
                    continue;
                }
                let crate::storage::TriggerKind::Constraint { timing, .. } = trigger.kind else {
                    continue;
                };
                found = true;
                if !timing.is_deferrable() && mode == crate::sql::ast::ConstraintMode::Deferred {
                    return Err(sql_err!(
                        sqlstate::WRONG_OBJECT_TYPE,
                        "constraint \"{}\" is not deferrable",
                        written.name
                    ));
                }
                if !timing.is_deferrable() {
                    continue;
                }
                if count == output.len() {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "SET CONSTRAINTS matches more than {} constraints",
                        output.len()
                    ));
                }
                output[count] = crate::sql::txn::ConstraintIdentity::Trigger {
                    table: table as u32,
                    slot: slot as u16,
                };
                count += 1;
            }
        }
        Ok((found, count))
    };

    if let Some(schema) = written.schema {
        let (found, count) = collect_schema(schema)?;
        if found {
            return Ok(count);
        }
    } else {
        for entry in storage.path().entries() {
            let crate::storage::PathEntry::Schema(schema_slot) = entry else {
                continue;
            };
            let schema = storage.schema_def(*schema_slot as usize).name;
            let (found, count) = collect_schema(schema.as_str())?;
            if found {
                return Ok(count);
            }
        }
    }
    Err(sql_err!(
        sqlstate::UNDEFINED_OBJECT,
        "constraint \"{}\" does not exist",
        written.name
    ))
}

#[cfg(test)]
thread_local! {
    /// Parent rows the worker's foreign key checks examined.
    pub(crate) static PARENT_ROWS_VISITED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Most rows one parent key hash names before a foreign key check scans the
/// parent instead: the hash of a unique key rarely collides.
const PARENT_KEY_CANDIDATES: usize = 16;

/// Visits the parent rows that can hold the child's key: the unique key
/// cache's candidates for its hash and the rows with pending changes.
/// Returns `false`, visiting nothing, when the cache cannot answer (no
/// complete cache for exactly these columns, a column type that hashes
/// differently in the child, or more candidates than
/// [`PARENT_KEY_CANDIDATES`]); the caller then scans the parent.
#[expect(
    clippy::too_many_arguments,
    reason = "a parent probe maps the child's key onto the parent's physical columns"
)]
fn parent_key_candidates(
    storage: &Storage,
    row_table: usize,
    parent_definition: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    parent_cols: &[u16],
    mapped: &[u16],
    key: &[Datum],
    key_types: &[ColType],
    txid: u32,
    visit: &mut crate::storage::RowReadVisitor<'_>,
) -> Result<bool, SqlError> {
    let mut values = [Datum::Null; MAX_COLUMNS];
    let mut collations = [crate::sql::ast::Collation::None; crate::storage::MAX_INDEX_COLS];
    if parent_cols.len() > collations.len() {
        return Ok(false);
    }
    for (position, &parent_column) in parent_cols.iter().enumerate() {
        let value = key[position];
        let column = &parent_definition.columns()[usize::from(parent_column)];
        if value.is_null() {
            return Ok(false);
        }
        // The cache hashes the referenced column's own values; a key of
        // another type probes with the equal value of that type.
        let probe = if key_types[position] == column.ctype
            || matches!(
                (key_types[position], column.ctype),
                (ColType::Text, ColType::Varchar)
            ) {
            Some(value)
        } else {
            match super::foreign_key_equality::referenced_probe_value(value, column.ctype) {
                Some(probe) => probe,
                None => return Ok(false),
            }
        };
        let Some(probe) = probe else {
            // No referenced value equals this key.
            return Ok(true);
        };
        values[usize::from(parent_column)] = probe;
        collations[position] = column.collation;
    }
    let hash = crate::sql::eval::hash_key_collated_catalog(
        storage,
        txid,
        &values,
        parent_cols,
        &collations[..parent_cols.len()],
    )?;
    let mut candidates = [0u64; PARENT_KEY_CANDIDATES];
    let mut count = 0usize;
    let mut overflow = false;
    // Durable candidates borrow an index block, so rows are read after the
    // probe returns.
    let served = storage.probe_value(row_table, mapped, hash, |rowid, _| {
        if count < candidates.len() {
            candidates[count] = rowid;
            count += 1;
        } else {
            overflow = true;
        }
        Ok(())
    })?;
    if !served || overflow {
        return Ok(false);
    }
    for &rowid in &candidates[..count] {
        if let Some(state) = storage.row_state(row_table, rowid)?
            && visit(rowid, state)?.is_break()
        {
            return Ok(true);
        }
    }
    // The cache holds committed keys; the transaction's own changes and
    // other in-progress ones are visited as changed rows.
    storage.for_each_pending_row_change(row_table, &mut |rowid, _, _| match storage
        .row_state(row_table, rowid)?
    {
        Some(state) => visit(rowid, state),
        None => Ok(core::ops::ControlFlow::Continue(())),
    })?;
    Ok(true)
}

/// Whether a parent row has, in `parent_cols`, a tuple equal to `key`: the
/// comparison values of a referencing row, of `key_types`. A write locks that
/// parent row (see [`ParentRead`]).
#[allow(clippy::too_many_arguments)]
fn parent_has_key(
    storage: &Storage,
    parent_index: usize,
    parent_cols: &[u16],
    key: &[Datum],
    key_types: &[ColType],
    txid: u32,
    read: ParentRead,
    arena: &Arena,
) -> Result<bool, SqlError> {
    use super::row_conflicts::{LockedVersion, concurrent_change, uses_transaction_snapshot};
    let parent_definition = storage.table_def(parent_index, txid).clone();
    // Read committed follows the latest committed parent, as PostgreSQL's
    // check query takes a new snapshot. The other levels keep theirs, except
    // for a partitioned parent: PostgreSQL's `RI_FKey_check` reads it with
    // a current snapshot (`detectNewRows`), so a parent deleted after the
    // transaction's snapshot is missing (`23503`) and a newer one counts.
    let partitioned_parent = parent_definition.partition.is_partitioned();
    let latest = match read {
        ParentRead::Validation(read) => Some(read),
        ParentRead::Write { isolation, current }
            if !uses_transaction_snapshot(isolation) || partitioned_parent =>
        {
            Some(current)
        }
        ParentRead::Write { .. } => None,
    };
    let key_matches = |physical: &crate::storage::TableDefinition<_>,
                       mapped: &[u16],
                       bytes: &[u8]|
     -> Result<bool, SqlError> {
        let mut schema = [ColType::Bool; MAX_COLUMNS];
        physical.schema(&mut schema);
        let mut prow = [Datum::Null; MAX_COLUMNS];
        rowenc::decode(bytes, &schema[..physical.n_columns], &mut prow)?;
        let mark = arena.mark();
        let mut equal = Ok(true);
        for ((&parent_column, &physical_column), child) in parent_cols.iter().zip(mapped).zip(key) {
            let parent = &prow[physical_column as usize];
            equal = if parent.is_null() {
                Ok(false)
            } else {
                key_value_equal(
                    storage,
                    txid,
                    parent_definition.columns()[parent_column as usize].collation,
                    child,
                    parent,
                    arena,
                )
            };
            if !matches!(equal, Ok(true)) {
                break;
            }
        }
        // SAFETY: the composite fields read after the mark do not escape.
        unsafe { arena.rewind_to(mark) };
        equal
    };
    let mut found = None;
    constraint_row_tables(storage, parent_index, txid, |row_table| {
        if found.is_some() {
            return Ok(());
        }
        let physical = storage.table_def(row_table, txid).clone();
        let mapped = super::remap_columns(&physical, &parent_definition, parent_cols)?;
        let mut visit = |rowid: u64, state: crate::storage::RowRead<'_>| {
            use core::ops::ControlFlow;
            #[cfg(test)]
            PARENT_ROWS_VISITED.with(|visited| visited.set(visited.get() + 1));
            let moved_out = state.committed.is_none() && state.committed_moved_out;
            let home = match latest {
                Some(read) => read.visible_row_home(storage, row_table, rowid, state, txid)?,
                None => storage.visible_row_home_at(
                    row_table,
                    rowid,
                    state,
                    txid,
                    crate::storage::SNAPSHOT_ALL,
                    storage.commit_snapshot(),
                )?,
            };
            let Some(home) = home else {
                // A matching parent the statement saw that another
                // transaction then moved to a different partition cannot be
                // followed: PostgreSQL's check query, which locked the row
                // under an older snapshot, fails with `40001`.
                if moved_out
                    && matches!(read, ParentRead::Write { isolation, .. } if !uses_transaction_snapshot(isolation))
                    && let Some(state) = storage.row_state(row_table, rowid)?
                    && let Some(selected) =
                        storage.visible_row_home(row_table, rowid, state, txid)?
                    && storage.with_row_bytes(row_table, rowid, selected, |bytes| {
                        key_matches(&physical, &mapped, bytes)
                    })?
                {
                    return Err(super::row_conflicts::moved_out(false));
                }
                return Ok(ControlFlow::Continue(()));
            };
            let seen = home.snapshot();
            if storage.with_row_bytes(row_table, rowid, home, |bytes| {
                key_matches(&physical, &mapped, bytes)
            })? {
                found = Some((row_table, rowid, seen));
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        };
        // A current read finds the key through the parent's unique key cache
        // and its pending changes instead of scanning every parent row.
        if latest.is_some()
            && !partitioned_parent
            && parent_key_candidates(
                storage,
                row_table,
                &parent_definition,
                parent_cols,
                &mapped[..parent_cols.len()],
                key,
                key_types,
                txid,
                &mut visit,
            )?
        {
            return Ok(());
        }
        match latest {
            Some(read) => read.for_each_row_state(storage, row_table, &mut visit)?,
            None => storage.for_each_row_state(row_table, &mut visit)?,
        }
        Ok(())
    })?;
    let Some((row_table, rowid, seen)) = found else {
        return Ok(false);
    };
    let ParentRead::Write { isolation, .. } = read else {
        return Ok(true);
    };
    let locked = super::row_conflicts::lock_selected_row(
        storage,
        txid,
        row_table,
        rowid,
        seen,
        crate::sql::ast::LockStrength::KeyShare,
        crate::sql::ast::LockWait::Wait,
    )?
    .ok_or_else(|| {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a foreign key check skipped its locked parent row"
        )
    })?;
    let physical = storage.table_def(row_table, txid).clone();
    match locked {
        LockedVersion::Selected => Ok(true),
        LockedVersion::Deleted if uses_transaction_snapshot(isolation) => {
            Err(concurrent_change(false))
        }
        LockedVersion::Deleted => Ok(false),
        LockedVersion::Updated(home) if uses_transaction_snapshot(isolation) => {
            // A key share lock survives a committed change that kept every
            // unique key column, as PostgreSQL's tuple lock does.
            if super::row_conflicts::key_unchanged(storage, txid, row_table, rowid, seen, home)? {
                Ok(true)
            } else {
                Err(concurrent_change(false))
            }
        }
        LockedVersion::Updated(home) => {
            let mapped = super::remap_columns(&physical, &parent_definition, parent_cols)?;
            let latest = storage.row_bytes(row_table, rowid, home, arena)?;
            key_matches(&physical, &mapped, latest)
        }
    }
}

/// After a parent row is deleted (`new_parent` None) or its referenced key
/// updated (Some), queues every referencing foreign key's action among the
/// row's AFTER events, as PostgreSQL's `RI_ConstraintTrigger_a_*` triggers:
/// NO ACTION and RESTRICT check that the old key is no longer referenced;
/// CASCADE, SET NULL and SET DEFAULT delete or rewrite the referencing rows
/// (see [`fire_foreign_key_action`]). `old_parent` and `new_parent` are rows
/// of the parent table `parent_schema.parent_name`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_fk_parent_actions(
    storage: &Storage,
    txn: &mut TxnState,
    parent_schema: &str,
    parent_name: &str,
    old_parent: &[Datum],
    new_parent: Option<&[Datum]>,
    arena: &Arena,
) -> Result<(), SqlError> {
    let parent_index = storage
        .find_visible(parent_schema, parent_name, txn.txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "foreign key parent is not visible"
            )
        })?;
    let physical_parent = storage.table_def(parent_index, txn.txid).clone();
    for child_index in 0..storage.table_count() {
        if !storage.table(child_index).visible_to(txn.txid) {
            continue;
        }
        let (_, cdef) = storage.select_table_definition(child_index, txn.txid);
        for fk_index in 0..cdef.n_fkeys {
            let fk = cdef.fkeys[fk_index];
            if !fk.validation.enforced() {
                continue;
            }
            if !references_parent(storage, &fk, parent_schema, parent_name, txn.txid) {
                continue;
            }
            let declared_parent = storage
                .find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txn.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "foreign key parent is not visible"
                    )
                })?;
            let declared_parent = storage.table_def(declared_parent, txn.txid).clone();
            let mapped =
                super::remap_columns(&physical_parent, &declared_parent, fk.parent_cols())?;
            let parent_columns = &mapped[..fk.parent_cols().len()];
            // An update triggers this key's action only when the key changed.
            if let Some(new_parent) = new_parent {
                let mut changed = false;
                for (&child_column, &parent_column) in fk.columns().iter().zip(parent_columns) {
                    let old = &old_parent[parent_column as usize];
                    let new = &new_parent[parent_column as usize];
                    if !compare_datums_collated(
                        storage,
                        txn.txid,
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
            use crate::storage::FkAction as StorageFkAction;
            let (trigger, event) = if new_parent.is_none() {
                (
                    crate::storage::ForeignKeyTrigger::ActionDelete,
                    crate::sql::ast::TriggerEvents::DELETE,
                )
            } else {
                (
                    crate::storage::ForeignKeyTrigger::ActionUpdate,
                    crate::sql::ast::TriggerEvents::UPDATE,
                )
            };
            let check = match action {
                StorageFkAction::NoAction => {
                    crate::sql::txn::ForeignKeyCheck::Referenced { restrict: false }
                }
                StorageFkAction::Restrict => {
                    crate::sql::txn::ForeignKeyCheck::Referenced { restrict: true }
                }
                StorageFkAction::Cascade => crate::sql::txn::ForeignKeyCheck::Action(
                    crate::sql::txn::ReferentialAction::Cascade,
                ),
                StorageFkAction::SetNull => crate::sql::txn::ForeignKeyCheck::Action(
                    crate::sql::txn::ReferentialAction::SetNull,
                ),
                StorageFkAction::SetDefault => crate::sql::txn::ForeignKeyCheck::Action(
                    crate::sql::txn::ReferentialAction::SetDefault,
                ),
            };
            // A check keeps the old key alone; a cascaded update also the new
            // key (see [`decode_action_keys`]).
            let encoded = match check {
                crate::sql::txn::ForeignKeyCheck::Action(_) => encode_action_keys(
                    old_parent,
                    new_parent,
                    parent_columns,
                    physical_parent.n_columns,
                    arena,
                )?,
                _ => encode_referenced_key(
                    old_parent,
                    parent_columns,
                    physical_parent.n_columns,
                    arena,
                )?,
            };
            let identity = txn.constraint_identity(child_index as u32, fk.name);
            txn.pend_foreign_key_event(
                parent_index as u16,
                foreign_key_trigger_name(&fk, trigger)?,
                identity,
                check,
                parent_index as u16,
                event,
                0,
                Some(encoded),
            )?;
        }
    }
    Ok(())
}

/// Runs the queued referential action `index` (CASCADE, SET NULL or SET
/// DEFAULT) at the end of the statement that deleted or re-keyed the parent
/// row, as PostgreSQL's action trigger does: it runs the query
/// `ri_triggers.c` builds (`DELETE FROM ONLY child WHERE ...` or `UPDATE
/// ONLY child SET ...`) as the child table's owner. Rules rewrite it, the
/// child's BEFORE triggers fire, and its AFTER events join the queue of the
/// statement running this event. An error inside it names the query as
/// context.
fn fire_foreign_key_action(
    context: &mut super::TriggerExecContext<'_, '_, '_>,
    index: u64,
) -> Result<(), SqlError> {
    use crate::storage::FkAction as StorageFkAction;
    let arena = context.arena;
    let (event, _, fired) = context.txn.trigger_event(index)?;
    if fired {
        return Ok(());
    }
    let crate::sql::txn::DeferredTriggerKind::ForeignKey {
        identity,
        check: crate::sql::txn::ForeignKeyCheck::Action(referential),
        effective_table,
        ..
    } = event.kind
    else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "queued event is not a referential action"
        ));
    };
    if !context.txn.constraint_identity_is_current(identity) {
        return context.txn.fire_trigger_event(index);
    }
    let current = context.txn.current_constraint_identity(identity);
    let crate::sql::txn::ConstraintIdentity::Table { table, name, .. } = current else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a referential action names a constraint trigger"
        ));
    };
    let txid = context.txn.txid;
    let child_index = table as usize;
    let parent_index = usize::from(effective_table);
    let storage = context.storage();
    if child_index >= storage.table_count()
        || !storage.table(child_index).visible_to(txid)
        || parent_index >= storage.table_count()
        || !storage.table(parent_index).visible_to(txid)
    {
        return context.txn.fire_trigger_event(index);
    }
    let (_, cdef) = storage.select_table_definition(child_index, txid);
    let Some(fk) = cdef.fkeys().iter().find(|fk| fk.name == name).copied() else {
        return context.txn.fire_trigger_event(index);
    };
    if !fk.validation.enforced() {
        return context.txn.fire_trigger_event(index);
    }
    let physical_parent = storage.table_def(parent_index, txid).clone();
    let declared_parent = storage
        .find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "foreign key parent is not visible"
            )
        })?;
    let declared_parent = storage.table_def(declared_parent, txid).clone();
    let mapped = super::remap_columns(&physical_parent, &declared_parent, fk.parent_cols())?;
    let parent_columns = &mapped[..fk.parent_cols().len()];
    let keys = event
        .old
        .map(|tuple| context.txn.trigger_event_tuple(tuple, arena))
        .transpose()?
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a referential action lost its old key"
            )
        })?;
    let storage = context.storage();
    let (old_bytes, old_parent, new_parent) = decode_action_keys(keys, &physical_parent, arena)?;
    let action = match referential {
        crate::sql::txn::ReferentialAction::Cascade => StorageFkAction::Cascade,
        crate::sql::txn::ReferentialAction::SetNull => StorageFkAction::SetNull,
        crate::sql::txn::ReferentialAction::SetDefault => StorageFkAction::SetDefault,
    };
    // PostgreSQL's referential queries read the latest committed rows; under
    // repeatable read a referencing row the transaction's snapshot does not
    // see in the same version is a concurrent change (its crosscheck
    // snapshot).
    if super::row_conflicts::uses_transaction_snapshot(context.txn.isolation) {
        crosscheck_referencing_rows(
            storage,
            txid,
            child_index,
            &cdef,
            &fk,
            &physical_parent,
            parent_columns,
            old_parent,
            arena,
        )?;
    }
    let query = referential_query(
        storage,
        txid,
        action,
        &cdef,
        &fk,
        &physical_parent,
        parent_columns,
        old_parent,
        new_parent,
        arena,
    )?;
    let statement = parse_referential_query(query.executable, arena)?;
    let untagged = rewrite_sets_no_tag(storage, child_index, statement, txid, arena)?;
    let owner = crate::storage::SessionIdentity::bound(storage.capture_role_slot(
        storage.object_owner(storage.table_access_object(child_index, txid), txid),
        txid,
    )?);
    let after_events = crate::sql::txn::AfterEventOwner {
        depth: context.txn.trigger_depth() + 1,
        owner_depth: context.txn.trigger_depth(),
        owner_command: context.txn.command_id(),
        latest_snapshot: true,
    };
    let previous = context.txn.replace_after_event_owner(Some(after_events));
    let mut pending = None;
    let frame = crate::sql::error_context::QueryFrame::new(
        query.display,
        crate::sql::error_context::QueryMode::Statement,
        false,
    );
    let result = crate::sql::error_context::within(&frame, || {
        crate::sql::error_position::with_internal_source(query.executable, || {
            super::with_bound_parameter_types(query.types, || {
                let _user = crate::sql::eval::funcs::system::enter_current_user(owner);
                let _security = crate::storage::enter_no_force_row_security();
                super::execute_nested_data_modification(
                    context,
                    statement,
                    query.parameters,
                    None,
                    None,
                    &mut pending,
                    None,
                )
            })
        })
    });
    context.txn.replace_after_event_owner(previous);
    result?;
    if pending.is_some() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a referential action left its write unfinished"
        ));
    }
    if untagged {
        return Err(crate::sql::error_position::with_hint(
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "referential integrity query on \"{}\" from constraint \"{}\" on \"{}\" gave unexpected result",
                declared_parent.name.as_str(),
                fk.name.as_str(),
                cdef.name.as_str()
            ),
            "This is most likely due to a rule having rewritten the query.",
        ));
    }
    // A SET DEFAULT that left a row referencing the deleted key (the default
    // was that key) fails as NO ACTION would (`ri_set`).
    if action == StorageFkAction::SetDefault {
        let storage = context.storage();
        check_referenced_key(
            storage,
            context.txn,
            current,
            false,
            parent_index,
            old_bytes,
            storage.validation_read(),
            arena,
        )?;
    }
    context.txn.fire_trigger_event(index)
}

/// Fails with PostgreSQL's crosscheck error when a row referencing the old
/// key at the latest committed state is not the version the transaction's
/// snapshot sees.
#[expect(
    clippy::too_many_arguments,
    reason = "the scan carries the key and both relations"
)]
fn crosscheck_referencing_rows(
    storage: &Storage,
    txid: u32,
    child_index: usize,
    cdef: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    fk: &crate::storage::ForeignKey,
    parent: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    parent_columns: &[u16],
    old_parent: &[Datum],
    arena: &Arena,
) -> Result<(), SqlError> {
    let mut cschema = [ColType::Bool; MAX_COLUMNS];
    cdef.schema(&mut cschema);
    let cschema = &cschema[..cdef.n_columns];
    let latest = storage.validation_read();
    latest.for_each_row_state(storage, child_index, &mut |rowid, state| {
        use core::ops::ControlFlow;
        let Some(home) = latest.visible_row_home(storage, child_index, rowid, state, txid)? else {
            return Ok(ControlFlow::Continue(()));
        };
        let seen = home.snapshot();
        let refers = storage.with_row_bytes(child_index, rowid, home, |bytes| {
            let mut crow = [Datum::Null; MAX_COLUMNS];
            rowenc::decode(bytes, cschema, &mut crow)?;
            referencing_key_matches(
                storage,
                txid,
                cdef,
                fk,
                parent,
                parent_columns,
                old_parent,
                &crow[..cdef.n_columns],
                arena,
            )
        })?;
        if !refers {
            return Ok(ControlFlow::Continue(()));
        }
        let snapshot_version = match storage.row_state(child_index, rowid)? {
            Some(state) => storage
                .visible_row_home_at(
                    child_index,
                    rowid,
                    state,
                    txid,
                    crate::storage::SNAPSHOT_ALL,
                    storage.commit_snapshot(),
                )?
                .map(|home| home.snapshot().row_version()),
            None => None,
        };
        if snapshot_version != Some(seen.row_version()) {
            return Err(super::row_conflicts::concurrent_change(false));
        }
        Ok(ControlFlow::Continue(()))
    })
}

/// A referential action's query: the text PostgreSQL 18.6's `ri_triggers.c`
/// builds, which errors report as context; the text pos3ql runs, without
/// the casts to polymorphic operator inputs it cannot write; and the key
/// parameters with their declared types.
struct ReferentialQuery<'a> {
    display: &'a str,
    executable: &'a str,
    parameters: &'a [Datum<'a>],
    types: &'a [i32],
}

const REFERENTIAL_QUERY_BYTES: usize = 32 * 1024;

#[expect(
    clippy::too_many_arguments,
    reason = "the query carries the key, both relations and the action"
)]
fn referential_query<'a>(
    storage: &Storage,
    txid: u32,
    action: crate::storage::FkAction,
    cdef: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    fk: &crate::storage::ForeignKey,
    parent: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    parent_columns: &[u16],
    old_parent: &[Datum<'a>],
    new_parent: Option<&[Datum<'a>]>,
    arena: &'a Arena,
) -> Result<ReferentialQuery<'a>, SqlError> {
    use crate::storage::FkAction as StorageFkAction;
    use core::fmt::Write;
    let keys = fk.columns().len();
    let cascade_update = action == StorageFkAction::Cascade && new_parent.is_some();
    let count = if cascade_update { keys * 2 } else { keys };
    let parameters = arena
        .alloc_slice_with(count, |_| Datum::Null)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let types = arena
        .alloc_slice_with(count, |_| 0i32)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let mut display = crate::util::StackStr::<REFERENTIAL_QUERY_BYTES>::new();
    let mut executable = crate::util::StackStr::<REFERENTIAL_QUERY_BYTES>::new();
    let quote = write_quoted;
    // `ON DELETE SET NULL (columns)` changes only the columns it names.
    let set_columns = if new_parent.is_none() && !fk.delete_set_cols().is_empty() {
        fk.delete_set_cols()
    } else {
        fk.columns()
    };
    let only = if cdef.partition.is_partitioned() {
        ""
    } else {
        "ONLY "
    };
    let declared_type = |column: &crate::storage::ColumnMeta| {
        storage
            .routine_type_oid(column.ctype, column.user_type, txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "foreign key column type has no OID"
                )
            })
    };
    for out in [&mut display, &mut executable] {
        let out: &mut dyn Write = out;
        let mut write = || -> core::fmt::Result {
            if action == StorageFkAction::Cascade && new_parent.is_none() {
                write!(out, "DELETE FROM {only}")?;
            } else {
                write!(out, "UPDATE {only}")?;
            }
            quote(out, cdef.schema.as_str())?;
            out.write_char('.')?;
            quote(out, cdef.name.as_str())?;
            if action == StorageFkAction::Cascade && new_parent.is_none() {
                return Ok(());
            }
            out.write_str(" SET")?;
            for (key, &child_column) in set_columns.iter().enumerate() {
                out.write_str(if key == 0 { " " } else { ", " })?;
                quote(out, cdef.columns()[usize::from(child_column)].name.as_str())?;
                match action {
                    StorageFkAction::Cascade => write!(out, " = ${}", key + 1)?,
                    StorageFkAction::SetNull => out.write_str(" = NULL")?,
                    _ => out.write_str(" = DEFAULT")?,
                }
            }
            Ok(())
        };
        write().map_err(|_| referential_query_full())?;
    }
    let first_key_parameter = if cascade_update { keys } else { 0 };
    for (key, (&child_column, &parent_column)) in
        fk.columns().iter().zip(parent_columns).enumerate()
    {
        let child_column = &cdef.columns()[usize::from(child_column)];
        let parent_meta = &parent.columns()[usize::from(parent_column)];
        let pk_type = declared_type(parent_meta)?;
        let fk_type = declared_type(child_column)?;
        if let Some(new_parent) = new_parent.filter(|_| cascade_update) {
            parameters[key] = new_parent[usize::from(parent_column)];
            types[key] = pk_type;
        }
        let parameter = first_key_parameter + key;
        parameters[parameter] = old_parent[usize::from(parent_column)];
        types[parameter] = pk_type;
        let operator =
            super::foreign_key_equality::key_operators(child_column.ctype, parent_meta.ctype)[0];
        let (left, right) =
            super::foreign_key_equality::operator_inputs(operator).ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "foreign key equality operator {} is not catalogued",
                    operator
                )
            })?;
        for (out, executable) in [(&mut display, false), (&mut executable, true)] {
            let out: &mut dyn Write = out;
            let mut write = || -> core::fmt::Result {
                write!(
                    out,
                    " {} ${}",
                    if key == 0 { "WHERE" } else { "AND" },
                    parameter + 1
                )?;
                write_cast(out, pk_type, left, executable)?;
                out.write_str(" OPERATOR(pg_catalog.=) ")?;
                quote(out, child_column.name.as_str())?;
                write_cast(out, fk_type, right, executable)
            };
            write().map_err(|_| referential_query_full())?;
        }
    }
    if display.is_truncated() || executable.is_truncated() {
        return Err(referential_query_full());
    }
    let display = arena
        .alloc_str(display.as_str())
        .map_err(|_| crate::sql::eval::arena_full())?;
    let executable = arena
        .alloc_str(executable.as_str())
        .map_err(|_| crate::sql::eval::arena_full())?;
    Ok(ReferentialQuery {
        display,
        executable,
        parameters,
        types,
    })
}

fn referential_query_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "referential action query exceeds {} bytes",
        REFERENTIAL_QUERY_BYTES
    )
}

/// PostgreSQL's `generate_operator_clause` cast of an operand whose type is
/// not the operator's input type, spelled `::pg_catalog.typname`. pos3ql
/// runs the query without casts to polymorphic inputs, which compare their
/// operands' own types.
fn write_cast(
    out: &mut dyn core::fmt::Write,
    operand: i32,
    input: i32,
    executable: bool,
) -> core::fmt::Result {
    if operand == input {
        return Ok(());
    }
    let name = match input {
        2277 => "anyarray",
        3500 => "anyenum",
        3831 => "anyrange",
        4537 => "anymultirange",
        2249 => "record",
        input => {
            return match ColType::from_oid(input) {
                Some(input) => write!(out, "::pg_catalog.{}", input.catalog_name()),
                None => Err(core::fmt::Error),
            };
        }
    };
    if executable {
        return Ok(());
    }
    write!(out, "::pg_catalog.{name}")
}

fn parse_referential_query<'a>(
    text: &'a str,
    arena: &'a Arena,
) -> Result<&'a crate::sql::ast::Stmt<'a>, SqlError> {
    let mut parser = crate::sql::parser::Parser::new(text, arena)
        .map_err(|error| crate::sql::parse_error_to_sql(&error))?;
    let statement = parser
        .next_stmt()
        .map_err(|error| crate::sql::parse_error_to_sql(&error))?
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "a referential action query is empty"
            )
        })?;
    arena
        .alloc(statement)
        .map(|statement| &*statement)
        .map_err(|_| crate::sql::eval::arena_full())
}

/// Whether the child's rules leave no query to report the action's result:
/// an unconditional INSTEAD rule removes the query and no INSTEAD action of
/// the same command replaces it (PostgreSQL's `canSetTag`), which
/// `ri_PerformCheck` reports as an unexpected result.
fn rewrite_sets_no_tag(
    storage: &Storage,
    child_index: usize,
    statement: &crate::sql::ast::Stmt<'_>,
    txid: u32,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let event = match statement {
        crate::sql::ast::Stmt::Update(_) => crate::storage::RewriteEvent::Update,
        _ => crate::storage::RewriteEvent::Delete,
    };
    let target = crate::storage::RuleTarget::Table(child_index as u16);
    let mut suppressed = false;
    let mut replaced = false;
    for (_, rule) in storage.firing_rules_for(target, event, false, txid) {
        let definition = rule.definition_for(txid);
        if definition.mode != crate::storage::RewriteMode::Instead {
            continue;
        }
        suppressed |= definition.condition.is_none();
        for action in definition.action_sql() {
            let mark = arena.mark();
            let same = crate::sql::parser::Parser::new(action, arena)
                .ok()
                .and_then(|mut parser| parser.next_stmt().ok().flatten())
                .is_some_and(|action| {
                    let action = match action {
                        crate::sql::ast::Stmt::With { statement, .. } => *statement,
                        action => action,
                    };
                    core::mem::discriminant(&action) == core::mem::discriminant(statement)
                });
            // SAFETY: the parsed action does not escape this probe.
            unsafe { arena.rewind_to(mark) };
            replaced |= same;
        }
    }
    Ok(suppressed && !replaced)
}

/// A queued action's keys: the old parent row with only its referenced key,
/// and for an update the new one, each encoded at the parent's width after
/// the old row's length.
#[inline(never)]
fn encode_action_keys<'a>(
    old_parent: &[Datum],
    new_parent: Option<&[Datum]>,
    parent_columns: &[u16],
    n_columns: usize,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let mut old = [Datum::Null; MAX_COLUMNS];
    let mut new = [Datum::Null; MAX_COLUMNS];
    for &column in parent_columns {
        old[usize::from(column)] = old_parent[usize::from(column)];
        if let Some(new_parent) = new_parent {
            new[usize::from(column)] = new_parent[usize::from(column)];
        }
    }
    let (old, new) = (&old[..n_columns], &new[..n_columns]);
    let old_length = rowenc::encoded_len(old);
    let new_length = if new_parent.is_some() {
        rowenc::encoded_len(new)
    } else {
        0
    };
    let length = u32::try_from(old_length).map_err(|_| crate::sql::eval::arena_full())?;
    let encoded = arena
        .alloc_slice_with(4 + old_length + new_length, |_| 0u8)
        .map_err(|_| crate::sql::eval::arena_full())?;
    encoded[..4].copy_from_slice(&length.to_le_bytes());
    rowenc::encode(old, &mut encoded[4..4 + old_length]);
    if new_parent.is_some() {
        rowenc::encode(new, &mut encoded[4 + old_length..]);
    }
    Ok(encoded)
}

/// The encoded old key row, the old key and the new key of
/// [`encode_action_keys`], decoded at the parent's width.
#[allow(clippy::type_complexity)]
fn decode_action_keys<'a>(
    keys: &'a [u8],
    parent: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    arena: &'a Arena,
) -> Result<(&'a [u8], &'a [Datum<'a>], Option<&'a [Datum<'a>]>), SqlError> {
    let corrupt = || {
        sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a referential action's keys are malformed"
        )
    };
    let (length, rest) = keys.split_first_chunk::<4>().ok_or_else(corrupt)?;
    let length = usize::try_from(u32::from_le_bytes(*length)).map_err(|_| corrupt())?;
    let (old, new) = rest.split_at_checked(length).ok_or_else(corrupt)?;
    let mut schema = [ColType::Bool; MAX_COLUMNS];
    parent.schema(&mut schema);
    let schema = &schema[..parent.n_columns];
    let decode = |bytes: &'a [u8]| -> Result<&'a [Datum<'a>], SqlError> {
        let values = arena
            .alloc_slice_with(parent.n_columns, |_| Datum::Null)
            .map_err(|_| crate::sql::eval::arena_full())?;
        rowenc::decode(bytes, schema, values)?;
        Ok(values)
    };
    let old_values = decode(old)?;
    let new_values = if new.is_empty() {
        None
    } else {
        Some(decode(new)?)
    };
    Ok((old, old_values, new_values))
}

/// The old parent row with only its referenced key, as a queued check keeps
/// it. Kept out of the recursive cascade's frame.
#[inline(never)]
fn encode_referenced_key<'a>(
    old_parent: &[Datum],
    parent_columns: &[u16],
    n_columns: usize,
    arena: &'a Arena,
) -> Result<&'a [u8], SqlError> {
    let mut key = [Datum::Null; MAX_COLUMNS];
    for &column in parent_columns {
        key[column as usize] = old_parent[column as usize];
    }
    let key = &key[..n_columns];
    let encoded = arena
        .alloc_slice_with(rowenc::encoded_len(key), |_| 0u8)
        .map_err(|_| crate::sql::eval::arena_full())?;
    rowenc::encode(key, encoded);
    Ok(encoded)
}

/// Whether `child_row` references the parent key `old_parent` (a row of
/// `parent`) through `fk` (MATCH SIMPLE: a NULL on either side matches
/// nothing).
#[allow(clippy::too_many_arguments)]
fn referencing_key_matches(
    storage: &Storage,
    txid: u32,
    child: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    fk: &crate::storage::ForeignKey,
    parent: &crate::storage::TableDefinition<impl crate::storage::DefinitionBacking>,
    parent_columns: &[u16],
    old_parent: &[Datum],
    child_row: &[Datum],
    arena: &Arena,
) -> Result<bool, SqlError> {
    let mark = arena.mark();
    let result = (|| {
        for (&child_column, &parent_column) in fk.columns().iter().zip(parent_columns) {
            let referencing = child_row[usize::from(child_column)];
            let referenced = &old_parent[usize::from(parent_column)];
            if referencing.is_null() || referenced.is_null() {
                return Ok(false);
            }
            let parent_column = &parent.columns()[usize::from(parent_column)];
            let equality = super::foreign_key_equality::stored_key_equality(
                child.columns()[usize::from(child_column)].ctype,
                parent_column.ctype,
            )?;
            let referencing =
                super::foreign_key_equality::comparison_value(equality, referencing, arena)?;
            if !key_value_equal(
                storage,
                txid,
                parent_column.collation,
                &referencing,
                referenced,
                arena,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    // SAFETY: the comparison values allocated after the mark do not escape;
    // only the owned answer is returned.
    unsafe { arena.rewind_to(mark) };
    result
}

/// Whether a referencing key value equals a referenced one. Two composite
/// values compare field by field (PostgreSQL's `record_eq`), also when their
/// types differ, the referenced value on the left as in PostgreSQL's
/// referential queries.
fn key_value_equal(
    storage: &Storage,
    txid: u32,
    collation: crate::sql::ast::Collation,
    referencing: &Datum,
    referenced: &Datum,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let composite =
        |value: &Datum| matches!(value, Datum::Composite { .. } | Datum::CompositeText { .. });
    if composite(referencing) && composite(referenced) {
        let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
        let equal = crate::sql::eval::named_composite_comparison(
            crate::sql::ast::BinaryOp::Eq,
            *referenced,
            *referencing,
            Some(&catalog),
            arena,
        )?;
        return Ok(matches!(equal, Some(Datum::Bool(true))));
    }
    Ok(compare_datums_collated(storage, txid, collation, referencing, referenced)?.is_eq())
}

/// A queued NO ACTION or RESTRICT check of a deleted or re-keyed parent row:
/// fails when a row of the child table still references the old key. NO
/// ACTION first looks for another parent row providing that key, as
/// PostgreSQL's `ri_Check_Pk_Match` does; RESTRICT does not. The referencing
/// row found is locked `FOR KEY SHARE`, as PostgreSQL's check query locks it.
#[expect(
    clippy::too_many_arguments,
    reason = "the check carries the queued key and the snapshot it started with"
)]
fn check_referenced_key(
    storage: &Storage,
    txn: &TxnState,
    identity: crate::sql::txn::ConstraintIdentity,
    restrict: bool,
    parent_index: usize,
    old: &[u8],
    read: ValidationRead,
    arena: &Arena,
) -> Result<(), SqlError> {
    let crate::sql::txn::ConstraintIdentity::Table { table, name, .. } = identity else {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "a referenced-key check names a constraint trigger"
        ));
    };
    let txid = txn.txid;
    let child_index = table as usize;
    if child_index >= storage.table_count()
        || !storage.table(child_index).visible_to(txid)
        || parent_index >= storage.table_count()
        || !storage.table(parent_index).visible_to(txid)
    {
        return Ok(());
    }
    let (_, child) = storage.select_table_definition(child_index, txid);
    let Some(fk) = child.fkeys().iter().find(|fk| fk.name == name).copied() else {
        return Ok(());
    };
    if !fk.validation.enforced() {
        return Ok(());
    }
    let mut child_schema = [ColType::Bool; MAX_COLUMNS];
    child.schema(&mut child_schema);
    let child_schema = &child_schema[..child.n_columns];
    let physical_parent = storage.table_def(parent_index, txid).clone();
    let mut parent_schema = [ColType::Bool; MAX_COLUMNS];
    physical_parent.schema(&mut parent_schema);
    let mut old_parent = [Datum::Null; MAX_COLUMNS];
    rowenc::decode(
        old,
        &parent_schema[..physical_parent.n_columns],
        &mut old_parent,
    )?;
    let old_parent = &old_parent[..physical_parent.n_columns];
    let declared_index = storage
        .find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "foreign key parent is not visible"
            )
        })?;
    let declared_parent = storage.table_def(declared_index, txid).clone();
    let mapped = super::remap_columns(&physical_parent, &declared_parent, fk.parent_cols())?;
    let parent_columns = &mapped[..fk.parent_cols().len()];
    if parent_columns
        .iter()
        .any(|column| old_parent[*column as usize].is_null())
    {
        return Ok(());
    }
    let mut old_key = [Datum::Null; crate::storage::MAX_INDEX_COLS];
    let mut old_key_types = [ColType::Bool; crate::storage::MAX_INDEX_COLS];
    for (position, &column) in parent_columns.iter().enumerate() {
        old_key[position] = old_parent[usize::from(column)];
        old_key_types[position] = parent_schema[usize::from(column)];
    }
    if !restrict
        && parent_has_key(
            storage,
            declared_index,
            fk.parent_cols(),
            &old_key[..parent_columns.len()],
            &old_key_types[..parent_columns.len()],
            txid,
            ParentRead::Write {
                isolation: txn.isolation,
                current: read,
            },
            arena,
        )?
    {
        return Ok(());
    }
    let referenced = loop {
        let mut found = None;
        read.for_each_row_state(storage, child_index, &mut |rowid, state| {
            use core::ops::ControlFlow;
            let Some(home) = read.visible_row_home(storage, child_index, rowid, state, txid)?
            else {
                return Ok(ControlFlow::Continue(()));
            };
            let seen = home.snapshot();
            storage.with_row_bytes(child_index, rowid, home, |bytes| {
                let mut row = [Datum::Null; MAX_COLUMNS];
                rowenc::decode(bytes, child_schema, &mut row)?;
                if referencing_key_matches(
                    storage,
                    txid,
                    &child,
                    &fk,
                    &physical_parent,
                    parent_columns,
                    old_parent,
                    &row[..child.n_columns],
                    arena,
                )? {
                    found = Some((rowid, seen));
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            })
        })?;
        let Some((rowid, seen)) = found else {
            break false;
        };
        match super::row_conflicts::lock_selected_row(
            storage,
            txid,
            child_index,
            rowid,
            seen,
            crate::sql::ast::LockStrength::KeyShare,
            crate::sql::ast::LockWait::Wait,
        )? {
            Some(super::row_conflicts::LockedVersion::Selected) => break true,
            Some(_) => continue,
            None => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "a foreign key check skipped a locked row"
                ));
            }
        }
    };
    if !referenced {
        return Ok(());
    }
    let detail = key_detail(
        &mut parent_columns
            .iter()
            .map(|column| physical_parent.columns()[*column as usize].name.as_str()),
        &mut parent_columns
            .iter()
            .map(|column| old_parent[*column as usize]),
        // PostgreSQL 18 words a RESTRICT violation without "still".
        format_args!(
            " is {}referenced from table \"{}\".",
            if restrict { "" } else { "still " },
            child.name.as_str()
        ),
    )?;
    crate::sql::eval::stash_object_diagnostic(
        Some(detail.as_str()),
        crate::sql::eval::ErrorObjects::table_constraint(
            child.schema.as_str(),
            child.name.as_str(),
            fk.name.as_str(),
        ),
    );
    // PostgreSQL names the RESTRICT setting (ri_ReportViolation).
    let (code, setting) = if restrict {
        (sqlstate::RESTRICT_VIOLATION, "RESTRICT setting of ")
    } else {
        (sqlstate::FOREIGN_KEY_VIOLATION, "")
    };
    Err(sql_err!(
        code,
        "update or delete on table \"{}\" violates {}foreign key constraint \"{}\" on table \"{}\"",
        physical_parent.name.as_str(),
        setting,
        fk.name.as_str(),
        child.name.as_str()
    ))
}

/// A key referencing a partitioned relation also protects mutations directed
/// at one of its leaves. Ordinary inheritance does not extend REFERENCES.
fn references_parent(
    storage: &Storage,
    foreign_key: &crate::storage::ForeignKey,
    schema: &str,
    name: &str,
    txid: u32,
) -> bool {
    if foreign_key.parent_schema.as_str() == schema && foreign_key.parent.as_str() == name {
        return true;
    }
    let Some(actual) = storage.find_visible(schema, name, txid) else {
        return false;
    };
    let Some(parent) = storage.find_visible(
        foreign_key.parent_schema.as_str(),
        foreign_key.parent.as_str(),
        txid,
    ) else {
        return false;
    };
    let partitioned = storage.table_def(parent, txid).partition.is_partitioned();
    partitioned && storage.partition_descends_from(actual, parent, txid)
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
            .any(|fk| references_parent(storage, fk, schema, name, txid))
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
    let parent_index = storage
        .find_visible(parent_schema, parent_name, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "foreign key parent is not visible"
            )
        })?;
    let physical_parent = storage.table_def(parent_index, txid).clone();
    for column_index in 0..storage.table_count() {
        if !storage.table(column_index).visible_to(txid) {
            continue;
        }
        let cdef = storage.table_def(column_index, txid).clone();
        for fk in cdef.fkeys() {
            if !references_parent(storage, fk, parent_schema, parent_name, txid) {
                continue;
            }
            let declared_parent = storage
                .find_visible(fk.parent_schema.as_str(), fk.parent.as_str(), txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "foreign key parent is not visible"
                    )
                })?;
            let declared_parent = storage.table_def(declared_parent, txid).clone();
            let mapped =
                super::remap_columns(&physical_parent, &declared_parent, fk.parent_cols())?;
            for (&child_column, &pc) in fk.columns().iter().zip(&mapped) {
                let i = pc as usize;
                let (a, b) = (&old[i], &new[i]);
                if !compare_datums_collated(
                    storage,
                    txid,
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

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;

    #[test]
    fn indexed_tuple_limit_is_checked_before_checkpoint() {
        assert!(check_index_tuple_size(&[0], &[Datum::Text("small")]).is_ok());
        let text = "x".repeat(crate::store::VALUE_INDEX_KEY_MAX);
        let error = check_index_tuple_size(&[0], &[Datum::Text(&text)]).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("index row size"));

        let payload = "y".repeat(crate::store::VALUE_INDEX_TUPLE_MAX);
        let error = check_index_tuple_size_with_include(
            &[0],
            &[1],
            &[Datum::Int4(1), Datum::Text(&payload)],
        )
        .unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(error.message.as_str().contains("index row size"));
    }

    #[test]
    fn malformed_index_column_is_an_error_instead_of_a_panic() {
        let error = check_index_tuple_size(&[1], &[Datum::Int4(7)]).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(error.message.as_str().contains("outside row width 1"));

        let error = check_index_tuple_size_with_include(&[0], &[1], &[Datum::Int4(7)]).unwrap_err();
        assert_eq!(error.sqlstate, sqlstate::INTERNAL_ERROR);
        assert!(error.message.as_str().contains("outside row width 1"));
    }
}
