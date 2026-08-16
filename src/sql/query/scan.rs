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
use crate::sql::ast::{
    BinaryOp, Collation, Expr, FromClause, JoinKind, Select, SelectItem, SetTree, TableRef,
};
use crate::sql::eval::{
    ColumnLookup, EvalHooks, SqlError, cast_to, compare_datums_collated, eval_full,
    hash_key_collated, sqlstate,
};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{MAX_COLUMNS, Storage, rowenc};

use super::plan::{
    MAX_CONJUNCTS, conjunct_passes, expr_tables, fill_join_order, flatten_and, fold_null,
    is_error_safe,
};
use super::{
    MAX_JOIN_TABLES, QueryScope, ResolvedColumn, arena_full, check_timeout, reorder_qual,
    simplify_qual, where_passes,
};

struct NoColumns;

impl<'a> ColumnLookup<'a> for NoColumns {
    fn lookup(&self, _qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        Err(sql_err!(
            sqlstate::UNDEFINED_COLUMN,
            "column \"{}\" does not exist",
            name
        ))
    }
}

fn refresh_catalog_object_names<'a>(
    storage: &Storage,
    txid: u32,
    values: &mut [Datum<'a>],
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let catalog = super::storage_catalog(storage, arena, txid);
    for value in values {
        let Datum::RegObject {
            type_oid,
            referenced_oid,
            ..
        } = *value
        else {
            continue;
        };
        let target = ColType::from_oid(type_oid).ok_or_else(|| {
            sql_err!(
                sqlstate::PROTOCOL_VIOLATION,
                "invalid catalog object type OID"
            )
        })?;
        *value = crate::sql::eval::regobject_cast(
            Datum::Int4(referenced_oid),
            target,
            Some(&catalog),
            arena,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PaxColumnDemand, PaxReadDemand};

    #[test]
    fn selected_columns_require_a_proof() {
        let mut proof = PaxColumnDemand::empty();
        proof.observe(1, 3);

        let full = PaxReadDemand::full_row(super::PaxFullRowReason::WildcardProjection);
        assert_eq!(full.selected_mask(1), None);
        assert_eq!(PaxReadDemand::selected(proof).selected_mask(0), Some(0));
        assert_eq!(
            PaxReadDemand::selected(proof).selected_mask(1),
            Some(1 << 3)
        );
    }
}

/// A complete value probe over one base table. The rowids are sorted so
/// taking the cache path does not make result/error order depend on hash-slot
/// placement.
struct IndexedCandidates<'a> {
    table: usize,
    rowids: &'a [u64],
}

/// A bound base-table row. Storage sources normally carry the canonical row
/// encoding; a columnar source can instead hand the executor its already
/// decoded selected values without reconstituting unneeded payloads.
#[derive(Clone, Copy)]
enum BoundRow<'a> {
    Encoded(&'a [u8]),
    Values(&'a [Datum<'a>]),
}

/// Fixed physical-column demand for every base table in one source scan.
///
/// The masks live in statement stack/arena state rather than a growable plan
/// structure: every source must have a complete proof before PAX may omit a
/// column from its decoded row.
#[derive(Clone, Copy)]
struct PaxColumnDemand {
    masks: [u64; MAX_JOIN_TABLES],
}

/// Why a source intentionally uses full-row decoding.
///
/// A full row is a protocol mode, never an absent or accidental demand proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PaxFullRowReason {
    IncompleteScope,
    WildcardProjection,
    UnprovableExpression,
}

/// The only two legal physical read modes for a source scan.
///
/// The selected state exists only after the query walker has proved the
/// complete set of observable base fields. Full-row state is explicit for
/// whole-row or derived expressions; it is never an absent proof.
#[derive(Clone, Copy)]
pub(super) struct PaxReadDemand(PaxReadMode);

#[derive(Clone, Copy)]
enum PaxReadMode {
    FullRow {
        reason: PaxFullRowReason,
        columns: PaxColumnDemand,
    },
    Selected(PaxColumnDemand),
}

impl PaxReadDemand {
    pub(super) const fn full_row(reason: PaxFullRowReason) -> Self {
        Self(PaxReadMode::FullRow {
            reason,
            columns: PaxColumnDemand::empty(),
        })
    }

    fn selected(columns: PaxColumnDemand) -> Self {
        Self(PaxReadMode::Selected(columns))
    }

    /// The selected PAX fields for `table`, if this scan has a proof.
    fn selected_mask(self, table: usize) -> Option<u64> {
        match self.0 {
            PaxReadMode::FullRow {
                reason:
                    PaxFullRowReason::IncompleteScope
                    | PaxFullRowReason::WildcardProjection
                    | PaxFullRowReason::UnprovableExpression,
                columns,
            } => {
                debug_assert!(columns.masks.iter().all(|mask| *mask == 0));
                None
            }
            PaxReadMode::Selected(columns) => Some(columns.mask(table)),
        }
    }
}

impl PaxColumnDemand {
    const fn empty() -> Self {
        Self {
            masks: [0; MAX_JOIN_TABLES],
        }
    }

    fn observe(&mut self, table: usize, column: usize) {
        debug_assert!(table < MAX_JOIN_TABLES);
        debug_assert!(column < u64::BITS as usize);
        self.masks[table] |= 1u64 << column;
    }

    /// The verified physical fields required for one base source.
    pub(crate) fn mask(self, table: usize) -> u64 {
        self.masks[table]
    }
}

/// Builds the complete physical-column demand for a source scan.
/// Returning `None` deliberately retains full decoding whenever an expression
/// can observe a row shape this scan does not own (a derived row or whole-row
/// value). Correlated nested queries contribute their outer-column references
/// to this scan's proof and retain their own independent inner proof.
pub(super) fn pax_column_demand(
    scope: &QueryScope,
    from: &FromClause,
    expressions: &[&Expr],
) -> PaxReadDemand {
    if scope.n == 0 || scope.defs[..scope.n].iter().any(Option::is_none) {
        return PaxReadDemand::full_row(PaxFullRowReason::IncompleteScope);
    }
    pax_column_demand_bounded(scope, from, expressions)
        .map(PaxReadDemand::selected)
        .unwrap_or_else(|| PaxReadDemand::full_row(PaxFullRowReason::UnprovableExpression))
}

#[inline(never)]
fn pax_column_demand_bounded(
    scope: &QueryScope,
    from: &FromClause,
    expressions: &[&Expr],
) -> Option<PaxColumnDemand> {
    let mut columns = PaxColumnDemand::empty();
    fn collect_table(table: &TableRef, scope: &QueryScope, columns: &mut PaxColumnDemand) -> bool {
        if let Some(arguments) = table.func_args
            && arguments
                .iter()
                .any(|argument| !collect(argument, scope, columns))
        {
            return false;
        }
        table
            .subquery
            .is_none_or(|select| collect_select(select, scope, columns))
    }

    fn collect_set(tree: &SetTree, scope: &QueryScope, columns: &mut PaxColumnDemand) -> bool {
        match tree {
            SetTree::Select(select) => collect_select(select, scope, columns),
            SetTree::Op { left, right, .. } => {
                collect_set(left, scope, columns) && collect_set(right, scope, columns)
            }
        }
    }

    fn collect_select(select: &Select, scope: &QueryScope, columns: &mut PaxColumnDemand) -> bool {
        if let Some(body) = select.set_body
            && !collect_set(body, scope, columns)
        {
            return false;
        }
        if let Some(from) = select.from
            && (!collect_table(&from.base, scope, columns)
                || from.joins.iter().any(|join| {
                    !collect_table(&join.table, scope, columns)
                        || join.on.is_some_and(|on| !collect(on, scope, columns))
                }))
        {
            return false;
        }
        if select
            .with
            .iter()
            .any(|cte| !collect_select(cte.query, scope, columns))
        {
            return false;
        }
        select.items.iter().all(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                collect(expression, scope, columns)
            }
            // A wildcard belongs to the nested select's own output. It cannot
            // name an enclosing physical column.
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => true,
        }) && select
            .distinct_on
            .iter()
            .all(|expression| collect(expression, scope, columns))
            && select
                .where_clause
                .is_none_or(|expression| collect(expression, scope, columns))
            && select
                .group_by
                .iter()
                .all(|expression| collect(expression, scope, columns))
            && select
                .having
                .is_none_or(|expression| collect(expression, scope, columns))
            && select
                .order_by
                .iter()
                .all(|order| collect(order.expression, scope, columns))
            && select
                .limit
                .is_none_or(|expression| collect(expression, scope, columns))
            && select
                .offset
                .is_none_or(|expression| collect(expression, scope, columns))
    }

    fn collect(expression: &Expr, scope: &QueryScope, columns: &mut PaxColumnDemand) -> bool {
        match expression {
            Expr::Column { qualifier, name } => match scope.find_column(*qualifier, name) {
                Ok(ResolvedColumn::Table(table, column)) if scope.derived[table].is_none() => {
                    columns.observe(table, column)
                }
                Ok(ResolvedColumn::Table(_, _)) => {}
                // An unresolved name can be an enclosing correlated column.
                // The select walker records it against the enclosing physical
                // scope while this inner scan needs no local span for it.
                Err(_) => {}
                Ok(ResolvedColumn::Merged(_)) => return false,
            },
            Expr::WholeRow(_) | Expr::SchemaColumn { .. } => return false,
            Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
                return collect_select(select, scope, columns);
            }
            Expr::InSubquery {
                operand, select, ..
            } => return collect(operand, scope, columns) && collect_select(select, scope, columns),
            _ => {}
        }
        let mut complete = true;
        let _ = super::subquery::walk_children(expression, &mut |child| {
            if !collect(child, scope, columns) {
                complete = false;
            }
            Ok(())
        });
        complete
    }
    for expression in expressions {
        if !collect(expression, scope, &mut columns) {
            return None;
        }
    }
    for (index, join) in from.joins.iter().enumerate() {
        if let Some(condition) = join.on.or(scope.join_on[index])
            && !collect(condition, scope, &mut columns)
        {
            return None;
        }
    }
    Some(columns)
}

/// Finds one single-column `indexed_column = constant` conjunct. This is
/// intentionally conservative: joins, derived rows, parameters and
/// multi-column keys stay on the ordinary scan until their access path can
/// preserve the same visibility and coercion guarantees.
fn indexed_candidates<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    txid: u32,
    where_clause: Option<&Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<IndexedCandidates<'a>>, SqlError> {
    if scope.n != 1
        || scope.derived[0].is_some()
        || scope.lateral[0]
        // Resident and object generations carry the published base plus the
        // latest overlay, not every intermediate key image retained for a
        // repeatable-read snapshot. The ordinary MVCC scan owns that case.
        || storage.commit_snapshot() != storage.lsn()
        || storage.has_pending_rows(scope.slots[0])
    {
        return Ok(None);
    }
    fn reverse(operator: BinaryOp) -> BinaryOp {
        match operator {
            BinaryOp::Lt => BinaryOp::Gt,
            BinaryOp::LtEq => BinaryOp::GtEq,
            BinaryOp::Gt => BinaryOp::Lt,
            BinaryOp::GtEq => BinaryOp::LtEq,
            other => other,
        }
    }
    fn find<'a>(
        expression: &'a Expr<'a>,
        scope: &QueryScope<'_>,
    ) -> Option<(usize, &'a Expr<'a>, BinaryOp)> {
        match expression {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
            } => find(left, scope).or_else(|| find(right, scope)),
            Expr::Binary {
                operator,
                left,
                right,
            } if matches!(
                operator,
                BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq
            ) =>
            {
                let side = |column: &'a Expr<'a>,
                            constant: &'a Expr<'a>,
                            operator: BinaryOp|
                 -> Option<(usize, &'a Expr<'a>, BinaryOp)> {
                    let Expr::Column { qualifier, name } = column else {
                        return None;
                    };
                    // `is_constant` also admits stable/volatile scalar calls
                    // for the expression planner. An index key must be
                    // invariant between candidate selection and the ordinary
                    // WHERE recheck, so decline every call here.
                    if !constant.is_constant() || constant.contains_call() {
                        return None;
                    }
                    match scope.find_column(*qualifier, name).ok()? {
                        ResolvedColumn::Table(0, index) => Some((index, constant, operator)),
                        _ => None,
                    }
                };
                side(left, right, *operator).or_else(|| side(right, left, reverse(*operator)))
            }
            _ => None,
        }
    }
    let Some((column, constant, operator)) = where_clause.and_then(|clause| find(clause, scope))
    else {
        return Ok(None);
    };
    let columns = [column as u16];
    let slot = scope.slots[0];
    if (operator == BinaryOp::Eq && !storage.value_probe_complete(slot, &columns))
        || (operator != BinaryOp::Eq && !storage.value_durable_complete(slot, &columns))
    {
        return Ok(None);
    }
    let target_type = scope.defs[0]
        .expect("physical table has definition")
        .columns()[column]
        .ctype;
    let target_collation = scope.defs[0]
        .expect("physical table has definition")
        .columns()[column]
        .collation;
    let statistics = storage.table_statistics(slot, txid);
    let expected_rows = if statistics.valid && statistics.columns[column].valid {
        let distinct = statistics.columns[column].distinct_values.max(1);
        statistics.rows.div_ceil(distinct).max(1)
    } else {
        1
    };
    if storage.sequential_spill_scan_is_cheaper(slot, expected_rows, txid) {
        return Ok(None);
    }
    let raw = eval_full(constant, arena, params, &NoColumns, hooks)?;
    let raw_type = ColType::from_oid(raw.type_oid());
    let integer =
        |column_type: ColType| matches!(column_type, ColType::Int2 | ColType::Int4 | ColType::Int8);
    let integer_compatible = raw_type.is_some_and(integer) && integer(target_type);
    // An untyped string literal is coerced to the indexed column by the
    // equality operator. Already-typed constants are safe only when their
    // representation has the same equality hash (the integer widths share
    // one canonical hash). Declining other cross-type operators avoids, for
    // example, turning `integer_column = 1.1::numeric` into an index probe for
    // a rounded integer.
    if !matches!(constant, Expr::Str(_)) && raw_type != Some(target_type) && !integer_compatible {
        return Ok(None);
    }
    let value = cast_to(raw, target_type, arena)?;
    if value.is_null() {
        return Ok(Some(IndexedCandidates {
            table: 0,
            rowids: &[],
        }));
    }
    let key_matches = |key: &[u8]| -> Result<bool, SqlError> {
        let mut decoded = [Datum::Null];
        rowenc::decode(key, &[target_type], &mut decoded)?;
        if decoded[0].is_null() {
            return Ok(false);
        }
        let ordering = compare_datums_collated(storage, target_collation, &decoded[0], &value)?;
        Ok(match operator {
            BinaryOp::Eq => ordering.is_eq(),
            BinaryOp::Lt => ordering.is_lt(),
            BinaryOp::LtEq => ordering.is_le(),
            BinaryOp::Gt => ordering.is_gt(),
            BinaryOp::GtEq => ordering.is_ge(),
            _ => unreachable!("filtered comparison"),
        })
    };
    let hash = hash_key_collated(&[value], &[0], &[target_collation]);
    let mut count = 0usize;
    if operator == BinaryOp::Eq {
        let complete = storage.probe_value(slot, &columns, hash, |_| count += 1)?;
        debug_assert!(complete, "completeness checked before probe");
    } else {
        let complete = storage.walk_value_index(slot, &columns, |_, key| {
            if key_matches(key)? {
                count += 1;
            }
            Ok(())
        })?;
        debug_assert!(complete, "durable completeness checked before scan");
    }
    let Ok(rowids) = arena.alloc_slice_with(count, |_| 0u64) else {
        // Candidate materialization is an optimization. A broad range or a
        // highly duplicated key may exceed statement scratch even though the
        // ordinary streaming table walk does not; decline the access path
        // instead of changing a successful query into a 54000 error.
        return Ok(None);
    };
    let mut fill = 0usize;
    if operator == BinaryOp::Eq {
        storage.probe_value(slot, &columns, hash, |rowid| {
            rowids[fill] = rowid;
            fill += 1;
        })?;
    } else {
        storage.walk_value_index(slot, &columns, |rowid, key| {
            if key_matches(key)? {
                rowids[fill] = rowid;
                fill += 1;
            }
            Ok(())
        })?;
    }
    rowids.sort_unstable();
    let mut unique = 0usize;
    for read in 0..rowids.len() {
        if read == 0 || rowids[read] != rowids[read - 1] {
            rowids[unique] = rowids[read];
            unique += 1;
        }
    }
    Ok(Some(IndexedCandidates {
        table: 0,
        rowids: &rowids[..unique],
    }))
}

/// One assembled source row: per table, decoded values (empty slice =
/// LEFT-join null row; None = not yet joined).
pub struct JoinRow<'s, 'v, 'd> {
    pub scope: &'s QueryScope<'d>,
    pub values: [Option<&'s [Datum<'v>]>; MAX_JOIN_TABLES],
    /// Stable MVCC identities for physical base-table contributors. Derived
    /// rows, function rows, and outer-join null sides carry `None`.
    pub rowids: &'s [Option<u64>],
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

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        match self.scope.find_column(qualifier, name).ok() {
            Some(ResolvedColumn::Table(table, column)) => self.scope.defs[table]
                .and_then(|definition| definition.columns().get(column))
                .map(|column| column.collation)
                .unwrap_or(crate::sql::ast::Collation::None),
            Some(ResolvedColumn::Merged(merged)) => self.scope.merged[merged].parts
                [..self.scope.merged[merged].n_parts]
                .first()
                .and_then(|&(table, column)| self.scope.defs[table]?.columns().get(column))
                .map(|column| column.collation)
                .unwrap_or(crate::sql::ast::Collation::None),
            None => crate::sql::ast::Collation::None,
        }
    }

    fn column_domain(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::SqlName> {
        match self.scope.find_column(qualifier, name).ok()? {
            ResolvedColumn::Table(t, c) => self.scope.defs[t].and_then(|def| {
                def.columns()
                    .get(c)
                    .and_then(|col| col.user_type.map(|identity| identity.name))
            }),
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

    fn collation(&self, q: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        let inner = self.inner.collation(q, name);
        if inner != crate::sql::ast::Collation::None {
            inner
        } else {
            self.outer
                .map(|outer| outer.collation(q, name))
                .unwrap_or(crate::sql::ast::Collation::None)
        }
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
        return super::table_func_rows_outer(tref, storage, txid, arena, params, outer, None, None);
    }
    Err(sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "LATERAL is supported only for a subquery or a set-returning function"
    ))
}

fn external_lateral_function_run<'a, C: ColumnLookup<'a>>(
    storage: &'a Storage,
    txid: u32,
    tref: &'a crate::sql::ast::TableRef<'a>,
    width: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: &C,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let call = arena
        .alloc(Expr::Call {
            name: tref.table,
            args: tref.func_args.expect("table function"),
            star: false,
            distinct: false,
            order_by: &[],
            over: None,
            filter: None,
        })
        .map_err(|_| arena_full())?;
    let catalog = super::storage_catalog(storage, arena, txid);
    let hooks = EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    let count = super::srf::srf_count(&*call, arena, params, outer, &hooks)?;
    let base_width = width - usize::from(tref.with_ordinality);
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
    for index in 1..=count {
        let mark = arena.mark();
        let row_hooks = EvalHooks {
            srf_index: Some(index),
            ..hooks
        };
        let value = eval_full(&*call, arena, params, outer, &row_hooks)?;
        let mut values = [Datum::Null; MAX_COLUMNS];
        match (base_width, value) {
            (1, value) => values[0] = value,
            (_, Datum::Record(fields)) if fields.len() == base_width => {
                for (column, field) in fields.iter().enumerate() {
                    values[column] = field.value;
                }
            }
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "set-returning function row width does not match its definition"
                ));
            }
        }
        if tref.with_ordinality {
            values[base_width] = Datum::Int8(index as i64);
        }
        storage
            .with_block_store(|blocks| {
                sorter.push_projected_by(blocks, width, |column| values[column], &mut compare)
            })
            .expect("lateral function run has a block store")?;
        // SAFETY: the evaluated SRF row was encoded into the immutable run;
        // no datum allocated after this mark is retained.
        unsafe { arena.rewind_to(mark) };
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("lateral function run has a block store")
}

enum LateralRows<'a> {
    External(Option<crate::sql::external::ExternalRun>),
    Local(&'a [&'a [u8]]),
}

#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn materialize_lateral_source<'a, C: ColumnLookup<'a>>(
    storage: &'a Storage,
    txid: u32,
    tref: &'a crate::sql::ast::TableRef<'a>,
    width: usize,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: &C,
) -> Result<LateralRows<'a>, SqlError> {
    let external_function = tref.func_args.is_some()
        && super::srf::is_srf_name(tref.table)
        && !tref.table.eq_ignore_ascii_case("pg_options_to_table")
        && !tref.table.eq_ignore_ascii_case("pg_get_sequence_data");
    if !storage.spill_attached() || (tref.subquery.is_none() && !external_function) {
        return Ok(LateralRows::Local(materialize_lateral(
            storage, txid, tref, arena, params, outer,
        )?));
    }
    if tref.subquery.is_none() {
        return Ok(LateralRows::External(external_lateral_function_run(
            storage, txid, tref, width, arena, params, outer,
        )?));
    }
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
    super::select_into_rows_recycling(
        storage,
        txid,
        tref.subquery.expect("checked"),
        arena,
        params,
        Some(outer),
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
    let run = storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("spill-attached block store")?;
    Ok(LateralRows::External(run))
}

fn consume_external_lateral_run<'a>(
    storage: &'a Storage,
    run: crate::sql::external::ExternalRun,
    arena: &'a Arena,
    recycle_rows: bool,
    visit: &mut impl FnMut(usize, &'a [u8]) -> Result<bool, SqlError>,
) -> Result<bool, SqlError> {
    let mut reader = storage.external_run_reader()?;
    storage
        .with_block_store(|blocks| reader.start(blocks, run))
        .expect("spill-attached block store")?;
    let mut index = 0usize;
    while let Some(bytes) = reader.row() {
        check_timeout()?;
        let mark = recycle_rows.then(|| arena.mark());
        let owned = arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
        let keep_scanning = visit(index, owned)?;
        index += 1;
        if let Some(mark) = mark {
            // SAFETY: the candidate and every evaluator result derived from it
            // were consumed synchronously by `visit`.
            unsafe { arena.rewind_to(mark) };
        }
        if !keep_scanning {
            return Ok(false);
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("spill-attached block store")?;
    }
    Ok(true)
}

fn record_external_match(
    storage: &Storage,
    writer: Option<core::ptr::NonNull<crate::sql::external::ExternalSorter>>,
    depth: usize,
    index: usize,
) -> Result<(), SqlError> {
    let Some(mut writer) = writer else {
        return Ok(());
    };
    // SAFETY: the pointer is created from a live sorter immediately around a
    // synchronous scan and is never retained by a row or immutable run.
    let writer = unsafe { writer.as_mut() };
    let mut compare = compare_external_matches;
    storage
        .with_block_store(|blocks| {
            writer.push_projected_by(
                blocks,
                2,
                |column| {
                    if column == 0 {
                        Datum::Int4(depth as i32)
                    } else {
                        Datum::Int8(index as i64)
                    }
                },
                &mut compare,
            )
        })
        .expect("external match map has a block store")
}

fn external_match_contains(
    storage: &Storage,
    reader: &mut crate::sql::external::ExternalRunReader,
    depth: usize,
    index: usize,
) -> Result<bool, SqlError> {
    while let Some(row) = reader.row() {
        let row_depth = match crate::sql::exec::decode_projected_pub(row, 0) {
            Datum::Int4(value) => value as usize,
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "external join match depth is corrupt"
                ));
            }
        };
        let row_index = match crate::sql::exec::decode_projected_pub(row, 1) {
            Datum::Int8(value) => value as usize,
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "external join match index is corrupt"
                ));
            }
        };
        match (row_depth, row_index).cmp(&(depth, index)) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal => return Ok(true),
            core::cmp::Ordering::Greater => return Ok(false),
        }
        storage
            .with_block_store(|blocks| reader.advance(blocks))
            .expect("external match map has a block store")?;
    }
    Ok(false)
}

fn compare_external_matches(left: &[u8], right: &[u8]) -> Result<core::cmp::Ordering, SqlError> {
    let key = |row: &[u8]| -> Result<(i32, i64), SqlError> {
        let depth = match crate::sql::exec::decode_projected_pub(row, 0) {
            Datum::Int4(value) => value,
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "external join match depth is corrupt"
                ));
            }
        };
        let index = match crate::sql::exec::decode_projected_pub(row, 1) {
            Datum::Int8(value) => value,
            _ => {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "external join match index is corrupt"
                ));
            }
        };
        Ok((depth, index))
    };
    Ok(key(left)?.cmp(&key(right)?))
}

fn combine_external_match_runs(
    storage: &Storage,
    left: Option<crate::sql::external::ExternalRun>,
    right: Option<crate::sql::external::ExternalRun>,
) -> Result<Option<crate::sql::external::ExternalRun>, SqlError> {
    let mut sorter = storage.external_sorter()?;
    sorter.reset();
    let mut compare = compare_external_matches;
    let mut reader = storage.external_run_reader()?;
    for run in [left, right].into_iter().flatten() {
        storage
            .with_block_store(|blocks| reader.start(blocks, run))
            .expect("external match map has a block store")?;
        while let Some(row) = reader.row() {
            storage
                .with_block_store(|blocks| sorter.push_encoded(blocks, row, &mut compare))
                .expect("external match map has a block store")?;
            storage
                .with_block_store(|blocks| reader.advance(blocks))
                .expect("external match map has a block store")?;
        }
    }
    storage
        .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
        .expect("external match map has a block store")
}

/// Source scan with an explicit complete-row or proven-column PAX read mode.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_source_with_pax_columns<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    pax_demand: PaxReadDemand,
    f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    scan_source_mode(
        storage,
        scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        hooks,
        outer,
        false,
        pax_demand,
        f,
    )
}

/// Recycling source scan with an explicit complete-row or proven-column PAX
/// read mode.
#[allow(clippy::too_many_arguments)]
pub(super) fn scan_source_recycling_with_pax_columns<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    pax_demand: PaxReadDemand,
    f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    scan_source_mode(
        storage,
        scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        hooks,
        outer,
        true,
        pax_demand,
        f,
    )
}

/// One build-side entry: the join-key hash, the row's MVCC identity, and its
/// encoded bytes (kept to assemble matches and re-decode the key on a hash
/// hit, so any key width works without growing the entry).
#[derive(Clone, Copy)]
struct HashEntry<'a> {
    hash: u64,
    rowid: Option<u64>,
    row: BoundRow<'a>,
}

const EMPTY_HASH_ENTRY: HashEntry<'static> = HashEntry {
    hash: 0,
    rowid: None,
    row: BoundRow::Encoded(&[]),
};

/// Whether two join-key column types compare and hash identically to the `=`
/// operator: the same type, or any mix of integer widths (an int is an int at
/// any width to both `compare_datums` and `hash_key`). Other type pairs select
/// the nested-loop plan rather than assume a coercion-compatible hash key.
fn join_key_types_compatible(a: ColType, b: ColType) -> bool {
    fn is_integer(t: ColType) -> bool {
        matches!(t, ColType::Int2 | ColType::Int4 | ColType::Int8)
    }
    a == b || (is_integer(a) && is_integer(b))
}

/// Extracts every equi-join key for a two-table hash join: each
/// `probe_col = build_col` conjunct in the ON clause or the WHERE that is a
/// plain column reference into each table with a type compatible with `=`.
/// Returns the matched pairs (probe column, build column). The keys only
/// generate candidates — the full ON and WHERE still run at the leaf, so a
/// missed extraction never changes a result, only the access path.
#[allow(clippy::type_complexity)]
fn hash_join_keys<'a>(
    scope: &QueryScope<'a>,
    on: Option<&'a Expr<'a>>,
    where_clause: Option<&'a Expr<'a>>,
    probe_t: usize,
    build_t: usize,
) -> Result<Option<([(usize, usize); 8], usize)>, SqlError> {
    let mut conjuncts: [&Expr; MAX_CONJUNCTS] = [&Expr::Null; MAX_CONJUNCTS];
    let mut nc = 0usize;
    for source in [on, where_clause].into_iter().flatten() {
        let mut flat = [source; MAX_CONJUNCTS];
        let mut count = 0;
        let parts: &[&Expr] = if flatten_and(source, &mut flat, &mut count) {
            &flat[..count]
        } else {
            core::slice::from_ref(&source)
        };
        for &p in parts {
            if nc < MAX_CONJUNCTS {
                conjuncts[nc] = p;
                nc += 1;
            }
        }
    }
    let mut pairs = [(0usize, 0usize); 8];
    let mut npairs = 0usize;
    for &c in &conjuncts[..nc] {
        let Expr::Binary {
            operator: BinaryOp::Eq,
            left,
            right,
        } = c
        else {
            continue;
        };
        let Expr::Column {
            qualifier: lq,
            name: ln,
        } = **left
        else {
            continue;
        };
        let Expr::Column {
            qualifier: rq,
            name: rn,
        } = **right
        else {
            continue;
        };
        let (Ok(ResolvedColumn::Table(lt, lc)), Ok(ResolvedColumn::Table(rt, rc))) =
            (scope.find_column(lq, ln), scope.find_column(rq, rn))
        else {
            continue;
        };
        let (probe_col, build_col) = if lt == probe_t && rt == build_t {
            (lc, rc)
        } else if rt == probe_t && lt == build_t {
            (rc, lc)
        } else {
            continue;
        };
        let pt = scope.defs[probe_t].expect("resolved").columns()[probe_col].ctype;
        let bt = scope.defs[build_t].expect("resolved").columns()[build_col].ctype;
        let probe_collation = scope.defs[probe_t].expect("resolved").columns()[probe_col].collation;
        let build_collation = scope.defs[build_t].expect("resolved").columns()[build_col].collation;
        if probe_collation != build_collation {
            return Err(sql_err!(
                crate::sql::eval::sqlstate::COLLATION_MISMATCH,
                "collation mismatch between \"{}\" and \"{}\"",
                probe_collation.name(),
                build_collation.name()
            ));
        }
        if join_key_types_compatible(pt, bt) && npairs < pairs.len() {
            pairs[npairs] = (probe_col, build_col);
            npairs += 1;
        }
    }
    if npairs == 0 {
        Ok(None)
    } else {
        Ok(Some((pairs, npairs)))
    }
}

/// A selected two-table hash implementation. Planning is shared with EXPLAIN
/// so reported and executed physical join choices remain one decision.
pub(crate) struct HashJoinPlan<'a> {
    probe_table: usize,
    build_table: usize,
    keys: [(usize, usize); 8],
    key_count: usize,
    key_collations: [Collation; 8],
    on: Option<&'a Expr<'a>>,
    preserves_probe_rows: bool,
    build_capacity: usize,
}

/// Selects the hash implementation only when its fixed build table and
/// equi-key representation can execute this join exactly. Every other shape
/// selects the ordinary nested-loop implementation before execution.
pub(crate) fn select_hash_join_plan<'a>(
    storage: &Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    where_clause: Option<&'a Expr<'a>>,
    order: &[usize],
) -> Result<Option<HashJoinPlan<'a>>, SqlError> {
    if scope.n != 2 || from.joins.len() != 1 {
        return Ok(None);
    }
    let join = &from.joins[0];
    if !matches!(
        join.kind,
        JoinKind::Inner | JoinKind::Cross | JoinKind::Left
    ) {
        return Ok(None);
    }
    let first_table = order[0];
    let second_table = order[1];
    if scope.lateral[first_table] || scope.lateral[second_table] {
        return Ok(None);
    }
    // An inner or cross join may build its fixed hash table from a derived
    // source. Its encoded rows carry a fixed output schema; probing a derived
    // source would instead require a second materialization lifetime.
    let (probe_table, build_table) = if scope.derived[first_table].is_some()
        && scope.derived[second_table].is_none()
        && matches!(join.kind, JoinKind::Inner | JoinKind::Cross)
    {
        (second_table, first_table)
    } else {
        (first_table, second_table)
    };
    if scope.derived[probe_table].is_some() {
        return Ok(None);
    }
    let on = join.on.or(scope.join_on[0]);
    let Some((keys, key_count)) =
        hash_join_keys(scope, on, where_clause, probe_table, build_table)?
    else {
        return Ok(None);
    };
    const MAX_HASH_ENTRIES: usize = 1 << 15;
    let build_capacity = if let Some(run) = scope.external_runs[build_table] {
        usize::try_from(run.rows()).map_err(|_| arena_full())?
    } else {
        scope.derived[build_table].map_or_else(
            || storage.planning_row_estimate(scope.slots[build_table]) as usize,
            <[&[u8]]>::len,
        )
    };
    if build_capacity == 0 || build_capacity > MAX_HASH_ENTRIES {
        return Ok(None);
    }
    let mut key_collations = [Collation::None; 8];
    for (index, &(probe_column, _)) in keys.iter().take(key_count).enumerate() {
        key_collations[index] =
            scope.defs[probe_table].expect("resolved").columns()[probe_column].collation;
    }
    Ok(Some(HashJoinPlan {
        probe_table,
        build_table,
        keys,
        key_count,
        key_collations,
        on,
        preserves_probe_rows: matches!(join.kind, JoinKind::Left),
        build_capacity,
    }))
}

#[allow(clippy::too_many_arguments)]
fn scan_source_mode<'a>(
    storage: &'a Storage,
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    outer: Option<&dyn ColumnLookup<'a>>,
    recycle_rows: bool,
    pax_demand: PaxReadDemand,
    f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
) -> Result<(), SqlError> {
    let current_role = storage.current_role_slot(txid).ok_or_else(|| {
        sql_err!(
            sqlstate::INSUFFICIENT_PRIVILEGE,
            "current role is not present in the role catalog"
        )
    })?;
    for table in 0..scope.n {
        if scope.derived[table].is_none() {
            let authorization_role =
                scope.authorization_roles[table].map_or(current_role, usize::from);
            let object = storage.table_access_object(scope.slots[table], txid);
            let definition = storage.table_def(scope.slots[table], txid);
            storage.require_schema_usage_as(
                definition.schema.as_str(),
                authorization_role,
                txid,
            )?;
            if !storage.has_object_privilege(
                object,
                authorization_role,
                crate::storage::PrivilegeSet::SELECT,
                txid,
            ) {
                return Err(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "permission denied for table {}",
                    definition.name.as_str()
                ));
            }
            storage.lock_table(
                txid,
                scope.slots[table],
                crate::sql::ast::TableLockMode::AccessShare,
                false,
            )?;
            storage.record_serializable_read(txid, scope.slots[table]);
        }
    }
    fn recycled<R>(arena: &Arena, enabled: bool, operation: impl FnOnce() -> R) -> R {
        let mark = enabled.then(|| arena.mark());
        let result = operation();
        if let Some(mark) = mark {
            // SAFETY: recycling callers consume their source row inside
            // `operation`; no reference into its arena suffix is observed
            // after this point.
            unsafe { arena.rewind_to(mark) };
        }
        result
    }

    // Simplify plan-time-decided boolean arms, fold `col IS [NOT] NULL` on
    // NOT-NULL columns, then order the WHERE conjuncts by PostgreSQL's clause
    // cost once, up front, so the per-row leaf evaluates them cheapest-first
    // without re-sorting.
    // Physical-plan choice is made from the parsed predicate, exactly as
    // EXPLAIN sees it. Simplification below is an evaluation detail and must
    // not change the reported implementation.
    let planning_where_clause = where_clause;
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
        storage: &Storage,
        txid: u32,
        scope: &'s QueryScope<'d>,
        bound: &[Option<BoundRow<'v>>],
        bound_rowids: &'s [Option<u64>],
        order: &[usize],
        count: usize,
        buffers: &'s mut [[Datum<'v>; MAX_COLUMNS]],
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
                Some(BoundRow::Encoded(bytes)) => {
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
                        refresh_catalog_object_names(storage, txid, buffer, arena)?;
                    }
                    values[t] = Some(&buffer[..def.n_columns]);
                }
                Some(BoundRow::Values(row_values)) => values[t] = Some(row_values),
                None => values[t] = Some(&[]), // outer-join null row
            }
        }
        Ok(JoinRow {
            scope,
            values,
            rowids: &bound_rowids[..scope.n],
        })
    }

    /// Executes a previously selected two-table hash plan. Its fixed capacity
    /// is a plan invariant: stale statistics that exceed it raise 54000 rather
    /// than changing the execution strategy after query execution begins.
    #[allow(clippy::too_many_arguments)]
    fn execute_hash_join_plan<'a>(
        storage: &'a Storage,
        scope: &QueryScope<'a>,
        txid: u32,
        where_clause: Option<&'a Expr<'a>>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
        order: &[usize],
        plan: HashJoinPlan<'a>,
        decode_buffers: &mut [[Datum<'a>; MAX_COLUMNS]],
        recycle_rows: bool,
        pax_demand: PaxReadDemand,
        f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
    ) -> Result<(), SqlError> {
        use core::ops::ControlFlow;
        let probe_t = plan.probe_table;
        let build_t = plan.build_table;
        let keys = plan.keys;
        let nkeys = plan.key_count;
        let key_collations = plan.key_collations;
        let on = plan.on;
        let build_slot = scope.slots[build_t];
        let build_def = scope.defs[build_t].expect("resolved");
        let build_derived_rows = scope.derived[build_t];
        let build_external_run = scope.external_runs[build_t];
        let build_count = plan.build_capacity;
        let completed = (|| -> Result<bool, SqlError> {
            let buckets_len = (build_count * 2).next_power_of_two().max(16);
            let entries = arena
                .alloc_slice_with(build_count, |_| EMPTY_HASH_ENTRY)
                .map_err(|_| arena_full())?;
            let next = arena
                .alloc_slice_with(build_count, |_| 0u32)
                .map_err(|_| arena_full())?;
            let buckets = arena
                .alloc_slice_with(buckets_len, |_| u32::MAX)
                .map_err(|_| arena_full())?;
            let mut build_schema = [ColType::Bool; MAX_COLUMNS];
            build_def.schema(&mut build_schema);
            let build_schema = &build_schema[..build_def.n_columns];
            let hash_cols: [u16; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
            let mut key_vals = [Datum::Null; 8];
            let mut n = 0usize;
            macro_rules! insert_build {
                ($rowid:expr, $row:expr, $values:expr) => {{
                    let values = $values;
                    let mut any_null = false;
                    for i in 0..nkeys {
                        key_vals[i] = values[keys[i].1];
                        if key_vals[i].is_null() {
                            any_null = true;
                            break;
                        }
                    }
                    if !any_null {
                        if n == entries.len() {
                            return Err(arena_full());
                        }
                        let hash = hash_key_collated(
                            &key_vals[..nkeys],
                            &hash_cols[..nkeys],
                            &key_collations[..nkeys],
                        );
                        let bucket = (hash as usize) & (buckets_len - 1);
                        entries[n] = HashEntry {
                            hash,
                            rowid: $rowid,
                            row: $row,
                        };
                        next[n] = buckets[bucket];
                        buckets[bucket] = n as u32;
                        n += 1;
                    }
                }};
            }
            macro_rules! insert_derived {
                ($bytes:expr) => {{
                    let bytes = $bytes;
                    let mut values = [Datum::Null; MAX_COLUMNS];
                    for (column, value) in values.iter_mut().enumerate().take(build_def.n_columns) {
                        *value =
                            crate::sql::exec::decode_projected_col_record(bytes, column, arena)?;
                    }
                    insert_build!(
                        None,
                        BoundRow::Encoded(bytes),
                        &values[..build_def.n_columns]
                    );
                }};
            }
            if let Some(run) = build_external_run {
                let mut reader = storage.external_run_reader()?;
                storage
                    .with_block_store(|blocks| reader.start(blocks, run))
                    .expect("external run has a block store")?;
                while let Some(bytes) = reader.row() {
                    let bytes = arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
                    insert_derived!(bytes);
                    storage
                        .with_block_store(|blocks| reader.advance(blocks))
                        .expect("external run has a block store")?;
                }
            } else if let Some(rows) = build_derived_rows {
                for &bytes in rows {
                    insert_derived!(bytes);
                }
            } else if let Some(demand) = pax_demand.selected_mask(build_t)
                && storage.spill_rows_are_unshadowed(build_slot)
            {
                storage.for_each_spilled_row_batch(
                    build_slot,
                    arena,
                    false,
                    Some(demand),
                    &mut |rows| {
                        for spilled in rows {
                            let crate::storage::SpilledRowRepresentation::Values(values) =
                                spilled.representation
                            else {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "PAX scan did not return selected values"
                                ));
                            };
                            insert_build!(Some(spilled.rowid), BoundRow::Values(values), values);
                        }
                        Ok(ControlFlow::Continue(()))
                    },
                )?;
            } else {
                storage.for_each_row_state(build_slot, &mut |rowid, state| {
                    let Some(home) = storage.visible_row_home(build_slot, rowid, state, txid)?
                    else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    let bytes = storage.row_bytes(build_slot, rowid, home, arena)?;
                    let mut buffer = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, build_schema, &mut buffer)?;
                    let mut any_null = false;
                    for i in 0..nkeys {
                        key_vals[i] = buffer[keys[i].1];
                        if key_vals[i].is_null() {
                            any_null = true;
                            break;
                        }
                    }
                    if any_null {
                        return Ok(ControlFlow::Continue(()));
                    }
                    if n == entries.len() {
                        return Err(arena_full());
                    }
                    let hash = hash_key_collated(
                        &key_vals[..nkeys],
                        &hash_cols[..nkeys],
                        &key_collations[..nkeys],
                    );
                    let bucket = (hash as usize) & (buckets_len - 1);
                    entries[n] = HashEntry {
                        hash,
                        rowid: Some(rowid),
                        row: BoundRow::Encoded(bytes),
                    };
                    next[n] = buckets[bucket];
                    buckets[bucket] = n as u32;
                    n += 1;
                    Ok(ControlFlow::Continue(()))
                })?;
            }

            // Probe: scan the outer side once, resolving each row's key
            // against the build table. The outer side is iterated in the same
            // sorted order the nested loop uses (rowid/offset), so un-ORDER-BY'd
            // output matches byte-for-byte. Per-row decode and leaf-eval scratch
            // recycle; the build table (allocated above the mark) survives.
            let probe_slot = scope.slots[probe_t];
            let probe_def = scope.defs[probe_t].expect("resolved");
            let mut probe_schema = [ColType::Bool; MAX_COLUMNS];
            probe_def.schema(&mut probe_schema);
            let probe_schema = &probe_schema[..probe_def.n_columns];
            if let Some(demand) = pax_demand.selected_mask(probe_t)
                && storage.spill_rows_are_unshadowed(probe_slot)
            {
                let mut stopped = false;
                storage.for_each_spilled_row_batch(
                    probe_slot,
                    arena,
                    recycle_rows,
                    Some(demand),
                    &mut |rows| {
                        for spilled in rows {
                            check_timeout()?;
                            let crate::storage::SpilledRowRepresentation::Values(values) =
                                spilled.representation
                            else {
                                return Err(sql_err!(
                                    sqlstate::INTERNAL_ERROR,
                                    "PAX scan did not return selected values"
                                ));
                            };
                            let mut any_null = false;
                            for i in 0..nkeys {
                                key_vals[i] = values[keys[i].0];
                                if key_vals[i].is_null() {
                                    any_null = true;
                                    break;
                                }
                            }
                            let mut matched_any = false;
                            if !any_null {
                                let hash = hash_key_collated(
                                    &key_vals[..nkeys],
                                    &hash_cols[..nkeys],
                                    &key_collations[..nkeys],
                                );
                                let mut index = buckets[(hash as usize) & (buckets_len - 1)];
                                while index != u32::MAX {
                                    let entry = &entries[index as usize];
                                    if entry.hash == hash {
                                        let mut build_values = [Datum::Null; MAX_COLUMNS];
                                        match entry.row {
                                            BoundRow::Encoded(bytes) => rowenc::decode(
                                                bytes,
                                                build_schema,
                                                &mut build_values,
                                            )?,
                                            BoundRow::Values(row_values) => build_values
                                                [..row_values.len()]
                                                .copy_from_slice(row_values),
                                        }
                                        let mut keys_match = true;
                                        for key in 0..nkeys {
                                            if !compare_datums_collated(
                                                storage,
                                                key_collations[key],
                                                &build_values[keys[key].1],
                                                &key_vals[key],
                                            )?
                                            .is_eq()
                                            {
                                                keys_match = false;
                                                break;
                                            }
                                        }
                                        if keys_match {
                                            let mut bound = [None::<BoundRow>; 2];
                                            let mut rowids = [None; 2];
                                            bound[probe_t] = Some(BoundRow::Values(values));
                                            rowids[probe_t] = Some(spilled.rowid);
                                            bound[build_t] = Some(entry.row);
                                            rowids[build_t] = entry.rowid;
                                            let row = assemble(
                                                storage,
                                                txid,
                                                scope,
                                                &bound,
                                                &rowids,
                                                order,
                                                2,
                                                decode_buffers,
                                                arena,
                                            )?;
                                            if let Some(condition) = on {
                                                let chained = Chained { inner: &row, outer };
                                                if !matches!(
                                                    eval_full(
                                                        condition, arena, params, &chained, hooks
                                                    )?,
                                                    Datum::Bool(true)
                                                ) {
                                                    index = next[index as usize];
                                                    continue;
                                                }
                                            }
                                            matched_any = true;
                                            if let Some(predicate) = where_clause {
                                                let chained = Chained { inner: &row, outer };
                                                if !where_passes(
                                                    predicate, arena, params, &chained, hooks,
                                                )? {
                                                    index = next[index as usize];
                                                    continue;
                                                }
                                            }
                                            if !f(&row)? {
                                                stopped = true;
                                                return Ok(ControlFlow::Break(()));
                                            }
                                        }
                                    }
                                    index = next[index as usize];
                                }
                            }
                            if !matched_any && plan.preserves_probe_rows {
                                let mut bound = [None::<BoundRow>; 2];
                                let mut rowids = [None; 2];
                                bound[probe_t] = Some(BoundRow::Values(values));
                                rowids[probe_t] = Some(spilled.rowid);
                                let row = assemble(
                                    storage,
                                    txid,
                                    scope,
                                    &bound,
                                    &rowids,
                                    order,
                                    2,
                                    decode_buffers,
                                    arena,
                                )?;
                                let passes = if let Some(predicate) = where_clause {
                                    let chained = Chained { inner: &row, outer };
                                    where_passes(predicate, arena, params, &chained, hooks)?
                                } else {
                                    true
                                };
                                if passes && !f(&row)? {
                                    stopped = true;
                                    return Ok(ControlFlow::Break(()));
                                }
                            }
                        }
                        Ok(ControlFlow::Continue(()))
                    },
                )?;
                if stopped {
                    return Ok(true);
                }
                return Ok(true);
            }
            // Collect and sort the probe table's visible rows to match the
            // nested loop's output order.
            let probe_count = storage.visible_row_count(probe_slot, txid)?;
            let probe_ordered = arena
                .alloc_slice_with(probe_count.max(1), |_| {
                    (
                        0u64,
                        crate::storage::RowHome::Heap(crate::storage::RowLoc { offset: 0, len: 0 }),
                    )
                })
                .map_err(|_| arena_full())?;
            let mut probe_fill = 0usize;
            storage.for_each_row_state(probe_slot, &mut |rowid, state| {
                if let Some(home) = storage.visible_row_home(probe_slot, rowid, state, txid)? {
                    probe_ordered[probe_fill] = (rowid, home);
                    probe_fill += 1;
                }
                Ok(ControlFlow::Continue(()))
            })?;
            probe_ordered[..probe_fill].sort_unstable_by_key(|(rowid, home)| match home {
                crate::storage::RowHome::Spilled { .. } => (0u8, *rowid, 0u32),
                crate::storage::RowHome::Heap(loc) => (1u8, 0, loc.offset),
            });
            for &(rowid, home) in &probe_ordered[..probe_fill] {
                let keep = recycled(arena, recycle_rows, || -> Result<bool, SqlError> {
                    check_timeout()?;
                    let bytes = storage.row_bytes(probe_slot, rowid, home, arena)?;
                    let mut buffer = [Datum::Null; MAX_COLUMNS];
                    rowenc::decode(bytes, probe_schema, &mut buffer)?;
                    let mut any_null = false;
                    for i in 0..nkeys {
                        key_vals[i] = buffer[keys[i].0];
                        if key_vals[i].is_null() {
                            any_null = true;
                            break;
                        }
                    }
                    let mut matched_any = false;
                    if !any_null {
                        let hash = hash_key_collated(
                            &key_vals[..nkeys],
                            &hash_cols[..nkeys],
                            &key_collations[..nkeys],
                        );
                        let mut idx = buckets[(hash as usize) & (buckets_len - 1)];
                        while idx != u32::MAX {
                            let entry = &entries[idx as usize];
                            if entry.hash == hash {
                                let mut build_buf = [Datum::Null; MAX_COLUMNS];
                                match entry.row {
                                    BoundRow::Encoded(bytes) => {
                                        rowenc::decode(bytes, build_schema, &mut build_buf)?;
                                    }
                                    BoundRow::Values(values) => {
                                        build_buf[..values.len()].copy_from_slice(values);
                                    }
                                }
                                let mut matched = true;
                                for i in 0..nkeys {
                                    if !compare_datums_collated(
                                        storage,
                                        key_collations[i],
                                        &build_buf[keys[i].1],
                                        &key_vals[i],
                                    )?
                                    .is_eq()
                                    {
                                        matched = false;
                                        break;
                                    }
                                }
                                if matched {
                                    let bound = &mut [None::<BoundRow>, None];
                                    let bound_rowids = &mut [None, None];
                                    bound[probe_t] = Some(BoundRow::Encoded(bytes));
                                    bound_rowids[probe_t] = Some(rowid);
                                    bound[build_t] = Some(entry.row);
                                    bound_rowids[build_t] = entry.rowid;
                                    let row = assemble(
                                        storage,
                                        txid,
                                        scope,
                                        bound,
                                        bound_rowids,
                                        order,
                                        2,
                                        decode_buffers,
                                        arena,
                                    )?;
                                    if let Some(on) = on {
                                        let chained = Chained { inner: &row, outer };
                                        match eval_full(on, arena, params, &chained, hooks)? {
                                            Datum::Bool(true) => {}
                                            Datum::Bool(false) | Datum::Null => {
                                                idx = next[idx as usize];
                                                continue;
                                            }
                                            _ => {
                                                return Err(sql_err!(
                                                    sqlstate::DATATYPE_MISMATCH,
                                                    "argument of JOIN/ON must be type boolean"
                                                ));
                                            }
                                        }
                                    }
                                    matched_any = true;
                                    if let Some(w) = where_clause {
                                        let chained = Chained { inner: &row, outer };
                                        if !where_passes(w, arena, params, &chained, hooks)? {
                                            idx = next[idx as usize];
                                            continue;
                                        }
                                    }
                                    if !f(&row)? {
                                        return Ok(false);
                                    }
                                }
                            }
                            idx = next[idx as usize];
                        }
                    }
                    // LEFT JOIN: no ON-passing match (NULL key, or all candidates
                    // rejected) → preserve the outer row with NULLs for the inner.
                    if !matched_any && plan.preserves_probe_rows {
                        let bound = &mut [None::<BoundRow>, None];
                        let bound_rowids = &mut [None, None];
                        bound[probe_t] = Some(BoundRow::Encoded(bytes));
                        bound_rowids[probe_t] = Some(rowid);
                        let row = assemble(
                            storage,
                            txid,
                            scope,
                            bound,
                            bound_rowids,
                            order,
                            2,
                            decode_buffers,
                            arena,
                        )?;
                        if let Some(w) = where_clause {
                            let chained = Chained { inner: &row, outer };
                            if !where_passes(w, arena, params, &chained, hooks)? {
                                return Ok(true);
                            }
                        }
                        if !f(&row)? {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                })?;
                if !keep {
                    break;
                }
            }
            Ok(true)
        })()?;
        let _ = completed;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_passes<'a>(
        storage: &Storage,
        txid: u32,
        scope: &QueryScope<'a>,
        bound: &[Option<BoundRow<'a>>],
        bound_rowids: &[Option<u64>],
        order: &[usize],
        count: usize,
        on: Option<&Expr<'a>>,
        pushdown: &[&Expr<'a>],
        decode_buffers: &mut [[Datum<'a>; MAX_COLUMNS]],
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<bool, SqlError> {
        if let Some(on) = on {
            let row = assemble(
                storage,
                txid,
                scope,
                bound,
                bound_rowids,
                order,
                count,
                decode_buffers,
                arena,
            )?;
            let chained_row = Chained { inner: &row, outer };
            match eval_full(on, arena, params, &chained_row, hooks)? {
                Datum::Bool(true) => {}
                Datum::Bool(false) | Datum::Null => return Ok(false),
                _ => {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "argument of JOIN/ON must be type boolean"
                    ));
                }
            }
        }
        if pushdown.is_empty() {
            return Ok(true);
        }
        let row = assemble(
            storage,
            txid,
            scope,
            bound,
            bound_rowids,
            order,
            count,
            decode_buffers,
            arena,
        )?;
        let chained_row = Chained { inner: &row, outer };
        for &conjunct in pushdown {
            if !conjunct_passes(conjunct, arena, params, &chained_row, hooks)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Binds and checks one row before the recursive descent. Keeping this
    /// non-recursive half out of `level` bounds each recursive frame even
    /// though every source representation shares the same semantics.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn candidate_matches<'a>(
        storage: &'a Storage,
        txid: u32,
        scope: &QueryScope<'a>,
        arena: &'a Arena,
        params: &[Datum<'a>],
        hooks: &EvalHooks<'_, 'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
        depth: usize,
        index: usize,
        candidate: BoundRow<'a>,
        rowid: Option<u64>,
        bound: &mut [Option<BoundRow<'a>>],
        bound_rowids: &mut [Option<u64>],
        matched: &[Option<&[core::cell::Cell<bool>]>],
        external_match_writer: Option<core::ptr::NonNull<crate::sql::external::ExternalSorter>>,
        pushdown: &[&[&'a Expr<'a>]],
        order: &[usize],
        decode_buffers: &mut [[Datum<'a>; MAX_COLUMNS]],
        on: Option<&'a Expr<'a>>,
    ) -> Result<bool, SqlError> {
        bound[order[depth]] = Some(candidate);
        bound_rowids[order[depth]] = rowid;
        if !candidate_passes(
            storage,
            txid,
            scope,
            bound,
            bound_rowids,
            order,
            depth + 1,
            on,
            pushdown[depth],
            decode_buffers,
            arena,
            params,
            hooks,
            outer,
        )? {
            return Ok(false);
        }
        if let Some(matched_rows) = matched[depth] {
            matched_rows[index].set(true);
        }
        record_external_match(storage, external_match_writer, depth, index)?;
        Ok(true)
    }

    // Recursive join state is small; product-sized decode scratch is owned by
    // the statement arena and passed through each level.
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
        bound: &mut [Option<BoundRow<'a>>],
        bound_rowids: &mut [Option<u64>],
        // For each RIGHT/FULL join level, one flag per scanned row of that
        // level's table, marking those that found a left partner.
        matched: &[Option<&[core::cell::Cell<bool>]>],
        external_match_writer: Option<core::ptr::NonNull<crate::sql::external::ExternalSorter>>,
        // Error-safe WHERE conjuncts to check at each depth (predicate pushdown).
        pushdown: &[&[&'a Expr<'a>]],
        // Execution order: `order[depth]` is the scope-table joined at this depth
        // (identity unless a cross join was cost-reordered).
        order: &[usize],
        indexed: Option<&IndexedCandidates<'a>>,
        decode_buffers: &mut [[Datum<'a>; MAX_COLUMNS]],
        recycle_rows: bool,
        pax_demand: PaxReadDemand,
        f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>,
    ) -> Result<bool, SqlError> {
        if depth == scope.n {
            let row = assemble(
                storage,
                txid,
                scope,
                bound,
                bound_rowids,
                order,
                depth,
                decode_buffers,
                arena,
            )?;
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
        // USING/NATURAL predicates are synthesized at plan time.
        let on = join.and_then(|join| join.on.or(scope.join_on[depth - 1]));
        let mut matched_any = false;
        // Every source representation expands the recursive call at this site;
        // `candidate_matches` returns before that descent, keeping the
        // wide-range-table recursion to one live frame per join edge.
        macro_rules! visit_candidate {
            ($index:expr, $candidate:expr, $rowid:expr) => {{
                if !candidate_matches(
                    storage,
                    txid,
                    scope,
                    arena,
                    params,
                    hooks,
                    outer,
                    depth,
                    $index,
                    $candidate,
                    $rowid,
                    bound,
                    bound_rowids,
                    matched,
                    external_match_writer,
                    pushdown,
                    order,
                    decode_buffers,
                    on,
                )? {
                    Ok(true)
                } else {
                    matched_any = true;
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
                        depth + 1,
                        bound,
                        bound_rowids,
                        matched,
                        external_match_writer,
                        pushdown,
                        order,
                        indexed,
                        decode_buffers,
                        recycle_rows,
                        pax_demand,
                        f,
                    )
                }
            }};
        }
        macro_rules! visit_spilled_rows {
            ($slot:expr, $index:ident, $aborted:ident) => {{
                storage.for_each_spilled_row_batch(
                    $slot,
                    arena,
                    recycle_rows,
                    pax_demand.selected_mask(order[depth]),
                    &mut |rows| {
                        for spilled in rows {
                            check_timeout()?;
                            let this = $index;
                            $index += 1;
                            let keep_scanning = recycled(arena, recycle_rows, || {
                                visit_candidate!(
                                    this,
                                    match spilled.representation {
                                        crate::storage::SpilledRowRepresentation::Encoded(
                                            bytes,
                                        ) => BoundRow::Encoded(bytes),
                                        crate::storage::SpilledRowRepresentation::Values(
                                            values,
                                        ) => BoundRow::Values(values),
                                    },
                                    Some(spilled.rowid)
                                )
                            })?;
                            if !keep_scanning {
                                $aborted = true;
                                return Ok(core::ops::ControlFlow::Break(()));
                            }
                        }
                        Ok(core::ops::ControlFlow::Continue(()))
                    },
                )?;
            }};
        }
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
            let outer_row = assemble(
                storage,
                txid,
                scope,
                bound,
                bound_rowids,
                order,
                depth,
                decode_buffers,
                arena,
            )?;
            let chained = Chained {
                inner: &outer_row,
                outer,
            };
            match materialize_lateral_source(
                storage,
                txid,
                tref,
                scope.defs[t].expect("resolved").n_columns,
                arena,
                params,
                &chained,
            )? {
                LateralRows::External(run) => {
                    if let Some(run) = run
                        && !consume_external_lateral_run(
                            storage,
                            run,
                            arena,
                            recycle_rows,
                            &mut |index, bytes| {
                                visit_candidate!(index, BoundRow::Encoded(bytes), None)
                            },
                        )?
                    {
                        return Ok(false);
                    }
                }
                LateralRows::Local(rows) => {
                    for (index, bytes) in rows.iter().enumerate() {
                        check_timeout()?;
                        let keep_scanning = recycled(arena, recycle_rows, || {
                            visit_candidate!(index, BoundRow::Encoded(bytes), None)
                        })?;
                        if !keep_scanning {
                            return Ok(false);
                        }
                    }
                }
            }
        } else if let Some(run) = scope.external_runs[order[depth]] {
            let mut reader = storage.external_run_reader()?;
            let mut index = 0usize;
            storage
                .with_block_store(|blocks| reader.start(blocks, run))
                .expect("external run has a block store")?;
            loop {
                let keep_scanning = {
                    let Some(bytes) = reader.row() else { break };
                    check_timeout()?;
                    let this = index;
                    index += 1;
                    recycled(arena, recycle_rows, || {
                        // The cursor replaces its row buffer on advance, so
                        // deeper join levels retain this row in recycling
                        // statement storage only for the callback's lifetime.
                        let owned = arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
                        visit_candidate!(this, BoundRow::Encoded(owned), None)
                    })
                }?;
                if !keep_scanning {
                    return Ok(false);
                }
                storage
                    .with_block_store(|blocks| reader.advance(blocks))
                    .expect("external run has a block store")?;
            }
        } else if let Some(rows) = scope.derived[order[depth]] {
            for (index, bytes) in rows.iter().enumerate() {
                check_timeout()?;
                let keep_scanning = recycled(arena, recycle_rows, || {
                    visit_candidate!(index, BoundRow::Encoded(bytes), None)
                })?;
                if !keep_scanning {
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
            let candidates = indexed
                .filter(|access| access.table == order[depth])
                .map(|access| access.rowids);
            // A cold, overlay-free table is already being merged in SST data
            // blocks. Carry the selected entry bytes out of that cursor rather
            // than point-reading every row a second time. Any resident overlay
            // stays on the general row-state path below, which owns its MVCC
            // shadowing and mixed heap/SST physical order.
            if candidates.is_none() && storage.spill_rows_are_unshadowed(slot) {
                let mut index = 0usize;
                let mut aborted = false;
                visit_spilled_rows!(slot, index, aborted);
                if aborted {
                    return Ok(false);
                }
                return Ok(true);
            }
            let count = candidates
                .map(<[u64]>::len)
                .unwrap_or(storage.visible_row_count(slot, txid)?);
            let ordered = arena
                .alloc_slice_with(count, |_| {
                    (
                        0u64,
                        crate::storage::RowHome::Heap(crate::storage::RowLoc { offset: 0, len: 0 }),
                    )
                })
                .map_err(|_| arena_full())?;
            let mut fill = 0usize;
            if let Some(rowids) = candidates {
                for &rowid in rowids {
                    let Some(state) = storage.row_state(slot, rowid)? else {
                        continue;
                    };
                    if let Some(home) = storage.visible_row_home(slot, rowid, state, txid)? {
                        ordered[fill] = (rowid, home);
                        fill += 1;
                    }
                }
            } else {
                storage.for_each_row_state(slot, &mut |rowid, state| {
                    if let Some(home) = storage.visible_row_home(slot, rowid, state, txid)? {
                        ordered[fill] = (rowid, home);
                        fill += 1;
                    }
                    Ok(core::ops::ControlFlow::Continue(()))
                })?;
                debug_assert_eq!(fill, count, "visible count is stable");
            }
            // Spilled rows sort by rowid (their SST order — the physical order
            // they were written in); heap rows keep heap-offset order after
            // them, matching insertion order within each group.
            ordered[..fill].sort_unstable_by_key(|(rowid, home)| match home {
                crate::storage::RowHome::Spilled { .. } => (0u8, *rowid, 0u32),
                crate::storage::RowHome::Heap(loc) => (1u8, 0, loc.offset),
            });
            for (this, &(rowid, home)) in ordered[..fill].iter().enumerate() {
                check_timeout()?;
                let keep_scanning = recycled(arena, recycle_rows, || {
                    let bytes = storage.row_bytes(scope.slots[order[depth]], rowid, home, arena)?;
                    visit_candidate!(this, BoundRow::Encoded(bytes), Some(rowid))
                })?;
                if !keep_scanning {
                    return Ok(false);
                }
            }
        } else {
            let slot = scope.slots[order[depth]];
            let mut index = 0usize;
            let mut aborted = false;
            if storage.spill_rows_are_unshadowed(slot) {
                visit_spilled_rows!(slot, index, aborted);
            } else {
                storage.for_each_row_state(slot, &mut |rowid, state| {
                    use core::ops::ControlFlow;
                    check_timeout()?;
                    let Some(home) = storage.visible_row_home(slot, rowid, state, txid)? else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    let this = index;
                    index += 1;
                    let keep_scanning = recycled(arena, recycle_rows, || {
                        let bytes = storage.row_bytes(slot, rowid, home, arena)?;
                        visit_candidate!(this, BoundRow::Encoded(bytes), Some(rowid))
                    })?;
                    if !keep_scanning {
                        aborted = true;
                        return Ok(ControlFlow::Break(()));
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
            }
            if aborted {
                return Ok(false);
            }
        }
        // LEFT/FULL join with no match at this level: emit one null row (the
        // left side preserved, this table nulled).
        if !matched_any && join.is_some_and(|j| matches!(j.kind, JoinKind::Left | JoinKind::Full)) {
            bound[order[depth]] = None;
            bound_rowids[order[depth]] = None;
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
                bound_rowids,
                matched,
                external_match_writer,
                pushdown,
                order,
                indexed,
                decode_buffers,
                recycle_rows,
                pax_demand,
                f,
            )? {
                return Ok(false);
            }
        }
        bound[order[depth]] = None;
        bound_rowids[order[depth]] = None;
        Ok(true)
    }

    // For every RIGHT/FULL join level, one match flag per row of that
    // level's table (arena-backed, so no post-init allocation). An unmatched
    // row null-pads the tables to its left and still joins the tables to its
    // right (post-passes below, shallowest level first — so a deeper level's
    // flags also accumulate matches found during a shallower post-pass).
    let matched = arena
        .alloc_slice_with(scope.n, |_| None)
        .map_err(|_| arena_full())?;
    let external_match_map = storage.spill_attached()
        && from
            .joins
            .iter()
            .any(|join| matches!(join.kind, JoinKind::Right | JoinKind::Full));
    for (i, j) in from.joins.iter().enumerate() {
        if !matches!(j.kind, JoinKind::Right | JoinKind::Full) {
            continue;
        }
        if external_match_map {
            continue;
        }
        let t = i + 1;
        let n_rows = if let Some(run) = scope.external_runs[t] {
            usize::try_from(run.rows()).map_err(|_| arena_full())?
        } else if let Some(rows) = scope.derived[t] {
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
    let order = arena
        .alloc_slice_with(scope.n, |index| index)
        .map_err(|_| arena_full())?;
    if all_cross && !any_lateral {
        fill_join_order(storage, scope, planning_where_clause, order);
    }
    let inv_order = arena
        .alloc_slice_with(scope.n, |_| 0usize)
        .map_err(|_| arena_full())?;
    for (pos, &t) in order.iter().enumerate() {
        inv_order[t] = pos;
    }
    let pushdown_buffers = arena
        .alloc_slice_with(scope.n, |_| [&Expr::Null; MAX_CONJUNCTS])
        .map_err(|_| arena_full())?;
    let pushdown_counts = arena
        .alloc_slice_with(scope.n, |_| 0usize)
        .map_err(|_| arena_full())?;
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
                if d < scope.n && pushdown_counts[d] < MAX_CONJUNCTS {
                    pushdown_buffers[d][pushdown_counts[d]] = c;
                    pushdown_counts[d] += 1;
                }
            }
        }
    }
    let pushdown = arena
        .alloc_slice_with(scope.n, |depth| {
            &pushdown_buffers[depth][..pushdown_counts[depth]]
        })
        .map_err(|_| arena_full())?;

    let indexed = indexed_candidates(storage, scope, txid, where_clause, arena, params, hooks)?;
    let bound = arena
        .alloc_slice_with(scope.n, |_| None)
        .map_err(|_| arena_full())?;
    let bound_rowids = arena
        .alloc_slice_with(scope.n, |_| None)
        .map_err(|_| arena_full())?;
    let decode_buffers = arena
        .alloc_slice_with(scope.n.max(1), |_| [Datum::Null; MAX_COLUMNS])
        .map_err(|_| arena_full())?;
    let hash_plan = select_hash_join_plan(storage, scope, from, planning_where_clause, order)?;
    if let Some(hash_plan) = hash_plan {
        execute_hash_join_plan(
            storage,
            scope,
            txid,
            where_clause,
            arena,
            params,
            hooks,
            outer,
            order,
            hash_plan,
            decode_buffers,
            recycle_rows,
            pax_demand,
            f,
        )?;
        return Ok(());
    }
    let mut match_run = None;
    let mut initial_match_sorter = if external_match_map {
        let mut sorter = storage.external_sorter()?;
        sorter.reset();
        Some(sorter)
    } else {
        None
    };
    {
        let external_match_writer = initial_match_sorter
            .as_deref_mut()
            .map(|sorter| core::ptr::NonNull::from(&mut **sorter));
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
            bound,
            bound_rowids,
            matched,
            external_match_writer,
            pushdown,
            order,
            indexed.as_ref(),
            decode_buffers,
            recycle_rows,
            pax_demand,
            f,
        )?;
    }
    if let Some(mut sorter) = initial_match_sorter {
        let mut compare = compare_external_matches;
        match_run = storage
            .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
            .expect("external match map has a block store")?;
    }

    // RIGHT/FULL post-passes, shallowest level first: each unmatched row of
    // that level's table binds with every table to its left nulled and then
    // joins the deeper tables normally (so its own matches mark deeper
    // levels' flags before those levels' post-passes run).
    for d in 1..scope.n {
        if !matches!(from.joins[d - 1].kind, JoinKind::Right | JoinKind::Full) {
            continue;
        }
        let local_matches = matched[d];
        let mut phase_match_sorter = if external_match_map {
            let mut sorter = storage.external_sorter()?;
            sorter.reset();
            Some(sorter)
        } else {
            None
        };
        {
            let external_match_writer = phase_match_sorter
                .as_deref_mut()
                .map(|sorter| core::ptr::NonNull::from(&mut **sorter));
            let mut external_match_reader = if external_match_map && match_run.is_some() {
                let mut reader = storage.external_run_reader()?;
                storage
                    .with_block_store(|blocks| reader.start(blocks, match_run.expect("checked")))
                    .expect("external match map has a block store")?;
                Some(reader)
            } else {
                None
            };
            let mut emit_unmatched =
                |candidate: BoundRow<'a>,
                 rowid: Option<u64>,
                 f: &mut dyn FnMut(&JoinRow<'_, 'a, '_>) -> Result<bool, SqlError>|
                 -> Result<bool, SqlError> {
                    bound.fill(None);
                    bound_rowids.fill(None);
                    bound[d] = Some(candidate);
                    bound_rowids[d] = rowid;
                    if d + 1 == scope.n {
                        // Last level: the row is complete once the left side nulls.
                        let row = assemble(
                            storage,
                            txid,
                            scope,
                            bound,
                            bound_rowids,
                            order,
                            scope.n,
                            decode_buffers,
                            arena,
                        )?;
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
                        bound,
                        bound_rowids,
                        matched,
                        external_match_writer,
                        pushdown,
                        order,
                        indexed.as_ref(),
                        decode_buffers,
                        recycle_rows,
                        pax_demand,
                        f,
                    )
                };
            if let Some(run) = scope.external_runs[d] {
                let mut reader = storage.external_run_reader()?;
                let mut index = 0usize;
                storage
                    .with_block_store(|blocks| reader.start(blocks, run))
                    .expect("external run has a block store")?;
                loop {
                    let keep_scanning = {
                        let Some(bytes) = reader.row() else { break };
                        let this = index;
                        index += 1;
                        recycled(arena, recycle_rows, || {
                            let already_matched = if external_match_map {
                                match external_match_reader.as_deref_mut() {
                                    Some(reader) => {
                                        external_match_contains(storage, reader, d, this)?
                                    }
                                    None => false,
                                }
                            } else {
                                local_matches.expect("local match map")[this].get()
                            };
                            if already_matched {
                                Ok(true)
                            } else {
                                let owned =
                                    arena.alloc_slice_copy(bytes).map_err(|_| arena_full())?;
                                emit_unmatched(BoundRow::Encoded(owned), None, f)
                            }
                        })
                    }?;
                    if !keep_scanning {
                        return Ok(());
                    }
                    storage
                        .with_block_store(|blocks| reader.advance(blocks))
                        .expect("external run has a block store")?;
                }
            } else if let Some(rows) = scope.derived[d] {
                for (index, bytes) in rows.iter().enumerate() {
                    let keep_scanning = recycled(arena, recycle_rows, || {
                        let already_matched = if external_match_map {
                            match external_match_reader.as_deref_mut() {
                                Some(reader) => external_match_contains(storage, reader, d, index)?,
                                None => false,
                            }
                        } else {
                            local_matches.expect("local match map")[index].get()
                        };
                        if already_matched {
                            Ok(true)
                        } else {
                            emit_unmatched(BoundRow::Encoded(bytes), None, f)
                        }
                    })?;
                    if !keep_scanning {
                        return Ok(());
                    }
                }
            } else if let Some(demand) = pax_demand.selected_mask(d)
                && storage.spill_rows_are_unshadowed(scope.slots[d])
            {
                let mut index = 0usize;
                let mut done = false;
                storage.for_each_spilled_row_batch(
                    scope.slots[d],
                    arena,
                    recycle_rows,
                    Some(demand),
                    &mut |rows| {
                        for spilled in rows {
                            let this = index;
                            index += 1;
                            let keep_scanning = recycled(arena, recycle_rows, || {
                                let already_matched = if external_match_map {
                                    match external_match_reader.as_deref_mut() {
                                        Some(reader) => {
                                            external_match_contains(storage, reader, d, this)?
                                        }
                                        None => false,
                                    }
                                } else {
                                    local_matches.expect("local match map")[this].get()
                                };
                                if already_matched {
                                    Ok(true)
                                } else {
                                    emit_unmatched(
                                        match spilled.representation {
                                            crate::storage::SpilledRowRepresentation::Encoded(
                                                bytes,
                                            ) => BoundRow::Encoded(bytes),
                                            crate::storage::SpilledRowRepresentation::Values(
                                                values,
                                            ) => BoundRow::Values(values),
                                        },
                                        Some(spilled.rowid),
                                        f,
                                    )
                                }
                            })?;
                            if !keep_scanning {
                                done = true;
                                return Ok(core::ops::ControlFlow::Break(()));
                            }
                        }
                        Ok(core::ops::ControlFlow::Continue(()))
                    },
                )?;
                if done {
                    return Ok(());
                }
            } else {
                let mut index = 0usize;
                let mut done = false;
                storage.for_each_row_state(scope.slots[d], &mut |rowid, state| {
                    use core::ops::ControlFlow;
                    let Some(home) =
                        storage.visible_row_home(scope.slots[d], rowid, state, txid)?
                    else {
                        return Ok(ControlFlow::Continue(()));
                    };
                    let this = index;
                    index += 1;
                    let keep_scanning = recycled(arena, recycle_rows, || {
                        let already_matched = if external_match_map {
                            match external_match_reader.as_deref_mut() {
                                Some(reader) => external_match_contains(storage, reader, d, this)?,
                                None => false,
                            }
                        } else {
                            local_matches.expect("local match map")[this].get()
                        };
                        if already_matched {
                            Ok(true)
                        } else {
                            let bytes = storage.row_bytes(scope.slots[d], rowid, home, arena)?;
                            emit_unmatched(BoundRow::Encoded(bytes), Some(rowid), f)
                        }
                    })?;
                    if !keep_scanning {
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
        if let Some(mut sorter) = phase_match_sorter {
            let mut compare = compare_external_matches;
            let phase_run = storage
                .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
                .expect("external match map has a block store")?;
            drop(sorter);
            match_run = combine_external_match_runs(storage, match_run, phase_run)?;
        }
    }
    Ok(())
}
