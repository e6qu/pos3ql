//! The expression keys of a table's indexes that ANALYZE gathers statistics
//! for (`compute_index_stats`): each key expression and partial index
//! predicate bound to the table, evaluated over the sampled rows.

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::eval::{EvalHooks, SqlError, eval_full};
use crate::sql::types::Datum;
use crate::storage::{IndexExpressionSampler, SampledIndexKey, Storage, TableDef};

use super::RowCtx;

/// Expression key `position` of `index` on the table `definition`, its
/// stored text `text`, bound to the table, with its type; `None` for a key
/// of a type the catalog defines (an enum, a domain or a composite), which
/// keeps no statistics here.
pub(crate) fn expression_key<'w>(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    index: &crate::storage::IndexDef,
    position: usize,
    text: &str,
    arena: &'w Arena,
) -> Result<Option<(&'w Expr<'w>, crate::sql::types::ColType)>, SqlError> {
    let expression = super::expression_bindings::bound_index_expression(
        text,
        storage,
        index,
        crate::storage::ExpressionOwner::IndexKey(position as u8),
        txid,
        arena,
    )?;
    let (type_oid, _) =
        super::infer_type_catalog(expression, Some(definition), storage, txid, arena)?;
    Ok(match super::catalog_column_type(storage, txid, type_oid) {
        Some((ctype, None)) => Some((expression, ctype)),
        _ => None,
    })
}

/// One index with expression keys.
#[derive(Clone, Copy)]
struct SampledIndex<'w> {
    stamp: u64,
    keys: &'w [SampledIndexKey],
    /// Each key's bound expression, in `keys` order.
    expressions: &'w [&'w Expr<'w>],
    predicate: Option<&'w Expr<'w>>,
}

/// The expression indexes of one table, bound in the statement arena.
pub(crate) struct AnalyzedIndexes<'w> {
    definition: TableDef,
    txid: u32,
    indexes: &'w [SampledIndex<'w>],
    arena: &'w Arena,
}

impl<'w> AnalyzedIndexes<'w> {
    /// The indexes of the table in `slot` that have expression keys, as
    /// ANALYZE of the whole table samples them; a partitioned table's
    /// indexes hold no rows of their own and are left out.
    pub(crate) fn of_table(
        storage: &Storage,
        slot: usize,
        txid: u32,
        default_target: usize,
        arena: &'w Arena,
    ) -> Result<Self, SqlError> {
        let arena_full = crate::sql::eval::arena_full;
        let definition = (*storage.table_def(slot, txid)).clone();
        let partitioned = definition.partition.is_partitioned();
        let count = if partitioned {
            0
        } else {
            storage
                .indexes_for_slot(slot, txid)
                .filter(|index| index.expressions[..index.n_cols].iter().any(Option::is_some))
                .count()
        };
        let indexes = arena
            .alloc_slice_with(count, |_| SampledIndex {
                stamp: 0,
                keys: &[],
                expressions: &[],
                predicate: None,
            })
            .map_err(|_| arena_full())?;
        let mut filled = 0;
        for index in storage.indexes_for_slot(slot, txid) {
            if filled == count {
                break;
            }
            let expression_keys = index.expressions[..index.n_cols]
                .iter()
                .filter(|expression| expression.is_some())
                .count();
            if expression_keys == 0 {
                continue;
            }
            let keys = arena
                .alloc_slice_with(expression_keys, |_| SampledIndexKey {
                    attribute: 0,
                    ctype: crate::sql::types::ColType::Bool,
                    collation: crate::sql::ast::Collation::None,
                    target: default_target,
                })
                .map_err(|_| arena_full())?;
            let expressions = arena
                .alloc_slice_with(expression_keys, |_| &Expr::Null)
                .map_err(|_| arena_full())?;
            let mut key = 0;
            for (position, text) in index.expressions[..index.n_cols].iter().enumerate() {
                let Some(text) = text else {
                    continue;
                };
                let Some((expression, ctype)) =
                    expression_key(
                    storage,
                    txid,
                    &definition,
                    &index,
                    position,
                    text.as_str(),
                    arena,
                )?
                else {
                    continue;
                };
                keys[key] = SampledIndexKey {
                    attribute: position as u16 + 1,
                    ctype,
                    collation: index.collations[position],
                    target: default_target,
                };
                expressions[key] = expression;
                key += 1;
            }
            let predicate = index
                .predicate
                .map(|text| {
                    super::expression_bindings::bound_index_expression(
                        text.as_str(),
                        storage,
                        &index,
                        crate::storage::ExpressionOwner::IndexPredicate,
                        txid,
                        arena,
                    )
                })
                .transpose()?;
            indexes[filled] = SampledIndex {
                stamp: index.created_at,
                keys: &keys[..key],
                expressions: &expressions[..key],
                predicate,
            };
            filled += 1;
        }
        Ok(Self {
            definition,
            txid,
            indexes: &indexes[..filled],
            arena,
        })
    }

    fn value(&self, storage: &Storage, expression: &Expr<'w>, row: &[Datum<'w>]) -> Result<Datum<'w>, SqlError> {
        let catalog = crate::sql::query::storage_catalog(storage, self.arena, self.txid);
        let hooks = EvalHooks {
            catalog: Some(&catalog),
            ..crate::sql::eval::NO_HOOKS
        };
        eval_full(
            expression,
            self.arena,
            crate::sql::eval::NO_PARAMS,
            &RowCtx {
                table_oid: None,
                def: crate::storage::RelationDefinition::from_definition(&self.definition),
                values: row,
                alias: None,
            },
            &hooks,
        )
    }
}

impl<'w> IndexExpressionSampler<'w> for AnalyzedIndexes<'w> {
    fn len(&self) -> usize {
        self.indexes.len()
    }

    fn stamp(&self, index: usize) -> u64 {
        self.indexes[index].stamp
    }

    fn keys(&self, index: usize) -> &[SampledIndexKey] {
        self.indexes[index].keys
    }

    fn partial(&self, index: usize) -> bool {
        self.indexes[index].predicate.is_some()
    }

    fn passes(
        &self,
        storage: &Storage,
        index: usize,
        row: &[Datum<'w>],
    ) -> Result<bool, SqlError> {
        let Some(predicate) = self.indexes[index].predicate else {
            return Ok(true);
        };
        Ok(matches!(self.value(storage, predicate, row)?, Datum::Bool(true)))
    }

    fn evaluate(
        &self,
        storage: &Storage,
        index: usize,
        row: &[Datum<'w>],
        out: &mut [Datum<'w>],
    ) -> Result<(), SqlError> {
        for (slot, expression) in out.iter_mut().zip(self.indexes[index].expressions) {
            *slot = self.value(storage, expression, row)?;
        }
        Ok(())
    }
}
