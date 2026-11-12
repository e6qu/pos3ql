//! Resolving a FROM clause into the scope a query's columns are looked up in.
//!
//! A [`QueryScope`] names every table, derived table, table function and
//! materialized CTE a query reads, in join order, and answers what a column
//! reference means: which of them owns it, whether it is ambiguous, and whether
//! `USING`/`NATURAL` merged it with a column of the same name on the other side
//! (a [`MergedColumn`], which is what `SELECT *` shows and what an unqualified
//! reference resolves to). It is built twice over: a schema-only form for
//! describe, and an executing form that also materializes derived tables.

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{BinaryOp, Expr, FromClause, MaterializedCte, TableRef};
use crate::sql::eval::{ColumnLookup, SequenceAccess, SqlError, sqlstate};
use crate::sql::types::{ColType, Datum};
use crate::sql_err;
use crate::storage::{
    ColumnMeta, MAX_COLUMNS, RelationDefinition, SqlName, Storage, TableDef, TableDefinitionImages,
    UserTypeName,
};
use crate::util::StackStr;

use super::{
    Chained, arena_full, collect_aggs, collect_windows, common_using_type, select_into_rows,
    table_func_def, table_func_def_outer, table_func_rows_outer,
};

fn catalog_scan(
    table: &TableRef<'_>,
    predicate: Option<&Expr<'_>>,
    unqualified: bool,
) -> crate::sql::catalog_snapshot::CatalogScan {
    use crate::sql::catalog_snapshot::{CatalogRowKey, CatalogScan};
    fn constrained(expression: &Expr<'_>, column: &str, table: &str, unqualified: bool) -> bool {
        match expression {
            Expr::Binary {
                operator: BinaryOp::And,
                left,
                right,
                ..
            } => {
                constrained(left, column, table, unqualified)
                    || constrained(right, column, table, unqualified)
            }
            Expr::Binary {
                operator: BinaryOp::Eq,
                left,
                right,
                ..
            } => {
                let matches = |key: &Expr<'_>, value: &Expr<'_>| {
                    let Expr::Column { qualifier, name } = key else {
                        return false;
                    };
                    if *name != column || !qualifier.map_or(unqualified, |name| name == table) {
                        return false;
                    }
                    let mut references = false;
                    value.for_each_column_reference(&mut |_, _| references = true);
                    !references
                };
                matches(left, right) || matches(right, left)
            }
            _ => false,
        }
    }
    let Some(relation) = crate::sql::catalog_snapshot_keys::RELATIONS
        .iter()
        .find(|relation| relation.name == table.table)
    else {
        return CatalogScan::Sequential;
    };
    let CatalogRowKey::Columns(columns) = relation.key else {
        return CatalogScan::Sequential;
    };
    if predicate.is_some_and(|predicate| {
        columns.iter().all(|column| {
            constrained(
                predicate,
                column,
                table.alias.unwrap_or(table.table),
                unqualified,
            )
        })
    }) {
        CatalogScan::Index
    } else {
        CatalogScan::Sequential
    }
}

/// 42P10 for a column alias list longer than its table's columns. The
/// query an `INSERT` with subscripted targets reads instead reports
/// PostgreSQL's `INSERT has more target columns than expressions`.
pub(crate) fn derived_alias_mismatch(table: &str, available: usize, specified: usize) -> SqlError {
    if table == crate::sql::ast::INSERT_SUBSCRIPT_SOURCE {
        return sql_err!(
            sqlstate::SYNTAX_ERROR,
            "INSERT has more target columns than expressions"
        );
    }
    sql_err!(
        sqlstate::INVALID_COLUMN_REFERENCE,
        "table \"{}\" has {} columns available but {} columns specified",
        table,
        available,
        specified
    )
}

fn validate_table_sample<'a>(
    storage: &Storage,
    table: &TableRef<'a>,
    txid: u32,
    arena: &'a Arena,
) -> Result<(), SqlError> {
    let Some(sample) = table.sample else {
        return Ok(());
    };
    let columns = crate::sql::exec::CatalogCols {
        definition: None,
        alias: None,
        output_aliases: &[],
        storage,
        txid,
        arena,
    };
    for expression in [Some(sample.percentage), sample.repeatable]
        .into_iter()
        .flatten()
    {
        let mut reference: Option<(bool, StackStr<128>)> = None;
        expression.for_each_column_reference(&mut |qualifier, name| {
            if reference.is_none() {
                reference = Some((
                    qualifier.is_some(),
                    StackStr::from_str(qualifier.unwrap_or(name)),
                ));
            }
        });
        if let Some((qualified, name)) = reference {
            return Err(if qualified {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "invalid reference to FROM-clause entry for table \"{}\"",
                    name.as_str()
                )
            } else {
                sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name.as_str()
                )
            });
        }
        let mut aggregates = ArenaList::new(arena);
        collect_aggs(expression, &mut aggregates, storage, txid, &columns)?;
        if !aggregates.is_empty() {
            return Err(sql_err!(
                sqlstate::GROUPING_ERROR,
                "aggregate functions are not allowed in functions in FROM"
            ));
        }
        let mut windows = ArenaList::new(arena);
        collect_windows(expression, &mut windows, storage, txid, &columns)?;
        if !windows.is_empty() {
            return Err(sql_err!(
                sqlstate::WINDOWING_ERROR,
                "window functions are not allowed in functions in FROM"
            ));
        }
        if super::srf::expression_has_project_set(expression, storage, txid) {
            return Err(sql_err!(
                sqlstate::DATATYPE_MISMATCH,
                "argument of TABLESAMPLE must not return a set"
            ));
        }
    }
    Ok(())
}

/// A `USING`/NATURAL join output column: the merged sides in join order. Its
/// value is the first non-null contributor (PostgreSQL's join output
/// variable — a COALESCE across the joined sides, observable with outer
/// joins).
#[derive(Clone, Copy)]
pub struct MergedColumn<'d> {
    pub name: &'d str,
    pub parts: &'d [(usize, usize)],
    /// The merged column's type: the common type of the contributors.
    pub ctype: ColType,
}

#[derive(Clone, Copy)]
pub(super) struct UsingAlias<'d> {
    pub name: &'d str,
    pub first_merged: usize,
    pub n_columns: usize,
}

/// A column name resolved against a query scope: a plain table column
/// (table index, column index), or a USING/NATURAL-merged join column
/// (index into `QueryScope::merged`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolvedColumn {
    Table(usize, usize),
    Merged(usize),
}

pub(crate) fn materialized_definition<'a>(
    storage: &Storage,
    txid: u32,
    name: &str,
    column_names: &[&str],
    column_types: &[(i32, i16, i32)],
    column_collations: &[crate::sql::ast::Collation],
    arena: &'a Arena,
) -> Result<RelationDefinition<'a>, SqlError> {
    debug_assert_eq!(column_names.len(), column_types.len());
    debug_assert_eq!(column_names.len(), column_collations.len());
    let mut columns = [ColumnMeta::EMPTY; MAX_COLUMNS];
    for (index, slot) in columns.iter_mut().enumerate().take(column_names.len()) {
        let (ctype, user_type) =
            crate::sql::exec::catalog_column_type(storage, txid, column_types[index].0)
                .ok_or_else(|| {
                    sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "CTE column \"{}\" type (oid {}) is not supported",
                        column_names[index],
                        column_types[index].0
                    )
                })?;
        *slot = ColumnMeta {
            name: SqlName::parse(column_names[index])?,
            ctype,
            type_mod: column_types[index].2,
            collation: column_collations[index],
            user_type,
            ..ColumnMeta::EMPTY
        };
    }
    retain_query_definition(
        RelationDefinition {
            schema: SqlName::EMPTY,
            name: SqlName::parse(name)?,
            columns: &columns[..column_names.len()],
            n_columns: column_names.len(),
        },
        arena,
    )
}

pub(super) fn retain_query_definition<'a>(
    definition: RelationDefinition<'_>,
    arena: &'a Arena,
) -> Result<RelationDefinition<'a>, SqlError> {
    let columns = arena
        .alloc_interned_slice(definition.columns)
        .map_err(|_| arena_full())?;
    Ok(RelationDefinition {
        columns,
        ..definition
    })
}

#[derive(Clone, Copy)]
enum ScopeDefinition<'a> {
    Synthetic {
        definition: RelationDefinition<'a>,
        table: Option<usize>,
    },
    Relation {
        image: crate::storage::DefinitionImageId,
        names: &'a [&'a str],
    },
    Retained {
        table: usize,
        names: &'a [&'a str],
    },
}

// Definition references borrow this owner; escaped names borrow the arena.
pub(crate) struct QueryDefinitions<'a> {
    sources: &'a mut [Option<ScopeDefinition<'a>>],
    images: TableDefinitionImages,
    transaction: u32,
}

impl<'a> QueryDefinitions<'a> {
    pub(crate) fn get(&self, index: usize) -> Option<RelationDefinition<'_>> {
        match self.sources.get(index).copied().flatten()? {
            ScopeDefinition::Synthetic { definition, .. } => Some(definition),
            ScopeDefinition::Relation { image, .. } => Some(
                self.images
                    .retained_relation(image)
                    .expect("scope retains its catalog definition"),
            ),
            ScopeDefinition::Retained { table, .. } => Some(RelationDefinition::from_definition(
                self.images
                    .retained_definition(table, self.transaction)
                    .expect("scope retains its registered definition"),
            )),
        }
    }

    pub(crate) fn stored(&self, index: usize) -> Option<&TableDef> {
        match self.sources.get(index).copied().flatten()? {
            ScopeDefinition::Synthetic {
                table: Some(table), ..
            }
            | ScopeDefinition::Retained { table, .. } => {
                self.images.retained_definition(table, self.transaction)
            }
            ScopeDefinition::Relation { .. } => None,
            ScopeDefinition::Synthetic { table: None, .. } => None,
        }
    }

    fn retain(&self, storage: &Storage, table: usize) -> Result<&TableDef, SqlError> {
        self.images.definition(storage, table, self.transaction)
    }

    fn set_arena(&mut self, index: usize, definition: RelationDefinition<'a>) {
        self.sources[index] = Some(ScopeDefinition::Synthetic {
            definition,
            table: None,
        });
    }

    fn set_relation(
        &mut self,
        index: usize,
        definition: RelationDefinition<'_>,
        arena: &'a Arena,
    ) -> Result<(), SqlError> {
        let definition = retain_query_definition(definition, arena)?;
        self.set_arena(index, definition);
        Ok(())
    }

    pub(crate) fn column_name(&self, index: usize, column: usize) -> &'a str {
        match self.sources[index].expect("source is resolved") {
            ScopeDefinition::Synthetic { definition, .. } => {
                definition.columns[column].name.as_str()
            }
            ScopeDefinition::Retained { names, .. } | ScopeDefinition::Relation { names, .. } => {
                names[column]
            }
        }
    }
}

/// The resolved FROM clause: per table, its exposed name (alias or table
/// name), definition, and storage slot.
pub struct QueryScope<'d> {
    txid: u32,
    type_sources: Option<crate::storage::StatementFileLease>,
    pub names: &'d mut [&'d str],
    target_aliases: &'d [&'d str],
    pub(crate) defs: QueryDefinitions<'d>,
    pub slots: &'d mut [usize],
    /// Per-source system fields encoded after the visible relation shape.
    hidden_columns: &'d mut [usize],
    /// Effective authorization role for each physical relation. Stored views
    /// populate this with their owner; ordinary references leave it empty and
    /// execute as the session's current role.
    pub authorization_roles: &'d mut [Option<u16>],
    /// Original view identity for an expanded derived source. The underlying
    /// body has its own authorization role; this identity checks the caller's
    /// projected-column authority at the outer query boundary.
    pub view_accesses: &'d mut [Option<u16>],
    /// Derived tables (`FROM (SELECT ...) alias`): the materialized rows,
    /// self-describing-encoded. `None` marks a physical table (scanned from
    /// storage by `slots`).
    pub derived: &'d mut [Option<&'d [&'d [u8]]>],
    /// Object-backed derived rows. `derived` remains `Some` for these entries
    /// so decoding and relation-kind checks take the derived-table path; the
    /// empty arena slice is only a marker, not the row authority.
    pub(crate) external_runs: &'d mut [Option<crate::sql::external::ExternalRun>],
    /// A `LATERAL` FROM item: its rows depend on the outer row, so they are
    /// **not** pre-materialized (`derived` holds an empty placeholder). The
    /// scan re-runs the item's body per outer row, resolving its outer column
    /// references against the tables to its left.
    pub lateral: &'d mut [bool],
    /// Marks a set-returning-function scan (`FROM func(args)`), whose output row
    /// type is its single scalar column — so a whole-row reference to the table
    /// alias yields that scalar, not a one-field record (which is how a
    /// subquery- or storage-derived table's whole-row reference behaves).
    pub func_scalar: &'d mut [bool],
    pub n: usize,
    /// USING/NATURAL-merged join columns (see `MergedColumn`).
    pub merged: &'d mut [MergedColumn<'d>],
    pub n_merged: usize,
    /// Qualifiers introduced by `USING (...) AS alias`, indexed by join.
    using_aliases: &'d mut [Option<UsingAlias<'d>>],
    /// The join tree's output columns in PostgreSQL's order — each
    /// USING/NATURAL join hoists its merged columns to the front and hides
    /// the per-side copies. `n_output == 0` means no merges anywhere: the
    /// output is every table's columns in scope order (the common case, kept
    /// implicit).
    output: &'d mut [ResolvedColumn],
    n_output: usize,
    /// Synthesized USING/NATURAL equality predicates, indexed like
    /// `FromClause::joins` (whose `on` is None for such joins). Filled only
    /// on the executor path (predicate synthesis needs an arena).
    pub join_on: &'d mut [Option<&'d Expr<'d>>],
    /// Names exposed by unaliased parenthesized joins, each a column window
    /// of the join's single derived entry.
    members: &'d [ScopeMember<'d>],
    /// Per parenthesized join entry, the member column each of its visible
    /// columns copies (`None` for a merged `USING` column), so a column can
    /// be named again unambiguously.
    join_origins: &'d [(usize, &'d [Option<usize>])],
    /// Internal names for columns whose name repeats within their entry
    /// (`(SELECT 1 id, 2 id) s`): a NUL byte, which no SQL identifier
    /// contains, then the column's index. Only re-resolution of an already
    /// resolved column produces them.
    positional_names: &'d [(usize, usize, &'d str)],
    /// Tables inside an aliased parenthesized join, which its alias hides.
    hidden_members: &'d [&'d str],
    /// Per aliased parenthesized join entry, the hidden member each of its
    /// columns comes from (`None` for a merged `USING` column), which
    /// PostgreSQL names in an ungrouped-column error.
    hidden_origins: &'d [(usize, &'d [Option<&'d str>])],
    /// The first entry a `LATERAL` FROM item may read. A data-modifying
    /// statement's target (entry 0) is out of reach of its FROM items.
    pub(crate) lateral_from: usize,
    /// Set while adding the right side of a `RIGHT` or `FULL` join, which
    /// may not read the join's other side even when `LATERAL`.
    right_of_outer_join: bool,
}

/// A table (or `USING` alias) inside an unaliased parenthesized join. Its
/// columns are `first..first + n_columns` of the join's derived entry, which
/// keeps them out of unqualified lookup and `SELECT *`.
#[derive(Clone, Copy)]
pub(crate) struct ScopeMember<'d> {
    name: &'d str,
    table: usize,
    first: usize,
    n_columns: usize,
    /// The `num_nulls(member)` column: nonzero when an outer join inside
    /// the parenthesized join null-extended this member.
    presence: Option<usize>,
    /// The catalog relation an unaliased member names, for three-part
    /// qualifiers and alias-hiding diagnostics.
    relation: Option<(&'d str, &'d str)>,
    aliased: bool,
    scalar: bool,
}

/// PostgreSQL's error for a FROM item that exists but is out of reach: an
/// earlier comma-separated item seen from a join condition, or a table
/// hidden inside an aliased parenthesized join.
fn unreachable_entry(name: &str) -> SqlError {
    crate::sql::error_position::with_detail(
        sql_err!(
            sqlstate::UNDEFINED_TABLE,
            "invalid reference to FROM-clause entry for table \"{}\"",
            name
        ),
        format_args!(
            "There is an entry for table \"{}\", but it cannot be referenced from this part of the query.",
            name
        ),
    )
}

/// What a derived table sees outside itself. Of the FROM items before it
/// (`siblings[..preceding]`), only those from `visible_from` on are in reach:
/// all of them for a `LATERAL` item, none for any other. Beyond them are the
/// enclosing queries. A name only an out-of-reach item answers fails with
/// PostgreSQL's invalid-reference diagnostics, with its `LATERAL` hint when
/// marking the item `LATERAL` would reach it.
pub(crate) struct DerivedTableOuter<'s, 'd, 'o, 'a> {
    pub(crate) siblings: &'s QueryScope<'d>,
    /// How many of `siblings`' entries precede the derived table; later
    /// ones are not in the query yet when PostgreSQL analyses it.
    pub(crate) preceding: usize,
    pub(crate) visible_from: usize,
    /// Whether the derived table is `LATERAL`.
    pub(crate) lateral: bool,
    pub(crate) enclosing: Option<&'o dyn ColumnLookup<'a>>,
}

impl<'a> DerivedTableOuter<'_, '_, '_, 'a> {
    /// A derived table at the end of `siblings`, `LATERAL` or not.
    pub(crate) fn after<'s, 'd, 'o>(
        siblings: &'s QueryScope<'d>,
        lateral: bool,
        enclosing: Option<&'o dyn ColumnLookup<'a>>,
    ) -> DerivedTableOuter<'s, 'd, 'o, 'a> {
        DerivedTableOuter {
            siblings,
            preceding: siblings.n,
            visible_from: if lateral && !siblings.right_of_outer_join {
                siblings.lateral_from
            } else {
                siblings.n
            },
            lateral,
            enclosing,
        }
    }

    fn target(&self) -> &dyn ColumnLookup<'a> {
        self.enclosing.unwrap_or(&crate::sql::eval::NoColumns)
    }

    fn reachable(&self, table: usize) -> bool {
        (self.visible_from..self.preceding).contains(&table)
    }

    /// Whether an earlier FROM item in reach answers `qualifier.name`.
    fn sibling_column(&self, qualifier: Option<&str>, name: &str) -> bool {
        if self.visible_from >= self.preceding {
            return false;
        }
        match self.siblings.find_column_unpositioned(qualifier, name) {
            Ok(ResolvedColumn::Table(table, _)) => self.reachable(table),
            Ok(ResolvedColumn::Merged(_)) => {
                self.visible_from == 0 && self.preceding == self.siblings.n
            }
            Err(_) => false,
        }
    }

    fn sibling_table(&self, table: &str) -> bool {
        self.visible_from < self.preceding
            && self
                .siblings
                .source(table)
                .is_ok_and(|source| self.reachable(source.table))
    }

    /// PostgreSQL's error when only an out-of-reach FROM item answers
    /// `qualifier.name`.
    fn unreachable(&self, qualifier: Option<&str>, name: &str) -> Option<SqlError> {
        let scope = self.siblings;
        let hidden = |table: usize| table < self.preceding && !self.reachable(table);
        // A `LATERAL` item finds the statement's target, or the other side
        // of its `RIGHT`/`FULL` join, but may not read it.
        let lateral_target = |table: usize| {
            (self.lateral && table < self.visible_from).then(|| {
                sql_err!(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    "invalid reference to FROM-clause entry for table \"{}\"",
                    scope.names[table]
                )
            })
        };
        let lateral_reason = |error: SqlError, table: usize| {
            if table < scope.lateral_from {
                target_hint(error, scope.names[table], qualifier, name)
            } else {
                crate::sql::error_position::at_reference(
                    crate::sql::error_position::with_detail(
                        error,
                        format_args!(
                            "The combining JOIN type must be INNER or LEFT for a LATERAL reference."
                        ),
                    ),
                    qualifier.unwrap_or(name),
                )
            }
        };
        let error = match qualifier {
            Some(qualifier) => {
                let table = scope.source(qualifier).ok()?.table;
                if !hidden(table) {
                    return None;
                }
                if let Some(error) = lateral_target(table) {
                    return Some(lateral_reason(error, table));
                }
                let error = unreachable_entry(qualifier);
                if table >= scope.lateral_from {
                    crate::sql::error_position::with_hint(
                        error,
                        "To reference that table, you must mark this subquery with LATERAL.",
                    )
                } else {
                    error
                }
            }
            None => {
                let mut owners = (0..self.preceding.min(scope.n))
                    .filter(|table| hidden(*table))
                    .filter_map(|table| Some((table, scope.visible_column_owner(table, name)?)));
                let (table, first) = owners.next()?;
                if let Some(error) = lateral_target(table) {
                    return Some(lateral_reason(error, table));
                }
                let error = sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                );
                if owners.next().is_some() {
                    crate::sql::error_position::with_hint(
                        crate::sql::error_position::with_detail(
                            error,
                            format_args!(
                                "There are columns named \"{}\", but they are in tables that cannot be referenced from this part of the query.",
                                name
                            ),
                        ),
                        "Try using a table-qualified name.",
                    )
                } else {
                    let error = crate::sql::error_position::with_detail(
                        error,
                        format_args!(
                            "There is a column named \"{}\" in table \"{}\", but it cannot be referenced from this part of the query.",
                            name, first
                        ),
                    );
                    if table >= scope.lateral_from {
                        crate::sql::error_position::with_hint(
                            error,
                            "To reference that column, you must mark this subquery with LATERAL.",
                        )
                    } else {
                        error
                    }
                }
            }
        };
        Some(crate::sql::error_position::column_reference(
            error, qualifier, name,
        ))
    }

    /// The enclosing queries' answer, or the out-of-reach diagnostics when
    /// they have none. An earlier FROM item is searched before the enclosing
    /// queries' own out-of-reach items, as PostgreSQL searches outward.
    fn answer<T>(
        &self,
        qualifier: Option<&str>,
        name: &str,
        enclosing: Result<T, SqlError>,
    ) -> Result<T, SqlError> {
        match enclosing {
            Err(error)
                if matches!(
                    error.sqlstate.as_str(),
                    sqlstate::UNDEFINED_COLUMN | sqlstate::UNDEFINED_TABLE
                ) =>
            {
                Err(self.unreachable(qualifier, name).unwrap_or(error))
            }
            answer => answer,
        }
    }
}

/// PostgreSQL's hint naming a statement's target that a `LATERAL` item
/// found but may not read.
fn target_hint(error: SqlError, table: &str, qualifier: Option<&str>, name: &str) -> SqlError {
    crate::sql::error_position::at_reference(
        crate::sql::error_position::with_computed_hint(
            error,
            format_args!(
                "There is an entry for table \"{}\", but it cannot be referenced from this part of the query.",
                table
            ),
        ),
        qualifier.unwrap_or(name),
    )
}

impl<'a> ColumnLookup<'a> for DerivedTableOuter<'_, '_, '_, 'a> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).lookup(qualifier, name);
        }
        self.answer(qualifier, name, self.target().lookup(qualifier, name))
    }

    fn procedural_variable(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Option<crate::sql::guc::VariableConflict> {
        self.target().procedural_variable(qualifier, name)
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).column_type_mod(qualifier, name);
        }
        self.target().column_type_mod(qualifier, name)
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).resolve_column_type(qualifier, name);
        }
        let enclosing = match self.enclosing {
            Some(enclosing) if !enclosing.is_empty_scope() => {
                enclosing.resolve_column_type(qualifier, name)
            }
            _ => Err(crate::sql::error_position::column_reference(
                crate::sql::eval::empty_scope_column_error(qualifier, name),
                qualifier,
                name,
            )),
        };
        self.answer(qualifier, name, enclosing)
    }

    fn is_empty_scope(&self) -> bool {
        self.preceding == 0
            && self
                .enclosing
                .is_none_or(|enclosing| enclosing.is_empty_scope())
    }

    fn recursive_state(&self, qualifier: &str, index: usize) -> Result<Datum<'a>, SqlError> {
        self.target().recursive_state(qualifier, index)
    }

    fn column_identity(&self, qualifier: Option<&str>, name: &str) -> Option<(u32, u32)> {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).column_identity(qualifier, name);
        }
        self.target().column_identity(qualifier, name)
    }

    fn check_unassigned_row_field(
        &self,
        qualifier: &str,
        name: &str,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
        arena: &'a Arena,
    ) -> Result<(), SqlError> {
        if self.sibling_table(qualifier) {
            return Ok(());
        }
        self.target()
            .check_unassigned_row_field(qualifier, name, catalog, arena)
    }

    fn whole_row_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<Option<&'a [crate::sql::types::RecordField<'a>]>, SqlError> {
        if self.sibling_table(table) {
            return super::ScopeSchema(self.siblings).whole_row_fields(table, arena);
        }
        self.target().whole_row_fields(table, arena)
    }

    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        if self.sibling_table(table) {
            return super::ScopeSchema(self.siblings)
                .whole_row_field_metadata(table, index, catalog);
        }
        self.target()
            .whole_row_field_metadata(table, index, catalog)
    }

    fn whole_row_expansion_fields(
        &self,
        table: &str,
        arena: &'a Arena,
    ) -> Result<&'a [crate::sql::types::RecordField<'a>], SqlError> {
        if self.sibling_table(table) {
            return super::ScopeSchema(self.siblings).whole_row_expansion_fields(table, arena);
        }
        self.target().whole_row_expansion_fields(table, arena)
    }

    fn whole_row_present(&self, table: &str) -> Result<bool, SqlError> {
        if self.sibling_table(table) {
            return super::ScopeSchema(self.siblings).whole_row_present(table);
        }
        self.target().whole_row_present(table)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).col_type(qualifier, name);
        }
        self.target().col_type(qualifier, name)
    }

    fn column_type_oid(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).column_type_oid(qualifier, name);
        }
        self.target().column_type_oid(qualifier, name)
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).collation(qualifier, name);
        }
        self.target().collation(qualifier, name)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> crate::sql::ast::Collation {
        let own = super::ScopeSchema(self.siblings).record_field_collation(base, field);
        if own != crate::sql::ast::Collation::None && self.visible_from < self.preceding {
            return own;
        }
        self.target().record_field_collation(base, field)
    }

    fn column_user_type(&self, qualifier: Option<&str>, name: &str) -> Option<UserTypeName> {
        if self.sibling_column(qualifier, name) {
            return super::ScopeSchema(self.siblings).column_user_type(qualifier, name);
        }
        self.target().column_user_type(qualifier, name)
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        if self.sibling_table(table) {
            return super::ScopeSchema(self.siblings).whole_row_is_scalar(table);
        }
        self.target().whole_row_is_scalar(table)
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        if self.sibling_table(table) {
            return super::ScopeSchema(self.siblings).whole_row_type(table);
        }
        self.target().whole_row_type(table)
    }
}

/// Reads a member's `num_nulls(member)` presence column.
pub(crate) fn member_present(value: Datum<'_>) -> Result<bool, SqlError> {
    match value {
        Datum::Int4(nulls) => Ok(nulls == 0),
        _ => Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "parenthesized join member presence is not an integer"
        )),
    }
}

/// A join member's catalog relation (schema, name), whether an alias names
/// it, and whether it is a scalar function source.
type MemberRelation<'d> = (Option<(&'d str, &'d str)>, bool, bool);

/// A qualifier's columns: a whole scope entry or a parenthesized join
/// member's window of one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct ScopeSource {
    pub table: usize,
    pub first: usize,
    pub n_columns: usize,
    pub presence: Option<usize>,
}

/// Type-only lookup over FROM items already registered in a scope. Table
/// functions are implicitly lateral in PostgreSQL, so their output definition
/// may depend on a preceding column before a concrete row has been bound.
struct ScopeTypes<'s, 'd>(&'s QueryScope<'d>);

/// Schema construction can inspect a caller's columns without retaining values
/// whose lifetime belongs to that caller's row scratch.
pub(crate) struct OuterColumnTypes<'s, 'row>(pub(crate) &'s dyn ColumnLookup<'row>);

impl<'a> ColumnLookup<'a> for OuterColumnTypes<'_, '_> {
    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        self.0.whole_row_field_metadata(table, index, catalog)
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        self.0.column_type_mod(qualifier, name)
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        self.0.resolve_column_type(qualifier, name)
    }

    fn is_empty_scope(&self) -> bool {
        self.0.is_empty_scope()
    }

    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        self.0.lookup(qualifier, name).map(|_| Datum::Null)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.0.col_type(qualifier, name)
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.0.collation(qualifier, name)
    }

    fn column_user_type(&self, qualifier: Option<&str>, name: &str) -> Option<UserTypeName> {
        self.0.column_user_type(qualifier, name)
    }

    fn column_type_oid(&self, qualifier: Option<&str>, name: &str) -> Option<i32> {
        self.0.column_type_oid(qualifier, name)
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.0.whole_row_is_scalar(table)
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        self.0.whole_row_type(table)
    }
}

impl<'a> ColumnLookup<'a> for ScopeTypes<'_, '_> {
    fn whole_row_field_metadata(
        &self,
        table: &str,
        index: usize,
        catalog: Option<&dyn crate::sql::eval::CatalogAccess>,
    ) -> Option<(crate::util::StackStr<64>, crate::sql::exec::StaticTypeMeta)> {
        super::ScopeSchema(self.0).whole_row_field_metadata(table, index, catalog)
    }

    fn column_type_mod(&self, qualifier: Option<&str>, name: &str) -> i32 {
        super::ScopeSchema(self.0).column_type_mod(qualifier, name)
    }

    fn resolve_column_type(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ColType, SqlError> {
        super::ScopeSchema(self.0).resolve_column_type(qualifier, name)
    }

    fn is_empty_scope(&self) -> bool {
        self.0.n == 0
    }

    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Result<Datum<'a>, SqlError> {
        self.0.find_column(qualifier, name)?;
        Ok(Datum::Null)
    }

    fn col_type(&self, qualifier: Option<&str>, name: &str) -> Option<ColType> {
        self.0
            .find_column(qualifier, name)
            .ok()
            .map(|column| self.0.output_type(column))
    }

    fn collation(&self, qualifier: Option<&str>, name: &str) -> crate::sql::ast::Collation {
        self.0
            .find_column(qualifier, name)
            .ok()
            .map(|column| self.0.output_collation(column))
            .unwrap_or(crate::sql::ast::Collation::None)
    }

    fn record_field_collation(&self, base: &Expr<'a>, field: &str) -> crate::sql::ast::Collation {
        crate::sql::exec::record_field_metadata(base, field, &super::ScopeCols(self.0))
            .map_or(crate::sql::ast::Collation::None, |meta| meta.collation)
    }

    fn column_user_type(&self, qualifier: Option<&str>, name: &str) -> Option<UserTypeName> {
        match self.0.find_column(qualifier, name).ok()? {
            ResolvedColumn::Table(table, column) => {
                self.0.defs.get(table)?.columns.get(column)?.user_type
            }
            ResolvedColumn::Merged(_) => None,
        }
    }

    fn whole_row_is_scalar(&self, table: &str) -> bool {
        self.0.func_scalar_type(table).is_some()
    }

    fn whole_row_type(&self, table: &str) -> Option<ColType> {
        self.0.qualified_star_columns(table).ok()?;
        Some(self.0.func_scalar_type(table).unwrap_or(ColType::Record))
    }
}

impl<'d> QueryScope<'d> {
    pub(super) fn set_target_aliases(
        &mut self,
        names: &[&'d str],
        arena: &'d Arena,
    ) -> Result<(), SqlError> {
        self.target_aliases = arena.alloc_slice_copy(names).map_err(|_| arena_full())?;
        Ok(())
    }
    /// Describe correlated RETURNING expressions without adding OLD and NEW
    /// as separate, ambiguous sources of unqualified columns.
    pub(crate) fn for_returning(
        storage: &'d Storage,
        definition: RelationDefinition<'_>,
        alias: Option<&'d str>,
        output_aliases: &[&'d str],
        txid: u32,
        arena: &'d Arena,
    ) -> Result<Self, SqlError> {
        let schema = arena
            .alloc_str(definition.schema.as_str())
            .map_err(|_| arena_full())?;
        let table = arena
            .alloc_str(definition.name.as_str())
            .map_err(|_| arena_full())?;
        let from = arena
            .alloc(crate::sql::ast::FromClause {
                base: TableRef {
                    schema: Some(schema),
                    table,
                    alias,
                    subquery: None,
                    func_args: None,
                    func_argument_names: &[],
                    func_variadic: false,
                    func_source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                    rows_from: None,
                    col_alias: None,
                    inheritance: crate::sql::ast::RelationInheritance::Only,
                    sample: None,
                    cte: None,
                    with_ordinality: false,
                    lateral: false,
                    authorization_role: None,
                    bound_table: None,
                    view_access: None,
                    join_members: None,
                },
                joins: &[],
            })
            .map_err(|_| arena_full())?;
        let mut scope = Self::empty(storage, txid, arena, from)?;
        scope.names[0] = alias.unwrap_or(table);
        scope.defs.set_relation(0, definition, arena)?;
        scope.type_sources = storage.select_type_definition_graph(
            definition
                .columns()
                .iter()
                .filter_map(|column| column.user_type),
            txid,
            arena,
        )?;
        scope.slots[0] = usize::MAX;
        scope.n = 1;
        scope.target_aliases = arena
            .alloc_slice_copy(output_aliases)
            .map_err(|_| arena_full())?;
        Ok(scope)
    }

    fn empty(
        storage: &'d Storage,
        txid: u32,
        arena: &'d Arena,
        from: &FromClause<'d>,
    ) -> Result<Self, SqlError> {
        Self::empty_sources(storage, txid, arena, from.joins.len() + 1, from.joins.len())
    }

    pub(super) fn singleton(
        storage: &'d Storage,
        txid: u32,
        arena: &'d Arena,
    ) -> Result<Self, SqlError> {
        Self::empty_sources(storage, txid, arena, 0, 0)
    }

    fn empty_sources(
        storage: &'d Storage,
        txid: u32,
        arena: &'d Arena,
        table_count: usize,
        join_count: usize,
    ) -> Result<Self, SqlError> {
        let names = arena
            .alloc_slice_with(table_count, |_| "")
            .map_err(|_| arena_full())?;
        let defs = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let slots = arena
            .alloc_slice_with(table_count, |_| 0)
            .map_err(|_| arena_full())?;
        let hidden_columns = arena
            .alloc_slice_with(table_count, |_| 0)
            .map_err(|_| arena_full())?;
        let authorization_roles = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let view_accesses = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let derived = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let external_runs = arena
            .alloc_slice_with(table_count, |_| None)
            .map_err(|_| arena_full())?;
        let lateral = arena
            .alloc_slice_with(table_count, |_| false)
            .map_err(|_| arena_full())?;
        let func_scalar = arena
            .alloc_slice_with(table_count, |_| false)
            .map_err(|_| arena_full())?;
        let merged = arena
            .alloc_slice_with(0, |_| MergedColumn {
                name: "",
                parts: &[],
                ctype: ColType::Bool,
            })
            .map_err(|_| arena_full())?;
        let output = arena
            .alloc_slice_with(0, |_| ResolvedColumn::Table(0, 0))
            .map_err(|_| arena_full())?;
        let join_on = arena
            .alloc_slice_with(join_count.max(1), |_| None)
            .map_err(|_| arena_full())?;
        let using_aliases = arena
            .alloc_slice_with(join_count.max(1), |_| None)
            .map_err(|_| arena_full())?;
        Ok(QueryScope {
            txid,
            type_sources: None,
            names,
            target_aliases: &[],
            defs: QueryDefinitions {
                sources: defs,
                images: storage.table_definition_images(),
                transaction: txid,
            },
            slots,
            hidden_columns,
            authorization_roles,
            view_accesses,
            derived,
            external_runs,
            lateral,
            func_scalar,
            n: 0,
            merged,
            n_merged: 0,
            using_aliases,
            output,
            n_output: 0,
            join_on,
            members: &[],
            join_origins: &[],
            positional_names: &[],
            hidden_members: &[],
            hidden_origins: &[],
            lateral_from: 0,
            right_of_outer_join: false,
        })
    }

    /// Gives every column whose name repeats within its entry a positional
    /// internal name (see `positional_names`).
    fn name_repeated_columns(&mut self, arena: &'d Arena) -> Result<(), SqlError> {
        let mut names = ArenaList::new(arena);
        // Catalog relations cannot repeat a column name.
        for table in (0..self.n).filter(|&table| self.derived[table].is_some()) {
            let columns = self.defs.get(table).expect("resolved").columns();
            for (column, meta) in columns.iter().enumerate() {
                if columns
                    .iter()
                    .filter(|other| other.name.as_str() == meta.name.as_str())
                    .count()
                    > 1
                {
                    let name = crate::stack_format!(16, "\0{}", column);
                    let name = arena.alloc_str(name.as_str()).map_err(|_| arena_full())?;
                    names
                        .push((table, column, name))
                        .map_err(|_| arena_full())?;
                }
            }
        }
        self.positional_names = names.as_slice();
        Ok(())
    }

    /// A qualifier and name that resolve back to exactly column `column` of
    /// entry `table`: a parenthesized join's copy of a member column is
    /// named through that member.
    pub(crate) fn column_reference_name(&self, table: usize, column: usize) -> (&str, &str) {
        let origin = self
            .join_origins
            .iter()
            .find(|(entry, _)| *entry == table)
            .map(|(_, origins)| origins.get(column).copied().flatten().unwrap_or(column));
        if let Some(stored) = origin
            && let Some(member) = self.members.iter().find(|member| {
                member.table == table
                    && (member.first..member.first + member.n_columns).contains(&stored)
            })
        {
            return (member.name, self.defs.column_name(table, stored));
        }
        let name = self
            .positional_names
            .iter()
            .find(|(entry, index, _)| *entry == table && *index == column)
            .map_or_else(
                || self.defs.column_name(table, column),
                |(_, _, name)| *name,
            );
        (self.names[table], name)
    }

    /// The table PostgreSQL names for column `column` of entry `table` in an
    /// error: a column of an aliased parenthesized join is its member's.
    pub(crate) fn column_error_name(&self, table: usize, column: usize) -> (&str, &str) {
        let (qualifier, name) = self.column_reference_name(table, column);
        let member = self
            .hidden_origins
            .iter()
            .find(|(entry, _)| *entry == table)
            .and_then(|(_, origins)| origins.get(column).copied().flatten());
        (member.unwrap_or(qualifier), name)
    }

    /// Whether `exposed` already names a FROM item or join member.
    fn name_taken(&self, exposed: &str) -> bool {
        self.names[..self.n].contains(&exposed)
            || self.members.iter().any(|member| member.name == exposed)
    }

    /// PostgreSQL's name for an unaliased derived table or parenthesized
    /// join, made unique so internal re-resolution of its columns stays
    /// unambiguous when several coexist.
    fn unnamed_entry<'a>(
        &self,
        tref: &TableRef<'_>,
        arena: &'a Arena,
    ) -> Result<&'a str, SqlError> {
        let base = if tref.join_members.is_some() {
            "unnamed_join"
        } else {
            "unnamed_subquery"
        };
        if !self.name_taken(base) {
            return Ok(base);
        }
        for suffix in 2..=self.n + 1 {
            let candidate = crate::stack_format!(64, "{} {}", base, suffix);
            if !self.name_taken(candidate.as_str()) {
                return arena
                    .alloc_str(candidate.as_str())
                    .map_err(|_| arena_full());
            }
        }
        Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "no free name for an unaliased FROM item"
        ))
    }

    fn duplicate_name(&self, exposed: &str) -> Result<(), SqlError> {
        if self.name_taken(exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        Ok(())
    }

    /// Describes the derived table `tref` about to join this scope. A
    /// `LATERAL` item reads the FROM items before it, then the enclosing
    /// queries; any other derived table reads only the enclosing queries.
    #[expect(
        clippy::too_many_arguments,
        reason = "description carries the derived table's explicit context"
    )]
    fn describe_derived<'a>(
        &self,
        storage: &Storage,
        tref: &TableRef<'_>,
        sub: &crate::sql::ast::Select<'_>,
        exposed: &str,
        col_alias: Option<&[&str]>,
        txid: u32,
        arena: &'a Arena,
        outer: Option<&dyn ColumnLookup<'_>>,
    ) -> Result<RelationDefinition<'a>, SqlError> {
        let derived_outer = DerivedTableOuter::after(self, tref.lateral, outer);
        super::srf::describe_derived_definition(
            storage,
            sub,
            exposed,
            col_alias,
            txid,
            arena,
            Some(&derived_outer),
        )
    }

    /// An aliased parenthesized join's column aliases are checked against
    /// the join's columns, which PostgreSQL calls a join expression.
    #[expect(
        clippy::too_many_arguments,
        reason = "the check describes the join with its enclosing queries"
    )]
    fn check_join_alias_width(
        &self,
        storage: &Storage,
        tref: &TableRef<'_>,
        sub: &crate::sql::ast::Select<'_>,
        exposed: &str,
        txid: u32,
        arena: &'d Arena,
        outer: Option<&dyn ColumnLookup<'_>>,
    ) -> Result<(), SqlError> {
        let (Some(_), Some(aliases)) = (tref.join_members, tref.col_alias) else {
            return Ok(());
        };
        let definition =
            self.describe_derived(storage, tref, sub, exposed, None, txid, arena, outer)?;
        if aliases.len() > definition.n_columns {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "join expression \"{}\" has {} columns available but {} columns specified",
                exposed,
                definition.n_columns,
                aliases.len()
            ));
        }
        Ok(())
    }

    /// Records the member names of a parenthesized join just added: exposed
    /// when it has no alias, otherwise hidden behind it.
    fn register_join_members<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let Some(members) = tref.join_members else {
            return Ok(());
        };
        if tref.alias.is_none() {
            return self.add_join_members(storage, tref, members, txid, arena);
        }
        let mut hidden = ArenaList::new(arena);
        for name in self.hidden_members {
            hidden.push(*name).map_err(|_| arena_full())?;
        }
        for member in members {
            hidden.push(member.name).map_err(|_| arena_full())?;
        }
        self.hidden_members = hidden.as_slice();
        let sub = tref
            .subquery
            .expect("a parenthesized join is a derived table");
        let inner_from = sub
            .from
            .as_ref()
            .expect("a parenthesized join has a FROM clause");
        let outer_schema = tref.lateral.then_some(super::ScopeSchema(&*self));
        let inner = QueryScope::resolve_schema(
            storage,
            inner_from,
            txid,
            arena,
            outer_schema
                .as_ref()
                .map(|schema| schema as &dyn ColumnLookup<'_>),
        )?;
        let mut origins = ArenaList::new(arena);
        for index in 0..inner.star_columns() {
            origins
                .push(match inner.star_entry(index) {
                    ResolvedColumn::Merged(_) => None,
                    ResolvedColumn::Table(table, column) => Some(
                        arena
                            .alloc_str(inner.column_reference_name(table, column).0)
                            .map_err(|_| arena_full())?,
                    ),
                })
                .map_err(|_| arena_full())?;
        }
        let mut hidden_origins = ArenaList::new(arena);
        for origin in self.hidden_origins {
            hidden_origins.push(*origin).map_err(|_| arena_full())?;
        }
        hidden_origins
            .push((self.n - 1, origins.as_slice()))
            .map_err(|_| arena_full())?;
        self.hidden_origins = hidden_origins.as_slice();
        Ok(())
    }

    /// Registers the members of the unaliased parenthesized join that was
    /// just added as entry `self.n - 1`, and narrows that entry's visible
    /// columns to the join's own output.
    fn add_join_members<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        members: &'a [crate::sql::ast::JoinMember<'a>],
        txid: u32,
        arena: &'a Arena,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let table = self.n - 1;
        let sub = tref
            .subquery
            .expect("a parenthesized join is a derived table");
        let inner_from = sub
            .from
            .as_ref()
            .expect("a parenthesized join has a FROM clause");
        let outer_schema = tref.lateral.then_some(super::ScopeSchema(&*self));
        let inner = QueryScope::resolve_schema(
            storage,
            inner_from,
            txid,
            arena,
            outer_schema
                .as_ref()
                .map(|schema| schema as &dyn ColumnLookup<'_>),
        )?;
        inner.check_join_conditions(inner_from, None, Some(&*self))?;
        for member in members {
            self.duplicate_name(member.name)?;
        }
        let visible = inner.star_columns();
        let mut first = visible;
        let mut scope_members = ArenaList::new(arena);
        for member in self.members {
            scope_members.push(*member).map_err(|_| arena_full())?;
        }
        let mut n_table_members = 0usize;
        for member in members {
            let n_columns = inner.qualified_star_columns(member.name)?;
            let (relation, aliased, scalar) = if member.using_alias {
                (None, true, false)
            } else {
                n_table_members += 1;
                inner.member_relation(member.name, arena)?
            };
            scope_members
                .push(ScopeMember {
                    name: member.name,
                    table,
                    first,
                    n_columns,
                    presence: None,
                    relation,
                    aliased,
                    scalar,
                })
                .map_err(|_| arena_full())?;
            first += n_columns;
        }
        let members_start = self.members.len();
        let scope_members = scope_members.into_mut_slice().map_err(|_| arena_full())?;
        let mut presence = first;
        for (member, written) in scope_members[members_start..].iter_mut().zip(members) {
            if !written.using_alias {
                member.presence = Some(presence);
                presence += 1;
            }
        }
        debug_assert_eq!(presence, first + n_table_members);
        let mut origins = ArenaList::new(arena);
        for index in 0..visible {
            let origin = match inner.star_entry(index) {
                ResolvedColumn::Merged(_) => None,
                ResolvedColumn::Table(inner_table, inner_column) => {
                    let (qualifier, name) = inner.column_reference_name(inner_table, inner_column);
                    let columns = self
                        .defs
                        .get(table)
                        .expect("join entry is resolved")
                        .columns;
                    scope_members[members_start..]
                        .iter()
                        .find(|member| member.name == qualifier && member.presence.is_some())
                        .and_then(|member| {
                            columns[member.first..member.first + member.n_columns]
                                .iter()
                                .position(|column| column.name.as_str() == name)
                                .map(|offset| member.first + offset)
                        })
                }
            };
            origins.push(origin).map_err(|_| arena_full())?;
        }
        let mut join_origins = ArenaList::new(arena);
        for origin in self.join_origins {
            join_origins.push(*origin).map_err(|_| arena_full())?;
        }
        join_origins
            .push((table, origins.as_slice()))
            .map_err(|_| arena_full())?;
        self.join_origins = join_origins.as_slice();
        let definition = self.defs.get(table).expect("join entry is resolved");
        if definition.columns.len() != presence {
            return Err(sql_err!(
                sqlstate::INTERNAL_ERROR,
                "parenthesized join output has {} columns, expected {}",
                definition.columns.len(),
                presence
            ));
        }
        let definition = RelationDefinition {
            n_columns: visible,
            ..definition
        };
        let definition = retain_query_definition(definition, arena)?;
        self.defs.set_arena(table, definition);
        self.members = scope_members;
        Ok(())
    }

    /// Relation identity of a name this scope exposes, as a member of an
    /// enclosing parenthesized join sees it.
    fn member_relation(
        &self,
        name: &str,
        arena: &'d Arena,
    ) -> Result<MemberRelation<'d>, SqlError> {
        if let Some(member) = self.members.iter().find(|member| member.name == name) {
            return Ok((member.relation, member.aliased, member.scalar));
        }
        let table = self.entry_index(name)?;
        let scalar = self.func_scalar_column(name).is_some();
        let definition = self.defs.get(table).expect("resolved");
        if definition.schema.as_str().is_empty() {
            return Ok((None, true, scalar));
        }
        let schema = arena
            .alloc_str(definition.schema.as_str())
            .map_err(|_| arena_full())?;
        let relation_name = arena
            .alloc_str(definition.name.as_str())
            .map_err(|_| arena_full())?;
        Ok((
            Some((schema, relation_name)),
            self.names[table] != relation_name,
            scalar,
        ))
    }

    fn member(&self, qualifier: &str) -> Option<&ScopeMember<'d>> {
        match qualifier.split_once('.') {
            None => self.members.iter().find(|member| member.name == qualifier),
            Some((schema, table)) => self
                .members
                .iter()
                .find(|member| !member.aliased && member.relation == Some((schema, table))),
        }
    }

    /// A source's visible columns, in its own order.
    pub(crate) fn source_columns(&self, source: ScopeSource) -> &[ColumnMeta] {
        &self.defs.get(source.table).expect("resolved").columns
            [source.first..source.first + source.n_columns]
    }

    /// The columns a qualifier names: a FROM item, or a member of an
    /// unaliased parenthesized join.
    pub(crate) fn source(&self, qualifier: &str) -> Result<ScopeSource, SqlError> {
        if let Some(member) = self.member(qualifier) {
            return Ok(ScopeSource {
                table: member.table,
                first: member.first,
                n_columns: member.n_columns,
                presence: member.presence,
            });
        }
        let table = self.entry_index(qualifier)?;
        Ok(ScopeSource {
            table,
            first: 0,
            n_columns: self.defs.get(table).expect("resolved").n_columns,
            presence: None,
        })
    }

    /// Resolves execution sources, materializing derived rows once and keeping
    /// only their actual output-column metadata.
    pub fn resolve_exec<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        predicate: Option<&'a Expr<'a>>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        sequences: Option<&dyn SequenceAccess>,
    ) -> Result<QueryScope<'a>, SqlError> {
        Self::resolve_exec_outer(
            storage, from, predicate, txid, arena, params, sequences, None,
        )
    }

    /// Execution scope with an enclosing row available while FROM items are
    /// resolved. Correlated subqueries need this for a base table function
    /// such as `unnest(outer.array)`.
    #[expect(
        clippy::too_many_arguments,
        reason = "scope construction carries explicit query restrictions and execution context"
    )]
    pub fn resolve_exec_outer<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        predicate: Option<&'a Expr<'a>>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        sequences: Option<&dyn SequenceAccess>,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<QueryScope<'a>, SqlError> {
        let mut scope = QueryScope::empty(storage, txid, arena, from)?;
        scope.add_exec(
            storage,
            &from.base,
            txid,
            arena,
            params,
            sequences,
            outer,
            catalog_scan(&from.base, predicate, from.joins.is_empty()),
        )?;
        for j in from.joins {
            scope.right_of_outer_join = j.kind.is_right_or_full();
            scope.add_exec(
                storage,
                &j.table,
                txid,
                arena,
                params,
                sequences,
                outer,
                catalog_scan(&j.table, predicate, false),
            )?;
        }
        scope.right_of_outer_join = false;
        scope.check_join_conditions(from, outer, None)?;
        scope.build_merges(from, arena, Some(arena))?;
        scope.name_repeated_columns(arena)?;
        Ok(scope)
    }

    /// PostgreSQL analyses a join's `ON` condition against only the FROM
    /// items that join combines: earlier comma-separated items and later
    /// joins are in the query but out of the condition's reach. `hidden` is
    /// the enclosing FROM clause of a parenthesized join, equally out of
    /// reach.
    pub(super) fn check_join_conditions(
        &self,
        from: &FromClause<'_>,
        outer: Option<&dyn ColumnLookup<'_>>,
        hidden: Option<&QueryScope<'_>>,
    ) -> Result<(), SqlError> {
        let mut start = 0;
        for (index, join) in from.joins.iter().enumerate() {
            let table = index + 1;
            if join.kind == crate::sql::ast::JoinKind::Comma {
                start = table;
            }
            if let Some(condition) = join.on {
                self.check_condition_references(condition, start..table + 1, outer, hidden)?;
            }
        }
        Ok(())
    }

    fn check_condition_references(
        &self,
        expression: &Expr<'_>,
        visible: core::ops::Range<usize>,
        outer: Option<&dyn ColumnLookup<'_>>,
        hidden: Option<&QueryScope<'_>>,
    ) -> Result<(), SqlError> {
        if let Expr::Column { qualifier, name } = expression {
            self.check_condition_column(*qualifier, name, visible.clone(), outer, hidden)?;
        }
        let mut failure = None;
        let _ = super::subquery::walk_children(expression, &mut |child| {
            if failure.is_none()
                && let Err(error) =
                    self.check_condition_references(child, visible.clone(), outer, hidden)
            {
                failure = Some(error);
            }
            Ok(())
        });
        failure.map_or(Ok(()), Err)
    }

    fn check_condition_column(
        &self,
        qualifier: Option<&str>,
        name: &str,
        visible: core::ops::Range<usize>,
        outer: Option<&dyn ColumnLookup<'_>>,
        hidden: Option<&QueryScope<'_>>,
    ) -> Result<(), SqlError> {
        let outer_resolves =
            || outer.is_some_and(|outer| outer.col_type(qualifier, name).is_some());
        match qualifier {
            Some(qualifier) => {
                let table = match self.source(qualifier) {
                    Ok(source) => Some(source.table),
                    Err(_) => None,
                };
                if table.is_some_and(|table| visible.contains(&table)) || outer_resolves() {
                    return Ok(());
                }
                let invisible = match table {
                    Some(table) => table < visible.start,
                    None => hidden.is_some_and(|hidden| hidden.source(qualifier).is_ok()),
                };
                if invisible {
                    return Err(crate::sql::error_position::column_reference(
                        unreachable_entry(qualifier),
                        Some(qualifier),
                        name,
                    ));
                }
                if table.is_some() {
                    return Err(crate::sql::error_position::column_reference(
                        sql_err!(
                            sqlstate::UNDEFINED_TABLE,
                            "missing FROM-clause entry for table \"{}\"",
                            qualifier
                        ),
                        Some(qualifier),
                        name,
                    ));
                }
                Ok(())
            }
            None => {
                if visible
                    .clone()
                    .any(|table| self.visible_column_owner(table, name).is_some())
                    || self.using_alias(name).is_some()
                    || self.source(name).is_ok()
                    || outer_resolves()
                {
                    return Ok(());
                }
                let earlier = (0..visible.start)
                    .find_map(|table| self.visible_column_owner(table, name))
                    .or_else(|| {
                        hidden.and_then(|hidden| {
                            (0..hidden.n).find_map(|table| hidden.visible_column_owner(table, name))
                        })
                    });
                let later = (visible.end..self.n)
                    .any(|table| self.visible_column_owner(table, name).is_some());
                let error = sql_err!(
                    sqlstate::UNDEFINED_COLUMN,
                    "column \"{}\" does not exist",
                    name
                );
                let error = match earlier {
                    Some(owner) => crate::sql::error_position::with_detail(
                        error,
                        format_args!(
                            "There is a column named \"{}\" in table \"{}\", but it cannot be referenced from this part of the query.",
                            name, owner
                        ),
                    ),
                    None if later => error,
                    None => return Ok(()),
                };
                Err(crate::sql::error_position::column_reference(
                    error, None, name,
                ))
            }
        }
    }

    /// The name of the table (or join member) of entry `table` that has a
    /// visible column `name`.
    fn visible_column_owner(&self, table: usize, name: &str) -> Option<&str> {
        let definition = self.defs.get(table)?;
        definition.column_index(name)?;
        self.members
            .iter()
            .filter(|member| member.table == table && member.presence.is_some())
            .find(|member| {
                definition.columns[member.first..member.first + member.n_columns]
                    .iter()
                    .any(|column| column.name.as_str() == name)
            })
            .map_or(Some(self.names[table]), |member| Some(member.name))
    }

    /// Registers a materialized CTE's actual output columns and rows. Without
    /// `read` the scope is schema only (Describe path).
    fn add_materialized<'a>(
        &mut self,
        storage: &Storage,
        tref: &'a TableRef<'a>,
        m: &'a MaterializedCte<'a>,
        txid: u32,
        arena: &'a Arena,
        read: Option<super::DeferredCteRead<'_, 'a>>,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let exposed = tref.alias.unwrap_or(tref.table);
        if !exposed.is_empty() {
            self.duplicate_name(exposed)?;
        }
        let ncols = m.column_names.len();
        if ncols > MAX_COLUMNS {
            return Err(sql_err!(sqlstate::TOO_MANY_COLUMNS, "too many columns"));
        }
        if let Some(aliases) = tref.col_alias
            && aliases.len() > ncols
        {
            return Err(derived_alias_mismatch(exposed, ncols, aliases.len()));
        }
        let mut names = [""; MAX_COLUMNS];
        for (index, name) in names.iter_mut().enumerate().take(ncols) {
            *name = tref
                .col_alias
                .and_then(|aliases| aliases.get(index).copied())
                .unwrap_or(m.column_names[index]);
        }
        let cached = m.definition.filter(|definition| {
            definition.n_columns == ncols
                && definition
                    .columns()
                    .iter()
                    .zip(&names[..ncols])
                    .all(|(column, name)| column.name.as_str() == *name)
        });
        let def_reference = match cached {
            Some(definition) => definition,
            None => match m.definition {
                Some(definition) => {
                    if definition.n_columns != ncols {
                        return Err(sql_err!(
                            sqlstate::INTERNAL_ERROR,
                            "materialized relation visible width changed"
                        ));
                    }
                    let columns = arena
                        .alloc_slice_copy(definition.columns)
                        .map_err(|_| arena_full())?;
                    for (column, name) in columns.iter_mut().zip(&names[..ncols]) {
                        column.name = SqlName::parse(name)?;
                    }
                    RelationDefinition {
                        columns,
                        ..definition
                    }
                }
                None => materialized_definition(
                    storage,
                    txid,
                    exposed,
                    &names[..ncols],
                    m.column_types,
                    m.column_collations,
                    arena,
                )?,
            },
        };
        self.names[self.n] = exposed;
        self.defs.set_arena(self.n, def_reference);
        let materialize = read.is_some();
        self.derived[self.n] = Some(if materialize {
            m.read_rows(arena, read)?
        } else {
            &[]
        });
        self.external_runs[self.n] = if materialize { m.external_run() } else { None };
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
    }

    /// Add one FROM item, materializing a derived table if `tref` is a subquery.
    #[expect(
        clippy::too_many_arguments,
        reason = "execution scope carries typed transaction, memory, sequence, and correlation contexts"
    )]
    fn add_exec<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        sequences: Option<&dyn SequenceAccess>,
        outer: Option<&dyn ColumnLookup<'a>>,
        catalog_scan: crate::sql::catalog_snapshot::CatalogScan,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        validate_table_sample(storage, tref, txid, arena)?;
        if tref.sample.is_some()
            && (tref.cte.is_some()
                || tref.is_function_source()
                || tref.subquery.is_some()
                || storage
                    .sequence_on_path(tref.schema, tref.table, txid)
                    .is_some()
                || matches!(
                    storage.resolve_relation(tref.schema, tref.table, txid),
                    Some(crate::storage::ResolvedRelation::Catalog)
                ))
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "TABLESAMPLE clause can only be applied to tables and materialized views"
            ));
        }
        if let Some(m) = tref.cte {
            return self.add_materialized(
                storage,
                tref,
                m,
                txid,
                arena,
                Some(super::DeferredCteRead {
                    storage,
                    txid,
                    params,
                    sequences,
                }),
            );
        }
        if tref.is_function_source() {
            return self.add_table_func(storage, tref, txid, arena, params, sequences, true, outer);
        }
        let Some(sub) = tref.subquery else {
            if matches!(
                storage.resolve_relation(tref.schema, tref.table, txid),
                Some(crate::storage::ResolvedRelation::Catalog)
            ) {
                return self.add_catalog(storage, tref, txid, arena, true, catalog_scan);
            }
            if let Some(slot) = storage.sequence_on_path(tref.schema, tref.table, txid) {
                return self.add_sequence(storage, tref, slot, txid, arena, true);
            }
            if let Some(crate::storage::ResolvedRelation::Table(slot)) =
                storage.resolve_relation(tref.schema, tref.table, txid)
                && storage.table_def(slot, txid).kind == crate::storage::TableKind::Foreign
            {
                if tref.sample.is_some() {
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "TABLESAMPLE is not supported for foreign tables"
                    ));
                }
                let source = self.n;
                self.add(storage, tref, txid, arena)?;
                let rows = crate::sql::foreign::materialize(storage, slot, txid, arena, sequences)?;
                self.derived[source] = Some(rows);
                return Ok(());
            }
            return self.add(storage, tref, txid, arena);
        };
        let exposed = match tref.alias {
            Some(alias) => {
                self.duplicate_name(alias)?;
                alias
            }
            None => self.unnamed_entry(tref, arena)?,
        };
        // A LATERAL subquery's def is typed against the tables to its left
        // (a body may project an outer column); a plain derived table is typed
        // on its own. Its rows are materialized per outer row by the scan, so
        // register an empty placeholder here rather than running it now.
        self.check_join_alias_width(storage, tref, sub, exposed, txid, arena, outer)?;
        let mut definition = self.describe_derived(
            storage,
            tref,
            sub,
            exposed,
            tref.col_alias,
            txid,
            arena,
            outer,
        )?;
        if let Some(slot) = tref.view_access {
            let view = storage.view(usize::from(slot));
            definition.schema = view.schema_for(txid);
            definition.name = view.name_for(txid);
        }
        self.defs.set_arena(self.n, definition);
        // On the right of a RIGHT or FULL join a LATERAL item may read only
        // FROM items outside that join (analysis rejects the rest), which
        // are fixed for the whole join: its rows are materialized once.
        if tref.lateral && !self.right_of_outer_join {
            self.names[self.n] = exposed;
            self.derived[self.n] = Some(&[]);
            self.lateral[self.n] = true;
            self.slots[self.n] = usize::MAX;
            self.view_accesses[self.n] = tref.view_access;
            self.n += 1;
            self.register_join_members(storage, tref, txid, arena)?;
            return Ok(());
        }
        let rows: &'a [&'a [u8]];
        let external_run;
        if storage.spill_attached() {
            // A stable all-equal sort is an insertion-order spool. Resetting
            // one leased producer cannot disturb a nested child's producer.
            let mut compare = |_left: &[u8], _right: &[u8]| Ok(core::cmp::Ordering::Equal);
            let mut sorter = storage.external_sorter()?;
            sorter.reset();
            select_into_rows(
                storage,
                txid,
                sub,
                arena,
                params,
                outer,
                sequences,
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
            external_run = storage
                .with_block_store(|blocks| sorter.finish(blocks, &mut compare))
                .expect("spill-attached block store")?;
            rows = &[];
        } else {
            // Local-only mode has no authoritative external tier: retain the
            // existing arena representation and fail loudly at its bound.
            const EMPTY: &[u8] = &[];
            let mut store: *mut &[u8] = core::ptr::null_mut();
            let mut len = 0usize;
            let mut cap = 0usize;
            select_into_rows(
                storage,
                txid,
                sub,
                arena,
                params,
                outer,
                sequences,
                &mut |values| {
                    let encoded = crate::sql::exec::encode_projected_pub(values, arena)?;
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
                    unsafe { store.add(len).write(encoded) };
                    len += 1;
                    Ok(())
                },
            )?;
            rows = if len == 0 {
                &[]
            } else {
                unsafe { core::slice::from_raw_parts(store, len) }
            };
            external_run = None;
        }
        self.names[self.n] = exposed;
        self.derived[self.n] = Some(rows);
        self.external_runs[self.n] = external_run;
        self.slots[self.n] = usize::MAX;
        self.view_accesses[self.n] = tref.view_access;
        self.n += 1;
        self.register_join_members(storage, tref, txid, arena)?;
        Ok(())
    }

    /// Resolves schema-only sources, including actual derived-table columns
    /// (`FROM (SELECT ...)`) without materializing its rows. Used where only
    /// the output schema is needed. An enclosing row supplies declared types
    /// for correlated table-function arguments without executing the functions.
    pub fn resolve_schema<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        txid: u32,
        arena: &'a Arena,
        outer: Option<&dyn ColumnLookup<'_>>,
    ) -> Result<QueryScope<'a>, SqlError> {
        Self::resolve_schema_from(storage, from, txid, arena, outer, 0)
    }

    /// Schema of a data-modifying statement's target (`from.base`) joined
    /// with its `FROM`/`USING` items, which cannot read the target.
    pub(crate) fn resolve_dml_schema<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        txid: u32,
        arena: &'a Arena,
        outer: Option<&dyn ColumnLookup<'_>>,
    ) -> Result<QueryScope<'a>, SqlError> {
        Self::resolve_schema_from(storage, from, txid, arena, outer, 1)
    }

    fn resolve_schema_from<'a>(
        storage: &'a Storage,
        from: &'a FromClause<'a>,
        txid: u32,
        arena: &'a Arena,
        outer: Option<&dyn ColumnLookup<'_>>,
        lateral_from: usize,
    ) -> Result<QueryScope<'a>, SqlError> {
        let mut scope = QueryScope::empty(storage, txid, arena, from)?;
        scope.add_schema(storage, &from.base, txid, arena, outer)?;
        scope.lateral_from = lateral_from;
        for j in from.joins {
            scope.right_of_outer_join = j.kind.is_right_or_full();
            scope.add_schema(storage, &j.table, txid, arena, outer)?;
        }
        scope.right_of_outer_join = false;
        scope.check_join_conditions(from, outer, None)?;
        scope.build_merges(from, arena, None)?;
        scope.name_repeated_columns(arena)?;
        Ok(scope)
    }

    fn add_schema<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
        outer: Option<&dyn ColumnLookup<'_>>,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        validate_table_sample(storage, tref, txid, arena)?;
        if tref.sample.is_some()
            && (tref.cte.is_some()
                || tref.is_function_source()
                || tref.subquery.is_some()
                || storage
                    .sequence_on_path(tref.schema, tref.table, txid)
                    .is_some()
                || matches!(
                    storage.resolve_relation(tref.schema, tref.table, txid),
                    Some(crate::storage::ResolvedRelation::Catalog)
                ))
        {
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "TABLESAMPLE clause can only be applied to tables and materialized views"
            ));
        }
        if let Some(m) = tref.cte {
            return self.add_materialized(storage, tref, m, txid, arena, None);
        }
        if tref.is_function_source() {
            let outer_types = outer.map(OuterColumnTypes);
            return self.add_table_func(
                storage,
                tref,
                txid,
                arena,
                &[],
                None,
                false,
                outer_types
                    .as_ref()
                    .map(|columns| columns as &dyn ColumnLookup<'a>),
            );
        }
        let Some(sub) = tref.subquery else {
            if matches!(
                storage.resolve_relation(tref.schema, tref.table, txid),
                Some(crate::storage::ResolvedRelation::Catalog)
            ) {
                return self.add_catalog(
                    storage,
                    tref,
                    txid,
                    arena,
                    false,
                    crate::sql::catalog_snapshot::CatalogScan::Sequential,
                );
            }
            if let Some(slot) = storage.sequence_on_path(tref.schema, tref.table, txid) {
                return self.add_sequence(storage, tref, slot, txid, arena, false);
            }
            return self.add(storage, tref, txid, arena);
        };
        let exposed = match tref.alias {
            Some(alias) => {
                self.duplicate_name(alias)?;
                alias
            }
            None => self.unnamed_entry(tref, arena)?,
        };
        self.check_join_alias_width(storage, tref, sub, exposed, txid, arena, outer)?;
        let mut definition = self.describe_derived(
            storage,
            tref,
            sub,
            exposed,
            tref.col_alias,
            txid,
            arena,
            outer,
        )?;
        if let Some(slot) = tref.view_access {
            let view = storage.view(usize::from(slot));
            definition.schema = view.schema_for(txid);
            definition.name = view.name_for(txid);
        }
        self.names[self.n] = exposed;
        self.defs.set_arena(self.n, definition);
        self.authorization_roles[self.n] = tref.authorization_role;
        self.view_accesses[self.n] = tref.view_access;
        // No rows: this scope is never scanned, only described. An empty row
        // set keeps a stray scan safe rather than reading a physical slot.
        self.derived[self.n] = Some(&[]);
        self.lateral[self.n] = tref.lateral;
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        self.register_join_members(storage, tref, txid, arena)?;
        Ok(())
    }

    /// Registers a `pg_catalog` / `information_schema` relation as a
    /// derived-table entry (synthesized rows), so the general executor can
    /// join it, use it in subqueries, etc. `materialize` false = schema only
    /// (Describe path).
    fn add_catalog<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
        materialize: bool,
        scan: crate::sql::catalog_snapshot::CatalogScan,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let namespace = storage
            .path()
            .catalog_namespace(tref.schema, tref.table)
            .ok_or_else(|| {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "catalog relation \"{}\" has no selected namespace",
                    tref.table
                )
            })?;
        let synth = if materialize {
            crate::sql::catalog::synthesize(
                storage,
                Some(namespace),
                tref.table,
                txid,
                scan,
                arena,
            )?
        } else {
            crate::sql::catalog::synthesize_definition(Some(namespace), tref.table, arena)?
        };
        let exposed = tref.alias.unwrap_or(tref.table);
        self.duplicate_name(exposed)?;
        let mut definition = synth.def;
        if let Some(aliases) = tref.col_alias {
            if aliases.len() > definition.n_columns {
                return Err(derived_alias_mismatch(
                    exposed,
                    definition.n_columns,
                    aliases.len(),
                ));
            }
            let columns = arena
                .alloc_slice_copy(definition.columns)
                .map_err(|_| arena_full())?;
            for (column, alias) in columns.iter_mut().zip(aliases) {
                column.name = SqlName::parse(alias)?;
            }
            definition.columns = columns;
        }
        let columns = definition.columns;
        let names = arena
            .alloc_slice_with(columns.len(), |column| columns[column].name.as_str())
            .map_err(|_| arena_full())?;
        let image = self.defs.images.retain_relation(&definition)?;
        let rows: &'a [&'a [u8]] = if materialize {
            const EMPTY: &[u8] = &[];
            let encoded = arena
                .alloc_slice_with(synth.rows.len(), |_| EMPTY)
                .map_err(|_| arena_full())?;
            for (i, r) in synth.rows.iter().enumerate() {
                if r.len() > MAX_COLUMNS {
                    return Err(arena_full());
                }
                let mut values = [Datum::Null; MAX_COLUMNS];
                for (column, value) in r.iter().copied().enumerate() {
                    // Catalog builders may compute identifiers as signed
                    // integers, but tagged rows must carry the advertised oid
                    // representation before joins and expression evaluation.
                    values[column] = if synth.def.columns[column].ctype == ColType::Oid {
                        super::super::eval::cast_to(value, ColType::Oid, arena)?
                    } else {
                        value
                    };
                }
                encoded[i] = crate::sql::exec::encode_projected_pub(&values[..r.len()], arena)?;
            }
            encoded
        } else {
            &[]
        };
        self.names[self.n] = exposed;
        self.defs.sources[self.n] = Some(ScopeDefinition::Relation { image, names });
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.hidden_columns[self.n] = synth.hidden_columns;
        self.n += 1;
        Ok(())
    }

    /// Registers PostgreSQL's single-row sequence relation. Sequence values
    /// remain storage-owned; the derived row is only a bounded query image.
    fn add_sequence<'a>(
        &mut self,
        storage: &Storage,
        tref: &'a TableRef<'a>,
        slot: usize,
        txid: u32,
        arena: &'a Arena,
        materialize: bool,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let sequence = storage.sequence_for(slot, txid);
        let exposed = tref.alias.unwrap_or(tref.table);
        self.duplicate_name(exposed)?;
        if let Some(aliases) = tref.col_alias
            && aliases.len() > 3
        {
            return Err(sql_err!(
                sqlstate::INVALID_COLUMN_REFERENCE,
                "table \"{}\" has 3 columns available but {} columns specified",
                exposed,
                aliases.len()
            ));
        }
        let default_names = ["last_value", "log_cnt", "is_called"];
        let types = [ColType::Int8, ColType::Int8, ColType::Bool];
        let mut columns = [ColumnMeta::EMPTY; 3];
        for index in 0..3 {
            columns[index] = ColumnMeta {
                name: SqlName::parse(
                    tref.col_alias
                        .and_then(|aliases| aliases.get(index).copied())
                        .unwrap_or(default_names[index]),
                )?,
                ctype: types[index],
                ..ColumnMeta::EMPTY
            };
        }
        self.defs.set_relation(
            self.n,
            RelationDefinition {
                schema: sequence.schema,
                name: sequence.name,
                n_columns: columns.len(),
                columns: &columns,
            },
            arena,
        )?;
        let rows: &'a [&'a [u8]] = if materialize {
            let role = storage.current_role_slot(txid).ok_or_else(|| {
                sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "current role is not present in the role catalog"
                )
            })?;
            storage.require_schema_usage_as(sequence.schema.as_str(), role, txid)?;
            let object = crate::storage::AccessObject {
                class: crate::storage::AccessClass::Sequence,
                slot: u16::try_from(slot).map_err(|_| {
                    sql_err!(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many sequences")
                })?,
            };
            if !storage.has_object_privilege(
                object,
                role,
                crate::storage::PrivilegeSet::SELECT,
                txid,
            ) {
                return Err(sql_err!(
                    sqlstate::INSUFFICIENT_PRIVILEGE,
                    "permission denied for sequence {}",
                    sequence.name.as_str()
                ));
            }
            let (last_value, is_called) = storage.sequence_value_for(slot, txid);
            let encoded = crate::sql::exec::encode_projected_pub(
                &[
                    Datum::Int8(last_value),
                    Datum::Int8(storage.sequence_log_count_for(slot, txid)),
                    Datum::Bool(is_called),
                ],
                arena,
            )?;
            arena
                .alloc_slice_with(1, |_| encoded)
                .map_err(|_| arena_full())?
        } else {
            &[]
        };
        self.names[self.n] = exposed;
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.n += 1;
        Ok(())
    }

    /// A function on the right of a RIGHT or FULL join may not read the
    /// join's other side, as PostgreSQL's analysis reports.
    fn reject_outer_join_lateral_arguments(&self, tref: &TableRef<'_>) -> Result<(), SqlError> {
        fn check(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Result<(), SqlError> {
            if let Expr::Column { qualifier, name } = *expression
                && let Ok(ResolvedColumn::Table(table, _)) =
                    scope.find_column_unpositioned(qualifier, name)
            {
                return Err(crate::sql::error_position::at_reference(
                    crate::sql::error_position::with_detail(
                        sql_err!(
                            sqlstate::INVALID_COLUMN_REFERENCE,
                            "invalid reference to FROM-clause entry for table \"{}\"",
                            scope.names[table]
                        ),
                        format_args!(
                            "The combining JOIN type must be INNER or LEFT for a LATERAL reference."
                        ),
                    ),
                    qualifier.unwrap_or(name),
                ));
            }
            super::subquery::walk_children(expression, &mut |child| check(scope, child))
        }
        let functions = tref
            .rows_from
            .unwrap_or(core::slice::from_ref(tref))
            .iter()
            .filter_map(|function| function.func_args);
        for arguments in functions {
            for argument in arguments {
                check(self, argument)?;
            }
        }
        Ok(())
    }

    /// Registers a table function (`FROM func(args) alias`) as a derived-table
    /// entry. `materialize` false = schema only (Describe / synth-def path).
    #[allow(clippy::too_many_arguments)]
    fn add_table_func<'a>(
        &mut self,
        storage: &'a Storage,
        tref: &'a TableRef<'a>,
        txid: u32,
        arena: &'a Arena,
        params: &[Datum<'a>],
        sequences: Option<&dyn SequenceAccess>,
        materialize: bool,
        outer: Option<&dyn ColumnLookup<'a>>,
    ) -> Result<(), SqlError>
    where
        'a: 'd,
    {
        let scope_types = ScopeTypes(self);
        let columns = Chained {
            inner: if self.n == 0 {
                &crate::sql::eval::NoColumns
            } else {
                &scope_types
            },
            outer,
        };
        let def_reference = if self.n == 0 && outer.is_none() {
            table_func_def(tref, storage, txid, arena, params)?
        } else {
            table_func_def_outer(tref, storage, txid, arena, params, &columns)?
        };
        if self.right_of_outer_join {
            self.reject_outer_join_lateral_arguments(tref)?;
        }
        let exposed = tref.alias.unwrap_or(tref.table);
        if !exposed.is_empty() {
            self.duplicate_name(exposed)?;
        }
        // Every table function is implicitly lateral to preceding FROM items;
        // the keyword is optional for functions in PostgreSQL. Such rows are
        // built per left-hand row by the scan.
        if (tref.lateral || self.n > 0) && !self.right_of_outer_join {
            self.names[self.n] = exposed;
            self.defs.set_arena(self.n, def_reference);
            self.derived[self.n] = Some(&[]);
            self.lateral[self.n] = true;
            self.func_scalar[self.n] = true;
            self.slots[self.n] = usize::MAX;
            self.n += 1;
            return Ok(());
        }
        let catalog = super::storage_catalog(storage, arena, txid);
        let subqueries = super::ProceduralSubqueries { storage, txid };
        let hooks = crate::sql::eval::EvalHooks {
            catalog: Some(&catalog),
            subquery_executor: Some(&subqueries),
            sequences,
            ..crate::sql::eval::NO_HOOKS
        };
        let rows: &'a [&'a [u8]] = if !materialize {
            &[]
        } else if outer.is_some() {
            table_func_rows_outer(
                tref,
                storage,
                txid,
                arena,
                params,
                &columns,
                Some(&hooks),
                None,
                None,
            )?
        } else {
            table_func_rows_outer(
                tref,
                storage,
                txid,
                arena,
                params,
                &crate::sql::eval::NoColumns,
                Some(&hooks),
                None,
                None,
            )?
        };
        self.names[self.n] = exposed;
        self.defs.set_arena(self.n, def_reference);
        self.derived[self.n] = Some(rows);
        self.slots[self.n] = usize::MAX;
        self.func_scalar[self.n] = true;
        self.n += 1;
        Ok(())
    }

    fn add(
        &mut self,
        storage: &'d Storage,
        tref: &'d TableRef<'d>,
        txid: u32,
        arena: &'d Arena,
    ) -> Result<(), SqlError> {
        // `txid == 0` (schema-only / Describe) resolves against the committed
        // catalog; a real transaction sees its own uncommitted CREATE/DROP.
        let slot = tref.bound_table.map(usize::from).or_else(|| {
            storage
                .resolve_relation(tref.schema, tref.table, txid)
                .and_then(|relation| match relation {
                    crate::storage::ResolvedRelation::Table(slot) => Some(slot),
                    _ => None,
                })
        });
        let Some(slot) = slot else {
            let error = match tref.schema {
                Some(s) => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}.{}\" does not exist",
                    s,
                    tref.table
                ),
                None => sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "relation \"{}\" does not exist",
                    tref.table
                ),
            };
            return Err(crate::sql::error_position::at_reference(
                error,
                tref.schema.unwrap_or(tref.table),
            ));
        };
        let stored_def = self.defs.retain(storage, slot)?;
        let source_scope = self
            .type_sources
            .as_ref()
            .map(|owner| owner.enter())
            .transpose()?;
        let selected = storage.select_type_definition_graph(
            stored_def
                .columns()
                .iter()
                .filter_map(|column| column.user_type),
            txid,
            arena,
        )?;
        if self.type_sources.is_none() {
            self.type_sources = selected;
        }
        drop(source_scope);
        let exposed = match tref.alias {
            Some(alias) => alias,
            None => arena
                .alloc_str(stored_def.name.as_str())
                .map_err(|_| arena_full())?,
        };
        let renamed = if let Some(aliases) = tref.col_alias {
            if aliases.len() > stored_def.n_columns {
                return Err(derived_alias_mismatch(
                    exposed,
                    stored_def.n_columns,
                    aliases.len(),
                ));
            }
            let columns = arena
                .alloc_slice_copy(stored_def.columns())
                .map_err(|_| arena_full())?;
            for (column, alias) in columns.iter_mut().zip(aliases) {
                column.name = SqlName::parse(alias)?;
            }
            Some(RelationDefinition {
                columns,
                ..RelationDefinition::from_definition(stored_def)
            })
        } else {
            None
        };
        let def = renamed.unwrap_or_else(|| RelationDefinition::from_definition(stored_def));
        // Two same-named entries coexist only when both are *unaliased base
        // tables of different schemas* (their references then need the
        // three-part spelling); any other duplicate — aliases, the same
        // table twice — is PostgreSQL's 42712.
        if self.members.iter().any(|member| member.name == exposed) {
            return Err(sql_err!(
                sqlstate::DUPLICATE_ALIAS,
                "table name \"{}\" specified more than once",
                exposed
            ));
        }
        for t in 0..self.n {
            if self.names[t] != exposed {
                continue;
            }
            let both_distinct_tables = tref.alias.is_none()
                && self.defs.get(t).is_some_and(|d| {
                    self.slots[t] != usize::MAX
                        && d.name.as_str() == exposed
                        && d.schema.as_str() != def.schema.as_str()
                });
            if !both_distinct_tables {
                return Err(sql_err!(
                    sqlstate::DUPLICATE_ALIAS,
                    "table name \"{}\" specified more than once",
                    exposed
                ));
            }
        }
        self.names[self.n] = exposed;
        let source = match renamed {
            Some(definition) => ScopeDefinition::Synthetic {
                definition,
                table: Some(slot),
            },
            None => {
                let reused = self.defs.sources[..self.n]
                    .iter()
                    .flatten()
                    .find_map(|source| match source {
                        ScopeDefinition::Retained { table, names } if *table == slot => {
                            Some(*names)
                        }
                        _ => None,
                    });
                let names = match reused {
                    Some(names) => names,
                    None => {
                        let names = arena
                            .alloc_slice_with(stored_def.n_columns, |_| "")
                            .map_err(|_| arena_full())?;
                        for (name, column) in names.iter_mut().zip(stored_def.columns()) {
                            *name = arena
                                .alloc_str(column.name.as_str())
                                .map_err(|_| arena_full())?;
                        }
                        &*names
                    }
                };
                ScopeDefinition::Retained { table: slot, names }
            }
        };
        self.defs.sources[self.n] = Some(source);
        self.slots[self.n] = slot;
        self.authorization_roles[self.n] = tref.authorization_role;
        self.n += 1;
        Ok(())
    }

    /// (table position, column index) for a possibly-qualified name.
    /// Resolves every USING/NATURAL join in `from`: computes the merged
    /// output columns (and the join tree's output order) and synthesizes the
    /// equality predicates (when `arena` is given — the schema-only Describe
    /// path passes None and never evaluates joins).
    fn build_merges(
        &mut self,
        from: &FromClause<'d>,
        scratch_arena: &'d Arena,
        expression_arena: Option<&'d Arena>,
    ) -> Result<(), SqlError> {
        if !from.joins.iter().any(|j| j.natural || j.using.is_some()) {
            return Ok(());
        }
        // Definitions are now available: a merged join needs capacity for its
        // actual columns, not the maximum relation width for every source.
        let merged_capacity = from
            .joins
            .iter()
            .enumerate()
            .map(|(index, join)| {
                if join.natural {
                    self.defs.get(index + 1).expect("resolved").n_columns
                } else {
                    join.using.map_or(0, |using| using.columns.len())
                }
            })
            .sum();
        self.merged = scratch_arena
            .alloc_slice_with(merged_capacity, |_| MergedColumn {
                name: "",
                parts: &[],
                ctype: ColType::Bool,
            })
            .map_err(|_| arena_full())?;
        // The left join tree's output columns, updated join by join.
        let out = scratch_arena
            .alloc_slice_with(self.total_columns(), |_| ResolvedColumn::Table(0, 0))
            .map_err(|_| arena_full())?;
        let mut n_out = 0usize;
        for c in 0..self.defs.get(0).expect("resolved").n_columns {
            out[n_out] = ResolvedColumn::Table(0, c);
            n_out += 1;
        }
        // Output position where the current comma-separated FROM item starts:
        // a USING or NATURAL join sees only its own item's join tree.
        let mut group_start = 0usize;
        for (join_index, join) in from.joins.iter().enumerate() {
            let right_t = join_index + 1;
            let right_def = self.defs.get(right_t).expect("resolved");
            if join.kind == crate::sql::ast::JoinKind::Comma {
                group_start = n_out;
            }
            if !(join.natural || join.using.is_some()) {
                for c in 0..right_def.n_columns {
                    out[n_out] = ResolvedColumn::Table(right_t, c);
                    n_out += 1;
                }
                continue;
            }
            // The using-column list: explicit, or (NATURAL) every left-tree
            // output name the right table also has, in left output order.
            let mut natural_using = [""; MAX_COLUMNS];
            let using = if let Some(clause) = join.using {
                clause.columns
            } else {
                let mut n_using = 0usize;
                for entry in &out[group_start..n_out] {
                    let name = self.output_name(*entry);
                    if right_def.column_index(name).is_some()
                        && !natural_using[..n_using].contains(&name)
                    {
                        natural_using[n_using] = name;
                        n_using += 1;
                    }
                }
                &natural_using[..n_using]
            };
            let mut predicate: Option<&'d Expr<'d>> = None;
            let first_new_merge = self.n_merged;
            for &name in using {
                // The name must be unique in the left tree and present on
                // the right (empirically pinned against PostgreSQL 18.4).
                let mut left_entry = None;
                for (k, entry) in out[..n_out].iter().enumerate().skip(group_start) {
                    if self.output_name(*entry) == name {
                        if left_entry.is_some() {
                            return Err(sql_err!(
                                crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
                                "common column name \"{}\" appears more than once in left table",
                                name
                            ));
                        }
                        left_entry = Some((k, *entry));
                    }
                }
                let Some((left_k, left)) = left_entry else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" specified in USING clause does not exist in left table",
                        name
                    ));
                };
                let Some(right_c) = right_def.column_index(name) else {
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column \"{}\" specified in USING clause does not exist in right table",
                        name
                    ));
                };
                let left_type = self.output_type(left);
                let right_type = right_def.columns[right_c].ctype;
                let Some(ctype) = common_using_type(left_type, right_type) else {
                    // PostgreSQL fails resolving the merged column's `=`
                    // operator at parse time, even over empty tables.
                    return Err(sql_err!(
                        sqlstate::UNDEFINED_FUNCTION,
                        "operator does not exist: {} = {}",
                        left_type.name(),
                        right_type.name()
                    ));
                };
                let (first_part, prior_parts) = match left {
                    ResolvedColumn::Table(t, c) => (Some((t, c)), &[][..]),
                    ResolvedColumn::Merged(m) => (None, self.merged[m].parts),
                };
                let parts = scratch_arena
                    .alloc_slice_with(
                        prior_parts.len() + usize::from(first_part.is_some()) + 1,
                        |index| {
                            first_part
                                .filter(|_| index == 0)
                                .or_else(|| prior_parts.get(index).copied())
                                .unwrap_or((right_t, right_c))
                        },
                    )
                    .map_err(|_| arena_full())?;
                let merge = MergedColumn { name, parts, ctype };
                self.merged[self.n_merged] = merge;
                self.n_merged += 1;
                // Remove the consumed left entry; the merged column is
                // prepended to the output below, after all names resolve.
                out.copy_within(left_k + 1..n_out, left_k);
                n_out -= 1;
                if let Some(arena) = expression_arena {
                    let left_ref = self.output_expression(left, arena)?;
                    let right_ref = arena
                        .alloc(Expr::Column {
                            qualifier: Some(self.names[right_t]),
                            name: self.defs.column_name(right_t, right_c),
                        })
                        .map_err(|_| arena_full())?;
                    let eq = arena
                        .alloc(Expr::Binary {
                            source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                            operator: BinaryOp::Eq,
                            left: left_ref,
                            right: right_ref,
                        })
                        .map_err(|_| arena_full())?;
                    predicate = Some(match predicate {
                        None => eq,
                        Some(prev) => arena
                            .alloc(Expr::Binary {
                                source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                                operator: BinaryOp::And,
                                left: prev,
                                right: eq,
                            })
                            .map_err(|_| arena_full())?,
                    });
                }
            }
            // New output: this join's merged columns first within its FROM
            // item, then the item's remaining left-tree output, then the right
            // table's columns minus the consumed ones.
            let n_new = self.n_merged - first_new_merge;
            if let Some(alias) = join.using.and_then(|using| using.alias) {
                if self.name_taken(alias)
                    || self.using_aliases[..join_index]
                        .iter()
                        .flatten()
                        .any(|prior| prior.name == alias)
                {
                    return Err(sql_err!(
                        sqlstate::DUPLICATE_ALIAS,
                        "table name \"{}\" specified more than once",
                        alias
                    ));
                }
                self.using_aliases[join_index] = Some(UsingAlias {
                    name: alias,
                    first_merged: first_new_merge,
                    n_columns: n_new,
                });
            }
            out.copy_within(group_start..n_out, group_start + n_new);
            for (k, slot) in out[group_start..group_start + n_new].iter_mut().enumerate() {
                *slot = ResolvedColumn::Merged(first_new_merge + k);
            }
            n_out += n_new;
            for c in 0..right_def.n_columns {
                let consumed = (first_new_merge..self.n_merged)
                    .any(|m| self.merged[m].parts.last() == Some(&(right_t, c)));
                if !consumed {
                    out[n_out] = ResolvedColumn::Table(right_t, c);
                    n_out += 1;
                }
            }
            self.join_on[join_index] = predicate;
        }
        self.output = out;
        self.n_output = n_out;
        Ok(())
    }

    /// The exposed name of a join-tree output column.
    pub(crate) fn output_name(&self, entry: ResolvedColumn) -> &'d str {
        match entry {
            ResolvedColumn::Table(t, c) => self.defs.column_name(t, c),
            ResolvedColumn::Merged(m) => self.merged[m].name,
        }
    }

    /// The type of a join-tree output column.
    pub fn output_type(&self, entry: ResolvedColumn) -> ColType {
        match entry {
            ResolvedColumn::Table(t, c) => self.defs.get(t).expect("resolved").columns[c].ctype,
            ResolvedColumn::Merged(m) => self.merged[m].ctype,
        }
    }

    /// The declared collation of a join-tree output column.
    pub(crate) fn output_collation(&self, entry: ResolvedColumn) -> crate::sql::ast::Collation {
        match entry {
            ResolvedColumn::Table(table, column) => {
                self.defs.get(table).expect("resolved").columns[column].collation
            }
            ResolvedColumn::Merged(merged) => self.merged[merged]
                .parts
                .first()
                .map(|&(table, column)| {
                    self.defs.get(table).expect("resolved").columns[column].collation
                })
                .unwrap_or(crate::sql::ast::Collation::None),
        }
    }

    /// Derives a projection's collation while its source scope is still
    /// available, before a derived relation turns it into column metadata.
    pub(crate) fn expression_collation(
        &self,
        storage: &Storage,
        arena: &Arena,
        expression: &Expr<'_>,
    ) -> Result<crate::sql::ast::Collation, SqlError> {
        let catalog = super::storage_catalog(storage, arena, self.txid);
        crate::sql::eval::resolved_expression_collation(
            expression,
            &ScopeTypes(self),
            Some(&catalog),
        )
    }

    pub(crate) fn described_expression_collation(
        &self,
        storage: &Storage,
        arena: &Arena,
        expression: &Expr<'_>,
    ) -> Result<
        (
            crate::sql::ast::Collation,
            crate::sql::types::CollationDerivation,
        ),
        SqlError,
    > {
        let catalog = super::storage_catalog(storage, arena, self.txid);
        crate::sql::eval::described_expression_collation(
            expression,
            &ScopeTypes(self),
            Some(&catalog),
        )
    }

    /// An expression reading a join-tree output column: a qualified column
    /// reference, or (merged) a COALESCE across the contributors.
    pub(super) fn star_expression(
        &self,
        entry: ResolvedColumn,
        arena: &'d Arena,
    ) -> Result<&'d Expr<'d>, SqlError> {
        self.output_expression(entry, arena)
    }

    fn output_expression(
        &self,
        entry: ResolvedColumn,
        arena: &'d Arena,
    ) -> Result<&'d Expr<'d>, SqlError> {
        match entry {
            ResolvedColumn::Table(t, c) => Ok(&*arena
                .alloc(Expr::Column {
                    qualifier: Some(self.names[t]),
                    name: self.defs.column_name(t, c),
                })
                .map_err(|_| arena_full())?),
            ResolvedColumn::Merged(m) => {
                let mc = &self.merged[m];
                let args = arena
                    .alloc_slice_with(mc.parts.len(), |_| &Expr::Null as &'d Expr<'d>)
                    .map_err(|_| arena_full())?;
                for (i, &(t, c)) in mc.parts.iter().enumerate() {
                    args[i] = &*arena
                        .alloc(Expr::Column {
                            qualifier: Some(self.names[t]),
                            name: self.defs.column_name(t, c),
                        })
                        .map_err(|_| arena_full())?;
                }
                Ok(&*arena
                    .alloc(Expr::Call {
                        kind: crate::sql::ast::CallKind::SqlSyntax,
                        bound_oid: None,
                        bound_strict: None,
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                        name: "coalesce",
                        args,
                        argument_names: &[],
                        variadic: false,
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    })
                    .map_err(|_| arena_full())?)
            }
        }
    }

    /// `output_expression` with its names copied into `arena`, for a caller
    /// whose expressions outlive this scope.
    pub(crate) fn copied_output_expression<'a>(
        &self,
        entry: ResolvedColumn,
        arena: &'a Arena,
    ) -> Result<&'a Expr<'a>, SqlError> {
        let column = |table: usize, column: usize| -> Result<&'a Expr<'a>, SqlError> {
            let (qualifier, name) = self.column_reference_name(table, column);
            let qualifier = arena.alloc_str(qualifier).map_err(|_| arena_full())?;
            let name = arena.alloc_str(name).map_err(|_| arena_full())?;
            Ok(&*arena
                .alloc(Expr::Column {
                    qualifier: Some(qualifier),
                    name,
                })
                .map_err(|_| arena_full())?)
        };
        match entry {
            ResolvedColumn::Table(table, index) => column(table, index),
            ResolvedColumn::Merged(merged) => {
                let parts = self.merged[merged].parts;
                let args = arena
                    .alloc_slice_with(parts.len(), |_| &Expr::Null as &'a Expr<'a>)
                    .map_err(|_| arena_full())?;
                for (argument, &(table, index)) in args.iter_mut().zip(parts) {
                    *argument = column(table, index)?;
                }
                Ok(&*arena
                    .alloc(Expr::Call {
                        kind: crate::sql::ast::CallKind::SqlSyntax,
                        bound_oid: None,
                        bound_strict: None,
                        source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                        name: "coalesce",
                        args,
                        argument_names: &[],
                        variadic: false,
                        star: false,
                        distinct: false,
                        order_by: &[],
                        over: None,
                        filter: None,
                    })
                    .map_err(|_| arena_full())?)
            }
        }
    }

    /// Number of `SELECT *` output columns: merged join-tree output when
    /// USING/NATURAL merges exist, else every table's column count.
    pub fn star_columns(&self) -> usize {
        if self.n_output > 0 {
            self.n_output
        } else {
            self.total_columns()
        }
    }

    /// The i-th `SELECT *` output column.
    pub fn star_entry(&self, i: usize) -> ResolvedColumn {
        if self.n_output > 0 {
            return self.output[i];
        }
        let mut k = i;
        for t in 0..self.n {
            let n_cols = self.defs.get(t).expect("resolved").n_columns;
            if k < n_cols {
                return ResolvedColumn::Table(t, k);
            }
            k -= n_cols;
        }
        unreachable!("star_entry index out of range");
    }

    /// The scope index of the FROM item exposed as `name` (for `t.*`).
    /// Whether entry `t` answers to qualifier `q`. A plain qualifier matches
    /// the exposed name; a composed `schema.table` qualifier (a three-part
    /// reference) matches only an unaliased base table of that schema.
    fn entry_answers_to(&self, t: usize, q: &str) -> bool {
        if self.target_aliases.contains(&q) {
            return t == 0;
        }
        match q.split_once('.') {
            None => self.names[t] == q,
            Some((schema, table)) => {
                let Some(def) = self.defs.get(t) else {
                    return false;
                };
                self.names[t] == table
                    && def.name.as_str() == table
                    && def.schema.as_str() == schema
            }
        }
    }

    fn entry_index(&self, name: &str) -> Result<usize, SqlError> {
        let mut found = None;
        for t in 0..self.n {
            if self.entry_answers_to(t, name) {
                if found.is_some() {
                    // Two same-named tables from different schemas: a bare
                    // reference cannot pick one, exactly as PostgreSQL.
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::AMBIGUOUS_ALIAS,
                        "table reference \"{}\" is ambiguous",
                        name
                    ));
                }
                found = Some(t);
            }
        }
        if found.is_none() && self.hidden_members.contains(&name) {
            return Err(unreachable_entry(name));
        }
        found.ok_or_else(|| match name.split_once('.') {
            // A composed qualifier whose table name matches some entry is an
            // *invalid* reference (wrong schema or an alias); one matching
            // nothing is *missing* — PostgreSQL's two wordings.
            Some((schema, table))
                if self.names[..self.n].contains(&table)
                    || (0..self.n).any(|index| {
                        self.defs.get(index).is_some_and(|definition| {
                            definition.schema.as_str() == schema
                                && definition.name.as_str() == table
                        })
                    }) =>
            {
                sql_err!(
                    sqlstate::UNDEFINED_TABLE,
                    "invalid reference to FROM-clause entry for table \"{}\"",
                    table
                )
            }
            Some((_, table)) => sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                table
            ),
            // The name of an aliased table is hidden by its alias.
            None if let Some(alias) = (0..self.n)
                .find(|&index| {
                    self.derived[index].is_none()
                        && self.names[index] != name
                        && self
                            .defs
                            .get(index)
                            .is_some_and(|definition| definition.name.as_str() == name)
                })
                .map(|index| self.names[index]) =>
            {
                crate::sql::error_position::with_computed_hint(
                    sql_err!(
                        sqlstate::UNDEFINED_TABLE,
                        "invalid reference to FROM-clause entry for table \"{}\"",
                        name
                    ),
                    format_args!(
                        "Perhaps you meant to reference the table alias \"{}\".",
                        alias
                    ),
                )
            }
            None => sql_err!(
                sqlstate::UNDEFINED_TABLE,
                "missing FROM-clause entry for table \"{}\"",
                name
            ),
        })
    }

    pub(super) fn using_alias(&self, name: &str) -> Option<UsingAlias<'d>> {
        self.using_aliases
            .iter()
            .flatten()
            .find(|a| a.name == name)
            .copied()
    }

    /// Width of a qualified star, whether it names a FROM item or a merged
    /// USING-column alias.
    pub fn qualified_star_columns(&self, name: &str) -> Result<usize, SqlError> {
        if let Some(alias) = self.using_alias(name) {
            return Ok(alias.n_columns);
        }
        Ok(self.source(name)?.n_columns)
    }

    /// Resolved column at a qualified-star position.
    pub fn qualified_star_entry(
        &self,
        name: &str,
        index: usize,
    ) -> Result<ResolvedColumn, SqlError> {
        if let Some(alias) = self.using_alias(name) {
            return (index < alias.n_columns)
                .then_some(ResolvedColumn::Merged(alias.first_merged + index))
                .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_COLUMN, "column does not exist"));
        }
        let source = self.source(name)?;
        (index < source.n_columns)
            .then_some(ResolvedColumn::Table(source.table, source.first + index))
            .ok_or_else(|| sql_err!(sqlstate::UNDEFINED_COLUMN, "column does not exist"))
    }

    /// If `name` refers to a set-returning-function scan, the type of its single
    /// scalar output column — the type a whole-row reference to it carries. A
    /// storage- or subquery-derived table returns None (its whole-row reference
    /// is a record).
    pub fn func_scalar_type(&self, name: &str) -> Option<ColType> {
        self.func_scalar_column(name)
            .map(|column| self.output_type(column))
    }

    pub(super) fn func_scalar_column(&self, name: &str) -> Option<ResolvedColumn> {
        if let Some(member) = self.member(name) {
            return member
                .scalar
                .then_some(ResolvedColumn::Table(member.table, member.first));
        }
        let t = self.entry_index(name).ok()?;
        if !self.func_scalar[t] {
            return None;
        }
        let def = self.defs.get(t)?;
        (def.n_columns == 1).then_some(ResolvedColumn::Table(t, 0))
    }

    pub fn find_column(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ResolvedColumn, SqlError> {
        self.find_column_unpositioned(qualifier, name)
            .map_err(|error| crate::sql::error_position::column_reference(error, qualifier, name))
    }

    fn find_column_unpositioned(
        &self,
        qualifier: Option<&str>,
        name: &str,
    ) -> Result<ResolvedColumn, SqlError> {
        match qualifier {
            Some(q) => {
                if let Some(alias) = self.using_alias(q) {
                    return (0..alias.n_columns)
                        .map(|index| alias.first_merged + index)
                        .find(|&merged| self.merged[merged].name == name)
                        .map(ResolvedColumn::Merged)
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_COLUMN,
                                "column {}.{} does not exist",
                                q,
                                name
                            )
                        });
                }
                let source = self.source(q)?;
                if let Some(position) = name.strip_prefix('\0') {
                    return position
                        .parse::<usize>()
                        .ok()
                        .filter(|&index| index < source.n_columns)
                        .map(|index| ResolvedColumn::Table(source.table, source.first + index))
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::INTERNAL_ERROR,
                                "positional column is out of range"
                            )
                        });
                }
                let column = if self.member(q).is_some() {
                    self.defs.get(source.table).expect("resolved").columns
                        [source.first..source.first + source.n_columns]
                        .iter()
                        .position(|column| column.name.as_str() == name)
                        .map(|index| source.first + index)
                } else {
                    self.scope_column_index(source.table, name)
                };
                if column.is_some()
                    && self
                        .source_columns(source)
                        .iter()
                        .filter(|column| column.name.as_str() == name)
                        .count()
                        > 1
                {
                    return Err(sql_err!(
                        crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
                        "column reference \"{}\" is ambiguous",
                        name
                    ));
                }
                match column {
                    Some(c) => Ok(ResolvedColumn::Table(source.table, c)),
                    None => Err(sql_err!(
                        sqlstate::UNDEFINED_COLUMN,
                        "column {}.{} does not exist",
                        q,
                        name
                    )),
                }
            }
            None => {
                // Unqualified names resolve against the join tree's output
                // columns: a USING/NATURAL-merged column appears there once,
                // so referencing it is not ambiguous.
                if self.n_output > 0 {
                    let mut found = None;
                    for k in 0..self.n_output {
                        if self.output_name(self.output[k]) == name {
                            if found.is_some() {
                                return Err(sql_err!(
                                    crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
                                    "column reference \"{}\" is ambiguous",
                                    name
                                ));
                            }
                            found = Some(self.output[k]);
                        }
                    }
                    return found
                        .or_else(|| self.func_scalar_column(name))
                        .ok_or_else(|| {
                            sql_err!(
                                sqlstate::UNDEFINED_COLUMN,
                                "column \"{}\" does not exist",
                                name
                            )
                        });
                }
                let mut found = None;
                for t in 0..self.n {
                    if let Some(c) = self.scope_column_index(t, name) {
                        let duplicated = || {
                            self.defs
                                .get(t)
                                .expect("resolved")
                                .columns()
                                .iter()
                                .filter(|column| column.name.as_str() == name)
                                .count()
                                > 1
                        };
                        if found.is_some() || duplicated() {
                            return Err(sql_err!(
                                crate::sql::eval::sqlstate::AMBIGUOUS_COLUMN,
                                "column reference \"{}\" is ambiguous",
                                name
                            ));
                        }
                        found = Some(ResolvedColumn::Table(t, c));
                    }
                }
                found
                    .or_else(|| self.func_scalar_column(name))
                    .ok_or_else(|| {
                        sql_err!(
                            sqlstate::UNDEFINED_COLUMN,
                            "column \"{}\" does not exist",
                            name
                        )
                    })
            }
        }
    }

    pub fn total_columns(&self) -> usize {
        (0..self.n)
            .map(|t| self.defs.get(t).expect("resolved").n_columns)
            .sum()
    }

    /// Width of an entry in flat (table-by-table) row layouts: its visible
    /// columns, plus the member columns of a parenthesized join.
    pub(crate) fn stored_width(&self, table: usize) -> usize {
        let definition = self.defs.get(table).expect("resolved");
        if self.members.iter().any(|member| member.table == table) {
            definition.columns.len()
        } else {
            definition.n_columns
        }
    }

    /// Total width of a flat row over every entry.
    pub(crate) fn stored_columns(&self) -> usize {
        (0..self.n).map(|table| self.stored_width(table)).sum()
    }

    pub(super) fn row_width(&self, table: usize) -> usize {
        self.defs.get(table).expect("resolved").columns.len()
    }

    fn scope_column_index(&self, table: usize, name: &str) -> Option<usize> {
        let definition = self.defs.get(table).expect("resolved");
        definition.column_index(name).or_else(|| {
            definition.columns
                [definition.n_columns..definition.n_columns + self.hidden_columns[table]]
                .iter()
                .position(|column| column.name.as_str() == name)
                .map(|index| definition.n_columns + index)
        })
    }
}

#[cfg(all(test, not(all(miri, feature = "miri-ownership"))))]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mem::budget::Budget;

    fn config() -> Config {
        let mut config = Config::default_dev();
        config.max_tables = 2;
        config.max_catalog_versions_per_object = 1;
        config.query_workspace_slots = 1;
        config.memtable_bytes = 1 << 16;
        config.row_overlay_slots = 16;
        config.txn_rows = 16;
        config.value_index_rows = 32;
        config.max_value_indexes = 2;
        config.max_extension_scripts = 2;
        config.extension_script_bytes = 1 << 16;
        config
    }

    fn definition(name: &str, width: usize) -> crate::storage::TableDefBuilder {
        let mut definition = crate::storage::TableDefBuilder {
            schema: SqlName::parse("public").unwrap(),
            name: SqlName::parse(name).unwrap(),
            n_columns: width,
            ..crate::storage::TableDefBuilder::empty()
        };
        for (index, column) in definition.columns[..width].iter_mut().enumerate() {
            *column = ColumnMeta {
                name: SqlName::parse(crate::stack_format!(64, "c{}", index).as_str()).unwrap(),
                ctype: ColType::Int8,
                ..ColumnMeta::EMPTY
            };
        }
        definition
    }

    #[test]
    fn sequence_schema_and_rows_fit_actual_columns_in_small_arena() {
        let config = config();
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let change = storage
            .create_sequence(
                crate::storage::SequenceCreateSpec {
                    allocation: crate::storage::CatalogAllocation::New,
                    schema: SqlName::parse("public").unwrap(),
                    name: SqlName::parse("narrow_sequence").unwrap(),
                    spec: crate::storage::SeqSpec {
                        data_type: crate::storage::SeqType::Bigint,
                        increment: 1,
                        min_value: 1,
                        max_value: i64::MAX,
                        start_value: 1,
                        cache: 1,
                        cycle: false,
                    },
                    owner: None,
                    generator_for: None,
                    persistence: crate::storage::RelationPersistence::Permanent,
                },
                0,
            )
            .unwrap();
        storage.finish_sequence_change(change, crate::storage::CatalogDdlDecision::Commit);
        for materialize in [false, true] {
            let mut arena_budget = Budget::new(16 << 10);
            let arena = Arena::new(&mut arena_budget, "narrow sequence scope", 16 << 10).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let query = crate::sql::parser::parse_query(
                    "SELECT s.saved,s.called FROM public.narrow_sequence AS s(saved,logged,called)",
                    &arena,
                )
                .unwrap();
                let from = query.from.as_ref().unwrap();
                let scope = if materialize {
                    QueryScope::resolve_exec(&storage, from, None, 0, &arena, &[], None).unwrap()
                } else {
                    QueryScope::resolve_schema(&storage, from, 0, &arena, None).unwrap()
                };
                assert_eq!(scope.defs.get(0).unwrap().n_columns, 3);
                assert_eq!(scope.defs.get(0).unwrap().schema.as_str(), "public");
                assert_eq!(
                    scope.find_column(Some("s"), "saved").unwrap(),
                    ResolvedColumn::Table(0, 0)
                );
                assert_eq!(
                    scope.find_column(Some("s"), "called").unwrap(),
                    ResolvedColumn::Table(0, 2)
                );
                let rows = scope.derived[0].unwrap();
                assert_eq!(rows.len(), usize::from(materialize));
                if let Some(row) = rows.first() {
                    assert!(matches!(
                        crate::sql::exec::decode_projected_pub(row, 0),
                        Datum::Int8(1)
                    ));
                    assert!(matches!(
                        crate::sql::exec::decode_projected_pub(row, 2),
                        Datum::Bool(false)
                    ));
                }
                assert!(arena.used() < 16 << 10);
            });
        }
    }

    #[test]
    fn query_definition_ownership_preserves_images_across_storage_lifecycle() {
        let config = config();
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage.create_table(definition("original", 1)).unwrap();
        let mut sources = [Some(ScopeDefinition::Retained {
            table,
            names: &["c0"],
        })];
        let images = storage.table_definition_images();
        images.definition(&storage, table, 0).unwrap();
        let definitions = QueryDefinitions {
            sources: &mut sources,
            images,
            transaction: 0,
        };
        crate::mem::guard::forbid_alloc(|| {
            let pending_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    definition("pending", 1),
                    &[None; MAX_COLUMNS],
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            let pending = storage.table_definition_images();
            pending.definition(&storage, table, 7).unwrap();
            storage.rollback_table_def(pending_change);
            assert_eq!(
                pending.retained_definition(table, 7).unwrap().name.as_str(),
                "pending"
            );
            let published_change = storage
                .write_table_def(
                    storage.select_table_definition(table, 7).0,
                    definition("published", 1),
                    &[None; MAX_COLUMNS],
                    false,
                    crate::storage::ColumnSet::EMPTY,
                )
                .unwrap();
            storage
                .prepare_definition_publication(
                    7,
                    [published_change].into_iter(),
                    core::iter::empty(),
                )
                .unwrap();
            storage
                .prepare_commit_generations(7, core::iter::empty(), core::iter::empty())
                .unwrap();
            assert_eq!(
                storage.commit_table_def(published_change).unwrap(),
                Some(false)
            );
            storage.finish_definition_publication(7).unwrap();
            storage.release_commit_generations(7);
            storage.release_object_locks(7);
            storage.commit_drop(table);
            assert_eq!(
                storage.create_table(definition("replacement", 1)).unwrap(),
                table
            );
            assert_eq!(definitions.get(0).unwrap().name.as_str(), "original");
            drop(storage);
            assert_eq!(definitions.get(0).unwrap().columns()[0].name.as_str(), "c0");
        });
    }

    #[test]
    fn query_definition_ownership_wide_aliases_share_one_image_and_keep_names() {
        let mut config = config();
        config.definition_image_slots = 3;
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage
            .create_table(definition(
                "wide_source",
                crate::storage::MAX_RELATION_COLUMNS,
            ))
            .unwrap();
        let capacity = config.definition_image_slots;
        let mut owners = Vec::with_capacity(capacity - 1);
        let mut arena_budget = Budget::new(1 << 17);
        let arena = Arena::new(&mut arena_budget, "query definition names", 1 << 17).unwrap();
        let query = crate::sql::parser::parse_query(
            "SELECT a.c0 FROM wide_source a CROSS JOIN wide_source b",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for _ in 1..capacity {
                let owner = storage.table_definition_images();
                owner.definition(&storage, table, 0).unwrap();
                owners.push(owner);
            }
            let scope =
                QueryScope::resolve_schema(&storage, query.from.as_ref().unwrap(), 0, &arena, None)
                    .unwrap();
            assert!(core::ptr::eq(
                scope.defs.stored(0).unwrap(),
                scope.defs.stored(1).unwrap()
            ));
            assert_eq!(
                scope.defs.get(0).unwrap().n_columns,
                crate::storage::MAX_RELATION_COLUMNS
            );
            assert!(core::ptr::eq(
                scope.defs.column_name(0, 0),
                scope.defs.column_name(1, 0)
            ));
            let escaped = scope.output_name(ResolvedColumn::Table(
                0,
                crate::storage::MAX_RELATION_COLUMNS - 1,
            ));
            assert_eq!(escaped, "c1599");
            drop(scope);
            let reused = storage.table_definition_images();
            reused.definition(&storage, table, 0).unwrap();
            assert_eq!(escaped, "c1599");
        });
    }

    #[test]
    fn query_definition_ownership_nested_schema_uses_actual_column_arena_bytes() {
        let mut config = config();
        config.definition_image_slots = 4;
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        storage.create_table(definition("source", 2)).unwrap();
        let mut arena_budget = Budget::new(256 << 10);
        let arena = Arena::new(&mut arena_budget, "nested schema columns", 256 << 10).unwrap();
        assert!(core::mem::size_of::<crate::storage::TableDefBuilder>() > arena.capacity());
        let query = crate::sql::parser::parse_query(
            "SELECT output.c0 FROM (SELECT * FROM \
             (SELECT c0,c1 FROM source) AS inner_output) AS output",
            &arena,
        )
        .unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let scope =
                QueryScope::resolve_schema(&storage, query.from.as_ref().unwrap(), 0, &arena, None)
                    .unwrap();
            let definition = scope.defs.get(0).unwrap();
            assert_eq!(definition.n_columns, 2);
            assert_eq!(definition.columns()[0].ctype, ColType::Int8);
            assert_eq!(definition.columns()[1].ctype, ColType::Int8);
            let name = scope.output_name(ResolvedColumn::Table(0, 1));
            assert_eq!(name, "c1");
            drop(scope);
            let execution = QueryScope::resolve_exec(
                &storage,
                query.from.as_ref().unwrap(),
                None,
                0,
                &arena,
                crate::sql::eval::NO_PARAMS,
                None,
            )
            .unwrap();
            assert_eq!(execution.total_columns(), 2);
            assert_eq!(
                execution.defs.get(0).unwrap().columns()[0].ctype,
                ColType::Int8
            );
            drop(execution);
            assert_eq!(name, "c1");
        });
    }

    #[test]
    fn materialized_aliases_preserve_private_physical_columns_without_exposing_names() {
        let mut config = config();
        config.definition_image_slots = 2;
        config.table_metadata_bytes = 64 << 10;
        config.table_metadata_leases = 128;
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let storage = Storage::new(&config, &mut budget, 0).unwrap();
        let mut arena_budget = Budget::new(16 << 10);
        let arena =
            Arena::new(&mut arena_budget, "private physical CTE columns", 16 << 10).unwrap();
        let parsed =
            crate::sql::parser::parse_query("SELECT renamed FROM source AS alias(renamed)", &arena)
                .unwrap();
        let columns = [
            ColumnMeta {
                name: SqlName::parse("visible").unwrap(),
                ctype: ColType::Int4,
                ..ColumnMeta::EMPTY
            },
            ColumnMeta {
                name: SqlName::parse("internal").unwrap(),
                ctype: ColType::Int8,
                ..ColumnMeta::EMPTY
            },
        ];
        let relation = MaterializedCte {
            column_names: &["visible"],
            column_types: &[(crate::sql::types::oid::INT4, 4, -1)],
            column_collations: &[crate::sql::ast::Collation::None],
            definition: Some(RelationDefinition {
                schema: SqlName::EMPTY,
                name: SqlName::parse("source").unwrap(),
                n_columns: 1,
                columns: &columns,
            }),
            source: crate::sql::ast::MaterializedCteSource::Inline(&[]),
        };
        let from = FromClause {
            base: TableRef {
                cte: Some(&relation),
                ..parsed.from.as_ref().unwrap().base
            },
            joins: &[],
        };
        crate::mem::guard::forbid_alloc(|| {
            let scope = QueryScope::resolve_schema(&storage, &from, 0, &arena, None).unwrap();
            let selected = scope.defs.get(0).unwrap();
            assert_eq!(selected.n_columns, 1);
            assert_eq!(selected.columns.len(), 2);
            assert_eq!(scope.row_width(0), 2);
            assert_eq!(selected.columns()[0].name.as_str(), "renamed");
            assert_eq!(selected.columns[1].name.as_str(), "internal");
            assert_eq!(selected.column_index("internal"), None);
            assert_eq!(scope.star_columns(), 1);
        });
    }

    #[test]
    fn query_definition_ownership_merged_joins_size_buffers_from_actual_columns() {
        let mut config = config();
        config.definition_image_slots = 2;
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        storage.create_table(definition("left_source", 1)).unwrap();
        storage.create_table(definition("right_source", 1)).unwrap();
        for sql in [
            "SELECT merged.* FROM left_source JOIN right_source USING(c0) AS merged",
            "SELECT * FROM left_source NATURAL JOIN right_source",
        ] {
            let mut arena_budget = Budget::new(16 << 10);
            let arena = Arena::new(&mut arena_budget, "actual merged columns", 16 << 10).unwrap();
            let query = crate::sql::parser::parse_query(sql, &arena).unwrap();
            crate::mem::guard::forbid_alloc(|| {
                let scope = QueryScope::resolve_schema(
                    &storage,
                    query.from.as_ref().unwrap(),
                    0,
                    &arena,
                    None,
                )
                .unwrap();
                assert_eq!(scope.output.len(), 2);
                assert_eq!(scope.merged.len(), 1);
                assert_eq!(scope.star_columns(), 1);
                assert_eq!(scope.output_name(scope.star_entry(0)), "c0");
                assert_eq!(scope.output_type(scope.star_entry(0)), ColType::Int8);
            });
        }
    }

    #[test]
    fn query_definition_ownership_keeps_transaction_visible_shapes() {
        let config = config();
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage.create_table(definition("source", 1)).unwrap();
        storage
            .write_table_def(
                storage.select_table_definition(table, 7).0,
                definition("source", 2),
                &[None; MAX_COLUMNS],
                false,
                crate::storage::ColumnSet::EMPTY,
            )
            .unwrap();
        let mut arena_budget = Budget::new(1 << 17);
        let arena = Arena::new(&mut arena_budget, "visible shapes", 1 << 17).unwrap();
        let query = crate::sql::parser::parse_query("SELECT * FROM source", &arena).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            let pending =
                QueryScope::resolve_schema(&storage, query.from.as_ref().unwrap(), 7, &arena, None)
                    .unwrap();
            let committed =
                QueryScope::resolve_schema(&storage, query.from.as_ref().unwrap(), 8, &arena, None)
                    .unwrap();
            assert_eq!(pending.star_columns(), 2);
            assert_eq!(committed.star_columns(), 1);
            assert_eq!(pending.output_name(ResolvedColumn::Table(0, 1)), "c1");
            assert!(!core::ptr::eq(
                pending.defs.get(0).unwrap().columns().as_ptr(),
                committed.defs.get(0).unwrap().columns().as_ptr()
            ));
        });
    }

    #[test]
    fn query_definition_ownership_failed_resolution_releases_capacity() {
        let mut config = config();
        config.definition_image_slots = 3;
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        let table = storage.create_table(definition("first", 1)).unwrap();
        storage.create_table(definition("second", 1)).unwrap();
        let capacity = config.definition_image_slots;
        let mut owners = Vec::with_capacity(capacity - 1);
        let mut arena_budget = Budget::new(1 << 17);
        let arena = Arena::new(&mut arena_budget, "query resolution", 1 << 17).unwrap();
        let query =
            crate::sql::parser::parse_query("SELECT a.c0 FROM first a CROSS JOIN second b", &arena)
                .unwrap();
        let single = crate::sql::parser::parse_query("SELECT c0 FROM first", &arena).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for _ in 1..capacity {
                let owner = storage.table_definition_images();
                owner.definition(&storage, table, 0).unwrap();
                owners.push(owner);
            }
            for _ in 0..8 {
                let error = match QueryScope::resolve_schema(
                    &storage,
                    query.from.as_ref().unwrap(),
                    0,
                    &arena,
                    None,
                ) {
                    Ok(_) => panic!("two images exceed the one free cell"),
                    Err(error) => error,
                };
                assert_eq!(error.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
                assert!(
                    error
                        .message
                        .as_str()
                        .contains("table definition image pool")
                );
                let scope = QueryScope::resolve_schema(
                    &storage,
                    single.from.as_ref().unwrap(),
                    0,
                    &arena,
                    None,
                )
                .unwrap();
                assert_eq!(scope.defs.get(0).unwrap().name.as_str(), "first");
            }
            drop(owners.pop().unwrap());
            let scope =
                QueryScope::resolve_schema(&storage, query.from.as_ref().unwrap(), 0, &arena, None)
                    .unwrap();
            assert_eq!(scope.defs.get(1).unwrap().name.as_str(), "second");
        });
    }

    #[test]
    fn query_definition_ownership_rejects_aliases_and_using_without_leaks() {
        let config = config();
        let mut budget = Budget::new(Storage::extra_budget_bytes(&config) + config.memtable_bytes);
        let mut storage = Storage::new(&config, &mut budget, 0).unwrap();
        storage.create_table(definition("source", 1)).unwrap();
        let mut arena_budget = Budget::new(1 << 22);
        let arena = Arena::new(&mut arena_budget, "query errors", 1 << 22).unwrap();
        crate::mem::guard::forbid_alloc(|| {
            for (sql, state) in [
                (
                    "SELECT * FROM source a CROSS JOIN source a",
                    sqlstate::DUPLICATE_ALIAS,
                ),
                (
                    "SELECT * FROM source a JOIN source b USING (missing)",
                    sqlstate::UNDEFINED_COLUMN,
                ),
                (
                    "SELECT * FROM source a(one, two)",
                    sqlstate::INVALID_COLUMN_REFERENCE,
                ),
            ] {
                let query = crate::sql::parser::parse_query(sql, &arena).unwrap();
                for _ in 0..8 {
                    let error = match QueryScope::resolve_schema(
                        &storage,
                        query.from.as_ref().unwrap(),
                        0,
                        &arena,
                        None,
                    ) {
                        Ok(_) => panic!("invalid scope must fail"),
                        Err(error) => error,
                    };
                    assert_eq!(error.sqlstate, state);
                }
            }
        });
    }
}
