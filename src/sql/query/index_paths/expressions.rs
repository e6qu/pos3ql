//! Index key expressions read as variables of their own, as
//! `examine_variable` matches an expression with an index's key expression:
//! a condition or `ORDER BY` key on `lower(b)` reads the statistics ANALYZE
//! gathered for the expression of an index on `lower(b)`, and an index scan
//! applies it.
//!
//! Each distinct key expression of the table's btree indexes, newest index
//! first and keys in order, is a variable numbered after the table's
//! columns. An expression bound to other objects (a user-defined routine or
//! type) is no variable; its index stays outside the model.

use crate::mem::arena::Arena;
use crate::sql::ast::{Collation, Expr};
use crate::sql::eval::SqlError;
use crate::sql::types::{ColType, Datum};
use crate::storage::{Storage, TableDef};

use super::super::scope::{QueryScope, ResolvedColumn};

/// The most distinct key expressions the model reads on one table; an index
/// with more is outside it.
pub(super) const MAX_EXPRESSION_VARIABLES: usize = 32;

/// One index key expression.
#[derive(Clone, Copy)]
pub(crate) struct ExpressionVariable<'a> {
    /// The expression with every column named, unqualified, by its table
    /// column's name: the form two equal expressions share.
    pub(super) canonical: &'a Expr<'a>,
    /// The expression as the index evaluates it.
    pub(crate) bound: &'a Expr<'a>,
    pub(crate) ctype: ColType,
    pub(crate) collation: Collation,
    /// The index and key position whose statistics describe it.
    pub(super) statistics: Option<(u64, u16)>,
    /// A unique whole-table index has it as its only key.
    pub(super) unique: bool,
    /// A whole-table btree leads with it.
    pub(super) indexed: bool,
}

/// The table's key expressions, in variable order.
#[derive(Clone, Copy)]
pub(crate) struct ExpressionVariables<'a> {
    /// The first variable's number: the table's column count.
    pub(crate) first: usize,
    pub(crate) variables: &'a [ExpressionVariable<'a>],
}

impl<'a> ExpressionVariables<'a> {
    /// The variable numbered `column`, when it is no table column.
    pub(crate) fn get(&self, column: usize) -> Option<&ExpressionVariable<'a>> {
        column
            .checked_sub(self.first)
            .and_then(|at| self.variables.get(at))
    }

    /// The variable `expression` (a query's, read in `scope`) is.
    pub(crate) fn find(
        &self,
        scope: &QueryScope<'_>,
        expression: &'a Expr<'a>,
        arena: &'a Arena,
    ) -> Result<Option<usize>, SqlError> {
        if self.variables.is_empty() || matches!(expression, Expr::Column { .. }) {
            return Ok(None);
        }
        let Some(definition) = scope.defs.stored(0) else {
            return Ok(None);
        };
        let resolve = |qualifier: Option<&str>, name: &str| match scope.find_column(qualifier, name)
        {
            Ok(ResolvedColumn::Table(0, column)) => Some(column),
            _ => None,
        };
        let Some(canonical) = canonical(expression, &resolve, definition, arena)? else {
            return Ok(None);
        };
        Ok(self
            .variables
            .iter()
            .position(|variable| *variable.canonical == *canonical)
            .map(|at| self.first + at))
    }
}

/// `expression` with each column it reads named by `definition`'s column
/// name, unqualified; `None` when a column is no column of the table, or
/// the expression reads a whole row or a subquery.
pub(super) fn canonical<'a>(
    expression: &'a Expr<'a>,
    resolve: &dyn Fn(Option<&str>, &str) -> Option<usize>,
    definition: &TableDef,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    struct Failure(SqlError);
    impl From<crate::mem::arena::ArenaFull> for Failure {
        fn from(_: crate::mem::arena::ArenaFull) -> Self {
            Self(crate::sql::eval::arena_full())
        }
    }
    if expression.contains_subquery() {
        return Ok(None);
    }
    let mut foreign = false;
    let rewritten = crate::sql::ast::rewrite_expression(
        expression,
        arena,
        &mut |node: &'a Expr<'a>| -> Result<Option<&'a Expr<'a>>, Failure> {
            match node {
                Expr::Column { qualifier, name } => {
                    let Some(column) = resolve(*qualifier, name) else {
                        foreign = true;
                        return Ok(None);
                    };
                    let name = arena.alloc_str(definition.columns()[column].name.as_str())?;
                    Ok(Some(arena.alloc(Expr::Column {
                        qualifier: None,
                        name,
                    })?))
                }
                Expr::WholeRow(_) | Expr::Param(_) | Expr::RoutineParam { .. } => {
                    foreign = true;
                    Ok(None)
                }
                _ => Ok(None),
            }
        },
    )
    .map_err(|Failure(error)| error)?;
    Ok((!foreign).then_some(rewritten))
}

/// The key expressions of the btree indexes `catalog` lists (newest first)
/// of the table in `slot`, whose definition is `definition` and statistics
/// `statistics`, numbered after its columns. Each expression key of
/// `catalog` is renumbered to its variable; the returned mask has a bit for
/// each index with an expression key the model does not read.
pub(crate) fn table_expressions<'a>(
    storage: &Storage,
    txid: u32,
    slot: usize,
    definition: &TableDef,
    statistics: &crate::storage::TableStatistics,
    catalog: &mut [crate::sql::catalog::PlannerIndex],
    arena: &'a Arena,
) -> Result<(ExpressionVariables<'a>, u64), SqlError> {
    let mut held = [None::<ExpressionVariable<'a>>; MAX_EXPRESSION_VARIABLES];
    // The variables whose statistics an earlier index settled.
    let mut settled = [false; MAX_EXPRESSION_VARIABLES];
    let mut count = 0;
    let mut unread = 0u64;
    let first = definition.columns().len();
    for (at, planned) in catalog.iter_mut().enumerate() {
        if !planned.expression_keys[..planned.n_cols]
            .iter()
            .any(|&key| key)
        {
            continue;
        }
        let Some(index) = storage
            .indexes_for_slot(slot, txid)
            .find(|index| index.name_for(txid).as_str() == planned.name.as_str())
        else {
            unread |= 1 << at.min(63);
            continue;
        };
        let btree = index.method == crate::sql::ast::IndexAccessMethod::Btree;
        for position in 0..planned.n_cols {
            if !planned.expression_keys[position] {
                continue;
            }
            let Some(variable) = key_variable(storage, txid, definition, &index, position, arena)?
            else {
                unread |= 1 << at.min(63);
                continue;
            };
            let found = held[..count]
                .iter()
                .flatten()
                .position(|other| *other.canonical == *variable.canonical);
            let number = match found {
                Some(number) => number,
                None if count == MAX_EXPRESSION_VARIABLES => {
                    unread |= 1 << at.min(63);
                    continue;
                }
                None => {
                    held[count] = Some(variable);
                    count += 1;
                    count - 1
                }
            };
            let Ok(column) = u16::try_from(first + number) else {
                unread |= 1 << at.min(63);
                continue;
            };
            planned.columns[position] = column;
            let variable = held[number].as_mut().expect("held variable");
            // `get_actual_variable_range` reads a whole-table btree led by
            // the expression.
            variable.indexed |= btree && position == 0 && !planned.partial;
            if settled[number] {
                continue;
            }
            // `examine_variable`: a unique index of the one key, then the
            // first whole-table index holding statistics of the expression.
            variable.unique |= planned.unique && planned.n_cols == 1 && !planned.partial;
            if !planned.partial
                && statistics
                    .index_column(index.created_at, position as u16 + 1)
                    .is_some()
            {
                variable.statistics = Some((index.created_at, position as u16 + 1));
                settled[number] = true;
            }
        }
    }
    let variables = arena
        .alloc_slice_with(count, |at| held[at].expect("dense variables"))
        .map_err(|_| crate::sql::eval::arena_full())?;
    Ok((ExpressionVariables { first, variables }, unread))
}

/// Key `position` of `index` as a variable: `None` for a plain column, or
/// an expression the model does not read (bound to a user-defined object).
pub(crate) fn key_variable<'a>(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    index: &crate::storage::IndexDef,
    position: usize,
    arena: &'a Arena,
) -> Result<Option<ExpressionVariable<'a>>, SqlError> {
    let Some(text) = index.expressions[position] else {
        return Ok(None);
    };
    let Some((bound, ctype)) = crate::sql::exec::analyze_indexes::expression_key(
        storage,
        txid,
        definition,
        index,
        position,
        text.as_str(),
        arena,
    )?
    else {
        return Ok(None);
    };
    let bound = if index
        .expression_dependencies_of(crate::storage::ExpressionOwner::IndexKey(position as u8))
        .next()
        .is_some()
    {
        match inlined_key(storage, txid, definition, bound, arena)? {
            Some(inlined) => inlined,
            None => return Ok(None),
        }
    } else {
        bound
    };
    let resolve = |qualifier: Option<&str>, name: &str| {
        qualifier
            .is_none()
            .then(|| definition.column_index(name))
            .flatten()
    };
    let Some(canonical) = canonical(bound, &resolve, definition, arena)? else {
        return Ok(None);
    };
    Ok(Some(ExpressionVariable {
        canonical,
        bound,
        ctype,
        collation: index.collations[position],
        statistics: None,
        unique: false,
        indexed: false,
    }))
}

/// `expression`, an index key bound to user-defined objects, as the planner
/// reads it (`RelationGetIndexExpressions` folds it with
/// `eval_const_expressions`, inlining SQL function calls); `None` when it
/// still calls a user-defined routine or reads a user-defined type, which
/// the model does not evaluate.
fn inlined_key<'a>(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    expression: &'a Expr<'a>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    let columns = crate::sql::exec::CatalogCols {
        remote_ctid: false,
        definition: Some(crate::storage::RelationDefinition::from_definition(
            definition,
        )),
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let folded = crate::sql::eval::fold_constants_in_scope(expression, arena, &catalog, &columns)?;
    let mut dependencies = storage.empty_stored_query_dependencies(arena)?;
    crate::sql::query::collect_runtime_expression(
        folded,
        storage,
        txid,
        &mut dependencies,
        arena,
        &columns,
    )?;
    Ok(dependencies.entries().is_empty().then_some(folded))
}

/// The value of variable `column` in the table row `values`: a column's, or
/// its key expression evaluated over the row.
pub(crate) fn row_value<'a>(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    variables: &ExpressionVariables<'a>,
    column: usize,
    values: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let Some(variable) = variables.get(column) else {
        return Ok(values[column]);
    };
    evaluate(storage, txid, definition, variable.bound, values, arena)
}

/// `expression`, a key expression of a table defined by `definition`,
/// evaluated over its row `values`.
pub(crate) fn evaluate<'a>(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    expression: &'a Expr<'a>,
    values: &[Datum<'a>],
    arena: &'a Arena,
) -> Result<Datum<'a>, SqlError> {
    let catalog = crate::sql::query::storage_catalog(storage, arena, txid);
    let hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&catalog),
        ..crate::sql::eval::NO_HOOKS
    };
    crate::sql::eval::eval_full(
        expression,
        arena,
        crate::sql::eval::NO_PARAMS,
        &crate::sql::exec::RowCtx {
            table_oid: None,
            def: crate::storage::RelationDefinition::from_definition(definition),
            values,
            alias: None,
        },
        &hooks,
    )
}

/// The expression keys of the index an index scan reads, bound to read its
/// key values from each table row.
pub(crate) struct KeyReader<'a> {
    expressions: [Option<&'a Expr<'a>>; crate::storage::MAX_INDEX_COLS],
}

impl<'a> KeyReader<'a> {
    /// Binds the expression keys among `keys` of the index named `index` on
    /// the table in `slot`.
    pub(crate) fn new(
        storage: &Storage,
        txid: u32,
        slot: usize,
        index: &str,
        keys: &[super::IndexKeyOrder],
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let mut expressions = [None; crate::storage::MAX_INDEX_COLS];
        if keys.iter().any(|key| key.expression.is_some()) {
            let definition = index_definition(storage, txid, slot, index)?;
            for (at, key) in keys.iter().enumerate() {
                let Some(expression) = key.expression else {
                    continue;
                };
                let position = usize::from(expression.position);
                let text = definition.expressions[position].ok_or_else(|| {
                    crate::sql_err!(
                        crate::sql::eval::sqlstate::INTERNAL_ERROR,
                        "index \"{}\" key {} is no expression",
                        index,
                        position + 1
                    )
                })?;
                let owner = crate::storage::ExpressionOwner::IndexKey(expression.position);
                let bound = crate::sql::exec::expression_bindings::bound_index_expression(
                    text.as_str(),
                    storage,
                    &definition,
                    owner,
                    txid,
                    arena,
                )?;
                // A key over an inlinable SQL function evaluates its body.
                let bound = if definition
                    .expression_dependencies_of(owner)
                    .next()
                    .is_some()
                {
                    let table = storage.table_def(slot, txid);
                    inlined_key(storage, txid, &table, bound, arena)?.unwrap_or(bound)
                } else {
                    bound
                };
                expressions[at] = Some(bound);
            }
        }
        Ok(Self { expressions })
    }

    /// Key `at`'s value in the table row `values`, of a table defined by
    /// `definition`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the evaluation context is explicit"
    )]
    pub(crate) fn value(
        &self,
        storage: &Storage,
        txid: u32,
        definition: &TableDef,
        keys: &[super::IndexKeyOrder],
        at: usize,
        values: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        match self.expressions[at] {
            Some(expression) => evaluate(storage, txid, definition, expression, values, arena),
            None => Ok(values[keys[at].column]),
        }
    }
}

fn index_definition(
    storage: &Storage,
    txid: u32,
    slot: usize,
    name: &str,
) -> Result<crate::storage::IndexDef, SqlError> {
    storage
        .indexes_for_slot(slot, txid)
        .find(|index| index.name_for(txid).as_str() == name)
        .ok_or_else(|| {
            crate::sql_err!(
                crate::sql::eval::sqlstate::INTERNAL_ERROR,
                "index \"{}\" is absent",
                name
            )
        })
}
