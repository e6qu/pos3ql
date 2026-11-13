//! Schema-only discovery of UNKNOWN inputs whose destination runs array input.

use crate::mem::arena::Arena;
use crate::sql::type_inputs::InputKind;
use crate::sql::{
    ast::{self, Expr, Select, SelectItem, Stmt},
    eval::{ColumnLookup, RuntimeColumnTypes, SqlError},
    exec::ColTypeResolver,
    types::oid,
};
use crate::sql_err;
use crate::storage::{NamedTypeKind, RoutineCallKind, RoutineCallSelection, Storage};

type Visit<'a> = dyn FnMut(&Expr<'_>, i32, i32, InputKind) -> Result<(), SqlError> + 'a;
type PlannerVisit<'a> = dyn FnMut(
        &Expr<'_>,
        &dyn ColTypeResolver,
        Option<(i32, i32, crate::sql::type_inputs::AssignmentSite)>,
    ) -> Result<(), SqlError>
    + 'a;

// Discovery can borrow storage briefly even when caller values live for the
// entire execution. This adapter exposes the same declared identities only.
pub(super) struct MetadataOnly<'s, 'v> {
    pub(super) row: &'s dyn ColumnLookup<'v>,
    pub(super) columns: &'s dyn ColTypeResolver,
}

impl<'a> ColumnLookup<'a> for MetadataOnly<'_, '_> {
    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        self.columns
            .column_meta(qualifier, name)
            .map_or(-1, |column| column.type_mod)
    }

    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        self.row.whole_row_field_metadata(table, index, catalog)
    }

    fn lookup(
        &self,
        _qualifier: Option<&str>,
        _name: &str,
    ) -> Result<crate::sql::types::Datum<'a>, SqlError> {
        Err(sql_err!(
            super::sqlstate::INTERNAL_ERROR,
            "SQL input discovery cannot read row values"
        ))
    }
    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<crate::sql::types::ColType, SqlError> {
        // A column descriptor cannot replace whole-row alias resolution.
        // Only saved record OIDs need catalog-backed physical type lookup.
        if self.row.column_type_oid(qualifier, name).is_some() {
            self.col_type(qualifier, name).ok_or_else(|| {
                sql_err!(
                    super::sqlstate::UNDEFINED_OBJECT,
                    "record field's declared type is unavailable"
                )
            })
        } else {
            self.row.resolve_column_type(qualifier, name)
        }
    }
    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<crate::sql::types::ColType> {
        if let Some(type_oid) = self.row.column_type_oid(qualifier, name) {
            crate::sql::eval::constructor_type_from_oid(type_oid, &|oid| {
                self.columns.domain_base_type(oid)
            })
        } else {
            self.row.col_type(qualifier, name)
        }
    }
    fn column_user_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::storage::UserTypeName> {
        self.row.column_user_type(qualifier, name)
    }
    fn column_type_oid(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        self.row.column_type_oid(qualifier, name)
    }

    fn row_variable(&self, name: &str) -> Option<crate::sql::eval::RowVariable> {
        self.row.row_variable(name)
    }
    fn collation(&self, qualifier: Option<&str>, name: &str) -> ast::Collation {
        self.row.collation(qualifier, name)
    }
    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        self.row.column_identity(qualifier, name)
    }
    fn is_empty_scope(&self) -> bool {
        self.row.is_empty_scope()
    }
    fn whole_row_type(&self, table: &str) -> Option<crate::sql::types::ColType> {
        self.row.whole_row_type(table)
    }
    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.row.whole_row_is_scalar(table)
    }
    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> ast::Collation {
        crate::sql::exec::record_field_metadata(base, field, self.columns)
            .map_or(ast::Collation::None, |meta| meta.collation)
    }
}

pub(crate) fn capture_contextual_type_inputs(
    parser: &crate::sql::parser::Parser<'_>,
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
) -> Result<(), SqlError> {
    capture_discovered_type_inputs(parser, storage, txid, arena, |visit| {
        discover_defaults(parser, statement, storage, txid, arena, visit)
            .and_then(|()| discover(statement, storage, txid, arena, visit))
    })
}

pub(crate) fn capture_contextual_expression_type_inputs<'a>(
    parser: &crate::sql::parser::Parser<'_>,
    expression: &Expr<'a>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    columns: &dyn ColTypeResolver,
    row: &dyn ColumnLookup<'a>,
) -> Result<(), SqlError> {
    capture_discovered_type_inputs(parser, storage, txid, arena, |visit| {
        let metadata = MetadataOnly { row, columns };
        let mut walker = Walker {
            storage,
            txid,
            arena,
            visit,
            planner: PlannerPass::TypeInputs,
        };
        walker.expression(expression, columns, &metadata)
    })
}

pub(crate) fn capture_procedural_statement_type_inputs<'a>(
    parser: &crate::sql::parser::Parser<'_>,
    statement: &'a Stmt<'a>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: &dyn ColumnLookup<'a>,
) -> Result<(), SqlError> {
    capture_discovered_type_inputs(parser, storage, txid, arena, |visit| {
        let catalog = super::storage_catalog(storage, arena, txid);
        let columns = RuntimeColumnTypes {
            row: outer,
            catalog: Some(&catalog),
        };
        let outer = MetadataOnly {
            row: outer,
            columns: &columns,
        };
        let mut walker = Walker {
            storage,
            txid,
            arena,
            visit,
            planner: PlannerPass::TypeInputs,
        };
        walker.statement(statement, Some(&outer))
    })
}

fn capture_discovered_type_inputs(
    parser: &crate::sql::parser::Parser<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    discover: impl FnOnce(&mut Visit<'_>) -> Result<(), SqlError>,
) -> Result<(), SqlError> {
    let literals = parser.string_literals();
    if literals.is_empty()
        || !(0..storage.domain_count()).any(|slot| storage.domain_slot_visible_to(slot, txid))
    {
        return Ok(());
    }
    let found = arena
        .alloc_slice_with(literals.len(), |_| None)
        .map_err(|_| super::arena_full())?;
    let mark = arena.mark();
    let mut count = 0;
    let mut visit = |expression: &Expr<'_>, target, type_mod, kind| {
        let expression = match expression {
            Expr::Cast { operand, .. } => *operand,
            expression => expression,
        };
        let Expr::Str(value) = expression else {
            return Ok(());
        };
        let Some(index) = literals
            .iter()
            .position(|(expression, _)| match expression {
                Expr::Str(source) => {
                    value.len() == source.len() && core::ptr::eq(value.as_ptr(), source.as_ptr())
                }
                _ => false,
            })
        else {
            // A stored source owns its own admitted inputs, rather than the
            // raw statement currently being analyzed.
            return Ok(());
        };
        if let Some(previous) = found[..count]
            .iter()
            .flatten()
            .find(|(previous, _, _, previous_kind)| *previous == index && *previous_kind == kind)
        {
            if *previous != (index, target, type_mod, kind) {
                return Err(sql_err!(
                    super::sqlstate::DATATYPE_MISMATCH,
                    "SQL input occurrence has conflicting destination types"
                ));
            }
            return Ok(());
        }
        found[count] = Some((index, target, type_mod, kind));
        count += 1;
        Ok(())
    };
    let result = discover(&mut visit);
    // Only copied source ordinals and type identities escape schema scratch.
    unsafe { arena.rewind_to(mark) };
    result?;
    let keys = arena
        .alloc_slice_with(count, |_| (0usize, 0usize, InputKind::ExplicitCast))
        .map_err(|_| super::arena_full())?;
    for (key, (index, target, type_mod, kind)) in keys
        .iter_mut()
        .zip(found[..count].iter().flatten().copied())
    {
        let source = literals[index].1;
        if kind == InputKind::ImplicitLiteral {
            parser.add_contextual_type_input(literals[index].0, target, type_mod)?;
            *key = (source.start, source.end, kind);
        } else {
            let candidate = parser
                .type_input_candidates()
                .iter()
                .find(|candidate| candidate.start == source.start && candidate.kind == kind)
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::DATA_EXCEPTION,
                        "explicit SQL input occurrence is missing"
                    )
                })?;
            *key = (candidate.start, candidate.end, kind);
        }
    }
    parser.reorder_type_input_candidates(keys)?;
    Ok(())
}

fn discover_defaults(
    parser: &crate::sql::parser::Parser<'_>,
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut Visit<'_>,
) -> Result<(), SqlError> {
    let table = match statement {
        Stmt::CreateTable(table) => Some(table),
        Stmt::CreateForeignTable(table) => Some(&table.relation),
        _ => None,
    };
    if let Some(table) = table {
        for column in table.columns {
            let Some(source) = column.default_text else {
                continue;
            };
            let target = crate::sql::exec::resolve_routine_type(storage, txid, column.type_name)?;
            let target = storage
                .routine_type_oid(target.ctype, target.user_type, txid)
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::UNDEFINED_OBJECT,
                        "default input type is unavailable"
                    )
                })?;
            discover_default_source(
                parser,
                source,
                target,
                column.type_mod,
                storage,
                txid,
                arena,
                visit,
            )?;
        }
        return Ok(());
    }
    // A view's column defaults, written as ALTER VIEW's own action or as
    // ALTER TABLE subcommands on a view.
    let view_defaults = match statement {
        Stmt::AlterView {
            name,
            action: ast::AlterViewAction::SetDefault { column, expression },
            ..
        } => {
            return discover_view_default(
                parser, name, column, expression, storage, txid, arena, visit,
            );
        }
        Stmt::AlterView {
            name,
            action: ast::AlterViewAction::TableActions(actions),
            ..
        } => Some((name, *actions)),
        Stmt::AlterTable(alter) => Some((&alter.table, alter.actions)),
        _ => None,
    };
    if let Some((name, actions)) = view_defaults {
        if matches!(
            storage.resolve_relation(name.schema, name.name, txid),
            Some(crate::storage::ResolvedRelation::View(_))
        ) {
            for action in actions {
                if let ast::AlterAction::SetDefault {
                    column, value_text, ..
                } = action
                {
                    discover_view_default(
                        parser, name, column, value_text, storage, txid, arena, visit,
                    )?;
                }
            }
        }
        return Ok(());
    }
    let (source, target, type_mod) = match statement {
        Stmt::CreateDomain(domain) => {
            let Some(source) = domain.default_text else {
                return Ok(());
            };
            let target = crate::sql::exec::resolve_routine_type(storage, txid, domain.base_type)?;
            (
                source,
                storage.routine_type_oid(target.ctype, target.user_type, txid),
                domain.base_type_mod,
            )
        }
        Stmt::AlterDomain {
            name,
            action: ast::AlterDomainAction::SetDefault(source),
        } => {
            let name = match name.schema {
                Some(schema) => crate::stack_format!(130, "{}.{}", schema, name.name),
                None => crate::util::StackStr::from_str(name.name),
            };
            let Some(domain) = storage.find_domain(name.as_str(), txid) else {
                return Ok(());
            };
            (
                *source,
                storage.routine_type_oid(
                    domain.base,
                    domain.base_domain.or(domain.base_user_type),
                    txid,
                ),
                domain.base_type_mod,
            )
        }
        Stmt::AlterView {
            name,
            action: ast::AlterViewAction::SetDefault { column, expression },
            ..
        } => {
            let Some(crate::storage::ResolvedRelation::View(slot)) =
                storage.resolve_relation(name.schema, name.name, txid)
            else {
                return Ok(());
            };
            let view = storage.view(slot);
            let Some(index) = view
                .columns_for(txid)
                .names()
                .iter()
                .position(|name| name.as_str() == *column)
            else {
                return Ok(());
            };
            let descriptions = arena
                .alloc_slice_with(view.columns_for(txid).len(), |_| {
                    crate::sql::types::ColDesc::new("", 0, 0)
                })
                .map_err(|_| super::arena_full())?;
            let count = crate::sql::catalog::describe_view(&view, txid, arena, descriptions)?;
            let description = descriptions
                .get(index)
                .filter(|_| index < count)
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::INTERNAL_ERROR,
                        "view default output type is missing"
                    )
                })?;
            (
                *expression,
                Some(description.type_oid),
                description.type_mod,
            )
        }
        _ => return Ok(()),
    };
    let Some(target) = target else {
        return Err(sql_err!(
            super::sqlstate::UNDEFINED_OBJECT,
            "default input type is unavailable"
        ));
    };
    discover_default_source(
        parser, source, target, type_mod, storage, txid, arena, visit,
    )
}

/// A view column default's checked inputs, typed by the view's output column.
#[expect(
    clippy::too_many_arguments,
    reason = "default discovery keeps its original source, view column, catalog and admission callback explicit"
)]
fn discover_view_default(
    parser: &crate::sql::parser::Parser<'_>,
    name: &ast::QualName<'_>,
    column: &str,
    expression: &str,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut Visit<'_>,
) -> Result<(), SqlError> {
    let Some(crate::storage::ResolvedRelation::View(slot)) =
        storage.resolve_relation(name.schema, name.name, txid)
    else {
        return Ok(());
    };
    let view = storage.view(slot);
    let Some(index) = view
        .columns_for(txid)
        .names()
        .iter()
        .position(|name| name.as_str() == column)
    else {
        return Ok(());
    };
    let descriptions = arena
        .alloc_slice_with(view.columns_for(txid).len(), |_| {
            crate::sql::types::ColDesc::new("", 0, 0)
        })
        .map_err(|_| super::arena_full())?;
    let count = crate::sql::catalog::describe_view(&view, txid, arena, descriptions)?;
    let description = descriptions
        .get(index)
        .filter(|_| index < count)
        .ok_or_else(|| {
            sql_err!(
                super::sqlstate::INTERNAL_ERROR,
                "view default output type is missing"
            )
        })?;
    discover_default_source(
        parser,
        expression,
        description.type_oid,
        description.type_mod,
        storage,
        txid,
        arena,
        visit,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "default discovery keeps its original source, declared type, catalog and admission callback explicit"
)]
fn discover_default_source(
    parser: &crate::sql::parser::Parser<'_>,
    source: &str,
    target: i32,
    type_mod: i32,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut Visit<'_>,
) -> Result<(), SqlError> {
    let (offset, _) = parser.source_range(source).ok_or_else(|| {
        sql_err!(
            super::sqlstate::INTERNAL_ERROR,
            "default SQL has no original source occurrence"
        )
    })?;
    let syntax = crate::sql::parser::type_input_sources(
        source,
        crate::sql::parser::StoredSqlKind::Expression,
        arena,
    )?;
    let expression = syntax
        .expression
        .expect("expression source parser retains its root");
    let literals = parser.string_literals();
    let mut translate = |expression: &Expr<'_>, oid, type_mod, kind| {
        let expression = match expression {
            Expr::Cast { operand, .. } => *operand,
            expression => expression,
        };
        let Expr::Str(value) = expression else {
            return Ok(());
        };
        let site = syntax.literals.iter().find(|(expression, _)| matches!(expression, Expr::Str(source) if source.len() == value.len() && core::ptr::eq(source.as_ptr(), value.as_ptr())))
            .ok_or_else(|| sql_err!(super::sqlstate::INTERNAL_ERROR, "default literal has no parser source"))?.1;
        let original = literals
            .iter()
            .find(|(_, source)| {
                source.start == offset + site.start && source.end == offset + site.end
            })
            .ok_or_else(|| {
                sql_err!(
                    super::sqlstate::INTERNAL_ERROR,
                    "default literal has no command source"
                )
            })?
            .0;
        visit(original, oid, type_mod, kind)
    };
    let columns = crate::sql::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    let mut walker = Walker {
        storage,
        txid,
        arena,
        visit: &mut translate,
        planner: PlannerPass::TypeInputs,
    };
    walker.expression(expression, &columns, &crate::sql::eval::NoColumns)?;
    walker.input(expression, target, type_mod)
}

fn discover(
    statement: &Stmt<'_>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    visit: &mut Visit<'_>,
) -> Result<(), SqlError> {
    let mut walker = Walker {
        storage,
        txid,
        arena,
        visit,
        planner: PlannerPass::TypeInputs,
    };
    walker.statement(statement, None)
}

/// Planning visits selected expression calls with schema metadata only. The
/// callback must copy its facts before the scoped column resolver is dropped.
pub(crate) fn visit_planner_calls<'a>(
    statement: &Stmt<'a>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    outer: Option<&dyn ColumnLookup<'a>>,
    visit: &mut PlannerVisit<'_>,
) -> Result<(), SqlError> {
    arena
        .with_remaining_region(|scratch| {
            crate::sql::exec::with_temporary_record_shapes(|| {
                let catalog = super::storage_catalog(storage, scratch, txid);
                let empty = crate::sql::eval::NoColumns;
                let outer = outer.unwrap_or(&empty);
                let columns = RuntimeColumnTypes {
                    row: outer,
                    catalog: Some(&catalog),
                };
                let metadata = MetadataOnly {
                    row: outer,
                    columns: &columns,
                };
                let mut ignore = |_: &Expr<'_>, _: i32, _: i32, _: InputKind| Ok(());
                Walker {
                    storage,
                    txid,
                    arena: scratch,
                    visit: &mut ignore,
                    planner: PlannerPass::Analyze,
                }
                .planner_statement(statement, &metadata)
            })
        })
        .map_err(|_| super::arena_full())??;
    let catalog = super::storage_catalog(storage, arena, txid);
    let empty = crate::sql::eval::NoColumns;
    let outer = outer.unwrap_or(&empty);
    let columns = RuntimeColumnTypes {
        row: outer,
        catalog: Some(&catalog),
    };
    let metadata = MetadataOnly {
        row: outer,
        columns: &columns,
    };
    let mut ignore = |_: &Expr<'_>, _: i32, _: i32, _: InputKind| Ok(());
    Walker {
        storage,
        txid,
        arena,
        visit: &mut ignore,
        planner: PlannerPass::Capture(visit),
    }
    .planner_statement(statement, &metadata)
}

pub(crate) fn visit_planner_expression<'a>(
    expression: &Expr<'a>,
    storage: &Storage,
    txid: u32,
    arena: &Arena,
    row: &dyn ColumnLookup<'a>,
    visit: &mut PlannerVisit<'_>,
) -> Result<(), SqlError> {
    arena
        .with_remaining_region(|scratch| {
            crate::sql::exec::with_temporary_record_shapes(|| {
                let catalog = super::storage_catalog(storage, scratch, txid);
                let columns = RuntimeColumnTypes {
                    row,
                    catalog: Some(&catalog),
                };
                let metadata = MetadataOnly {
                    row,
                    columns: &columns,
                };
                let mut ignore = |_: &Expr<'_>, _: i32, _: i32, _: InputKind| Ok(());
                Walker {
                    storage,
                    txid,
                    arena: scratch,
                    visit: &mut ignore,
                    planner: PlannerPass::Analyze,
                }
                .expression(expression, &columns, &metadata)
            })
        })
        .map_err(|_| super::arena_full())??;
    let catalog = super::storage_catalog(storage, arena, txid);
    let columns = RuntimeColumnTypes {
        row,
        catalog: Some(&catalog),
    };
    let metadata = MetadataOnly {
        row,
        columns: &columns,
    };
    let mut ignore = |_: &Expr<'_>, _: i32, _: i32, _: InputKind| Ok(());
    Walker {
        storage,
        txid,
        arena,
        visit: &mut ignore,
        planner: PlannerPass::Capture(visit),
    }
    .expression(expression, &columns, &metadata)
}

enum PlannerPass<'v, 'callback> {
    TypeInputs,
    Analyze,
    Capture(&'v mut PlannerVisit<'callback>),
}

/// The output list PostgreSQL's planner gives an `EXISTS` query it
/// simplifies.
const EXISTS_OUTPUT: [ast::SelectItem<'static>; 1] = [ast::SelectItem::Expr {
    expression: &Expr::Int(1),
    alias: None,
}];

/// Whether PostgreSQL's `simplify_EXISTS_query` replaces this `EXISTS`
/// query's output list: a plain query without grouping, aggregates, window
/// functions, set-returning output, set operations, `LIMIT` or `OFFSET`.
fn exists_discards_output(select: &ast::Select<'_>, storage: &Storage, txid: u32) -> bool {
    select.set_body.is_none()
        && select.group_by.is_empty()
        && select.having.is_none()
        && select.limit.is_none()
        && select.offset.is_none()
        && select.locking.is_empty()
        && select.items.iter().all(|item| match item {
            ast::SelectItem::Expr { expression, .. } | ast::SelectItem::RecordStar(expression) => {
                !super::expr_has_window(expression) && !contains_aggregate_call(expression)
            }
            ast::SelectItem::Wildcard(_) | ast::SelectItem::TableWildcard(_) => true,
        })
        && !super::srf::has_project_set(select.items, storage, txid)
}

/// Whether an expression calls an aggregate, outside its own subqueries.
fn contains_aggregate_call(expression: &Expr<'_>) -> bool {
    if expression.is_aggregate_use() {
        return true;
    }
    let mut found = false;
    let _ = super::walk_children(expression, &mut |child| {
        found |= contains_aggregate_call(child);
        Ok(())
    });
    found
}

struct Walker<'a, 'v, 'callback> {
    storage: &'a Storage,
    txid: u32,
    arena: &'a Arena,
    visit: &'v mut Visit<'callback>,
    planner: PlannerPass<'v, 'callback>,
}

impl<'storage, 'v, 'callback> Walker<'storage, 'v, 'callback> {
    fn planner_statement<'a>(
        &mut self,
        statement: &'a Stmt<'a>,
        outer: &dyn ColumnLookup<'a>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        self.statement(statement, Some(outer))
    }

    /// A `CALL`'s arguments, typed by `columns` and read from `row`.
    fn call_statement<'a>(
        &mut self,
        statement: &'a Stmt<'a>,
        columns: &dyn ColTypeResolver,
        row: &dyn ColumnLookup<'a>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        let Stmt::Call {
            name,
            arguments,
            argument_names,
            variadic,
            ..
        } = statement
        else {
            return Ok(());
        };
        let name = match name.schema {
            Some(schema) => self
                .arena
                .alloc_str_display(format_args!("{schema}.{}", name.name))
                .map_err(|_| super::arena_full())?,
            None => name.name,
        };
        for argument in *arguments {
            self.expression(argument, columns, row)?;
        }
        self.call(
            name,
            None,
            arguments,
            argument_names,
            *variadic,
            true,
            columns,
        )
    }

    fn input(&mut self, expression: &Expr<'_>, target: i32, type_mod: i32) -> Result<(), SqlError> {
        if matches!(self.planner, PlannerPass::Analyze) {
            return Ok(());
        }
        if matches!(expression, Expr::Str(_))
            && matches!(
                NamedTypeKind::from_oid(target),
                Some((NamedTypeKind::Domain, _, true))
            )
        {
            (self.visit)(expression, target, type_mod, InputKind::ImplicitLiteral)?;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "routine input analysis preserves named/variadic syntax, procedure kind and declared columns"
    )]
    fn call(
        &mut self,
        name: &str,
        bound_oid: Option<i32>,
        arguments: &[&Expr<'_>],
        names: &[Option<&str>],
        variadic: bool,
        procedure: bool,
        columns: &dyn ColTypeResolver,
    ) -> Result<(), SqlError> {
        if !arguments
            .iter()
            .any(|argument| matches!(argument, Expr::Str(_)))
        {
            return Ok(());
        }
        let has_stored_name = (0..self.storage.routine_count()).any(|slot| {
            let Some(identity) = self.storage.routine_identity_for(slot, self.txid) else {
                return false;
            };
            if let Some(oid) = bound_oid {
                return identity.oid == oid;
            }
            name == identity.name.as_str()
                || name
                    .strip_suffix(identity.name.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.'))
        });
        if !has_stored_name {
            return Ok(());
        }
        if arguments.len() > crate::storage::MAX_ROUTINE_ARGUMENTS {
            return Err(sql_err!(
                super::sqlstate::TOO_MANY_ARGUMENTS,
                "routine call has too many arguments"
            ));
        }
        let mut types = [oid::UNKNOWN; crate::storage::MAX_ROUTINE_ARGUMENTS];
        for (ty, argument) in types.iter_mut().zip(arguments) {
            *ty = crate::sql::exec::infer_routine_argument_oid(argument, columns)?;
        }
        let selection = match bound_oid {
            Some(oid) => self
                .storage
                .function_for_captured_call(
                    oid,
                    names,
                    &types[..arguments.len()],
                    variadic,
                    self.txid,
                )
                .and_then(|routine| {
                    self.storage
                        .routine_slot_by_oid(crate::storage::routine_oid(&routine), self.txid)
                })
                .map(RoutineCallSelection::Stored),
            None => Some(self.storage.select_routine_call(
                name,
                names,
                &types[..arguments.len()],
                variadic,
                self.txid,
                if procedure {
                    RoutineCallKind::Procedure
                } else {
                    RoutineCallKind::Function
                },
            )),
        };
        let Some(RoutineCallSelection::Stored(slot)) = selection else {
            return Ok(());
        };
        let routine = self.storage.routine_for(slot, self.txid);
        let routine = if procedure {
            routine
        } else {
            self.storage
                .function_for_captured_call(
                    crate::storage::routine_oid(&routine),
                    names,
                    &types[..arguments.len()],
                    variadic,
                    self.txid,
                )
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::INTERNAL_ERROR,
                        "selected routine input contract is unavailable"
                    )
                })?
        };
        let mapping = if procedure {
            routine.procedure_call_mapping(names, arguments.len(), variadic)
        } else {
            self.storage
                .routine_call_mapping(slot, names, arguments.len(), variadic, self.txid)
                .map(|(_, mapping)| mapping)
        }
        .ok_or_else(|| {
            sql_err!(
                super::sqlstate::INTERNAL_ERROR,
                "selected routine input mapping is unavailable"
            )
        })?;
        for (written, expression) in arguments.iter().enumerate() {
            let parameter = usize::from(mapping[written]);
            if parameter == usize::from(u8::MAX) {
                continue;
            }
            let argument = routine.arguments().get(parameter).ok_or_else(|| {
                sql_err!(
                    super::sqlstate::INTERNAL_ERROR,
                    "selected routine input is unavailable"
                )
            })?;
            let mode = routine
                .parameters()
                .iter()
                .filter(|parameter| parameter.mode.is_input())
                .nth(parameter)
                .map(|parameter| parameter.mode);
            if !variadic
                && matches!(
                    mode,
                    Some(crate::storage::RoutineParameterMode::Variadic { .. })
                )
            {
                continue;
            }
            let target = self
                .storage
                .routine_type_oid(argument.ctype, argument.user_type, self.txid)
                .ok_or_else(|| {
                    sql_err!(
                        super::sqlstate::UNDEFINED_OBJECT,
                        "routine input type is unavailable"
                    )
                })?;
            self.input(expression, target, -1)?;
        }
        Ok(())
    }

    fn expression<'a>(
        &mut self,
        expression: &Expr<'a>,
        columns: &dyn ColTypeResolver,
        row: &dyn ColumnLookup<'a>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        if matches!(self.planner, PlannerPass::Capture(_)) {
            use crate::sql::planner_inputs::inline::ConstantValue;
            let storage = self.storage;
            let txid = self.txid;
            let arena = self.arena;
            let constant = |expression| {
                crate::sql::planner_inputs::inline::constant_value(
                    expression, columns, storage, txid, arena,
                )
            };
            if let Expr::Binary {
                operator: operator @ (ast::BinaryOp::And | ast::BinaryOp::Or),
                left,
                right,
                ..
            } = expression
            {
                self.expression(left, columns, row)?;
                if !matches!(constant(left)?, Some(ConstantValue::Boolean(value)) if value == (*operator == ast::BinaryOp::Or))
                {
                    self.expression(right, columns, row)?;
                }
                return Ok(());
            }
            if let Expr::Case {
                operand,
                whens,
                otherwise,
                ..
            } = expression
            {
                if let Some(operand) = operand {
                    self.expression(operand, columns, row)?;
                }
                for (condition, result) in *whens {
                    self.expression(condition, columns, row)?;
                    let folded = if let Some(operand) = operand {
                        let comparison = Expr::Binary {
                            source_position: ast::SourcePosition::UNKNOWN,
                            operator: ast::BinaryOp::Eq,
                            left: operand,
                            right: condition,
                        };
                        crate::sql::planner_inputs::inline::constant_value(
                            &comparison,
                            columns,
                            storage,
                            txid,
                            arena,
                        )?
                    } else {
                        constant(condition)?
                    };
                    match folded {
                        Some(ConstantValue::Boolean(false) | ConstantValue::Null) => continue,
                        Some(ConstantValue::Boolean(true)) => {
                            return self.expression(result, columns, row);
                        }
                        _ => self.expression(result, columns, row)?,
                    }
                }
                if let Some(otherwise) = otherwise {
                    self.expression(otherwise, columns, row)?;
                }
                return Ok(());
            }
            if let Expr::Call {
                kind: ast::CallKind::SqlSyntax,
                name: "coalesce",
                args,
                ..
            } = expression
            {
                for argument in *args {
                    self.expression(argument, columns, row)?;
                    if constant(argument)?.is_some_and(|value| value != ConstantValue::Null) {
                        break;
                    }
                }
                return Ok(());
            }
        }
        match expression {
            // PostgreSQL's planner discards a simple `EXISTS` query's output
            // list before planning it, so its constants are never folded.
            Expr::Exists(select)
                if matches!(self.planner, PlannerPass::Capture(_))
                    && exists_discards_output(select, self.storage, self.txid) =>
            {
                let simplified = self
                    .arena
                    .alloc(ast::Select {
                        items: &EXISTS_OUTPUT,
                        ..**select
                    })
                    .map_err(|_| super::arena_full())?;
                return self.select(simplified, Some(row), None);
            }
            Expr::Subquery(select)
            | Expr::Exists(select)
            | Expr::ArraySubquery(select)
            | Expr::RowSubquery { select, .. } => return self.select(select, Some(row), None),
            Expr::InSubquery {
                operand, select, ..
            }
            | Expr::QuantifiedSubquery {
                operand, select, ..
            } => {
                self.expression(operand, columns, row)?;
                return self.select(select, Some(row), None);
            }
            _ => {}
        }
        super::walk_children(expression, &mut |child| {
            self.expression(child, columns, row)
        })?;
        if matches!(self.planner, PlannerPass::Analyze) {
            if matches!(
                expression,
                Expr::Call {
                    kind: ast::CallKind::Routine,
                    ..
                }
            ) {
                super::selected_aggregate_call(expression, self.storage, self.txid, columns)?;
            }
            if matches!(
                expression,
                Expr::Call { .. }
                    | Expr::Cast { .. }
                    | Expr::Binary { .. }
                    | Expr::Unary { .. }
                    | Expr::Case { .. }
                    | Expr::InList { .. }
                    | Expr::AnyAll { .. }
            ) {
                crate::sql::exec::infer_type_res(expression, columns)?;
            }
            return Ok(());
        }
        if matches!(self.planner, PlannerPass::Capture(_))
            && matches!(
                expression,
                Expr::Binary { .. }
                    | Expr::Unary { .. }
                    | Expr::Call {
                        kind: ast::CallKind::Routine,
                        ..
                    }
            )
        {
            crate::sql::planner_inputs::inline::constant_value(
                expression,
                columns,
                self.storage,
                self.txid,
                self.arena,
            )?;
        }
        if let PlannerPass::Capture(planner) = &mut self.planner {
            if matches!(
                expression,
                Expr::Call {
                    kind: ast::CallKind::Routine,
                    ..
                } | Expr::Cast { .. }
                    | Expr::Binary { .. }
                    | Expr::Unary { .. }
            ) {
                planner(expression, columns, None)?;
            } else if let Expr::Call {
                kind: ast::CallKind::SqlSyntax,
                name,
                ..
            } = expression
                && ast::catalog_operator_call(name).is_some()
            {
                planner(expression, columns, None)?;
            }
            return Ok(());
        }
        if let Expr::Cast {
            operand: Expr::Str(_),
            type_name,
            type_mod,
            ..
        } = expression
            && let Some(target) =
                crate::sql::type_inputs::target_oid(self.storage, self.txid, type_name)
        {
            (self.visit)(expression, target, *type_mod, InputKind::ExplicitCast)?;
        }
        if let Expr::Call {
            kind: ast::CallKind::Routine,
            name,
            bound_oid,
            args,
            argument_names,
            variadic,
            ..
        } = expression
        {
            self.call(
                name,
                *bound_oid,
                args,
                argument_names,
                *variadic,
                false,
                columns,
            )?;
        }
        Ok(())
    }

    /// Visits a FROM item. Its function arguments read `row`; a derived
    /// table reads `lateral` when marked `LATERAL`, otherwise `hidden`.
    fn table_ref<'a>(
        &mut self,
        table: &ast::TableRef<'a>,
        columns: &dyn ColTypeResolver,
        row: &dyn ColumnLookup<'a>,
        lateral: &dyn ColumnLookup<'a>,
        hidden: &dyn ColumnLookup<'a>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        // A plain derived table reads only the levels around its query; a
        // LATERAL one also reads the tables beside it.
        if let Some(query) = table.subquery {
            let outer = if table.lateral { lateral } else { hidden };
            self.select(query, Some(outer), None)?;
        }
        if let Some(arguments) = table.func_args {
            for expression in arguments {
                self.expression(expression, columns, row)?;
            }
            let name = match table.schema {
                Some(schema) => self
                    .arena
                    .alloc_str_display(format_args!("{schema}.{}", table.table))
                    .map_err(|_| super::arena_full())?,
                None => table.table,
            };
            if let PlannerPass::Capture(planner) = &mut self.planner {
                planner(
                    &Expr::Call {
                        kind: ast::CallKind::Routine,
                        name,
                        bound_oid: None,
                        bound_strict: None,
                        source_position: table.func_source_position,
                        args: arguments,
                        argument_names: table.func_argument_names,
                        variadic: table.func_variadic,
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    },
                    columns,
                    None,
                )?;
            } else if matches!(self.planner, PlannerPass::TypeInputs) {
                self.call(
                    name,
                    None,
                    arguments,
                    table.func_argument_names,
                    table.func_variadic,
                    false,
                    columns,
                )?;
            }
        }
        if let Some(group) = table.rows_from {
            for table in group {
                self.table_ref(table, columns, row, lateral, hidden)?;
            }
        }
        Ok(())
    }

    fn select<'a>(
        &mut self,
        select: &'a Select<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
        targets: Option<&[(i32, i32, Option<crate::sql::type_inputs::AssignmentSite>)]>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        let arena = self.arena;
        let mark = arena.mark();
        let result = self.select_in_scope(select, outer, targets);
        // Callbacks retain copied source identities or persistent facts only.
        // The resolver and its schema owners have finished before reclamation.
        unsafe { arena.rewind_to(mark) };
        result
    }

    fn select_in_scope<'a>(
        &mut self,
        select: &'a Select<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
        targets: Option<&[(i32, i32, Option<crate::sql::type_inputs::AssignmentSite>)]>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        let select = super::expand_ctes(select, self.storage, self.txid, self.arena)?;
        if let Some(tree) = select.set_body {
            self.set_tree(tree, outer)?;
            let storage = self.storage;
            let txid = self.txid;
            let arena = self.arena;
            if let Some(targets) = targets
                && let PlannerPass::Capture(planner) = &mut self.planner
            {
                let mark = arena.mark();
                let result: Result<(), SqlError> = (|| {
                    let output = super::srf::describe_derived_definition(
                        storage, select, "", None, txid, arena, outer,
                    )?;
                    // Set outputs can repeat names such as ?column?. Bind by
                    // ordinal so one output never acquires a sibling's type.
                    let output_columns = arena
                        .alloc_slice_with(output.n_columns, |index| output.columns()[index])
                        .map_err(|_| super::arena_full())?;
                    for (index, column) in output_columns.iter_mut().enumerate() {
                        column.name = crate::storage::SqlName::parse(
                            crate::stack_format!(32, "output{index}").as_str(),
                        )?;
                    }
                    let output = crate::storage::RelationDefinition {
                        schema: output.schema,
                        name: output.name,
                        n_columns: output.n_columns,
                        columns: output_columns,
                    };
                    let columns = crate::sql::exec::CatalogCols {
                        definition: Some(output),
                        alias: None,
                        output_aliases: &[],
                        storage,
                        txid,
                        arena,
                    };
                    for (index, column) in output.columns().iter().enumerate() {
                        if let Some((target, type_mod, Some(site))) = targets.get(index) {
                            planner(
                                &Expr::Column {
                                    qualifier: None,
                                    name: column.name.as_str(),
                                },
                                &columns,
                                Some((*target, *type_mod, *site)),
                            )?;
                        }
                    }
                    Ok(())
                })();
                // Callbacks retain only detached facts in the persistent tail.
                unsafe { arena.rewind_to(mark) };
                result?;
            }
        }
        let scope = match select.from.as_ref() {
            Some(from) => Some(super::QueryScope::resolve_schema(
                self.storage,
                from,
                self.txid,
                self.arena,
                outer,
            )?),
            None if matches!(self.planner, PlannerPass::Analyze) => Some(
                super::QueryScope::singleton(self.storage, self.txid, self.arena)?,
            ),
            None => None,
        };
        let schema = scope.as_ref().map(super::materialize::ScopeSchema);
        let empty = crate::sql::eval::NoColumns;
        let inner: &dyn ColumnLookup<'_> = schema
            .as_ref()
            .map_or(&empty as &dyn ColumnLookup<'_>, |schema| schema);
        let row = super::Chained { inner, outer };
        let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
        let columns = RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        if let Some(from) = select.from {
            let scope = scope.as_ref().expect("a FROM clause resolves a scope");
            let reach =
                |preceding, lateral: bool, reads_left: bool| super::scope::DerivedTableOuter {
                    siblings: scope,
                    preceding,
                    visible_from: if lateral && reads_left {
                        scope.lateral_from
                    } else {
                        preceding
                    },
                    lateral,
                    enclosing: outer,
                };
            self.table_ref(
                &from.base,
                &columns,
                &row,
                &reach(0, true, true),
                &reach(0, false, true),
            )?;
            for (index, join) in from.joins.iter().enumerate() {
                let reads_left = !join.kind.is_right_or_full();
                self.table_ref(
                    &join.table,
                    &columns,
                    &row,
                    &reach(index + 1, true, reads_left),
                    &reach(index + 1, false, reads_left),
                )?;
                if let Some(expression) = join.on {
                    self.expression(expression, &columns, &row)?;
                }
            }
        }
        let mut output_index = 0usize;
        let mut target_inputs = crate::mem::arena::ArenaList::new(self.arena);
        for item in select.items {
            if matches!(self.planner, PlannerPass::Analyze) {
                super::validate_projection_source(
                    item,
                    scope
                        .as_ref()
                        .expect("analysis owns a source or singleton scope"),
                )?;
                if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) =
                    item
                {
                    crate::sql::exec::infer_type_res(expression, &columns)?;
                }
            }
            if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) = item {
                if matches!(item, SelectItem::Expr { .. })
                    && let Some((target, type_mod, _)) =
                        targets.and_then(|targets| targets.get(output_index))
                    && matches!(expression, Expr::Str(_))
                    && matches!(
                        NamedTypeKind::from_oid(*target),
                        Some((NamedTypeKind::Domain, _, true))
                    )
                {
                    target_inputs
                        .push((*expression, *target, *type_mod))
                        .map_err(|_| super::arena_full())?;
                }
                self.expression(expression, &columns, &row)?;
                if matches!(item, SelectItem::Expr { .. })
                    && let Some((target, type_mod, Some(site))) =
                        targets.and_then(|targets| targets.get(output_index))
                    && let PlannerPass::Capture(planner) = &mut self.planner
                {
                    planner(expression, &columns, Some((*target, *type_mod, *site)))?;
                }
            }
            if targets.is_some() {
                let width = match item {
                    SelectItem::Expr { .. } => 1,
                    SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                        let scope = scope.as_ref().ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_TABLE,
                                "SELECT * has no source relation"
                            )
                        })?;
                        let count = match item {
                            SelectItem::TableWildcard(name) => {
                                scope.qualified_star_columns(name)?
                            }
                            _ => scope.star_columns(),
                        };
                        if let PlannerPass::Capture(planner) = &mut self.planner {
                            for index in 0..count {
                                let entry = match item {
                                    SelectItem::TableWildcard(name) => {
                                        scope.qualified_star_entry(name, index)?
                                    }
                                    _ => scope.star_entry(index),
                                };
                                let expression = match entry {
                                    super::ResolvedColumn::Table(table, column) => {
                                        let (qualifier, name) =
                                            scope.column_reference_name(table, column);
                                        Expr::Column {
                                            qualifier: Some(qualifier),
                                            name,
                                        }
                                    }
                                    super::ResolvedColumn::Merged(_)
                                    | super::ResolvedColumn::TableOid(_) => Expr::Column {
                                        qualifier: None,
                                        name: scope.output_name(entry),
                                    },
                                };
                                if let Some((target, type_mod, Some(site))) =
                                    targets.and_then(|targets| targets.get(output_index + index))
                                {
                                    planner(
                                        &expression,
                                        &columns,
                                        Some((*target, *type_mod, *site)),
                                    )?;
                                }
                            }
                        }
                        count
                    }
                    SelectItem::RecordStar(expression) => {
                        let mut index = output_index;
                        let mut result = Ok(());
                        let width = crate::sql::exec::record_shape_metadata(
                            expression,
                            &columns,
                            |name, _| {
                                if result.is_ok()
                                    && let PlannerPass::Capture(planner) = &mut self.planner
                                    && let Some((target, type_mod, Some(site))) =
                                        targets.and_then(|targets| targets.get(index))
                                {
                                    result = planner(
                                        &Expr::Field {
                                            base: expression,
                                            field: name,
                                        },
                                        &columns,
                                        Some((*target, *type_mod, *site)),
                                    );
                                }
                                index += 1;
                            },
                        )?;
                        result?;
                        width.ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::FEATURE_NOT_SUPPORTED,
                                "record output shape is unavailable"
                            )
                        })?
                    }
                };
                output_index = output_index
                    .checked_add(width)
                    .ok_or_else(super::arena_full)?;
            }
        }
        for expression in select
            .where_clause
            .into_iter()
            .chain(select.having)
            .chain(select.order_by.iter().map(|order| order.expression))
            .chain(select.group_by.iter().copied())
            .chain(select.distinct_on.iter().copied())
            .chain(select.offset)
            .chain(select.limit)
        {
            self.expression(expression, &columns, &row)?;
        }
        for (expression, target, type_mod) in target_inputs.as_slice() {
            self.input(expression, *target, *type_mod)?;
        }
        Ok(())
    }

    fn set_query<'a>(
        &mut self,
        query: &'a ast::SetQuery<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        let storage = self.storage;
        let txid = self.txid;
        let arena = self.arena;
        let mark = arena.mark();
        let result = (|| {
            let planner = !matches!(self.planner, PlannerPass::TypeInputs);
            super::cte::visit_cte_sources(
                query.with,
                storage,
                txid,
                None,
                None,
                arena,
                &mut |source| match source {
                    super::cte::ScopedCte::Select(select) if !planner => {
                        self.select(select, outer, None)
                    }
                    super::cte::ScopedCte::Select(_) => Ok(()),
                    super::cte::ScopedCte::DataModification(statement) => {
                        self.statement(statement, outer)
                    }
                },
            )?;
            let tree = super::cte::expand_set_tree(
                query.with, query.body, storage, txid, None, None, arena,
            )?;
            self.set_tree(tree, outer)
        })();
        // Selected facts are copied before the shared CTE shapes are reclaimed.
        unsafe { arena.rewind_to(mark) };
        result
    }

    fn set_tree<'a>(
        &mut self,
        tree: &'a ast::SetTree<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        match tree {
            ast::SetTree::Select(select) => self.select(select, outer, None),
            ast::SetTree::Op { left, right, .. } => {
                self.set_tree(left, outer)?;
                self.set_tree(right, outer)
            }
        }
    }

    /// Visits `statement`, whose queries see `outer` (a PL/pgSQL routine's
    /// variables) beyond their own FROM items.
    fn statement<'a>(
        &mut self,
        statement: &'a Stmt<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        match statement {
            Stmt::Select(select) => self.select(select, outer, None),
            Stmt::SetQuery(query) => self.set_query(query, outer),
            Stmt::With { ctes, statement } => {
                let storage = self.storage;
                let txid = self.txid;
                let arena = self.arena;
                let planner = !matches!(self.planner, PlannerPass::TypeInputs);
                super::cte::visit_cte_sources(
                    ctes,
                    storage,
                    txid,
                    None,
                    None,
                    arena,
                    &mut |source| match source {
                        super::cte::ScopedCte::Select(select) if !planner => {
                            self.select(select, outer, None)
                        }
                        super::cte::ScopedCte::Select(_) => Ok(()),
                        super::cte::ScopedCte::DataModification(statement) => {
                            self.statement(statement, outer)
                        }
                    },
                )?;
                let statement = super::cte::expand_dml_ctes_schema(
                    statement,
                    ctes,
                    self.storage,
                    self.txid,
                    None,
                    None,
                    self.arena,
                )?;
                self.statement(statement, outer)
            }
            Stmt::Explain { statement, .. } => self.statement(statement, outer),
            Stmt::Call { .. } => {
                // A procedural CALL's arguments read the block's variables.
                if let Some(outer) = outer {
                    let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
                    let columns = RuntimeColumnTypes {
                        row: outer,
                        catalog: Some(&catalog),
                    };
                    return self.call_statement(statement, &columns, outer);
                }
                let catalog = crate::sql::exec::CatalogCols {
                    definition: None,
                    alias: None,
                    output_aliases: &[],
                    storage: self.storage,
                    txid: self.txid,
                    arena: self.arena,
                };
                self.call_statement(statement, &catalog, &crate::sql::eval::NoColumns)
            }
            Stmt::Insert(_) | Stmt::Update(_) | Stmt::Delete(_) | Stmt::Merge(_) => {
                self.dml(statement, outer)
            }
            _ => Ok(()),
        }
    }

    fn dml<'a>(
        &mut self,
        statement: &'a Stmt<'a>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'storage: 'a,
    {
        match statement {
            // `pg_settings`'s rule runs a query instead of a write.
            Stmt::Update(update) if crate::sql::is_pg_settings(update.table) => {
                let select = crate::sql::pg_settings_rule_query(update, self.arena)?;
                return self.select(select, outer, None);
            }
            Stmt::Insert(insert) if crate::sql::is_pg_settings(insert.table) => return Ok(()),
            Stmt::Delete(delete) if crate::sql::is_pg_settings(delete.table) => return Ok(()),
            _ => {}
        }
        let storage = self.storage;
        let txid = self.txid;
        let arena = self.arena;
        let (target, alias, extra, returning) = match statement {
            Stmt::Insert(insert) => (insert.table, insert.alias, None, insert.returning),
            Stmt::Update(update) => (update.table, update.alias, update.from, update.returning),
            Stmt::Delete(delete) => (delete.table, delete.alias, delete.using, delete.returning),
            Stmt::Merge(merge) => {
                let source = self
                    .arena
                    .alloc(ast::FromClause {
                        base: merge.source,
                        joins: &[],
                    })
                    .map_err(|_| super::arena_full())?;
                (
                    merge.target,
                    merge.target_alias,
                    Some(&*source),
                    merge.returning,
                )
            }
            _ => unreachable!(),
        };
        let mut scope = super::dependencies::dml_dependency_scope(
            self.storage,
            target,
            alias,
            extra,
            self.txid,
            self.arena,
            outer.map(|outer| outer as &dyn ColumnLookup<'_>),
        )?;
        let schema = super::materialize::ScopeSchema(&scope);
        let row = super::Chained {
            inner: &schema,
            outer,
        };
        let catalog = super::storage_catalog(self.storage, self.arena, self.txid);
        let columns = RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        if let Some(from) = extra {
            // The assignment target is entry 0, out of every FROM item's reach.
            let reach = |preceding, lateral: bool| super::scope::DerivedTableOuter {
                siblings: &scope,
                preceding,
                visible_from: if lateral {
                    scope.lateral_from
                } else {
                    preceding
                },
                lateral,
                enclosing: outer,
            };
            self.table_ref(
                &from.base,
                &columns,
                &row,
                &reach(1, true),
                &reach(1, false),
            )?;
            for (index, join) in from.joins.iter().enumerate() {
                self.table_ref(
                    &join.table,
                    &columns,
                    &row,
                    &reach(index + 2, true),
                    &reach(index + 2, false),
                )?;
                if let Some(expression) = join.on {
                    self.expression(expression, &columns, &row)?;
                }
            }
        }
        if let Stmt::Insert(insert) = statement
            && let Some(select) = insert.select
        {
            let definition = scope.defs.get(0).expect("assignment target resolved");
            let count = if insert.columns.is_empty() {
                definition.columns().len()
            } else {
                insert.columns.len()
            };
            let targets = arena
                .alloc_slice_with(count, |_| (0, -1, None))
                .map_err(|_| super::arena_full())?;
            for (index, target) in targets.iter_mut().enumerate() {
                let column_index = if insert.columns.is_empty() {
                    index
                } else {
                    definition
                        .column_index(insert.columns[index])
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_COLUMN,
                                "column \"{}\" does not exist",
                                insert.columns[index]
                            )
                        })?
                };
                let column = &definition.columns()[column_index];
                *target = (
                    storage
                        .routine_type_oid(column.ctype, column.user_type, txid)
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_OBJECT,
                                "assignment input type is unavailable"
                            )
                        })?,
                    column.type_mod,
                    insert.source_position.get().map(|statement_position| {
                        crate::sql::type_inputs::AssignmentSite {
                            statement_position,
                            node: crate::sql::type_inputs::AssignmentNode::SelectOutput {
                                output: index,
                            },
                        }
                    }),
                );
            }
            // The source inherits procedural variables, not the target table's
            // columns. Its own FROM entries still resolve in the inner scope.
            self.select(select, outer, Some(targets))?;
        }
        if let Stmt::Merge(merge) = statement {
            self.expression(merge.on, &columns, &row)?;
        }
        let check_sources = |assignments: &[(&str, &Expr<'_>)]| {
            assignments.iter().try_for_each(|(_, expression)| {
                super::dependencies::check_multiple_assignment_source(
                    expression, &scope, storage, txid, arena,
                )
            })
        };
        match statement {
            Stmt::Update(update) => check_sources(update.assignments)?,
            Stmt::Merge(merge) => {
                for when in merge.whens {
                    if let ast::MergeActionRef::Update(assignments) = when.action() {
                        check_sources(assignments)?;
                    }
                }
            }
            _ => {}
        }
        super::dependencies::visit_resolved_dml_assignment_inputs(
            statement,
            storage,
            txid,
            arena,
            outer,
            &scope,
            true,
            &mut |group, assignment_columns| {
                let row = MetadataOnly {
                    row: &row,
                    columns: assignment_columns,
                };
                for assignment in group {
                    self.expression(assignment.expression, assignment_columns, &row)?;
                }
                for assignment in group {
                    let column = assignment.column;
                    let target = storage
                        .routine_type_oid(column.ctype, column.user_type, txid)
                        .ok_or_else(|| {
                            sql_err!(
                                super::sqlstate::UNDEFINED_OBJECT,
                                "assignment input type is unavailable"
                            )
                        })?;
                    self.input(assignment.expression, target, column.type_mod)?;
                    if let PlannerPass::Capture(planner) = &mut self.planner
                        && let Some(site) = assignment.site
                    {
                        planner(
                            assignment.expression,
                            assignment_columns,
                            Some((target, column.type_mod, site)),
                        )?;
                    }
                }
                Ok(())
            },
        )?;
        let filter = match statement {
            Stmt::Update(update) => update.where_clause,
            Stmt::Delete(delete) => delete.where_clause,
            Stmt::Insert(insert) => insert
                .on_conflict
                .and_then(|conflict| conflict.update_where),
            _ => None,
        };
        if matches!(statement, Stmt::Insert(_)) && filter.is_some() {
            scope.set_target_aliases(&["excluded"], arena)?;
        }
        let schema = super::materialize::ScopeSchema(&scope);
        let row = super::Chained {
            inner: &schema,
            outer,
        };
        let columns = RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        if let Some(expression) = filter {
            self.expression(expression, &columns, &row)?;
        }
        if returning.is_empty() {
            return Ok(());
        }
        scope.set_target_aliases(&[returning.old_name(), returning.new_name()], arena)?;
        let schema = super::materialize::ScopeSchema(&scope);
        let row = super::Chained {
            inner: &schema,
            outer,
        };
        let columns = RuntimeColumnTypes {
            row: &row,
            catalog: Some(&catalog),
        };
        for item in returning.items {
            match item {
                SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                    self.expression(expression, &columns, &row)?;
                }
                SelectItem::TableWildcard(qualifier) => {
                    scope
                        .qualified_star_columns(qualifier)
                        .map_err(|error| crate::sql::error_position::at_item(error, item))?;
                }
                SelectItem::Wildcard(_) => {}
            }
        }
        Ok(())
    }
}
