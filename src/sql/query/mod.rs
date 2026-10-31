//! SELECT execution: one pipeline for single tables and joins.
//!
//! Shape: resolve the FROM clause into a scope → enumerate source rows
//! (nested-loop joins, LEFT emitting a null row) → WHERE → then either
//! stream straight to the wire, or materialize projected rows as tagged
//! byte strings in the statement arena for GROUP BY / DISTINCT / ORDER BY.
//! ORDER BY keys ride along as hidden columns after the visible ones, so
//! arbitrary key expressions order both plain and joined queries.
//!
//! Subqueries are uncorrelated and pre-evaluated once per statement; their
//! results are injected into evaluation by node identity (EvalHooks).

use crate::mem::arena::{Arena, ArenaList};
use crate::pg::respond::{Responder, ResultFmt};
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, Storage, TableDef};

use super::ast::{
    Expr, FrameBound, FromClause, OrderBy, QualName, Select, SelectItem, SetQuery, Stmt, TableRef,
    WindowFrame,
};
use super::eval::{
    CatalogAccess, ColumnLookup, EvalHooks, PrivilegeRelation, SequenceAccess, SqlError,
    SubqueryValues, eval_full, resolved_expression_collation, sqlstate,
};
use super::exec::{MAX_PROJ, describe_items};
use super::types::{ColDesc, ColType, Datum};

mod retained;
pub(crate) use retained::RetainedSelect;

mod setops;
use setops::materialize_set_body;
pub use setops::set_query;
pub(crate) use setops::set_query_into_rows;

mod materialize;
pub(crate) use materialize::ScopeSchema;
use materialize::{
    external_materialized_into, finalize_projected_row, materialized_rows, materialized_select,
};

mod scan;
pub(crate) use plan::plan_time_bool;
pub use scan::JoinRow;
pub(crate) use scan::select_hash_join_plan;
use scan::{
    Chained, PaxReadDemand, pax_column_demand,
    scan_source_recycling_retaining_match_with_pax_columns,
    scan_source_recycling_with_indexed_candidates, scan_source_recycling_with_pax_columns,
    scan_source_with_pax_columns,
};
pub(crate) use scan::{
    IndexAccessPlan, OrderedIndexAccessPlan, dml_index_access_plan, dml_indexed_candidates,
    index_access_plan, ordered_index_access_plan, parameterized_index_access_plan,
};
pub(crate) use scan::{
    RowSecurityExpression, RowSecurityPlan, conjoin_row_security, plan_row_security,
    row_security_passes,
};

mod scope;
pub(crate) use cte::{bind_dml_materialized_relations, bind_materialized_relations};
pub use scope::{MergedColumn, QueryScope, ResolvedColumn};

mod cte;
pub(crate) use cte::captured_type_oid;
mod dependencies;
use cte::expand_set_tree_exec;
pub(crate) use cte::{
    RuleTransitionType, attach_rule_source, expand_set_tree, expand_stored_query,
    expand_stored_query_exec, expand_stored_rule_action_exec, expand_stored_rule_expression_exec,
    expand_stored_statement_exec, restrict_rule_original, rule_original_transition,
    rule_transition_source,
};
pub use cte::{
    describe_set_query, expand_ctes, expand_ctes_exec, expand_ctes_under, expand_dml_ctes,
    rewrite_view_dml,
};

pub(crate) fn stored_query_dependencies(
    sql: &str,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    bindings: Option<crate::storage::StoredQueryDependencyView<'_>>,
    arena: &Arena,
) -> Result<crate::storage::StoredQueryDependencies, SqlError> {
    dependencies::collect(sql, storage, txid, path, bindings, arena)
}

pub(crate) fn stored_routine_dependencies(
    program: &RoutineFunctionProgram<'_>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    arena: &Arena,
) -> Result<crate::storage::StoredQueryDependencies, SqlError> {
    dependencies::collect_routine_program(program, storage, txid, path, arena)
}

pub(crate) fn stored_rule_dependencies(
    actions: &[crate::sql::ast::RuleAction<'_>],
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    arena: &Arena,
    transition: &dyn crate::sql::exec::ColTypeResolver,
) -> Result<crate::storage::StoredQueryDependencies, SqlError> {
    dependencies::collect_rule_actions(actions, storage, txid, path, arena, transition)
}

pub(crate) fn dml_input_dependencies(
    statement: &crate::sql::ast::Stmt<'_>,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    arena: &Arena,
) -> Result<
    (
        crate::storage::StoredQueryDependencies,
        crate::storage::StoredQueryDependencies,
    ),
    SqlError,
> {
    dependencies::collect_dml_input(statement, storage, txid, path, arena)
}

#[derive(Clone, Copy)]
pub(crate) enum StoredQueryRenameTarget<'a> {
    CompositeField {
        slot: crate::storage::TypeCreation,
        from: &'a str,
    },
    TableColumn {
        table: usize,
        column: usize,
        sources: &'a [super::parser::ColumnSource<'a>],
    },
    CatalogColumns {
        table: usize,
        sources: &'a [super::parser::ColumnSource<'a>],
        bindings: &'a [core::cell::Cell<Option<usize>>],
    },
}

pub(crate) struct StoredQueryRename<'a> {
    pub storage: &'a Storage,
    pub txid: u32,
    pub target: StoredQueryRenameTarget<'a>,
    pub arena: &'a Arena,
}

struct StoredQueryRenameScope<'scope, 'arena> {
    current: &'scope QueryScope<'arena>,
    from: &'scope FromClause<'arena>,
    outer: Option<&'scope StoredQueryRenameScope<'scope, 'arena>>,
}

impl StoredQueryRenameScope<'_, '_> {
    fn column_origin(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<Option<(usize, usize)>, SqlError> {
        match self.current.find_column(qualifier, name) {
            Ok(ResolvedColumn::Table(source, selected)) => {
                let reference = if source == 0 {
                    &self.from.base
                } else {
                    &self.from.joins[source - 1].table
                };
                Ok((self.current.derived[source].is_none()
                    && reference
                        .col_alias
                        .is_none_or(|aliases| selected >= aliases.len()))
                .then_some((self.current.slots[source], selected)))
            }
            Ok(ResolvedColumn::Merged(_)) => Ok(None),
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                self.outer
                    .map_or(Ok(None), |outer| outer.column_origin(qualifier, name))
            }
            Err(error) => Err(error),
        }
    }
}

impl<'a> StoredQueryRename<'a> {
    fn collect_expression(
        &self,
        expression: &'a Expr<'a>,
        columns: &dyn super::exec::ColTypeResolver,
        outer_scope: Option<&StoredQueryRenameScope<'_, 'a>>,
        sites: &mut [bool],
        count: &mut usize,
    ) -> Result<(), SqlError> {
        match expression {
            Expr::Subquery(select) | Expr::Exists(select) | Expr::ArraySubquery(select) => {
                return collect_stored_query_composite_field_rename_select(
                    select,
                    self,
                    outer_scope,
                    sites,
                    count,
                );
            }
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                self.collect_expression(operand, columns, outer_scope, sites, count)?;
                return collect_stored_query_composite_field_rename_select(
                    select,
                    self,
                    outer_scope,
                    sites,
                    count,
                );
            }
            _ => {}
        }
        super::query::walk_children(expression, &mut |child| {
            self.collect_expression(child, columns, outer_scope, sites, count)
        })?;
        match self.target {
            StoredQueryRenameTarget::CompositeField { slot, from } => {
                super::exec::collect_composite_field_rename_site(
                    expression, columns, slot, from, sites, count,
                )
            }
            StoredQueryRenameTarget::TableColumn { table, sources, .. }
            | StoredQueryRenameTarget::CatalogColumns { table, sources, .. } => {
                let (qualifier, name) = match expression {
                    Expr::Column { qualifier, name } => (*qualifier, *name),
                    Expr::SchemaColumn {
                        schema,
                        table,
                        name,
                    } => {
                        let qualifier = crate::stack_format!(130, "{}.{}", schema, table);
                        (
                            Some(
                                self.arena
                                    .alloc_str(qualifier.as_str())
                                    .map_err(|_| arena_full())?,
                            ),
                            *name,
                        )
                    }
                    _ => return Ok(()),
                };
                if let Some((origin, column)) =
                    outer_scope.map_or(Ok(None), |scope| scope.column_origin(qualifier, name))?
                    && origin == table
                {
                    for (index, source) in sources.iter().enumerate() {
                        if core::ptr::eq(source.name.as_ptr(), name.as_ptr())
                            && source.name.len() == name.len()
                        {
                            match self.target {
                                StoredQueryRenameTarget::TableColumn { column: target, .. } => {
                                    sites[index] |= column == target;
                                }
                                StoredQueryRenameTarget::CatalogColumns { bindings, .. } => {
                                    bindings[index].set(Some(column));
                                }
                                StoredQueryRenameTarget::CompositeField { .. } => unreachable!(),
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn catalog_expression_bindings<'a>(
    select: &'a Select<'a>,
    sources: &'a [super::parser::ColumnSource<'a>],
    storage: &'a Storage,
    txid: u32,
    table: usize,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
) -> Result<&'a [core::cell::Cell<Option<usize>>], SqlError> {
    let select =
        cte::expand_stored_query(select, storage, txid, storage.path(), dependencies, arena)?;
    let values = arena
        .alloc_slice_with(sources.len(), |_| None::<usize>)
        .map_err(|_| arena_full())?;
    let bindings = core::cell::Cell::from_mut(values).as_slice_of_cells();
    let selection = StoredQueryRename {
        storage,
        txid,
        arena,
        target: StoredQueryRenameTarget::CatalogColumns {
            table,
            sources,
            bindings,
        },
    };
    collect_stored_query_composite_field_rename_select(select, &selection, None, &mut [], &mut 0)?;
    Ok(bindings)
}

fn collect_stored_query_composite_field_rename_set_tree<'a>(
    tree: &'a crate::sql::ast::SetTree<'a>,
    rename: &StoredQueryRename<'a>,
    outer_scope: Option<&StoredQueryRenameScope<'_, 'a>>,
    sites: &mut [bool],
    count: &mut usize,
) -> Result<(), SqlError> {
    match tree {
        crate::sql::ast::SetTree::Select(select) => {
            collect_stored_query_composite_field_rename_select(
                select,
                rename,
                outer_scope,
                sites,
                count,
            )
        }
        crate::sql::ast::SetTree::Op { left, right, .. } => {
            collect_stored_query_composite_field_rename_set_tree(
                left,
                rename,
                outer_scope,
                sites,
                count,
            )?;
            collect_stored_query_composite_field_rename_set_tree(
                right,
                rename,
                outer_scope,
                sites,
                count,
            )
        }
    }
}

fn collect_stored_query_composite_field_rename_select<'a>(
    select: &'a Select<'a>,
    rename: &StoredQueryRename<'a>,
    outer_scope: Option<&StoredQueryRenameScope<'_, 'a>>,
    sites: &mut [bool],
    count: &mut usize,
) -> Result<(), SqlError> {
    if let Some(tree) = select.set_body {
        collect_stored_query_composite_field_rename_set_tree(
            tree,
            rename,
            outer_scope,
            sites,
            count,
        )?;
        let columns: &dyn super::exec::ColTypeResolver = match outer_scope {
            Some(scope) => &ScopeCols(scope.current),
            None => &super::exec::NoCols,
        };
        for expression in select
            .order_by
            .iter()
            .map(|order| order.expression)
            .chain(select.limit)
            .chain(select.offset)
        {
            rename.collect_expression(expression, columns, outer_scope, sites, count)?;
        }
        return Ok(());
    }

    let scope = match &select.from {
        Some(from_clause) => Some(QueryScope::resolve_schema(
            rename.storage,
            from_clause,
            rename.txid,
            rename.arena,
            None,
        )?),
        None => None,
    };
    let catalog_columns = scope.as_ref().map(|scope| CatalogScopeCols {
        scope,
        outer_scope: outer_scope.map(|scope| scope.current),
        storage: rename.storage,
        txid: rename.txid,
        arena: rename.arena,
    });
    let columns: &dyn super::exec::ColTypeResolver = match (&catalog_columns, outer_scope) {
        (Some(columns), _) => columns,
        (None, Some(scope)) => &ScopeCols(scope.current),
        (None, None) => &super::exec::NoCols,
    };
    let scope_chain = scope.as_ref().map(|current| StoredQueryRenameScope {
        current,
        from: select.from.as_ref().expect("a resolved scope has FROM"),
        outer: outer_scope,
    });
    let nested_outer = scope_chain.as_ref().or(outer_scope);

    // Follow source order: selector flags are paired with lexer positions in
    // the unchanged stored SQL, including nested queries and set-op leaves.
    for expression in select.distinct_on {
        rename.collect_expression(expression, columns, nested_outer, sites, count)?;
    }
    for item in select.items {
        match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                rename.collect_expression(expression, columns, nested_outer, sites, count)?;
            }
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {}
        }
    }
    if let Some(from_clause) = &select.from {
        for table in core::iter::once(&from_clause.base)
            .chain(from_clause.joins.iter().map(|join| &join.table))
        {
            if let Some(sample) = table.sample {
                rename.collect_expression(
                    sample.percentage,
                    columns,
                    nested_outer,
                    sites,
                    count,
                )?;
                if let Some(repeatable) = sample.repeatable {
                    rename.collect_expression(repeatable, columns, nested_outer, sites, count)?;
                }
            }
            if let Some(arguments) = table.func_args {
                for argument in arguments {
                    rename.collect_expression(argument, columns, nested_outer, sites, count)?;
                }
            }
            if let Some(functions) = table.rows_from {
                for function in functions {
                    if let Some(arguments) = function.func_args {
                        for argument in arguments {
                            rename.collect_expression(
                                argument,
                                columns,
                                nested_outer,
                                sites,
                                count,
                            )?;
                        }
                    }
                }
            }
            if let Some(subquery) = table.subquery {
                collect_stored_query_composite_field_rename_select(
                    subquery,
                    rename,
                    nested_outer,
                    sites,
                    count,
                )?;
            }
        }
        for join in from_clause.joins {
            if let Some(on) = join.on {
                rename.collect_expression(on, columns, nested_outer, sites, count)?;
            }
        }
    }
    for expression in select
        .where_clause
        .into_iter()
        .chain(select.group_by.iter().copied())
        .chain(select.having)
        .chain(select.order_by.iter().map(|order| order.expression))
        .chain(select.limit)
        .chain(select.offset)
    {
        rename.collect_expression(expression, columns, nested_outer, sites, count)?;
    }
    Ok(())
}

pub(crate) fn stored_query_composite_field_rename_sites<'a>(
    sql: &'a str,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    rename: &StoredQueryRename<'a>,
    sites: &mut [bool],
) -> Result<usize, SqlError> {
    let parsed = super::parser::parse_query(sql, rename.arena)?;
    let expanded = cte::expand_stored_query(
        parsed,
        rename.storage,
        rename.txid,
        path,
        dependencies,
        rename.arena,
    )?;
    let mut count = 0usize;
    collect_stored_query_composite_field_rename_select(expanded, rename, None, sites, &mut count)?;
    Ok(count)
}

#[expect(
    clippy::too_many_arguments,
    reason = "stored query and selected column identity are explicit"
)]
pub(crate) fn stored_query_table_column_rename<'a>(
    sql: &'a str,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    storage: &'a Storage,
    txid: u32,
    table: usize,
    column: usize,
    to: &str,
    arena: &'a Arena,
) -> Result<crate::util::StackStr<{ crate::storage::VIEW_SQL_MAX }>, SqlError> {
    use core::fmt::Write as _;
    let (parsed, source_map) = super::parser::parse_query_with_column_sources(sql, arena)?;
    let sources = source_map.columns;
    let expanded = cte::expand_stored_query(parsed, storage, txid, path, dependencies, arena)?;
    let sites = arena
        .alloc_slice_with(sources.len(), |_| false)
        .map_err(|_| arena_full())?;
    let rename = StoredQueryRename {
        storage,
        txid,
        arena,
        target: StoredQueryRenameTarget::TableColumn {
            table,
            column,
            sources,
        },
    };
    collect_stored_query_composite_field_rename_select(expanded, &rename, None, sites, &mut 0)?;
    #[derive(Clone, Copy)]
    struct Edit {
        start: usize,
        end: usize,
        projection: Option<usize>,
    }
    let mut edits = ArenaList::new(arena);
    for (source, selected) in sources.iter().zip(sites.iter()) {
        if !selected {
            continue;
        }
        edits
            .push(Edit {
                start: source.start,
                end: source.end,
                projection: None,
            })
            .map_err(|_| arena_full())?;
    }
    // A subquery's implicit output name is a binding used by its parent.
    // Preserve it explicitly when changing the column that supplied its name.
    for (index, projection) in source_map.projections.iter().enumerate() {
        if sources.iter().zip(sites.iter()).any(|(source, selected)| {
            *selected
                && source.start >= projection.start
                && source.end <= projection.end
                && source.name == projection.name
        }) {
            edits
                .push(Edit {
                    start: projection.end,
                    end: projection.end,
                    projection: Some(index),
                })
                .map_err(|_| arena_full())?;
        }
    }
    let edits = edits.into_mut_slice().map_err(|_| arena_full())?;
    edits.sort_unstable_by_key(|edit| (edit.start, edit.end));
    let mut rewritten = crate::util::StackStr::new();
    let mut copied = 0;
    for edit in edits {
        if edit.start < copied || edit.end > sql.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "stored column source spans overlap"
            ));
        }
        let _ = rewritten.write_str(&sql[copied..edit.start]);
        let name = if let Some(index) = edit.projection {
            let _ = rewritten.write_str(" AS ");
            source_map.projections[index].name
        } else {
            to
        };
        super::exec::write_identifier(&mut rewritten, name);
        copied = edit.end;
    }
    let _ = rewritten.write_str(&sql[copied..]);
    if rewritten.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "renamed view query exceeds its fixed SQL capacity"
        ));
    }
    Ok(rewritten)
}

mod aggregate;
use aggregate::{AggState, fold_aggregates};

mod srf;
pub(crate) use srf::{
    describe_derived_definition, expression_has_project_set, is_srf_name as is_builtin_set_routine,
    synth_derived_def, synth_derived_def_outer, table_func_rows_outer,
};
use srf::{find_srf, has_project_set, prepare_project_set, table_func_def, table_func_def_outer};

mod group;
use group::{grouped_rows, grouped_select};

mod plan;
pub(crate) use plan::join_order;
use plan::{postpone_cost, reorder_qual, simplify_qual, where_passes};

mod subquery;
pub(crate) use subquery::{ProceduralSubqueries, subquery_expression_type};
use subquery::{
    correlated_in_expression, correlated_scan_conjuncts, correlated_where_passes, merge_correlated,
    prepare_outer_subqueries, subquery_witness,
};
pub(crate) use subquery::{expression_has_correlated_subquery, subquery_hooks_outer};
pub use subquery::{prepare_subqueries, subquery_hooks};
pub(crate) use subquery::{type_witness, walk_children};

mod window;
use window::{
    cmp_key_rows, dedup_window_rows, external_window_into, project_window_rows,
    rewrite_grouped_windows, window_select,
};

/// Relation count handled by compact `u64` planner proofs. Wider joins execute
/// exactly with arena-sized state and skip these optional access-path proofs.
pub(crate) const MAX_OPTIMIZED_JOIN_TABLES: usize = u64::BITS as usize;
use core::cell::{Cell, RefCell};
std::thread_local! {
    /// Wall-clock deadline (micros since 2000-01-01) for the running statement;
    /// 0 means no `statement_timeout` is armed. Single-threaded per connection.
    static DEADLINE: Cell<i64> = const { Cell::new(0) };
}

/// Arms `statement_timeout` for the current statement (`timeout_ms == 0` clears
/// it). Call [`disarm_timeout`] when the statement completes.
pub fn arm_timeout(timeout_ms: u64) {
    let dl = if timeout_ms == 0 {
        0
    } else {
        super::datetime::now_micros().saturating_add(timeout_ms as i64 * 1000)
    };
    DEADLINE.with(|d| d.set(dl));
}

pub(crate) fn statement_deadline() -> i64 {
    DEADLINE.with(Cell::get)
}

pub(crate) fn restore_statement_deadline(deadline: i64) {
    DEADLINE.with(|value| value.set(deadline));
}

/// Clears any armed statement deadline.
pub fn disarm_timeout() {
    DEADLINE.with(|d| d.set(0));
}

/// Errors 57014 if the armed statement deadline has passed. Every scan
/// boundary reads the deadline: amortization can turn an expired statement
/// into a successful partial result when a small outer scan owns a huge inner
/// nested loop.
pub fn check_timeout() -> Result<(), SqlError> {
    let dl = DEADLINE.with(|d| d.get());
    if dl == 0 {
        return Ok(());
    }
    if super::datetime::now_micros() >= dl {
        return Err(sql_err!(
            sqlstate::QUERY_CANCELED,
            "canceling statement due to statement timeout"
        ));
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) struct PendingRoutineInvocation<'a> {
    pub slot: usize,
    pub intrinsic_oid: Option<i32>,
    pub arguments: &'a [u8],
    pub argument_count: usize,
}

/// A completed routine invocation is either a scalar expression value or the
/// encoded rows of a table source. Keeping the two contracts distinct prevents
/// a restart from ever interpreting one as the other.
#[derive(Clone, Copy)]
enum RoutineInvocationResult<'a> {
    Scalar(Datum<'a>),
    Rows(&'a [&'a [u8]]),
}

pub(crate) struct RoutineInvocationState<'a> {
    next: Cell<usize>,
    completed: Cell<usize>,
    pending: Cell<Option<PendingRoutineInvocation<'a>>>,
    results: RefCell<ArenaList<'a, RoutineInvocationResult<'a>>>,
}

#[derive(Clone, Copy)]
pub(crate) struct RoutineInvocationContext<'a> {
    invocations: &'a RoutineInvocationState<'a>,
    statement_arena: &'a Arena,
}

impl<'a> RoutineInvocationContext<'a> {
    pub(crate) const fn new(
        invocations: &'a RoutineInvocationState<'a>,
        statement_arena: &'a Arena,
    ) -> Self {
        Self {
            invocations,
            statement_arena,
        }
    }
}

thread_local! {
    static ACTIVE_ROUTINE_INVOCATIONS: Cell<Option<(*const (), *const ())>> = const { Cell::new(None) };
}

/// Installs the statement-owned routine invocation log while its query work
/// arena is scanned. Scan callbacks do not own an engine reference, so this
/// scoped bridge carries the already-borrowed context without duplicating it.
pub(crate) struct RoutineInvocationScope(Option<(*const (), *const ())>);

impl Drop for RoutineInvocationScope {
    fn drop(&mut self) {
        ACTIVE_ROUTINE_INVOCATIONS.with(|active| active.set(self.0));
    }
}

pub(crate) fn enter_routine_invocation_scope(
    context: Option<RoutineInvocationContext<'_>>,
) -> RoutineInvocationScope {
    let current = match context {
        Some(context) => Some((
            context.invocations as *const RoutineInvocationState<'_> as *const (),
            context.statement_arena as *const Arena as *const (),
        )),
        None => ACTIVE_ROUTINE_INVOCATIONS.with(Cell::get),
    };
    let prior = ACTIVE_ROUTINE_INVOCATIONS.with(|active| active.replace(current));
    RoutineInvocationScope(prior)
}

pub(crate) fn active_routine_invocations<'a>() -> Option<(&'a RoutineInvocationState<'a>, &'a Arena)>
{
    ACTIVE_ROUTINE_INVOCATIONS.with(|active| {
        let (invocations, arena) = active.get()?;
        // The only writer is `enter_routine_invocation_scope`; its guard stays
        // live for the synchronous scan and restores any enclosing execution.
        Some(unsafe {
            (
                &*(invocations as *const RoutineInvocationState<'a>),
                &*(arena as *const Arena),
            )
        })
    })
}

thread_local! {
    static ACTIVE_READ_EFFECTS: Cell<Option<core::ptr::NonNull<crate::checkpoint::AtomicBlockReadScope>>> = const { Cell::new(None) };
}

pub(crate) struct StatementReadEffectScope<'a> {
    previous: Option<core::ptr::NonNull<crate::checkpoint::AtomicBlockReadScope>>,
    _reads: Option<&'a crate::checkpoint::AtomicBlockReadScope>,
}

impl Drop for StatementReadEffectScope<'_> {
    fn drop(&mut self) {
        ACTIVE_READ_EFFECTS.with(|active| active.set(self.previous));
    }
}

pub(crate) fn enter_statement_read_effect_scope(
    reads: Option<&crate::checkpoint::AtomicBlockReadScope>,
) -> StatementReadEffectScope<'_> {
    let previous = ACTIVE_READ_EFFECTS.with(|active| {
        let previous = active.get();
        if let Some(reads) = reads {
            active.set(Some(core::ptr::NonNull::from(reads)));
        }
        previous
    });
    StatementReadEffectScope {
        previous,
        _reads: reads,
    }
}

/// A statement can yield before its first effect. After an effect starts,
/// remaining reads use the atomic client until that whole statement ends.
pub(crate) fn protect_statement_effects() {
    ACTIVE_READ_EFFECTS.with(|active| {
        if let Some(reads) = active.get() {
            // The scope's borrow pins this stack value through every nested
            // executor call; dropping the scope restores the enclosing pointer.
            unsafe { reads.as_ref() }.activate();
        }
    });
}

impl<'a> RoutineInvocationState<'a> {
    pub(crate) fn new(arena: &'a Arena) -> Self {
        Self {
            next: Cell::new(0),
            completed: Cell::new(0),
            pending: Cell::new(None),
            results: RefCell::new(ArenaList::new_persistent(arena)),
        }
    }

    pub(crate) fn begin_attempt(&self) {
        self.next.set(0);
        self.pending.set(None);
    }

    pub(crate) fn resolve<'query>(
        &self,
        slot: usize,
        arguments: &[Datum],
        statement_arena: &'a Arena,
        query_arena: &'query Arena,
    ) -> Result<Option<Datum<'query>>, SqlError> {
        let ordinal = self.next.get();
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            let RoutineInvocationResult::Scalar(value) = self.results.borrow().as_slice()[ordinal]
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SQL routine invocation changed from a table source to a scalar expression"
                ));
            };
            return Ok(Some(super::exec::detach_routine_datum(value, query_arena)?));
        }
        self.pending.set(Some(PendingRoutineInvocation {
            slot,
            intrinsic_oid: None,
            arguments: super::exec::encode_projected_persistent_pub(arguments, statement_arena)?,
            argument_count: arguments.len(),
        }));
        Err(sql_err!(
            sqlstate::INTERNAL_ROUTINE_INVOCATION,
            "mutable SQL routine invocation is pending"
        ))
    }

    pub(crate) fn resolve_intrinsic<'query>(
        &self,
        oid: i32,
        arguments: &[Datum],
        statement_arena: &'a Arena,
        query_arena: &'query Arena,
    ) -> Result<Datum<'query>, SqlError> {
        let ordinal = self.next.get();
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            let RoutineInvocationResult::Scalar(value) = self.results.borrow().as_slice()[ordinal]
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "intrinsic invocation changed result shape"
                ));
            };
            return super::exec::detach_routine_datum(value, query_arena);
        }
        self.pending.set(Some(PendingRoutineInvocation {
            slot: 0,
            intrinsic_oid: Some(oid),
            arguments: super::exec::encode_projected_persistent_pub(arguments, statement_arena)?,
            argument_count: arguments.len(),
        }));
        Err(sql_err!(
            sqlstate::INTERNAL_ROUTINE_INVOCATION,
            "mutable intrinsic invocation is pending"
        ))
    }

    pub(crate) fn take_pending(&self) -> Option<PendingRoutineInvocation<'a>> {
        self.pending.replace(None)
    }

    pub(crate) fn complete(&self, result: Datum<'a>) -> Result<(), SqlError> {
        let ordinal = self.completed.get();
        self.results
            .borrow_mut()
            .push(RoutineInvocationResult::Scalar(result))
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "mutable routine replay exceeds statement memory"
                )
            })?;
        self.completed.set(ordinal + 1);
        Ok(())
    }

    pub(crate) fn resolve_rows(
        &self,
        slot: usize,
        arguments: &[Datum],
        statement_arena: &'a Arena,
    ) -> Result<Option<&'a [&'a [u8]]>, SqlError> {
        let ordinal = self.next.get();
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            let RoutineInvocationResult::Rows(rows) = self.results.borrow().as_slice()[ordinal]
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SQL routine invocation changed from a scalar expression to a table source"
                ));
            };
            return Ok(Some(rows));
        }
        self.pending.set(Some(PendingRoutineInvocation {
            slot,
            intrinsic_oid: None,
            arguments: super::exec::encode_projected_persistent_pub(arguments, statement_arena)?,
            argument_count: arguments.len(),
        }));
        Err(sql_err!(
            sqlstate::INTERNAL_ROUTINE_INVOCATION,
            "mutable SQL routine invocation is pending"
        ))
    }

    pub(crate) fn complete_rows(&self, rows: &'a [&'a [u8]]) -> Result<(), SqlError> {
        let ordinal = self.completed.get();
        self.results
            .borrow_mut()
            .push(RoutineInvocationResult::Rows(rows))
            .map_err(|_| {
                sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "mutable routine replay exceeds statement memory"
                )
            })?;
        self.completed.set(ordinal + 1);
        Ok(())
    }
}
const SUBQUERY_DEPTH: u32 = 4;

type Outcome = Result<Result<(), SqlError>, WireFull>;

/// A SQL-language function body can contain either ordinary or set-operation
/// queries; no DML statement can cross this boundary.
#[derive(Clone, Copy)]
pub(crate) enum RoutineQuery<'a> {
    Select(&'a Select<'a>),
    Set(&'a SetQuery<'a>),
}

/// A non-final SQL-function statement. It cannot be used as the result, so a
/// command tag or discarded `RETURNING` stream cannot escape the function.
#[derive(Clone, Copy)]
pub(crate) enum RoutinePrelude<'a> {
    Statement(&'a Stmt<'a>),
    Forbidden(&'static str),
}

#[derive(Clone, Copy)]
pub(crate) enum RoutineFunctionResult<'a> {
    Query(&'a RoutineQuery<'a>),
    DataModification(&'a Stmt<'a>),
    Void(&'a Stmt<'a>),
    Forbidden(&'static str),
}

/// A SQL-language function program with one typed final result statement.
/// PostgreSQL runs every preceding statement and takes values only from the final result.
pub(crate) struct RoutineFunctionProgram<'a> {
    pub preceding: &'a [RoutinePrelude<'a>],
    pub result: RoutineFunctionResult<'a>,
}

/// Parses a SQL-language function body. PostgreSQL permits supported SQL
/// statements before the final result, but transaction control cannot cross a
/// function boundary.
pub(crate) fn parse_routine_function_program<'a>(
    body: &'a str,
    arena: &'a Arena,
    returns_void: bool,
    routine_name: &'a str,
    parameters: &[crate::storage::RoutineArgumentDef],
) -> Result<RoutineFunctionProgram<'a>, SqlError> {
    let mut parser = super::parser::Parser::new(body, arena)
        .and_then(|parser| parser.with_routine_parameters(routine_name, parameters))
        .map_err(|error| super::parse_error_to_sql(&error))?;
    let parsed = arena
        .alloc_slice_with(super::parser::statement_capacity(body), |_| None)
        .map_err(|_| arena_full())?;
    let mut count = 0usize;
    loop {
        let statement = parser
            .next_stmt()
            .map_err(|error| super::parse_error_to_sql(&error))?;
        let Some(statement) = statement else { break };
        let step = if let Some(name) = routine_statement_forbidden(&statement) {
            RoutinePrelude::Forbidden(name)
        } else {
            RoutinePrelude::Statement(arena.alloc(statement).map_err(|_| arena_full())?)
        };
        debug_assert!(count < parsed.len());
        parsed[count] = Some(step);
        count += 1;
    }
    let Some(last) = count.checked_sub(1).and_then(|index| parsed[index]) else {
        return Err(sql_err!(sqlstate::SYNTAX_ERROR, "function body is empty"));
    };
    let result = if returns_void {
        match last {
            RoutinePrelude::Statement(statement) => RoutineFunctionResult::Void(statement),
            RoutinePrelude::Forbidden(statement) => RoutineFunctionResult::Forbidden(statement),
        }
    } else {
        match last {
            RoutinePrelude::Statement(Stmt::Select(query)) => RoutineFunctionResult::Query(
                arena
                    .alloc(RoutineQuery::Select(query))
                    .map_err(|_| arena_full())?,
            ),
            RoutinePrelude::Statement(Stmt::SetQuery(query)) => RoutineFunctionResult::Query(
                arena
                    .alloc(RoutineQuery::Set(query))
                    .map_err(|_| arena_full())?,
            ),
            RoutinePrelude::Statement(statement) if statement_returns_rows(statement) => {
                RoutineFunctionResult::DataModification(statement)
            }
            RoutinePrelude::Statement(_) | RoutinePrelude::Forbidden(_) => {
                return Err(sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "SQL function body must end with a SELECT query or data-modifying statement with RETURNING"
                ));
            }
        }
    };
    let preceding = arena
        .alloc_slice_with(count - 1, |index| {
            parsed[index].expect("counted routine step")
        })
        .map_err(|_| arena_full())?;
    Ok(RoutineFunctionProgram { preceding, result })
}

pub(crate) fn parse_stored_routine_function_program<'a>(
    body_kind: crate::storage::RoutineBodyKind,
    body: &'a str,
    arena: &'a Arena,
    returns_void: bool,
    routine_name: &'a str,
    parameters: &[crate::storage::RoutineArgumentDef],
) -> Result<RoutineFunctionProgram<'a>, SqlError> {
    let executable = if body_kind == crate::storage::RoutineBodyKind::Return {
        use core::fmt::Write;
        let mut query = crate::util::StackStr::<{ crate::storage::ROUTINE_SQL_MAX + 8 }>::new();
        write!(query, "SELECT {body}").map_err(|_| {
            sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "routine definition exceeds {} bytes",
                crate::storage::ROUTINE_SQL_MAX
            )
        })?;
        arena.alloc_str(query.as_str()).map_err(|_| arena_full())?
    } else {
        body
    };
    parse_routine_function_program(executable, arena, returns_void, routine_name, parameters)
}

fn routine_statement_forbidden(statement: &Stmt<'_>) -> Option<&'static str> {
    Some(match statement {
        Stmt::Begin(_) => "BEGIN",
        Stmt::Commit => "COMMIT",
        Stmt::Rollback => "ROLLBACK",
        Stmt::Savepoint(_) => "SAVEPOINT",
        Stmt::ReleaseSavepoint(_) => "RELEASE SAVEPOINT",
        Stmt::RollbackToSavepoint(_) => "ROLLBACK TO SAVEPOINT",
        Stmt::Vacuum { .. } => "VACUUM",
        Stmt::Checkpoint => "CHECKPOINT",
        _ => return None,
    })
}

/// Whether a scalar routine needs the engine-owned executor rather than the
/// read-only evaluator bridge. This is a property of the parsed program, not
/// a spelling heuristic at an individual call site.
pub(crate) fn routine_program_requires_mutable_execution(
    program: &RoutineFunctionProgram<'_>,
) -> bool {
    let needs_sequences = |name: &str, _arguments: usize| {
        matches!(name, "nextval" | "currval" | "lastval" | "setval")
    };
    let query_needs_engine = |query: &RoutineQuery<'_>| match query {
        RoutineQuery::Select(select) => cte::select_contains_function(select, &needs_sequences),
        RoutineQuery::Set(query) => {
            query
                .with
                .iter()
                .any(|cte| cte::select_contains_function(cte.query, &needs_sequences))
                || cte::set_tree_contains_function(query.body, &needs_sequences)
                || query.order_by.iter().any(|order| {
                    cte::expression_contains_function(order.expression, &needs_sequences)
                })
                || query
                    .limit
                    .is_some_and(|expr| cte::expression_contains_function(expr, &needs_sequences))
                || query
                    .offset
                    .is_some_and(|expr| cte::expression_contains_function(expr, &needs_sequences))
        }
    };
    let statement_needs_engine = |statement: &Stmt<'_>| match statement {
        Stmt::Select(select) => query_needs_engine(&RoutineQuery::Select(select)),
        Stmt::SetQuery(query) => query_needs_engine(&RoutineQuery::Set(query)),
        _ => true,
    };
    program.preceding.iter().any(|step| match step {
        RoutinePrelude::Statement(statement) => statement_needs_engine(statement),
        RoutinePrelude::Forbidden(_) => true,
    }) || match program.result {
        RoutineFunctionResult::Query(query) => query_needs_engine(query),
        RoutineFunctionResult::Void(statement) => statement_needs_engine(statement),
        RoutineFunctionResult::DataModification(_) | RoutineFunctionResult::Forbidden(_) => true,
    }
}

pub(crate) fn routine_forbidden_statement_error(statement: &str) -> SqlError {
    sql_err!(
        sqlstate::ACTIVE_SQL_TRANSACTION,
        "{} cannot be executed from an SQL function",
        statement
    )
}

pub(crate) fn statement_returns_rows(statement: &Stmt<'_>) -> bool {
    match statement {
        Stmt::Insert(insert) => !insert.returning.is_empty(),
        Stmt::Update(update) => !update.returning.is_empty(),
        Stmt::Delete(delete) => !delete.returning.is_empty(),
        Stmt::Merge(merge) => !merge.returning.is_empty(),
        Stmt::With { statement, .. } => statement_returns_rows(statement),
        _ => false,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "routine-query execution plumbing"
)]
pub(crate) fn execute_routine_query<'a>(
    query: &RoutineQuery<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    recycling: bool,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    execute_routine_query_under(
        query, storage, txid, arena, params, recycling, None, None, emit,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "stored routine execution carries its durable identity and result sink"
)]
pub(crate) fn execute_bound_routine_query<'a>(
    query: &RoutineQuery<'a>,
    storage: &'a Storage,
    routine_slot: usize,
    routine: crate::storage::RoutineDef,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    recycling: bool,
    sequences: Option<&dyn SequenceAccess>,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    if routine.body_kind == crate::storage::RoutineBodyKind::String {
        return execute_routine_query_under(
            query, storage, txid, arena, params, recycling, sequences, None, emit,
        );
    }
    let user = super::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(routine.creation_path.as_str(), user.as_str(), txid);
    let dependencies = storage.snapshot_routine_dependencies_for(routine_slot, txid, arena)?;
    execute_routine_query_under(
        query,
        storage,
        txid,
        arena,
        params,
        recycling,
        sequences,
        Some((path, dependencies.view())),
        emit,
    )
}

pub(crate) fn bind_stored_routine_statement<'a>(
    statement: &'a Stmt<'a>,
    storage: &Storage,
    routine_slot: usize,
    routine: crate::storage::RoutineDef,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
) -> Result<&'a Stmt<'a>, SqlError> {
    if routine.body_kind == crate::storage::RoutineBodyKind::String {
        return Ok(statement);
    }
    let user = super::eval::funcs::system::session_user_owned();
    let path = storage.compute_path(routine.creation_path.as_str(), user.as_str(), txid);
    let dependencies = storage.snapshot_routine_dependencies_for(routine_slot, txid, arena)?;
    let rebound = match statement {
        Stmt::Select(select) => Stmt::Select(*expand_stored_query_exec(
            select,
            storage,
            txid,
            path,
            dependencies.view(),
            arena,
            params,
            None,
        )?),
        Stmt::SetQuery(query) => {
            let body = cte::expand_stored_set_tree_exec(
                query.with,
                query.body,
                storage,
                txid,
                cte::StoredExecutionContext::new(path, dependencies.view(), params, None),
                arena,
            )?;
            Stmt::SetQuery(SetQuery {
                with: &[],
                body,
                ..*query
            })
        }
        Stmt::Insert(_)
        | Stmt::Update(_)
        | Stmt::Delete(_)
        | Stmt::Merge(_)
        | Stmt::With { .. } => {
            return expand_stored_statement_exec(
                statement,
                storage,
                txid,
                path,
                dependencies.view(),
                arena,
                params,
                None,
            );
        }
        _ => return Ok(statement),
    };
    arena
        .alloc(rebound)
        .map(|statement| &*statement)
        .map_err(|_| arena_full())
}

#[expect(
    clippy::too_many_arguments,
    reason = "routine query execution carries its optional creation-time binding"
)]
fn execute_routine_query_under<'a>(
    query: &RoutineQuery<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    recycling: bool,
    sequences: Option<&dyn SequenceAccess>,
    stored: Option<(
        crate::storage::PathContext,
        crate::storage::StoredQueryDependencyView<'a>,
    )>,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    match query {
        RoutineQuery::Select(select) => {
            let select = match stored {
                Some((path, dependencies)) => expand_stored_query_exec(
                    select,
                    storage,
                    txid,
                    path,
                    dependencies,
                    arena,
                    params,
                    sequences,
                )?,
                None => expand_ctes_exec(select, storage, txid, arena, params, &[], sequences)?,
            };
            validate_locking(select)?;
            if recycling {
                select_into_rows_recycling(
                    storage, txid, select, arena, params, None, sequences, emit,
                )
            } else {
                select_into_rows(storage, txid, select, arena, params, None, sequences, emit)
            }
        }
        RoutineQuery::Set(query) => {
            let Some((path, dependencies)) = stored else {
                return setops::set_query_into_rows(
                    storage, txid, query, arena, params, sequences, emit,
                );
            };
            let body = cte::expand_stored_set_tree_exec(
                query.with,
                query.body,
                storage,
                txid,
                cte::StoredExecutionContext::new(path, dependencies, params, sequences),
                arena,
            )?;
            let rebound = SetQuery {
                with: &[],
                body,
                ..**query
            };
            let rebound = arena.alloc(rebound).map_err(|_| arena_full())?;
            setops::set_query_into_rows(storage, txid, rebound, arena, params, sequences, emit)
        }
    }
}

/// Bridges `EvalHooks`' abstract `CatalogAccess` to the concrete `Storage`, so
/// `pg_get_indexdef` can reconstruct an index's definition during evaluation.
pub(super) struct StorageCatalog<'storage, 'workspace, 'invocation, 'statement> {
    storage: &'storage Storage,
    routine_workspace: &'workspace Arena,
    txid: u32,
    invocations: Option<&'invocation RoutineInvocationState<'statement>>,
    statement_arena: Option<&'statement Arena>,
}

pub(super) fn storage_catalog<'storage, 'workspace>(
    storage: &'storage Storage,
    routine_workspace: &'workspace Arena,
    txid: u32,
) -> StorageCatalog<'storage, 'workspace, 'static, 'static> {
    StorageCatalog {
        storage,
        routine_workspace,
        txid,
        invocations: None,
        statement_arena: None,
    }
}

pub(crate) fn execute_index_comparison<'a>(
    storage: &Storage,
    txid: u32,
    function_oid: i32,
    left: Datum<'a>,
    right: Datum<'a>,
    arena: &'a Arena,
) -> Result<core::cmp::Ordering, SqlError> {
    let slot = storage
        .routine_slot_by_oid(function_oid, txid)
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "index support function does not exist"
            )
        })?;
    let catalog = storage_catalog(storage, arena, txid);
    let result = catalog
        .execute_routine(slot, storage.routine_for(slot, txid), &[left, right], arena)?
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INVALID_FUNCTION_DEFINITION,
                "index comparison function produced no scalar result"
            )
        })?;
    let comparison = match result {
        Datum::Int4(value) => value,
        Datum::Null => {
            return Err(sql_err!(
                sqlstate::NULL_VALUE_NOT_ALLOWED,
                "index comparison function returned null"
            ));
        }
        _ => {
            return Err(sql_err!(
                sqlstate::INVALID_FUNCTION_DEFINITION,
                "index comparison function must return integer"
            ));
        }
    };
    Ok(comparison.cmp(&0))
}

pub(crate) fn builtin_operator_result(name: &str, arguments: &[i32]) -> Option<ColType> {
    if let Some(result) = super::eval::funcs::geometry::operator_result(name, arguments) {
        return Some(result);
    }
    if arguments
        .iter()
        .any(|oid| matches!(ColType::from_oid(*oid), Some(ColType::Geometry(_))))
    {
        return None;
    }
    if arguments.len() != 2 {
        return None;
    }
    let operator = super::ast::BinaryOp::from_operator_name(name)?;
    super::exec::builtin_binary_type(&operator, arguments[0], arguments[1])
        .ok()
        .and_then(|(oid, _)| ColType::from_oid(oid))
}

impl StorageCatalog<'_, '_, '_, '_> {
    /// The composite rowtype fields of a table or view: column name, resolved
    /// type OID, and a NULL value per column — the shape `json_populate_record`
    /// and rowtype casts populate from.
    fn rowtype_fields<'a>(
        &self,
        type_oid: i32,
        arena: &'a Arena,
    ) -> Result<&'a [super::types::RecordField<'a>], SqlError> {
        use super::types::oid;
        let field_type_oid = |ctype: ColType,
                              user_type: Option<crate::storage::UserTypeName>|
         -> Result<i32, SqlError> {
            match user_type {
                Some(identity) => self
                    .storage
                    .user_type_identity_oid(identity, matches!(ctype, ColType::Array(_)), self.txid)
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "composite field type identity is unavailable"
                        )
                    }),
                None => Ok(ctype.oid()),
            }
        };
        let mut columns = ArenaList::new(arena);
        if (oid::FIRST_TABLE_COMPOSITE..oid::FIRST_VIEW_COMPOSITE).contains(&type_oid) {
            let slot = super::catalog::table_slot_by_row_type_oid(
                self.storage,
                self.txid,
                type_oid,
                false,
            )
            .ok_or_else(|| {
                sql_err!(sqlstate::INTERNAL_ERROR, "table rowtype OID is not a table")
            })?;
            for column in self.storage.table_def(slot, self.txid).columns() {
                let name = arena
                    .alloc_str(column.name.as_str())
                    .map_err(|_| arena_full())?;
                let field_oid = field_type_oid(column.ctype, column.user_type)?;
                columns
                    .push(super::types::RecordField {
                        name,
                        type_oid: field_oid,
                        value: Datum::Null,
                    })
                    .map_err(|_| arena_full())?;
            }
            return Ok(columns.as_slice());
        }
        let slot =
            super::catalog::view_slot_by_row_type_oid(self.storage, self.txid, type_oid, false)
                .ok_or_else(|| {
                    sql_err!(sqlstate::INTERNAL_ERROR, "view rowtype OID is not a view")
                })?;
        let view = self.storage.view(slot);
        let count = view.columns_for(self.txid).names().len();
        let described = arena
            .alloc_slice_with(count, |_| super::types::ColDesc::new("", 0, 0))
            .map_err(|_| arena_full())?;
        let count =
            super::catalog::describe_view(self.storage, self.txid, &view, arena, described)?;
        for column in &described[..count] {
            let name = arena.alloc_str(column.name).map_err(|_| arena_full())?;
            columns
                .push(super::types::RecordField {
                    name,
                    type_oid: column.type_oid,
                    value: Datum::Null,
                })
                .map_err(|_| arena_full())?;
        }
        Ok(columns.as_slice())
    }

    fn coerce_result<'a>(
        &self,
        value: Datum<'a>,
        target: crate::storage::RoutineResult,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        super::exec::coerce_routine_argument(
            value,
            crate::storage::RoutineArgumentDef {
                name: crate::storage::SqlName::EMPTY,
                ctype: target.ctype,
                user_type: target.user_type,
            },
            self.storage,
            self.txid,
            arena,
        )
    }

    fn execute_defined_cast<'a>(
        &self,
        source_oid: i32,
        target_oid: i32,
        value: Datum<'a>,
        context: super::eval::DefinedCastContext,
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        let Some((_, cast)) = self
            .storage
            .cast_for_oids(source_oid, target_oid, self.txid)
        else {
            return Ok(None);
        };
        if [cast.source, cast.target].into_iter().any(|result| {
            result.user_type.is_some_and(|identity| {
                self.storage
                    .domain_identity_slot(
                        identity.schema.as_str(),
                        identity.name.as_str(),
                        self.txid,
                    )
                    .is_some()
            })
        }) {
            return Ok(None);
        }
        let permitted = match context {
            super::eval::DefinedCastContext::Explicit => true,
            super::eval::DefinedCastContext::Assignment => cast.context.permits_assignment(),
            super::eval::DefinedCastContext::Implicit => cast.context.permits_implicit(),
        };
        if !permitted {
            return Ok(None);
        }
        let result = match cast.method {
            crate::storage::CastMethod::Binary => value,
            crate::storage::CastMethod::InOut => {
                let text = super::eval::datum_to_text(value, arena)?;
                self.coerce_result(Datum::Text(text), cast.target, arena)?
            }
            crate::storage::CastMethod::Function(slot) => {
                let slot = self
                    .storage
                    .routine_slot_by_oid(slot, self.txid)
                    .ok_or_else(|| {
                        sql_err!(sqlstate::UNDEFINED_FUNCTION, "cast function does not exist")
                    })?;
                self.storage.require_routine_execute(slot, self.txid)?;
                let routine = self.storage.routine_for(slot, self.txid);
                let mut arguments = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
                arguments[0] = value;
                if routine.argument_count >= 2 {
                    arguments[1] = Datum::Int4(-1);
                }
                if routine.argument_count == 3 {
                    arguments[2] =
                        Datum::Bool(matches!(context, super::eval::DefinedCastContext::Explicit));
                }
                self.execute_routine(slot, routine, &arguments[..routine.argument_count], arena)?
                    .unwrap_or(Datum::Null)
            }
        };
        self.coerce_result(result, cast.target, arena).map(Some)
    }

    fn execute_catalog_operator<'a>(
        &self,
        schema: Option<&str>,
        name: &str,
        arguments: &[Datum<'a>],
        argument_type_oids: &[i32],
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        if arguments.len() != argument_type_oids.len() || !(1..=2).contains(&arguments.len()) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "operator expression has an invalid argument contract"
            ));
        }
        let mut builtin_arguments = [Datum::Null; 2];
        let mut builtin_oids = [crate::sql::types::oid::UNKNOWN; 2];
        builtin_arguments[..arguments.len()].copy_from_slice(arguments);
        builtin_oids[..argument_type_oids.len()].copy_from_slice(argument_type_oids);
        if arguments.len() == 2 {
            let left = ColType::from_oid(builtin_oids[0]);
            let right = ColType::from_oid(builtin_oids[1]);
            match (left, right) {
                (None, Some(ColType::Geometry(known)))
                    if builtin_oids[0] == crate::sql::types::oid::UNKNOWN =>
                {
                    if let Some(kind) =
                        super::eval::funcs::geometry::unknown_operand_kind(name, known, true)
                    {
                        builtin_arguments[0] = super::eval::cast_to(
                            builtin_arguments[0],
                            ColType::Geometry(kind),
                            arena,
                        )?;
                        builtin_oids[0] = ColType::Geometry(kind).oid();
                    }
                }
                (Some(ColType::Geometry(known)), None)
                    if builtin_oids[1] == crate::sql::types::oid::UNKNOWN =>
                {
                    if let Some(kind) =
                        super::eval::funcs::geometry::unknown_operand_kind(name, known, false)
                    {
                        builtin_arguments[1] = super::eval::cast_to(
                            builtin_arguments[1],
                            ColType::Geometry(kind),
                            arena,
                        )?;
                        builtin_oids[1] = ColType::Geometry(kind).oid();
                    }
                }
                _ => {}
            }
        }
        if schema.is_none_or(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
            && let Some(result) = super::eval::funcs::geometry::operator(
                name,
                &builtin_arguments[..arguments.len()],
                &builtin_oids[..argument_type_oids.len()],
                arena,
            )
        {
            return result.map(Some);
        }
        let (left_oid, right_oid) = if argument_type_oids.len() == 1 {
            (None, Some(argument_type_oids[0]))
        } else {
            (Some(argument_type_oids[0]), Some(argument_type_oids[1]))
        };
        let Some(slot) = self
            .storage
            .operator_slot_for_oids(schema, name, left_oid, right_oid, self.txid)?
        else {
            return Ok(None);
        };
        let operator = self.storage.operator_for(slot, self.txid);
        let mut expected = [crate::storage::RoutineResult::TEXT; 2];
        let mut expected_count = 0usize;
        for argument in [operator.signature.left, operator.signature.right]
            .into_iter()
            .flatten()
        {
            expected[expected_count] = argument;
            expected_count += 1;
        }
        if expected_count != arguments.len() {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "operator catalog signature has an invalid arity"
            ));
        }
        let mut coerced = [Datum::Null; 2];
        for index in 0..expected_count {
            let expected_oid = self
                .storage
                .routine_type_oid(expected[index].ctype, expected[index].user_type, self.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "operator argument type identity is unavailable"
                    )
                })?;
            coerced[index] = if argument_type_oids[index] == expected_oid {
                self.coerce_result(arguments[index], expected[index], arena)?
            } else if let Some(value) = self.execute_defined_cast(
                argument_type_oids[index],
                expected_oid,
                arguments[index],
                super::eval::DefinedCastContext::Implicit,
                arena,
            )? {
                value
            } else {
                self.coerce_result(arguments[index], expected[index], arena)?
            };
        }
        let function_oid = operator
            .implementation
            .routine()
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_FUNCTION, "operator is only a shell"))?;
        let function = self
            .storage
            .routine_slot_by_oid(function_oid, self.txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator function does not exist"
                )
            })?;
        self.storage.require_routine_execute(function, self.txid)?;
        self.execute_routine(
            function,
            self.storage.routine_for(function, self.txid),
            &coerced[..expected_count],
            arena,
        )?
        .ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "operator function produced no scalar result"
            )
        })
        .map(Some)
    }

    fn execute_routine<'a>(
        &self,
        slot: usize,
        routine: crate::storage::RoutineDef,
        arguments: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        if routine.attributes.strict && arguments.iter().any(Datum::is_null) {
            return Ok(Some(Datum::Null));
        }
        let owner = crate::storage::SessionIdentity::bound(
            self.storage
                .capture_role_slot(routine.ownership.owner_to(self.txid).into(), self.txid)?,
        );
        let _security = routine
            .attributes
            .security_definer
            .then(|| super::eval::funcs::system::enter_current_user(owner));
        let _config = (!routine.configs().is_empty())
            .then(|| super::guc::enter_active_routine_configs(routine.configs()))
            .transpose()?;
        let (result_contract, record_columns) = match routine.kind {
            crate::storage::RoutineKind::Function { result }
            | crate::storage::RoutineKind::SetFunction { result } => (Some(result), None),
            crate::storage::RoutineKind::RecordFunction {
                set_returning: false,
            } => (
                None,
                Some(&routine.result_columns[..routine.result_column_count]),
            ),
            _ => unreachable!("routine call resolution returns functions only"),
        };
        let result_type = result_contract.map_or(ColType::Record, |result| result.ctype);
        let _type_sources = self.storage.select_type_definition_graph(
            routine
                .arguments()
                .iter()
                .filter_map(|argument| argument.user_type)
                .chain(result_contract.and_then(|result| result.user_type))
                .chain(
                    routine.result_columns[..routine.result_column_count]
                        .iter()
                        .filter_map(|column| column.user_type),
                ),
            self.txid,
            arena,
        )?;
        if routine.language == crate::storage::RoutineLanguage::PlPgSql {
            if let Some(invocations) = self.invocations {
                return invocations.resolve(
                    slot,
                    arguments,
                    self.statement_arena
                        .expect("routine invocation state has statement arena"),
                    arena,
                );
            }
            if let Some((invocations, statement_arena)) = active_routine_invocations() {
                return invocations.resolve(slot, arguments, statement_arena, arena);
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "PL/pgSQL functions require a resumable query executor"
            ));
        }
        let _formal_scope =
            super::exec::enter_routine_parameter_types(routine.arguments(), Some(self))?;
        let function_program = parse_stored_routine_function_program(
            routine.body_kind,
            routine.body.as_str(),
            self.routine_workspace,
            result_type == ColType::Void,
            routine.name.as_str(),
            routine.arguments(),
        )?;
        if routine_program_requires_mutable_execution(&function_program) {
            if let Some(invocations) = self.invocations {
                return invocations.resolve(
                    slot,
                    arguments,
                    self.statement_arena
                        .expect("routine invocation state has statement arena"),
                    arena,
                );
            }
            if let Some((invocations, statement_arena)) = active_routine_invocations() {
                return invocations.resolve(slot, arguments, statement_arena, arena);
            }
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "data-modifying SQL functions require a resumable query executor"
            ));
        }
        let track = matches!(
            super::guc::active_track_functions(),
            super::guc::TrackFunctions::All
        );
        let timed = super::statistics::begin_function_timing(track);
        if track
            && let Err(error) = self
                .storage
                .ensure_function_statistics(crate::storage::routine_oid(&routine))
        {
            let _ = super::statistics::finish_function_timing(timed);
            return Err(error);
        }
        let output = (|| -> Result<Option<Datum<'a>>, SqlError> {
            let mut parameters = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
            for (slot, argument) in arguments.iter().enumerate() {
                let encoded =
                    crate::sql::exec::encode_projected_pub(&[*argument], self.routine_workspace)?;
                parameters[slot] = crate::sql::exec::decode_projected_col_record(
                    encoded,
                    0,
                    self.routine_workspace,
                )?;
            }
            for step in function_program.preceding {
                let RoutinePrelude::Statement(statement) = step else {
                    let RoutinePrelude::Forbidden(statement) = step else {
                        unreachable!("routine prelude has two variants");
                    };
                    return Err(routine_forbidden_statement_error(statement));
                };
                let query = match statement {
                    Stmt::Select(query) => RoutineQuery::Select(query),
                    Stmt::SetQuery(query) => RoutineQuery::Set(query),
                    _ => unreachable!("mutable routine prelude was rejected above"),
                };
                execute_bound_routine_query(
                    &query,
                    self.storage,
                    slot,
                    routine,
                    self.txid,
                    self.routine_workspace,
                    &parameters[..arguments.len()],
                    true,
                    None,
                    &mut |_| Ok(()),
                )?;
            }
            let mut result = None;
            let result_query = match function_program.result {
                RoutineFunctionResult::Query(result_query) => result_query,
                RoutineFunctionResult::DataModification(_) => {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "data-modifying SQL function results require a resumable query executor"
                    ));
                }
                RoutineFunctionResult::Void(statement) => {
                    let query = match statement {
                        Stmt::Select(query) => RoutineQuery::Select(query),
                        Stmt::SetQuery(query) => RoutineQuery::Set(query),
                        _ => {
                            return Err(sql_err!(
                                sqlstate::FEATURE_NOT_SUPPORTED,
                                "data-modifying SQL functions require a resumable query executor"
                            ));
                        }
                    };
                    execute_bound_routine_query(
                        &query,
                        self.storage,
                        slot,
                        routine,
                        self.txid,
                        self.routine_workspace,
                        &parameters[..arguments.len()],
                        true,
                        None,
                        &mut |_| Ok(()),
                    )?;
                    return Ok(Some(Datum::Text("")));
                }
                RoutineFunctionResult::Forbidden(statement) => {
                    return Err(routine_forbidden_statement_error(statement));
                }
            };
            execute_bound_routine_query(
                result_query,
                self.storage,
                slot,
                routine,
                self.txid,
                self.routine_workspace,
                &parameters[..arguments.len()],
                true,
                None,
                &mut |values| {
                    let expected = record_columns.map_or(1, <[_]>::len);
                    if values.len() != expected {
                        return Err(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "SQL function query returns {} columns but {} were declared",
                            values.len(),
                            expected
                        ));
                    }
                    if result.is_some() {
                        return Ok(());
                    }
                    let encoded = crate::sql::exec::encode_projected_pub(values, arena)?;
                    result = Some(if let Some(columns) = record_columns {
                        let fields = arena
                            .alloc_slice_with(columns.len(), |_| super::types::RecordField {
                                name: "",
                                type_oid: super::types::oid::UNKNOWN,
                                value: Datum::Null,
                            })
                            .map_err(|_| arena_full())?;
                        for (index, (field, column)) in fields.iter_mut().zip(columns).enumerate() {
                            field.name = arena
                                .alloc_str(column.name.as_str())
                                .map_err(|_| arena_full())?;
                            field.type_oid = self
                                .storage
                                .routine_type_oid(column.ctype, column.user_type, self.txid)
                                .ok_or_else(|| {
                                    sql_err!(
                                        sqlstate::INTERNAL_ERROR,
                                        "routine result type identity is unavailable"
                                    )
                                })?;
                            field.value = crate::sql::exec::decode_projected_col_record(
                                encoded, index, arena,
                            )?;
                        }
                        Datum::Record(fields)
                    } else {
                        crate::sql::exec::decode_projected_col_record(encoded, 0, arena)?
                    });
                    Ok(())
                },
            )?;
            let result = result.unwrap_or(Datum::Null);
            let Some(result_contract) = result_contract else {
                return Ok(Some(result));
            };
            if result_contract.polymorphic_type().is_some() {
                Ok(Some(result))
            } else {
                super::eval::cast_to(result, result_type, arena).map(Some)
            }
        })();
        if let Some((total, own)) = super::statistics::finish_function_timing(timed)
            && output.is_ok()
        {
            self.storage.record_function_call(
                self.txid,
                crate::storage::routine_oid(&routine),
                total,
                own,
            )?;
        }
        output
    }
}

fn brin_index_slot(storage: &Storage, txid: u32, oid: i32) -> Result<usize, SqlError> {
    if let Some((schema, name)) = super::catalog::index_identity_by_oid(storage, txid, oid) {
        let Some(slot) = storage.index_slot(schema.as_str(), name.as_str(), txid) else {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "\"{}\" is not a BRIN index",
                name.as_str()
            ));
        };
        let index = storage
            .index_visible_to(slot, txid)
            .expect("resolved index slot remains visible");
        if index.method != crate::sql::ast::IndexAccessMethod::Brin
            || !matches!(
                index.mutable_for(txid).kind,
                crate::storage::IndexKind::Ordinary
            )
        {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "\"{}\" is not a BRIN index",
                name.as_str()
            ));
        }
        storage.require_owner(
            crate::storage::AccessObject {
                class: crate::storage::AccessClass::Index,
                slot: slot as u16,
            },
            txid,
            "index",
        )?;
        return Ok(slot);
    }
    for slot in 0..storage.table_count() {
        if storage.table_slot_visible_to(slot, txid)
            && super::catalog::user_table_oid(storage, slot) == oid
        {
            return Err(sql_err!(
                sqlstate::WRONG_OBJECT_TYPE,
                "\"{}\" is not an index",
                storage.table_def(slot, txid).name.as_str()
            ));
        }
    }
    Err(sql_err!(
        sqlstate::UNDEFINED_TABLE,
        "relation with OID {} does not exist",
        oid
    ))
}

impl super::eval::CatalogAccess for StorageCatalog<'_, '_, '_, '_> {
    fn current_backend_pid(&self) -> Result<i32, SqlError> {
        let pid = self.storage.current_connection_id();
        if pid <= 0 {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "backend identity access is unavailable"
            ));
        }
        Ok(pid)
    }

    fn blocking_backend_pids(&self, backend_pid: i32, output: &mut [i32]) -> usize {
        self.storage.blocking_backend_pids(backend_pid, output)
    }

    fn blocking_backend_pid_count(&self, backend_pid: i32) -> usize {
        self.storage.blocking_backend_pid_count(backend_pid)
    }

    fn signal_backend(&self, backend_pid: i32, terminate: bool) -> Result<bool, SqlError> {
        self.storage
            .request_backend_signal(backend_pid, terminate, self.txid)
    }

    fn listening_channel_count(&self) -> usize {
        self.storage
            .listening_channel_count(self.storage.current_connection_id())
    }

    fn listening_channel_at(&self, index: usize) -> Option<crate::sql::notify::Channel> {
        self.storage
            .listening_channel_at(self.storage.current_connection_id(), index)
    }

    fn acquire_advisory_lock(
        &self,
        class_id: u32,
        object_id: u32,
        object_sub_id: i16,
        shared: bool,
        transaction_scope: bool,
        try_only: bool,
    ) -> Result<bool, SqlError> {
        let decision = self.storage.acquire_advisory_lock(
            self.txid,
            crate::sql::lock::AdvisoryKey {
                class_id,
                object_id,
                object_sub_id,
            },
            if shared {
                crate::sql::lock::AdvisoryMode::Shared
            } else {
                crate::sql::lock::AdvisoryMode::Exclusive
            },
            transaction_scope,
            try_only,
        )?;
        match decision {
            crate::sql::lock::AdvisoryDecision::Acquired => Ok(true),
            crate::sql::lock::AdvisoryDecision::Unavailable => Ok(false),
            crate::sql::lock::AdvisoryDecision::Waiting => Err(sql_err!(
                sqlstate::INTERNAL_LOCK_WAIT,
                "statement is waiting for an advisory lock"
            )),
        }
    }

    fn unlock_advisory_lock(
        &self,
        class_id: u32,
        object_id: u32,
        object_sub_id: i16,
        shared: bool,
    ) -> Result<bool, SqlError> {
        self.storage.unlock_advisory_session(
            crate::sql::lock::AdvisoryKey {
                class_id,
                object_id,
                object_sub_id,
            },
            if shared {
                crate::sql::lock::AdvisoryMode::Shared
            } else {
                crate::sql::lock::AdvisoryMode::Exclusive
            },
        )
    }

    fn unlock_all_advisory_locks(&self) -> Result<(), SqlError> {
        self.storage.unlock_all_advisory_session()
    }

    fn current_transaction_id(&self, assign: bool) -> Result<Option<u64>, SqlError> {
        Ok(if assign {
            Some(self.storage.assign_transaction_identity(self.txid))
        } else {
            self.storage.assigned_transaction_identity(self.txid)
        })
    }

    fn brin_summarize_new_values(&self, index_oid: i32) -> Result<i32, SqlError> {
        let slot = brin_index_slot(self.storage, self.txid, index_oid)?;
        self.storage.brin_summarize_new_values(slot, self.txid)
    }

    fn brin_summarize_range(&self, index_oid: i32, block: u64) -> Result<i32, SqlError> {
        let slot = brin_index_slot(self.storage, self.txid, index_oid)?;
        self.storage.brin_summarize_range(slot, block, self.txid)
    }

    fn brin_desummarize_range(&self, index_oid: i32, block: u64) -> Result<(), SqlError> {
        let slot = brin_index_slot(self.storage, self.txid, index_oid)?;
        self.storage.brin_desummarize_range(slot, block, self.txid)
    }

    fn current_transaction_snapshot<'a>(
        &self,
        arena: &'a Arena,
    ) -> Result<super::snapshot::Snapshot<'a>, SqlError> {
        self.storage.current_transaction_snapshot(self.txid, arena)
    }

    fn transaction_status(&self, transaction_id: u64) -> Result<Option<&'static str>, SqlError> {
        self.storage.transaction_status(transaction_id)
    }

    fn transaction_id_age(&self, transaction_id: u32) -> Result<i32, SqlError> {
        if transaction_id < 3 {
            return Ok(i32::MAX);
        }
        let next = self.storage.latest_transaction_identity().wrapping_add(1) as u32;
        Ok(next.wrapping_sub(transaction_id) as i32)
    }

    fn replica_identity_index_oid(&self, relation_oid: i32) -> Option<i32> {
        super::catalog::replica_identity_index_oid(self.storage, self.txid, relation_oid)
    }

    fn resolve_collation(&self, schema: Option<&str>, name: &str) -> Option<super::ast::Collation> {
        self.storage.resolve_collation(schema, name, self.txid)
    }

    fn collation_oid(&self, name: &str) -> Option<i32> {
        super::catalog::collation_oid_by_name(self.storage, self.txid, name)
    }

    fn collation_name<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::collation_name_by_oid(self.storage, self.txid, oid, arena)
    }

    fn resolve_text_search_configuration(&self, schema: Option<&str>, name: &str) -> Option<i32> {
        let slot = self.storage.text_search_slot_on_path(
            super::ast::TextSearchObjectKind::Configuration,
            schema,
            name,
            self.txid,
        )?;
        Some(
            self.storage
                .text_search_object(slot)
                .definition_for(self.txid)
                .oid(),
        )
    }

    fn rewrite_text_search_query<'a>(
        &self,
        source: &str,
        query_sql: &str,
        arena: &'a Arena,
    ) -> Result<&'a str, SqlError> {
        let query = super::parser::parse_query(query_sql, arena)?;
        let query = RoutineQuery::Select(query);
        let source = arena.alloc_str(source).map_err(|_| arena_full())?;
        let mut rewritten = super::full_text::canonical_query(source, arena)?;
        execute_routine_query(
            &query,
            self.storage,
            self.txid,
            arena,
            &[],
            false,
            &mut |row| {
                if row.len() != 2 {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_rewrite query must return two tsquery columns"
                    ));
                }
                let (Datum::TsQuery(target), Datum::TsQuery(replacement)) = (row[0], row[1]) else {
                    return Err(sql_err!(
                        sqlstate::DATATYPE_MISMATCH,
                        "ts_rewrite query must return two tsquery columns"
                    ));
                };
                rewritten = super::full_text::rewrite_query(
                    rewritten,
                    target.as_str(),
                    replacement.as_str(),
                    arena,
                )?;
                Ok(())
            },
        )?;
        Ok(rewritten)
    }

    fn text_search_configuration_name(&self, oid: i32) -> Option<crate::util::StackStr<128>> {
        let slot = self.storage.text_search_slot_by_oid(
            super::ast::TextSearchObjectKind::Configuration,
            oid,
            self.txid,
        )?;
        let definition = self
            .storage
            .text_search_object(slot)
            .definition_for(self.txid);
        let mut output = crate::util::StackStr::new();
        if definition.schema().as_str() != "pg_catalog"
            && !self
                .storage
                .schema_is_on_path(definition.schema(), self.txid)
        {
            use core::fmt::Write as _;
            write!(output, "{}.", definition.schema().as_str()).ok()?;
        }
        use core::fmt::Write as _;
        write!(output, "{}", definition.name().as_str()).ok()?;
        (!output.is_truncated()).then_some(output)
    }

    fn resolve_text_search_dictionary(&self, schema: Option<&str>, name: &str) -> Option<i32> {
        let slot = self.storage.text_search_slot_on_path(
            super::ast::TextSearchObjectKind::Dictionary,
            schema,
            name,
            self.txid,
        )?;
        Some(
            self.storage
                .text_search_object(slot)
                .definition_for(self.txid)
                .oid(),
        )
    }

    fn text_search_dictionary_name(&self, oid: i32) -> Option<crate::util::StackStr<128>> {
        let slot = self.storage.text_search_slot_by_oid(
            super::ast::TextSearchObjectKind::Dictionary,
            oid,
            self.txid,
        )?;
        let definition = self
            .storage
            .text_search_object(slot)
            .definition_for(self.txid);
        let mut output = crate::util::StackStr::new();
        if definition.schema().as_str() != "pg_catalog"
            && !self
                .storage
                .schema_is_on_path(definition.schema(), self.txid)
        {
            use core::fmt::Write as _;
            write!(output, "{}.", definition.schema().as_str()).ok()?;
        }
        use core::fmt::Write as _;
        write!(output, "{}", definition.name().as_str()).ok()?;
        (!output.is_truncated()).then_some(output)
    }

    fn lexize_text_search_dictionary<'a>(
        &self,
        dictionary_oid: i32,
        token: &str,
        arena: &'a Arena,
    ) -> Result<super::full_text::TextSearchLexeme<'a>, SqlError> {
        let slot = self
            .storage
            .text_search_slot_by_oid(
                super::ast::TextSearchObjectKind::Dictionary,
                dictionary_oid,
                self.txid,
            )
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "text search dictionary with OID {} does not exist",
                    dictionary_oid
                )
            })?;
        let super::super::storage::TextSearchDefinition::Dictionary { behavior, .. } = self
            .storage
            .text_search_object(slot)
            .definition_for(self.txid)
        else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid text-search dictionary catalog entry"
            ));
        };
        match behavior {
            super::super::storage::TextSearchDictionaryBehavior::Simple { accept: false } => {
                Ok(super::full_text::TextSearchLexeme::Unmapped)
            }
            super::super::storage::TextSearchDictionaryBehavior::Simple { accept: true } => Ok(
                match super::full_text::normalize_token(
                    token,
                    super::full_text::TextSearchConfig::Simple,
                    arena,
                )? {
                    Some(lexeme) => super::full_text::TextSearchLexeme::Lexeme(lexeme),
                    None => super::full_text::TextSearchLexeme::StopWord,
                },
            ),
            super::super::storage::TextSearchDictionaryBehavior::EnglishStem => Ok(
                match super::full_text::normalize_token(
                    token,
                    super::full_text::TextSearchConfig::English,
                    arena,
                )? {
                    Some(lexeme) => super::full_text::TextSearchLexeme::Lexeme(lexeme),
                    None => super::full_text::TextSearchLexeme::StopWord,
                },
            ),
        }
    }

    fn normalize_text_search_token<'a>(
        &self,
        configuration_oid: i32,
        token_type: u8,
        token: &str,
        arena: &'a Arena,
    ) -> Result<super::full_text::TextSearchLexeme<'a>, SqlError> {
        let slot = self
            .storage
            .text_search_slot_by_oid(
                super::ast::TextSearchObjectKind::Configuration,
                configuration_oid,
                self.txid,
            )
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_OBJECT,
                    "text search configuration with OID {} does not exist",
                    configuration_oid
                )
            })?;
        let super::super::storage::TextSearchDefinition::Configuration { mappings, .. } = self
            .storage
            .text_search_object(slot)
            .definition_for(self.txid)
        else {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid text-search configuration catalog entry"
            ));
        };
        let token_index = usize::from(token_type.checked_sub(1).ok_or_else(|| {
            sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid text-search parser token type"
            )
        })?);
        if token_index >= super::super::storage::TEXT_SEARCH_TOKEN_TYPES {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid text-search parser token type"
            ));
        }
        for dictionary_oid in mappings.dictionaries[token_index]
            .iter()
            .take(usize::from(mappings.counts[token_index]))
        {
            let dictionary_slot = self
                .storage
                .text_search_slot_by_oid(
                    super::ast::TextSearchObjectKind::Dictionary,
                    *dictionary_oid,
                    self.txid,
                )
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "text-search configuration references a missing dictionary"
                    )
                })?;
            let super::super::storage::TextSearchDefinition::Dictionary { behavior, .. } = self
                .storage
                .text_search_object(dictionary_slot)
                .definition_for(self.txid)
            else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "invalid text-search dictionary catalog entry"
                ));
            };
            match behavior {
                super::super::storage::TextSearchDictionaryBehavior::Simple { accept: false } => {}
                super::super::storage::TextSearchDictionaryBehavior::Simple { accept: true } => {
                    return Ok(
                        match super::full_text::normalize_token(
                            token,
                            super::full_text::TextSearchConfig::Simple,
                            arena,
                        )? {
                            Some(lexeme) => super::full_text::TextSearchLexeme::Lexeme(lexeme),
                            None => super::full_text::TextSearchLexeme::StopWord,
                        },
                    );
                }
                super::super::storage::TextSearchDictionaryBehavior::EnglishStem => {
                    return Ok(
                        match super::full_text::normalize_token(
                            token,
                            super::full_text::TextSearchConfig::English,
                            arena,
                        )? {
                            Some(lexeme) => super::full_text::TextSearchLexeme::Lexeme(lexeme),
                            None => super::full_text::TextSearchLexeme::StopWord,
                        },
                    );
                }
            }
        }
        Ok(super::full_text::TextSearchLexeme::Unmapped)
    }

    fn has_operator_candidate(&self, name: &str) -> bool {
        self.storage
            .operators_visible_to(self.txid)
            .any(|(_, operator)| {
                operator.name.as_str() == name
                    && self.storage.schema_is_on_path(operator.schema, self.txid)
            })
    }

    fn statistics_definition<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::extended_statistics_definition_by_oid(self.storage, self.txid, oid, arena)
    }
    fn statistics_expressions<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        super::catalog::extended_statistics_expressions_by_oid(self.storage, self.txid, oid, arena)
    }
    fn statistics_columns<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::extended_statistics_columns_by_oid(self.storage, self.txid, oid, arena)
    }
    fn tablespace_location<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::tablespace_location_by_oid(self.storage, self.txid, oid, arena)
    }
    fn materialize_enum<'a>(
        &self,
        value: Datum<'a>,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        super::enum_values::resolve(value, self.storage, self.txid, arena)
    }

    fn enum_member_identity(
        &self,
        slot: crate::storage::TypeCreation,
        label: &str,
    ) -> Result<Option<super::types::EnumMemberId>, SqlError> {
        let Some(definition) = self.storage.read_enum_definition(
            crate::storage::TypeDefinitionTarget::by_creation(
                self.storage,
                crate::storage::NamedTypeKind::Enum,
                slot,
            )?,
            self.txid,
        )?
        else {
            return Ok(None);
        };
        Ok(definition
            .members()
            .position(|member| member.label.as_str() == label)
            .map(|index| {
                super::types::EnumMemberId::from_ordinal(index as u32)
                    .expect("validated enum member ordinal")
            }))
    }

    fn materialize_composite<'a>(
        &self,
        slot: crate::storage::TypeCreation,
        physical_fields: u16,
        text: &'a str,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        super::exec::decode_stored_composite_text(
            text,
            slot,
            physical_fields,
            self.storage,
            self.txid,
            arena,
        )
    }

    fn composite_array_element<'a>(
        &self,
        value: Datum<'a>,
        slot: crate::storage::TypeCreation,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        super::exec::coerce_user_type_array_element(
            value,
            super::types::ArrElem::Composite(Some(slot)),
            self.storage,
            self.txid,
            arena,
        )
    }

    fn null_composite_fields<'a>(
        &self,
        type_oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a [super::types::RecordField<'a>]>, SqlError> {
        use super::types::oid;
        if (oid::FIRST_TABLE_COMPOSITE..oid::FIRST_TABLE_COMPOSITE_ARRAY).contains(&type_oid) {
            return self.rowtype_fields(type_oid, arena).map(Some);
        }
        let Some((crate::storage::NamedTypeKind::Composite, creation, false)) =
            crate::storage::NamedTypeKind::from_oid(type_oid)
        else {
            return Ok(None);
        };
        let target = crate::storage::TypeDefinitionTarget::by_creation(
            self.storage,
            crate::storage::NamedTypeKind::Composite,
            creation,
        )?;
        let Some(definition) = self.storage.read_composite_definition(target, self.txid)? else {
            return Ok(None);
        };
        let fields = arena
            .alloc_slice_with(definition.active_field_count(), |_| {
                super::types::RecordField {
                    name: "",
                    type_oid: super::types::oid::UNKNOWN,
                    value: Datum::Null,
                }
            })
            .map_err(|_| arena_full())?;
        for (out, field) in fields.iter_mut().zip(definition.active_fields()) {
            out.name = arena
                .alloc_str(field.name.as_str())
                .map_err(|_| arena_full())?;
            out.type_oid = match field.user_type {
                Some(identity) => self
                    .storage
                    .user_type_identity_oid(
                        identity,
                        matches!(field.ctype, ColType::Array(_)),
                        self.txid,
                    )
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "composite field type identity is unavailable"
                        )
                    })?,
                None => field.ctype.oid(),
            };
        }
        Ok(Some(fields))
    }

    fn composite_base_type_oid(&self, type_oid: i32) -> Option<i32> {
        if super::catalog::table_slot_by_row_type_oid(self.storage, self.txid, type_oid, false)
            .is_some()
            || super::catalog::view_slot_by_row_type_oid(self.storage, self.txid, type_oid, false)
                .is_some()
        {
            return Some(type_oid);
        }
        let result = self.storage.routine_result_for_oid(type_oid, self.txid)?;
        match result.ctype {
            ColType::Composite(Some(creation)) => Some(super::types::oid::composite_oid(creation)),
            _ => None,
        }
    }

    fn composite_field_type_mods<'a>(
        &self,
        type_oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a [i32]>, SqlError> {
        if let Some(slot) =
            super::catalog::table_slot_by_row_type_oid(self.storage, self.txid, type_oid, false)
        {
            let definition = self.storage.table_def(slot, self.txid);
            return arena
                .alloc_slice_with(definition.n_columns, |index| {
                    definition.columns()[index].type_mod
                })
                .map(|modifiers| Some(&*modifiers))
                .map_err(|_| arena_full());
        }
        if let Some(slot) =
            super::catalog::view_slot_by_row_type_oid(self.storage, self.txid, type_oid, false)
        {
            let view = self.storage.view(slot);
            let width = view.columns_for(self.txid).names().len();
            let modifiers = arena
                .alloc_slice_with(width, |_| -1)
                .map_err(|_| arena_full())?;
            let mark = arena.mark();
            let result: Result<usize, SqlError> = (|| {
                let columns = arena
                    .alloc_slice_with(width, |_| super::types::ColDesc::new("", 0, 0))
                    .map_err(|_| arena_full())?;
                let count =
                    super::catalog::describe_view(self.storage, self.txid, &view, arena, columns)?;
                for (modifier, column) in modifiers.iter_mut().zip(&columns[..count]) {
                    *modifier = column.type_mod;
                }
                Ok(count)
            })();
            // Only modifiers allocated below the mark survive the temporary description.
            unsafe { arena.rewind_to(mark) };
            return Ok(Some(&modifiers[..result?]));
        }
        let Some((crate::storage::NamedTypeKind::Composite, creation, false)) =
            crate::storage::NamedTypeKind::from_oid(type_oid)
        else {
            return Ok(None);
        };
        let target = crate::storage::TypeDefinitionTarget::by_creation(
            self.storage,
            crate::storage::NamedTypeKind::Composite,
            creation,
        )?;
        let Some(definition) = self.storage.read_composite_definition(target, self.txid)? else {
            return Ok(None);
        };
        let modifiers = arena
            .alloc_slice_with(definition.active_field_count(), |_| -1)
            .map_err(|_| arena_full())?;
        for (modifier, field) in modifiers.iter_mut().zip(definition.active_fields()) {
            *modifier = field.type_mod;
        }
        Ok(Some(modifiers))
    }

    fn is_composite_type_name(&self, type_name: &str) -> bool {
        let type_name = type_name.strip_suffix("[]").unwrap_or(type_name);
        if self
            .storage
            .resolve_composite_slot(type_name, self.txid)
            .is_some()
        {
            return true;
        }
        let (qualifier, bare) = type_name
            .split_once('.')
            .map_or((None, type_name), |(schema, name)| (Some(schema), name));
        if self
            .storage
            .resolve_relation(qualifier, bare, self.txid)
            .is_some()
        {
            return true;
        }
        self.storage
            .resolve_domain_slot(type_name, self.txid)
            .is_some_and(|slot| {
                matches!(
                    self.storage.domain_for(slot, self.txid).base,
                    ColType::Composite(_)
                )
            })
    }

    fn compare_text(
        &self,
        collation: super::ast::Collation,
        left: &str,
        right: &str,
    ) -> Result<core::cmp::Ordering, SqlError> {
        self.storage.compare_text(collation, left, right)
    }

    fn uses_full_unicode_case_mapping(
        &self,
        collation: super::ast::Collation,
    ) -> Result<bool, SqlError> {
        Ok(match collation {
            super::ast::Collation::PgUnicodeFast => true,
            super::ast::Collation::Catalog(selected) => selected.unicode_case(),
            _ => false,
        })
    }

    fn convert_encoding<'a>(
        &self,
        source: crate::storage::PgEncoding,
        destination: crate::storage::PgEncoding,
        input: &[u8],
        arena: &'a Arena,
    ) -> Result<&'a [u8], SqlError> {
        let procedure = if source == destination {
            None
        } else if source == crate::storage::PgEncoding::UTF8
            && destination == crate::storage::PgEncoding::LATIN1
        {
            Some(4375)
        } else if source == crate::storage::PgEncoding::LATIN1
            && destination == crate::storage::PgEncoding::UTF8
        {
            Some(4374)
        } else {
            self.storage
                .default_conversion(source, destination, self.txid)
                .map(|definition| definition.procedure)
        };
        super::eval::funcs::bytea::convert_encoding(source, destination, procedure, input, arena)
    }

    fn has_routine_candidate(&self, name: &str, arity: usize) -> bool {
        self.storage
            .has_function_routine_candidate(name, arity, self.txid)
    }

    fn resolve_routine_call<'name>(
        &self,
        name: &'name str,
        argument_names: &[Option<&str>],
        variadic: bool,
        argument_type_oids: &[i32],
    ) -> Result<Option<super::eval::RoutineCallTarget<'name>>, SqlError> {
        if let Some((schema, operator)) = super::ast::catalog_operator_call(name) {
            return Ok(Some(super::eval::RoutineCallTarget::Operator {
                schema,
                name: operator,
            }));
        }
        let Some(slot) = (if argument_names.is_empty() {
            self.storage.routine_slot_for_call_syntax_oids(
                name,
                argument_type_oids,
                variadic,
                self.txid,
            )
        } else {
            self.storage.routine_slot_for_named_call_oids(
                name,
                argument_names,
                argument_type_oids,
                self.txid,
            )
        }) else {
            return Ok(None);
        };
        let declared = self.storage.routine_for(slot, self.txid);
        let argument_mapping = declared
            .call_input_mapping(argument_names, argument_type_oids.len(), variadic)
            .expect("resolved routine call has a valid argument mapping");
        Ok(Some(super::eval::RoutineCallTarget::Stored {
            oid: crate::storage::routine_oid(&declared),
            argument_mapping,
            variadic,
        }))
    }

    fn routine_evaluation(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        variadic: bool,
        argument_type_oids: &[i32],
    ) -> Result<Option<super::eval::ExpressionEvaluation>, SqlError> {
        let Some(super::eval::RoutineCallTarget::Stored { oid, .. }) =
            self.resolve_routine_call(name, argument_names, variadic, argument_type_oids)?
        else {
            return Ok(None);
        };
        let slot = self
            .storage
            .routine_slot_by_oid(oid, self.txid)
            .expect("resolved routine identity exists");
        let routine = self.storage.routine_for(slot, self.txid);
        Ok(Some(
            if routine.attributes.volatility == crate::storage::RoutineVolatility::Immutable
                && !routine.kind.is_set_returning()
            {
                super::eval::ExpressionEvaluation::Immutable
            } else {
                super::eval::ExpressionEvaluation::Runtime
            },
        ))
    }

    fn operator_evaluation(
        &self,
        schema: Option<&str>,
        name: &str,
        arguments: &[i32],
    ) -> Result<Option<super::eval::ExpressionEvaluation>, SqlError> {
        if !(1..=2).contains(&arguments.len()) {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "invalid operator input contract"
            ));
        }
        let (left, right) = if arguments.len() == 1 {
            (None, Some(arguments[0]))
        } else {
            (Some(arguments[0]), Some(arguments[1]))
        };
        let Some(slot) = self
            .storage
            .operator_slot_for_oids(schema, name, left, right, self.txid)?
        else {
            return Ok(None);
        };
        let operator = self.storage.operator_for(slot, self.txid);
        for (actual, expected) in [
            (left, operator.signature.left),
            (right, operator.signature.right),
        ] {
            let (Some(actual), Some(expected)) = (actual, expected) else {
                continue;
            };
            let expected = self
                .storage
                .routine_type_oid(expected.ctype, expected.user_type, self.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "operator input type identity is unavailable"
                    )
                })?;
            if actual != expected
                && (matches!(
                    crate::storage::NamedTypeKind::from_oid(expected),
                    Some((crate::storage::NamedTypeKind::Domain, _, _))
                ) || self.cast_evaluation(actual, expected)?
                    == Some(super::eval::ExpressionEvaluation::Runtime))
            {
                return Ok(Some(super::eval::ExpressionEvaluation::Runtime));
            }
        }
        let Some(oid) = operator.implementation.routine() else {
            return Ok(Some(super::eval::ExpressionEvaluation::Runtime));
        };
        let slot = self
            .storage
            .routine_slot_by_oid(oid, self.txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "operator function does not exist"
                )
            })?;
        let routine = self.storage.routine_for(slot, self.txid);
        Ok(Some(
            if routine.attributes.volatility == crate::storage::RoutineVolatility::Immutable {
                super::eval::ExpressionEvaluation::Immutable
            } else {
                super::eval::ExpressionEvaluation::Runtime
            },
        ))
    }

    fn call_routine<'a>(
        &self,
        target: super::eval::RoutineCallTarget<'_>,
        arguments: &[&'a Expr<'a>],
        argument_type_oids: &[i32],
        evaluate: &mut dyn FnMut(&'a Expr<'a>) -> Result<Datum<'a>, SqlError>,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        let (oid, mapping, variadic) = match target {
            super::eval::RoutineCallTarget::Stored {
                oid,
                argument_mapping,
                variadic,
            } => (oid, argument_mapping, variadic),
            super::eval::RoutineCallTarget::Operator { schema, name } => {
                let values = arena
                    .alloc_slice_with(arguments.len(), |_| Datum::Null)
                    .map_err(|_| arena_full())?;
                for (value, expression) in values.iter_mut().zip(arguments) {
                    *value = evaluate(expression)?;
                }
                return self
                    .execute_catalog_operator(schema, name, values, argument_type_oids, arena)?
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "operator does not exist: {}",
                            name
                        )
                    });
            }
        };
        let slot = self
            .storage
            .routine_slot_by_oid(oid, self.txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "function with OID {} does not exist",
                    oid
                )
            })?;
        self.storage.require_routine_execute(slot, self.txid)?;
        let declared = self.storage.routine_for(slot, self.txid);
        let mut completed = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut completed_type_oids =
            [super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let mut variadic_values = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        // PostgreSQL orders named inputs and inserted defaults by the declared
        // signature before execution; spelling order cannot reorder effects.
        for input_index in 0..declared.argument_count {
            let parameter = declared
                .parameter_for_input(input_index)
                .expect("input signature has a declared parameter");
            let mut supplied = false;
            let mut variadic_count = 0usize;
            let expanded_variadic = !variadic
                && matches!(
                    parameter.mode,
                    crate::storage::RoutineParameterMode::Variadic { .. }
                );
            for (call_index, expression) in arguments.iter().enumerate() {
                if usize::from(mapping[call_index]) != input_index {
                    continue;
                }
                let value = evaluate(expression)?;
                supplied = true;
                if expanded_variadic {
                    variadic_values[variadic_count] = value;
                    variadic_count += 1;
                } else {
                    completed[input_index] = value;
                    completed_type_oids[input_index] = argument_type_oids[call_index];
                }
            }
            if expanded_variadic && supplied {
                let ColType::Array(element) = parameter.ctype else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "variadic parameter is not an array"
                    ));
                };
                completed[input_index] = Datum::Array {
                    element,
                    raw: super::array::build(&variadic_values[..variadic_count], arena)?,
                };
            } else if supplied {
                continue;
            } else if let Some(default) = parameter.mode.default() {
                let source = arena
                    .alloc_str(default.as_str())
                    .map_err(|_| arena_full())?;
                let expression = super::parser::parse_expression(source, arena)?;
                completed[input_index] =
                    super::eval::cast_to(evaluate(expression)?, parameter.ctype, arena)?;
            } else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "resolved routine is missing a required argument"
                ));
            }
            completed_type_oids[input_index] = self
                .storage
                .routine_type_oid(parameter.ctype, parameter.user_type, self.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "routine parameter type identity is unavailable"
                    )
                })?;
        }
        let routine = self
            .storage
            .routine_for_bound_call(
                slot,
                &completed_type_oids[..declared.argument_count],
                self.txid,
            )
            .expect("resolved routine call has a valid polymorphic binding");
        for (index, argument) in routine.arguments().iter().copied().enumerate() {
            let expected_oid = self
                .storage
                .routine_type_oid(argument.ctype, argument.user_type, self.txid)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "routine argument type identity is unavailable"
                    )
                })?;
            completed[index] = if completed_type_oids[index] != expected_oid {
                match self.execute_defined_cast(
                    completed_type_oids[index],
                    expected_oid,
                    completed[index],
                    super::eval::DefinedCastContext::Implicit,
                    arena,
                )? {
                    Some(value) => value,
                    None => super::exec::coerce_routine_argument(
                        completed[index],
                        argument,
                        self.storage,
                        self.txid,
                        arena,
                    )?,
                }
            } else {
                super::exec::coerce_routine_argument(
                    completed[index],
                    argument,
                    self.storage,
                    self.txid,
                    arena,
                )?
            };
        }
        self.execute_routine(slot, routine, &completed[..declared.argument_count], arena)?
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "resolved routine has no scalar result"
                )
            })
    }

    fn call_routine_oid<'a>(
        &self,
        oid: i32,
        arguments: &[Datum<'a>],
        argument_type_oids: &[i32],
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        let Some(slot) = self.storage.routine_slot_by_oid(oid, self.txid) else {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "aggregate support function with OID {} does not exist",
                oid
            ));
        };
        let routine = self.storage.routine_for(slot, self.txid);
        if arguments.len() != routine.argument_count || argument_type_oids.len() != arguments.len()
        {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "aggregate support function argument contract changed"
            ));
        }
        let routine = self
            .storage
            .routine_for_bound_call(slot, argument_type_oids, self.txid)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "aggregate support function polymorphic contract does not match its arguments"
                )
            })?;
        self.execute_routine(slot, routine, arguments, arena)
    }

    fn cast_evaluation(
        &self,
        source_oid: i32,
        target_oid: i32,
    ) -> Result<Option<super::eval::ExpressionEvaluation>, SqlError> {
        let Some((_, cast)) = self
            .storage
            .cast_for_oids(source_oid, target_oid, self.txid)
        else {
            return Ok(None);
        };
        let immutable = match cast.method {
            crate::storage::CastMethod::Binary => true,
            crate::storage::CastMethod::InOut => false,
            crate::storage::CastMethod::Function(oid) => {
                let slot = self
                    .storage
                    .routine_slot_by_oid(oid, self.txid)
                    .ok_or_else(|| {
                        sql_err!(sqlstate::UNDEFINED_FUNCTION, "cast function does not exist")
                    })?;
                self.storage
                    .routine_for(slot, self.txid)
                    .attributes
                    .volatility
                    == crate::storage::RoutineVolatility::Immutable
            }
        };
        Ok(Some(if immutable {
            super::eval::ExpressionEvaluation::Immutable
        } else {
            super::eval::ExpressionEvaluation::Runtime
        }))
    }

    fn cast_defined<'a>(
        &self,
        source_oid: i32,
        target_oid: i32,
        value: Datum<'a>,
        context: super::eval::DefinedCastContext,
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        self.execute_defined_cast(source_oid, target_oid, value, context, arena)
    }

    fn call_operator<'a>(
        &self,
        name: &str,
        left: Datum<'a>,
        right: Datum<'a>,
        left_oid: i32,
        right_oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        self.execute_catalog_operator(None, name, &[left, right], &[left_oid, right_oid], arena)
    }

    fn subquery_type(
        &self,
        expression: &Expr<'_>,
        outer: &dyn ColumnLookup<'_>,
    ) -> Result<Option<i32>, SqlError> {
        let mark = self.routine_workspace.mark();
        let result = subquery::subquery_expression_type(
            expression,
            self.storage,
            self.txid,
            self.routine_workspace,
            outer,
        );
        // Describing a result never exposes this workspace's temporary bytes.
        unsafe {
            self.routine_workspace.rewind_to(mark);
        }
        result
    }

    fn routine_result_oid(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        variadic: bool,
        argument_type_oids: &[i32],
    ) -> Option<i32> {
        if let Some((schema, operator)) = super::ast::catalog_operator_call(name) {
            if !(1..=2).contains(&argument_type_oids.len()) {
                return None;
            }
            if schema.is_none_or(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
                && let Some(ctype) =
                    super::eval::funcs::geometry::operator_result(operator, argument_type_oids)
            {
                return Some(ctype.oid());
            }
            if argument_type_oids.len() == 2
                && schema.is_some_and(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
            {
                return builtin_operator_result(operator, argument_type_oids).map(ColType::oid);
            }
            let (left_oid, right_oid) = if argument_type_oids.len() == 1 {
                (None, Some(argument_type_oids[0]))
            } else {
                (Some(argument_type_oids[0]), Some(argument_type_oids[1]))
            };
            let slot = self
                .storage
                .operator_slot_for_oids(schema, operator, left_oid, right_oid, self.txid)
                .ok()??;
            let result = self
                .storage
                .operator_for(slot, self.txid)
                .implementation
                .result()?;
            return self
                .storage
                .routine_type_oid(result.ctype, result.user_type, self.txid);
        }
        let routine = if argument_names.is_empty() {
            self.storage.function_for_call_syntax_oids(
                name,
                argument_type_oids,
                variadic,
                self.txid,
            )
        } else {
            self.storage.function_for_named_call_oids(
                name,
                argument_names,
                argument_type_oids,
                self.txid,
            )
        };
        if let Some(routine) = routine {
            return self
                .storage
                .routine_function_result_oid(&routine, self.txid);
        }
        if !argument_names.is_empty() || variadic {
            return None;
        }
        let (_, _, aggregate) =
            self.storage
                .aggregate_for_call_oids(name, argument_type_oids, self.txid)?;
        self.storage.routine_type_oid(
            aggregate.result_type.ctype,
            aggregate.result_type.user_type,
            self.txid,
        )
    }

    fn sequence_state_by_oid(&self, oid: i32) -> Option<(i64, bool)> {
        super::catalog::sequence_state_by_oid(self.storage, oid, self.txid)
    }

    fn relation_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::relation_oid_visibility(self.storage, self.txid, oid)
    }

    fn type_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::type_oid_visibility(self.storage, self.txid, oid)
    }

    fn function_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::function_oid_visibility(self.storage, self.txid, oid)
    }

    fn operator_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::operator_oid_visibility(self.storage, self.txid, oid)
    }

    fn operator_class_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::operator_class_oid_visibility(self.storage, self.txid, oid)
    }

    fn operator_family_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::operator_family_oid_visibility(self.storage, self.txid, oid)
    }

    fn conversion_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::conversion_oid_visibility(self.storage, self.txid, oid)
    }

    fn statistics_object_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::statistics_oid_visibility(self.storage, self.txid, oid)
    }

    fn text_search_dictionary_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::text_search_oid_visibility(
            self.storage,
            self.txid,
            oid,
            super::ast::TextSearchObjectKind::Dictionary,
        )
    }

    fn text_search_configuration_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::text_search_oid_visibility(
            self.storage,
            self.txid,
            oid,
            super::ast::TextSearchObjectKind::Configuration,
        )
    }

    fn catalog_object(
        &self,
        class_id: i32,
        object_id: i32,
        sub_id: i32,
    ) -> Result<super::event_trigger::EventObject, SqlError> {
        super::event_trigger::catalog_object(self.storage, self.txid, class_id, object_id, sub_id)
    }

    fn catalog_object_address(
        &self,
        object_type: &str,
        names: &[&str],
        arguments: &[&str],
    ) -> Result<(i32, i32, i32), SqlError> {
        super::event_trigger::catalog_object_address(
            self.storage,
            self.txid,
            object_type,
            names,
            arguments,
        )
    }

    fn serial_sequence_name<'a>(
        &self,
        table: &str,
        column: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::serial_sequence_name(self.storage, self.txid, table, column, arena)
    }

    fn function_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::function_def_text(self.storage, self.txid, oid, arena)
    }

    fn trigger_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::trigger_def_text(self.storage, self.txid, oid, arena)
    }

    fn function_arguments<'a>(
        &self,
        oid: i32,
        identity: bool,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::function_arguments_text(self.storage, self.txid, oid, identity, arena)
    }

    fn function_result<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::function_result_text(self.storage, self.txid, oid, arena)
    }

    fn collation_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::collation_oid_visibility(self.storage, self.txid, oid)
    }

    fn relation_is_publishable(&self, oid: i32) -> Option<bool> {
        super::catalog::relation_oid_is_visible(self.storage, self.txid, oid).then_some(
            super::catalog::relation_oid_is_publishable(self.storage, self.txid, oid),
        )
    }

    fn current_temporary_namespace_oid(&self) -> i32 {
        super::catalog::current_temporary_namespace_oid(self.storage, self.txid)
    }

    fn is_other_temporary_namespace(&self, oid: i32) -> bool {
        super::catalog::is_other_temporary_namespace(self.storage, self.txid, oid)
    }

    fn index_def<'a>(
        &self,
        oid: i32,
        col: i32,
        pretty: bool,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::index_def_text(self.storage, self.txid, oid, col, pretty, arena)
    }
    fn constraint_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::constraint_def_text(self.storage, self.txid, oid, arena)
    }
    fn expression_def<'a>(
        &self,
        node: &str,
        relation: i32,
        pretty: bool,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::expression_def_text(self.storage, self.txid, node, relation, pretty, arena)
    }
    fn partition_key_def<'a>(
        &self,
        oid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::partition_key_def_text(self.storage, self.txid, oid, arena)
    }
    fn relname<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::relname_text(self.storage, self.txid, oid, arena)
    }

    fn reloid(&self, name: &str) -> Result<Option<i32>, SqlError> {
        super::catalog::reloid_of_name(self.storage, self.txid, name)
    }

    fn role_name<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        if let Some(slot) = self.storage.role_slot_by_oid(oid, self.txid) {
            let name = self.storage.role_name(slot, self.txid);
            Ok(Some(
                arena.alloc_str(name.as_str()).map_err(|_| arena_full())?,
            ))
        } else if let Some(name) = super::catalog::predefined_role_name(oid) {
            Ok(Some(arena.alloc_str(name).map_err(|_| arena_full())?))
        } else {
            Ok(None)
        }
    }

    fn role_oid(&self, name: &str) -> Option<i32> {
        self.storage
            .find_role_visible(name, self.txid)
            .map(|slot| self.storage.role_oid(slot))
            .or_else(|| super::catalog::predefined_role_oid(name))
    }

    fn written_role_oid(&self, name: &str) -> Option<i32> {
        super::catalog::role_oid_by_written(self.storage, self.txid, name)
    }

    fn object_acl<'a>(
        &self,
        classid: u32,
        objid: u32,
        objsubid: i32,
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        super::catalog::object_acl_by_address(
            self.storage,
            self.txid,
            classid,
            objid,
            objsubid,
            arena,
        )
    }

    fn schema_name<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        let Some(name) = super::catalog::schema_name_by_oid(self.storage, self.txid, oid) else {
            return Ok(None);
        };
        Ok(Some(
            arena.alloc_str(name.as_str()).map_err(|_| arena_full())?,
        ))
    }

    fn schema_oid(&self, name: &str) -> Option<i32> {
        super::catalog::schema_oid_by_name(self.storage, self.txid, name)
    }

    fn written_schema_oid(&self, name: &str) -> Option<i32> {
        super::catalog::schema_oid_by_written(self.storage, self.txid, name)
    }

    fn routine_name<'a>(
        &self,
        oid: i32,
        signature: bool,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::routine_name_by_oid(self.storage, self.txid, oid, signature, arena)
    }

    fn routine_oid(&self, name: &str, signature: bool) -> Result<Option<i32>, SqlError> {
        super::catalog::routine_oid_by_name(self.storage, self.txid, name, signature)
    }

    fn operator_name<'a>(
        &self,
        oid: i32,
        signature: bool,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::operator_name_by_oid(self.storage, self.txid, oid, signature, arena)
    }

    fn operator_oid(&self, name: &str, signature: bool) -> Result<Option<i32>, SqlError> {
        super::catalog::operator_oid_by_name(self.storage, self.txid, name, signature)
    }

    fn has_table_privilege(
        &self,
        role: Option<&str>,
        relation: PrivilegeRelation<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let object = match privilege_relation(self.storage, self.txid, relation)? {
            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                self.storage.table_access_object(slot, self.txid)
            }
            Some(crate::storage::ResolvedRelation::View(slot)) => crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: slot as u16,
            },
            Some(crate::storage::ResolvedRelation::Catalog) => {
                let name = match relation {
                    PrivilegeRelation::Name(name) => {
                        super::identifiers::QualifiedObjectName::parse(name)?.name
                    }
                    PrivilegeRelation::Oid(oid) => crate::storage::SqlName::parse(
                        super::catalog::catalog_relation_name(oid)
                            .expect("catalog identity was resolved"),
                    )?,
                };
                // PostgreSQL 18.6 system_views.sql revokes PUBLIC access to
                // these views; initdb also protects pg_authid/pg_largeobject.
                let public_select = !matches!(
                    name.as_str(),
                    "pg_authid"
                        | "pg_largeobject"
                        | "pg_shadow"
                        | "pg_statistic"
                        | "pg_statistic_ext_data"
                        | "pg_file_settings"
                        | "pg_hba_file_rules"
                        | "pg_ident_file_mappings"
                        | "pg_config"
                        | "pg_shmem_allocations"
                        | "pg_shmem_allocations_numa"
                        | "pg_backend_memory_contexts"
                        | "pg_user_mapping"
                        | "pg_replication_origin_status"
                        | "pg_subscription"
                        | "pg_aios"
                );
                let owned = self
                    .storage
                    .role_slot_by_oid(10, self.txid)
                    .is_some_and(|owner| {
                        self.storage.role_has_privileges_of(role, owner, self.txid)
                    });
                return privilege_query(
                    privileges,
                    if public_select {
                        crate::storage::PrivilegeSet::SELECT
                    } else {
                        crate::storage::PrivilegeSet::NONE
                    },
                    crate::storage::PrivilegeSet::TABLE_ALL,
                    |_| owned,
                    |_| owned,
                )
                .map(Some);
            }
            None => return Ok(None),
        };
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            crate::storage::PrivilegeSet::TABLE_ALL,
            |privilege| {
                self.storage
                    .has_object_privilege(object, role, privilege, self.txid)
            },
            |privilege| {
                self.storage
                    .has_object_grant_option(object, role, privilege, self.txid)
            },
        )
        .map(Some)
    }

    fn has_column_privilege(
        &self,
        role: Option<&str>,
        relation: PrivilegeRelation<'_>,
        column: super::eval::PrivilegeColumn<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let (object, column) = match privilege_relation(self.storage, self.txid, relation)? {
            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                let definition = self.storage.table_def(slot, self.txid);
                let column = match column {
                    super::eval::PrivilegeColumn::Name(name) => definition.column_index(name),
                    super::eval::PrivilegeColumn::Number(number) if number > 0 => {
                        definition.attribute_column(number as u16)
                    }
                    super::eval::PrivilegeColumn::Number(_) => None,
                };
                (self.storage.table_access_object(slot, self.txid), column)
            }
            Some(crate::storage::ResolvedRelation::View(slot)) => {
                let object = crate::storage::AccessObject {
                    class: crate::storage::AccessClass::View,
                    slot: slot as u16,
                };
                let view = self.storage.view(slot);
                let mut descriptions = [ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
                let count = super::catalog::describe_view(
                    self.storage,
                    self.txid,
                    &view,
                    self.routine_workspace,
                    &mut descriptions,
                )?;
                let column = match column {
                    super::eval::PrivilegeColumn::Name(name) => descriptions[..count]
                        .iter()
                        .position(|description| description.name.eq_ignore_ascii_case(name)),
                    super::eval::PrivilegeColumn::Number(number) if number > 0 => {
                        let index = number as usize - 1;
                        (index < count).then_some(index)
                    }
                    super::eval::PrivilegeColumn::Number(_) => None,
                };
                (object, column)
            }
            Some(crate::storage::ResolvedRelation::Catalog) | None => return Ok(None),
        };
        let column =
            column.ok_or_else(|| sql_err!(sqlstate::UNDEFINED_COLUMN, "column does not exist"))?;
        let target = crate::storage::ColumnPrivilegeTarget::new(object, column as u16)?;
        let all = crate::storage::PrivilegeSet::SELECT
            .union(crate::storage::PrivilegeSet::INSERT)
            .union(crate::storage::PrivilegeSet::UPDATE)
            .union(crate::storage::PrivilegeSet::REFERENCES);
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            all,
            |privilege| {
                self.storage
                    .has_column_privilege(target, role, privilege, self.txid)
            },
            |privilege| {
                self.storage
                    .has_column_grant_option(target, role, privilege, self.txid)
            },
        )
        .map(Some)
    }

    fn has_any_column_privilege(
        &self,
        role: Option<&str>,
        relation: PrivilegeRelation<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let (object, columns) = match privilege_relation(self.storage, self.txid, relation)? {
            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                let definition = self.storage.table_def(slot, self.txid);
                (
                    self.storage.table_access_object(slot, self.txid),
                    definition.n_columns,
                )
            }
            Some(crate::storage::ResolvedRelation::View(slot)) => {
                let view = self.storage.view(slot);
                let mut descriptions = [ColDesc::new("", 0, 0); super::exec::MAX_PROJ];
                let count = super::catalog::describe_view(
                    self.storage,
                    self.txid,
                    &view,
                    self.routine_workspace,
                    &mut descriptions,
                )?;
                (
                    crate::storage::AccessObject {
                        class: crate::storage::AccessClass::View,
                        slot: slot as u16,
                    },
                    count,
                )
            }
            Some(crate::storage::ResolvedRelation::Catalog) | None => return Ok(None),
        };
        let all = crate::storage::PrivilegeSet::SELECT
            .union(crate::storage::PrivilegeSet::INSERT)
            .union(crate::storage::PrivilegeSet::UPDATE)
            .union(crate::storage::PrivilegeSet::REFERENCES);
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            all,
            |privilege| {
                self.storage
                    .has_object_privilege(object, role, privilege, self.txid)
                    || (0..columns).any(|column| {
                        crate::storage::ColumnPrivilegeTarget::new(object, column as u16).is_ok_and(
                            |target| {
                                self.storage
                                    .has_column_privilege(target, role, privilege, self.txid)
                            },
                        )
                    })
            },
            |privilege| {
                self.storage
                    .has_object_grant_option(object, role, privilege, self.txid)
                    || (0..columns).any(|column| {
                        crate::storage::ColumnPrivilegeTarget::new(object, column as u16).is_ok_and(
                            |target| {
                                self.storage
                                    .has_column_grant_option(target, role, privilege, self.txid)
                            },
                        )
                    })
            },
        )
        .map(Some)
    }

    fn has_sequence_privilege(
        &self,
        role: Option<&str>,
        sequence: PrivilegeRelation<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let oid = privilege_relation_oid(self.storage, self.txid, sequence)?;
        let slot = super::catalog::sequence_slot_by_oid(self.storage, self.txid, oid);
        let Some(slot) = slot else {
            if super::catalog::relation_oid_visibility(self.storage, self.txid, oid).is_some() {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "object {} is not a sequence",
                    oid
                ));
            }
            return Ok(None);
        };
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Sequence,
            slot: slot as u16,
        };
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            crate::storage::PrivilegeSet::SEQUENCE_ALL,
            |privilege| {
                self.storage
                    .has_object_privilege(object, role, privilege, self.txid)
            },
            |privilege| {
                self.storage
                    .has_object_grant_option(object, role, privilege, self.txid)
            },
        )
        .map(Some)
    }

    fn has_schema_privilege(
        &self,
        role: Option<&str>,
        schema: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        use super::eval::PrivilegeObject;
        use crate::storage::PrivilegeSet;
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let oid = match schema {
            PrivilegeObject::Oid(oid) => oid,
            PrivilegeObject::Name(name) => {
                super::catalog::schema_oid_by_written(self.storage, self.txid, name).ok_or_else(
                    || {
                        sql_err!(
                            sqlstate::INVALID_SCHEMA_NAME,
                            "schema \"{}\" does not exist",
                            name
                        )
                    },
                )?
            }
        };
        let requested = PrivilegeRequirements::parse(privileges, PrivilegeSet::SCHEMA_ALL)?;
        if self.storage.role(role).attributes_to(self.txid).superuser {
            return Ok(Some(true));
        }
        let Some(schema) = super::catalog::schema_name_by_oid(self.storage, self.txid, oid) else {
            return Ok(None);
        };
        if matches!(
            schema.as_str(),
            "pg_catalog" | "information_schema" | "pg_toast"
        ) {
            let owner = self.storage.role_has_privileges_of(role, 0, self.txid);
            let public = schema.as_str() != "pg_toast";
            return Ok(Some(requested.allows(
                PrivilegeSet::NONE,
                |privilege| owner || (public && privilege == PrivilegeSet::USAGE),
                |_| owner,
            )));
        }
        let Some(slot) = self.storage.find_schema_visible(schema.as_str(), self.txid) else {
            return Ok(None);
        };
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Schema,
            slot: slot as u16,
        };
        Ok(Some(requested.allows(
            crate::storage::PrivilegeSet::NONE,
            |privilege| {
                self.storage
                    .has_object_privilege(object, role, privilege, self.txid)
            },
            |privilege| {
                self.storage
                    .has_object_grant_option(object, role, privilege, self.txid)
            },
        )))
    }

    fn has_type_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        use super::types::oid;
        use crate::storage::{AccessClass, AccessObject, PrivilegeSet};
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let type_oid = match target {
            super::eval::PrivilegeObject::Oid(oid) => oid,
            super::eval::PrivilegeObject::Name(name) => {
                let resolved = super::eval::regtype_cast(
                    Datum::Text(name),
                    Some(self),
                    self.routine_workspace,
                )?;
                let Datum::Regtype { referenced_oid, .. } = resolved else {
                    unreachable!("type name resolved to regtype")
                };
                referenced_oid
            }
        };
        let requested = PrivilegeRequirements::parse(privileges, PrivilegeSet::TYPE_ALL)?;
        if self.storage.role(role).attributes_to(self.txid).superuser {
            return Ok(Some(true));
        }
        let named = crate::storage::NamedTypeKind::from_oid(type_oid).and_then(|(kind, _, _)| {
            let result = self.storage.routine_result_for_oid(type_oid, self.txid)?;
            let reference =
                crate::storage::TypeReference::from_bound_name(result.user_type?).ok()?;
            let class = match kind {
                crate::storage::NamedTypeKind::Domain => AccessClass::Domain,
                crate::storage::NamedTypeKind::Enum => AccessClass::Enum,
                crate::storage::NamedTypeKind::Composite => AccessClass::Composite,
            };
            Some(AccessObject {
                class,
                slot: reference.catalog_slot(),
            })
        });
        if let Some(object) = named {
            let slot = usize::from(object.slot);
            let visible = match object.class {
                AccessClass::Domain => self.storage.domain_slot_visible_to(slot, self.txid),
                AccessClass::Enum => self.storage.enum_slot_visible_to(slot, self.txid),
                AccessClass::Composite => self.storage.composite_slot_visible_to(slot, self.txid),
                _ => unreachable!("named type object was classified"),
            };
            if !visible {
                return Ok(None);
            }
            return Ok(Some(requested.allows(
                PrivilegeSet::NONE,
                |privilege| {
                    self.storage
                        .has_object_privilege(object, role, privilege, self.txid)
                },
                |privilege| {
                    self.storage
                        .has_object_grant_option(object, role, privilege, self.txid)
                },
            )));
        }
        let owner = {
            let row_type = [false, true].into_iter().find_map(|array| {
                super::catalog::table_slot_by_row_type_oid(self.storage, self.txid, type_oid, array)
                    .map(|slot| AccessObject {
                        class: AccessClass::Table,
                        slot: slot as u16,
                    })
                    .or_else(|| {
                        super::catalog::view_slot_by_row_type_oid(
                            self.storage,
                            self.txid,
                            type_oid,
                            array,
                        )
                        .map(|slot| AccessObject {
                            class: AccessClass::View,
                            slot: slot as u16,
                        })
                    })
            });
            if let Some(object) = row_type {
                let slot = usize::from(object.slot);
                let visible = match object.class {
                    AccessClass::Table => self.storage.table_slot_visible_to(slot, self.txid),
                    AccessClass::View => self.storage.view_slot_visible_to(slot, self.txid),
                    _ => unreachable!("row type was classified"),
                };
                if !visible {
                    return Ok(None);
                }
                self.storage.object_owner(object, self.txid)
            } else if type_oid < oid::FIRST_DOMAIN
                && super::catalog::intrinsic_type_name(type_oid).is_some()
            {
                self.storage
                    .role_slot_by_oid(10, self.txid)
                    .expect("bootstrap type owner exists")
            } else {
                return Ok(None);
            }
        };
        let owned = self.storage.role_has_privileges_of(role, owner, self.txid);
        Ok(Some(requested.allows(
            PrivilegeSet::USAGE,
            |_| owned,
            |_| owned,
        )))
    }

    fn has_language_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        object_privilege_query(
            self.storage,
            self.txid,
            role,
            crate::storage::AccessClass::Language,
            target,
            crate::storage::PrivilegeSet::USAGE,
            privileges,
        )
    }

    fn has_function_privilege(
        &self,
        role: Option<&str>,
        function: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        use super::eval::PrivilegeObject;
        use crate::storage::PrivilegeSet;
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let oid = match function {
            PrivilegeObject::Oid(oid) => oid,
            PrivilegeObject::Name(name) => {
                super::catalog::routine_oid_by_name(self.storage, self.txid, name, true)?
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_FUNCTION,
                            "function \"{}\" does not exist",
                            name
                        )
                    })?
            }
        };
        let requested = PrivilegeRequirements::parse(privileges, PrivilegeSet::FUNCTION_ALL)?;
        if self.storage.role(role).attributes_to(self.txid).superuser {
            return Ok(Some(true));
        }
        if let Some(public) = super::catalog::intrinsic_routine_public_execute(oid) {
            let owner = self.storage.role_has_privileges_of(role, 0, self.txid);
            return Ok(Some(requested.allows(
                PrivilegeSet::NONE,
                |_| owner || public,
                |_| owner,
            )));
        }
        let Some(slot) = self.storage.routine_slot_by_oid(oid, self.txid) else {
            return Ok(None);
        };
        let object = crate::storage::Storage::routine_access_object(slot);
        Ok(Some(requested.allows(
            crate::storage::PrivilegeSet::NONE,
            |privilege| {
                self.storage
                    .has_object_privilege(object, role, privilege, self.txid)
            },
            |privilege| {
                self.storage
                    .has_object_grant_option(object, role, privilege, self.txid)
            },
        )))
    }

    fn has_database_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        object_privilege_query(
            self.storage,
            self.txid,
            role,
            crate::storage::AccessClass::Database,
            target,
            crate::storage::PrivilegeSet::DATABASE_ALL,
            privileges,
        )
    }

    fn has_tablespace_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        object_privilege_query(
            self.storage,
            self.txid,
            role,
            crate::storage::AccessClass::Tablespace,
            target,
            crate::storage::PrivilegeSet::CREATE,
            privileges,
        )
    }

    fn has_foreign_data_wrapper_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        object_privilege_query(
            self.storage,
            self.txid,
            role,
            crate::storage::AccessClass::ForeignDataWrapper,
            target,
            crate::storage::PrivilegeSet::USAGE,
            privileges,
        )
    }

    fn has_server_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        object_privilege_query(
            self.storage,
            self.txid,
            role,
            crate::storage::AccessClass::ForeignServer,
            target,
            crate::storage::PrivilegeSet::USAGE,
            privileges,
        )
    }

    fn has_largeobject_privilege(
        &self,
        role: Option<&str>,
        target: super::eval::PrivilegeObject<'_>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        object_privilege_query(
            self.storage,
            self.txid,
            role,
            crate::storage::AccessClass::LargeObject,
            target,
            crate::storage::PrivilegeSet::LARGE_OBJECT_ALL,
            privileges,
        )
    }

    fn has_parameter_privilege(
        &self,
        role: Option<&str>,
        parameter: &str,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let parameter = match crate::sql::ast::ParameterName::parse(parameter) {
            Some(parameter) if crate::sql::guc::knows_parameter(parameter.as_str()) => parameter,
            _ => return Ok(None),
        };
        parameter_privilege_query(
            privileges,
            |privilege| {
                self.storage
                    .has_parameter_privilege(parameter, role, privilege, self.txid)
            },
            |privilege| {
                self.storage
                    .has_parameter_grant_option(parameter, role, privilege, self.txid)
            },
        )
        .map(Some)
    }

    fn comment<'a>(
        &self,
        target: crate::sql::eval::DescriptionTarget<'_>,
        oid: u32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::comment_text_for(self.storage, self.txid, target, oid, arena)
    }

    fn type_name<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::user_type_name_text(self.storage, self.txid, oid, arena)
    }

    fn view_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::view_def_text(self.storage, self.txid, oid, arena)
    }

    fn rule_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::rule_def_text(self.storage, self.txid, oid, arena)
    }

    fn relation_size(&self, oid: i32) -> Result<Option<i64>, SqlError> {
        super::catalog::relation_size(self.storage, self.txid, oid)
    }

    fn database_size(&self) -> Result<i64, SqlError> {
        super::catalog::database_size(self.storage, self.txid)
    }

    fn enum_name<'a>(
        &self,
        slot: crate::storage::TypeCreation,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        let Some(def) = self.storage.read_enum_definition(
            crate::storage::TypeDefinitionTarget::by_creation(
                self.storage,
                crate::storage::NamedTypeKind::Enum,
                slot,
            )?,
            self.txid,
        )?
        else {
            return Ok(None);
        };
        if !def.visible_to(self.txid) {
            return Ok(None);
        }
        Ok(Some(
            arena
                .alloc_str(def.name.as_str())
                .map_err(|_| arena_full())?,
        ))
    }

    fn enum_label_sort(
        &self,
        slot: crate::storage::TypeCreation,
        label: &str,
    ) -> Result<Option<f64>, SqlError> {
        let Some(def) = self.storage.read_enum_definition(
            crate::storage::TypeDefinitionTarget::by_creation(
                self.storage,
                crate::storage::NamedTypeKind::Enum,
                slot,
            )?,
            self.txid,
        )?
        else {
            return Ok(None);
        };
        if !def.visible_to(self.txid) {
            return Ok(None);
        }
        super::exec::ensure_enum_value_is_safe(self.storage, slot, label, self.txid)?;
        Ok(def.sort_of(label))
    }

    fn ensure_enum_label_safe(
        &self,
        slot: crate::storage::TypeCreation,
        label: &str,
    ) -> Result<(), SqlError> {
        super::exec::ensure_enum_value_is_safe(self.storage, slot, label, self.txid)
    }

    fn enum_values<'a>(
        &self,
        slot: crate::storage::TypeCreation,
        arena: &'a Arena,
    ) -> Result<Option<&'a [Datum<'a>]>, SqlError> {
        let Some(def) = self.storage.read_enum_definition(
            crate::storage::TypeDefinitionTarget::by_creation(
                self.storage,
                crate::storage::NamedTypeKind::Enum,
                slot,
            )?,
            self.txid,
        )?
        else {
            return Ok(None);
        };
        if !def.visible_to(self.txid) {
            return Ok(None);
        }
        let values = arena
            .alloc_slice_with(def.members().len(), |_| Datum::Null)
            .map_err(|_| arena_full())?;
        for (index, (value, member)) in values.iter_mut().zip(def.members()).enumerate() {
            *value = Datum::Enum {
                slot,
                member: Some(
                    crate::sql::types::EnumMemberId::from_ordinal(index as u32)
                        .expect("validated enum member ordinal"),
                ),
                sort: member.sort,
                label: arena
                    .alloc_str(member.label.as_str())
                    .map_err(|_| arena_full())?,
            };
        }
        Ok(Some(values))
    }

    fn enum_creation_of_name(&self, type_name: &str) -> Option<crate::storage::TypeCreation> {
        self.storage
            .resolve_enum_slot(type_name, self.txid)
            .map(|slot| {
                crate::storage::TypeCreation::parse(
                    self.storage.enum_for(slot, self.txid).created_at,
                )
                .expect("admitted enum birth")
            })
    }

    fn cast_user_type<'a>(
        &self,
        type_name: &str,
        value: Datum<'a>,
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        if let Some(element_name) = type_name.strip_suffix("[]") {
            if let Some(slot) = self.storage.resolve_enum_slot(element_name, self.txid) {
                return super::exec::coerce_user_type_array(
                    value,
                    super::types::ArrElem::Enum(Some(
                        self.storage.enum_for(slot, self.txid).creation(),
                    )),
                    self.storage,
                    self.txid,
                    arena,
                )
                .map(Some);
            }
            if let Some(slot) = self.storage.resolve_domain_slot(element_name, self.txid) {
                let domain = self.storage.domain(slot);
                let Some(element) = super::types::ArrElem::domain(domain.creation(), domain.base)
                else {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "arrays of domain {} require a scalar base type",
                        element_name
                    ));
                };
                return super::exec::coerce_user_type_array(
                    value,
                    element,
                    self.storage,
                    self.txid,
                    arena,
                )
                .map(Some);
            }
            if let Some(slot) = self.storage.resolve_composite_slot(element_name, self.txid) {
                return super::exec::coerce_user_type_array(
                    value,
                    super::types::ArrElem::Composite(Some(
                        self.storage.composite_for(slot, self.txid).creation(),
                    )),
                    self.storage,
                    self.txid,
                    arena,
                )
                .map(Some);
            }
            return Ok(None);
        }
        if let Some(slot) = self.storage.resolve_domain_slot(type_name, self.txid) {
            return super::exec::coerce_domain_value(
                self.storage,
                crate::storage::TypeDefinitionTarget::Slot(crate::storage::NamedTypeSlot::Domain(
                    slot,
                )),
                value,
                self.txid,
                arena,
                super::eval::NO_PARAMS,
            )
            .map(Some);
        }
        if let Some(slot) = self.storage.resolve_composite_slot(type_name, self.txid) {
            return match value {
                Datum::Text(text) => super::exec::decode_composite_text(
                    text,
                    self.storage.composite_for(slot, self.txid).creation(),
                    self.storage,
                    self.txid,
                    arena,
                ),
                value => super::exec::coerce_composite_value(
                    value,
                    Some(self.storage.composite_for(slot, self.txid).creation()),
                    self.storage,
                    self.txid,
                    arena,
                ),
            }
            .map(Some);
        }
        let Some(slot) = self.storage.resolve_enum_slot(type_name, self.txid) else {
            // A table/view rowtype name: NULL casts populate nothing; other
            // values report PostgreSQL's cannot-coerce error until rowtype
            // value decoding exists.
            let (qualifier, bare) = type_name
                .split_once('.')
                .map_or((None, type_name), |(schema, name)| (Some(schema), name));
            if self
                .storage
                .resolve_relation(qualifier, bare, self.txid)
                .is_some()
            {
                if value.is_null() {
                    return Ok(Some(Datum::Null));
                }
                return Err(sql_err!(
                    sqlstate::CANNOT_COERCE,
                    "cannot cast type record to {}",
                    type_name
                ));
            }
            return Ok(None);
        };
        super::exec::coerce_enum_value(
            value,
            Some(self.storage.enum_for(slot, self.txid).creation()),
            self.storage,
            self.txid,
            arena,
        )
        .map(Some)
    }

    fn user_type_name<'a>(
        &self,
        type_name: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        self.user_type_oid(type_name).map_or(Ok(None), |oid| {
            super::catalog::user_type_name_text(self.storage, self.txid, oid, arena)
        })
    }

    fn user_type_oid(&self, type_name: &str) -> Option<i32> {
        super::catalog::user_type_oid(self.storage, self.txid, type_name)
    }

    fn declared_user_type_oid(
        &self,
        ctype: ColType,
        identity: crate::storage::UserTypeName,
    ) -> Option<i32> {
        self.storage
            .routine_type_oid(ctype, Some(identity), self.txid)
    }

    fn domain_base_type(&self, type_oid: i32) -> Option<ColType> {
        self.storage.domain_base_type_for_oid(type_oid, self.txid)
    }

    fn user_type_identity_oid(
        &self,
        identity: crate::storage::UserTypeName,
        array: bool,
    ) -> Option<i32> {
        self.storage
            .user_type_identity_oid(identity, array, self.txid)
    }
}

fn privilege_relation_oid(
    storage: &Storage,
    transaction: u32,
    relation: PrivilegeRelation<'_>,
) -> Result<i32, SqlError> {
    let written = match relation {
        PrivilegeRelation::Name(written) => written,
        PrivilegeRelation::Oid(oid) => return Ok(oid),
    };
    let parsed = super::identifiers::QualifiedObjectName::parse(written)?;
    if parsed
        .database
        .is_some_and(|database| database != storage.current_database_name(transaction))
    {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cross-database relation references are not supported"
        ));
    }
    if let Some(schema) = parsed.schema.as_ref() {
        let schema_exists = if schema.as_str() == "pg_temp" {
            super::catalog::current_temporary_namespace_oid(storage, transaction) != 0
        } else {
            super::catalog::schema_oid_by_name(storage, transaction, schema.as_str()).is_some()
        };
        if !schema_exists {
            return Err(sql_err!(
                sqlstate::INVALID_SCHEMA_NAME,
                r#"schema "{}" does not exist"#,
                schema.as_str()
            ));
        }
        if storage
            .find_schema_visible(schema.as_str(), transaction)
            .is_some()
        {
            storage.require_schema_usage(schema.as_str(), transaction)?;
        }
    }
    super::catalog::relation_oid_for_name(
        storage,
        transaction,
        parsed.schema.as_ref().map(crate::storage::SqlName::as_str),
        parsed.name.as_str(),
    )
    .ok_or_else(|| {
        sql_err!(
            sqlstate::UNDEFINED_TABLE,
            r#"relation "{}" does not exist"#,
            written
        )
    })
}

pub(super) fn privilege_relation(
    storage: &Storage,
    transaction: u32,
    relation: PrivilegeRelation<'_>,
) -> Result<Option<crate::storage::ResolvedRelation>, SqlError> {
    let oid = privilege_relation_oid(storage, transaction, relation)?;
    if super::catalog::catalog_relation_oid_by_oid(oid) {
        return Ok(Some(crate::storage::ResolvedRelation::Catalog));
    }
    if let Some(slot) = super::catalog::table_slot_by_oid(storage, transaction, oid) {
        return Ok(Some(crate::storage::ResolvedRelation::Table(slot)));
    }
    let resolved = super::catalog::view_slot_by_oid(storage, transaction, oid)
        .map(crate::storage::ResolvedRelation::View);
    if resolved.is_none() && matches!(relation, PrivilegeRelation::Name(_)) {
        return Err(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "privilege lookup for this relation kind is not implemented"
        ));
    }
    Ok(resolved)
}

fn privilege_role(storage: &Storage, role: Option<&str>, txid: u32) -> Option<usize> {
    match role {
        Some(role) => storage.find_role_visible(role, txid),
        None => storage.current_role_slot(txid),
    }
}

/// A complete privilege request, checked before a missing-object result.
struct PrivilegeRequirements {
    ordinary: crate::storage::PrivilegeSet,
    grant_option: crate::storage::PrivilegeSet,
}

impl PrivilegeRequirements {
    fn parse(written: &str, all: crate::storage::PrivilegeSet) -> Result<Self, SqlError> {
        let mut requested = Self {
            ordinary: crate::storage::PrivilegeSet::NONE,
            grant_option: crate::storage::PrivilegeSet::NONE,
        };
        for item in written.split(',') {
            let item = item.trim();
            const GRANT_OPTION: &str = " WITH GRANT OPTION";
            let (name, grant_option) = if item
                .get(item.len().saturating_sub(GRANT_OPTION.len())..)
                .is_some_and(|suffix| suffix.eq_ignore_ascii_case(GRANT_OPTION))
            {
                (item[..item.len() - GRANT_OPTION.len()].trim_end(), true)
            } else {
                (item, false)
            };
            let privilege = if name.eq_ignore_ascii_case("select") {
                crate::storage::PrivilegeSet::SELECT
            } else if name.eq_ignore_ascii_case("insert") {
                crate::storage::PrivilegeSet::INSERT
            } else if name.eq_ignore_ascii_case("update") {
                crate::storage::PrivilegeSet::UPDATE
            } else if name.eq_ignore_ascii_case("delete") {
                crate::storage::PrivilegeSet::DELETE
            } else if name.eq_ignore_ascii_case("truncate") {
                crate::storage::PrivilegeSet::TRUNCATE
            } else if name.eq_ignore_ascii_case("references") {
                crate::storage::PrivilegeSet::REFERENCES
            } else if name.eq_ignore_ascii_case("trigger") {
                crate::storage::PrivilegeSet::TRIGGER
            } else if name.eq_ignore_ascii_case("usage") {
                crate::storage::PrivilegeSet::USAGE
            } else if name.eq_ignore_ascii_case("create") {
                crate::storage::PrivilegeSet::CREATE
            } else if name.eq_ignore_ascii_case("execute") {
                crate::storage::PrivilegeSet::EXECUTE
            } else if name.eq_ignore_ascii_case("maintain") {
                crate::storage::PrivilegeSet::MAINTAIN
            } else if name.eq_ignore_ascii_case("connect") {
                crate::storage::PrivilegeSet::CONNECT
            } else if name.eq_ignore_ascii_case("temporary") || name.eq_ignore_ascii_case("temp") {
                crate::storage::PrivilegeSet::TEMPORARY
            } else {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized privilege type: \"{}\"",
                    name
                ));
            };
            if !all.contains(privilege) {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized privilege type: \"{}\"",
                    name
                ));
            }
            if grant_option {
                requested.grant_option = requested.grant_option.union(privilege);
            } else {
                requested.ordinary = requested.ordinary.union(privilege);
            }
        }
        Ok(requested)
    }

    fn allows(
        &self,
        catalog_default: crate::storage::PrivilegeSet,
        has_privilege: impl Fn(crate::storage::PrivilegeSet) -> bool,
        has_grant_option: impl Fn(crate::storage::PrivilegeSet) -> bool,
    ) -> bool {
        for bit in 0..u16::BITS {
            let privilege = crate::storage::PrivilegeSet(1 << bit);
            if self.ordinary.contains(privilege)
                && (catalog_default.contains(privilege) || has_privilege(privilege))
                || self.grant_option.contains(privilege) && has_grant_option(privilege)
            {
                return true;
            }
        }
        false
    }
}

fn privilege_query(
    written: &str,
    catalog_default: crate::storage::PrivilegeSet,
    all: crate::storage::PrivilegeSet,
    has_privilege: impl Fn(crate::storage::PrivilegeSet) -> bool,
    has_grant_option: impl Fn(crate::storage::PrivilegeSet) -> bool,
) -> Result<bool, SqlError> {
    Ok(PrivilegeRequirements::parse(written, all)?.allows(
        catalog_default,
        has_privilege,
        has_grant_option,
    ))
}

fn object_privilege_query(
    storage: &Storage,
    txid: u32,
    role: Option<&str>,
    class: crate::storage::AccessClass,
    target: super::eval::PrivilegeObject<'_>,
    all: crate::storage::PrivilegeSet,
    privileges: &str,
) -> Result<Option<bool>, SqlError> {
    use super::eval::PrivilegeObject;
    use crate::storage::{AccessClass, AccessObject, LargeObjectOid};
    enum Lookup {
        Named(AccessObject),
        Numeric(i32),
    }
    let Some(role) = privilege_role(storage, role, txid) else {
        return Ok(None);
    };
    let target = match target {
        PrivilegeObject::Name(value) if class == AccessClass::LargeObject => {
            let oid = super::eval::parse_oid(value)?;
            Lookup::Numeric(oid as i32)
        }
        PrivilegeObject::Name(name) => {
            let object = storage
                .resolve_access_object(class, "", name, txid)
                .ok_or_else(|| {
                    let label = match class {
                        AccessClass::Database => "database",
                        AccessClass::Tablespace => "tablespace",
                        AccessClass::Language => "language",
                        AccessClass::ForeignDataWrapper => "foreign-data wrapper",
                        AccessClass::ForeignServer => "server",
                        _ => unreachable!("object privilege class was classified"),
                    };
                    let state = if class == AccessClass::Database {
                        sqlstate::INVALID_CATALOG_NAME
                    } else {
                        sqlstate::UNDEFINED_OBJECT
                    };
                    sql_err!(state, r#"{} "{}" does not exist"#, label, name)
                })?;
            Lookup::Named(object)
        }
        PrivilegeObject::Oid(oid) => Lookup::Numeric(oid),
    };
    let requested = PrivilegeRequirements::parse(privileges, all)?;
    if class != AccessClass::LargeObject && storage.role(role).attributes_to(txid).superuser {
        return Ok(Some(true));
    }
    let object = match target {
        Lookup::Named(object) => Some(object),
        Lookup::Numeric(oid) => {
            let slot = match class {
                AccessClass::Database => crate::storage::DatabaseOid::parse(oid)
                    .and_then(|oid| storage.database_slot_by_oid(oid, txid)),
                AccessClass::Tablespace => {
                    storage
                        .tablespaces_visible_to(txid)
                        .find_map(|(slot, definition)| {
                            let actual = super::catalog::tablespace_oid(slot, definition);
                            (actual == oid).then_some(slot)
                        })
                }
                AccessClass::Language => super::catalog::procedural_language_name(oid)
                    .and_then(|_| u16::try_from(oid).ok())
                    .map(usize::from),
                AccessClass::ForeignDataWrapper => {
                    storage.foreign_wrappers(txid).find_map(|(slot, _)| {
                        (storage.foreign_wrapper_entry(slot).oid() == oid).then_some(slot)
                    })
                }
                AccessClass::ForeignServer => {
                    storage.foreign_servers(txid).find_map(|(slot, _)| {
                        (storage.foreign_server_entry(slot).oid() == oid).then_some(slot)
                    })
                }
                AccessClass::LargeObject => LargeObjectOid::parse(oid as u32)
                    .and_then(|oid| storage.large_object_slot(oid, txid)),
                _ => unreachable!("object privilege class was classified"),
            };
            slot.and_then(|slot| u16::try_from(slot).ok())
                .map(|slot| AccessObject { class, slot })
        }
    };
    let Some(object) = object else {
        return Ok(None);
    };
    Ok(Some(requested.allows(
        crate::storage::PrivilegeSet::NONE,
        |privilege| storage.has_object_privilege(object, role, privilege, txid),
        |privilege| storage.has_object_grant_option(object, role, privilege, txid),
    )))
}

fn parameter_privilege_query(
    written: &str,
    has_privilege: impl Fn(crate::sql::ast::ParameterPrivileges) -> bool,
    has_grant_option: impl Fn(crate::sql::ast::ParameterPrivileges) -> bool,
) -> Result<bool, SqlError> {
    let mut answer = false;
    for item in written.split(',') {
        let item = item.trim();
        const GRANT_OPTION: &str = " WITH GRANT OPTION";
        let (name, grant_option) = if item
            .get(item.len().saturating_sub(GRANT_OPTION.len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(GRANT_OPTION))
        {
            (item[..item.len() - GRANT_OPTION.len()].trim_end(), true)
        } else {
            (item, false)
        };
        let privilege = if name.eq_ignore_ascii_case("set") {
            crate::sql::ast::ParameterPrivileges::SET
        } else if name.eq_ignore_ascii_case("alter system") {
            crate::sql::ast::ParameterPrivileges::ALTER_SYSTEM
        } else {
            return Err(sql_err!(
                sqlstate::INVALID_PARAMETER_VALUE,
                "unrecognized privilege type: \"{}\"",
                name
            ));
        };
        answer |= if grant_option {
            has_grant_option(privilege)
        } else {
            has_privilege(privilege)
        };
    }
    Ok(answer)
}

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

/// The sole query-result wire boundary. Composite values are stored in rows as
/// canonical text, so binary output materializes them before the retryable
/// responder writes the PostgreSQL send representation.
pub(crate) fn emit_data_row(
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    responder: &mut Responder,
    values: &[Datum],
) -> Outcome {
    let rows = match responder.cursor_row_output() {
        crate::pg::respond::CursorRowOutput::Wire => None,
        crate::pg::respond::CursorRowOutput::Save(rows) => Some(rows),
        crate::pg::respond::CursorRowOutput::Discard => return sql_ok(),
    };
    if values
        .iter()
        .any(|value| matches!(value, Datum::PgDdlCommand))
    {
        return sql_fail(sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot display a value of type pg_ddl_command"
        ));
    }
    if let Some(rows) = rows {
        let len = match super::exec::projected_row_len(values) {
            Ok(len) => len,
            Err(error) => return sql_fail(error),
        };
        let Some(total) = len.checked_add(4) else {
            return sql_fail(super::cursor_result_too_large());
        };
        let Ok(length) = u32::try_from(len) else {
            return sql_fail(super::cursor_result_too_large());
        };
        let out = rows.writable();
        if total > out.len() {
            return sql_fail(super::cursor_result_too_large());
        }
        // Publish the row boundary only after every typed value is encoded.
        if let Err(error) = super::exec::encode_projected_into(values, &mut out[4..total]) {
            return sql_fail(error);
        }
        out[..4].copy_from_slice(&length.to_le_bytes());
        rows.advance(total);
        return sql_ok();
    }
    let formats = responder.result_formats();
    let catalog = storage_catalog(storage, arena, txid);
    let mut text_values = [Datum::Null; MAX_PROJ];
    for (index, output) in text_values.iter_mut().enumerate().take(values.len()) {
        *output =
            match super::eval::materialize_composite_text_output(values[index], &catalog, arena) {
                Ok(value) => value,
                Err(error) => return sql_fail(error),
            };
    }
    let text_values = &text_values[..values.len()];
    let any_binary = (0..values.len()).any(|index| formats.is_binary(index));
    if !any_binary {
        return responder.data_row(text_values).map(|()| Ok(()));
    }
    let mut plans = [super::exec::BinaryFieldPlan::Direct; MAX_PROJ];
    for (index, plan) in plans.iter_mut().enumerate().take(values.len()) {
        if formats.is_binary(index) {
            if formats.is_binary(index)
                && responder.result_type_oid(index).is_some_and(|oid| {
                    super::exec::binary_output_capability(storage, oid, txid)
                        == super::exec::BinaryOutputCapability::MissingAclItemSend
                })
            {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "no binary output function available for type aclitem"
                ));
            }
            *plan = match super::exec::binary_field_plan(&values[index], storage, txid, arena) {
                Ok(plan) => plan,
                Err(error) => return sql_fail(error),
            };
            if formats.is_binary(index)
                && matches!(*plan, super::exec::BinaryFieldPlan::Unavailable)
            {
                return sql_fail(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "no binary output function available for type aclitem"
                ));
            }
        }
    }
    let render = responder.render_context();
    let emit = |message: &mut crate::pg::wire::MsgOut, selected: ResultFmt| {
        for (index, value) in values.iter().enumerate() {
            if selected.is_binary(index) {
                super::exec::encode_binary_field_plan(message, value, plans[index]);
            } else {
                Responder::encode_value_text(message, &text_values[index], render);
            }
        }
    };
    responder.data_row_prepared(values, &|message| emit(message, formats))?;
    sql_ok()
}

/// The aggregate hook data for a select's items: the aggregate-call node
/// addresses and their folded values.
type AggregateHookData<'a> = (&'a [*const Expr<'a>], &'a [Datum<'a>]);

/// FROM-less aggregation: PostgreSQL treats the missing FROM clause as a
/// single virtual row (zero rows when WHERE is false). Returns the aggregate
/// hook data for evaluating the select items, or None when the query yields
/// no output row at all (WHERE false under GROUP BY, or HAVING false).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
pub(super) fn fromless_aggregate_hooks<'a, R: ColumnLookup<'a>>(
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<Option<AggregateHookData<'a>>, SqlError> {
    let pass = match statement.where_clause {
        Some(w) => where_passes(w, arena, params, row, hooks)?,
        None => true,
    };
    if !statement.group_by.is_empty() && !pass {
        // Zero input rows grouped: zero groups, zero output rows (a plain
        // aggregate still emits its one row over the empty input).
        return Ok(None);
    }
    let states = arena
        .alloc_slice_with(agg_nodes.len(), |_| AggState::default())
        .map_err(|_| arena_full())?;
    for (i, (_, node)) in agg_nodes.iter().enumerate() {
        states[i].init(node, storage, txid, &super::exec::NoCols, arena)?;
    }
    if pass {
        for (i, (_, node)) in agg_nodes.iter().enumerate() {
            states[i].update(node, arena, params, row, hooks)?;
        }
    }
    let values = arena
        .alloc_slice_with(agg_nodes.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (i, state) in states[..agg_nodes.len()].iter_mut().enumerate() {
        values[i] = state.finish(arena, hooks.catalog)?;
    }
    let ptrs: &[*const Expr] = arena
        .alloc_slice_with(agg_nodes.len(), |i| agg_nodes[i].0)
        .map_err(|_| arena_full())?;
    if let Some(h) = statement.having {
        let agg_hooks = EvalHooks {
            aggs: Some((ptrs, values)),
            ..*hooks
        };
        let held = matches!(
            eval_full(h, arena, params, row, &agg_hooks)?,
            Datum::Bool(true)
        );
        if !held {
            return Ok(None);
        }
    }
    Ok(Some((ptrs, values)))
}

fn array_subquery_column_type(witness: Datum) -> Option<ColType> {
    match super::exec::coltype_of_oid(witness.type_oid())? {
        ColType::Array(element) => Some(ColType::Array(element)),
        scalar => super::types::ArrElem::from_coltype(scalar).map(ColType::Array),
    }
}

/// Values established before RowDescription refine otherwise unknown columns.
#[allow(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn patch_subquery_column_types<'a>(
    items: &'a [SelectItem<'a>],
    scope: Option<&QueryScope<'a>>,
    subs: &SubqueryValues,
    params: &[Datum],
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    columns: &mut [ColDesc],
) -> Result<(), SqlError> {
    let mut slot = 0usize;
    for item in items {
        match item {
            SelectItem::Wildcard => slot += scope.map_or(0, |s| s.star_columns()),
            SelectItem::TableWildcard(q) => {
                slot += scope
                    .and_then(|s| s.qualified_star_columns(q).ok())
                    .unwrap_or(0);
            }
            SelectItem::RecordStar(base) => {
                slot += scope
                    .map(|s| record_star_width(base, s))
                    .transpose()?
                    .unwrap_or(0);
            }
            SelectItem::Expr { expression, .. } => {
                if slot < columns.len()
                    && matches!(**expression, Expr::Subquery(_) | Expr::ArraySubquery(_))
                {
                    let node: *const Expr = *expression;
                    match subs
                        .scalars
                        .iter()
                        .find(|(p, _, _)| core::ptr::eq(*p, node))
                    {
                        Some((_, v, w)) => {
                            let typed = if v.is_null() { w } else { v };
                            let ct = if matches!(**expression, Expr::ArraySubquery(_)) {
                                array_subquery_column_type(*typed)
                            } else {
                                super::exec::coltype_of_oid(typed.type_oid())
                            };
                            if !typed.is_null()
                                && let Some(ct) = ct
                            {
                                columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                            }
                        }
                        // A correlated subquery has no pre-evaluated value;
                        // infer its column type from the inner select's item.
                        None => {
                            if let Expr::Subquery(sub) | Expr::ArraySubquery(sub) = &**expression
                                && let Some(SelectItem::Expr {
                                    expression: inner, ..
                                }) = sub.items.first()
                                && let Some(from) = sub.from
                                && let Ok(inner_scope) =
                                    QueryScope::resolve_schema(storage, &from, txid, arena, None)
                                && let Ok(witness) = subquery_witness(
                                    storage,
                                    txid,
                                    inner,
                                    Some(&inner_scope),
                                    arena,
                                )
                            {
                                let ct = if matches!(**expression, Expr::ArraySubquery(_)) {
                                    array_subquery_column_type(witness)
                                } else {
                                    super::exec::coltype_of_oid(witness.type_oid())
                                };
                                if !witness.is_null()
                                    && let Some(ct) = ct
                                {
                                    columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                                }
                            }
                        }
                    }
                }
                if slot < columns.len()
                    && let Expr::Param(n) = **expression
                    && let Some(v) = params.get(n as usize - 1)
                    && !v.is_null()
                    && columns[slot].type_oid == super::types::oid::TEXT
                    && let Some(ct) = super::exec::coltype_of_oid(v.type_oid())
                {
                    columns[slot] = ColDesc::of_type(columns[slot].name, ct);
                }
                slot += 1;
            }
        }
    }
    Ok(())
}

type CorrelatedScalarScratch<'a> = [(*const Expr<'a>, Datum<'a>, Datum<'a>)];
type CorrelatedListScratch<'a> = [super::eval::SubqueryList<'a>];

/// Materializes only the correlated subqueries needed for one source row.
/// Streaming and row-emitting SELECT share this arena-backed seam, keeping
/// per-row visibility and scratch bounds identical.
#[allow(clippy::too_many_arguments)]
fn correlated_row_subqueries<'a, 'scratch>(
    correlated: &[&'a Expr<'a>],
    base: &SubqueryValues<'a, 'a>,
    row: &dyn ColumnLookup<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    scalar_scratch: &'scratch mut CorrelatedScalarScratch<'a>,
    list_scratch: &'scratch mut CorrelatedListScratch<'a>,
    sequences: Option<&dyn SequenceAccess>,
) -> Result<Option<SubqueryValues<'scratch, 'a>>, SqlError> {
    if correlated.is_empty() {
        Ok(None)
    } else {
        merge_correlated(
            correlated,
            base,
            row,
            storage,
            txid,
            arena,
            params,
            scalar_scratch,
            list_scratch,
            sequences,
        )
        .map(Some)
    }
}

fn correlated_row_hooks<'scratch, 'a>(
    base: &EvalHooks<'scratch, 'a>,
    subqueries: &'scratch SubqueryValues<'scratch, 'a>,
) -> EvalHooks<'scratch, 'a> {
    EvalHooks {
        group: None,
        aggs: None,
        subs: Some(subqueries),
        subquery_executor: base.subquery_executor,
        windows: None,
        catalog: base.catalog,
        srf_index: None,
        project_sets: None,
        sequences: base.sequences,
        merge_action: base.merge_action,
    }
}

/// PostgreSQL has no equality or ordering for `json` — two documents differing
/// only in whitespace or key order are the same value but not the same text, so
/// it declines rather than answer by a rule it does not hold to, and offers
/// canonicalized `jsonb` instead. The `=` operator already declines here; these
/// three sort and deduplicate by the projected encoding and so never consult
/// it, which is why each has to be checked where its keys are known rather than
/// where they are compared. `cid` and `cid[]` have equality but no ordering.
fn check_key_types<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let noncomparable = |e: &Expr<'a>| {
        infer_scope_type(e, scope)
            .ok()
            .and_then(|(oid, _)| ColType::from_oid(oid))
            .filter(|ctype| !ctype.has_builtin_equality())
    };
    let unordered_cid = |e: &Expr<'a>| {
        infer_scope_type(e, scope)
            .ok()
            .and_then(|(oid, _)| ColType::from_oid(oid))
            .is_some_and(|ctype| ctype.has_builtin_equality() && !ctype.has_builtin_ordering())
    };
    for key in statement.group_by.iter().chain(statement.distinct_on) {
        if let Some(ctype) = noncomparable(key) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "could not identify an equality operator for type {}",
                ctype.name()
            ));
        }
    }
    for order in statement.order_by {
        let target = resolve_order_target(order.expression, statement.items, scope, arena)?;
        if let Some(ctype) = noncomparable(target) {
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "could not identify an ordering operator for type {}",
                ctype.name()
            ));
        }
        if unordered_cid(target) {
            let type_name = if matches!(
                infer_scope_type(target, scope)
                    .ok()
                    .and_then(|(oid, _)| ColType::from_oid(oid)),
                Some(ColType::Array(_))
            ) {
                "cid[]"
            } else {
                "cid"
            };
            return Err(sql_err!(
                sqlstate::UNDEFINED_FUNCTION,
                "could not identify an ordering operator for type {}",
                type_name
            ));
        }
    }
    if statement.distinct {
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item
                && let Some(ctype) = noncomparable(expression)
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_FUNCTION,
                    "could not identify an equality operator for type {}",
                    ctype.name()
                ));
            }
        }
    }
    Ok(())
}

/// `GROUP BY <n>` names the *n*th select-list column, exactly as `ORDER BY <n>`
/// does. Resolved once, against the scope so a star item expands the same way,
/// and the resolved expressions replace the ordinals — so grouping, HAVING,
/// grouping sets and the ungrouped-column check all see what the position
/// stood for rather than the literal integer. A bare integer only; `GROUP BY
/// 1+0` is a constant expression in PostgreSQL too, and errors as one.
fn resolve_group_ordinals<'a>(
    statement: &'a Select<'a>,
    scope: Option<&QueryScope<'a>>,
    arena: &'a Arena,
    storage: &Storage,
    txid: u32,
) -> Result<&'a Select<'a>, SqlError> {
    // An aggregate cannot be a grouping key, however it got there — written
    // directly, nested in an expression, or named by a position. Checked after
    // resolution so `GROUP BY 1` naming an aggregate item is caught too.
    let refuse_aggregates = |keys: &[&'a Expr<'a>]| -> Result<(), SqlError> {
        for key in keys {
            let mut nodes = ArenaList::new(arena);
            collect_aggs(key, &mut nodes, storage, txid)?;
            if !nodes.is_empty() {
                return Err(sql_err!(
                    sqlstate::GROUPING_ERROR,
                    "aggregate functions are not allowed in GROUP BY"
                ));
            }
        }
        Ok(())
    };
    let has_ordinals = statement
        .group_by
        .iter()
        .any(|grouping| matches!(grouping, Expr::Int(_)));
    let resolved: &[&Expr] = if has_ordinals {
        let resolved = arena
            .alloc_slice_with(statement.group_by.len(), |_| &Expr::Null)
            .map_err(|_| arena_full())?;
        for (slot, grouping) in resolved.iter_mut().zip(statement.group_by) {
            *slot = match grouping {
                Expr::Int(_) => {
                    resolve_position_target(grouping, statement.items, scope, arena, "GROUP BY")?
                }
                _ => grouping,
            };
        }
        &*resolved
    } else {
        statement.group_by
    };
    refuse_aggregates(resolved)?;

    // PostgreSQL stores visible expressions and otherwise absent grouping
    // expressions in one target list. Enforce that combined boundary rather
    // than accepting 1,664 hidden keys beside an additional visible result.
    let mut target_entries = 0usize;
    for item in statement.items {
        target_entries += match item {
            SelectItem::Wildcard => scope.map_or(0, QueryScope::star_columns),
            SelectItem::TableWildcard(qualifier) => match scope {
                Some(scope) => scope.qualified_star_columns(qualifier)?,
                None => 0,
            },
            SelectItem::RecordStar(expression) => scope
                .map(|scope| record_star_width(expression, scope))
                .transpose()?
                .unwrap_or(0),
            SelectItem::Expr { .. } => 1,
        };
    }
    for (index, grouping) in resolved.iter().enumerate() {
        let already_visible = statement.items.iter().any(|item| {
            let SelectItem::Expr { expression, .. } = item else {
                return false;
            };
            **expression == **grouping
                || scope.is_some_and(|scope| group::same_scope_column(scope, expression, grouping))
        });
        let already_counted = resolved[..index].iter().any(|previous| {
            **previous == **grouping
                || scope.is_some_and(|scope| group::same_scope_column(scope, previous, grouping))
        });
        if !already_visible && !already_counted {
            target_entries += 1;
        }
        if target_entries > super::parser::MAX_GROUP_TERMS {
            return Err(sql_err!(
                sqlstate::TOO_MANY_COLUMNS,
                "target lists can have at most 1664 entries"
            ));
        }
    }
    if has_ordinals {
        let mut rewritten = *statement;
        rewritten.group_by = resolved;
        Ok(&*arena.alloc(rewritten).map_err(|_| arena_full())?)
    } else {
        Ok(statement)
    }
}

/// ORDER BY `n` refers to the n-th *output* column: select-list stars count
/// one position per expanded column, as in PostgreSQL. A position inside a
/// star synthesizes the column reference; names and expressions delegate to
/// the select-list name-binding rules.
pub(super) fn resolve_order_target<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    resolve_position_target(expression, items, Some(scope), arena, "ORDER BY")
}

#[derive(Clone, Copy)]
enum SortKeySource {
    Projection(usize),
    EarlierKey(usize),
    Expression,
}

/// A sort key retains the output slot selected by an alias or position.
/// Equal expressions in two output slots can have different volatile values.
#[derive(Clone, Copy)]
pub(super) struct ResolvedSortKey<'a> {
    expression: &'a Expr<'a>,
    source: SortKeySource,
}

impl<'a> ResolvedSortKey<'a> {
    pub(super) fn value(
        self,
        projected: &[Datum<'a>],
        earlier: &[Datum<'a>],
        evaluate: impl FnOnce(&'a Expr<'a>) -> Result<Datum<'a>, SqlError>,
    ) -> Result<Datum<'a>, SqlError> {
        match self.source {
            SortKeySource::Projection(index) => Ok(projected[index]),
            SortKeySource::EarlierKey(index) => Ok(earlier[index]),
            SortKeySource::Expression => evaluate(self.expression),
        }
    }
}

pub(super) fn resolve_sort_key<'a>(
    original: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
    earlier: &[Option<ResolvedSortKey<'a>>],
) -> Result<ResolvedSortKey<'a>, SqlError> {
    let expression = resolve_order_target(original, items, scope, arena)?;
    let source = if let Expr::Int(position) = original {
        // Resolution above checked this against the expanded output width.
        SortKeySource::Projection(*position as usize - 1)
    } else {
        let mut index = 0;
        let mut equal_output = None;
        let mut named_output = None;
        for item in items {
            match item {
                SelectItem::Expr {
                    expression: output,
                    alias,
                } => {
                    if matches!(original, Expr::Column { qualifier: None, name }
                        if *name == alias.unwrap_or(super::exec::derived_name(output)))
                    {
                        named_output = Some(index);
                        break;
                    }
                    if equal_output.is_none() && **output == *expression {
                        equal_output = Some(index);
                    }
                    index += 1;
                }
                SelectItem::Wildcard => index += scope.star_columns(),
                SelectItem::TableWildcard(qualifier) => {
                    index += scope.qualified_star_columns(qualifier)?;
                }
                SelectItem::RecordStar(base) => index += record_star_width(base, scope)?,
            }
        }
        if let Some(index) = named_output.or(equal_output) {
            SortKeySource::Projection(index)
        } else if let Some(index) = earlier
            .iter()
            .position(|key| key.is_some_and(|key| *key.expression == *expression))
        {
            SortKeySource::EarlierKey(index)
        } else {
            SortKeySource::Expression
        }
    };
    Ok(ResolvedSortKey { expression, source })
}

/// An `ORDER BY` / `GROUP BY` target: a bare integer is a 1-based position in
/// the select list (a star item expanding to its columns), anything else is the
/// expression itself, matched against the select list's aliases.
fn resolve_position_target<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: Option<&QueryScope<'a>>,
    arena: &'a Arena,
    clause: &str,
) -> Result<&'a Expr<'a>, SqlError> {
    let Expr::Int(n) = expression else {
        return super::exec::resolve_order_expr_pub(expression, items);
    };
    let index = *n;
    let position_error = || {
        sql_err!(
            sqlstate::INVALID_COLUMN_REFERENCE,
            "{} position {} is not in select list",
            clause,
            index
        )
    };
    if index < 1 {
        return Err(position_error());
    }
    let column_ref = |qualifier: Option<&'a str>, name: &'a str| {
        Ok(&*arena
            .alloc(Expr::Column { qualifier, name })
            .map_err(|_| arena_full())?)
    };
    let mut remaining = index as usize - 1;
    for item in items {
        match item {
            SelectItem::Expr { expression, .. } => {
                if remaining == 0 {
                    return Ok(expression);
                }
                remaining -= 1;
            }
            SelectItem::Wildcard => {
                // A star without a FROM is invalid before any position could
                // resolve into it, so a missing scope reports the position.
                let Some(scope) = scope else {
                    return Err(position_error());
                };
                let width = scope.star_columns();
                if remaining < width {
                    return match scope.star_entry(remaining) {
                        ResolvedColumn::Table(t, c) => {
                            column_ref(Some(scope.names[t]), scope.defs.column_name(t, c))
                        }
                        // Unqualified: resolves back to the merged column.
                        ResolvedColumn::Merged(m) => column_ref(None, scope.merged[m].name),
                    };
                }
                remaining -= width;
            }
            SelectItem::TableWildcard(q) => {
                let Some(scope) = scope else {
                    return Err(position_error());
                };
                let width = scope.qualified_star_columns(q)?;
                if remaining < width {
                    let entry = scope.qualified_star_entry(q, remaining)?;
                    return column_ref(Some(q), scope.output_name(entry));
                }
                remaining -= width;
            }
            SelectItem::RecordStar(base) => {
                let Some(scope) = scope else {
                    return Err(position_error());
                };
                let width = record_star_width(base, scope)?;
                if remaining < width {
                    let target = remaining;
                    let mut result = None;
                    let mut index = 0usize;
                    super::exec::record_shape(base, &ScopeCols(scope), |name, _| {
                        if result.is_none() && index == target {
                            result =
                                Some(arena.alloc_str(name).map_err(|_| arena_full()).and_then(
                                    |field| {
                                        arena
                                            .alloc(Expr::Field { base, field })
                                            .map(|expression| &*expression)
                                            .map_err(|_| arena_full())
                                    },
                                ));
                        }
                        index += 1;
                    })?;
                    return result.unwrap_or_else(|| Err(position_error()));
                }
                remaining -= width;
            }
        }
    }
    Err(position_error())
}

/// The common type of a USING/NATURAL-merged column pair, per PostgreSQL's
/// `select_common_type` (the preferred type of the category wins). `None`
/// means the pair has no `=` operator — an error, as in PostgreSQL.
fn common_using_type(a: ColType, b: ColType) -> Option<ColType> {
    use ColType::*;
    if a == b {
        return Some(a);
    }
    if matches!((a, b), (Name, Text) | (Text, Name)) {
        return Some(a);
    }
    if matches!((a, b), (Name, Varchar | Bpchar) | (Varchar | Bpchar, Name)) {
        return Some(Name);
    }
    if let (ColType::Composite(slot), ColType::Record)
    | (ColType::Record, ColType::Composite(slot)) = (a, b)
    {
        return Some(ColType::Composite(slot));
    }
    let numeric_rank = |t: ColType| match t {
        Int2 => Some(0),
        Int4 => Some(1),
        Int8 => Some(2),
        Numeric => Some(3),
        Float4 => Some(4),
        Float8 => Some(5),
        _ => None,
    };
    if let (Some(ra), Some(rb)) = (numeric_rank(a), numeric_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    if matches!(a, Text | Varchar | Bpchar) && matches!(b, Text | Varchar | Bpchar) {
        return Some(Text);
    }
    let datetime_rank = |t: ColType| match t {
        Date => Some(0),
        Timestamp => Some(1),
        Timestamptz => Some(2),
        _ => None,
    };
    if let (Some(ra), Some(rb)) = (datetime_rank(a), datetime_rank(b)) {
        return Some(if ra >= rb { a } else { b });
    }
    if matches!(a, Bit { .. }) && matches!(b, Bit { .. }) {
        return Some(Bit { varying: true });
    }
    None
}

/// A view that PostgreSQL treats as auto-updatable: a single base table, no
/// aggregation/DISTINCT/GROUP BY/HAVING/LIMIT/joins, and every output column a
/// plain base column. `where_clause` is the view's own filter, to be AND-ed
/// into DML; `columns` are the exposed base columns.
pub struct UpdatableView<'a> {
    /// The base table, fully qualified from the view's own resolution so the
    /// rewritten DML binds the same table regardless of the session's path.
    pub base: QualName<'a>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub columns: &'a [&'a str],
    /// Base relation names in the same ordinal positions as `columns`.
    pub base_columns: &'a [&'a str],
    /// Durable defaults for the exposed columns, applied before base-table
    /// defaults when INSERT leaves a view column unspecified.
    pub defaults: crate::storage::ViewColumns,
    pub check_option: Option<crate::storage::ViewCheckOption>,
}

/// If `name` is a view, resolve it for DML: `Ok(Some(..))` when auto-updatable,
/// `Err` (0A000) when it is a view but not auto-updatable, `Ok(None)` when it
/// is not a view at all (the DML then targets a table normally).
/// Description and catalog callers share this metadata-only resolution.
/// Execution locks its target before resolving or selecting rewrite rules.
pub fn resolve_view_for_dml<'a>(
    storage: &Storage,
    name: QualName,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<UpdatableView<'a>>, SqlError> {
    let view_slot = match storage.resolve_relation(name.schema, name.name, txid) {
        Some(crate::storage::ResolvedRelation::View(slot)) => slot,
        _ => return Ok(None),
    };
    let check_option = storage.view(view_slot).check_option_for(txid);
    // The body re-resolves under the view creator's search path.
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let query = storage.snapshot_view_query(view_slot, txid, arena)?;
    let view_path = storage.compute_path(query.creation_path.as_str(), user.as_str(), txid);
    // Copy the definition into the arena so the parsed AST no longer borrows
    // storage (the caller then takes a mutable storage borrow to run the DML).
    let sql = arena
        .alloc_str(query.sql.as_str())
        .map_err(|_| arena_full())?;
    let name = name.name;
    let not_updatable = || {
        sql_err!(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot change view \"{}\": it is not auto-updatable",
            name
        )
    };
    let sel = super::parser::parse_view_select(sql, arena)?;
    let dependencies = query.dependencies;
    let sel = expand_stored_query(sel, storage, txid, view_path, dependencies.view(), arena)?;
    if sel.set_body.is_some()
        || sel.distinct
        || !sel.group_by.is_empty()
        || sel.having.is_some()
        || sel.limit.is_some()
        || sel.offset.is_some()
    {
        return Err(not_updatable());
    }
    let Some(from) = &sel.from else {
        return Err(not_updatable());
    };
    if !from.joins.is_empty() || from.base.subquery.is_some() {
        return Err(not_updatable());
    }
    let Some(crate::storage::ResolvedRelation::Table(ti)) =
        storage.resolve_relation_under(&view_path, from.base.schema, from.base.table, txid)
    else {
        return Err(not_updatable());
    };
    let base_def = storage.table_def(ti, txid);
    let base = QualName {
        schema: Some(
            arena
                .alloc_str(base_def.schema.as_str())
                .map_err(|_| arena_full())?,
        ),
        name: arena
            .alloc_str(base_def.name.as_str())
            .map_err(|_| arena_full())?,
    };
    let mut base_columns = [""; MAX_PROJ];
    let mut n = 0;
    for it in sel.items {
        match it {
            SelectItem::Wildcard => {
                for c in base_def.columns() {
                    if n == MAX_PROJ {
                        return Err(not_updatable());
                    }
                    // Copy into the arena so it does not borrow storage.
                    let base_column = arena.alloc_str(c.name.as_str()).map_err(|_| arena_full())?;
                    if base_columns[..n].contains(&base_column) {
                        return Err(not_updatable());
                    }
                    base_columns[n] = base_column;
                    n += 1;
                }
            }
            // The output must be a plain base column; catalog aliases map it
            // back to the same base name at the DML boundary.
            SelectItem::Expr {
                expression: Expr::Column { name: cn, .. },
                alias: _,
            } => {
                if n == MAX_PROJ {
                    return Err(not_updatable());
                }
                if base_columns[..n].contains(cn) {
                    return Err(not_updatable());
                }
                base_columns[n] = cn;
                n += 1;
            }
            _ => return Err(not_updatable()),
        }
    }
    let view_image = storage.view(view_slot);
    let columns = view_image.columns_for(txid);
    if columns.len() != n {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "view \"{}\" has an invalid output-column definition",
            name
        ));
    }
    let mut exposed_columns = [""; MAX_PROJ];
    for (target, column) in exposed_columns.iter_mut().zip(columns.names()) {
        *target = arena.alloc_str(column.as_str()).map_err(|_| arena_full())?;
    }
    let columns = arena
        .alloc_slice_copy(&exposed_columns[..n])
        .map_err(|_| arena_full())?;
    let base_columns = arena
        .alloc_slice_copy(&base_columns[..n])
        .map_err(|_| arena_full())?;
    Ok(Some(UpdatableView {
        base,
        where_clause: sel.where_clause,
        columns,
        base_columns,
        defaults: *storage.view(view_slot).columns_for(txid),
        check_option,
    }))
}

/// Whether a view satisfies the same structural predicate used by DML
/// rewriting. Catalog rendering must use this predicate too, otherwise
/// `information_schema.views` can disagree with executable behavior.
pub fn view_is_auto_updatable(
    storage: &Storage,
    schema: &str,
    name: &str,
    txid: u32,
    arena: &Arena,
) -> Result<bool, SqlError> {
    match resolve_view_for_dml(
        storage,
        QualName {
            schema: Some(schema),
            name,
        },
        txid,
        arena,
    ) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(error) if error.sqlstate == sqlstate::FEATURE_NOT_SUPPORTED => Ok(false),
        Err(error) => Err(error),
    }
}

/// Combines a view's filter with a DML's WHERE (AND), for view DML rewriting.
pub fn and_where<'a>(
    view_where: Option<&'a Expr<'a>>,
    dml_where: Option<&'a Expr<'a>>,
    arena: &'a Arena,
) -> Result<Option<&'a Expr<'a>>, SqlError> {
    match (view_where, dml_where) {
        (None, w) | (w, None) => Ok(w),
        (Some(a), Some(b)) => {
            let e = Expr::Binary {
                operator: super::ast::BinaryOp::And,
                left: a,
                right: b,
            };
            Ok(Some(&*arena.alloc(e).map_err(|_| arena_full())?))
        }
    }
}

/// Rejects pseudo-type view outputs after the query-descriptor boundary.
pub fn validate_view_columns(columns: &[ColDesc<'_>]) -> Result<(), SqlError> {
    for column in columns {
        if matches!(
            column.type_oid,
            super::types::oid::RECORD | super::types::oid::RECORD_ARRAY
        ) {
            let type_name = if column.type_oid == super::types::oid::RECORD {
                "record"
            } else {
                "record[]"
            };
            return Err(sql_err!(
                sqlstate::INVALID_TABLE_DEFINITION,
                "column \"{}\" has pseudo-type {}",
                column.name,
                type_name
            ));
        }
    }
    Ok(())
}

/// Resolves a query's output columns (name / type OID / typmod) without running
/// it — for CREATE VIEW validation and for building a CREATE TABLE AS backing
/// table. Returns the column count written into `out`.
pub fn describe_query<'a>(
    sql: &'a str,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    out: &mut [ColDesc<'a>],
) -> Result<usize, SqlError> {
    let sel = super::parser::parse_query(sql, arena)?;
    let sel = expand_ctes(sel, storage, txid, arena)?;
    describe_select(sel, storage, txid, arena, out)
}

/// Resolves a stored view body's output under the creator's captured search
/// path, rather than the session path of the client inspecting the view.
pub fn describe_query_under<'a>(
    sql: &'a str,
    storage: &'a Storage,
    txid: u32,
    path: crate::storage::PathContext,
    arena: &'a Arena,
    out: &mut [ColDesc<'a>],
) -> Result<usize, SqlError> {
    let select = super::parser::parse_query(sql, arena)?;
    let select = expand_ctes_under(select, storage, txid, path, arena)?;
    describe_select(select, storage, txid, arena, out)
}

pub(crate) fn describe_stored_query<'a>(
    sql: &'a str,
    storage: &'a Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: crate::storage::StoredQueryDependencyView<'_>,
    arena: &'a Arena,
    out: &mut [ColDesc<'a>],
) -> Result<usize, SqlError> {
    let select = super::parser::parse_query(sql, arena)?;
    let select = expand_stored_query(select, storage, txid, path, dependencies, arena)?;
    describe_select(select, storage, txid, arena, out)
}

pub(crate) fn describe_select<'a>(
    sel: &'a crate::sql::ast::Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    out: &mut [ColDesc<'a>],
) -> Result<usize, SqlError> {
    // A set-operation's output columns come from its leftmost SELECT branch.
    if let Some(body) = sel.set_body {
        return describe_set_tree(body, storage, txid, arena, out);
    }
    match &sel.from {
        Some(from) => {
            let scope = QueryScope::resolve_schema(storage, from, txid, arena, None)?;
            describe_select_items(sel.items, Some(&scope), storage, txid, arena, out)
        }
        None => describe_select_items(sel.items, None, storage, txid, arena, out),
    }
}

fn describe_set_tree<'a>(
    tree: &'a crate::sql::ast::SetTree<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    out: &mut [ColDesc<'a>],
) -> Result<usize, SqlError> {
    match tree {
        crate::sql::ast::SetTree::Select(s) => describe_select(s, storage, txid, arena, out),
        crate::sql::ast::SetTree::Op { left, .. } => {
            describe_set_tree(left, storage, txid, arena, out)
        }
    }
}

/// Walks an expression tree collecting aggregate call nodes. A windowed call
/// (`sum(x) OVER (...)`) is not one: it is a window function, and counting it
/// here would send the query down the grouped executor instead of the window
/// one — so its arguments are walked into like any other expression.
pub(super) fn collect_aggs<'a>(
    expression: &'a Expr<'a>,
    out: &mut ArenaList<'a, (*const Expr<'a>, &'a Expr<'a>)>,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    let catalog_aggregate = matches!(
        expression,
        Expr::Call { name, args, over: None, order_by, .. }
            if storage.has_aggregate_candidate(name, args.len(), txid)
                || storage.has_aggregate_candidate(name, args.len() + order_by.len(), txid)
    );
    if expression.is_aggregate_use() || catalog_aggregate {
        if out
            .as_slice()
            .iter()
            .any(|(p, _)| core::ptr::eq(*p, expression))
        {
            return Ok(());
        }
        out.push((expression as *const _, expression))
            .map_err(|_| arena_full())?;
        return Ok(()); // aggregate arguments evaluate per input row
    }
    walk_children(expression, &mut |child| {
        collect_aggs(child, out, storage, txid)
    })
}

/// Collects window-function call nodes (a `Call` with an `OVER` clause).
pub(super) fn collect_windows<'a>(
    expression: &'a Expr<'a>,
    out: &mut ArenaList<'a, &'a Expr<'a>>,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    if let Expr::Call {
        over: Some(_),
        distinct,
        order_by,
        filter,
        ..
    } = expression
    {
        // PostgreSQL rejects these call decorations on window functions in
        // parse analysis; accepting them would silently compute a different
        // query.
        if *distinct {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "DISTINCT is not implemented for window functions"
            ));
        }
        if !order_by.is_empty() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "aggregate ORDER BY is not implemented for window functions"
            ));
        }
        let catalog_aggregate = matches!(
            expression,
            Expr::Call { name, args, order_by, .. }
                if storage.has_aggregate_candidate(name, args.len(), txid)
                    || storage.has_aggregate_candidate(name, args.len() + order_by.len(), txid)
        );
        if filter.is_some() && !expression.is_aggregate() && !catalog_aggregate {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "FILTER is not implemented for non-aggregate window functions"
            ));
        }
        if out.as_slice().iter().any(|e| core::ptr::eq(*e, expression)) {
            return Ok(());
        }
        out.push(expression).map_err(|_| arena_full())?;
        // The arguments and PARTITION/ORDER expressions evaluate per input row;
        // a window function nested inside another is not supported and would be
        // found by the analysis pass, not here.
        return Ok(());
    }
    walk_children(expression, &mut |child| {
        collect_windows(child, out, storage, txid)
    })
}

/// Builds a `JoinRow` view over one flat materialized row (all scope columns
/// concatenated, table by table).
fn window_row<'r, 'a>(
    scope: &'r QueryScope<'a>,
    flat: &'r [Datum<'a>],
    offs: &'r [usize],
) -> JoinRow<'r, 'a, 'a> {
    JoinRow {
        scope,
        values: scan::JoinRowValues::Flat {
            values: flat,
            offsets: offs,
        },
        rowids: &[],
    }
}

/// Collects aggregate-call nodes for a grouped window query: aggregates
/// outside window functions plus those inside window arguments and keys
/// (`sum(sum(v)) OVER (...)`: the inner sum aggregates per group).
fn collect_grouped_aggs<'a>(
    e: &'a Expr<'a>,
    out: &mut ArenaList<'a, (*const Expr<'a>, &'a Expr<'a>)>,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    if let Expr::Call {
        over: Some(spec),
        args,
        filter,
        ..
    } = e
    {
        for a in *args {
            collect_grouped_aggs(a, out, storage, txid)?;
        }
        for pk in spec.partition_by {
            collect_grouped_aggs(pk, out, storage, txid)?;
        }
        for o in spec.order_by {
            collect_grouped_aggs(o.expression, out, storage, txid)?;
        }
        if let Some(frame) = &spec.frame {
            for bound in [&frame.start, &frame.end] {
                if let FrameBound::Preceding(x) | FrameBound::Following(x) = bound {
                    collect_grouped_aggs(x, out, storage, txid)?;
                }
            }
        }
        if let Some(f) = filter {
            collect_grouped_aggs(f, out, storage, txid)?;
        }
        return Ok(());
    }
    let catalog_aggregate = matches!(
        e,
        Expr::Call { name, args, order_by, .. }
            if storage.has_aggregate_candidate(name, args.len(), txid)
                || storage.has_aggregate_candidate(name, args.len() + order_by.len(), txid)
    );
    if e.is_aggregate() || catalog_aggregate {
        return collect_aggs(e, out, storage, txid);
    }
    // GROUPING() reads the current grouping-set mask, so it must evaluate in
    // the inner grouped select, like an aggregate.
    if let Expr::Call {
        name: "grouping",
        over: None,
        ..
    } = e
    {
        if out.as_slice().iter().any(|(p, _)| core::ptr::eq(*p, e)) {
            return Ok(());
        }
        out.push((e as *const _, e)).map_err(|_| arena_full())?;
        return Ok(());
    }
    walk_children(e, &mut |child| {
        collect_grouped_aggs(child, out, storage, txid)
    })
}

/// Context for [`rewrite_grouped_expr`]: the inner derived table's exposed
/// columns for each grouping key and aggregate.
struct GroupedRewrite<'a, 's> {
    group_by: &'a [&'a Expr<'a>],
    group_names: &'a [&'a str],
    aggs: &'a [(*const Expr<'a>, &'a Expr<'a>)],
    agg_names: &'a [&'a str],
    /// The source scope, to distinguish an unknown column (42703) from a
    /// known-but-ungrouped one (42803), as PostgreSQL.
    scope: Option<&'s QueryScope<'a>>,
}

/// Rewrites an outer-select expression of a grouped window query: aggregate
/// nodes and grouping-key expressions become references to the inner derived
/// table's columns; window calls keep their shape with rewritten insides. A
/// leftover bare column is the PostgreSQL grouping error (42803).
fn rewrite_grouped_expr<'a>(
    e: &'a Expr<'a>,
    context: &GroupedRewrite<'a, '_>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    if let Some(i) = context.aggs.iter().position(|(p, _)| core::ptr::eq(*p, e)) {
        return Ok(&*arena
            .alloc(Expr::Column {
                qualifier: None,
                name: context.agg_names[i],
            })
            .map_err(|_| arena_full())?);
    }
    if let Some(i) = context.group_by.iter().position(|g| **g == *e) {
        return Ok(&*arena
            .alloc(Expr::Column {
                qualifier: None,
                name: context.group_names[i],
            })
            .map_err(|_| arena_full())?);
    }
    let rewrite = |x: &'a Expr<'a>| rewrite_grouped_expr(x, context, arena);
    let alloc = |x: Expr<'a>| -> Result<&'a Expr<'a>, SqlError> {
        Ok(&*arena.alloc(x).map_err(|_| arena_full())?)
    };
    match e {
        Expr::WholeRow(t) => Err(sql_err!(
            sqlstate::GROUPING_ERROR,
            "column \"{}.*\" must appear in the GROUP BY clause or be used in an aggregate function",
            t
        )),
        Expr::SchemaColumn { table, name, .. } => Err(sql_err!(
            sqlstate::GROUPING_ERROR,
            "column \"{}.{}\" must appear in the GROUP BY clause or be used in an aggregate function",
            table,
            name
        )),
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::NumericLit(_)
        | Expr::Str(_)
        | Expr::BitLit(_)
        | Expr::Param(_)
        | Expr::DefaultMarker
        // Subqueries evaluate through their own hooks, not the group.
        | Expr::Subquery(_)
        | Expr::RowSubquery { .. }
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Ok(e),
        Expr::RoutineParam {
            qualifier, name, ..
        } => {
            if context
                .scope
                .is_some_and(|scope| scope.find_column(*qualifier, name).is_ok())
            {
                return Err(sql_err!(
                    sqlstate::GROUPING_ERROR,
                    "column \"{}{}{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                    qualifier.unwrap_or(""),
                    if qualifier.is_some() { "." } else { "" },
                    name
                ));
            }
            Ok(e)
        }
        Expr::Column { qualifier, name } => {
            // An unknown column errors as such; a known one is ungrouped.
            if let Some(scope) = context.scope
                && scope.find_column(*qualifier, name).is_err()
            {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                ));
            }
            Err(sql_err!(
                sqlstate::GROUPING_ERROR,
                "column \"{}{}{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                qualifier.unwrap_or(""),
                if qualifier.is_some() { "." } else { "" },
                name
            ))
        }
        Expr::Unary { operator, operand } => {
            alloc(Expr::Unary { operator: *operator, operand: rewrite(operand)? })
        }
        Expr::Binary { operator, left, right } => alloc(Expr::Binary {
            operator: *operator,
            left: rewrite(left)?,
            right: rewrite(right)?,
        }),
        Expr::Cast { operand, type_name, type_mod } => alloc(Expr::Cast {
            operand: rewrite(operand)?,
            type_name,
            type_mod: *type_mod,
        }),
        Expr::Collate { operand, collation } => alloc(Expr::Collate {
            operand: rewrite(operand)?,
            collation: *collation,
        }),
        Expr::IsNull { operand, negated } => {
            alloc(Expr::IsNull { operand: rewrite(operand)?, negated: *negated })
        }
        Expr::InList { operand, list, negated } => {
            let mut items = ArenaList::new(arena);
            for x in list.iter() {
                items.push(rewrite(x)?).map_err(|_| arena_full())?;
            }
            alloc(Expr::InList {
                operand: rewrite(operand)?,
                list: items.as_slice(),
                negated: *negated,
            })
        }
        Expr::Like { operand, pattern, negated, case_insensitive, escape } => alloc(Expr::Like {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
            escape: match escape {
                Some(e) => Some(rewrite(e)?),
                None => None,
            },
        }),
        Expr::Match { operand, pattern, negated, case_insensitive } => alloc(Expr::Match {
            operand: rewrite(operand)?,
            pattern: rewrite(pattern)?,
            negated: *negated,
            case_insensitive: *case_insensitive,
        }),
        Expr::Case { operand, whens, otherwise, synthetic } => {
            let operand = match operand {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            let mut pairs = ArenaList::new(arena);
            for (c, r) in whens.iter() {
                pairs
                    .push((rewrite(c)?, rewrite(r)?))
                    .map_err(|_| arena_full())?;
            }
            let whens = pairs.as_slice();
            let otherwise = match otherwise {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            alloc(Expr::Case { operand, whens, otherwise, synthetic: *synthetic })
        }
        Expr::Call {
            name,
            args,
            argument_names,
            variadic,
            star,
            distinct,
            order_by,
            over,
            filter,
        } => {
            let mut rewritten = ArenaList::new(arena);
            for a in args.iter() {
                rewritten.push(rewrite(a)?).map_err(|_| arena_full())?;
            }
            let args = rewritten.as_slice();
            let over = match over {
                None => None,
                Some(spec) => {
                    let mut parts = ArenaList::new(arena);
                    for pk in spec.partition_by.iter() {
                        parts.push(rewrite(pk)?).map_err(|_| arena_full())?;
                    }
                    let partition_by = parts.as_slice();
                    let mut obs = ArenaList::new(arena);
                    for o in spec.order_by.iter() {
                        obs.push(OrderBy { expression: rewrite(o.expression)?, ..*o })
                            .map_err(|_| arena_full())?;
                    }
                    let order_by = obs.as_slice();
                    let frame = match &spec.frame {
                        None => None,
                        Some(f) => {
                            let bound = |b: &FrameBound<'a>| -> Result<FrameBound<'a>, SqlError> {
                                Ok(match b {
                                    FrameBound::Preceding(x) => FrameBound::Preceding(rewrite(x)?),
                                    FrameBound::Following(x) => FrameBound::Following(rewrite(x)?),
                                    other => *other,
                                })
                            };
                            Some(WindowFrame {
                                units: f.units,
                                start: bound(&f.start)?,
                                end: bound(&f.end)?,
                                exclusion: f.exclusion,
                            })
                        }
                    };
                    let spec = super::ast::WindowSpec { partition_by, order_by, frame };
                    Some(&*arena.alloc(spec).map_err(|_| arena_full())?)
                }
            };
            let filter = match filter {
                None => None,
                Some(f) => Some(rewrite(f)?),
            };
            alloc(Expr::Call {
                name,
                args,
                argument_names,
                variadic: *variadic,
                star: *star,
                distinct: *distinct,
                order_by,
                over,
                filter,
            })
        }
        Expr::InSubquery { operand, select, negated } => alloc(Expr::InSubquery {
            operand: rewrite(operand)?,
            select,
            negated: *negated,
        }),
        Expr::QuantifiedSubquery {
            operand,
            operator,
            select,
            all,
        } => alloc(Expr::QuantifiedSubquery {
            operand: rewrite(operand)?,
            operator: *operator,
            select,
            all: *all,
        }),
        Expr::Array(items) => {
            let mut rewritten = ArenaList::new(arena);
            for x in items.iter() {
                rewritten.push(rewrite(x)?).map_err(|_| arena_full())?;
            }
            alloc(Expr::Array(rewritten.as_slice()))
        }
        Expr::Subscript { base, index } => {
            alloc(Expr::Subscript { base: rewrite(base)?, index: rewrite(index)? })
        }
        Expr::Slice { base, lower, upper } => alloc(Expr::Slice {
            base: rewrite(base)?,
            lower: lower.map(rewrite).transpose()?,
            upper: upper.map(rewrite).transpose()?,
        }),
        Expr::Field { base, field } => alloc(Expr::Field { base: rewrite(base)?, field }),
        Expr::RecordFieldIndex { base, index } => {
            alloc(Expr::RecordFieldIndex { base: rewrite(base)?, index: *index })
        }
        Expr::AnyAll { operand, operator, array, all } => alloc(Expr::AnyAll {
            operand: rewrite(operand)?,
            operator: *operator,
            array: rewrite(array)?,
            all: *all,
        }),
        Expr::RecursiveState { .. } => Ok(e),
    }
}

/// Check SELECT types before folding constants or requesting any input rows.
fn check_select_types<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    storage: &Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let columns = CatalogScopeCols {
        scope,
        outer_scope: None,
        storage,
        txid,
        arena,
    };
    let check = |e: &Expr| -> Result<(), SqlError> {
        if user_type_expression_description(e, "", storage, txid).is_some() {
            Ok(())
        } else {
            super::exec::infer_type_res(e, &columns).map(|_| ())
        }
    };
    // SELECT-list items first: PostgreSQL analyzes types before it folds
    // constants, so an invalid aggregate/operator (e.g. `min(boolean)`)
    // errors ahead of a constant division elsewhere in the query.
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            check(expression)?;
        }
    }
    if let Some(w) = statement.where_clause {
        check(w)?;
        let (oid, _) = super::exec::infer_type_res(w, &columns)?;
        let catalog = storage_catalog(storage, arena, txid);
        let boolean = oid == super::types::oid::BOOL
            || oid == super::types::oid::UNKNOWN
            || catalog.domain_base_type(oid) == Some(ColType::Bool);
        if !boolean {
            let type_name = match ColType::from_oid(oid) {
                Some(ctype) => ctype.name(),
                None => catalog.type_name(oid, arena)?.unwrap_or("unknown"),
            };
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "argument of WHERE must be type boolean, not type {}",
                type_name
            ));
        }
        if let Expr::Str(text) = w {
            super::eval::parse_bool(text)?;
        }
    }
    for g in statement.group_by {
        check(g)?;
    }
    if let Some(h) = statement.having {
        check(h)?;
    }
    for ob in statement.order_by {
        check(resolve_order_target(
            ob.expression,
            statement.items,
            scope,
            arena,
        )?)?;
    }
    Ok(())
}

/// Surfaces plan-time constant errors (e.g. `1/0`) across every expression
/// of a SELECT, matching PostgreSQL's constant folding.
pub fn check_select_constants<'a>(
    statement: &Select<'a>,
    arena: &'a Arena,
    storage: &Storage,
    txid: u32,
) -> Result<(), SqlError> {
    let catalog = storage_catalog(storage, arena, txid);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            super::eval::check_constant_errors(expression, arena, Some(&catalog))?;
        }
    }
    if let Some(w) = statement.where_clause {
        super::eval::check_constant_errors(w, arena, Some(&catalog))?;
    }
    for g in statement.group_by {
        super::eval::check_constant_errors(g, arena, Some(&catalog))?;
    }
    if let Some(h) = statement.having {
        super::eval::check_constant_errors(h, arena, Some(&catalog))?;
    }
    for ob in statement.order_by {
        super::eval::check_constant_errors(ob.expression, arena, Some(&catalog))?;
    }
    for bound in [statement.limit, statement.offset].into_iter().flatten() {
        super::eval::check_constant_errors(bound, arena, Some(&catalog))?;
    }
    Ok(())
}

/// The SELECT entry point (FROM present; FROM-less selects stay in the
/// engine).
/// True if the expression tree contains an aggregate *use* (a bare aggregate
/// call, not a window aggregate).
pub(super) fn expr_has_aggregate(expression: &Expr) -> bool {
    if expression.is_aggregate_use() {
        return true;
    }
    let mut found = false;
    let _ = subquery::walk_children(expression, &mut |child| {
        if expr_has_aggregate(child) {
            found = true;
        }
        Ok(())
    });
    found
}

/// True if the expression tree contains a window-function call (`OVER ...`).
pub(super) fn expr_has_window(expression: &Expr) -> bool {
    if matches!(expression, Expr::Call { over: Some(_), .. }) {
        return true;
    }
    let mut found = false;
    let _ = subquery::walk_children(expression, &mut |child| {
        if expr_has_window(child) {
            found = true;
        }
        Ok(())
    });
    found
}

/// True if `name` names a base table in `from` by the identifier a FOR-UPDATE
/// `OF` clause would use — the alias when one is given, else the table name.
fn from_binds_relation(from: Option<&FromClause>, name: &str) -> bool {
    let Some(from) = from else {
        return false;
    };
    core::iter::once(&from.base)
        .chain(from.joins.iter().map(|j| &j.table))
        .any(|r| r.alias.unwrap_or(r.table).eq_ignore_ascii_case(name))
}

/// Validates the analysis-time semantics of a query's `FOR UPDATE`/`FOR SHARE`/…
/// row-locking clauses, raising PostgreSQL's exact errors: `0A000` when the
/// clause combines with a construct it cannot lock (aggregates, GROUP BY,
/// HAVING, DISTINCT, window functions, or a set operation), and `42P01` when an
/// `OF` target names no relation in the FROM clause. Returns `Ok(())` when there
/// is no clause or every clause is well-formed.
///
pub fn validate_locking(statement: &Select) -> Result<(), SqlError> {
    for clause in statement.locking {
        let keyword = clause.strength.keyword();
        if statement.set_body.is_some() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} is not allowed with UNION/INTERSECT/EXCEPT",
                keyword
            ));
        }
        if statement.distinct {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} is not allowed with DISTINCT clause",
                keyword
            ));
        }
        if !statement.group_by.is_empty() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} is not allowed with GROUP BY clause",
                keyword
            ));
        }
        if statement.having.is_some() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "{} is not allowed with HAVING clause",
                keyword
            ));
        }
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                if expr_has_window(expression) {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "{} is not allowed with window functions",
                        keyword
                    ));
                }
                if expr_has_aggregate(expression) {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "{} is not allowed with aggregate functions",
                        keyword
                    ));
                }
            }
        }
        for of in clause.of {
            if !from_binds_relation(statement.from.as_ref(), of) {
                return Err(sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" in {} clause not found in FROM clause",
                    of,
                    keyword
                ));
            }
        }
    }
    Ok(())
}

/// Acquires every row lock contributed by one result row. Returns `false`
/// when `SKIP LOCKED` removes the row. A default-wait conflict becomes a
/// private control-flow error which the engine turns into a parked execution;
/// it is never serialized to the client.
pub(crate) fn lock_result_row(
    storage: &Storage,
    txid: u32,
    statement: &Select<'_>,
    scope: &QueryScope<'_>,
    rowids: &[Option<u64>],
) -> Result<bool, SqlError> {
    for clause in statement.locking {
        for (table, rowid) in rowids.iter().copied().enumerate().take(scope.n) {
            let targeted = if clause.of.is_empty() {
                scope.derived[table].is_none()
            } else {
                clause
                    .of
                    .iter()
                    .any(|name| scope.table_index(name).ok() == Some(table))
            };
            if !targeted {
                continue;
            }
            let Some(rowid) = rowid else {
                continue;
            };
            let slot = if scope
                .defs
                .get(table)
                .expect("locked source is resolved")
                .partition
                .is_partitioned()
            {
                storage
                    .partition_row_owner(scope.slots[table], rowid, txid)?
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "partitioned scan returned a row without a physical owner"
                        )
                    })?
            } else {
                scope.slots[table]
            };
            match storage.acquire_row_lock(slot, rowid, txid, clause.strength, clause.wait)? {
                crate::sql::lock::LockDecision::Acquired => {}
                crate::sql::lock::LockDecision::Skipped => return Ok(false),
                crate::sql::lock::LockDecision::Waiting => {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_LOCK_WAIT,
                        "statement is waiting for a row lock"
                    ));
                }
            }
        }
    }
    Ok(true)
}

pub fn select_query<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    seq: Option<&dyn SequenceAccess>,
    responder: &mut Responder,
) -> Outcome {
    select_query_resumable(
        storage, txid, statement, arena, params, seq, None, None, responder,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn select_query_resumable<'a, 'statement>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    seq: Option<&dyn SequenceAccess>,
    invocations: Option<&RoutineInvocationState<'statement>>,
    statement_arena: Option<&'statement Arena>,
    responder: &mut Responder,
) -> Outcome {
    let bound_catalog = storage_catalog(storage, arena, txid);
    let bound_queries = ProceduralSubqueries { storage, txid };
    let bound_hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&bound_catalog),
        subquery_executor: Some(&bound_queries),
        sequences: seq,
        ..crate::sql::eval::NO_HOOKS
    };

    let from = statement
        .from
        .as_ref()
        .expect("FROM-less handled by caller");
    // Windows over a grouped query evaluate after GROUP BY / HAVING: rewrite
    // to the two-level form up front (before the result is described) and run
    // the rewritten statement instead.
    {
        let mut win_probe = ArenaList::new(arena);
        let mut grouped_aggs = ArenaList::new(arena);
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                if let Err(e) = collect_windows(expression, &mut win_probe, storage, txid) {
                    return sql_fail(e);
                }
                if let Err(e) = collect_grouped_aggs(expression, &mut grouped_aggs, storage, txid) {
                    return sql_fail(e);
                }
            }
        }
        let has_srf = has_project_set(statement.items, storage, txid);
        if (!win_probe.is_empty() || has_srf)
            && (!statement.group_by.is_empty()
                || statement.having.is_some()
                || !grouped_aggs.is_empty())
        {
            let rewritten = match rewrite_grouped_windows(statement, storage, txid, arena) {
                Ok(r) => r,
                Err(e) => return sql_fail(e),
            };
            return select_query_resumable(
                storage,
                txid,
                rewritten,
                arena,
                params,
                seq,
                invocations,
                statement_arena,
                responder,
            );
        }
    }
    // Catalog relations (pg_catalog / information_schema) are synthesized and
    // registered as derived tables by resolve_exec, so they flow through the
    // general executor — joins, subqueries, aggregates, and ORDER BY included.
    let scope = match QueryScope::resolve_exec(
        storage,
        from,
        statement.where_clause,
        txid,
        arena,
        params,
        seq,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    // Relation locks are part of planning, before RowDescription. The scan
    // path repeats this idempotently for nested/correlated execution, but a
    // top-level wait must not leak a partial wire response.
    let relation_mode = if statement.locking.is_empty() {
        crate::sql::ast::TableLockMode::AccessShare
    } else {
        crate::sql::ast::TableLockMode::RowShare
    };
    for table in 0..scope.n {
        if scope.derived[table].is_none()
            && let Err(error) = storage.lock_table(txid, scope.slots[table], relation_mode, false)
        {
            return sql_fail(error);
        }
    }
    // A GROUP BY position names a select-list column; resolve it before
    // anything reads the grouping keys.
    let statement = match resolve_group_ordinals(statement, Some(&scope), arena, storage, txid) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    if let Err(e) = check_key_types(statement, &scope, arena) {
        return sql_fail(e);
    }

    // Subqueries first (uncorrelated, evaluated once).
    let n_items = statement.items.len();
    let n_order = statement.order_by.len();
    let sub_exprs = match arena.alloc_slice_with(4 + n_items + n_order + 2 * scope.n, |_| {
        Option::<&Expr>::None
    }) {
        Ok(s) => s,
        Err(_) => return sql_fail(arena_full()),
    };
    sub_exprs[0] = statement.where_clause;
    sub_exprs[1] = statement.having;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[4 + i] = Some(expression);
        }
    }
    // ORDER BY expressions may carry (correlated) subqueries too; a bare
    // ordinal resolves to a select item already covered above.
    for (i, ob) in statement.order_by.iter().enumerate() {
        if !matches!(ob.expression, Expr::Int(_)) {
            sub_exprs[4 + n_items + i] = Some(ob.expression);
        }
    }
    let sample_start = 4 + n_items + n_order;
    collect_table_sample_expressions(from, &mut sub_exprs[sample_start..]);
    // Uncorrelated subqueries are evaluated once; correlated ones are deferred
    // and re-evaluated per outer row during the scan.
    let outer_subs = match prepare_outer_subqueries(sub_exprs, storage, txid, arena, params, seq) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let correlated = outer_subs.correlated;
    let where_correlated = match correlated_in_expression(statement.where_clause, correlated, arena)
    {
        Ok(selected) => selected,
        Err(error) => return sql_fail(error),
    };
    let catalog = StorageCatalog {
        storage,
        routine_workspace: arena,
        txid,
        invocations,
        statement_arena,
    };
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&outer_subs.base),
        subquery_executor: None,
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        project_sets: None,
        sequences: seq,
        merge_action: None,
    };

    if let Err(error) = check_select_types(statement, &scope, storage, txid, arena) {
        return sql_fail(error);
    }

    // Constant folding runs after type analysis, matching PostgreSQL's
    // analyze-then-plan order: `min(boolean)` errors before `1/0` folds.
    if let Err(e) = check_select_constants(statement, arena, storage, txid) {
        return sql_fail(e);
    }

    // Result description.
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n_cols = match describe_scope_items(
        statement.items,
        &scope,
        None,
        storage,
        txid,
        arena,
        &mut columns,
    ) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
    if let Err(e) = patch_array_subquery_field_types(
        statement.items,
        Some(&scope),
        storage,
        txid,
        arena,
        &mut columns[..n_cols],
    ) {
        return sql_fail(e);
    }
    if let Err(error) = patch_subquery_column_types(
        statement.items,
        Some(&scope),
        &outer_subs.base,
        params,
        storage,
        txid,
        arena,
        &mut columns[..n_cols],
    ) {
        return sql_fail(error);
    }
    responder.row_description(&columns[..n_cols])?;

    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };
    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };

    // LIMIT 0 returns no rows without scanning or projecting anything, as
    // PostgreSQL does — so a per-row error in an unreturned row does not
    // surface (constant errors already surfaced via the plan-time check).
    if limit == 0 {
        if let Err(error) = scan::validate_materialized_view_reads(
            storage,
            &scope,
            statement.where_clause,
            arena,
            txid,
        ) {
            return sql_fail(error);
        }
        responder.command_complete("SELECT 0")?;
        return sql_ok();
    }

    // Window functions? They run over materialized rows before ORDER BY/LIMIT.
    // An ORDER BY key may be a window function without the select list holding
    // one (`ORDER BY rank() OVER (...)`), so it counts toward the decision.
    let mut win_nodes = ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item
            && let Err(e) = collect_windows(expression, &mut win_nodes, storage, txid)
        {
            return sql_fail(e);
        }
    }
    for ob in statement.order_by {
        if let Err(e) = collect_windows(ob.expression, &mut win_nodes, storage, txid) {
            return sql_fail(e);
        }
    }
    if !win_nodes.is_empty() {
        return window_select(
            storage,
            txid,
            statement,
            from,
            &scope,
            win_nodes.as_slice(),
            &hooks,
            correlated,
            &outer_subs.base,
            arena,
            params,
            limit,
            offset,
            responder,
        );
    }

    // Aggregates / GROUP BY?
    let mut agg_nodes = ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, storage, txid)
        {
            return sql_fail(e);
        }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, storage, txid)
    {
        return sql_fail(e);
    }
    for ob in statement.order_by {
        if let Err(e) = collect_aggs(ob.expression, &mut agg_nodes, storage, txid) {
            return sql_fail(e);
        }
    }
    if !agg_nodes.is_empty() || !statement.group_by.is_empty() {
        return grouped_select(
            storage,
            &scope,
            from,
            txid,
            statement,
            agg_nodes.as_slice(),
            arena,
            params,
            &hooks,
            correlated,
            limit,
            offset,
            responder,
        );
    }

    // Locking queries make a complete lock-acquisition pass before emitting
    // any wire output. Materialization gives that pass a rewindable row set,
    // including for queries that would otherwise stream.
    let needs_materialize =
        statement.distinct || !statement.order_by.is_empty() || !statement.locking.is_empty();
    if !needs_materialize {
        // Stream.
        let mut emitted = 0u64;
        let mut skipped = 0u64;
        let mut wire_full = false;
        let mut wire_result: Result<(), WireFull> = Ok(());
        // The scan applies only error-safe WHERE conjuncts independent of
        // correlated subqueries. The complete predicate still runs per row
        // against merged hooks after those subqueries have been evaluated.
        let where_in_scan =
            match correlated_scan_conjuncts(statement.where_clause, where_correlated, arena) {
                Ok(predicate) => predicate,
                Err(error) => return sql_fail(error),
            };
        // A set-returning `_pg_expandarray(array)` expands each row into one output
        // row per array element.
        let srf_call = find_srf(statement.items);
        let projection_has_project_set = has_project_set(statement.items, storage, txid);
        // This stream is the only path whose projection is consumed directly
        // from the scan callback. Its complete row demand is therefore known
        // here: projection expressions plus the complete WHERE. Every other
        // scan path retains full rows until it proves an equivalent contract.
        let pax_columns =
            streaming_pax_columns(&scope, from, statement.items, statement.where_clause);
        // Per-row correlated-subquery scratch, allocated once and reused: the
        // per-row rewind below reclaims the merged values, not this scratch.
        let (n_scalar_scratch, n_list_scratch) =
            subquery::merge_scratch_lens(Some(&outer_subs.base), correlated);
        let scalar_scratch: &mut CorrelatedScalarScratch = match arena
            .alloc_slice_with(n_scalar_scratch, |_| {
                (core::ptr::null(), Datum::Null, Datum::Null)
            }) {
            Ok(s) => s,
            Err(_) => return sql_fail(arena_full()),
        };
        let list_scratch: &mut CorrelatedListScratch =
            match arena.alloc_slice_with(n_list_scratch, |_| subquery::empty_subquery_list()) {
                Ok(s) => s,
                Err(_) => return sql_fail(arena_full()),
            };
        let scan = scan_source_recycling_with_pax_columns(
            storage,
            &scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            &hooks,
            None,
            pax_columns,
            &mut |row| {
                if emitted >= limit {
                    return Ok(false);
                }
                if !where_correlated.is_empty()
                    && !correlated_where_passes(
                        where_correlated,
                        &outer_subs.base,
                        statement.where_clause,
                        row,
                        storage,
                        txid,
                        arena,
                        params,
                        &hooks,
                        scalar_scratch,
                        list_scratch,
                    )?
                {
                    return Ok(true);
                }
                // Per-row hooks for correlated subqueries; then WHERE.
                let row_subqueries = correlated_row_subqueries(
                    correlated,
                    &outer_subs.base,
                    row,
                    storage,
                    txid,
                    arena,
                    params,
                    scalar_scratch,
                    list_scratch,
                    hooks.sequences,
                )?;
                let row_hooks_owned;
                let row_hooks: &EvalHooks = match row_subqueries.as_ref() {
                    Some(subqueries) => {
                        row_hooks_owned = correlated_row_hooks(&hooks, subqueries);
                        &row_hooks_owned
                    }
                    None => &hooks,
                };
                // Number of output rows this source row yields (1, unless an
                // `_pg_expandarray` expands it per array element).
                let project_set = if projection_has_project_set {
                    prepare_project_set(
                        statement.items,
                        storage,
                        txid,
                        arena,
                        params,
                        row,
                        row_hooks,
                    )?
                } else {
                    srf::ProjectSet::single()
                };
                for k in 1..=project_set.count {
                    if emitted >= limit {
                        break;
                    }
                    if !lock_result_row(storage, txid, statement, &scope, row.rowids)? {
                        continue;
                    }
                    if skipped < offset {
                        skipped += 1;
                        continue;
                    }
                    let srf_hooks;
                    let use_hooks: &EvalHooks = if srf_call.is_some() || project_set.any {
                        srf_hooks = EvalHooks {
                            srf_index: Some(k),
                            project_sets: Some(project_set.values),
                            ..*row_hooks
                        };
                        &srf_hooks
                    } else {
                        row_hooks
                    };
                    let mut projected = [Datum::Null; MAX_PROJ];
                    let n = project_row(
                        statement.items,
                        &scope,
                        row,
                        arena,
                        params,
                        use_hooks,
                        &mut projected,
                        None,
                    )?;
                    match emit_data_row(storage, txid, arena, responder, &projected[..n]) {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => return Err(error),
                        Err(wire) => {
                            wire_full = true;
                            wire_result = Err(wire);
                            return Ok(false);
                        }
                    }
                    emitted += 1;
                }
                Ok(true)
            },
        );
        if wire_full {
            return Err(WireFull);
        }
        if let Err(e) = scan {
            return sql_fail(e);
        }
        let tag = stack_format!(48, "SELECT {}", emitted);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // Materialize: visible columns + hidden ORDER BY keys (set-returning
    // functions expand inside the materializer).
    materialized_select(
        storage,
        &scope,
        from,
        txid,
        statement,
        arena,
        params,
        &hooks,
        correlated,
        &outer_subs.base,
        limit,
        offset,
        responder,
    )
}

/// Rewrites a FROM-less SELECT to read from a one-row derived table, so the
/// scanning path can run it. The virtual row a FROM-less SELECT already has is
/// spelled out as `(SELECT 1)`; nothing in the select list refers to it.
fn over_one_row<'a>(
    statement: &'a Select<'a>,
    arena: &'a Arena,
) -> Result<&'a Select<'a>, SqlError> {
    let one = arena.alloc(Expr::Int(1)).map_err(|_| arena_full())?;
    let items = arena
        .alloc_slice_copy(&[SelectItem::Expr {
            expression: one,
            alias: None,
        }])
        .map_err(|_| arena_full())?;
    let inner = Select {
        items,
        from: None,
        ..*statement
    };
    // The inner select carries only the row; every clause stays outside.
    let inner = Select {
        distinct: false,
        distinct_on: &[],
        where_clause: None,
        group_by: &[],
        grouping_sets: &[],
        having: None,
        order_by: &[],
        limit: None,
        offset: None,
        with_ties: false,
        with: &[],
        set_body: None,
        ..inner
    };
    let inner = &*arena.alloc(inner).map_err(|_| arena_full())?;
    let from = FromClause {
        base: TableRef {
            schema: None,
            table: "",
            alias: Some("?onerow"),
            subquery: Some(inner),
            func_args: None,
            func_argument_names: &[],
            func_variadic: false,
            rows_from: None,
            col_alias: None,
            inheritance: crate::sql::ast::RelationInheritance::Descendants,
            sample: None,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
            bound_table: None,
            view_access: None,
        },
        joins: &[],
    };
    Ok(&*arena
        .alloc(Select {
            from: Some(from),
            ..*statement
        })
        .map_err(|_| arena_full())?)
}

/// Admit the virtual input row once before scalar projection or set expansion.
/// Both wire results and INSERT/function row sources use this boundary.
fn prepare_fromless_projection<'a, R: ColumnLookup<'a>>(
    statement: &'a Select<'a>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    params: &[Datum<'a>],
    row: &R,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<srf::ProjectSet<'a>, SqlError> {
    if let Some(predicate) = statement.where_clause
        && !where_passes(predicate, arena, params, row, hooks)?
    {
        return Ok(srf::ProjectSet::empty());
    }
    prepare_project_set(statement.items, storage, txid, arena, params, row, hooks)
}

/// FROM-less `SELECT` (one virtual row, no columns). Item and WHERE
/// expressions may still contain subqueries — always uncorrelated here, since
/// there is no outer row to reference — so they are prepared once and injected
/// by node identity, exactly as the table path does.
pub fn constant_select<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    seq: Option<&dyn SequenceAccess>,
    responder: &mut Responder,
) -> Outcome {
    constant_select_resumable(
        storage, txid, statement, arena, params, seq, None, None, responder,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn constant_select_resumable<'a, 'statement>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    seq: Option<&dyn SequenceAccess>,
    invocations: Option<&RoutineInvocationState<'statement>>,
    statement_arena: Option<&'statement Arena>,
    responder: &mut Responder,
) -> Outcome {
    let bound_catalog = storage_catalog(storage, arena, txid);
    let bound_queries = ProceduralSubqueries { storage, txid };
    let bound_hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&bound_catalog),
        subquery_executor: Some(&bound_queries),
        sequences: seq,
        ..crate::sql::eval::NO_HOOKS
    };

    if let Err(e) = check_select_constants(statement, arena, storage, txid) {
        return sql_fail(e);
    }
    // A window function needs rows to compute over, and this path has no scan
    // to give it. A FROM-less SELECT is exactly one row, though, so it can be
    // written as a one-row derived table and handed to the ordinary path —
    // which already knows about partitions, frames and every window function
    // there is. Teaching this path any of that would be a second copy.
    let mut win_probe = ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item
            && let Err(e) = collect_windows(expression, &mut win_probe, storage, txid)
        {
            return sql_fail(e);
        }
    }
    if !win_probe.is_empty() {
        return match over_one_row(statement, arena) {
            Ok(wrapped) => select_query_resumable(
                storage,
                txid,
                wrapped,
                arena,
                params,
                seq,
                invocations,
                statement_arena,
                responder,
            ),
            Err(e) => sql_fail(e),
        };
    }
    // A GROUP BY position names a select-list column here too — a FROM-less
    // select has no scope, but its positions resolve against the items alone.
    let statement = match resolve_group_ordinals(statement, None, arena, storage, txid) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = match describe_catalog_items(statement.items, None, storage, txid, arena, &mut columns)
    {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };

    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            sub_exprs[1 + i] = Some(expression);
        }
    }
    let subs = match prepare_subqueries(
        &sub_exprs,
        storage,
        txid,
        arena,
        params,
        SUBQUERY_DEPTH,
        None,
        seq,
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    if let Err(e) = patch_array_subquery_field_types(
        statement.items,
        None,
        storage,
        txid,
        arena,
        &mut columns[..n],
    ) {
        return sql_fail(e);
    }
    if let Err(error) = patch_subquery_column_types(
        statement.items,
        None,
        &subs,
        params,
        storage,
        txid,
        arena,
        &mut columns[..n],
    ) {
        return sql_fail(error);
    }
    let catalog = StorageCatalog {
        storage,
        routine_workspace: arena,
        txid,
        invocations,
        statement_arena,
    };
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&subs),
        subquery_executor: None,
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        project_sets: None,
        sequences: seq,
        merge_action: None,
    };

    // Aggregates (or GROUP BY / HAVING) without FROM: PostgreSQL aggregates
    // over one virtual input row (zero when WHERE is false) and emits at most
    // one output row.
    let mut agg_nodes = ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, storage, txid)
        {
            return sql_fail(e);
        }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, storage, txid)
    {
        return sql_fail(e);
    }
    for ob in statement.order_by {
        if let Err(e) = collect_aggs(ob.expression, &mut agg_nodes, storage, txid) {
            return sql_fail(e);
        }
    }
    if !agg_nodes.is_empty() || statement.having.is_some() || !statement.group_by.is_empty() {
        if has_project_set(statement.items, storage, txid) {
            // The set-returning function expands after aggregation: rewrite
            // to the two-level form (aggregates in a derived table) and run
            // through the FROM executor.
            let rewritten = match rewrite_grouped_windows(statement, storage, txid, arena) {
                Ok(r) => r,
                Err(e) => return sql_fail(e),
            };
            return select_query_resumable(
                storage,
                txid,
                rewritten,
                arena,
                params,
                seq,
                invocations,
                statement_arena,
                responder,
            );
        }
        responder.row_description(&columns[..n])?;
        let hook_data = match fromless_aggregate_hooks(
            statement,
            agg_nodes.as_slice(),
            storage,
            txid,
            arena,
            params,
            &super::eval::NoColumns,
            &hooks,
        ) {
            Ok(d) => d,
            Err(e) => return sql_fail(e),
        };
        let mut rows = 0u64;
        if let Some((ptrs, values)) = hook_data {
            let agg_hooks = EvalHooks {
                aggs: Some((ptrs, values)),
                ..hooks
            };
            let mut vals = [Datum::Null; MAX_PROJ];
            for (i, item) in statement.items.iter().enumerate() {
                let SelectItem::Expr { expression, .. } = item else {
                    unreachable!("wildcard rejected by describe_items");
                };
                match eval_full(
                    expression,
                    arena,
                    params,
                    &super::eval::NoColumns,
                    &agg_hooks,
                ) {
                    Ok(v) => vals[i] = v,
                    Err(e) => return sql_fail(e),
                }
            }
            match emit_data_row(
                storage,
                txid,
                arena,
                responder,
                &vals[..statement.items.len()],
            ) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return sql_fail(error),
                Err(wire) => return Err(wire),
            }
            rows = 1;
        }
        let tag = stack_format!(48, "SELECT {}", rows);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // A set-returning function in the select list expands the single virtual
    // row into one output row per element/value.
    let srf_call = find_srf(statement.items);
    responder.row_description(&columns[..n])?;
    // Resolve ORDER BY targets against the select list: ordinals and output
    // names/expressions bind to an item (whose computed value is the key —
    // a set-returning item cannot re-evaluate outside its hook); anything
    // else evaluates per output row.
    // Each item occupies `col_start[i]..col_start[i+1]` output columns; a
    // `(record).*` item expands to several, everything else to one. `width` is
    // the true visible column count (matching the row description).
    let mut col_start = [0usize; MAX_PROJ + 1];
    {
        let mut col = 0usize;
        for (i, item) in statement.items.iter().enumerate() {
            col_start[i] = col;
            col += match item {
                SelectItem::RecordStar(_) => {
                    let mut described = [ColDesc::new("", 0, 0); MAX_PROJ];
                    match super::exec::describe_items(
                        core::slice::from_ref(item),
                        None,
                        None,
                        Some(storage),
                        txid,
                        arena,
                        &mut described,
                    ) {
                        Ok(width) => width,
                        Err(error) => return sql_fail(error),
                    }
                }
                _ => 1,
            };
        }
        col_start[statement.items.len()] = col;
    }
    let width = col_start[statement.items.len()];
    let n_order = statement.order_by.len();
    let mut order_item: [Option<usize>; MAX_PROJ] = [None; MAX_PROJ];
    for (j, ob) in statement.order_by.iter().enumerate() {
        if let Expr::Int(pos) = ob.expression {
            if *pos < 1 || *pos as usize > width {
                return sql_fail(sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "ORDER BY position {} is not in select list",
                    pos
                ));
            }
            order_item[j] = Some(*pos as usize - 1);
            continue;
        }
        // A non-ordinal key binds to the (single-column) item whose expression
        // or output name it matches; record-star items never match by name.
        order_item[j] = statement
            .items
            .iter()
            .position(|item| {
                matches!(item, SelectItem::Expr { expression, alias }
                if **expression == *ob.expression
                    || matches!(ob.expression, Expr::Column { qualifier: None, name }
                        if *name == alias.unwrap_or(super::exec::derived_name(expression))))
            })
            .map(|i| col_start[i]);
        if statement.distinct && order_item[j].is_none() {
            return sql_fail(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "for SELECT DISTINCT, ORDER BY expressions must appear in select list"
            ));
        }
    }

    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };
    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let project_set = if limit == 0 {
        srf::ProjectSet::empty()
    } else {
        match prepare_fromless_projection(
            statement,
            storage,
            txid,
            arena,
            params,
            &super::eval::NoColumns,
            &hooks,
        ) {
            Ok(project_set) => project_set,
            Err(error) => return sql_fail(error),
        }
    };

    // Materialize every output row (visible values + hidden sort keys).
    let mut n_rows = 0usize;
    let max_rows = project_set.count;
    let empty: &[u8] = &[];
    let encoded = match arena.alloc_slice_with(max_rows, |_| empty) {
        Ok(e) => e,
        Err(_) => return sql_fail(arena_full()),
    };
    for k in 1..=project_set.count {
        let khooks = if srf_call.is_some() || project_set.any {
            EvalHooks {
                srf_index: Some(k),
                project_sets: Some(project_set.values),
                ..hooks
            }
        } else {
            hooks
        };
        let mut values = [Datum::Null; MAX_PROJ + MAX_PROJ];
        for (i, item) in statement.items.iter().enumerate() {
            match item {
                SelectItem::Expr { expression, .. } => {
                    match eval_full(expression, arena, params, &super::eval::NoColumns, &khooks) {
                        Ok(v) => values[col_start[i]] = v,
                        Err(e) => return sql_fail(e),
                    }
                }
                SelectItem::RecordStar(base) => {
                    match super::eval::record_star_expand(
                        base,
                        arena,
                        params,
                        &super::eval::NoColumns,
                        &khooks,
                    ) {
                        Ok(fields) => {
                            for (k, f) in fields.iter().enumerate() {
                                values[col_start[i] + k] = f.value;
                            }
                        }
                        Err(e) => return sql_fail(e),
                    }
                }
                _ => unreachable!("wildcard rejected by describe_items in a FROM-less select"),
            }
        }
        for (j, ob) in statement.order_by.iter().enumerate() {
            values[width + j] = match order_item[j] {
                Some(i) => values[i],
                None => {
                    match eval_full(
                        ob.expression,
                        arena,
                        params,
                        &super::eval::NoColumns,
                        &khooks,
                    ) {
                        Ok(v) => v,
                        Err(e) => return sql_fail(e),
                    }
                }
            };
        }
        encoded[n_rows] = match super::exec::encode_projected_pub(&values[..width + n_order], arena)
        {
            Ok(b) => b,
            Err(e) => return sql_fail(e),
        };
        n_rows += 1;
    }
    let out_rows = &mut encoded[..n_rows];

    let mut live = out_rows.len();
    if statement.distinct {
        live = super::exec::sort_dedup_projected(out_rows, width);
    }
    let out_rows = &mut out_rows[..live];
    let mut order_collations = [super::ast::Collation::None; MAX_PROJ];
    for (index, order) in statement.order_by.iter().enumerate() {
        let expression = order_item[index]
            .and_then(|item| match statement.items[item] {
                SelectItem::Expr { expression, .. } => Some(expression),
                _ => None,
            })
            .unwrap_or(order.expression);
        let catalog = storage_catalog(storage, arena, txid);
        order_collations[index] = match resolved_expression_collation(
            expression,
            &super::eval::NoColumns,
            Some(&catalog),
        ) {
            Ok(collation) => collation,
            Err(error) => return sql_fail(error),
        };
    }
    if n_order > 0 {
        for row in out_rows.iter() {
            for (index, &collation) in order_collations.iter().enumerate().take(n_order) {
                if let Err(error) = storage.validate_text_collation(
                    collation,
                    &super::exec::decode_projected_pub(row, width + index),
                ) {
                    return sql_fail(error);
                }
            }
        }
        out_rows.sort_unstable_by(|a, b| {
            for (j, ob) in statement.order_by.iter().enumerate() {
                let ka = super::exec::decode_projected_pub(a, width + j);
                let kb = super::exec::decode_projected_pub(b, width + j);
                let ord = match (ka.is_null(), kb.is_null()) {
                    (true, true) => core::cmp::Ordering::Equal,
                    (true, false) => {
                        if ob.nulls_first {
                            core::cmp::Ordering::Less
                        } else {
                            core::cmp::Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if ob.nulls_first {
                            core::cmp::Ordering::Greater
                        } else {
                            core::cmp::Ordering::Less
                        }
                    }
                    (false, false) => {
                        let c = crate::sql::eval::compare_datums_collated(
                            storage,
                            txid,
                            order_collations[j],
                            &ka,
                            &kb,
                        )
                        .expect("validated FROM-less ORDER BY keys compare without error");
                        if ob.descending { c.reverse() } else { c }
                    }
                };
                if !ord.is_eq() {
                    return ord;
                }
            }
            core::cmp::Ordering::Equal
        });
    }

    let start = (offset as usize).min(out_rows.len());
    let mut end = offset.saturating_add(limit).min(out_rows.len() as u64) as usize;
    // FETCH FIRST ... WITH TIES over a FROM-less SRF result: keep rows tying
    // with the last on the ORDER BY keys (hidden columns after `width`).
    if statement.with_ties && limit > 0 {
        end = match materialize::extend_ties(
            storage,
            txid,
            &order_collations[..statement.order_by.len()],
            out_rows,
            width,
            statement.order_by.len(),
            end,
        ) {
            Ok(end) => end,
            Err(error) => return sql_fail(error),
        };
    }
    let mut rows = 0u64;
    for row in &out_rows[start..end] {
        let mut values = [Datum::Null; MAX_PROJ];
        for (i, slot) in values.iter_mut().take(width).enumerate() {
            *slot = match super::exec::decode_projected_col_record(row, i, arena) {
                Ok(value) => value,
                Err(error) => return sql_fail(error),
            };
        }
        match emit_data_row(storage, txid, arena, responder, &values[..width]) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return sql_fail(error),
            Err(wire) => return Err(wire),
        }
        rows += 1;
    }
    let tag = stack_format!(32, "SELECT {}", rows);
    responder.command_complete(tag.as_str())?;
    sql_ok()
}

/// Runs a `SELECT` used as an INSERT source, invoking `emit` once per output
/// row with that row's projected datums. The resulting table is unordered, so
/// ORDER BY is ignored; DISTINCT/GROUP BY/aggregate sources are rejected loudly
/// (not yet supported). Subqueries (including correlated) in the source are
/// supported.
#[allow(clippy::too_many_arguments)]
pub fn select_into_rows<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    seq: Option<&dyn SequenceAccess>,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    select_into_rows_mode(
        storage, txid, statement, arena, params, outer, seq, false, emit,
    )
}

pub(super) fn collect_table_sample_expressions<'a>(
    from: &'a FromClause<'a>,
    output: &mut [Option<&'a Expr<'a>>],
) {
    let mut at = 0;
    for table in core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)) {
        if let Some(sample) = table.sample {
            output[at] = Some(sample.percentage);
            output[at + 1] = sample.repeatable;
        }
        at += 2;
    }
}

/// Streaming row-source form for consumers that copy every emitted datum
/// before returning. In particular, external-run producers use it so a cold
/// SST scan recycles fetched row bytes instead of growing the statement arena.
#[allow(clippy::too_many_arguments)]
pub(super) fn select_into_rows_recycling<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    seq: Option<&dyn SequenceAccess>,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    select_into_rows_mode(
        storage, txid, statement, arena, params, outer, seq, true, emit,
    )
}

#[allow(clippy::too_many_arguments)]
fn select_into_rows_mode<'a>(
    storage: &'a Storage,
    txid: u32,
    statement: &'a Select<'a>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    outer: Option<&dyn ColumnLookup<'a>>,
    seq: Option<&dyn SequenceAccess>,
    recycle_rows: bool,
    emit: &mut dyn for<'row> FnMut(&[Datum<'row>]) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let bound_catalog = storage_catalog(storage, arena, txid);
    let bound_queries = ProceduralSubqueries { storage, txid };
    let bound_hooks = crate::sql::eval::EvalHooks {
        catalog: Some(&bound_catalog),
        subquery_executor: Some(&bound_queries),
        sequences: seq,
        ..crate::sql::eval::NO_HOOKS
    };

    if let Some(tree) = statement.set_body {
        if storage.spill_attached() {
            return setops::external_set_body_into(
                storage,
                txid,
                tree,
                statement.order_by,
                statement.limit,
                statement.offset,
                statement.with_ties,
                arena,
                params,
                seq,
                emit,
            );
        }
        let (mut rows, _target, n) = materialize_set_body(storage, txid, tree, arena, params, seq)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
        // A trailing ORDER BY needs one final sort over the combined multiset.
        let columns_owned: Option<[crate::sql::types::ColDesc; MAX_PROJ]> =
            (!statement.order_by.is_empty()).then(|| {
                let mut cols = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
                setops::describe_set_body(storage, tree, txid, &mut cols, arena, None).ok();
                cols
            });
        if let Some(ref cols) = columns_owned {
            let rows_mut = arena
                .alloc_slice_with(rows.len(), |i| rows[i])
                .map_err(|_| arena_full())?;
            setops::sort_set_rows(
                storage,
                txid,
                arena,
                rows_mut,
                statement.order_by,
                &cols[..n],
            )?;
            rows = rows_mut;
        }
        let start = (offset as usize).min(rows.len());
        let mut end = offset.saturating_add(limit).min(rows.len() as u64) as usize;
        if let Some(ref cols) = columns_owned
            && statement.with_ties
            && limit > 0
            && end < rows.len()
            && end > start
        {
            let boundary = rows[end - 1];
            while end < rows.len()
                && setops::set_rows_tie(
                    storage,
                    txid,
                    boundary,
                    rows[end],
                    statement.order_by,
                    &cols[..n],
                )?
            {
                end += 1;
            }
        }
        let mut vals = [Datum::Null; MAX_PROJ];
        for row in &rows[start..end] {
            for (c, slot) in vals[..n].iter_mut().enumerate() {
                *slot = super::exec::decode_projected_col_record(row, c, arena)?;
            }
            emit(&vals[..n])?;
        }
        return Ok(());
    }
    check_select_constants(statement, arena, storage, txid)?;
    let catalog = StorageCatalog {
        storage,
        routine_workspace: arena,
        txid,
        invocations: None,
        statement_arena: None,
    };
    let mut agg_nodes = ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
            collect_aggs(expression, &mut agg_nodes, storage, txid)?;
        }
    }
    if let Some(h) = statement.having {
        collect_aggs(h, &mut agg_nodes, storage, txid)?;
    }
    for ob in statement.order_by {
        collect_aggs(ob.expression, &mut agg_nodes, storage, txid)?;
    }
    // GROUP BY or aggregates: run the grouped executor (which sorts by any
    // ORDER BY and dedups DISTINCT) and emit each output row, honoring
    // LIMIT/OFFSET. A set-returning function expands after aggregation —
    // rewrite to the two-level form first.
    if (!statement.group_by.is_empty() || !agg_nodes.is_empty())
        && has_project_set(statement.items, storage, txid)
    {
        let rewritten = rewrite_grouped_windows(statement, storage, txid, arena)?;
        return select_into_rows_mode(
            storage,
            txid,
            rewritten,
            arena,
            params,
            outer,
            seq,
            recycle_rows,
            emit,
        );
    }
    if !statement.group_by.is_empty() || !agg_nodes.is_empty() {
        let Some(from) = &statement.from else {
            // FROM-less aggregate: one virtual input row.
            let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
            sub_exprs[0] = statement.where_clause;
            sub_exprs[1] = statement.having;
            for (i, item) in statement.items.iter().enumerate() {
                if let SelectItem::Expr { expression, .. } = item {
                    sub_exprs[2 + i] = Some(expression);
                }
            }
            let subs = prepare_subqueries(
                &sub_exprs,
                storage,
                txid,
                arena,
                params,
                SUBQUERY_DEPTH,
                outer,
                seq,
            )?;
            let hooks = EvalHooks {
                group: None,
                aggs: None,
                sequences: seq,
                subs: Some(&subs),
                subquery_executor: None,
                windows: None,
                catalog: Some(&catalog),
                srf_index: None,
                project_sets: None,
                merge_action: None,
            };
            let Some((ptrs, values)) = fromless_aggregate_hooks(
                statement,
                agg_nodes.as_slice(),
                storage,
                txid,
                arena,
                params,
                &super::eval::NoColumns,
                &hooks,
            )?
            else {
                return Ok(());
            };
            let agg_hooks = EvalHooks {
                aggs: Some((ptrs, values)),
                ..hooks
            };
            let offset =
                super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
            let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
            if limit == 0 || offset > 0 {
                return Ok(());
            }
            let mut vals = [Datum::Null; MAX_PROJ];
            let mut n = 0;
            for item in statement.items {
                match item {
                    SelectItem::Expr { expression, .. } => {
                        vals[n] = eval_full(
                            expression,
                            arena,
                            params,
                            &super::eval::NoColumns,
                            &agg_hooks,
                        )?;
                        n += 1;
                    }
                    SelectItem::RecordStar(base) => {
                        for field in super::eval::record_star_expand(
                            base,
                            arena,
                            params,
                            &super::eval::NoColumns,
                            &agg_hooks,
                        )? {
                            vals[n] = field.value;
                            n += 1;
                        }
                    }
                    _ => {
                        return Err(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "SELECT * with no tables specified is not valid"
                        ));
                    }
                }
            }
            emit(&vals[..n])?;
            return Ok(());
        };
        let scope = QueryScope::resolve_exec_outer(
            storage,
            from,
            statement.where_clause,
            txid,
            arena,
            params,
            seq,
            outer,
        )?;
        let statement = resolve_group_ordinals(statement, Some(&scope), arena, storage, txid)?;
        check_key_types(statement, &scope, arena)?;
        let sub_exprs = arena
            .alloc_slice_with(2 + statement.items.len() + 2 * scope.n, |_| None)
            .map_err(|_| arena_full())?;
        sub_exprs[0] = statement.where_clause;
        sub_exprs[1] = statement.having;
        for (i, item) in statement.items.iter().enumerate() {
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                sub_exprs[2 + i] = Some(expression);
            }
        }
        collect_table_sample_expressions(from, &mut sub_exprs[2 + statement.items.len()..]);
        let outer_subs = prepare_outer_subqueries(sub_exprs, storage, txid, arena, params, seq)?;
        let hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: Some(&outer_subs.base),
            subquery_executor: None,
            windows: None,
            catalog: Some(&catalog),
            srf_index: None,
            project_sets: None,
            sequences: seq,
            merge_action: None,
        };
        let (rows, width) = grouped_rows(
            storage,
            &scope,
            from,
            txid,
            statement,
            agg_nodes.as_slice(),
            arena,
            params,
            &hooks,
            outer_subs.correlated,
            outer,
        )?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
        let start = (offset as usize).min(rows.len());
        let n = ((rows.len() - start) as u64).min(limit) as usize;
        for row in &rows[start..start + n] {
            let mut out = [Datum::Null; MAX_PROJ];
            for (i, slot) in out.iter_mut().take(width).enumerate() {
                *slot = super::exec::decode_projected_col_record(row, i, arena)?;
            }
            emit(&out[..width])?;
        }
        return Ok(());
    }
    let sample_capacity = statement
        .from
        .as_ref()
        .map_or(0, |from| 2 * (from.joins.len() + 1));
    let sub_exprs = arena
        .alloc_slice_with(1 + statement.items.len() + sample_capacity, |_| None)
        .map_err(|_| arena_full())?;
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
            sub_exprs[1 + i] = Some(expression);
        }
    }

    let Some(from) = &statement.from else {
        // A window function here has nothing to compute over, so the single
        // virtual row is spelled out as a derived table and the whole query
        // re-enters through the scanning path (as `constant_select` does).
        let mut win_probe = ArenaList::new(arena);
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_windows(expression, &mut win_probe, storage, txid)?;
            }
        }
        if !win_probe.is_empty() {
            let wrapped = over_one_row(statement, arena)?;
            return select_into_rows_mode(
                storage,
                txid,
                wrapped,
                arena,
                params,
                outer,
                seq,
                recycle_rows,
                emit,
            );
        }
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
        if limit == 0 {
            return Ok(());
        }
        // A FROM-less body may still reference an outer row when it is a LATERAL
        // item (`LATERAL (SELECT t.a * 2)`); resolve against that outer scope.
        let cols: &dyn ColumnLookup = outer.unwrap_or(&super::eval::NoColumns);
        // FROM-less: one row (or zero, when WHERE is false), unless a
        // set-returning function in the list expands it to several.
        let subs = prepare_subqueries(
            sub_exprs,
            storage,
            txid,
            arena,
            params,
            SUBQUERY_DEPTH,
            outer,
            seq,
        )?;
        let hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: Some(&subs),
            subquery_executor: None,
            windows: None,
            catalog: Some(&catalog),
            srf_index: None,
            project_sets: None,
            sequences: seq,
            merge_action: None,
        };
        let srf_call = find_srf(statement.items);
        let project_set =
            prepare_fromless_projection(statement, storage, txid, arena, params, &cols, &hooks)?;
        let mut produced = 0u64;
        for k in 1..=project_set.count {
            let khooks = if srf_call.is_some() || project_set.any {
                EvalHooks {
                    srf_index: Some(k),
                    project_sets: Some(project_set.values),
                    ..hooks
                }
            } else {
                hooks
            };
            let mut vals = [Datum::Null; MAX_PROJ];
            let mut n = 0;
            for item in statement.items {
                match item {
                    SelectItem::Expr { expression, .. } => {
                        vals[n] = eval_full(expression, arena, params, &cols, &khooks)?;
                        n += 1;
                    }
                    SelectItem::RecordStar(base) => {
                        for field in
                            super::eval::record_star_expand(base, arena, params, &cols, &khooks)?
                        {
                            vals[n] = field.value;
                            n += 1;
                        }
                    }
                    _ => {
                        return Err(sql_err!(
                            sqlstate::SYNTAX_ERROR,
                            "SELECT * with no tables specified is not valid"
                        ));
                    }
                }
            }
            if produced >= offset {
                emit(&vals[..n])?;
            }
            produced = produced.saturating_add(1);
            if produced >= offset.saturating_add(limit) {
                break;
            }
        }
        return Ok(());
    };

    collect_table_sample_expressions(from, &mut sub_exprs[1 + statement.items.len()..]);
    let scope = QueryScope::resolve_exec_outer(
        storage,
        from,
        statement.where_clause,
        txid,
        arena,
        params,
        seq,
        outer,
    )?;
    let outer_subs = prepare_outer_subqueries(sub_exprs, storage, txid, arena, params, seq)?;
    let correlated = outer_subs.correlated;
    let where_correlated = correlated_in_expression(statement.where_clause, correlated, arena)?;
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&outer_subs.base),
        subquery_executor: None,
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        project_sets: None,
        sequences: seq,
        merge_action: None,
    };

    // Window functions (`OVER (...)`) in the projection: materialize the rows
    // with each window value computed, then emit. ORDER BY/LIMIT are handled by
    // the outer query, so the derived-table order is left unspecified.
    let mut win_nodes = ArenaList::new(arena);
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_windows(expression, &mut win_nodes, storage, txid)?;
        }
    }
    for ob in statement.order_by {
        collect_windows(ob.expression, &mut win_nodes, storage, txid)?;
    }
    if !win_nodes.is_empty() {
        // Windows over a grouped query: rewrite to the two-level form.
        let mut grouped_aggs = ArenaList::new(arena);
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_grouped_aggs(expression, &mut grouped_aggs, storage, txid)?;
            }
        }
        if !statement.group_by.is_empty() || statement.having.is_some() || !grouped_aggs.is_empty()
        {
            let rewritten = rewrite_grouped_windows(statement, storage, txid, arena)?;
            return select_into_rows_mode(
                storage,
                txid,
                rewritten,
                arena,
                params,
                outer,
                seq,
                recycle_rows,
                emit,
            );
        }
        if storage.spill_attached() {
            let offset =
                super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
            let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
            external_window_into(
                storage,
                txid,
                statement,
                from,
                &scope,
                win_nodes.as_slice(),
                &hooks,
                correlated,
                &outer_subs.base,
                arena,
                params,
                outer,
                limit,
                offset,
                &mut |values| {
                    emit(values)?;
                    Ok(true)
                },
            )?;
            return Ok(());
        }
        let (proj_rows, sort_keys) = project_window_rows(
            storage,
            txid,
            statement,
            from,
            &scope,
            win_nodes.as_slice(),
            &hooks,
            correlated,
            &outer_subs.base,
            arena,
            params,
            outer,
        )?;
        // DISTINCT dedups on the projected values; ORDER BY and LIMIT/OFFSET
        // apply here too (a derived table keeps its inner LIMIT).
        let (proj_rows, sort_keys) = if statement.distinct {
            dedup_window_rows(proj_rows, sort_keys, arena)?
        } else {
            (proj_rows, sort_keys)
        };
        let count = proj_rows.len();
        let order = arena
            .alloc_slice_with(count, |i| i)
            .map_err(|_| arena_full())?;
        if !statement.order_by.is_empty() {
            for x in 1..count {
                let mut y = x;
                while y > 0 {
                    let c = cmp_key_rows(
                        sort_keys[order[y - 1]],
                        sort_keys[order[y]],
                        statement.order_by,
                    );
                    if c == core::cmp::Ordering::Greater {
                        order.swap(y - 1, y);
                        y -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
        let start = (offset as usize).min(count);
        let n = ((count - start) as u64).min(limit) as usize;
        for &i in &order[start..start + n] {
            emit(proj_rows[i])?;
        }
        return Ok(());
    }

    // DISTINCT / ORDER BY / LIMIT / OFFSET need the whole set materialized
    // (so top-N and dedup are correct), then paged.
    if statement.distinct
        || !statement.order_by.is_empty()
        || statement.limit.is_some()
        || statement.offset.is_some()
    {
        if storage.spill_attached() {
            let offset =
                super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
            let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
            external_materialized_into(
                storage,
                &scope,
                from,
                txid,
                statement,
                arena,
                params,
                &hooks,
                correlated,
                &outer_subs.base,
                outer,
                limit,
                offset,
                &mut |values, _rowids| {
                    emit(values)?;
                    Ok(true)
                },
            )?;
            return Ok(());
        }
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
        let (rows, width, deferred, _identities_at) = materialized_rows(
            storage,
            &scope,
            from,
            txid,
            statement,
            arena,
            params,
            &hooks,
            correlated,
            &outer_subs.base,
            outer,
            (statement.limit.is_some()
                && limit != u64::MAX
                && !statement.with_ties
                && statement.locking.is_empty())
            .then_some(offset.saturating_add(limit).min(usize::MAX as u64) as usize),
        )?;
        // OFFSET rows flow through PostgreSQL's projection before Limit
        // discards them, so deferred items are evaluated for them too (their
        // errors surface); only rows past the offset are emitted.
        let window = offset.saturating_add(limit).min(usize::MAX as u64) as usize;
        for (index, row) in rows.iter().take(window).enumerate() {
            let mut out = [Datum::Null; MAX_PROJ];
            finalize_projected_row(
                row,
                width,
                deferred.as_ref(),
                statement,
                &scope,
                arena,
                params,
                &hooks,
                &mut out,
            )?;
            if (index as u64) >= offset {
                emit(&out[..width])?;
            }
        }
        return Ok(());
    }
    let where_in_scan = correlated_scan_conjuncts(statement.where_clause, where_correlated, arena)?;
    let offset = super::exec::eval_offset_pub(statement.offset, arena, params, &bound_hooks)?;
    let limit = super::exec::eval_limit_pub(statement.limit, arena, params, &bound_hooks)?;
    let stop_after = offset.saturating_add(limit);
    if stop_after == 0 {
        return Ok(());
    }
    let mut produced = 0u64;

    // A set-returning `_pg_expandarray(array)` in the projection expands each
    // source row into one output row per array element.
    let srf_call = find_srf(statement.items);
    let projection_has_project_set = has_project_set(statement.items, storage, txid);
    // Per-row correlated-subquery scratch, allocated once and reused across
    // the scan (this path may run without per-row arena recycling).
    let (n_scalar_scratch, n_list_scratch) =
        subquery::merge_scratch_lens(Some(&outer_subs.base), correlated);
    let scalar_scratch: &mut CorrelatedScalarScratch = arena
        .alloc_slice_with(n_scalar_scratch, |_| {
            (core::ptr::null(), Datum::Null, Datum::Null)
        })
        .map_err(|_| arena_full())?;
    let list_scratch: &mut CorrelatedListScratch = arena
        .alloc_slice_with(n_list_scratch, |_| subquery::empty_subquery_list())
        .map_err(|_| arena_full())?;
    let mut visit = |row: &JoinRow<'_, 'a, '_>| {
        if !where_correlated.is_empty()
            && !correlated_where_passes(
                where_correlated,
                &outer_subs.base,
                statement.where_clause,
                row,
                storage,
                txid,
                arena,
                params,
                &hooks,
                scalar_scratch,
                list_scratch,
            )?
        {
            return Ok(true);
        }
        let row_subqueries = correlated_row_subqueries(
            correlated,
            &outer_subs.base,
            row,
            storage,
            txid,
            arena,
            params,
            scalar_scratch,
            list_scratch,
            hooks.sequences,
        )?;
        let row_hooks_owned;
        let row_hooks: &EvalHooks = match row_subqueries.as_ref() {
            Some(subqueries) => {
                row_hooks_owned = correlated_row_hooks(&hooks, subqueries);
                &row_hooks_owned
            }
            None => &hooks,
        };
        let mut projected = [Datum::Null; MAX_PROJ];
        let project_set = if projection_has_project_set {
            prepare_project_set(
                statement.items,
                storage,
                txid,
                arena,
                params,
                row,
                row_hooks,
            )?
        } else {
            srf::ProjectSet::single()
        };
        if !project_set.any && srf_call.is_none() {
            let n = project_row(
                statement.items,
                &scope,
                row,
                arena,
                params,
                row_hooks,
                &mut projected,
                outer,
            )?;
            if produced >= offset {
                emit(&projected[..n])?;
            }
            produced = produced.saturating_add(1);
        } else {
            for k in 1..=project_set.count {
                let srf_hooks = EvalHooks {
                    srf_index: Some(k),
                    project_sets: Some(project_set.values),
                    ..*row_hooks
                };
                let n = project_row(
                    statement.items,
                    &scope,
                    row,
                    arena,
                    params,
                    &srf_hooks,
                    &mut projected,
                    outer,
                )?;
                if produced >= offset {
                    emit(&projected[..n])?;
                }
                produced = produced.saturating_add(1);
                if produced >= stop_after {
                    break;
                }
            }
        }
        Ok(produced < stop_after)
    };
    let pax_columns = streaming_pax_columns(&scope, from, statement.items, statement.where_clause);
    if recycle_rows {
        scan_source_recycling_with_pax_columns(
            storage,
            &scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            &hooks,
            outer,
            pax_columns,
            &mut visit,
        )
    } else {
        scan_source_with_pax_columns(
            storage,
            &scope,
            from,
            txid,
            where_in_scan,
            arena,
            params,
            &hooks,
            outer,
            pax_columns,
            &mut visit,
        )
    }
}

pub(super) fn streaming_pax_columns<'a>(
    scope: &QueryScope<'a>,
    from: &'a FromClause<'a>,
    items: &'a [SelectItem<'a>],
    where_clause: Option<&'a Expr<'a>>,
) -> PaxReadDemand {
    let mut expressions: [&Expr; MAX_PROJ + 1] = [&Expr::Null; MAX_PROJ + 1];
    let mut count = 0usize;
    for item in items {
        let expression = match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => expression,
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                return PaxReadDemand::full_row(scan::PaxFullRowReason::WildcardProjection);
            }
        };
        expressions[count] = expression;
        count += 1;
    }
    if let Some(where_clause) = where_clause {
        expressions[count] = where_clause;
        count += 1;
    }
    pax_column_demand(scope, from, &expressions[..count])
}

/// Projects one source row through the select items.
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn project_row<'a>(
    items: &[SelectItem<'a>],
    scope: &QueryScope,
    row: &JoinRow<'_, 'a, '_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
    // Enclosing query's row when this is a correlated subquery's body, so an
    // outer column in the select list resolves after this row's own.
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<usize, SqlError> {
    project_row_skipping(items, None, scope, row, arena, params, hooks, out, outer)
}

/// [`project_row`], with `skip` marking items whose evaluation is deferred
/// until after the sort (their slots stay NULL placeholders).
#[expect(clippy::too_many_arguments, reason = "query pipeline plumbing")]
fn project_row_skipping<'a>(
    items: &[SelectItem<'a>],
    skip: Option<&[bool; MAX_PROJ]>,
    scope: &QueryScope,
    row: &JoinRow<'_, 'a, '_>,
    arena: &'a Arena,
    params: &[Datum<'a>],
    hooks: &EvalHooks<'_, 'a>,
    out: &mut [Datum<'a>; MAX_PROJ],
    outer: Option<&dyn ColumnLookup<'a>>,
) -> Result<usize, SqlError> {
    // Star expansion reads this query's tables directly; only expressions may
    // reach past them to the enclosing row.
    let chained = Chained { inner: row, outer };
    let mut n = 0;
    for (item_index, item) in items.iter().enumerate() {
        if skip.is_some_and(|s| s[item_index]) {
            // A postponed item occupies one slot (wildcards are never skipped).
            if n == MAX_PROJ {
                return Err(sql_err!(
                    sqlstate::TOO_MANY_COLUMNS,
                    "target lists can have at most 1664 entries"
                ));
            }
            out[n] = Datum::Null;
            n += 1;
            continue;
        }
        match item {
            SelectItem::TableWildcard(q) => {
                let value_of = |entry| match entry {
                    ResolvedColumn::Table(t, c) => {
                        let values = row.table_values(t).expect("bound");
                        if values.is_empty() {
                            Datum::Null
                        } else {
                            values[c]
                        }
                    }
                    ResolvedColumn::Merged(m) => {
                        let merged = &scope.merged[m];
                        merged
                            .parts
                            .iter()
                            .map(|&(t, c)| {
                                let values = row.table_values(t).expect("bound");
                                if values.is_empty() {
                                    Datum::Null
                                } else {
                                    values[c]
                                }
                            })
                            .find(|value| !value.is_null())
                            .unwrap_or(Datum::Null)
                    }
                };
                for index in 0..scope.qualified_star_columns(q)? {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::TOO_MANY_COLUMNS,
                            "target lists can have at most 1664 entries"
                        ));
                    }
                    out[n] = value_of(scope.qualified_star_entry(q, index)?);
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                let value_of = |t: usize, c: usize| {
                    let vals = row.table_values(t).expect("bound");
                    if vals.is_empty() {
                        Datum::Null
                    } else {
                        vals[c]
                    }
                };
                for k in 0..scope.star_columns() {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::TOO_MANY_COLUMNS,
                            "target lists can have at most 1664 entries"
                        ));
                    }
                    out[n] = match scope.star_entry(k) {
                        ResolvedColumn::Table(t, c) => value_of(t, c),
                        // Merged USING/NATURAL column: first non-null side.
                        ResolvedColumn::Merged(m) => {
                            let mc = &scope.merged[m];
                            mc.parts
                                .iter()
                                .map(|&(t, c)| value_of(t, c))
                                .find(|v| !v.is_null())
                                .unwrap_or(Datum::Null)
                        }
                    };
                    n += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                for field in super::eval::record_star_expand(base, arena, params, row, hooks)? {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::TOO_MANY_COLUMNS,
                            "target lists can have at most 1664 entries"
                        ));
                    }
                    out[n] = field.value;
                    n += 1;
                }
            }
            SelectItem::Expr { expression, .. } => {
                if n == MAX_PROJ {
                    return Err(sql_err!(
                        sqlstate::TOO_MANY_COLUMNS,
                        "target lists can have at most 1664 entries"
                    ));
                }
                out[n] = eval_full(expression, arena, params, &chained, hooks)?;
                n += 1;
            }
        }
    }
    materialize_composite_outputs(&mut out[..n], arena, hooks)?;
    Ok(n)
}

/// Result rows must retain the structural composite value through the final
/// protocol encoder. Storage keeps a canonical text representation, but
/// turning that text into an anonymous record here would lose the declared
/// field OIDs required by PostgreSQL binary Result and COPY.
fn materialize_composite_outputs<'a>(
    values: &mut [Datum<'a>],
    arena: &'a Arena,
    hooks: &EvalHooks<'_, 'a>,
) -> Result<(), SqlError> {
    let Some(catalog) = hooks.catalog else {
        return Ok(());
    };
    for value in values {
        *value = super::eval::materialize_enum_values(*value, Some(catalog), arena)?;
        *value = match *value {
            Datum::CompositeText {
                slot,
                physical_fields,
                text,
            } => catalog.materialize_composite(slot, physical_fields, text, arena)?,
            other => other,
        };
    }
    Ok(())
}

/// Column descriptions across the whole scope (wildcards expand every
/// table).
pub fn describe_scope_items<'q>(
    items: &[SelectItem<'q>],
    scope: &QueryScope<'q>,
    outer_scope: Option<&QueryScope<'q>>,
    storage: &Storage,
    txid: u32,
    arena: &'q Arena,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    let mut n = 0;
    for item in items {
        match item {
            SelectItem::TableWildcard(q) => {
                for index in 0..scope.qualified_star_columns(q)? {
                    if n == out.len() {
                        return Err(sql_err!(
                            sqlstate::TOO_MANY_COLUMNS,
                            "target lists can have at most 1664 entries"
                        ));
                    }
                    let entry = scope.qualified_star_entry(q, index)?;
                    out[n] = ColDesc::of_type(scope.output_name(entry), scope.output_type(entry))
                        .with_type_mod(match entry {
                            ResolvedColumn::Table(table, column) => {
                                scope.defs.get(table).expect("resolved").columns[column].type_mod
                            }
                            ResolvedColumn::Merged(_) => -1,
                        });
                    out[n].collation = scope.output_collation(entry);
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                for k in 0..scope.star_columns() {
                    if n == out.len() {
                        return Err(sql_err!(
                            sqlstate::TOO_MANY_COLUMNS,
                            "target lists can have at most 1664 entries"
                        ));
                    }
                    let entry = scope.star_entry(k);
                    out[n] = ColDesc::of_type(scope.output_name(entry), scope.output_type(entry))
                        .with_type_mod(match entry {
                            ResolvedColumn::Table(t, c) => {
                                scope.defs.get(t).expect("resolved").columns[c].type_mod
                            }
                            ResolvedColumn::Merged(_) => -1,
                        });
                    out[n].collation = scope.output_collation(entry);
                    n += 1;
                }
            }
            SelectItem::RecordStar(base) => {
                n = describe_scope_record_star(base, scope, storage, txid, arena, out, n)?;
            }
            SelectItem::Expr { expression, alias } => {
                if n == out.len() {
                    return Err(sql_err!(
                        sqlstate::TOO_MANY_COLUMNS,
                        "target lists can have at most 1664 entries"
                    ));
                }
                // Multi-table type inference: columns resolve via scope.
                let name = alias.unwrap_or(super::exec::derived_name(expression));
                let resolver = CatalogScopeCols {
                    scope,
                    outer_scope,
                    storage,
                    txid,
                    arena,
                };
                if let Expr::Cast { .. } = expression {
                    super::exec::infer_type_res(expression, &resolver)?;
                }
                let user_type = user_type_expression_description(expression, name, storage, txid);
                let (mut oid, mut typlen) = match user_type {
                    Some(description) => (description.type_oid, description.typlen),
                    None => {
                        let (oid, typlen) = super::exec::infer_type_res(expression, &resolver)?;
                        if oid == super::types::oid::UNKNOWN {
                            (super::types::oid::TEXT, -1)
                        } else {
                            (oid, typlen)
                        }
                    }
                };
                // A bare column carries its declared modifier and a cast its
                // target's, as RowDescription reports them; anything computed
                // is -1 — the rule PostgreSQL follows.
                let field_meta = match expression {
                    Expr::Field { base, field } if user_type.is_none() => {
                        super::exec::record_field_metadata(base, field, &resolver).ok()
                    }
                    _ => None,
                };
                if let Some(meta) = field_meta {
                    oid = meta.ctype.oid();
                    typlen = meta.ctype.typlen();
                } else if let Some(meta) =
                    super::exec::routine_result_metadata(expression, &resolver)
                {
                    oid = meta.ctype.oid();
                    typlen = meta.ctype.typlen();
                }
                let type_mod = match expression {
                    Expr::Column { qualifier, name } => {
                        scope_column_type_mod(scope, outer_scope, *qualifier, name)
                    }
                    Expr::Cast { type_mod, .. } => *type_mod,
                    _ => field_meta.map_or(-1, |meta| meta.type_mod),
                };
                out[n] = ColDesc::new(name, oid, typlen)
                    .with_type_mod(user_type.map_or(type_mod, |description| description.type_mod));
                if let Some(ctype) = super::exec::coltype_of_oid(oid)
                    && ctype.is_collatable()
                {
                    (out[n].collation, out[n].collation_derivation) =
                        scope.described_expression_collation(storage, arena, expression)?;
                    if out[n].collation_derivation == super::types::CollationDerivation::None {
                        out[n].collation = super::ast::Collation::Default;
                        out[n].collation_derivation = super::types::CollationDerivation::Implicit;
                    }
                } else if let Some(meta) = field_meta {
                    out[n].collation = meta.collation;
                }
                n += 1;
            }
        }
    }
    for column in &mut out[..n] {
        *column = super::exec::wire_column_description(*column, storage, txid)?;
    }
    Ok(n)
}

/// Catalog-aware wrapper around the ordinary single-table/FROM-less
/// descriptor. Static inference deliberately has no catalog dependency, so an
/// exact cast to a domain/enum would otherwise be described as text even
/// though evaluation returns its base/enum representation.
pub fn describe_catalog_items<'q>(
    items: &[SelectItem<'q>],
    definition: Option<&'q TableDef>,
    storage: &'q Storage,
    txid: u32,
    arena: &'q Arena,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    describe_catalog_items_as(items, definition, None, storage, txid, arena, out)
}

pub fn describe_catalog_items_as<'q>(
    items: &[SelectItem<'q>],
    definition: Option<&'q TableDef>,
    alias: Option<&str>,
    storage: &'q Storage,
    txid: u32,
    arena: &'q Arena,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    // User-defined casts carry identity and, for named composites, field
    // shape through the catalog. Resolve them before the catalog-free
    // descriptor pass: static inference deliberately cannot reconstruct that
    // information from an expression such as `(composite_array[1]).field`.
    let mut count = 0;
    let mut item_width = [0usize; MAX_PROJ];
    for (item_index, item) in items.iter().enumerate() {
        if let SelectItem::Expr {
            expression: expression @ Expr::Cast { .. },
            ..
        } = item
        {
            super::exec::infer_type_res(
                expression,
                &super::exec::CatalogCols {
                    definition,
                    alias,
                    output_aliases: &[],
                    storage,
                    txid,
                    arena,
                },
            )?;
        }
        if let SelectItem::Expr { expression, alias } = item
            && let Some(description) = user_type_expression_description(
                expression,
                alias.unwrap_or(super::exec::derived_name(expression)),
                storage,
                txid,
            )
        {
            if count == out.len() {
                return Err(sql_err!(
                    sqlstate::TOO_MANY_COLUMNS,
                    "target lists can have at most 1664 entries"
                ));
            }
            out[count] = description;
            count += 1;
            item_width[item_index] = 1;
            continue;
        }
        let width = describe_items(
            core::slice::from_ref(item),
            definition,
            alias,
            Some(storage),
            txid,
            arena,
            &mut out[count..],
        )?;
        count += width;
        item_width[item_index] = width;
    }
    let mut column = 0;
    for (item_index, item) in items.iter().enumerate() {
        match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                column += definition.map_or(0, |table| table.n_columns);
            }
            SelectItem::RecordStar(_) => {
                column += item_width[item_index];
            }
            SelectItem::Expr { expression, alias } => {
                if let Some(description) = user_type_expression_description(
                    expression,
                    alias.unwrap_or(super::exec::derived_name(expression)),
                    storage,
                    txid,
                ) {
                    out[column] = description;
                }
                column += 1;
            }
        }
    }
    Ok(count)
}

/// Describes a SELECT using the catalog and statement arena available to the
/// protocol Describe path.
pub fn describe_select_items<'q>(
    items: &[SelectItem<'q>],
    scope: Option<&QueryScope<'q>>,
    storage: &'q Storage,
    txid: u32,
    arena: &'q Arena,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    let count = match scope {
        Some(scope) => describe_scope_items(items, scope, None, storage, txid, arena, out)?,
        None => describe_catalog_items(items, None, storage, txid, arena, out)?,
    };
    let mut column = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => column += scope.map_or(0, QueryScope::star_columns),
            SelectItem::TableWildcard(table) => {
                if let Some(scope) = scope
                    && let Ok(table) = scope.table_index(table)
                {
                    column += scope.defs.get(table).expect("resolved").n_columns;
                }
            }
            SelectItem::RecordStar(base) => {
                column += scope
                    .map(|scope| record_star_width(base, scope))
                    .transpose()?
                    .unwrap_or(0);
            }
            SelectItem::Expr { expression, .. } => {
                if let Some(description) =
                    subquery_result_type(expression, scope, storage, txid, arena)?
                {
                    out[column] =
                        ColDesc::new(out[column].name, description.type_oid, description.typlen)
                            .with_type_mod(description.type_mod);
                }
                column += 1;
            }
        }
    }
    patch_array_subquery_field_types(items, scope, storage, txid, arena, out)?;
    Ok(count)
}

/// Refines a field selected from an `ARRAY(subquery)` named-composite element.
/// Generic expression inference has no catalog for the subquery's inner FROM
/// scope; resolve that scope here, at the catalog-aware description boundary.
fn patch_array_subquery_field_types<'q>(
    items: &[SelectItem<'q>],
    outer_scope: Option<&QueryScope<'q>>,
    storage: &'q Storage,
    txid: u32,
    arena: &'q Arena,
    out: &mut [ColDesc<'q>],
) -> Result<(), SqlError> {
    let mut column = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => column += outer_scope.map_or(0, QueryScope::star_columns),
            SelectItem::TableWildcard(table) => {
                if let Some(scope) = outer_scope
                    && let Ok(table) = scope.table_index(table)
                {
                    column += scope.defs.get(table).expect("resolved").n_columns;
                }
            }
            SelectItem::RecordStar(base) => {
                column += outer_scope
                    .map(|scope| record_star_width(base, scope))
                    .transpose()?
                    .unwrap_or(0);
            }
            SelectItem::Expr { expression, .. } => {
                if let Some((ctype, type_mod)) =
                    array_subquery_field_type(expression, outer_scope, storage, txid, arena)?
                {
                    out[column] = ColDesc::of_type(out[column].name, ctype).with_type_mod(type_mod);
                }
                column += 1;
            }
        }
    }
    Ok(())
}

fn array_subquery_field_type<'q>(
    expression: &Expr<'q>,
    outer_scope: Option<&QueryScope<'q>>,
    storage: &'q Storage,
    txid: u32,
    arena: &'q Arena,
) -> Result<Option<(ColType, i32)>, SqlError> {
    let mark = arena.mark();
    let result = array_subquery_field_type_inner(expression, outer_scope, storage, txid, arena);
    // Type identities are owned values, not references to the temporary scope.
    unsafe {
        arena.rewind_to(mark);
    }
    result
}

fn array_subquery_field_type_inner<'q>(
    expression: &Expr<'q>,
    outer_scope: Option<&QueryScope<'q>>,
    storage: &'q Storage,
    txid: u32,
    arena: &'q Arena,
) -> Result<Option<(ColType, i32)>, SqlError> {
    let Expr::Field { base, field } = expression else {
        return Ok(None);
    };
    let Expr::Subscript { base: array, .. } = &**base else {
        return Ok(None);
    };
    let Expr::ArraySubquery(select) = &**array else {
        return Ok(None);
    };
    let mut element = [ColDesc::new("", 0, 0); 1];
    let count = match &select.from {
        Some(from) => {
            let scope = QueryScope::resolve_schema(
                storage,
                from,
                txid,
                arena,
                outer_scope
                    .map(ScopeSchema)
                    .as_ref()
                    .map(|schema| schema as &dyn ColumnLookup<'_>),
            )?;
            describe_scope_items(
                select.items,
                &scope,
                outer_scope,
                storage,
                txid,
                arena,
                &mut element,
            )?
        }
        None => describe_select_items(
            select.items,
            outer_scope,
            storage,
            txid,
            arena,
            &mut element,
        )?,
    };
    if count != 1 {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery must return only one column"
        ));
    }
    let oid = element[0].type_oid;
    let Some((crate::storage::NamedTypeKind::Composite, creation, false)) =
        crate::storage::NamedTypeKind::from_oid(oid)
    else {
        return Ok(None);
    };
    let target = crate::storage::TypeDefinitionTarget::by_creation(
        storage,
        crate::storage::NamedTypeKind::Composite,
        creation,
    )?;
    let composite = storage
        .read_composite_definition(target, txid)?
        .ok_or_else(|| {
            sql_err!(
                sqlstate::UNDEFINED_OBJECT,
                "composite type no longer exists"
            )
        })?;
    Ok(composite
        .active_fields()
        .find(|candidate| candidate.name.as_str().eq_ignore_ascii_case(field))
        .map(|candidate| (candidate.ctype, candidate.type_mod)))
}

#[derive(Clone, Copy)]
pub(crate) struct SubqueryResultType {
    pub(crate) type_oid: i32,
    pub(crate) typlen: i16,
    pub(crate) type_mod: i32,
}

pub(crate) fn subquery_result_type<'a>(
    expression: &Expr<'a>,
    outer_scope: Option<&QueryScope<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<SubqueryResultType>, SqlError> {
    let mark = arena.mark();
    let result = subquery_result_type_inner(expression, outer_scope, storage, txid, arena);
    unsafe {
        arena.rewind_to(mark);
    }
    result
}

fn subquery_result_type_inner<'a>(
    expression: &Expr<'a>,
    outer_scope: Option<&QueryScope<'a>>,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<Option<SubqueryResultType>, SqlError> {
    let (select, array) = match expression {
        Expr::Subquery(select) => (select, false),
        Expr::ArraySubquery(select) => (select, true),
        _ => return Ok(None),
    };
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let count = if let Some(body) = select.set_body {
        describe_set_tree(body, storage, txid, arena, &mut columns)?
    } else {
        match &select.from {
            Some(from) => {
                let scope = QueryScope::resolve_schema(
                    storage,
                    from,
                    txid,
                    arena,
                    outer_scope
                        .map(ScopeSchema)
                        .as_ref()
                        .map(|schema| schema as &dyn ColumnLookup<'_>),
                )?;
                describe_scope_items(
                    select.items,
                    &scope,
                    outer_scope,
                    storage,
                    txid,
                    arena,
                    &mut columns,
                )?
            }
            None => describe_select_items(
                select.items,
                outer_scope,
                storage,
                txid,
                arena,
                &mut columns,
            )?,
        }
    };
    if count != 1 {
        return Err(sql_err!(
            sqlstate::SYNTAX_ERROR,
            "subquery must return only one column"
        ));
    }
    let element = columns[0];
    let element_oid = element.type_oid;
    if !array {
        return Ok(Some(SubqueryResultType {
            type_oid: element.type_oid,
            typlen: element.typlen,
            type_mod: element.type_mod,
        }));
    }
    let array_oid = match super::exec::coltype_of_oid(element_oid) {
        Some(ColType::Array(_)) => Some(element_oid),
        Some(scalar) => {
            super::types::ArrElem::from_coltype(scalar).map(|element| element.array_oid())
        }
        None => crate::storage::NamedTypeKind::from_oid(element_oid).and_then(
            |(kind, creation, array)| {
                if array {
                    return None;
                }
                Some(match kind {
                    crate::storage::NamedTypeKind::Domain => {
                        super::types::oid::domain_array_oid(creation)
                    }
                    crate::storage::NamedTypeKind::Enum => {
                        super::types::oid::enum_array_oid(creation)
                    }
                    crate::storage::NamedTypeKind::Composite => {
                        super::types::oid::composite_array_oid(creation)
                    }
                })
            },
        ),
    };
    array_oid
        .ok_or_else(|| {
            sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "ARRAY subquery result type is not supported"
            )
        })
        .map(|type_oid| {
            Some(SubqueryResultType {
                type_oid,
                typlen: -1,
                type_mod: -1,
            })
        })
}

fn user_type_cast_description<'q>(
    expression: &Expr<'q>,
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    let Expr::Cast { type_name, .. } = expression else {
        return None;
    };
    let (base_name, array) = match type_name.strip_suffix("[]") {
        Some(base) => (base, true),
        None => (*type_name, false),
    };
    if let Some(slot) = storage.resolve_domain_slot(base_name, txid) {
        let domain = storage.domain(slot);
        return Some(if array {
            ColDesc::new(
                name,
                super::types::oid::domain_array_oid(storage.domain_for(slot, txid).creation()),
                -1,
            )
        } else {
            ColDesc::of_type(name, domain.base).with_type_mod(domain.base_type_mod)
        });
    }
    if let Some(slot) = storage.resolve_enum_slot(base_name, txid) {
        return Some(if array {
            ColDesc::new(
                name,
                super::types::oid::enum_array_oid(storage.enum_for(slot, txid).creation()),
                -1,
            )
        } else {
            ColDesc::new(
                name,
                super::types::oid::enum_oid(storage.enum_for(slot, txid).creation()),
                4,
            )
        });
    }
    storage.resolve_composite_slot(base_name, txid).map(|slot| {
        if array {
            ColDesc::new(
                name,
                super::types::oid::composite_array_oid(
                    storage.composite_for(slot, txid).creation(),
                ),
                -1,
            )
        } else {
            ColDesc::new(
                name,
                super::types::oid::composite_oid(storage.composite_for(slot, txid).creation()),
                -1,
            )
        }
    })
}

/// Catalog-backed expression description preserves user-type shape while
/// exposing a domain's base representation on the wire.
fn user_type_expression_description<'q>(
    expression: &Expr<'q>,
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    if let Some(description) = user_type_cast_description(expression, name, storage, txid) {
        return Some(description);
    }
    if let Expr::Call {
        name: routine_name,
        args,
        star: false,
        ..
    } = expression
    {
        let mut argument_type_oids =
            [super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        if args.len() <= argument_type_oids.len() {
            for (index, argument) in args.iter().enumerate() {
                argument_type_oids[index] = match argument {
                    Expr::Cast { type_name, .. } => {
                        crate::sql::catalog::user_type_oid(storage, txid, type_name)
                    }
                    _ => None,
                }
                .or_else(|| {
                    user_type_expression_description(argument, "", storage, txid)
                        .map(|description| description.type_oid)
                })
                .or_else(|| {
                    super::exec::infer_type_res(argument, &super::exec::NoCols)
                        .ok()
                        .map(|(oid, _)| oid)
                })?;
            }
            if let Some(routine) =
                storage.routine_for_call_oids(routine_name, &argument_type_oids[..args.len()], txid)
            {
                let result = routine.kind.function_result()?;
                return Some(ColDesc::new(name, result.oid(), result.typlen()));
            }
        }
    }
    match expression {
        Expr::Call {
            name: function,
            args,
            ..
        } if matches!(
            *function,
            "coalesce" | "greatest" | "least" | "min" | "max" | "nullif"
        ) =>
        {
            return matching_user_type_description(args, name, storage, txid);
        }
        Expr::Call {
            name: function,
            args,
            ..
        } if matches!(
            *function,
            "array_append" | "array_prepend" | "array_replace" | "array_remove" | "trim_array"
        ) =>
        {
            let (array, elements): (&Expr, &[&Expr]) = match *function {
                "array_append" => (args.first()?, &args[1..]),
                "array_prepend" => (args.get(1)?, &args[..1]),
                "array_replace" => (args.first()?, &args[2..]),
                "array_remove" => (args.first()?, &args[1..]),
                "trim_array" => (args.first()?, &[]),
                _ => unreachable!(),
            };
            return catalog_array_operation_description(array, elements, name, storage, txid);
        }
        Expr::Call {
            name: "array_cat",
            args,
            ..
        } => {
            let left = user_type_expression_description(args.first()?, name, storage, txid)?;
            let right = user_type_expression_description(args.get(1)?, name, storage, txid)?;
            return (left.type_oid == right.type_oid).then_some(left);
        }
        Expr::Call {
            name: "unnest",
            args,
            ..
        } => {
            let array = user_type_expression_description(args.first()?, name, storage, txid)?;
            return catalog_array_element_description(array, name, storage, txid);
        }
        Expr::Call {
            name: function,
            args,
            ..
        } if matches!(
            *function,
            "lag" | "lead" | "first_value" | "last_value" | "nth_value"
        ) =>
        {
            return user_type_expression_description(args.first()?, name, storage, txid);
        }
        Expr::Call {
            name: "array_agg",
            args,
            ..
        } => {
            return user_type_array_description(args, name, storage, txid);
        }
        Expr::Call {
            name: "array_fill",
            args,
            ..
        } => {
            return user_type_array_description(args.get(..1)?, name, storage, txid);
        }
        Expr::Case {
            whens, otherwise, ..
        } => {
            let mut result = None;
            for value in whens
                .iter()
                .map(|(_, value)| *value)
                .chain(otherwise.iter().copied())
            {
                if matches!(value, Expr::Null) {
                    continue;
                }
                let next = user_type_expression_description(value, name, storage, txid)?;
                if result.is_some_and(|existing: ColDesc| existing.type_oid != next.type_oid) {
                    return None;
                }
                result = Some(next);
            }
            return result;
        }
        Expr::Array(elements) => {
            return user_type_array_description(elements, name, storage, txid);
        }
        Expr::Subscript { base, .. } => {
            let array = user_type_expression_description(base, name, storage, txid)?;
            return catalog_array_element_description(array, name, storage, txid);
        }
        Expr::Slice { base, .. } => {
            let array = user_type_expression_description(base, name, storage, txid)?;
            let (ctype, _) = super::exec::catalog_column_type(storage, txid, array.type_oid)?;
            return matches!(ctype, super::types::ColType::Array(_)).then_some(array);
        }
        Expr::Field { base, field } => {
            if let Some(base) = user_type_expression_description(base, name, storage, txid)
                && let Some((ColType::Composite(Some(creation)), _)) =
                    super::exec::catalog_column_type(storage, txid, base.type_oid)
            {
                let target = crate::storage::TypeDefinitionTarget::by_creation(
                    storage,
                    crate::storage::NamedTypeKind::Composite,
                    creation,
                )
                .ok()?;
                let definition = storage.read_composite_definition(target, txid).ok()??;
                let field = definition
                    .active_fields()
                    .find(|candidate| candidate.name.as_str().eq_ignore_ascii_case(field))?;
                return Some(
                    ColDesc::of_type(name, field.ctype)
                        .with_type_mod(field.type_mod)
                        .with_collation(field.collation),
                );
            }
        }
        _ => {}
    }
    let Expr::Field { base, field } = expression else {
        return None;
    };
    let Expr::Call {
        name: row, args, ..
    } = &**base
    else {
        return None;
    };
    if !row.eq_ignore_ascii_case("row") {
        return None;
    }
    let position = super::exec::RECORD_FIELD_NAMES
        .iter()
        .position(|candidate| candidate.eq_ignore_ascii_case(field))?;
    user_type_cast_description(args.get(position)?, name, storage, txid)
}

fn catalog_array_element_description<'q>(
    array: ColDesc<'q>,
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    let (ctype, _) = super::exec::catalog_column_type(storage, txid, array.type_oid)?;
    match ctype {
        super::types::ColType::Array(super::types::ArrElem::Enum(Some(slot))) => {
            Some(ColDesc::new(name, super::types::oid::enum_oid(slot), 4))
        }
        super::types::ColType::Array(super::types::ArrElem::Composite(Some(slot))) => Some(
            ColDesc::new(name, super::types::oid::composite_oid(slot), -1),
        ),
        super::types::ColType::Array(super::types::ArrElem::Domain { identity: slot, .. }) => {
            let target = crate::storage::TypeDefinitionTarget::by_creation(
                storage,
                crate::storage::NamedTypeKind::Domain,
                slot?,
            )
            .ok()?;
            let domain = storage.read_domain_definition(target, txid).ok()??;
            Some(ColDesc::of_type(name, domain.base).with_type_mod(domain.base_type_mod))
        }
        _ => None,
    }
}

/// Describes array-polymorphic functions through their catalog-resolved array
/// argument. Static inference cannot recover user element OIDs from casts, so
/// the output array is valid only when each supplied element proves it belongs
/// to that resolved element type.
fn catalog_array_operation_description<'q>(
    array_expression: &'q Expr<'q>,
    element_expressions: &[&'q Expr<'q>],
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    let array = user_type_expression_description(array_expression, name, storage, txid)?;
    let (ctype, _) = super::exec::catalog_column_type(storage, txid, array.type_oid)?;
    let super::types::ColType::Array(element) = ctype else {
        return None;
    };
    for expression in element_expressions {
        if matches!(expression, Expr::Null) {
            continue;
        }
        let value = user_type_expression_description(expression, name, storage, txid)?;
        let matches = match element {
            super::types::ArrElem::Enum(Some(slot)) => {
                value.type_oid == super::types::oid::enum_oid(slot)
            }
            super::types::ArrElem::Domain { identity: slot, .. } => storage
                .domain_base_type_for_oid(super::types::oid::domain_oid(slot?), txid)
                .is_some_and(|base| value.type_oid == base.oid()),
            _ => false,
        };
        if !matches {
            return None;
        }
    }
    Some(array)
}

/// Derives an array descriptor from one catalog-resolved element type. Arrays
/// of an explicitly cast domain use PostgreSQL's domain-array OID; expressions
/// that have already coerced a domain to its base type deliberately use that
/// base array instead.
fn user_type_array_description<'q>(
    elements: &[&'q Expr<'q>],
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    let first = elements
        .iter()
        .find(|element| !matches!(element, Expr::Null))?;
    if let Expr::Cast { type_name, .. } = first {
        let domain_name = type_name.strip_suffix("[]").unwrap_or(type_name);
        if let Some(slot) = storage.resolve_domain_slot(domain_name, txid)
            && elements.iter().all(|element| match element {
                Expr::Null => false,
                Expr::Cast {
                    type_name: candidate,
                    ..
                } => candidate.eq_ignore_ascii_case(type_name),
                _ => false,
            })
        {
            return Some(ColDesc::new(
                name,
                super::types::oid::domain_array_oid(storage.domain_for(slot, txid).creation()),
                -1,
            ));
        }
    }
    let element = user_type_expression_description(first, name, storage, txid)?;
    for next in elements.iter().filter(|next| !matches!(next, Expr::Null)) {
        let next = user_type_expression_description(next, name, storage, txid)?;
        if next.type_oid != element.type_oid {
            return None;
        }
    }
    let (ctype, _) = super::exec::catalog_column_type(storage, txid, element.type_oid)?;
    let array_element = super::types::ArrElem::from_coltype(ctype)?;
    Some(ColDesc::new(
        name,
        super::types::ColType::Array(array_element).oid(),
        -1,
    ))
}

/// Resolves an expression family whose result type is the common type of its
/// arguments. A catalog cast is useful evidence only when every non-null
/// argument carries that same result type; otherwise normal PostgreSQL
/// coercion inference remains authoritative.
fn matching_user_type_description<'q>(
    arguments: &[&'q Expr<'q>],
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    let mut result = None;
    for argument in arguments {
        if matches!(argument, Expr::Null) {
            continue;
        }
        let description = user_type_expression_description(argument, name, storage, txid)?;
        if result.is_some_and(|existing: ColDesc| existing.type_oid != description.type_oid) {
            return None;
        }
        result = Some(description);
    }
    result
}

/// Emits one `ColDesc` per field of a `(record).*` expansion against a join
/// scope, resolving whole-row bases to their table's columns. Returns the new
/// column count.
fn describe_scope_record_star<'q>(
    base: &Expr<'q>,
    scope: &QueryScope<'q>,
    storage: &Storage,
    txid: u32,
    arena: &'q Arena,
    out: &mut [ColDesc<'q>],
    mut n: usize,
) -> Result<usize, SqlError> {
    let mut push = |desc: ColDesc<'q>, n: &mut usize| -> Result<(), SqlError> {
        if *n == out.len() {
            return Err(sql_err!(
                sqlstate::TOO_MANY_COLUMNS,
                "target lists can have at most 1664 entries"
            ));
        }
        out[*n] = desc;
        *n += 1;
        Ok(())
    };
    let resolver = CatalogScopeCols {
        scope,
        outer_scope: None,
        storage,
        txid,
        arena,
    };
    match base {
        Expr::Call { name, .. } if name.eq_ignore_ascii_case("row") => {
            super::exec::check_row_field_types(base, &resolver)?;
            let mut error = None;
            let mut index = 0usize;
            super::exec::record_shape(base, &resolver, |_, ctype| {
                if error.is_none() {
                    error = push(
                        ColDesc::of_type(super::exec::RECORD_FIELD_NAMES[index], ctype),
                        &mut n,
                    )
                    .err();
                }
                index += 1;
            })?
            .ok_or_else(|| super::exec::could_not_identify("*"))?;
            if let Some(error) = error {
                return Err(error);
            }
        }
        Expr::WholeRow(table)
        | Expr::Column {
            qualifier: None,
            name: table,
        } if scope.table_index(table).is_ok() => {
            let t = scope.table_index(table)?;
            for (column, c) in scope
                .defs
                .get(t)
                .expect("resolved")
                .columns()
                .iter()
                .enumerate()
            {
                push(
                    ColDesc::of_type(scope.defs.column_name(t, column), c.ctype)
                        .with_type_mod(c.type_mod)
                        .with_collation(c.collation),
                    &mut n,
                )?;
            }
        }
        Expr::Call { name, .. } if super::exec::builtin_record_srf_field_pub(name, 0).is_some() => {
            let mut index = 0;
            while let Some((field, ctype)) = super::exec::builtin_record_srf_field_pub(name, index)
            {
                push(ColDesc::of_type(field, ctype), &mut n)?;
                index += 1;
            }
        }
        Expr::Call { name, args, .. }
            if matches!(
                *name,
                "json_populate_record"
                    | "jsonb_populate_record"
                    | "json_populate_recordset"
                    | "jsonb_populate_recordset"
            ) =>
        {
            return describe_scope_record_star(
                args.first().copied().ok_or_else(|| {
                    sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "populate_record argument count"
                    )
                })?,
                scope,
                storage,
                txid,
                arena,
                out,
                n,
            );
        }
        Expr::Call {
            name,
            args,
            argument_names,
            variadic,
            ..
        } => {
            if args.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
                return Err(sql_err!(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "too many function arguments"
                ));
            }
            let mut argument_oids =
                [super::types::oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
            for (index, argument) in args.iter().enumerate() {
                argument_oids[index] =
                    super::exec::infer_routine_argument_oid(argument, &resolver)?;
            }
            let mut index = 0usize;
            while let Some((field_name, meta)) = super::exec::ColTypeResolver::routine_record_field(
                &resolver,
                name,
                argument_names,
                *variadic,
                &argument_oids[..args.len()],
                index,
            ) {
                let field_name = arena
                    .alloc_str(field_name.as_str())
                    .map_err(|_| arena_full())?;
                push(
                    ColDesc::of_type(field_name, meta.ctype)
                        .with_type_mod(meta.type_mod)
                        .with_collation(meta.collation),
                    &mut n,
                )?;
                index += 1;
            }
            if index == 0 {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            }
        }
        _ => {
            let slot = match base {
                Expr::Column { qualifier, name } => {
                    match super::exec::ColTypeResolver::resolve(&resolver, *qualifier, name)? {
                        ColType::Composite(slot) => slot,
                        _ => None,
                    }
                }
                Expr::Field { base, field } => {
                    match super::exec::record_field_type(base, field, &resolver)? {
                        ColType::Composite(slot) => slot,
                        _ => None,
                    }
                }
                Expr::Cast { type_name, .. } => storage
                    .resolve_composite_slot(type_name, txid)
                    .map(|slot| storage.composite_for(slot, txid).creation()),
                _ => None,
            };
            if let Some(slot) = slot {
                let target = crate::storage::TypeDefinitionTarget::by_creation(
                    storage,
                    crate::storage::NamedTypeKind::Composite,
                    slot,
                )?;
                let definition = storage
                    .read_composite_definition(target, txid)?
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_OBJECT,
                            "composite type no longer exists"
                        )
                    })?;
                for field in definition.active_fields() {
                    let name = arena
                        .alloc_str(field.name.as_str())
                        .map_err(|_| arena_full())?;
                    push(
                        ColDesc::of_type(name, field.ctype)
                            .with_type_mod(field.type_mod)
                            .with_collation(field.collation),
                        &mut n,
                    )?;
                }
                return Ok(n);
            }
            let Some(handle) = super::exec::expr_record_handle_pub(base, &resolver) else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            };
            let mut push_err = None;
            super::exec::visit_record_shape_metadata_pub(handle, |field_name, meta| {
                if push_err.is_some() {
                    return;
                }
                match arena.alloc_str(field_name) {
                    Ok(name) => {
                        if let Err(error) = push(
                            ColDesc::of_type(name, meta.ctype)
                                .with_type_mod(meta.type_mod)
                                .with_collation(meta.collation),
                            &mut n,
                        ) {
                            push_err = Some(error);
                        }
                    }
                    Err(_) => push_err = Some(arena_full()),
                }
            });
            if let Some(error) = push_err {
                return Err(error);
            }
            return Ok(n);
        }
    }
    Ok(n)
}

/// Resolves column types across all tables in a join scope.
pub(super) struct ScopeCols<'s, 'd>(pub(super) &'s QueryScope<'d>);
impl super::exec::ColTypeResolver for ScopeCols<'_, '_> {
    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        let entry = self.0.find_column(qualifier, name)?;
        Ok(self.0.output_type(entry))
    }

    fn column_meta(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<super::exec::StaticTypeMeta> {
        let entry = self.0.find_column(qualifier, name).ok()?;
        let ctype = self.0.output_type(entry);
        let type_mod = match entry {
            scope::ResolvedColumn::Table(table, column) => {
                self.0.defs.get(table)?.columns.get(column)?.type_mod
            }
            scope::ResolvedColumn::Merged(_) => -1,
        };
        Some(super::exec::StaticTypeMeta {
            ctype,
            type_oid: ctype.oid(),
            type_mod,
            collation: self.0.output_collation(entry),
        })
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.0.qualified_star_columns(name).is_ok()
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.0.func_scalar_type(name)
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        let t = self.0.table_index(name).ok()?;
        let def = self.0.defs.get(t)?;
        Some(&def.columns()[..def.n_columns])
    }

    fn whole_row_field(
        &self,
        name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, super::exec::StaticTypeMeta)> {
        let entry = self.0.qualified_star_entry(name, index).ok()?;
        let field_name = self.0.output_name(entry);
        Some((
            crate::util::StackStr::from_str(field_name),
            self.column_meta(Some(name), field_name)?,
        ))
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        let entry = self.0.find_column(qualifier, name).ok()?;
        if self.0.output_type(entry) != ColType::Record {
            return None;
        }
        match entry {
            scope::ResolvedColumn::Table(t, c) => Some(self.0.defs.get(t)?.columns[c].type_mod),
            _ => None,
        }
    }
}

pub(crate) struct CatalogScopeCols<'scope, 'definition, 'storage> {
    pub(crate) scope: &'scope QueryScope<'definition>,
    pub(crate) outer_scope: Option<&'scope QueryScope<'definition>>,
    pub(crate) storage: &'storage Storage,
    pub(crate) txid: u32,
    pub(crate) arena: &'storage Arena,
}

impl super::exec::ColTypeResolver for CatalogScopeCols<'_, '_, '_> {
    fn subquery_type(&self, expression: &Expr<'_>) -> Result<Option<(i32, i16)>, SqlError> {
        let schema = ScopeSchema(self.scope);
        let outer_schema = self.outer_scope.map(ScopeSchema);
        let outer = Chained {
            inner: &schema,
            outer: outer_schema
                .as_ref()
                .map(|schema| schema as &dyn ColumnLookup<'_>),
        };
        subquery_expression_type(expression, self.storage, self.txid, self.arena, &outer)
            .map(|oid| oid.map(|oid| (oid, ColType::from_oid(oid).map_or(-1, ColType::typlen))))
    }

    fn validate_explicit_cast(&self, source_oid: i32, target: ColType) -> Result<(), SqlError> {
        if self
            .storage
            .cast_for_oids(source_oid, target.oid(), self.txid)
            .is_none()
        {
            super::eval::validate_catalog_reference_cast(source_oid, target)?;
        }
        Ok(())
    }

    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Result<ColType, SqlError> {
        match self.scope.find_column(qualifier, name) {
            Ok(entry) => Ok(self.scope.output_type(entry)),
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                let outer_scope = self.outer_scope.ok_or(error)?;
                let entry = outer_scope.find_column(qualifier, name)?;
                Ok(outer_scope.output_type(entry))
            }
            Err(error) => Err(error),
        }
    }

    fn column_meta(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<super::exec::StaticTypeMeta> {
        let (scope, entry) = match self.scope.find_column(qualifier, name) {
            Ok(entry) => (self.scope, entry),
            Err(_) => {
                let outer = self.outer_scope?;
                (outer, outer.find_column(qualifier, name).ok()?)
            }
        };
        let ctype = scope.output_type(entry);
        let (type_oid, type_mod) = match entry {
            scope::ResolvedColumn::Table(table, column) => {
                let column = scope.defs.get(table)?.columns.get(column)?;
                (
                    self.storage
                        .routine_type_oid(column.ctype, column.user_type, self.txid)?,
                    column.type_mod,
                )
            }
            scope::ResolvedColumn::Merged(_) => (ctype.oid(), -1),
        };
        Some(super::exec::StaticTypeMeta {
            ctype,
            type_oid,
            type_mod,
            collation: scope.output_collation(entry),
        })
    }

    fn domain_base_type(&self, type_oid: i32) -> Option<ColType> {
        self.storage.domain_base_type_for_oid(type_oid, self.txid)
    }

    fn named_type_oid(&self, type_name: &str) -> Option<i32> {
        super::catalog::user_type_oid(self.storage, self.txid, type_name)
            .or_else(|| ColType::from_sql_name(type_name).map(ColType::oid))
    }

    fn operator_result(
        &self,
        name: &str,
        left_oid: i32,
        right_oid: i32,
    ) -> Option<super::exec::StaticTypeMeta> {
        let slot = self
            .storage
            .operator_slot_for_oids(None, name, Some(left_oid), Some(right_oid), self.txid)
            .ok()??;
        let result = self
            .storage
            .operator_for(slot, self.txid)
            .implementation
            .result()?;
        let ctype = result.ctype;
        Some(super::exec::StaticTypeMeta {
            type_oid: self
                .storage
                .routine_type_oid(ctype, result.user_type, self.txid)?,
            ctype,
            type_mod: -1,
            collation: if ctype.is_collatable() {
                super::ast::Collation::Default
            } else {
                super::ast::Collation::None
            },
        })
    }

    fn routine_result(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
    ) -> Option<super::exec::StaticTypeMeta> {
        if let Some((schema, operator)) = super::ast::catalog_operator_call(name) {
            if !(1..=2).contains(&arguments.len()) {
                return None;
            }
            if schema.is_none_or(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
                && let Some(ctype) =
                    super::eval::funcs::geometry::operator_result(operator, arguments)
            {
                return Some(super::exec::StaticTypeMeta {
                    type_oid: ctype.oid(),
                    ctype,
                    type_mod: -1,
                    collation: super::ast::Collation::None,
                });
            }
            if arguments.len() == 2
                && schema.is_some_and(|schema| schema.eq_ignore_ascii_case("pg_catalog"))
            {
                let ctype = builtin_operator_result(operator, arguments)?;
                return Some(super::exec::StaticTypeMeta {
                    type_oid: ctype.oid(),
                    ctype,
                    type_mod: -1,
                    collation: if ctype.is_collatable() {
                        super::ast::Collation::Default
                    } else {
                        super::ast::Collation::None
                    },
                });
            }
            let (left_oid, right_oid) = if arguments.len() == 1 {
                (None, Some(arguments[0]))
            } else {
                (Some(arguments[0]), Some(arguments[1]))
            };
            let slot = self
                .storage
                .operator_slot_for_oids(schema, operator, left_oid, right_oid, self.txid)
                .ok()??;
            let result = self
                .storage
                .operator_for(slot, self.txid)
                .implementation
                .result()?;
            let ctype = result.ctype;
            return Some(super::exec::StaticTypeMeta {
                type_oid: self.storage.routine_type_oid(
                    result.ctype,
                    result.user_type,
                    self.txid,
                )?,
                ctype,
                type_mod: -1,
                collation: if ctype.is_collatable() {
                    super::ast::Collation::Default
                } else {
                    super::ast::Collation::None
                },
            });
        }
        let routine = if argument_names.is_empty() {
            self.storage
                .function_for_call_syntax_oids(name, arguments, variadic, self.txid)
        } else {
            self.storage
                .function_for_named_call_oids(name, argument_names, arguments, self.txid)
        };
        let result = routine
            .and_then(|routine| match routine.kind {
                crate::storage::RoutineKind::Function { result }
                | crate::storage::RoutineKind::SetFunction { result } => Some(result),
                crate::storage::RoutineKind::RecordFunction { .. } => {
                    Some(crate::storage::RoutineResult::builtin(ColType::Record))
                }
                _ => None,
            })
            .or_else(|| {
                self.storage
                    .aggregate_for_call_oids(name, arguments, self.txid)
                    .map(|(_, _, aggregate)| aggregate.result_type)
            })?;
        let ctype = result.ctype;
        Some(super::exec::StaticTypeMeta {
            type_oid: self
                .storage
                .routine_type_oid(result.ctype, result.user_type, self.txid)?,
            ctype,
            type_mod: -1,
            collation: if ctype.is_collatable() {
                super::ast::Collation::Default
            } else {
                super::ast::Collation::None
            },
        })
    }

    fn routine_record_field(
        &self,
        name: &str,
        argument_names: &[Option<&str>],
        variadic: bool,
        arguments: &[i32],
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, super::exec::StaticTypeMeta)> {
        if let Some((field, type_oid, ctype)) =
            super::catalog::intrinsic_record_field(name, arguments, index)
        {
            return Some((
                crate::util::StackStr::from_str(field),
                super::exec::StaticTypeMeta {
                    type_oid,
                    ctype,
                    type_mod: -1,
                    collation: if ctype.is_collatable() {
                        super::ast::Collation::Default
                    } else {
                        super::ast::Collation::None
                    },
                },
            ));
        }
        let slot = if argument_names.is_empty() {
            if variadic {
                self.storage
                    .routine_slot_for_function_call_syntax_oids(name, arguments, true, self.txid)?
            } else {
                self.storage
                    .routine_slot_for_function_call_oids(name, arguments, self.txid)?
            }
        } else {
            self.storage.routine_slot_for_named_function_call_oids(
                name,
                argument_names,
                arguments,
                self.txid,
            )?
        };
        let routine = self.storage.routine_for(slot, self.txid);
        let column = routine.record_result_columns()?.get(index)?;
        Some((
            crate::util::StackStr::from_str(column.name.as_str()),
            super::exec::StaticTypeMeta {
                ctype: column.ctype,
                type_oid: self.storage.routine_type_oid(
                    column.ctype,
                    column.user_type,
                    self.txid,
                )?,
                type_mod: -1,
                collation: if column.ctype.is_collatable() {
                    super::ast::Collation::Default
                } else {
                    super::ast::Collation::None
                },
            },
        ))
    }

    fn composite_field(
        &self,
        slot: Option<crate::storage::TypeCreation>,
        field: &str,
    ) -> Option<super::exec::StaticTypeMeta> {
        super::exec::catalog_composite_field(self.storage, self.txid, slot, field)
    }

    fn visit_composite_fields(
        &self,
        slot: Option<crate::storage::TypeCreation>,
        visit: &mut dyn FnMut(&str, super::exec::StaticTypeMeta),
    ) -> Option<usize> {
        super::exec::visit_catalog_composite_fields(self.storage, self.txid, slot, visit)
    }

    fn visit_named_composite_fields(
        &self,
        type_name: &str,
        visit: &mut dyn FnMut(&str, crate::sql::exec::StaticTypeMeta),
    ) -> Result<Option<usize>, SqlError> {
        crate::sql::exec::visit_catalog_rowtype_fields(
            self.storage,
            self.txid,
            type_name,
            self.arena,
            visit,
        )
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.scope.qualified_star_columns(name).is_ok()
            || self
                .outer_scope
                .is_some_and(|scope| scope.qualified_star_columns(name).is_ok())
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.scope.func_scalar_type(name).or_else(|| {
            self.outer_scope
                .and_then(|scope| scope.func_scalar_type(name))
        })
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        if let Ok(table) = self.scope.table_index(name) {
            return Some(self.scope.defs.get(table)?.columns());
        }
        let scope = self.outer_scope?;
        let table = scope.table_index(name).ok()?;
        Some(scope.defs.get(table)?.columns())
    }

    fn whole_row_field(
        &self,
        name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, super::exec::StaticTypeMeta)> {
        let scope = if self.scope.qualified_star_columns(name).is_ok() {
            self.scope
        } else {
            let outer = self.outer_scope?;
            outer.qualified_star_columns(name).ok()?;
            outer
        };
        let entry = scope.qualified_star_entry(name, index).ok()?;
        let field_name = scope.output_name(entry);
        let ctype = scope.output_type(entry);
        let (type_oid, type_mod) = match entry {
            scope::ResolvedColumn::Table(table, column) => {
                let column = scope.defs.get(table)?.columns.get(column)?;
                (
                    self.storage
                        .routine_type_oid(column.ctype, column.user_type, self.txid)?,
                    column.type_mod,
                )
            }
            scope::ResolvedColumn::Merged(_) => (ctype.oid(), -1),
        };
        Some((
            crate::util::StackStr::from_str(field_name),
            super::exec::StaticTypeMeta {
                ctype,
                type_oid,
                type_mod,
                collation: scope.output_collation(entry),
            },
        ))
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        let (scope, entry) = match self.scope.find_column(qualifier, name) {
            Ok(entry) => (self.scope, entry),
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                let scope = self.outer_scope?;
                (scope, scope.find_column(qualifier, name).ok()?)
            }
            Err(_) => return None,
        };
        if scope.output_type(entry) != ColType::Record {
            return None;
        }
        match entry {
            scope::ResolvedColumn::Table(table, column) => {
                Some(scope.defs.get(table)?.columns[column].type_mod)
            }
            _ => None,
        }
    }
}

fn scope_column_type_mod<'a>(
    scope: &QueryScope<'a>,
    outer_scope: Option<&QueryScope<'a>>,
    qualifier: Option<&str>,
    name: &str,
) -> i32 {
    let found = match scope.find_column(qualifier, name) {
        Ok(entry) => Some((scope, entry)),
        Err(error)
            if matches!(
                error.sqlstate.as_str(),
                sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
            ) =>
        {
            outer_scope.and_then(|scope| {
                scope
                    .find_column(qualifier, name)
                    .ok()
                    .map(|entry| (scope, entry))
            })
        }
        Err(_) => None,
    };
    match found {
        Some((scope, ResolvedColumn::Table(table, column))) => {
            scope.defs.get(table).expect("resolved").columns[column].type_mod
        }
        _ => -1,
    }
}

/// The number of columns a `(base).*` record expansion contributes, or 0 when
/// its shape is not statically known (surfaced loudly at projection time).
pub(super) fn record_star_width(base: &Expr, scope: &QueryScope) -> Result<usize, SqlError> {
    Ok(super::exec::record_shape(base, &ScopeCols(scope), |_, _| {})?.unwrap_or(0))
}

fn infer_scope_type(expression: &Expr, scope: &QueryScope) -> Result<(i32, i16), SqlError> {
    let (oid, typlen) = super::exec::infer_type_res(expression, &ScopeCols(scope))?;
    if oid == super::types::oid::UNKNOWN {
        Ok((super::types::oid::TEXT, -1))
    } else {
        Ok((oid, typlen))
    }
}

/// Whether `target` occurs within `e` (pointer identity, expression-level
/// walk — nested subquery bodies evaluate their own subqueries).
fn expr_contains_node<'a>(e: &'a Expr<'a>, target: *const Expr<'a>) -> bool {
    if core::ptr::eq(e, target) {
        return true;
    }
    let mut found = false;
    let _ = walk_children(e, &mut |c| {
        if expr_contains_node(c, target) {
            found = true;
        }
        Ok(())
    });
    found
}

/// For `UPDATE ... FROM` / `DELETE ... USING`: enumerates the extra tables in
/// `from`, resolving the target row's columns through `target` (as the outer
/// scope), and invokes `on_match` with a combined lookup for the FIRST joined
/// row that satisfies `where_clause`. Returns whether any match was found.
#[allow(clippy::too_many_arguments)]
pub fn first_from_match<'a>(
    storage: &'a Storage,
    from: &'a FromClause<'a>,
    txid: u32,
    where_clause: Option<&'a Expr<'a>>,
    observed: &[&'a Expr<'a>],
    arena: &'a Arena,
    params: &[Datum<'a>],
    target: &dyn ColumnLookup<'a>,
    on_match: &mut dyn FnMut(&dyn ColumnLookup<'a>) -> Result<(), SqlError>,
    sequences: Option<&dyn SequenceAccess>,
) -> Result<bool, SqlError> {
    let scope = QueryScope::resolve_exec_outer(
        storage,
        from,
        where_clause,
        txid,
        arena,
        params,
        sequences,
        Some(target),
    )?;
    let subquery_expressions = arena
        .alloc_slice_with(1 + 2 * scope.n, |_| None)
        .map_err(|_| arena_full())?;
    subquery_expressions[0] = where_clause;
    collect_table_sample_expressions(from, &mut subquery_expressions[1..]);
    let subs = subquery_hooks(
        subquery_expressions,
        storage,
        txid,
        arena,
        params,
        sequences,
    )?;
    let catalog = StorageCatalog {
        storage,
        routine_workspace: arena,
        txid,
        invocations: None,
        statement_arena: None,
    };
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&subs),
        subquery_executor: None,
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        project_sets: None,
        sequences,
        merge_action: None,
    };
    let mut found = false;
    let mut expressions = [&Expr::Null; MAX_PROJ + 1];
    let mut expression_count = 0usize;
    if let Some(predicate) = where_clause {
        expressions[expression_count] = predicate;
        expression_count += 1;
    }
    for &expression in observed {
        expressions[expression_count] = expression;
        expression_count += 1;
    }
    let pax_columns = pax_column_demand(&scope, from, &expressions[..expression_count]);
    let retain_match = core::cell::Cell::new(false);
    scan_source_recycling_retaining_match_with_pax_columns(
        storage,
        &scope,
        from,
        txid,
        where_clause,
        arena,
        params,
        &hooks,
        Some(target),
        pax_columns,
        &retain_match,
        &mut |jr| {
            let chained_row = Chained {
                inner: jr,
                outer: Some(target),
            };
            on_match(&chained_row)?;
            retain_match.set(true);
            found = true;
            Ok(false) // stop at the first match (PostgreSQL uses one arbitrary row)
        },
    )?;
    Ok(found)
}

fn arena_full() -> SqlError {
    sql_err!(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        "query result exceeds the statement arena"
    )
}

/// Public view-DML rewriting uses this for arena-exhaustion.
pub fn arena_full_pub() -> SqlError {
    arena_full()
}
