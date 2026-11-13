//! Writes through automatically updatable views, as PostgreSQL's
//! `rewriteTargetView` performs them: every view level between the written
//! view and its base table is resolved, its columns and filter are composed
//! over the base table, and the written columns are checked level by level.

use crate::mem::arena::Arena;
use crate::sql::ast::{Expr, QualName, SelectItem};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql_err;
use crate::storage::{ColumnSet, Storage};

use super::{MAX_PROJ, arena_full, expr_has_window, materialize, parse_stored_query, srf};

/// PostgreSQL's view nesting bound for rewriting, matching view expansion.
pub(crate) const MAX_VIEW_LEVELS: usize = 12;

/// The statement that writes through a view, which selects the wording of
/// PostgreSQL's error for a view that is not automatically updatable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewWrite {
    Insert,
    Update,
    Delete,
    /// `MERGE`'s action of that kind.
    MergeInsert,
    MergeUpdate,
    MergeDelete,
    /// `WITH CHECK OPTION` on the view's definition.
    CheckOption,
}

impl ViewWrite {
    /// Whether PostgreSQL's `view_query_is_auto_updatable` requires an
    /// updatable column (`check_cols`).
    const fn checks_columns(self) -> bool {
        !matches!(self, Self::Delete)
    }

    const fn inserts(self) -> bool {
        matches!(self, Self::Insert | Self::MergeInsert)
    }
}

/// Why PostgreSQL's `view_query_is_auto_updatable` rejects a view, in the
/// order it checks them.
#[derive(Clone, Copy)]
pub(super) enum NotAutoUpdatable {
    Distinct,
    GroupBy,
    Having,
    SetOperation,
    With,
    LimitOffset,
    Aggregate,
    Window,
    SetReturning,
    NotSingleRelation,
    /// Checked for statements that assign columns.
    NoUpdatableColumns,
}

impl NotAutoUpdatable {
    const fn detail(self) -> &'static str {
        match self {
            Self::Distinct => "Views containing DISTINCT are not automatically updatable.",
            Self::GroupBy => "Views containing GROUP BY are not automatically updatable.",
            Self::Having => "Views containing HAVING are not automatically updatable.",
            Self::SetOperation => {
                "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable."
            }
            Self::With => "Views containing WITH are not automatically updatable.",
            Self::LimitOffset => {
                "Views containing LIMIT or OFFSET are not automatically updatable."
            }
            Self::Aggregate => {
                "Views that return aggregate functions are not automatically updatable."
            }
            Self::Window => "Views that return window functions are not automatically updatable.",
            Self::SetReturning => {
                "Views that return set-returning functions are not automatically updatable."
            }
            Self::NotSingleRelation => {
                "Views that do not select from a single table or view are not automatically updatable."
            }
            Self::NoUpdatableColumns => {
                "Views that have no updatable columns are not automatically updatable."
            }
        }
    }

    /// PostgreSQL's `rewriteTargetView` and `DefineView` errors.
    fn error(self, write: ViewWrite, view: &str) -> SqlError {
        let detail = self.detail();
        let (state, hint) = match write {
            ViewWrite::Insert => (
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot insert into view \"{}\"",
                    view
                ),
                "To enable inserting into the view, provide an INSTEAD OF INSERT trigger or an unconditional ON INSERT DO INSTEAD rule.",
            ),
            ViewWrite::Update => (
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot update view \"{}\"",
                    view
                ),
                "To enable updating the view, provide an INSTEAD OF UPDATE trigger or an unconditional ON UPDATE DO INSTEAD rule.",
            ),
            ViewWrite::Delete => (
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot delete from view \"{}\"",
                    view
                ),
                "To enable deleting from the view, provide an INSTEAD OF DELETE trigger or an unconditional ON DELETE DO INSTEAD rule.",
            ),
            ViewWrite::MergeInsert => (
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot insert into view \"{}\"",
                    view
                ),
                "To enable inserting into the view using MERGE, provide an INSTEAD OF INSERT trigger.",
            ),
            ViewWrite::MergeUpdate => (
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot update view \"{}\"",
                    view
                ),
                "To enable updating the view using MERGE, provide an INSTEAD OF UPDATE trigger.",
            ),
            ViewWrite::MergeDelete => (
                sql_err!(
                    sqlstate::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "cannot delete from view \"{}\"",
                    view
                ),
                "To enable deleting from the view using MERGE, provide an INSTEAD OF DELETE trigger.",
            ),
            ViewWrite::CheckOption => {
                let error = sql_err!(
                    sqlstate::FEATURE_NOT_SUPPORTED,
                    "WITH CHECK OPTION is supported only on automatically updatable views"
                );
                crate::sql::eval::stash_full_diagnostic(crate::sql::eval::Diagnostic {
                    detail: None,
                    hint: Some(crate::util::StackStr::from_str(detail)),
                    objects: crate::sql::eval::ErrorObjects::default(),
                });
                return error;
            }
        };
        crate::sql::eval::stash_full_diagnostic(crate::sql::eval::Diagnostic {
            detail: Some(crate::util::StackStr::from_str(detail)),
            hint: Some(crate::util::StackStr::from_str(hint)),
            objects: crate::sql::eval::ErrorObjects::default(),
        });
        state
    }
}

/// How a view column reads the relation below its view.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewColumnSource<'a> {
    /// A plain column of the relation below, by name.
    Column(&'a str),
    /// Any other expression.
    Computed,
    /// A whole-row reference.
    WholeRow,
    /// A system column such as `ctid`.
    SystemColumn,
}

impl ViewColumnSource<'_> {
    /// PostgreSQL's `view_col_is_auto_updatable` detail for a column that is
    /// not a plain column of the relation below.
    const fn blocked_detail(self) -> Option<&'static str> {
        match self {
            Self::Column(_) => None,
            Self::Computed => {
                Some("View columns that are not columns of their base relation are not updatable.")
            }
            Self::WholeRow => {
                Some("View columns that return whole-row references are not updatable.")
            }
            Self::SystemColumn => {
                Some("View columns that refer to system columns are not updatable.")
            }
        }
    }
}

/// One view between the written view and its base table, outermost first.
#[derive(Clone, Copy)]
pub struct ViewLevel<'a> {
    pub slot: usize,
    pub name: &'a str,
    pub columns: &'a [&'a str],
    pub sources: &'a [ViewColumnSource<'a>],
    /// The base-table column each view column reaches through every level
    /// below it; empty when the column is computed at some level.
    pub base_columns: &'a [&'a str],
    /// Durable defaults for the view's columns, applied before the levels
    /// below and the base table's own defaults.
    pub defaults: crate::storage::ViewColumns,
    pub(crate) default_inputs: &'a [(
        crate::storage::StoredInputSection,
        &'a [crate::sql::type_inputs::Binding<'a>],
    )],
    pub check_option: Option<crate::storage::ViewCheckOption>,
    /// The view's own filter, read over the base relation.
    where_clause: Option<&'a Expr<'a>>,
    /// Set on the last resolved level when PostgreSQL refuses to write
    /// through it; the levels below it are not resolved.
    blocked: Option<NotAutoUpdatable>,
    /// The view's output expressions as written, over the relation below.
    items: &'a [&'a Expr<'a>],
    /// The name the view's query gives the relation below it.
    below_alias: &'a str,
}

/// An automatically updatable view resolved down to its base table.
pub struct UpdatableView<'a> {
    /// The base table, fully qualified from the views' own resolution so the
    /// rewritten statement binds the same table regardless of the path.
    pub base: QualName<'a>,
    /// Whether the innermost view reads the base table's inheritance children.
    pub base_inheritance: crate::sql::ast::RelationInheritance,
    /// The name the composed expressions use for the base table.
    base_alias: &'a str,
    base_alias_schema: Option<&'a str>,
    base_column_names: &'a [&'a str],
    pub levels: &'a [ViewLevel<'a>],
    /// The written view's columns, read over the base relation.
    projected: &'a [&'a Expr<'a>],
}

impl<'a> UpdatableView<'a> {
    /// The written view's column names.
    pub fn columns(&self) -> &'a [&'a str] {
        self.levels[0].columns
    }

    fn blocked(&self) -> Option<(NotAutoUpdatable, &'a str)> {
        let last = self.levels.last()?;
        last.blocked.map(|reason| (reason, last.name))
    }

    /// The written view's columns over the base relation named `target`.
    pub(crate) fn projected_over(
        &self,
        target: &'a str,
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<&'a [&'a Expr<'a>], SqlError> {
        let base = self.base_references(target, arena)?;
        let projected = arena
            .alloc_slice_with(self.projected.len(), |_| &Expr::Null as &Expr)
            .map_err(|_| arena_full())?;
        for (slot, expression) in projected.iter_mut().zip(self.projected) {
            *slot = self.over_base(expression, base, storage, txid, arena)?;
        }
        Ok(projected)
    }

    /// Every level's filter, innermost first, over the base relation named
    /// `target`, AND-ed in front of `statement_where`.
    pub(crate) fn filter_over(
        &self,
        target: &'a str,
        statement_where: Option<&'a Expr<'a>>,
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<Option<&'a Expr<'a>>, SqlError> {
        let base = self.base_references(target, arena)?;
        let mut filter = statement_where;
        for level in self.levels {
            if let Some(predicate) = level.where_clause {
                let predicate = self.over_base(predicate, base, storage, txid, arena)?;
                filter = and_where(Some(predicate), filter, arena)?;
            }
        }
        Ok(filter)
    }

    /// The `WITH CHECK OPTION` predicates PostgreSQL enforces, innermost view
    /// first (the order `ExecWithCheckOptions` reports them in), over the
    /// unaliased base table.
    pub(crate) fn check_options(
        &self,
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<&'a [(&'a str, &'a Expr<'a>)], SqlError> {
        let base = self.base_references(self.base.name, arena)?;
        let mut checks = [("", &Expr::Null as &Expr); MAX_VIEW_LEVELS];
        let mut count = 0usize;
        // A cascaded check option of an outer view makes every view below it
        // check its filter too.
        let mut cascaded = false;
        for level in self.levels {
            let own_cascaded = matches!(
                level.check_option,
                Some(crate::storage::ViewCheckOption::Cascaded)
            );
            let checked = level.check_option.is_some() || cascaded;
            cascaded |= own_cascaded;
            if !checked {
                continue;
            }
            if let Some(predicate) = level.where_clause {
                checks[count] = (
                    level.name,
                    self.over_base(predicate, base, storage, txid, arena)?,
                );
                count += 1;
            }
        }
        checks[..count].reverse();
        arena
            .alloc_slice_copy(&checks[..count])
            .map(|checks| &*checks)
            .map_err(|_| arena_full())
    }

    /// The check options and defaults the write enforces.
    pub(crate) fn view_check(
        &self,
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<crate::sql::exec::ViewCheck<'a>, SqlError> {
        let checks = self.check_options(storage, txid, arena)?;
        let defaults = arena
            .alloc_slice_with(self.levels.len(), |index| {
                let level = &self.levels[index];
                crate::sql::exec::ViewInsertDefaults {
                    base_columns: level.base_columns,
                    columns: level.defaults,
                    inputs: level.default_inputs,
                }
            })
            .map_err(|_| arena_full())?;
        Ok(crate::sql::exec::ViewCheck { checks, defaults })
    }

    fn base_references(
        &self,
        target: &'a str,
        arena: &'a Arena,
    ) -> Result<&'a [&'a Expr<'a>], SqlError> {
        let references = arena
            .alloc_slice_with(self.base_column_names.len(), |_| &Expr::Null as &Expr)
            .map_err(|_| arena_full())?;
        for (slot, name) in references.iter_mut().zip(self.base_column_names) {
            *slot = arena
                .alloc(Expr::Column {
                    qualifier: Some(target),
                    name,
                })
                .map_err(|_| arena_full())?;
        }
        Ok(references)
    }

    fn over_base(
        &self,
        expression: &'a Expr<'a>,
        base: &'a [&'a Expr<'a>],
        storage: &Storage,
        txid: u32,
        arena: &'a Arena,
    ) -> Result<&'a Expr<'a>, SqlError> {
        super::cte::substitute_relation_columns(
            expression,
            self.base_alias,
            self.base_alias_schema,
            self.base_column_names,
            base,
            storage,
            txid,
            arena,
        )
    }

    /// PostgreSQL's per-level checks of the written view columns (`written`
    /// indexes [`Self::columns`]): each level's structural check, then the
    /// view defaults an INSERT adds, then its written columns in view order.
    /// Returns the columns each level writes.
    pub(crate) fn check_written(
        &self,
        written: &[bool],
        write: ViewWrite,
    ) -> Result<[ColumnSet; MAX_VIEW_LEVELS], SqlError> {
        let mut sets = [ColumnSet::EMPTY; MAX_VIEW_LEVELS];
        let mut current = [false; MAX_PROJ];
        current[..written.len()].copy_from_slice(written);
        for (index, level) in self.levels.iter().enumerate() {
            if let Some(reason) = level.blocked {
                return Err(reason.error(write, level.name));
            }
            if write.inserts() {
                for (column, flag) in current[..level.columns.len()].iter_mut().enumerate() {
                    if !*flag
                        && level.defaults.default_at(column).is_some_and(|default| {
                            !matches!(default, crate::storage::ColumnDefault::None)
                        })
                    {
                        *flag = true;
                    }
                }
            }
            for (column, source) in level.sources.iter().enumerate() {
                if !current[column] {
                    continue;
                }
                sets[index].insert(column);
                if let Some(detail) = source.blocked_detail() {
                    let verb = match write {
                        ViewWrite::Insert => "insert into",
                        ViewWrite::Update => "update",
                        _ => "merge into",
                    };
                    let error = sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "cannot {} column \"{}\" of view \"{}\"",
                        verb,
                        level.columns[column],
                        level.name
                    );
                    crate::sql::eval::stash_full_diagnostic(crate::sql::eval::Diagnostic {
                        detail: Some(crate::util::StackStr::from_str(detail)),
                        hint: None,
                        objects: crate::sql::eval::ErrorObjects::default(),
                    });
                    return Err(error);
                }
            }
            // Map the written columns onto the relation below.
            let below: &[&str] = match self.levels.get(index + 1) {
                Some(next) => next.columns,
                None => self.base_column_names,
            };
            let mut next = [false; MAX_PROJ];
            for (column, source) in level.sources.iter().enumerate() {
                let ViewColumnSource::Column(name) = source else {
                    continue;
                };
                if !current[column] {
                    continue;
                }
                let Some(position) = below.iter().position(|candidate| candidate == name) else {
                    return Err(sql_err!(
                        sqlstate::INTERNAL_ERROR,
                        "view column \"{}\" is absent from the relation below view \"{}\"",
                        name,
                        level.name
                    ));
                };
                if next[position] {
                    return Err(sql_err!(
                        sqlstate::SYNTAX_ERROR,
                        "multiple assignments to same column \"{}\"",
                        name
                    ));
                }
                next[position] = true;
            }
            current = next;
        }
        Ok(sets)
    }

    /// The columns of each level a statement reading the written view's
    /// columns `top` reads, as PostgreSQL's `adjust_view_column_set` maps a
    /// view's selected columns onto the relation below it: every column the
    /// view column's expression names.
    pub(crate) fn read_columns(&self, top: ColumnSet) -> [ColumnSet; MAX_VIEW_LEVELS] {
        let mut sets = [ColumnSet::EMPTY; MAX_VIEW_LEVELS];
        sets[0] = top;
        for (index, level) in self.levels.iter().enumerate() {
            let Some(next) = self.levels.get(index + 1) else {
                break;
            };
            let mut below = ColumnSet::EMPTY;
            for (column, item) in level.items.iter().enumerate() {
                if !sets[index].contains(column) {
                    continue;
                }
                let whole_row = matches!(level.sources[column], ViewColumnSource::WholeRow);
                for (position, name) in next.columns.iter().enumerate() {
                    if whole_row || super::cte::expr_mentions_column(item, level.below_alias, name)
                    {
                        below.insert(position);
                    }
                }
            }
            sets[index + 1] = below;
        }
        sets
    }

    /// The base-table column a written view column reaches, after
    /// [`Self::check_written`] accepted it.
    pub(crate) fn base_column(&self, view_column: usize) -> &'a str {
        self.levels[0].base_columns[view_column]
    }
}

/// A statement rewritten onto a view's base table.
pub struct ViewRewrite<'a> {
    pub statement: &'a crate::sql::ast::Stmt<'a>,
    /// The columns the statement writes at each view level, outermost first.
    pub written: [ColumnSet; MAX_VIEW_LEVELS],
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
                source_position: crate::sql::ast::SourcePosition::UNKNOWN,
                operator: crate::sql::ast::BinaryOp::And,
                left: a,
                right: b,
            };
            Ok(Some(&*arena.alloc(e).map_err(|_| arena_full())?))
        }
    }
}

/// If `name` is a view, resolve it for DML: `Ok(Some(..))` when the written
/// view itself is auto-updatable (a view below it that is not is reported by
/// [`UpdatableView::check_written`], after the written view's column checks,
/// as PostgreSQL orders them), `Err` when the written view is not
/// (PostgreSQL's error for `write`), `Ok(None)` when it is not a view at all.
/// Description and catalog callers share this metadata-only resolution.
/// Execution locks its target before resolving or selecting rewrite rules.
pub fn resolve_view_for_dml<'a>(
    storage: &Storage,
    name: QualName,
    txid: u32,
    arena: &'a Arena,
    write: ViewWrite,
) -> Result<Option<UpdatableView<'a>>, SqlError> {
    let Some(crate::storage::ResolvedRelation::View(slot)) =
        storage.resolve_relation(name.schema, name.name, txid)
    else {
        return Ok(None);
    };
    let view = resolve_updatable_view(storage, slot, txid, arena, Some(write))?;
    if let Some(reason) = view.levels[0].blocked {
        return Err(reason.error(write, view.levels[0].name));
    }
    Ok(Some(view))
}

/// The error for writing a built-in view whose query reads a function, as
/// `pg_settings` does.
pub fn function_view_write_error(view: &str, write: ViewWrite) -> SqlError {
    NotAutoUpdatable::NotSingleRelation.error(write, view)
}

/// Whether PostgreSQL's `relation_is_updatable` reports a view updatable
/// through every level down to its base table. Catalog rendering uses the
/// predicate the write path enforces, so `information_schema` agrees with
/// what a write does.
pub fn view_is_auto_updatable(
    storage: &Storage,
    slot: usize,
    txid: u32,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let view = resolve_updatable_view(storage, slot, txid, arena, None)?;
    Ok(view.blocked().is_none())
}

/// Whether PostgreSQL's `pg_column_is_updatable` reports the view's column
/// (zero-based) updatable: the view is, and the column reaches a base column.
pub fn view_column_is_updatable(
    storage: &Storage,
    slot: usize,
    column: usize,
    txid: u32,
    arena: &Arena,
) -> Result<bool, SqlError> {
    let view = resolve_updatable_view(storage, slot, txid, arena, None)?;
    Ok(view.blocked().is_none()
        && view.levels[0]
            .base_columns
            .get(column)
            .is_some_and(|name| !name.is_empty()))
}

/// One resolved level before its expressions are composed over the base.
#[derive(Clone, Copy)]
struct RawLevel<'a> {
    level: ViewLevel<'a>,
    /// The view's output expressions, over the relation below it.
    items: &'a [&'a Expr<'a>],
    /// The name the view's query gives the relation below it.
    below_alias: &'a str,
    below_alias_schema: Option<&'a str>,
}

/// Resolves the view in `slot` and every view below it, outermost first,
/// stopping at the first level PostgreSQL refuses to write through. `write`
/// is the statement's write, or `None` for a catalog's updatability check
/// (which requires an updatable column, as an UPDATE does).
fn resolve_updatable_view<'a>(
    storage: &Storage,
    slot: usize,
    txid: u32,
    arena: &'a Arena,
    write: Option<ViewWrite>,
) -> Result<UpdatableView<'a>, SqlError> {
    let check_columns = write.is_none_or(ViewWrite::checks_columns);
    let mut raw: [Option<RawLevel<'a>>; MAX_VIEW_LEVELS] = [None; MAX_VIEW_LEVELS];
    let mut count = 0usize;
    let mut view_slot = slot;
    let mut base = None;
    loop {
        if count == MAX_VIEW_LEVELS {
            let view = storage.view(view_slot);
            return Err(sql_err!(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "view \"{}\" nests too deeply (or references itself)",
                view.name_for(txid).as_str()
            ));
        }
        let (level, below) = resolve_level(storage, view_slot, txid, arena, check_columns)?;
        raw[count] = Some(level);
        count += 1;
        match below {
            Below::Blocked => break,
            Below::View(inner) => {
                if let Some(write) = write
                    && view_writes_itself(storage, inner, write, txid)
                {
                    let outer = storage.view(view_slot);
                    let inner = storage.view(inner);
                    return Err(sql_err!(
                        sqlstate::FEATURE_NOT_SUPPORTED,
                        "writing through view \"{}\" into view \"{}\", which has INSTEAD OF triggers or rewrite rules, is not supported",
                        outer.name_for(txid).as_str(),
                        inner.name_for(txid).as_str()
                    ));
                }
                view_slot = inner;
            }
            Below::Table {
                name,
                inheritance,
                columns,
            } => {
                base = Some((name, inheritance, columns));
                break;
            }
        }
    }
    let raw = &raw[..count];
    let mut levels = [None::<ViewLevel<'a>>; MAX_VIEW_LEVELS];
    let Some((base, base_inheritance, base_column_names)) = base else {
        // A level refuses the write; nothing below it is composed.
        for (target, level) in levels.iter_mut().zip(raw) {
            *target = level.map(|level| level.level);
        }
        let levels = arena
            .alloc_slice_with(count, |index| levels[index].expect("resolved level"))
            .map_err(|_| arena_full())?;
        return Ok(UpdatableView {
            base: QualName {
                schema: None,
                name: "",
            },
            base_inheritance: crate::sql::ast::RelationInheritance::Descendants,
            base_alias: "",
            base_alias_schema: None,
            base_column_names: &[],
            levels,
            projected: &[],
        });
    };
    // Compose from the innermost level outwards: each level's expressions
    // read the composed columns of the level below.
    let innermost = raw[count - 1].expect("resolved level");
    let mut projected = innermost.items;
    let mut below_base_columns: &[&str] = base_column_names;
    for index in (0..count).rev() {
        let level = raw[index].expect("resolved level");
        let mut composed = level.level;
        let base_columns = arena
            .alloc_slice_with(composed.sources.len(), |_| "")
            .map_err(|_| arena_full())?;
        let below_columns: &[&str] = match raw.get(index + 1) {
            Some(next) => next.expect("resolved level").level.columns,
            None => base_column_names,
        };
        for (target, source) in base_columns.iter_mut().zip(composed.sources) {
            if let ViewColumnSource::Column(name) = source
                && let Some(position) = below_columns.iter().position(|column| column == name)
            {
                *target = below_base_columns[position];
            }
        }
        composed.base_columns = base_columns;
        if index + 1 < count {
            let next = raw[index + 1].expect("resolved level");
            let items = arena
                .alloc_slice_with(level.items.len(), |_| &Expr::Null as &Expr)
                .map_err(|_| arena_full())?;
            for (slot, item) in items.iter_mut().zip(level.items) {
                *slot = super::cte::substitute_relation_columns(
                    item,
                    level.below_alias,
                    level.below_alias_schema,
                    next.level.columns,
                    projected,
                    storage,
                    txid,
                    arena,
                )?;
            }
            composed.where_clause = match composed.where_clause {
                Some(predicate) => Some(super::cte::substitute_relation_columns(
                    predicate,
                    level.below_alias,
                    level.below_alias_schema,
                    next.level.columns,
                    projected,
                    storage,
                    txid,
                    arena,
                )?),
                None => None,
            };
            projected = items;
        }
        below_base_columns = composed.base_columns;
        levels[index] = Some(composed);
    }
    let levels = arena
        .alloc_slice_with(count, |index| levels[index].expect("composed level"))
        .map_err(|_| arena_full())?;
    Ok(UpdatableView {
        base,
        base_inheritance,
        base_alias: innermost.below_alias,
        base_alias_schema: innermost.below_alias_schema,
        base_column_names,
        levels,
        projected,
    })
}

/// Whether PostgreSQL would write the view in `slot` with its own INSTEAD OF
/// triggers or rewrite rules rather than through its base relation.
fn view_writes_itself(storage: &Storage, slot: usize, write: ViewWrite, txid: u32) -> bool {
    let (event, rule_event) = match write {
        ViewWrite::Insert | ViewWrite::MergeInsert => (
            crate::sql::ast::TriggerEvents::INSERT,
            crate::storage::RewriteEvent::Insert,
        ),
        ViewWrite::Update | ViewWrite::MergeUpdate => (
            crate::sql::ast::TriggerEvents::UPDATE,
            crate::storage::RewriteEvent::Update,
        ),
        ViewWrite::Delete | ViewWrite::MergeDelete => (
            crate::sql::ast::TriggerEvents::DELETE,
            crate::storage::RewriteEvent::Delete,
        ),
        ViewWrite::CheckOption => return false,
    };
    storage.triggers_for_view(slot, txid).any(|(_, trigger)| {
        matches!(trigger.level, crate::sql::ast::TriggerLevel::Row)
            && matches!(trigger.timing, crate::sql::ast::TriggerTiming::InsteadOf)
            && trigger.events.contains(event)
    }) || storage
        .rules_for(
            crate::storage::RuleTarget::View(slot as u16),
            rule_event,
            txid,
        )
        .next()
        .is_some()
}

enum Below<'a> {
    Blocked,
    View(usize),
    Table {
        name: QualName<'a>,
        inheritance: crate::sql::ast::RelationInheritance,
        columns: &'a [&'a str],
    },
}

const SYSTEM_COLUMNS: [&str; 6] = ["ctid", "xmin", "xmax", "cmin", "cmax", "tableoid"];

/// One view's `view_query_is_auto_updatable` checks and column sources.
fn resolve_level<'a>(
    storage: &Storage,
    view_slot: usize,
    txid: u32,
    arena: &'a Arena,
    check_columns: bool,
) -> Result<(RawLevel<'a>, Below<'a>), SqlError> {
    let view = storage.view(view_slot);
    let name = arena
        .alloc_str(view.name_for(txid).as_str())
        .map_err(|_| arena_full())?;
    let view_columns = view.columns_for(txid);
    let columns = arena
        .alloc_slice_with(view_columns.len(), |_| "")
        .map_err(|_| arena_full())?;
    for (target, column) in columns.iter_mut().zip(view_columns.names()) {
        *target = arena.alloc_str(column.as_str()).map_err(|_| arena_full())?;
    }
    let mut level = ViewLevel {
        slot: view_slot,
        name,
        columns,
        sources: &[],
        base_columns: &[],
        defaults: *view_columns,
        default_inputs: view.default_inputs_for(txid).detached_sections(arena)?,
        check_option: view.check_option_for(txid),
        where_clause: None,
        blocked: None,
        items: &[],
        below_alias: "",
    };
    let blocked = |mut level: ViewLevel<'a>, reason| {
        level.blocked = Some(reason);
        Ok((
            RawLevel {
                level,
                items: &[],
                below_alias: "",
                below_alias_schema: None,
            },
            Below::Blocked,
        ))
    };
    // The body re-resolves under the view creator's search path.
    let user = crate::sql::eval::funcs::system::session_user_owned();
    let query = storage.snapshot_view_query(view_slot, txid, arena)?;
    let view_path = storage.compute_path(query.creation_path.as_str(), user.as_str(), txid);
    // Copy the definition into the arena so the parsed AST no longer borrows
    // storage (the caller then takes a mutable storage borrow to run the DML).
    let sql = arena
        .alloc_str(query.sql.as_str())
        .map_err(|_| arena_full())?;
    let input_source = query.input_source.unwrap_or(sql);
    let dependencies = query.dependencies;
    let sel = parse_stored_query(sql, input_source, storage, txid, dependencies.view(), arena)?;
    // `view_query_is_auto_updatable`'s checks, in its order. The query is
    // read as written: writing through a view needs no SELECT privilege on
    // the relations below it, which expanding nested views would check.
    let items_have = |found: &dyn Fn(&Expr<'_>) -> bool| {
        sel.items.iter().any(|item| match item {
            SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                found(expression)
            }
            SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => false,
        })
    };
    let reason = if sel.set_body.is_some() {
        // A set operation's leaves carry the clauses PostgreSQL checks first.
        Some(NotAutoUpdatable::SetOperation)
    } else if sel.distinct || !sel.distinct_on.is_empty() {
        Some(NotAutoUpdatable::Distinct)
    } else if !sel.group_by.is_empty() || !sel.grouping_sets.is_empty() {
        Some(NotAutoUpdatable::GroupBy)
    } else if sel.having.is_some() {
        Some(NotAutoUpdatable::Having)
    } else if !sel.with.is_empty() {
        Some(NotAutoUpdatable::With)
    } else if sel.limit.is_some() || sel.offset.is_some() {
        Some(NotAutoUpdatable::LimitOffset)
    } else if items_have(&|expression| {
        materialize::expression_has_aggregate_use(expression, storage, txid)
    }) {
        Some(NotAutoUpdatable::Aggregate)
    } else if items_have(&|expression| expr_has_window(expression)) {
        Some(NotAutoUpdatable::Window)
    } else if items_have(&|expression| srf::expression_has_project_set(expression, storage, txid)) {
        Some(NotAutoUpdatable::SetReturning)
    } else {
        None
    };
    if let Some(reason) = reason {
        return blocked(level, reason);
    }
    let Some(from) = sel.from.as_ref().filter(|from| {
        from.joins.is_empty()
            && from.base.subquery.is_none()
            && from.base.func_args.is_none()
            && from.base.rows_from.is_none()
    }) else {
        return blocked(level, NotAutoUpdatable::NotSingleRelation);
    };
    let below_alias = from.base.alias.unwrap_or(from.base.table);
    let below_alias_schema = from
        .base
        .alias
        .is_none()
        .then_some(from.base.schema)
        .flatten();
    let (below, below_columns) = match super::cte::stored_relation_binding(
        &from.base,
        storage,
        txid,
        Some(view_path),
        Some(dependencies.view()),
    )?
    .0
    {
        Some(crate::storage::ResolvedRelation::View(inner)) => {
            let inner_view = storage.view(inner);
            let inner_columns = inner_view.columns_for(txid);
            let names = arena
                .alloc_slice_with(inner_columns.len(), |_| "")
                .map_err(|_| arena_full())?;
            for (target, column) in names.iter_mut().zip(inner_columns.names()) {
                *target = arena.alloc_str(column.as_str()).map_err(|_| arena_full())?;
            }
            (Below::View(inner), &*names)
        }
        Some(crate::storage::ResolvedRelation::Table(table)) => {
            let definition = storage.table_def(table, txid);
            let schema = arena
                .alloc_str(definition.schema.as_str())
                .map_err(|_| arena_full())?;
            let names = arena
                .alloc_slice_with(definition.columns().len(), |_| "")
                .map_err(|_| arena_full())?;
            for (target, column) in names.iter_mut().zip(definition.columns()) {
                *target = arena
                    .alloc_str(column.name.as_str())
                    .map_err(|_| arena_full())?;
            }
            let table_name = arena
                .alloc_str(definition.name.as_str())
                .map_err(|_| arena_full())?;
            (
                Below::Table {
                    name: QualName {
                        schema: Some(schema),
                        name: table_name,
                    },
                    inheritance: from.base.inheritance,
                    columns: names,
                },
                &*names,
            )
        }
        _ => return blocked(level, NotAutoUpdatable::NotSingleRelation),
    };
    let expand = |expression: &'a Expr<'a>| {
        super::cte::expand_stored_expression_under(
            expression,
            storage,
            txid,
            view_path,
            dependencies.view(),
            arena,
        )
    };
    // Each output column, with `*` expanded over the relation below.
    let mut items = [&Expr::Null as &Expr; MAX_PROJ];
    let mut sources = [ViewColumnSource::Computed; MAX_PROJ];
    let mut count = 0usize;
    let mut push = |expression: &'a Expr<'a>, source| {
        if count == MAX_PROJ {
            return Err(sql_err!(
                sqlstate::TOO_MANY_COLUMNS,
                "view \"{}\" has too many columns",
                name
            ));
        }
        items[count] = expression;
        sources[count] = source;
        count += 1;
        Ok(())
    };
    let source_of = |expression: &Expr<'_>| -> ViewColumnSource<'a> {
        let (qualifier, column) = match expression {
            Expr::Column { qualifier, name } => (*qualifier, *name),
            Expr::SchemaColumn { table, name, .. } => (Some(*table), *name),
            Expr::WholeRow(_) => return ViewColumnSource::WholeRow,
            _ => return ViewColumnSource::Computed,
        };
        if qualifier.is_some_and(|qualifier| qualifier != below_alias) {
            return ViewColumnSource::Computed;
        }
        match below_columns.iter().find(|candidate| **candidate == column) {
            Some(found) => ViewColumnSource::Column(*found),
            None if SYSTEM_COLUMNS.contains(&column) => ViewColumnSource::SystemColumn,
            None if column == below_alias && qualifier.is_none() => ViewColumnSource::WholeRow,
            None => ViewColumnSource::Computed,
        }
    };
    for item in sel.items {
        match item {
            SelectItem::Wildcard(_) => {
                for column in below_columns {
                    let expression = arena
                        .alloc(Expr::Column {
                            qualifier: Some(below_alias),
                            name: column,
                        })
                        .map_err(|_| arena_full())?;
                    push(expression, ViewColumnSource::Column(column))?;
                }
            }
            SelectItem::TableWildcard(qualifier) if *qualifier == below_alias => {
                for column in below_columns {
                    let expression = arena
                        .alloc(Expr::Column {
                            qualifier: Some(below_alias),
                            name: column,
                        })
                        .map_err(|_| arena_full())?;
                    push(expression, ViewColumnSource::Column(column))?;
                }
            }
            SelectItem::Expr { expression, .. } => {
                push(expand(expression)?, source_of(expression))?
            }
            SelectItem::RecordStar(expression) => push(expression, ViewColumnSource::Computed)?,
            SelectItem::TableWildcard(_) => push(&Expr::Null, ViewColumnSource::Computed)?,
        }
    }
    if count != columns.len() {
        return Err(sql_err!(
            sqlstate::INTERNAL_ERROR,
            "view \"{}\" has an invalid output-column definition",
            name
        ));
    }
    let sources = arena
        .alloc_slice_copy(&sources[..count])
        .map_err(|_| arena_full())?;
    level.sources = sources;
    if check_columns
        && !sources
            .iter()
            .any(|source| matches!(source, ViewColumnSource::Column(_)))
    {
        return blocked(level, NotAutoUpdatable::NoUpdatableColumns);
    }
    level.where_clause = match sel.where_clause {
        Some(predicate) => Some(expand(predicate)?),
        None => None,
    };
    let items = arena
        .alloc_slice_copy(&items[..count])
        .map_err(|_| arena_full())?;
    level.items = items;
    level.below_alias = below_alias;
    Ok((
        RawLevel {
            level,
            items,
            below_alias,
            below_alias_schema,
        },
        below,
    ))
}
