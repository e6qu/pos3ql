//! Table expressions bound to the catalog objects they named when written.
//!
//! A CHECK constraint or a column default or generation expression keeps its
//! SQL text and, in the table definition, the dependencies captured under the
//! creator's search path. Before running or printing one, its text is bound to
//! those identities under their current names, as PostgreSQL's stored trees
//! name objects by OID: a later `search_path` or a rename changes nothing.

use super::{Arena, Expr, SqlError, Storage};
use crate::storage::{ExpressionOwner, TableDefBuilder};

/// The dependencies of `text`, an expression over the columns of `def`,
/// resolved under the current search path.
fn collect_expression_dependencies<B: crate::storage::DefinitionBacking>(
    storage: &Storage,
    def: &crate::storage::TableDefinition<B>,
    text: &str,
    txid: u32,
    arena: &Arena,
) -> Result<CapturedDependencies, SqlError> {
    let mark = arena.mark();
    let result = (|| {
        let expression = crate::sql::parser::parse_expr(text, arena)?;
        let columns = super::CatalogCols {
            definition: Some(crate::storage::RelationDefinition::from_definition(def)),
            alias: None,
            output_aliases: &[],
            storage,
            txid,
            arena,
        };
        let mut dependencies = storage.empty_stored_query_dependencies(arena)?;
        crate::sql::query::collect_runtime_expression(
            expression,
            storage,
            txid,
            &mut dependencies,
            arena,
            &columns,
        )?;
        let mut captured = CapturedDependencies {
            entries: [crate::storage::StoredQueryDependency::EMPTY;
                crate::storage::MAX_TABLE_EXPRESSION_DEPENDENCIES],
            count: 0,
        };
        // A sequence is named by a regclass literal, which sequence renames
        // and drops already maintain in the stored text.
        for entry in dependencies
            .entries()
            .iter()
            .filter(|entry| entry.class != crate::storage::DependencyClass::Sequence)
        {
            let slot = captured
                .entries
                .get_mut(captured.count)
                .ok_or_else(crate::storage::too_many_expression_dependencies)?;
            *slot = *entry;
            captured.count += 1;
        }
        Ok(captured)
    })();
    // SAFETY: the captured entries were copied out of the arena.
    unsafe { arena.rewind_to(mark) };
    result
}

struct CapturedDependencies {
    entries:
        [crate::storage::StoredQueryDependency; crate::storage::MAX_TABLE_EXPRESSION_DEPENDENCIES],
    count: usize,
}

impl CapturedDependencies {
    fn entries(&self) -> &[crate::storage::StoredQueryDependency] {
        &self.entries[..self.count]
    }
}

/// Captures the dependencies of `owner`, whose stored text is `text`, under
/// the current search path.
pub(crate) fn capture_expression_dependencies(
    storage: &Storage,
    def: &mut TableDefBuilder,
    owner: ExpressionOwner,
    text: &str,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let captured = collect_expression_dependencies(storage, &*def, text, txid, arena)?;
    def.set_expression_dependencies(owner, captured.entries())
}

/// Captures what the key expressions and predicate of `index`, a new index
/// on `table`, name under the current search path.
pub(crate) fn capture_index_expression_dependencies<B: crate::storage::DefinitionBacking>(
    storage: &Storage,
    index: &mut crate::storage::IndexDef,
    table: &crate::storage::TableDefinition<B>,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    index.n_expression_dependencies = 0;
    for position in 0..index.n_cols {
        if let Some(text) = index.expressions[position] {
            let captured =
                collect_expression_dependencies(storage, table, text.as_str(), txid, arena)?;
            index.push_expression_dependencies(
                ExpressionOwner::IndexKey(position as u8),
                captured.entries(),
            )?;
        }
    }
    if let Some(text) = index.predicate {
        let captured = collect_expression_dependencies(storage, table, text.as_str(), txid, arena)?;
        index.push_expression_dependencies(ExpressionOwner::IndexPredicate, captured.entries())?;
    }
    Ok(())
}

/// Captures the default or generation expression of column `column`, or
/// drops the column's dependencies when it has none.
pub(crate) fn capture_column_expression_dependencies(
    storage: &Storage,
    def: &mut TableDefBuilder,
    column: usize,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    let owner = ExpressionOwner::Column(def.columns[column].name);
    let Some(text) = def.columns[column].default.expression().copied() else {
        return def.set_expression_dependencies(owner, &[]);
    };
    let mark = arena.mark();
    // A default's stored text may carry enum member bindings; capture reads
    // the SQL as written.
    let captured = crate::sql::enum_values::checks::default_display(text.as_str(), arena)
        .and_then(|text| collect_expression_dependencies(storage, &*def, text, txid, arena));
    // SAFETY: the captured entries are values; nothing borrows the arena.
    unsafe { arena.rewind_to(mark) };
    def.set_expression_dependencies(owner, captured?.entries())
}

/// `expression`, the parsed text of `owner` in `def`, bound to the objects
/// it was written against under their current names.
pub(crate) fn bind_table_expression<'a, B: crate::storage::DefinitionBacking>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    def: &crate::storage::TableDefinition<B>,
    owner: ExpressionOwner,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    bind_expression(
        expression,
        storage,
        def.expression_dependencies_of(owner),
        txid,
        arena,
    )
}

/// The stored text of `owner` in `index`, parsed and bound to the objects it
/// was written against under their current names.
pub(crate) fn bound_index_expression<'a>(
    text: &str,
    storage: &Storage,
    index: &crate::storage::IndexDef,
    owner: ExpressionOwner,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let text = arena
        .alloc_str(text)
        .map_err(|_| crate::sql::eval::arena_full())?;
    let expression = crate::sql::parser::parse_expr(text, arena)?;
    bind_expression(
        expression,
        storage,
        index.expression_dependencies_of(owner),
        txid,
        arena,
    )
}

fn bind_expression<'a>(
    expression: &'a Expr<'a>,
    storage: &Storage,
    dependencies: impl Iterator<Item = crate::storage::StoredQueryDependency> + Clone,
    txid: u32,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    let count = dependencies.clone().count();
    if count == 0 {
        return Ok(expression);
    }
    let bound = arena
        .alloc_slice_with(count, |_| crate::storage::StoredQueryDependency::EMPTY)
        .map_err(|_| crate::sql::eval::arena_full())?;
    for (slot, dependency) in bound.iter_mut().zip(dependencies) {
        *slot = storage.rebound_stored_query_dependency(dependency, txid)?;
    }
    crate::sql::query::bind_stored_expression(
        expression,
        storage,
        txid,
        crate::storage::StoredQueryDependencyView::new(bound),
        arena,
    )
}
