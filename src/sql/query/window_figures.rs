//! The figures `EXPLAIN ANALYZE` prints for a window query's `Sort` and
//! `WindowAgg`: the sort tuple PostgreSQL's planner gives the window's
//! input (`make_window_input_target`), sized row by row, and the shape of
//! the window its partition buffer replays (`window_store`).

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{Expr, Select, SelectItem, WindowSpec};
use crate::sql::eval::{SqlError, sqlstate};
use crate::sql::types::Datum;
use crate::sql_err;

use super::postgres_memory::{
    SortTuples, TuplesortModel, TuplesortSpace, minimal_tuple_len_with_tids, value_written_size,
};
use super::window_store::{Frame, Function, Window};
use super::{QueryScope, ResolvedColumn, arena_full, instrument};

fn unsupported(what: &str) -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "EXPLAIN ANALYZE of {what} is not supported"
    )
}

/// A window whose input tuple pos3ql cannot charge against `work_mem`.
fn refused(what: &str) -> SqlError {
    sql_err!(sqlstate::FEATURE_NOT_SUPPORTED, "{what} is not supported")
}

/// One column of the tuple a window's input sort holds.
#[derive(Clone, Copy)]
pub(crate) enum InputColumn<'a> {
    /// A column of a relation of the query.
    Column(usize, usize),
    /// A window key computed below the window.
    Expression(&'a Expr<'a>),
    /// A window function of a clause below, by its position in the query's
    /// window functions.
    Result(usize),
}

fn resolve(scope: &QueryScope<'_>, expression: &Expr<'_>) -> Option<(usize, usize)> {
    match expression {
        Expr::Column { qualifier, name } => {
            match scope.find_column_unsuggested(*qualifier, name).ok()? {
                ResolvedColumn::Table(table, column) => Some((table, column)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Collects the columns `expression` reads as `pull_var_clause` does under
/// `PVC_RECURSE_WINDOWFUNCS`: a window function's arguments, not its window.
fn pull_columns<'a>(
    scope: &QueryScope<'_>,
    expression: &'a Expr<'a>,
    out: &mut ArenaList<'a, InputColumn<'a>>,
) -> Result<(), SqlError> {
    match expression {
        Expr::Column { qualifier, name } => match scope.find_column_unsuggested(*qualifier, name) {
            Ok(ResolvedColumn::Table(table, column)) => {
                push_once(out, InputColumn::Column(table, column))
            }
            Ok(ResolvedColumn::TableOid(_)) => push_once(out, InputColumn::Expression(expression)),
            Ok(ResolvedColumn::Merged(merged)) => {
                for &(table, column) in scope.merged_vars(merged) {
                    push_once(out, InputColumn::Column(table, column))?;
                }
                Ok(())
            }
            Err(_) => Ok(()),
        },
        // A whole-row reference is one column: a composite datum.
        Expr::WholeRow(_) => push_once(out, InputColumn::Expression(expression)),
        Expr::Call {
            args,
            order_by,
            filter,
            over: Some(_),
            ..
        } => {
            for argument in *args {
                pull_columns(scope, argument, out)?;
            }
            for order in *order_by {
                pull_columns(scope, order.expression, out)?;
            }
            if let Some(filter) = filter {
                pull_columns(scope, filter, out)?;
            }
            Ok(())
        }
        _ => {
            super::subquery::walk_children(expression, &mut |child| pull_columns(scope, child, out))
        }
    }
}

/// Adds `column` to a tuple's columns unless it holds it.
fn push_once<'a>(
    columns: &mut ArenaList<'a, InputColumn<'a>>,
    column: InputColumn<'a>,
) -> Result<(), SqlError> {
    let held = columns.as_slice().iter().any(|held| match (held, &column) {
        (InputColumn::Column(a, b), InputColumn::Column(c, d)) => (a, b) == (c, d),
        (InputColumn::Expression(a), InputColumn::Expression(b)) => a == b,
        _ => false,
    });
    if held {
        return Ok(());
    }
    columns.push(column).map_err(|_| arena_full())
}

#[derive(Clone, Copy)]
struct Target<'a> {
    column: Option<(usize, usize)>,
    expression: Option<&'a Expr<'a>>,
    window: bool,
    /// A USING-merged column, whose alias variable the planner expands.
    merged: Option<usize>,
}

/// The tuple of a window's input (`make_window_input_target`): the entries
/// of the processed target list that are window keys, then the columns the
/// others read, each once. A query whose select list the planner reads
/// differently from pos3ql fails with `0A000`, as its sort cannot be charged.
pub(crate) fn input_target<'a>(
    scope: &QueryScope<'a>,
    statement: &'a Select<'a>,
    win_nodes: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<&'a [InputColumn<'a>], SqlError> {
    // PostgreSQL refuses a window under `FOR UPDATE`, so the tuple holds no
    // row identity.
    debug_assert!(statement.locking.is_empty());
    let mut targets = ArenaList::new(arena);
    for item in statement.items {
        match item {
            SelectItem::Expr { expression, .. } => targets
                .push(Target {
                    column: resolve(scope, expression),
                    expression: Some(expression),
                    window: false,
                    merged: None,
                })
                .map_err(|_| arena_full())?,
            SelectItem::Wildcard(_) => {
                for index in 0..scope.star_columns() {
                    targets
                        .push(star_target(scope, scope.star_entry(index), arena)?)
                        .map_err(|_| arena_full())?;
                }
            }
            SelectItem::TableWildcard(name) => {
                for index in 0..scope.qualified_star_columns(name)? {
                    targets
                        .push(star_target(
                            scope,
                            scope.qualified_star_entry(name, index)?,
                            arena,
                        )?)
                        .map_err(|_| arena_full())?;
                }
            }
            // The fields are read from the record's own columns.
            SelectItem::RecordStar(base) => targets
                .push(Target {
                    column: None,
                    expression: Some(base),
                    window: false,
                    merged: None,
                })
                .map_err(|_| arena_full())?,
        }
    }
    let item_count = targets.len();
    let same = |held: &Target<'a>, key: &Expr<'a>| match (held.column, resolve(scope, key)) {
        (Some(left), Some(right)) => left == right,
        _ => held.expression.is_some_and(|held| held == key),
    };
    // The entry a key names, by position, output name or equal expression,
    // else a new one the list lacks.
    let keyed = |targets: &mut ArenaList<'a, Target<'a>>,
                 key: &'a Expr<'a>,
                 by_alias: bool,
                 positional: bool|
     -> Result<usize, SqlError> {
        let found = match key {
            // An integer in a window's keys is a constant, not a position.
            Expr::Int(position) if positional && *position >= 1 => {
                let position = *position as usize - 1;
                (position < item_count).then_some(position)
            }
            _ => None,
        }
        .or_else(|| match key {
            Expr::Column {
                qualifier: None,
                name,
            } if by_alias => statement.items.iter().position(
                |item| matches!(item, SelectItem::Expr { alias: Some(alias), .. } if alias == name),
            ),
            _ => None,
        })
        .or_else(|| targets.as_slice().iter().position(|held| same(held, key)));
        if let Some(found) = found {
            return Ok(found);
        }
        targets
            .push(Target {
                column: resolve(scope, key),
                expression: Some(key),
                window: false,
                merged: None,
            })
            .map_err(|_| arena_full())?;
        Ok(targets.len() - 1)
    };
    for order in statement.order_by {
        keyed(&mut targets, order.expression, true, true)?;
    }
    for key in statement.distinct_on {
        keyed(&mut targets, key, false, true)?;
    }
    let specs = statement
        .windows
        .iter()
        .copied()
        .chain(win_nodes.iter().filter_map(|node| match node {
            Expr::Call {
                over: Some(spec), ..
            } => Some(*spec),
            _ => None,
        }));
    for spec in specs {
        let active = win_nodes
            .iter()
            .any(|node| matches!(node, Expr::Call { over: Some(used), .. } if **used == *spec));
        for key in spec
            .order_by
            .iter()
            .map(|order| order.expression)
            .chain(spec.partition_by.iter().copied())
        {
            let at = keyed(&mut targets, key, false, false)?;
            if active && let Ok(slice) = targets.as_mut_slice() {
                slice[at].window = true;
            }
        }
    }
    let mut input = ArenaList::new(arena);
    for target in targets.as_slice().iter().filter(|target| target.window) {
        input
            .push(match (target.column, target.expression) {
                (Some((table, column)), _) => InputColumn::Column(table, column),
                (None, Some(expression)) => InputColumn::Expression(expression),
                (None, None) => unreachable!("a target is a column or an expression"),
            })
            .map_err(|_| arena_full())?;
    }
    let mut columns = ArenaList::new(arena);
    for target in targets.as_slice().iter().filter(|target| !target.window) {
        if let Some(merged) = target.merged {
            for &(table, column) in scope.merged_vars(merged) {
                push_once(&mut columns, InputColumn::Column(table, column))?;
            }
            continue;
        }
        match (target.column, target.expression) {
            (Some((table, column)), _) => {
                push_once(&mut columns, InputColumn::Column(table, column))?
            }
            (None, Some(expression)) => pull_columns(scope, expression, &mut columns)?,
            (None, None) => unreachable!("a target is a column or an expression"),
        }
    }
    for &column in columns.as_slice() {
        push_once(&mut input, column)?;
    }
    Ok(input.as_slice())
}

/// The target-list entry a `*` or `t.*` position expands to.
fn star_target<'a>(
    scope: &QueryScope<'a>,
    entry: ResolvedColumn,
    arena: &'a Arena,
) -> Result<Target<'a>, SqlError> {
    Ok(match entry {
        ResolvedColumn::Table(table, column) => Target {
            column: Some((table, column)),
            expression: None,
            window: false,
            merged: None,
        },
        ResolvedColumn::TableOid(_) => Target {
            column: None,
            expression: Some(scope.copied_output_expression(entry, arena)?),
            window: false,
            merged: None,
        },
        // The planner reads the alias variable's columns, not the merged
        // value: an expression stands for it as a window key.
        ResolvedColumn::Merged(merged) => Target {
            column: None,
            expression: Some(scope.copied_output_expression(entry, arena)?),
            window: false,
            merged: Some(merged),
        },
    })
}

/// The window PostgreSQL runs for the query's one window clause, or why its
/// buffer is not modelled.
pub(crate) fn shape<'a>(
    win_nodes: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<Window<'a>, SqlError> {
    let mut functions = ArenaList::new(arena);
    let mut aggregates = false;
    let mut frame_independent = true;
    let mut window: Option<&WindowSpec<'_>> = None;
    for node in win_nodes {
        let Expr::Call {
            name,
            over: Some(spec),
            ..
        } = node
        else {
            return Err(sql_err!(sqlstate::INTERNAL_ERROR, "not a window function"));
        };
        if window.is_some_and(|held| *held != **spec) {
            return Err(unsupported("a query with several window clauses"));
        }
        window = Some(spec);
        if spec.frame.is_some() {
            return Err(unsupported("a window with a frame clause"));
        }
        let bare = name.strip_prefix("pg_catalog.").unwrap_or(name);
        let function = [
            ("row_number", Function::RowNumber),
            ("rank", Function::Rank),
            ("dense_rank", Function::DenseRank),
        ]
        .into_iter()
        .find_map(|(known, function)| bare.eq_ignore_ascii_case(known).then_some(function));
        match function {
            Some(function) => functions.push(function).map_err(|_| arena_full())?,
            None => {
                if [
                    "percent_rank",
                    "cume_dist",
                    "ntile",
                    "lag",
                    "lead",
                    "first_value",
                    "last_value",
                    "nth_value",
                ]
                .iter()
                .any(|known| bare.eq_ignore_ascii_case(known))
                {
                    return Err(unsupported("this window function"));
                }
                aggregates = true;
                frame_independent = false;
            }
        }
    }
    let spec = window.ok_or_else(|| unsupported("a query without window functions"))?;
    Ok(Window {
        frame: if frame_independent {
            Frame::Rows
        } else {
            Frame::Range
        },
        ordered: !spec.order_by.is_empty(),
        functions: functions.as_slice(),
        aggregates,
    })
}

/// The tuple of one row of a window's input and how a sort writes it.
pub(crate) struct RowSize {
    /// The minimal tuple's `t_len`, or a datum's size when the tuple holds
    /// one value.
    pub(crate) length: usize,
    pub(crate) written: usize,
}

/// Sizes the input row `value` yields the columns of, as the sort and the
/// window's buffer hold it; `None` when a value has no PostgreSQL size here.
pub(crate) fn row_size<'a>(
    source: super::postgres_memory::SizeSource<'_>,
    target: &[InputColumn<'a>],
    arena: &'a Arena,
    mut value: impl FnMut(&InputColumn<'a>) -> Result<Datum<'a>, SqlError>,
) -> Result<Option<RowSize>, SqlError> {
    let values = arena
        .alloc_slice_with(target.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (slot, column) in values.iter_mut().zip(target) {
        *slot = value(column)?;
    }
    Ok(source.with(|sizing| {
        let length = minimal_tuple_len_with_tids(Some(sizing), values.iter().copied(), 0)?;
        let written = if let [only] = &*values {
            4 + value_written_size(
                Some(sizing),
                only,
                matches!(target[0], InputColumn::Expression(_)),
            )?
        } else {
            length - 10 + 4
        };
        Some(RowSize { length, written })
    }))
}

/// The error of `EXPLAIN ANALYZE` over a row with no PostgreSQL size here.
pub(crate) fn unsized_row() -> SqlError {
    refused("a window over a value with no PostgreSQL size here")
}

/// A sort's accounting of a window's input, fed in the scan's order.
#[derive(Clone, Copy)]
pub(crate) struct InputSort {
    model: TuplesortModel,
}

impl InputSort {
    pub(crate) fn new(target: &[InputColumn<'_>]) -> Self {
        Self {
            model: TuplesortModel::new(
                if target.len() == 1 {
                    SortTuples::Datum
                } else {
                    SortTuples::Bump
                },
                None,
            ),
        }
    }

    pub(crate) fn push(&mut self, size: &RowSize) {
        self.model.push(size.length, size.written);
    }

    /// Feeds one row; the row count of the run it made `dumptuples` write.
    pub(crate) fn push_run(&mut self, size: &RowSize) -> Option<usize> {
        self.model.push(size.length, size.written)
    }

    /// The last run the sort writes when its input ends.
    pub(crate) fn finish_run(&mut self) -> Option<usize> {
        self.model.finish_run()
    }

    /// Whether the rows outgrew `work_mem`.
    pub(crate) fn spilled(&self) -> bool {
        self.model.spilled()
    }

    /// The tapes the spilled sort merges.
    pub(crate) fn merge_order(&self) -> Option<usize> {
        self.model.merge_order()
    }

    /// The method and space the sort reports.
    pub(crate) fn space(self) -> instrument::SortSpace {
        match self.model.space(None) {
            TuplesortSpace::Memory(bytes) => instrument::SortSpace {
                method: instrument::SortMethod::Quicksort,
                bytes,
            },
            TuplesortSpace::Disk(bytes) => instrument::SortSpace {
                method: instrument::SortMethod::ExternalMerge,
                bytes,
            },
        }
    }
}
