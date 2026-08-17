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

use crate::mem::arena::Arena;
use crate::pg::respond::Responder;
use crate::pg::wire::WireFull;
use crate::sql_err;
use crate::stack_format;
use crate::storage::{ColumnMeta, Storage, TableDef};

use super::ast::{
    Expr, FrameBound, FromClause, OrderBy, QualName, Select, SelectItem, SetQuery, Stmt, TableRef,
    WindowFrame,
};
use super::eval::{
    ColumnLookup, EvalHooks, SequenceAccess, SqlError, SubqueryValues, eval_full,
    resolved_expression_collation, sqlstate,
};
use super::exec::{MAX_PROJ, describe_items};
use super::types::{ColDesc, ColType, Datum};

mod setops;
use setops::materialize_set_body;
pub use setops::set_query;

mod materialize;
use materialize::{
    ScopeSchema, external_materialized_into, finalize_projected_row, materialized_rows,
    materialized_select,
};

mod scan;
pub use scan::JoinRow;
pub(crate) use scan::select_hash_join_plan;
use scan::{
    Chained, PaxReadDemand, pax_column_demand, scan_source_recycling_with_pax_columns,
    scan_source_with_pax_columns,
};

mod scope;
pub(crate) use cte::{bind_dml_materialized_relations, bind_materialized_relations};
pub use scope::{MAX_MERGED_COLUMNS, MergedColumn, QueryScope, ResolvedColumn};

mod cte;
mod dependencies;
use cte::expand_set_tree_exec;
pub use cte::{
    describe_set_query, expand_ctes, expand_ctes_exec, expand_ctes_under, expand_dml_ctes,
    rewrite_view_dml,
};
pub(crate) use cte::{expand_set_tree, expand_stored_query};

pub fn stored_query_dependencies(
    sql: &str,
    storage: &Storage,
    txid: u32,
    path: crate::storage::PathContext,
    arena: &Arena,
) -> Result<crate::storage::StoredQueryDependencies, SqlError> {
    dependencies::collect(sql, storage, txid, path, arena)
}

mod aggregate;
use aggregate::{AggState, fold_aggregates};

mod srf;
use srf::{find_srf, srf_count, srf_max_count, table_func_def, table_func_def_outer};
pub(crate) use srf::{synth_derived_def, synth_derived_def_outer, table_func_rows_outer};

mod group;
use group::{grouped_rows, grouped_select};

mod plan;
pub(crate) use plan::join_order;
use plan::{postpone_cost, reorder_qual, simplify_qual, where_passes};

mod subquery;
pub(crate) use subquery::walk_children;
use subquery::{
    correlated_in_expression, correlated_scan_conjuncts, correlated_where_passes, merge_correlated,
    prepare_outer_subqueries, subquery_witness,
};
pub use subquery::{prepare_subqueries, subquery_hooks};

mod window;
use window::{
    cmp_key_rows, dedup_window_rows, external_window_into, project_window_rows,
    rewrite_grouped_windows, window_select,
};

/// Static executor envelope for one range table.
///
/// This matches the largest catalog the server's conformance configuration
/// can expose and keeps every range-table array allocation-free.  Runtime
/// work is proportional to `QueryScope::n`; the envelope only reserves
/// scratch, so a query can use every configured relation slot without a
/// second, smaller executor limit.
pub const MAX_JOIN_TABLES: usize = 64;
const MAX_AGGS: usize = 16;
pub(crate) const MAX_ROUTINE_INVOCATIONS: usize = 1024;

use core::cell::Cell;
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
    results: [Cell<RoutineInvocationResult<'a>>; MAX_ROUTINE_INVOCATIONS],
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

impl<'a> RoutineInvocationState<'a> {
    pub(crate) fn new() -> Self {
        Self {
            next: Cell::new(0),
            completed: Cell::new(0),
            pending: Cell::new(None),
            results: [const { Cell::new(RoutineInvocationResult::Scalar(Datum::Null)) };
                MAX_ROUTINE_INVOCATIONS],
        }
    }

    pub(crate) fn begin_attempt(&self) {
        self.next.set(0);
        self.pending.set(None);
    }

    pub(crate) fn rewind_cursor(&self) {
        self.next.set(0);
    }

    pub(crate) fn resolve<'query>(
        &self,
        slot: usize,
        arguments: &[Datum],
        statement_arena: &'a Arena,
        query_arena: &'query Arena,
    ) -> Result<Option<Datum<'query>>, SqlError> {
        let ordinal = self.next.get();
        if ordinal == MAX_ROUTINE_INVOCATIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL statement exceeds {} mutable routine invocations",
                MAX_ROUTINE_INVOCATIONS
            ));
        }
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            let RoutineInvocationResult::Scalar(value) = self.results[ordinal].get() else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SQL routine invocation changed from a table source to a scalar expression"
                ));
            };
            let encoded = super::exec::encode_projected_pub(&[value], query_arena)?;
            return Ok(Some(super::exec::decode_projected_pub(encoded, 0)));
        }
        self.pending.set(Some(PendingRoutineInvocation {
            slot,
            arguments: super::exec::encode_projected_pub(arguments, statement_arena)?,
            argument_count: arguments.len(),
        }));
        Err(sql_err!(
            sqlstate::INTERNAL_ROUTINE_INVOCATION,
            "mutable SQL routine invocation is pending"
        ))
    }

    pub(crate) fn take_pending(&self) -> Option<PendingRoutineInvocation<'a>> {
        self.pending.replace(None)
    }

    pub(crate) fn complete(&self, result: Datum<'a>) -> Result<(), SqlError> {
        let ordinal = self.completed.get();
        if ordinal == MAX_ROUTINE_INVOCATIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL statement exceeds {} mutable routine invocations",
                MAX_ROUTINE_INVOCATIONS
            ));
        }
        self.results[ordinal].set(RoutineInvocationResult::Scalar(result));
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
        if ordinal == MAX_ROUTINE_INVOCATIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL statement exceeds {} mutable routine invocations",
                MAX_ROUTINE_INVOCATIONS
            ));
        }
        self.next.set(ordinal + 1);
        if ordinal < self.completed.get() {
            let RoutineInvocationResult::Rows(rows) = self.results[ordinal].get() else {
                return Err(sql_err!(
                    sqlstate::INTERNAL_ERROR,
                    "SQL routine invocation changed from a scalar expression to a table source"
                ));
            };
            return Ok(Some(rows));
        }
        self.pending.set(Some(PendingRoutineInvocation {
            slot,
            arguments: super::exec::encode_projected_pub(arguments, statement_arena)?,
            argument_count: arguments.len(),
        }));
        Err(sql_err!(
            sqlstate::INTERNAL_ROUTINE_INVOCATION,
            "mutable SQL routine invocation is pending"
        ))
    }

    pub(crate) fn complete_rows(&self, rows: &'a [&'a [u8]]) -> Result<(), SqlError> {
        let ordinal = self.completed.get();
        if ordinal == MAX_ROUTINE_INVOCATIONS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL statement exceeds {} mutable routine invocations",
                MAX_ROUTINE_INVOCATIONS
            ));
        }
        self.results[ordinal].set(RoutineInvocationResult::Rows(rows));
        self.completed.set(ordinal + 1);
        Ok(())
    }
}
pub(super) const MAX_WINDOWS: usize = 16;
/// Maximum ORDER BY / PARTITION BY keys in one window clause.
const MAX_WIN_KEYS: usize = 8;
const MAX_SUBQUERIES: usize = 8;
const SUBQUERY_DEPTH: u32 = 4;

type Outcome = Result<Result<(), SqlError>, WireFull>;

/// A SQL-language function body can contain either ordinary or set-operation
/// queries; no DML statement can cross this boundary.
#[derive(Clone, Copy)]
pub(crate) enum RoutineQuery<'a> {
    Select(Select<'a>),
    Set(SetQuery<'a>),
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
) -> Result<RoutineFunctionProgram<'a>, SqlError> {
    const MAX_ROUTINE_STATEMENTS: usize = 64;
    let mut parser = super::parser::Parser::new(body, arena)
        .map_err(|error| super::parse_error_to_sql(&error))?;
    let mut parsed = [None; MAX_ROUTINE_STATEMENTS];
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
        if count == parsed.len() {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "SQL function body exceeds {} statements",
                parsed.len()
            ));
        }
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
                    .alloc(RoutineQuery::Select(*query))
                    .map_err(|_| arena_full())?,
            ),
            RoutinePrelude::Statement(Stmt::SetQuery(query)) => RoutineFunctionResult::Query(
                arena
                    .alloc(RoutineQuery::Set(*query))
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

pub(crate) fn routine_statement_is_query(statement: &Stmt<'_>) -> bool {
    matches!(statement, Stmt::Select(_) | Stmt::SetQuery(_))
}

/// Whether a scalar routine needs the engine-owned executor rather than the
/// read-only evaluator bridge. This is a property of the parsed program, not
/// a spelling heuristic at an individual call site.
pub(crate) fn routine_program_requires_mutable_execution(
    program: &RoutineFunctionProgram<'_>,
) -> bool {
    program.preceding.iter().any(|step| match step {
        RoutinePrelude::Statement(statement) => !routine_statement_is_query(statement),
        RoutinePrelude::Forbidden(_) => true,
    }) || matches!(
        program.result,
        RoutineFunctionResult::DataModification(_) | RoutineFunctionResult::Forbidden(_)
    ) || matches!(
        program.result,
        RoutineFunctionResult::Void(statement) if !routine_statement_is_query(statement)
    )
}

pub(crate) fn routine_forbidden_statement_error(statement: &str) -> SqlError {
    sql_err!(
        sqlstate::ACTIVE_SQL_TRANSACTION,
        "{} cannot be executed from an SQL function",
        statement
    )
}

fn statement_returns_rows(statement: &Stmt<'_>) -> bool {
    match statement {
        Stmt::Insert(insert) => !insert.returning.is_empty(),
        Stmt::Update(update) => !update.returning.is_empty(),
        Stmt::Delete(delete) => !delete.returning.is_empty(),
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
    match query {
        RoutineQuery::Select(select) => {
            let select = expand_ctes_exec(select, storage, txid, arena, params, &[])?;
            validate_locking(select)?;
            if recycling {
                select_into_rows_recycling(storage, txid, select, arena, params, None, None, emit)
            } else {
                select_into_rows(storage, txid, select, arena, params, None, None, emit)
            }
        }
        RoutineQuery::Set(query) => {
            setops::set_query_into_rows(storage, txid, query, arena, params, None, emit)
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

pub(super) fn storage_catalog<'a>(
    storage: &'a Storage,
    routine_workspace: &'a Arena,
    txid: u32,
) -> StorageCatalog<'a, 'a, 'static, 'static> {
    StorageCatalog {
        storage,
        routine_workspace,
        txid,
        invocations: None,
        statement_arena: None,
    }
}

impl super::eval::CatalogAccess for StorageCatalog<'_, '_, '_, '_> {
    fn materialize_composite<'a>(
        &self,
        slot: u16,
        text: &'a str,
        arena: &'a Arena,
    ) -> Result<Datum<'a>, SqlError> {
        super::exec::decode_composite_text(text, slot, self.storage, self.txid, arena)
    }

    fn compare_text(
        &self,
        collation: super::ast::Collation,
        left: &str,
        right: &str,
    ) -> Result<core::cmp::Ordering, SqlError> {
        self.storage.compare_text(collation, left, right)
    }

    fn call_routine<'a>(
        &self,
        name: &str,
        arguments: &[Datum<'a>],
        arena: &'a Arena,
    ) -> Result<Option<Datum<'a>>, SqlError> {
        let Some(slot) = self
            .storage
            .routine_slot_for_call(name, arguments, self.txid)
        else {
            return Ok(None);
        };
        self.storage.require_routine_execute(slot, self.txid)?;
        let routine = self.storage.routine(slot);
        let result_type = routine
            .kind
            .function_result()
            .expect("routine call resolution returns functions only");
        let _formal_scope = super::exec::enter_routine_parameter_types(routine.arguments());
        let function_program = parse_routine_function_program(
            routine.body.as_str(),
            self.routine_workspace,
            result_type == ColType::Void,
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
        let mut parameters = [Datum::Null; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (slot, argument) in arguments.iter().enumerate() {
            let encoded =
                crate::sql::exec::encode_projected_pub(&[*argument], self.routine_workspace)?;
            parameters[slot] = crate::sql::exec::decode_projected_pub(encoded, 0);
        }
        for step in function_program.preceding {
            let RoutinePrelude::Statement(statement) = step else {
                let RoutinePrelude::Forbidden(statement) = step else {
                    unreachable!("routine prelude has two variants");
                };
                return Err(routine_forbidden_statement_error(statement));
            };
            let query = match statement {
                Stmt::Select(query) => RoutineQuery::Select(*query),
                Stmt::SetQuery(query) => RoutineQuery::Set(*query),
                _ => unreachable!("mutable routine prelude was rejected above"),
            };
            execute_routine_query(
                &query,
                self.storage,
                self.txid,
                self.routine_workspace,
                &parameters[..arguments.len()],
                true,
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
                    Stmt::Select(query) => RoutineQuery::Select(*query),
                    Stmt::SetQuery(query) => RoutineQuery::Set(*query),
                    _ => {
                        return Err(sql_err!(
                            sqlstate::FEATURE_NOT_SUPPORTED,
                            "data-modifying SQL functions require a resumable query executor"
                        ));
                    }
                };
                execute_routine_query(
                    &query,
                    self.storage,
                    self.txid,
                    self.routine_workspace,
                    &parameters[..arguments.len()],
                    true,
                    &mut |_| Ok(()),
                )?;
                return Ok(Some(Datum::Null));
            }
            RoutineFunctionResult::Forbidden(statement) => {
                return Err(routine_forbidden_statement_error(statement));
            }
        };
        execute_routine_query(
            result_query,
            self.storage,
            self.txid,
            self.routine_workspace,
            &parameters[..arguments.len()],
            true,
            &mut |values| {
                if values.len() != 1 {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "SQL function query must return one column"
                    ));
                }
                if result.is_some() {
                    return Ok(());
                }
                let encoded = crate::sql::exec::encode_projected_pub(values, arena)?;
                result = Some(crate::sql::exec::decode_projected_pub(encoded, 0));
                Ok(())
            },
        )?;
        super::eval::cast_to(result.unwrap_or(Datum::Null), result_type, arena).map(Some)
    }

    fn rewind_routine_invocation_cursor(&self) {
        if let Some(invocations) = self.invocations {
            invocations.rewind_cursor();
        } else if let Some((invocations, _)) = active_routine_invocations() {
            invocations.rewind_cursor();
        }
    }

    fn relation_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::relation_oid_is_visible(self.storage, self.txid, oid).then_some(true)
    }

    fn type_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::type_oid_is_visible(self.storage, self.txid, oid).then_some(true)
    }

    fn function_is_visible(&self, oid: i32) -> Option<bool> {
        (super::catalog::function_oid_is_visible(oid)
            || self.storage.routine_slot_by_oid(oid, self.txid).is_some())
        .then_some(true)
    }

    fn function_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::function_def_text(self.storage, self.txid, oid, arena)
    }

    fn collation_is_visible(&self, oid: i32) -> Option<bool> {
        super::catalog::collation_oid_is_visible(oid).then_some(true)
    }

    fn relation_is_publishable(&self, oid: i32) -> Option<bool> {
        super::catalog::relation_oid_is_visible(self.storage, self.txid, oid).then_some(
            super::catalog::relation_oid_is_publishable(self.storage, self.txid, oid),
        )
    }

    fn index_def<'a>(
        &self,
        oid: i32,
        col: usize,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::index_def_text(self.storage, self.txid, oid, col, arena)
    }
    fn constraint_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::constraint_def_text(self.storage, self.txid, oid, arena)
    }
    fn relname<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::relname_text(self.storage, self.txid, oid, arena)
    }

    fn reloid(&self, name: &str) -> Option<i32> {
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
            .map(Storage::role_oid)
            .or_else(|| super::catalog::predefined_role_oid(name))
    }

    fn schema_name<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        let Some(name) = super::catalog::schema_name_by_oid(self.storage, self.txid, oid) else {
            return Ok(None);
        };
        Ok(Some(arena.alloc_str(name).map_err(|_| arena_full())?))
    }

    fn schema_oid(&self, name: &str) -> Option<i32> {
        super::catalog::schema_oid_by_name(self.storage, self.txid, name)
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
        super::catalog::operator_name_by_oid(oid, signature, arena)
    }

    fn operator_oid(&self, name: &str, signature: bool) -> Result<Option<i32>, SqlError> {
        super::catalog::operator_oid_by_name(name, signature)
    }

    fn has_table_privilege(
        &self,
        role: Option<&str>,
        relation: &str,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let (schema, name) = split_catalog_name(relation);
        let object = match self.storage.resolve_relation(schema, name, self.txid) {
            Some(crate::storage::ResolvedRelation::Table(slot)) => {
                self.storage.table_access_object(slot, self.txid)
            }
            Some(crate::storage::ResolvedRelation::View(slot)) => crate::storage::AccessObject {
                class: crate::storage::AccessClass::View,
                slot: slot as u16,
            },
            Some(crate::storage::ResolvedRelation::Catalog) => {
                return privilege_query(
                    privileges,
                    crate::storage::PrivilegeSet::SELECT,
                    crate::storage::PrivilegeSet::TABLE_ALL,
                    |_| false,
                    |_| false,
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

    fn has_sequence_privilege(
        &self,
        role: Option<&str>,
        sequence: &str,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let (schema, name) = split_catalog_name(sequence);
        let Some(slot) = self.storage.sequence_on_path(schema, name, self.txid) else {
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
        schema: &str,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let Some(slot) = self.storage.find_schema_visible(schema, self.txid) else {
            return Ok(None);
        };
        let object = crate::storage::AccessObject {
            class: crate::storage::AccessClass::Schema,
            slot: slot as u16,
        };
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            crate::storage::PrivilegeSet::SCHEMA_ALL,
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

    fn has_type_privilege(
        &self,
        role: Option<&str>,
        type_name: &str,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let (written_schema, name) = split_catalog_name(type_name);
        let domain = match written_schema {
            Some(schema) => self.storage.domain_slot(schema, name, self.txid),
            None => self.storage.resolve_domain_slot(name, self.txid),
        };
        let enumeration = match written_schema {
            Some(schema) => self.storage.enum_slot(schema, name, self.txid),
            None => self.storage.resolve_enum_slot(name, self.txid),
        };
        let object = domain
            .map(|slot| crate::storage::AccessObject {
                class: crate::storage::AccessClass::Domain,
                slot: slot as u16,
            })
            .or_else(|| {
                enumeration.map(|slot| crate::storage::AccessObject {
                    class: crate::storage::AccessClass::Enum,
                    slot: slot as u16,
                })
            });
        let Some(object) = object else {
            return Ok(None);
        };
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            crate::storage::PrivilegeSet::TYPE_ALL,
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

    fn has_function_privilege(
        &self,
        role: Option<&str>,
        function: &str,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let function = function.trim();
        let (written_name, written_arguments) = match function.strip_suffix(')') {
            Some(prefix) => prefix.rsplit_once('(').unwrap_or((function, "")),
            None => (function, ""),
        };
        let (schema, name) = split_catalog_name(written_name.trim());
        let mut argument_types = [ColType::Text; crate::storage::MAX_ROUTINE_ARGUMENTS];
        let written_arguments = written_arguments.trim();
        let argument_count = if written_arguments.is_empty() {
            0
        } else {
            let mut count = 0usize;
            for type_name in written_arguments.split(',') {
                if count == argument_types.len() {
                    return Ok(None);
                }
                let Some(ctype) = ColType::from_sql_name(type_name.trim()) else {
                    return Ok(None);
                };
                argument_types[count] = ctype;
                count += 1;
            }
            count
        };
        let Some(slot) = self.storage.routine_slot_by_signature(
            schema.unwrap_or("public"),
            name,
            &argument_types[..argument_count],
            self.txid,
        ) else {
            return Ok(None);
        };
        let object = crate::storage::Storage::routine_access_object(slot);
        privilege_query(
            privileges,
            crate::storage::PrivilegeSet::NONE,
            crate::storage::PrivilegeSet::FUNCTION_ALL,
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

    fn has_database_privilege(
        &self,
        role: Option<&str>,
        privileges: &str,
    ) -> Result<Option<bool>, SqlError> {
        let role = match privilege_role(self.storage, role, self.txid) {
            Some(role) => role,
            None => return Ok(None),
        };
        let attributes = self.storage.role(role).attributes_to(self.txid);
        let mut result = true;
        for written in privileges.split(',') {
            let privilege = written.trim();
            let allowed = if privilege.eq_ignore_ascii_case("connect")
                || privilege.eq_ignore_ascii_case("temporary")
                || privilege.eq_ignore_ascii_case("temp")
            {
                true
            } else if privilege.eq_ignore_ascii_case("create") {
                attributes.superuser || attributes.create_database
            } else {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized privilege type: \"{}\"",
                    privilege
                ));
            };
            result &= allowed;
        }
        Ok(Some(result))
    }

    fn comment<'a>(
        &self,
        catalog_name: &str,
        oid: i32,
        subid: i32,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        super::catalog::comment_text_for(self.storage, self.txid, catalog_name, oid, subid, arena)
    }

    fn type_name<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::user_type_name_text(self.storage, self.txid, oid, arena)
    }

    fn view_def<'a>(&self, oid: i32, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        super::catalog::view_def_text(self.storage, self.txid, oid, arena)
    }

    fn relation_size(&self, oid: i32) -> Result<Option<i64>, SqlError> {
        super::catalog::relation_size(self.storage, self.txid, oid)
    }

    fn database_size(&self) -> Result<i64, SqlError> {
        super::catalog::database_size(self.storage, self.txid)
    }

    fn enum_name<'a>(&self, slot: u16, arena: &'a Arena) -> Result<Option<&'a str>, SqlError> {
        let def = self.storage.enum_for(slot as usize, self.txid);
        if !def.visible_to(self.txid) {
            return Ok(None);
        }
        Ok(Some(
            arena
                .alloc_str(def.name.as_str())
                .map_err(|_| arena_full())?,
        ))
    }

    fn enum_label_sort(&self, slot: u16, label: &str) -> Option<f64> {
        let def = self.storage.enum_for(slot as usize, self.txid);
        if !def.visible_to(self.txid) {
            return None;
        }
        def.sort_of(label)
    }

    fn enum_slot_of_name(&self, type_name: &str) -> Option<u16> {
        self.storage
            .resolve_enum_slot(type_name, self.txid)
            .map(|s| s as u16)
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
                    super::types::ArrElem::Enum(slot as u16),
                    self.storage,
                    self.txid,
                    arena,
                )
                .map(Some);
            }
            if let Some(slot) = self.storage.resolve_domain_slot(element_name, self.txid) {
                let domain = self.storage.domain(slot);
                let Some(element) = super::types::ArrElem::domain(slot as u16, domain.base) else {
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
            return Ok(None);
        }
        if let Some(slot) = self.storage.resolve_domain_slot(type_name, self.txid) {
            return super::exec::coerce_domain_value(
                self.storage,
                slot,
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
                    slot as u16,
                    self.storage,
                    self.txid,
                    arena,
                ),
                value => super::exec::coerce_composite_value(
                    value,
                    slot as u16,
                    self.storage,
                    self.txid,
                    arena,
                ),
            }
            .map(Some);
        }
        let Some(slot) = self.storage.resolve_enum_slot(type_name, self.txid) else {
            return Ok(None);
        };
        super::exec::coerce_enum_value(value, slot as u16, self.storage, self.txid, arena).map(Some)
    }

    fn array_domain_element(&self, type_name: &str) -> Option<super::types::ArrElem> {
        let slot = self.storage.resolve_domain_slot(type_name, self.txid)?;
        let domain = self.storage.domain(slot);
        matches!(domain.base, super::types::ColType::Array(_))
            .then(|| super::types::ArrElem::domain(slot as u16, domain.base))?
    }

    fn user_array_name<'a>(
        &self,
        element: super::types::ArrElem,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        let name = match element {
            super::types::ArrElem::Enum(slot) => {
                let definition = self.storage.enum_for(slot as usize, self.txid);
                definition.visible_to(self.txid).then_some(definition.name)
            }
            super::types::ArrElem::Domain { slot, .. } => {
                let def = self.storage.domain(slot as usize);
                def.visible_to(self.txid).then_some(def.name)
            }
            super::types::ArrElem::Composite(slot) => {
                let def = self.storage.composite(slot as usize);
                def.visible_to(self.txid).then_some(def.name)
            }
            _ => None,
        };
        let Some(name) = name else { return Ok(None) };
        let rendered = crate::stack_format!(128, "{}[]", name.as_str());
        Ok(Some(
            arena
                .alloc_str(rendered.as_str())
                .map_err(|_| arena_full())?,
        ))
    }

    fn user_type_name<'a>(
        &self,
        type_name: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a str>, SqlError> {
        let (base, array) = match type_name.strip_suffix("[]") {
            Some(base) => (base, true),
            None => (type_name, false),
        };
        let visible = self.storage.resolve_domain_slot(base, self.txid).is_some()
            || self.storage.resolve_enum_slot(base, self.txid).is_some()
            || self
                .storage
                .resolve_composite_slot(base, self.txid)
                .is_some();
        if !visible {
            return Ok(None);
        }
        let rendered = if array {
            crate::stack_format!(128, "{}[]", base)
        } else {
            crate::stack_format!(128, "{}", base)
        };
        Ok(Some(
            arena
                .alloc_str(rendered.as_str())
                .map_err(|_| arena_full())?,
        ))
    }

    fn user_type_oid(&self, type_name: &str) -> Option<i32> {
        let (base, array) = match type_name.strip_suffix("[]") {
            Some(base) => (base, true),
            None => (type_name, false),
        };
        if let Some(slot) = self.storage.resolve_domain_slot(base, self.txid) {
            return Some(if array {
                super::types::oid::domain_array_oid(slot as u16)
            } else {
                super::types::oid::domain_oid(slot as u16)
            });
        }
        if let Some(slot) = self.storage.resolve_enum_slot(base, self.txid) {
            return Some(if array {
                super::types::oid::enum_array_oid(slot as u16)
            } else {
                super::types::oid::enum_oid(slot as u16)
            });
        }
        self.storage
            .resolve_composite_slot(base, self.txid)
            .map(|slot| {
                if array {
                    super::types::oid::composite_array_oid(slot as u16)
                } else {
                    super::types::oid::composite_oid(slot as u16)
                }
            })
    }
}

fn split_catalog_name(name: &str) -> (Option<&str>, &str) {
    name.rsplit_once('.')
        .map_or((None, name), |(schema, object)| (Some(schema), object))
}

fn privilege_role(storage: &Storage, role: Option<&str>, txid: u32) -> Option<usize> {
    match role {
        Some(role) => storage.find_role_visible(role, txid),
        None => storage.current_role_slot(txid),
    }
}

fn privilege_query(
    written: &str,
    catalog_default: crate::storage::PrivilegeSet,
    all: crate::storage::PrivilegeSet,
    has_privilege: impl Fn(crate::storage::PrivilegeSet) -> bool,
    has_grant_option: impl Fn(crate::storage::PrivilegeSet) -> bool,
) -> Result<bool, SqlError> {
    let mut answer = true;
    for item in written.split(',') {
        let item = item.trim();
        const GRANT_OPTION: &str = " WITH GRANT OPTION";
        let (name, grant_option) = if item.len() >= GRANT_OPTION.len()
            && item[item.len() - GRANT_OPTION.len()..].eq_ignore_ascii_case(GRANT_OPTION)
        {
            (item[..item.len() - GRANT_OPTION.len()].trim_end(), true)
        } else {
            (item, false)
        };
        let privilege =
            if name.eq_ignore_ascii_case("all") || name.eq_ignore_ascii_case("all privileges") {
                all
            } else if name.eq_ignore_ascii_case("select") {
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
            } else {
                return Err(sql_err!(
                    sqlstate::INVALID_PARAMETER_VALUE,
                    "unrecognized privilege type: \"{}\"",
                    name
                ));
            };
        let allowed = if grant_option {
            has_grant_option(privilege)
        } else {
            catalog_default.contains(privilege) || has_privilege(privilege)
        };
        answer &= allowed;
    }
    Ok(answer)
}

fn sql_ok() -> Outcome {
    Ok(Ok(()))
}

fn sql_fail(e: SqlError) -> Outcome {
    Ok(Err(e))
}

/// The aggregate hook data for a select's items: the aggregate-call node
/// addresses and their folded values.
type AggregateHookData<'a> = (&'a [*const Expr<'a>], &'a [Datum<'a>]);

/// FROM-less aggregation: PostgreSQL treats the missing FROM clause as a
/// single virtual row (zero rows when WHERE is false). Returns the aggregate
/// hook data for evaluating the select items, or None when the query yields
/// no output row at all (WHERE false under GROUP BY, or HAVING false).
pub(super) fn fromless_aggregate_hooks<'a, R: ColumnLookup<'a>>(
    statement: &'a Select<'a>,
    agg_nodes: &[(*const Expr<'a>, &'a Expr<'a>)],
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
    let mut states = [AggState::default(); MAX_AGGS];
    for (i, (_, node)) in agg_nodes.iter().enumerate() {
        states[i].init(node)?;
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
) {
    let mut slot = 0usize;
    for item in items {
        match item {
            SelectItem::Wildcard => slot += scope.map_or(0, |s| s.star_columns()),
            SelectItem::TableWildcard(q) => {
                slot += scope
                    .and_then(|s| {
                        s.table_index(q)
                            .ok()
                            .map(|t| s.defs[t].expect("resolved").n_columns)
                    })
                    .unwrap_or(0);
            }
            SelectItem::RecordStar(base) => {
                slot += scope.map_or(0, |s| record_star_width(base, s));
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
                                    QueryScope::resolve_schema(storage, &from, txid, arena)
                                && let Ok(witness) =
                                    subquery_witness(storage, txid, inner, Some(&inner_scope))
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
}

type CorrelatedScalarScratch<'a> = [(*const Expr<'a>, Datum<'a>, Datum<'a>); MAX_SUBQUERIES];
type CorrelatedListScratch<'a> = [super::eval::SubqueryList<'a>; MAX_SUBQUERIES];

/// Materializes only the correlated subqueries needed for one source row.
/// Streaming and row-emitting SELECT share this stack-backed seam, keeping
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
        windows: None,
        catalog: base.catalog,
        srf_index: None,
        sequences: base.sequences,
    }
}

/// PostgreSQL has no equality or ordering for `json` — two documents differing
/// only in whitespace or key order are the same value but not the same text, so
/// it declines rather than answer by a rule it does not hold to, and offers
/// canonicalized `jsonb` instead. The `=` operator already declines here; these
/// three sort and deduplicate by the projected encoding and so never consult
/// it, which is why each has to be checked where its keys are known rather than
/// where they are compared.
fn check_key_types<'a>(
    statement: &'a Select<'a>,
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let undefined = |ordering: bool| {
        Err(sql_err!(
            sqlstate::UNDEFINED_FUNCTION,
            "could not identify an {} operator for type json",
            if ordering { "ordering" } else { "equality" }
        ))
    };
    let is_json =
        |e: &Expr<'a>| matches!(infer_scope_type(e, scope), Ok((super::types::oid::JSON, _)));
    for key in statement.group_by.iter().chain(statement.distinct_on) {
        if is_json(key) {
            return undefined(false);
        }
    }
    for order in statement.order_by {
        let target = resolve_order_target(order.expression, statement.items, scope, arena)?;
        if is_json(target) {
            return undefined(true);
        }
    }
    if statement.distinct {
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item
                && is_json(expression)
            {
                return undefined(false);
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
) -> Result<&'a Select<'a>, SqlError> {
    // An aggregate cannot be a grouping key, however it got there — written
    // directly, nested in an expression, or named by a position. Checked after
    // resolution so `GROUP BY 1` naming an aggregate item is caught too.
    let refuse_aggregates = |keys: &[&'a Expr<'a>]| -> Result<(), SqlError> {
        for key in keys {
            let mut nodes: [(*const Expr, &Expr); MAX_AGGS] =
                [(core::ptr::null(), &Expr::Null); MAX_AGGS];
            let mut n = 0;
            collect_aggs(key, &mut nodes, &mut n)?;
            if n > 0 {
                return Err(sql_err!(
                    sqlstate::GROUPING_ERROR,
                    "aggregate functions are not allowed in GROUP BY"
                ));
            }
        }
        Ok(())
    };
    if !statement.group_by.iter().any(|g| matches!(g, Expr::Int(_))) {
        refuse_aggregates(statement.group_by)?;
        return Ok(statement);
    }
    // The parser bounds a GROUP BY list by the same limit it bounds any
    // expression list by, so a parsed statement always fits.
    let mut resolved = [&Expr::Null; super::parser::MAX_LIST];
    for (slot, g) in resolved.iter_mut().zip(statement.group_by) {
        *slot = match g {
            Expr::Int(_) => resolve_position_target(g, statement.items, scope, arena, "GROUP BY")?,
            _ => g,
        };
    }
    refuse_aggregates(&resolved[..statement.group_by.len()])?;
    let group_by = arena
        .alloc_slice_copy(&resolved[..statement.group_by.len()])
        .map_err(|_| arena_full())?;
    let mut rewritten = *statement;
    rewritten.group_by = &*group_by;
    Ok(&*arena.alloc(rewritten).map_err(|_| arena_full())?)
}

/// ORDER BY `n` refers to the n-th *output* column: select-list stars count
/// one position per expanded column, as in PostgreSQL. A position inside a
/// star synthesizes the column reference; names and expressions delegate to
/// the select-list name-binding rules.
fn resolve_order_target<'a>(
    expression: &'a Expr<'a>,
    items: &'a [SelectItem<'a>],
    scope: &QueryScope<'a>,
    arena: &'a Arena,
) -> Result<&'a Expr<'a>, SqlError> {
    resolve_position_target(expression, items, Some(scope), arena, "ORDER BY")
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
                        ResolvedColumn::Table(t, c) => column_ref(
                            Some(scope.names[t]),
                            scope.defs[t].expect("resolved").columns()[c].name.as_str(),
                        ),
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
                let t = scope.table_index(q)?;
                let def = scope.defs[t].expect("resolved");
                if remaining < def.n_columns {
                    return column_ref(
                        Some(scope.names[t]),
                        def.columns()[remaining].name.as_str(),
                    );
                }
                remaining -= def.n_columns;
            }
            SelectItem::RecordStar(base) => {
                let Some(scope) = scope else {
                    return Err(position_error());
                };
                let width = record_star_width(base, scope);
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
                    });
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
/// plain (un-aliased) base column. `where_clause` is the view's own filter, to
/// be AND-ed into any DML on the view; `columns` are the exposed base columns.
pub struct UpdatableView<'a> {
    /// The base table, fully qualified from the view's own resolution so the
    /// rewritten DML binds the same table regardless of the session's path.
    pub base: QualName<'a>,
    pub where_clause: Option<&'a Expr<'a>>,
    pub columns: &'a [&'a str],
}

/// If `name` is a view, resolve it for DML: `Ok(Some(..))` when auto-updatable,
/// `Err` (0A000) when it is a view but not auto-updatable, `Ok(None)` when it
/// is not a view at all (the DML then targets a table normally).
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
    let view = storage.view(view_slot);
    // The body re-resolves under the view creator's search path.
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let view_path = storage.compute_path(view.creation_path.as_str(), user.as_str(), txid);
    // Copy the definition into the arena so the parsed AST no longer borrows
    // storage (the caller then takes a mutable storage borrow to run the DML).
    let sql = arena
        .alloc_str(view.sql.as_str())
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
    let sel = expand_stored_query(
        sel,
        storage,
        txid,
        view_path,
        storage.view_dependencies(view_slot),
        arena,
    )?;
    if sel.distinct
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
    let mut columns = [""; MAX_PROJ];
    let mut n = 0;
    for it in sel.items {
        match it {
            SelectItem::Wildcard => {
                for c in base_def.columns() {
                    if n == MAX_PROJ {
                        return Err(not_updatable());
                    }
                    // Copy into the arena so it does not borrow storage.
                    columns[n] = arena.alloc_str(c.name.as_str()).map_err(|_| arena_full())?;
                    n += 1;
                }
            }
            // Only a plain, un-aliased base column keeps view and base names in
            // sync (so the view's/DML's WHERE resolve directly against the base).
            SelectItem::Expr {
                expression: Expr::Column { name: cn, .. },
                alias,
            } => {
                if alias.is_some_and(|a| a != *cn) {
                    return Err(not_updatable());
                }
                if n == MAX_PROJ {
                    return Err(not_updatable());
                }
                columns[n] = cn;
                n += 1;
            }
            _ => return Err(not_updatable()),
        }
    }
    let columns = arena
        .alloc_slice_copy(&columns[..n])
        .map_err(|_| arena_full())?;
    Ok(Some(UpdatableView {
        base,
        where_clause: sel.where_clause,
        columns,
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

/// Validates a view definition at CREATE VIEW time, as PostgreSQL does: the
/// SELECT must parse, its tables/views must exist, and its output columns must
/// resolve. Surfaces the same errors (42P01 / 42703) a query would.
pub fn validate_view<'a>(
    sql: &'a str,
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    // A view may reference a table/view created earlier in this transaction;
    // other sessions still cannot see either pending object.
    describe_query(sql, storage, txid, arena, &mut columns)?;
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

pub fn describe_stored_query<'a>(
    sql: &'a str,
    storage: &'a Storage,
    txid: u32,
    path: crate::storage::PathContext,
    dependencies: &crate::storage::StoredQueryDependencies,
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
            let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
            describe_scope_items(sel.items, &scope, None, storage, txid, arena, out)
        }
        None => describe_catalog_items(sel.items, None, storage, txid, out),
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
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if expression.is_aggregate_use() {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, expression)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many aggregates in one query"
            ));
        }
        out[*n] = (expression as *const _, expression);
        *n += 1;
        return Ok(()); // aggregate arguments evaluate per input row
    }
    walk_children(expression, &mut |child| collect_aggs(child, out, n))
}

/// Collects window-function call nodes (a `Call` with an `OVER` clause).
pub(super) fn collect_windows<'a>(
    expression: &'a Expr<'a>,
    out: &mut [&'a Expr<'a>; MAX_WINDOWS],
    n: &mut usize,
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
        if filter.is_some() && !expression.is_aggregate() {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "FILTER is not implemented for non-aggregate window functions"
            ));
        }
        if out[..*n].iter().any(|e| core::ptr::eq(*e, expression)) {
            return Ok(());
        }
        if *n == MAX_WINDOWS {
            return Err(sql_err!(
                sqlstate::TOO_MANY_ARGUMENTS,
                "too many window functions in one query"
            ));
        }
        out[*n] = expression;
        *n += 1;
        // The arguments and PARTITION/ORDER expressions evaluate per input row;
        // a window function nested inside another is not supported and would be
        // found by the analysis pass, not here.
        return Ok(());
    }
    walk_children(expression, &mut |child| collect_windows(child, out, n))
}

/// Builds a `JoinRow` view over one flat materialized row (all scope columns
/// concatenated, table by table).
fn window_row<'r, 'a>(
    scope: &'r QueryScope<'a>,
    flat: &'r [Datum<'a>],
    offs: &[usize],
) -> JoinRow<'r, 'a, 'a> {
    let mut values: [Option<&[Datum]>; MAX_JOIN_TABLES] = [None; MAX_JOIN_TABLES];
    for (t, offset) in offs.iter().enumerate().take(scope.n) {
        let nc = scope.defs[t].expect("resolved").n_columns;
        values[t] = Some(&flat[*offset..*offset + nc]);
    }
    JoinRow {
        scope,
        values,
        rowids: &[],
    }
}

/// Collects aggregate-call nodes for a grouped window query: aggregates
/// outside window functions plus those inside window arguments and keys
/// (`sum(sum(v)) OVER (...)`: the inner sum aggregates per group).
fn collect_grouped_aggs<'a>(
    e: &'a Expr<'a>,
    out: &mut [(*const Expr<'a>, &'a Expr<'a>); MAX_AGGS],
    n: &mut usize,
) -> Result<(), SqlError> {
    if let Expr::Call {
        over: Some(spec),
        args,
        filter,
        ..
    } = e
    {
        for a in *args {
            collect_grouped_aggs(a, out, n)?;
        }
        for pk in spec.partition_by {
            collect_grouped_aggs(pk, out, n)?;
        }
        for o in spec.order_by {
            collect_grouped_aggs(o.expression, out, n)?;
        }
        if let Some(frame) = &spec.frame {
            for bound in [&frame.start, &frame.end] {
                if let FrameBound::Preceding(x) | FrameBound::Following(x) = bound {
                    collect_grouped_aggs(x, out, n)?;
                }
            }
        }
        if let Some(f) = filter {
            collect_grouped_aggs(f, out, n)?;
        }
        return Ok(());
    }
    if e.is_aggregate() {
        return collect_aggs(e, out, n);
    }
    // GROUPING() reads the current grouping-set mask, so it must evaluate in
    // the inner grouped select, like an aggregate.
    if let Expr::Call {
        name: "grouping",
        over: None,
        ..
    } = e
    {
        if out[..*n].iter().any(|(p, _)| core::ptr::eq(*p, e)) {
            return Ok(());
        }
        if *n == MAX_AGGS {
            return Err(sql_err!(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "too many aggregates in one query"
            ));
        }
        out[*n] = (e as *const _, e);
        *n += 1;
        return Ok(());
    }
    walk_children(e, &mut |child| collect_grouped_aggs(child, out, n))
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
        | Expr::Exists(_)
        | Expr::ArraySubquery(_) => Ok(e),
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
            let mut items = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, x) in list.iter().enumerate() {
                items[i] = rewrite(x)?;
            }
            let list = arena.alloc_slice_copy(&items[..list.len()]).map_err(|_| arena_full())?;
            alloc(Expr::InList { operand: rewrite(operand)?, list, negated: *negated })
        }
        Expr::Between { operand, low, high, negated } => alloc(Expr::Between {
            operand: rewrite(operand)?,
            low: rewrite(low)?,
            high: rewrite(high)?,
            negated: *negated,
        }),
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
            let mut pairs = [(&Expr::Null as &'a Expr<'a>, &Expr::Null as &'a Expr<'a>);
                super::parser::MAX_LIST];
            for (i, (c, r)) in whens.iter().enumerate() {
                pairs[i] = (rewrite(c)?, rewrite(r)?);
            }
            let whens = arena.alloc_slice_copy(&pairs[..whens.len()]).map_err(|_| arena_full())?;
            let otherwise = match otherwise {
                Some(o) => Some(rewrite(o)?),
                None => None,
            };
            alloc(Expr::Case { operand, whens, otherwise, synthetic: *synthetic })
        }
        Expr::Call { name, args, star, distinct, order_by, over, filter } => {
            let mut rewritten = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, a) in args.iter().enumerate() {
                rewritten[i] = rewrite(a)?;
            }
            let args = arena.alloc_slice_copy(&rewritten[..args.len()]).map_err(|_| arena_full())?;
            let over = match over {
                None => None,
                Some(spec) => {
                    let mut parts = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
                    for (i, pk) in spec.partition_by.iter().enumerate() {
                        parts[i] = rewrite(pk)?;
                    }
                    let partition_by = arena
                        .alloc_slice_copy(&parts[..spec.partition_by.len()])
                        .map_err(|_| arena_full())?;
                    let mut obs = [OrderBy {
                        expression: &Expr::Null,
                        descending: false,
                        nulls_first: false,
                    }; super::parser::MAX_LIST];
                    for (i, o) in spec.order_by.iter().enumerate() {
                        obs[i] = OrderBy { expression: rewrite(o.expression)?, ..*o };
                    }
                    let order_by = arena
                        .alloc_slice_copy(&obs[..spec.order_by.len()])
                        .map_err(|_| arena_full())?;
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
        Expr::Array(items) => {
            let mut rewritten = [&Expr::Null as &'a Expr<'a>; super::parser::MAX_LIST];
            for (i, x) in items.iter().enumerate() {
                rewritten[i] = rewrite(x)?;
            }
            let items =
                arena.alloc_slice_copy(&rewritten[..items.len()]).map_err(|_| arena_full())?;
            alloc(Expr::Array(items))
        }
        Expr::Subscript { base, index } => {
            alloc(Expr::Subscript { base: rewrite(base)?, index: rewrite(index)? })
        }
        Expr::Slice { base, lower, upper } => alloc(Expr::Slice {
            base: rewrite(base)?,
            lower: lower.map(&rewrite).transpose()?,
            upper: upper.map(&rewrite).transpose()?,
        }),
        Expr::Field { base, field } => alloc(Expr::Field { base: rewrite(base)?, field }),
        Expr::AnyAll { operand, operator, array, all } => alloc(Expr::AnyAll {
            operand: rewrite(operand)?,
            operator: *operator,
            array: rewrite(array)?,
            all: *all,
        }),
    }
}

/// Surfaces plan-time constant errors (e.g. `1/0`) across every expression
/// of a SELECT, matching PostgreSQL's constant folding.
pub fn check_select_constants<'a>(
    statement: &Select<'a>,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            super::eval::check_constant_errors(expression, arena)?;
        }
    }
    if let Some(w) = statement.where_clause {
        super::eval::check_constant_errors(w, arena)?;
    }
    for g in statement.group_by {
        super::eval::check_constant_errors(g, arena)?;
    }
    if let Some(h) = statement.having {
        super::eval::check_constant_errors(h, arena)?;
    }
    for ob in statement.order_by {
        super::eval::check_constant_errors(ob.expression, arena)?;
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
            if let SelectItem::Expr { expression, .. } = item {
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
            match storage.acquire_row_lock(
                scope.slots[table],
                rowid,
                txid,
                clause.strength,
                clause.wait,
            )? {
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
    let from = statement
        .from
        .as_ref()
        .expect("FROM-less handled by caller");
    // Windows over a grouped query evaluate after GROUP BY / HAVING: rewrite
    // to the two-level form up front (before the result is described) and run
    // the rewritten statement instead.
    {
        let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
        let mut n_win_probe = 0;
        let mut grouped_aggs: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_grouped_aggs = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                if let Err(e) = collect_windows(expression, &mut win_probe, &mut n_win_probe) {
                    return sql_fail(e);
                }
                if let Err(e) =
                    collect_grouped_aggs(expression, &mut grouped_aggs, &mut n_grouped_aggs)
                {
                    return sql_fail(e);
                }
            }
        }
        let has_srf = find_srf(statement.items).is_some();
        if (n_win_probe > 0 || has_srf)
            && (!statement.group_by.is_empty() || statement.having.is_some() || n_grouped_aggs > 0)
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
    let scope = match QueryScope::resolve_exec(storage, from, txid, arena, params) {
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
    let statement = match resolve_group_ordinals(statement, Some(&scope), arena) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    if let Err(e) = check_key_types(statement, &scope, arena) {
        return sql_fail(e);
    }

    // Subqueries first (uncorrelated, evaluated once).
    let mut sub_exprs: [Option<&Expr>; 4 + 2 * super::parser::MAX_LIST] =
        [None; 4 + 2 * super::parser::MAX_LIST];
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
            sub_exprs[4 + super::parser::MAX_LIST + i] = Some(ob.expression);
        }
    }
    // Uncorrelated subqueries are evaluated once; correlated ones are deferred
    // and re-evaluated per outer row during the scan.
    let outer_subs = match prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let correlated = outer_subs.correlated;
    let mut where_correlated = [&Expr::Null; MAX_SUBQUERIES];
    let n_where_correlated =
        match correlated_in_expression(statement.where_clause, correlated, &mut where_correlated) {
            Ok(count) => count,
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
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        sequences: seq,
    };

    // Plan-time type analysis: validate operator/aggregate types across every
    // clause so incompatible types error before scanning (matching
    // PostgreSQL), not only when a row reaches them. SELECT items are also
    // type-checked by describe below.
    {
        let columns = CatalogScopeCols {
            scope: &scope,
            outer_scope: None,
            storage,
            txid,
        };
        let check = |e: &Expr| -> Result<(), SqlError> {
            super::exec::infer_type_res(e, &columns).map(|_| ())
        };
        let analyze = || -> Result<(), SqlError> {
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
                    &scope,
                    arena,
                )?)?;
            }
            Ok(())
        };
        if let Err(e) = analyze() {
            return sql_fail(e);
        }
    }

    // Constant folding runs after type analysis, matching PostgreSQL's
    // analyze-then-plan order: `min(boolean)` errors before `1/0` folds.
    if let Err(e) = check_select_constants(statement, arena) {
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
    patch_subquery_column_types(
        statement.items,
        Some(&scope),
        &outer_subs.base,
        params,
        storage,
        txid,
        arena,
        &mut columns[..n_cols],
    );
    responder.row_description(&columns[..n_cols])?;

    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };

    // LIMIT 0 returns no rows without scanning or projecting anything, as
    // PostgreSQL does — so a per-row error in an unreturned row does not
    // surface (constant errors already surfaced via the plan-time check).
    if limit == 0 {
        responder.command_complete("SELECT 0")?;
        return sql_ok();
    }

    // Window functions? They run over materialized rows before ORDER BY/LIMIT.
    // An ORDER BY key may be a window function without the select list holding
    // one (`ORDER BY rank() OVER (...)`), so it counts toward the decision.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_windows(expression, &mut win_nodes, &mut n_win)
        {
            return sql_fail(e);
        }
    }
    for ob in statement.order_by {
        if let Err(e) = collect_windows(ob.expression, &mut win_nodes, &mut n_win) {
            return sql_fail(e);
        }
    }
    if n_win > 0 {
        return window_select(
            storage,
            txid,
            statement,
            from,
            &scope,
            &win_nodes[..n_win],
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
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, &mut n_aggs)
        {
            return sql_fail(e);
        }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs)
    {
        return sql_fail(e);
    }
    for ob in statement.order_by {
        if let Err(e) = collect_aggs(ob.expression, &mut agg_nodes, &mut n_aggs) {
            return sql_fail(e);
        }
    }
    if n_aggs > 0 || !statement.group_by.is_empty() {
        return grouped_select(
            storage,
            &scope,
            from,
            txid,
            statement,
            &agg_nodes[..n_aggs],
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
        let where_in_scan = match correlated_scan_conjuncts(
            statement.where_clause,
            &where_correlated[..n_where_correlated],
            arena,
        ) {
            Ok(predicate) => predicate,
            Err(error) => return sql_fail(error),
        };
        // A set-returning `_pg_expandarray(array)` expands each row into one output
        // row per array element.
        let srf_call = find_srf(statement.items);
        // This stream is the only path whose projection is consumed directly
        // from the scan callback. Its complete row demand is therefore known
        // here: projection expressions plus the complete WHERE. Every other
        // scan path retains full rows until it proves an equivalent contract.
        let pax_columns =
            streaming_pax_columns(&scope, from, statement.items, statement.where_clause);
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
                if n_where_correlated > 0
                    && !correlated_where_passes(
                        &where_correlated[..n_where_correlated],
                        &outer_subs.base,
                        statement.where_clause,
                        row,
                        storage,
                        txid,
                        arena,
                        params,
                        &hooks,
                    )?
                {
                    return Ok(true);
                }
                // Per-row hooks for correlated subqueries; then WHERE.
                let mut scalar_scratch: CorrelatedScalarScratch =
                    [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
                let mut list_scratch: CorrelatedListScratch<'_> =
                    [subquery::empty_subquery_list(); MAX_SUBQUERIES];
                let row_subqueries = correlated_row_subqueries(
                    correlated,
                    &outer_subs.base,
                    row,
                    storage,
                    txid,
                    arena,
                    params,
                    &mut scalar_scratch,
                    &mut list_scratch,
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
                let count = srf_max_count(statement.items, arena, params, row, row_hooks)?;
                for k in 1..=count {
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
                    let use_hooks: &EvalHooks = if srf_call.is_some() {
                        srf_hooks = EvalHooks {
                            srf_index: Some(k),
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
                    if let Err(w) = responder.data_row(&projected[..n]) {
                        wire_full = true;
                        wire_result = Err(w);
                        return Ok(false);
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
            col_alias: None,
            cte: None,
            with_ordinality: false,
            lateral: false,
            authorization_role: None,
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
    if let Err(e) = check_select_constants(statement, arena) {
        return sql_fail(e);
    }
    // A window function needs rows to compute over, and this path has no scan
    // to give it. A FROM-less SELECT is exactly one row, though, so it can be
    // written as a one-row derived table and handed to the ordinary path —
    // which already knows about partitions, frames and every window function
    // there is. Teaching this path any of that would be a second copy.
    let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_windows(expression, &mut win_probe, &mut n_win)
        {
            return sql_fail(e);
        }
    }
    if n_win > 0 {
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
    let statement = match resolve_group_ordinals(statement, None, arena) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    let mut columns = [ColDesc::new("", 0, 0); MAX_PROJ];
    let n = match describe_catalog_items(statement.items, None, storage, txid, &mut columns) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };

    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
    sub_exprs[0] = statement.where_clause;
    for (i, item) in statement.items.iter().enumerate() {
        if let SelectItem::Expr { expression, .. } = item {
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
    ) {
        Ok(s) => s,
        Err(e) => return sql_fail(e),
    };
    patch_subquery_column_types(
        statement.items,
        None,
        &subs,
        params,
        storage,
        txid,
        arena,
        &mut columns[..n],
    );
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
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        sequences: seq,
    };

    // Aggregates (or GROUP BY / HAVING) without FROM: PostgreSQL aggregates
    // over one virtual input row (zero when WHERE is false) and emits at most
    // one output row.
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item
            && let Err(e) = collect_aggs(expression, &mut agg_nodes, &mut n_aggs)
        {
            return sql_fail(e);
        }
    }
    if let Some(h) = statement.having
        && let Err(e) = collect_aggs(h, &mut agg_nodes, &mut n_aggs)
    {
        return sql_fail(e);
    }
    for ob in statement.order_by {
        if let Err(e) = collect_aggs(ob.expression, &mut agg_nodes, &mut n_aggs) {
            return sql_fail(e);
        }
    }
    if n_aggs > 0 || statement.having.is_some() || !statement.group_by.is_empty() {
        if find_srf(statement.items).is_some() {
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
            &agg_nodes[..n_aggs],
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
            responder.data_row(&vals[..statement.items.len()])?;
            rows = 1;
        }
        let tag = stack_format!(48, "SELECT {}", rows);
        responder.command_complete(tag.as_str())?;
        return sql_ok();
    }

    // A set-returning function in the select list expands the single virtual
    // row into one output row per element/value.
    let srf_call = find_srf(statement.items);
    let count = match srf_max_count(
        statement.items,
        arena,
        params,
        &super::eval::NoColumns,
        &hooks,
    ) {
        Ok(n) => n,
        Err(e) => return sql_fail(e),
    };
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
                SelectItem::RecordStar(base) => {
                    super::exec::record_shape(base, &super::exec::NoCols, |_, _| {}).unwrap_or(0)
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

    // Materialize every output row (visible values + hidden sort keys).
    let mut n_rows = 0usize;
    let max_rows = count;
    let empty: &[u8] = &[];
    let encoded = match arena.alloc_slice_with(max_rows, |_| empty) {
        Ok(e) => e,
        Err(_) => return sql_fail(arena_full()),
    };
    for k in 1..=count {
        let khooks = if srf_call.is_some() {
            EvalHooks {
                srf_index: Some(k),
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
        if let Some(w) = statement.where_clause {
            match where_passes(w, arena, params, &super::eval::NoColumns, &khooks) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(e) => return sql_fail(e),
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
        order_collations[index] =
            match resolved_expression_collation(expression, &super::eval::NoColumns) {
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

    let limit = match super::exec::eval_limit_pub(statement.limit, arena, params) {
        Ok(l) => l,
        Err(e) => return sql_fail(e),
    };
    let offset = match super::exec::eval_offset_pub(statement.offset, arena, params) {
        Ok(o) => o,
        Err(e) => return sql_fail(e),
    };
    let start = (offset as usize).min(out_rows.len());
    let mut end = offset.saturating_add(limit).min(out_rows.len() as u64) as usize;
    // FETCH FIRST ... WITH TIES over a FROM-less SRF result: keep rows tying
    // with the last on the ORDER BY keys (hidden columns after `width`).
    if statement.with_ties && limit > 0 {
        end = match materialize::extend_ties(
            storage,
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
        responder.data_row(&values[..width])?;
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
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        // A trailing ORDER BY needs one final sort over the combined multiset.
        let columns_owned: Option<[crate::sql::types::ColDesc; MAX_PROJ]> =
            (!statement.order_by.is_empty()).then(|| {
                let mut cols = [crate::sql::types::ColDesc::new("", 0, 0); MAX_PROJ];
                setops::describe_set_body(storage, tree, txid, &mut cols, arena).ok();
                cols
            });
        if let Some(ref cols) = columns_owned {
            let rows_mut = arena
                .alloc_slice_with(rows.len(), |i| rows[i])
                .map_err(|_| arena_full())?;
            setops::sort_set_rows(storage, arena, rows_mut, statement.order_by, &cols[..n])?;
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
    check_select_constants(statement, arena)?;
    let catalog = StorageCatalog {
        storage,
        routine_workspace: arena,
        txid,
        invocations: None,
        statement_arena: None,
    };
    let mut agg_nodes: [(*const Expr, &Expr); MAX_AGGS] =
        [(core::ptr::null(), &Expr::Null); MAX_AGGS];
    let mut n_aggs = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_aggs(expression, &mut agg_nodes, &mut n_aggs)?;
        }
    }
    if let Some(h) = statement.having {
        collect_aggs(h, &mut agg_nodes, &mut n_aggs)?;
    }
    for ob in statement.order_by {
        collect_aggs(ob.expression, &mut agg_nodes, &mut n_aggs)?;
    }
    // GROUP BY or aggregates: run the grouped executor (which sorts by any
    // ORDER BY and dedups DISTINCT) and emit each output row, honoring
    // LIMIT/OFFSET. A set-returning function expands after aggregation —
    // rewrite to the two-level form first.
    if (!statement.group_by.is_empty() || n_aggs > 0) && find_srf(statement.items).is_some() {
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
    if !statement.group_by.is_empty() || n_aggs > 0 {
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
                None,
            )?;
            let hooks = EvalHooks {
                group: None,
                aggs: None,
                sequences: seq,
                subs: Some(&subs),
                windows: None,
                catalog: Some(&catalog),
                srf_index: None,
            };
            let Some((ptrs, values)) = fromless_aggregate_hooks(
                statement,
                &agg_nodes[..n_aggs],
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
            let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
            let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
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
        let scope = QueryScope::resolve_exec_outer(storage, from, txid, arena, params, outer)?;
        let statement = resolve_group_ordinals(statement, Some(&scope), arena)?;
        check_key_types(statement, &scope, arena)?;
        let mut sub_exprs: [Option<&Expr>; 2 + MAX_PROJ] = [None; 2 + MAX_PROJ];
        sub_exprs[0] = statement.where_clause;
        sub_exprs[1] = statement.having;
        for (i, item) in statement.items.iter().enumerate() {
            if let SelectItem::Expr { expression, .. } = item {
                sub_exprs[2 + i] = Some(expression);
            }
        }
        let outer_subs = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
        let hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: Some(&outer_subs.base),
            windows: None,
            catalog: Some(&catalog),
            srf_index: None,
            sequences: seq,
        };
        let (rows, width) = grouped_rows(
            storage,
            &scope,
            from,
            txid,
            statement,
            &agg_nodes[..n_aggs],
            arena,
            params,
            &hooks,
            outer_subs.correlated,
            outer,
        )?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
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
    let mut sub_exprs: [Option<&Expr>; 1 + MAX_PROJ] = [None; 1 + MAX_PROJ];
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
        let mut win_probe: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
        let mut n_win = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_windows(expression, &mut win_probe, &mut n_win)?;
            }
        }
        if n_win > 0 {
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
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
        if limit == 0 {
            return Ok(());
        }
        // A FROM-less body may still reference an outer row when it is a LATERAL
        // item (`LATERAL (SELECT t.a * 2)`); resolve against that outer scope.
        let cols: &dyn ColumnLookup = outer.unwrap_or(&super::eval::NoColumns);
        // FROM-less: one row (or zero, when WHERE is false), unless a
        // set-returning function in the list expands it to several.
        let subs = prepare_subqueries(
            &sub_exprs,
            storage,
            txid,
            arena,
            params,
            SUBQUERY_DEPTH,
            None,
        )?;
        let hooks = EvalHooks {
            group: None,
            aggs: None,
            subs: Some(&subs),
            windows: None,
            catalog: Some(&catalog),
            srf_index: None,
            sequences: seq,
        };
        let srf_call = find_srf(statement.items);
        let count = srf_max_count(statement.items, arena, params, &cols, &hooks)?;
        let mut produced = 0u64;
        for k in 1..=count {
            let khooks = if srf_call.is_some() {
                EvalHooks {
                    srf_index: Some(k),
                    ..hooks
                }
            } else {
                hooks
            };
            if let Some(w) = statement.where_clause
                && !where_passes(w, arena, params, &cols, &khooks)?
            {
                continue;
            }
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

    let scope = QueryScope::resolve_exec_outer(storage, from, txid, arena, params, outer)?;
    let outer_subs = prepare_outer_subqueries(&sub_exprs, storage, txid, arena, params)?;
    let correlated = outer_subs.correlated;
    let mut where_correlated = [&Expr::Null; MAX_SUBQUERIES];
    let n_where_correlated =
        correlated_in_expression(statement.where_clause, correlated, &mut where_correlated)?;
    let hooks = EvalHooks {
        group: None,
        aggs: None,
        subs: Some(&outer_subs.base),
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        sequences: seq,
    };

    // Window functions (`OVER (...)`) in the projection: materialize the rows
    // with each window value computed, then emit. ORDER BY/LIMIT are handled by
    // the outer query, so the derived-table order is left unspecified.
    let mut win_nodes: [&Expr; MAX_WINDOWS] = [&Expr::Null; MAX_WINDOWS];
    let mut n_win = 0;
    for item in statement.items {
        if let SelectItem::Expr { expression, .. } = item {
            collect_windows(expression, &mut win_nodes, &mut n_win)?;
        }
    }
    for ob in statement.order_by {
        collect_windows(ob.expression, &mut win_nodes, &mut n_win)?;
    }
    if n_win > 0 {
        // Windows over a grouped query: rewrite to the two-level form.
        let mut grouped_aggs: [(*const Expr, &Expr); MAX_AGGS] =
            [(core::ptr::null(), &Expr::Null); MAX_AGGS];
        let mut n_grouped_aggs = 0;
        for item in statement.items {
            if let SelectItem::Expr { expression, .. } = item {
                collect_grouped_aggs(expression, &mut grouped_aggs, &mut n_grouped_aggs)?;
            }
        }
        if !statement.group_by.is_empty() || statement.having.is_some() || n_grouped_aggs > 0 {
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
            let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
            let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
            external_window_into(
                storage,
                txid,
                statement,
                from,
                &scope,
                &win_nodes[..n_win],
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
            &win_nodes[..n_win],
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
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
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
            let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
            let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
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
        )?;
        let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
        let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
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
    let where_in_scan = correlated_scan_conjuncts(
        statement.where_clause,
        &where_correlated[..n_where_correlated],
        arena,
    )?;
    let limit = super::exec::eval_limit_pub(statement.limit, arena, params)?;
    let offset = super::exec::eval_offset_pub(statement.offset, arena, params)?;
    let stop_after = offset.saturating_add(limit);
    if stop_after == 0 {
        return Ok(());
    }
    let mut produced = 0u64;

    // A set-returning `_pg_expandarray(array)` in the projection expands each
    // source row into one output row per array element.
    let srf_call = find_srf(statement.items);
    let mut visit = |row: &JoinRow<'_, 'a, '_>| {
        if n_where_correlated > 0
            && !correlated_where_passes(
                &where_correlated[..n_where_correlated],
                &outer_subs.base,
                statement.where_clause,
                row,
                storage,
                txid,
                arena,
                params,
                &hooks,
            )?
        {
            return Ok(true);
        }
        let mut scalar_scratch: CorrelatedScalarScratch =
            [(core::ptr::null(), Datum::Null, Datum::Null); MAX_SUBQUERIES];
        let mut list_scratch: CorrelatedListScratch<'_> =
            [subquery::empty_subquery_list(); MAX_SUBQUERIES];
        let row_subqueries = correlated_row_subqueries(
            correlated,
            &outer_subs.base,
            row,
            storage,
            txid,
            arena,
            params,
            &mut scalar_scratch,
            &mut list_scratch,
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
        match srf_call {
            None => {
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
            }
            Some(c) => {
                let count = srf_count(c, arena, params, row, row_hooks)?;
                for k in 1..=count {
                    let srf_hooks = EvalHooks {
                        srf_index: Some(k),
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

fn streaming_pax_columns<'a>(
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
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    "select list expands past {} columns",
                    MAX_PROJ
                ));
            }
            out[n] = Datum::Null;
            n += 1;
            continue;
        }
        match item {
            SelectItem::TableWildcard(q) => {
                let t = scope.table_index(q)?;
                let vals = row.values[t].expect("bound");
                for c in 0..scope.defs[t].expect("resolved").n_columns {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = if vals.is_empty() {
                        Datum::Null
                    } else {
                        vals[c]
                    };
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                let value_of = |t: usize, c: usize| {
                    let vals = row.values[t].expect("bound");
                    if vals.is_empty() {
                        Datum::Null
                    } else {
                        vals[c]
                    }
                };
                for k in 0..scope.star_columns() {
                    if n == MAX_PROJ {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = match scope.star_entry(k) {
                        ResolvedColumn::Table(t, c) => value_of(t, c),
                        // Merged USING/NATURAL column: first non-null side.
                        ResolvedColumn::Merged(m) => {
                            let mc = &scope.merged[m];
                            mc.parts[..mc.n_parts]
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
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list expands past {} columns",
                            MAX_PROJ
                        ));
                    }
                    out[n] = field.value;
                    n += 1;
                }
            }
            SelectItem::Expr { expression, .. } => {
                if n == MAX_PROJ {
                    return Err(sql_err!(
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "select list expands past {} columns",
                        MAX_PROJ
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
        *value = match *value {
            Datum::CompositeText { slot, text } => {
                catalog.materialize_composite(slot, text, arena)?
            }
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
                let t = scope.table_index(q)?;
                for c in scope.defs[t].expect("resolved").columns() {
                    if n == out.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list too wide"
                        ));
                    }
                    out[n] = ColDesc::of_type(c.name.as_str(), c.ctype).with_type_mod(c.type_mod);
                    out[n].collation = c.collation;
                    n += 1;
                }
            }
            SelectItem::Wildcard => {
                for k in 0..scope.star_columns() {
                    if n == out.len() {
                        return Err(sql_err!(
                            sqlstate::PROGRAM_LIMIT_EXCEEDED,
                            "select list too wide"
                        ));
                    }
                    let entry = scope.star_entry(k);
                    out[n] = ColDesc::of_type(scope.output_name(entry), scope.output_type(entry))
                        .with_type_mod(match entry {
                            ResolvedColumn::Table(t, c) => {
                                scope.defs[t].expect("resolved").columns()[c].type_mod
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
                        sqlstate::PROGRAM_LIMIT_EXCEEDED,
                        "select list too wide"
                    ));
                }
                // Multi-table type inference: columns resolve via scope.
                let name = alias.unwrap_or(super::exec::derived_name(expression));
                let user_type = user_type_expression_description(expression, name, storage, txid);
                let (oid, typlen) = match user_type {
                    Some(description) => (description.type_oid, description.typlen),
                    None => {
                        let resolver = CatalogScopeCols {
                            scope,
                            outer_scope,
                            storage,
                            txid,
                        };
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
                let type_mod = match expression {
                    Expr::Column { qualifier, name } => {
                        scope_column_type_mod(scope, outer_scope, *qualifier, name)
                    }
                    Expr::Cast { type_mod, .. } => *type_mod,
                    _ => -1,
                };
                out[n] = ColDesc::new(name, oid, typlen)
                    .with_type_mod(user_type.map_or(type_mod, |description| description.type_mod));
                if let Some(ctype) = super::exec::coltype_of_oid(oid)
                    && ctype.is_collatable()
                {
                    out[n].collation = scope.expression_collation(expression)?;
                }
                n += 1;
            }
        }
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
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    describe_catalog_items_as(items, definition, None, storage, txid, out)
}

pub fn describe_catalog_items_as<'q>(
    items: &[SelectItem<'q>],
    definition: Option<&'q TableDef>,
    alias: Option<&str>,
    storage: &'q Storage,
    txid: u32,
    out: &mut [ColDesc<'q>],
) -> Result<usize, SqlError> {
    let count = describe_items(items, definition, alias, Some(storage), txid, out)?;
    let mut column = 0;
    for item in items {
        match item {
            SelectItem::Wildcard | SelectItem::TableWildcard(_) => {
                column += definition.map_or(0, |table| table.n_columns);
            }
            SelectItem::RecordStar(base) => {
                let resolver: &dyn super::exec::ColTypeResolver = match definition {
                    Some(definition) => &super::exec::AliasedDefCols { definition, alias },
                    None => &super::exec::NoCols,
                };
                column += super::exec::record_shape(base, resolver, |_, _| {})
                    .expect("described record star has a static shape");
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
                if let Expr::Call {
                    name,
                    args,
                    star: false,
                    ..
                } = expression
                {
                    let resolver: &dyn super::exec::ColTypeResolver = match definition {
                        Some(definition) => &super::exec::AliasedDefCols {
                            definition,
                            alias: *alias,
                        },
                        None => &super::exec::NoCols,
                    };
                    let mut argument_types = [ColType::Text; crate::storage::MAX_ROUTINE_ARGUMENTS];
                    if args.len() <= argument_types.len() {
                        let mut known = true;
                        for (argument_index, argument) in args.iter().enumerate() {
                            let Ok((oid, _)) = super::exec::infer_type_res(argument, resolver)
                            else {
                                known = false;
                                break;
                            };
                            let Some(ctype) = super::exec::coltype_of_oid_pub(oid) else {
                                known = false;
                                break;
                            };
                            argument_types[argument_index] = ctype;
                        }
                        if known
                            && let Some(routine) = storage.routine_for_call_types(
                                name,
                                &argument_types[..args.len()],
                                txid,
                            )
                        {
                            out[column] = ColDesc::new(
                                alias.unwrap_or(super::exec::derived_name(expression)),
                                routine
                                    .kind
                                    .function_result()
                                    .expect("scalar routine used as an expression has a result")
                                    .oid(),
                                routine
                                    .kind
                                    .function_result()
                                    .expect("scalar routine used as an expression has a result")
                                    .typlen(),
                            );
                        }
                    }
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
        None => describe_catalog_items(items, None, storage, txid, out)?,
    };
    let mut column = 0;
    for item in items {
        match item {
            SelectItem::Wildcard => column += scope.map_or(0, QueryScope::star_columns),
            SelectItem::TableWildcard(table) => {
                if let Some(scope) = scope
                    && let Ok(table) = scope.table_index(table)
                {
                    column += scope.defs[table].expect("resolved").n_columns;
                }
            }
            SelectItem::RecordStar(base) => {
                column += scope.map_or(0, |scope| record_star_width(base, scope));
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
    Ok(count)
}

#[derive(Clone, Copy)]
struct SubqueryResultType {
    type_oid: i32,
    typlen: i16,
    type_mod: i32,
}

fn subquery_result_type<'a>(
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
    let count = match &select.from {
        Some(from) => {
            let scope = QueryScope::resolve_schema(storage, from, txid, arena)?;
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
        None if (super::types::oid::FIRST_DOMAIN
            ..super::types::oid::FIRST_DOMAIN + u16::MAX as i32 + 1)
            .contains(&element_oid) =>
        {
            Some(super::types::oid::domain_array_oid(
                (element_oid - super::types::oid::FIRST_DOMAIN) as u16,
            ))
        }
        None if (super::types::oid::FIRST_ENUM
            ..super::types::oid::FIRST_ENUM + u16::MAX as i32 + 1)
            .contains(&element_oid) =>
        {
            Some(super::types::oid::enum_array_oid(
                (element_oid - super::types::oid::FIRST_ENUM) as u16,
            ))
        }
        None => None,
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
            ColDesc::new(name, super::types::oid::domain_array_oid(slot as u16), -1)
        } else {
            ColDesc::of_type(name, domain.base).with_type_mod(domain.base_type_mod)
        });
    }
    let slot = storage.resolve_enum_slot(base_name, txid)?;
    Some(if array {
        ColDesc::new(name, super::types::oid::enum_array_oid(slot as u16), -1)
    } else {
        ColDesc::new(name, super::types::oid::enum_oid(slot as u16), 4)
    })
}

/// Catalog-defined casts retain their type boundary even when they are nested
/// in an anonymous record field. Static expression inference intentionally has
/// no catalog dependency, so resolving this shape here prevents a valid domain
/// or enum field from being mistaken for an unknown literal.
fn user_type_expression_description<'q>(
    expression: &Expr<'q>,
    name: &'q str,
    storage: &Storage,
    txid: u32,
) -> Option<ColDesc<'q>> {
    if let Some(description) = user_type_cast_description(expression, name, storage, txid) {
        return Some(description);
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
        super::types::ColType::Array(super::types::ArrElem::Enum(slot)) => {
            Some(ColDesc::new(name, super::types::oid::enum_oid(slot), 4))
        }
        super::types::ColType::Array(super::types::ArrElem::Domain { slot, .. }) => {
            let domain = storage.domain(slot as usize);
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
            super::types::ArrElem::Enum(slot) => {
                value.type_oid == super::types::oid::enum_oid(slot)
            }
            super::types::ArrElem::Domain { slot, .. } => {
                value.type_oid == storage.domain(slot as usize).base.oid()
            }
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
                Expr::Null => true,
                Expr::Cast {
                    type_name: candidate,
                    ..
                } => candidate.eq_ignore_ascii_case(type_name),
                _ => false,
            })
        {
            return Some(ColDesc::new(
                name,
                super::types::oid::domain_array_oid(slot as u16),
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
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "select list too wide"
            ));
        }
        out[*n] = desc;
        *n += 1;
        Ok(())
    };
    match base {
        Expr::Call { name, args, .. } if name.eq_ignore_ascii_case("row") => {
            super::exec::check_row_field_types(base, &ScopeCols(scope))?;
            for (i, arg) in args
                .iter()
                .take(super::exec::RECORD_FIELD_NAMES.len())
                .enumerate()
            {
                let (oid, typlen) = infer_scope_type(arg, scope)?;
                push(
                    ColDesc::new(super::exec::RECORD_FIELD_NAMES[i], oid, typlen),
                    &mut n,
                )?;
            }
        }
        Expr::WholeRow(table)
        | Expr::Column {
            qualifier: None,
            name: table,
        } if scope.table_index(table).is_ok() => {
            let t = scope.table_index(table)?;
            for c in &scope.defs[t].expect("resolved").columns()
                [..scope.defs[t].expect("resolved").n_columns]
            {
                push(ColDesc::of_type(c.name.as_str(), c.ctype), &mut n)?;
            }
        }
        // json_each family: `(key, value)` with statically-known names/types.
        Expr::Call { name, .. } if super::exec::json_each_value_type_pub(name).is_some() => {
            push(ColDesc::of_type("key", ColType::Text), &mut n)?;
            let value_type = super::exec::json_each_value_type_pub(name).expect("checked");
            push(ColDesc::of_type("value", value_type), &mut n)?;
        }
        _ => {
            let resolver = CatalogScopeCols {
                scope,
                outer_scope: None,
                storage,
                txid,
            };
            let slot = match base {
                Expr::Column { qualifier, name } => {
                    match super::exec::ColTypeResolver::resolve(&resolver, *qualifier, name)? {
                        ColType::Composite(slot) => Some(slot),
                        _ => None,
                    }
                }
                Expr::Field { base, field } => {
                    match super::exec::record_field_type(base, field, &resolver)? {
                        ColType::Composite(slot) => Some(slot),
                        _ => None,
                    }
                }
                Expr::Cast { type_name, .. } => storage
                    .resolve_composite_slot(type_name, txid)
                    .map(|slot| slot as u16),
                _ => None,
            };
            if let Some(slot) = slot {
                for field in storage.composite(slot as usize).fields() {
                    let name = arena
                        .alloc_str(field.name.as_str())
                        .map_err(|_| arena_full())?;
                    push(
                        ColDesc::of_type(name, field.ctype).with_type_mod(field.type_mod),
                        &mut n,
                    )?;
                }
                return Ok(n);
            }
            let Some(handle) = super::exec::expr_record_handle_pub(base, &ScopeCols(scope)) else {
                return Err(sql_err!(
                    sqlstate::WRONG_OBJECT_TYPE,
                    "row expansion is not supported on this expression"
                ));
            };
            let mut push_err = None;
            super::exec::visit_record_shape_pub(handle, |field_name, ctype| {
                if push_err.is_some() {
                    return;
                }
                match arena.alloc_str(field_name) {
                    Ok(name) => {
                        if let Err(error) = push(ColDesc::of_type(name, ctype), &mut n) {
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

    fn is_whole_row(&self, name: &str) -> bool {
        self.0.table_index(name).is_ok()
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.0.func_scalar_type(name)
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        let t = self.0.table_index(name).ok()?;
        let def = self.0.defs[t]?;
        Some(&def.columns()[..def.n_columns])
    }

    fn record_column_handle(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        let entry = self.0.find_column(qualifier, name).ok()?;
        if self.0.output_type(entry) != ColType::Record {
            return None;
        }
        match entry {
            scope::ResolvedColumn::Table(t, c) => Some(self.0.defs[t]?.columns()[c].type_mod),
            _ => None,
        }
    }
}

struct CatalogScopeCols<'scope, 'definition, 'storage> {
    scope: &'scope QueryScope<'definition>,
    outer_scope: Option<&'scope QueryScope<'definition>>,
    storage: &'storage Storage,
    txid: u32,
}

impl super::exec::ColTypeResolver for CatalogScopeCols<'_, '_, '_> {
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

    fn routine_result(&self, name: &str, arguments: &[ColType]) -> Option<ColType> {
        self.storage
            .routine_for_call_types(name, arguments, self.txid)?
            .kind
            .function_result()
    }

    fn named_composite_field(
        &self,
        type_name: &str,
        index: usize,
    ) -> Option<(crate::util::StackStr<64>, ColType)> {
        let slot = self.storage.resolve_composite_slot(type_name, self.txid)?;
        let field = self.storage.composite(slot).fields().get(index)?;
        Some((
            crate::util::StackStr::from_str(field.name.as_str()),
            field.ctype,
        ))
    }

    fn is_whole_row(&self, name: &str) -> bool {
        self.scope.table_index(name).is_ok()
            || self
                .outer_scope
                .is_some_and(|scope| scope.table_index(name).is_ok())
    }

    fn whole_row_scalar_type(&self, name: &str) -> Option<ColType> {
        self.scope.func_scalar_type(name).or_else(|| {
            self.outer_scope
                .and_then(|scope| scope.func_scalar_type(name))
        })
    }

    fn table_columns(&self, name: &str) -> Option<&[ColumnMeta]> {
        if let Ok(table) = self.scope.table_index(name) {
            return Some(self.scope.defs[table]?.columns());
        }
        let scope = self.outer_scope?;
        let table = scope.table_index(name).ok()?;
        Some(scope.defs[table]?.columns())
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
                Some(scope.defs[table]?.columns()[column].type_mod)
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
            scope.defs[table].expect("resolved").columns()[column].type_mod
        }
        _ => -1,
    }
}

/// The number of columns a `(base).*` record expansion contributes, or 0 when
/// its shape is not statically known (surfaced loudly at projection time).
pub(super) fn record_star_width(base: &Expr, scope: &QueryScope) -> usize {
    super::exec::record_shape(base, &ScopeCols(scope), |_, _| {}).unwrap_or(0)
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
) -> Result<bool, SqlError> {
    let scope = QueryScope::resolve_exec_outer(storage, from, txid, arena, params, Some(target))?;
    let subs = subquery_hooks(&[where_clause], storage, txid, arena, params)?;
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
        windows: None,
        catalog: Some(&catalog),
        srf_index: None,
        sequences: None,
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
    scan_source_with_pax_columns(
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
        &mut |jr| {
            let chained_row = Chained {
                inner: jr,
                outer: Some(target),
            };
            on_match(&chained_row)?;
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
