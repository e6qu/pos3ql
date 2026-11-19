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
    SortTuples, TuplesortModel, TuplesortSpace, datum_written_size, minimal_tuple_len_with_tids,
};
use super::window_store::{Frame, Function, Window};
use super::{QueryScope, ResolvedColumn, arena_full, instrument};

fn unsupported(what: &str) -> SqlError {
    sql_err!(
        sqlstate::FEATURE_NOT_SUPPORTED,
        "EXPLAIN ANALYZE of {what} is not supported"
    )
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
    out: &mut ArenaList<'a, (usize, usize)>,
) -> Result<(), SqlError> {
    match expression {
        Expr::Column { .. } => {
            if let Some(column) = resolve(scope, expression)
                && !out.as_slice().contains(&column)
            {
                out.push(column).map_err(|_| arena_full())?;
            }
            Ok(())
        }
        Expr::WholeRow(_) => Err(unsupported("a window over a whole-row reference")),
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

#[derive(Clone, Copy)]
struct Target<'a> {
    column: Option<(usize, usize)>,
    expression: Option<&'a Expr<'a>>,
    window: bool,
}

/// The tuple of a window's input (`make_window_input_target`): the entries
/// of the processed target list that are window keys, then the columns the
/// others read, each once.
pub(crate) fn input_target<'a>(
    scope: &QueryScope<'a>,
    statement: &'a Select<'a>,
    win_nodes: &[&'a Expr<'a>],
    arena: &'a Arena,
) -> Result<&'a [InputColumn<'a>], SqlError> {
    if !statement.locking.is_empty() {
        return Err(unsupported("a window under row locks"));
    }
    let mut targets = ArenaList::new(arena);
    for item in statement.items {
        match item {
            SelectItem::Expr { expression, .. } => targets
                .push(Target {
                    column: resolve(scope, expression),
                    expression: Some(expression),
                    window: false,
                })
                .map_err(|_| arena_full())?,
            SelectItem::Wildcard(_) => {
                for index in 0..scope.star_columns() {
                    let ResolvedColumn::Table(table, column) = scope.star_entry(index) else {
                        return Err(unsupported("a window over a derived column list"));
                    };
                    targets
                        .push(Target {
                            column: Some((table, column)),
                            expression: None,
                            window: false,
                        })
                        .map_err(|_| arena_full())?;
                }
            }
            _ => return Err(unsupported("a window under this select list")),
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
                 by_alias: bool|
     -> Result<usize, SqlError> {
        let found = match key {
            Expr::Int(position) if *position >= 1 => {
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
            })
            .map_err(|_| arena_full())?;
        Ok(targets.len() - 1)
    };
    for order in statement.order_by {
        keyed(&mut targets, order.expression, true)?;
    }
    for key in statement.distinct_on {
        keyed(&mut targets, key, false)?;
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
            let at = keyed(&mut targets, key, false)?;
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
        match (target.column, target.expression) {
            (Some(column), _) => {
                if !columns.as_slice().contains(&column) {
                    columns.push(column).map_err(|_| arena_full())?;
                }
            }
            (None, Some(expression)) => pull_columns(scope, expression, &mut columns)?,
            (None, None) => unreachable!("a target is a column or an expression"),
        }
    }
    for &(table, column) in columns.as_slice() {
        let held = input.as_slice().iter().any(|entry| {
            matches!(entry, InputColumn::Column(held_table, held_column)
                if (*held_table, *held_column) == (table, column))
        });
        if !held {
            input
                .push(InputColumn::Column(table, column))
                .map_err(|_| arena_full())?;
        }
    }
    Ok(input.as_slice())
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
/// window's buffer hold it.
pub(crate) fn row_size<'a>(
    target: &[InputColumn<'a>],
    arena: &'a Arena,
    mut value: impl FnMut(&InputColumn<'a>) -> Result<Datum<'a>, SqlError>,
) -> Result<RowSize, SqlError> {
    let values = arena
        .alloc_slice_with(target.len(), |_| Datum::Null)
        .map_err(|_| arena_full())?;
    for (slot, column) in values.iter_mut().zip(target) {
        *slot = value(column)?;
    }
    let length = minimal_tuple_len_with_tids(values.iter().copied(), 0)
        .ok_or_else(|| unsupported("a window over a value with no PostgreSQL size here"))?;
    let written = if let [only] = values {
        4 + datum_written_size(only, matches!(target[0], InputColumn::Expression(_)))
    } else {
        length - 10 + 4
    };
    Ok(RowSize { length, written })
}

/// A sort's accounting of a window's input, fed in the scan's order.
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
