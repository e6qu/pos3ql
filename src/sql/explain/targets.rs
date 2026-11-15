//! The target lists `EXPLAIN VERBOSE` prints as each node's `Output:`.
//!
//! PostgreSQL's planner gives every plan node a target list
//! (`createplan.c`): a scan or join computes the query's scan/join target,
//! an upper node (`Agg`, `WindowAgg`, `ProjectSet`) computes its own, and
//! the nodes that do not project (`Sort`, `Limit`, `LockRows`, `Unique`,
//! `Hash`, `Materialize`) return their input's. A scan whose parent accepts
//! any column order returns the relation's physical columns instead
//! (`use_physical_tlist`). `show_plan_tlist` prints each entry, qualifying
//! columns when the range table holds more than one relation; an entry an
//! upper node reads from its input that is not a plain column prints in
//! parentheses (`get_variable` over `OUTER_VAR`).

use core::fmt::Write as _;

use crate::mem::arena::{Arena, ArenaList};
use crate::sql::ast::{Expr, Returning, Select, SelectItem};
use crate::sql::catalog::deparse::{Column, Columns, Context, Destination, Joined};
use crate::sql::eval::{SqlError, arena_full, sqlstate};
use crate::sql::query::{self, QueryScope, ResolvedColumn};
use crate::sql_err;
use crate::storage::{Storage, TableDef};
use crate::util::StackStr;

/// The longest `Output:` list a node holds.
pub(super) const OUTPUT_BYTES: usize = 1_024;

pub(super) type Output = StackStr<OUTPUT_BYTES>;

/// One target-list entry as the node holding it prints it.
#[derive(Clone, Copy)]
struct Entry<'a> {
    text: &'a str,
    /// A plain column (a `Var`), which an upper node reading it prints bare.
    var: bool,
    /// Already printed as read from an input, in parentheses when needed.
    referenced: bool,
}

impl<'a> Entry<'a> {
    /// The entry as a node reading it from its input prints it.
    fn referenced(self, arena: &'a Arena) -> Result<Self, SqlError> {
        if self.var || self.referenced {
            return Ok(self);
        }
        let text = arena
            .alloc_str_display(format_args!("({})", self.text))
            .map_err(|_| arena_full())?;
        Ok(Self {
            text,
            var: false,
            referenced: true,
        })
    }
}

/// One entry of the query's processed target list (`processed_tlist`):
/// select-list items, then the `ORDER BY`, `GROUP BY` and window keys the
/// list lacks, then the row identities a locking clause reads.
#[derive(Clone, Copy)]
struct Target<'a, 'e> {
    expression: Option<&'e Expr<'e>>,
    entry: Entry<'a>,
    /// A `GROUP BY` key (`sortgroupref` of a grouping clause).
    group: bool,
    /// A key of a window's `PARTITION BY` or `ORDER BY`.
    window: bool,
    /// Any sort or grouping reference (`ressortgroupref`).
    labelled: bool,
    /// The window function this entry is, by its clause index.
    window_clause: Option<usize>,
    /// The relation whose row identity (`ctid`) this entry is.
    identity: Option<usize>,
}

/// What a scan's parent asks of its target list (`CP_*` flags).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Demand {
    /// `CP_EXACT_TLIST`: exactly the computed target.
    Exact,
    /// `CP_SMALL_TLIST`: only the columns needed.
    Small,
    /// `CP_LABEL_TLIST`: any columns, but grouping references must be
    /// distinct plain columns.
    Label,
    /// No demand: the physical columns when the relation allows them.
    Any,
}

/// The target lists of one query block's nodes.
pub(super) struct BlockTargets<'a> {
    /// The scan/join target: what the top scan or join computes.
    scan_join: &'a [Entry<'a>],
    /// Whether every labelled entry of the scan/join target is a distinct
    /// plain column, as a physical target list under `CP_LABEL_TLIST`
    /// requires.
    labels_plain: bool,
    /// Per relation, the columns needed above its scan in the order its
    /// target list (`reltarget`) holds them, with the join position above
    /// which each is last needed (`usize::MAX`: by the query's output).
    needed: &'a [(usize, Entry<'a>, usize)],
    /// Per relation, its physical columns; None when it must project
    /// (row identities or whole-row references are read).
    physical: &'a [Option<&'a [Entry<'a>]>],
    /// Per stage, its output.
    stages: &'a [Output],
    /// The relations in join order.
    order: &'a [usize],
}

/// The relation names and columns a block's expressions read, as
/// PostgreSQL's deparser names them: a stored table's, a derived table's
/// or function's alias (`q.x`), a pulled-up `VALUES` list's
/// `"*VALUES*".column1`.
fn relations<'a>(
    scope: &'a QueryScope<'_>,
    statement: &Select<'_>,
    arena: &'a Arena,
) -> Result<&'a [Joined<'a>], Fail> {
    let items = statement
        .from
        .as_ref()
        .map(|from| core::iter::once(&from.base).chain(from.joins.iter().map(|join| &join.table)));
    let mut items = items.into_iter().flatten();
    let joined = arena
        .alloc_slice_with(scope.n, |_| None::<Joined<'a>>)
        .map_err(|_| arena_full())?;
    for (table, slot) in joined.iter_mut().enumerate() {
        let item = items.next();
        if let Some(definition) = scope.defs.stored(table) {
            *slot = Some(Joined::new(scope.names[table], definition));
            continue;
        }
        let Some(definition) = scope.defs.get(table) else {
            return Err(unsupported());
        };
        let values = item.is_some_and(|item| item.subquery.and_then(super::values_list).is_some());
        let columns = arena
            .alloc_slice_with(definition.n_columns, |index| Column {
                name: definition.columns[index].name.as_str(),
                type_oid: None,
                merged: None,
            })
            .map_err(|_| arena_full())?;
        if values {
            for (index, column) in columns.iter_mut().enumerate() {
                let printed = arena
                    .alloc_str_display(format_args!("column{}", index + 1))
                    .map_err(|_| arena_full())?;
                column.merged = Some((Some("*VALUES*"), printed));
            }
        }
        *slot = Some(Joined {
            unqualified: true,
            printed: crate::sql::catalog::deparse::printed_name(scope.names[table]),
            ..Joined::rule_columns(scope.names[table], Columns::Derived(columns))
        });
    }
    Ok(&*arena
        .alloc_slice_with(scope.n, |table| joined[table].expect("set above"))
        .map_err(|_| arena_full())?)
}

/// Why a block's target lists were not built.
#[expect(
    clippy::large_enum_variant,
    reason = "a short-lived error value; runtime memory forbids boxing it"
)]
enum Fail {
    /// An entry or shape outside this module's coverage: the caller keeps
    /// the output columns' names.
    Unsupported,
    Error(SqlError),
}

impl From<SqlError> for Fail {
    fn from(error: SqlError) -> Self {
        Self::Error(error)
    }
}

const fn unsupported() -> Fail {
    Fail::Unsupported
}

/// The node stages a block plans, bottom up, as their names.
#[derive(Clone, Copy)]
pub(super) struct Stage<'s> {
    pub name: &'s str,
    /// A gating or postponed-list `Result` that returns its input's rows.
    pub passthrough: bool,
}

impl<'a> BlockTargets<'a> {
    /// The target lists of `statement`'s nodes. `qualify` is
    /// `show_plan_tlist`'s prefixing rule: the statement's range table
    /// holds more than one relation. `Err` with [`unsupported`]'s state when
    /// an entry is outside the deparser's coverage. `aggregate` says the
    /// first aggregating stage groups the query; a later `HashAggregate` or
    /// a lone one of an unaggregated query applies `DISTINCT`.
    /// `bottom_computes`: the first stage is the block's join or its
    /// `Result` without FROM, which computes the scan/join target.
    #[expect(clippy::too_many_arguments, reason = "plan context")]
    pub(super) fn new(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        scope: Option<&'a QueryScope<'_>>,
        qualify: bool,
        stages: &[Stage<'_>],
        order: &'a [usize],
        window_names: &[&str],
        aggregate: bool,
        bottom_computes: bool,
        arena: &'a Arena,
    ) -> Result<Option<Self>, SqlError> {
        match Self::build(
            storage,
            txid,
            statement,
            scope,
            qualify,
            stages,
            order,
            window_names,
            aggregate,
            bottom_computes,
            arena,
        ) {
            Ok(targets) => Ok(Some(targets)),
            Err(Fail::Unsupported) => Ok(None),
            Err(Fail::Error(error)) => Err(error),
        }
    }

    #[expect(clippy::too_many_arguments, reason = "plan context")]
    fn build(
        storage: &'a Storage,
        txid: u32,
        statement: &'a Select<'a>,
        scope: Option<&'a QueryScope<'_>>,
        qualify: bool,
        stages: &[Stage<'_>],
        order: &'a [usize],
        window_names: &[&str],
        aggregate: bool,
        bottom_computes: bool,
        arena: &'a Arena,
    ) -> Result<Self, Fail> {
        let joined = match scope {
            Some(scope) => relations(scope, statement, arena)?,
            None => &[],
        };
        let relation_count = scope.map_or(0, |scope| scope.n);
        let context = Context {
            storage,
            txid,
            table: None,
            parameters: &[],
            joined,
            pretty: false,
            qualify,
            bare: None,
            domain_value: None,
            sublinks: Some(&super::registered_sublink),
            references: None,
            folded: true,
            arena,
            dependencies: None,
        };
        let resolve = |expression: &Expr<'_>| -> Option<(usize, usize)> {
            match expression {
                Expr::Column { qualifier, name } => {
                    match scope?.find_column_unsuggested(*qualifier, name).ok()? {
                        ResolvedColumn::Table(table, column) => Some((table, column)),
                        _ => None,
                    }
                }
                _ => None,
            }
        };
        let same = |left: &Expr<'_>, right: &Expr<'_>| -> bool {
            match (resolve(left), resolve(right)) {
                (Some(left), Some(right)) => left == right,
                _ => left == right,
            }
        };
        let deparse = |expression: &Expr<'_>| -> Result<Entry<'a>, Fail> {
            let text = crate::sql::catalog::deparse::deparse_expression(
                &context,
                expression,
                Destination::Value,
                arena,
            )?
            .ok_or_else(unsupported)?;
            Ok(Entry {
                text,
                var: matches!(expression, Expr::Column { .. } | Expr::WholeRow(_)),
                referenced: false,
            })
        };
        let column_expression = |table: usize, column: usize| -> Result<&'a Expr<'a>, Fail> {
            let scope = scope.ok_or_else(unsupported)?;
            let name = scope.defs.column_name(table, column);
            Ok(&*arena
                .alloc(Expr::Column {
                    qualifier: Some(
                        arena
                            .alloc_str(scope.names[table])
                            .map_err(|_| arena_full())?,
                    ),
                    name: arena.alloc_str(name).map_err(|_| arena_full())?,
                })
                .map_err(|_| arena_full())?)
        };
        let column_entry = |table: usize, column: usize| -> Result<Entry<'a>, Fail> {
            deparse(column_expression(table, column)?)
        };

        // The window functions and their clauses' names.
        let mut window_calls = ArenaList::new(arena);
        let mut window_clauses: &[usize] = &[];
        let mut window_count = 0usize;
        if let Some(scope) = scope
            && stages.iter().any(|stage| stage.name == "WindowAgg")
        {
            let columns = query::CatalogScopeCols {
                scope,
                outer: None,
                storage,
                txid,
                arena,
            };
            for item in statement.items {
                if let SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) =
                    item
                {
                    query::collect_windows(expression, &mut window_calls, storage, txid, &columns)?;
                }
            }
            for order in statement.order_by {
                query::collect_windows(
                    order.expression,
                    &mut window_calls,
                    storage,
                    txid,
                    &columns,
                )?;
            }
            let clauses = query::window_clauses(statement, window_calls.as_slice(), arena)?;
            window_clauses = clauses.of_node;
            window_count = clauses.clauses.len();
            if window_names.len() != window_count {
                return Err(unsupported());
            }
        }
        let window_calls = window_calls.as_slice();
        // A window function as EXPLAIN prints it: `OVER` its clause's name.
        let window_entry = |expression: &'a Expr<'a>| -> Result<Option<(Entry<'a>, usize)>, Fail> {
            let Some(at) = window_calls
                .iter()
                .position(|call| core::ptr::eq(*call, expression))
            else {
                return Ok(None);
            };
            let clause = window_clauses[at];
            let Expr::Call {
                over: Some(spec), ..
            } = expression
            else {
                return Err(unsupported());
            };
            let named = arena
                .alloc(crate::sql::ast::WindowSpec {
                    name: Some(
                        arena
                            .alloc_str(window_names[clause])
                            .map_err(|_| arena_full())?,
                    ),
                    ..**spec
                })
                .map_err(|_| arena_full())?;
            let mut call = *expression;
            if let Expr::Call { over, .. } = &mut call {
                *over = Some(named);
            }
            let call = arena.alloc(call).map_err(|_| arena_full())?;
            Ok(Some((deparse(call)?, clause)))
        };

        // The processed target list.
        let mut targets = ArenaList::new(arena);
        let push = |targets: &mut ArenaList<'a, Target<'a, 'a>>, target| {
            targets.push(target).map_err(|_| arena_full())
        };
        for item in statement.items {
            match item {
                SelectItem::Expr { expression, .. } => {
                    let (entry, window_clause) = match window_entry(expression)? {
                        Some((entry, clause)) => (entry, Some(clause)),
                        None => (deparse(expression)?, None),
                    };
                    push(
                        &mut targets,
                        Target {
                            expression: Some(expression),
                            entry,
                            group: false,
                            window: false,
                            labelled: false,
                            window_clause,
                            identity: None,
                        },
                    )?;
                }
                SelectItem::Wildcard(_) => {
                    let scope = scope.ok_or_else(unsupported)?;
                    for index in 0..scope.star_columns() {
                        let ResolvedColumn::Table(table, column) = scope.star_entry(index) else {
                            return Err(unsupported());
                        };
                        let expression = column_expression(table, column)?;
                        push(
                            &mut targets,
                            Target {
                                expression: Some(expression),
                                ..Target::column(deparse(expression)?)
                            },
                        )?;
                    }
                }
                SelectItem::TableWildcard(name) => {
                    let scope = scope.ok_or_else(unsupported)?;
                    let table = scope.names[..scope.n]
                        .iter()
                        .position(|held| held == name)
                        .ok_or_else(unsupported)?;
                    let columns = scope.defs.get(table).ok_or_else(unsupported)?.n_columns;
                    for column in 0..columns {
                        let expression = column_expression(table, column)?;
                        push(
                            &mut targets,
                            Target {
                                expression: Some(expression),
                                ..Target::column(deparse(expression)?)
                            },
                        )?;
                    }
                }
                SelectItem::RecordStar(_) => return Err(unsupported()),
            }
        }
        let item_count = targets.len();
        // The select-list entry a key names, by position, output name or
        // equal expression, else a new resjunk entry.
        let keyed = |targets: &mut ArenaList<'a, Target<'a, 'a>>,
                     key: &'a Expr<'a>,
                     by_alias: bool|
         -> Result<usize, Fail> {
            let found = match key {
                Expr::Int(position) if *position >= 1 => {
                    let position = *position as usize - 1;
                    (position < item_count).then_some(position)
                }
                _ => None,
            }
            .or_else(|| {
                match key {
                Expr::Column {
                    qualifier: None,
                    name,
                } if by_alias => statement.items.iter().position(|item| {
                    matches!(item, SelectItem::Expr { alias: Some(alias), .. } if alias == name)
                }),
                _ => None,
            }
            })
            .or_else(|| {
                targets.as_slice()[..targets.len()]
                    .iter()
                    .position(|target| target.expression.is_some_and(|held| same(held, key)))
            });
            if let Some(found) = found {
                return Ok(found);
            }
            let entry = deparse(key)?;
            push(
                targets,
                Target {
                    expression: Some(key),
                    entry,
                    group: false,
                    window: false,
                    labelled: false,
                    window_clause: None,
                    identity: None,
                },
            )?;
            Ok(targets.len() - 1)
        };
        let mark = |targets: &mut ArenaList<'a, Target<'a, 'a>>,
                    at: usize,
                    apply: &dyn Fn(&mut Target<'a, 'a>)| {
            let mut target = targets.as_slice()[at];
            apply(&mut target);
            if let Ok(slice) = targets.as_mut_slice() {
                slice[at] = target;
            }
        };
        for order in statement.order_by {
            let at = keyed(&mut targets, order.expression, true)?;
            mark(&mut targets, at, &|target| target.labelled = true);
        }
        for key in statement.group_by {
            let at = keyed(&mut targets, key, false)?;
            mark(&mut targets, at, &|target| {
                target.labelled = true;
                target.group = true;
            });
        }
        if statement.distinct {
            for at in 0..item_count {
                mark(&mut targets, at, &|target| target.labelled = true);
            }
        }
        for key in statement.distinct_on {
            let at = keyed(&mut targets, key, false)?;
            mark(&mut targets, at, &|target| target.labelled = true);
        }
        // The `WINDOW` clause's definitions, used or not, then the inline
        // specifications (`transformWindowDefinitions` over `p_windowdefs`).
        let specs = statement
            .windows
            .iter()
            .copied()
            .chain(window_calls.iter().filter_map(|call| match call {
                Expr::Call {
                    over: Some(spec), ..
                } => Some(*spec),
                _ => None,
            }));
        for spec in specs {
            // Only the keys of a window some call uses partition or order
            // the window input (`make_window_input_target`).
            let active = window_calls
                .iter()
                .any(|call| matches!(call, Expr::Call { over: Some(used), .. } if **used == *spec));
            for key in spec
                .partition_by
                .iter()
                .copied()
                .chain(spec.order_by.iter().map(|order| order.expression))
            {
                let at = keyed(&mut targets, key, false)?;
                mark(&mut targets, at, &|target| {
                    target.labelled = true;
                    target.window |= active;
                });
            }
        }
        // A locking clause reads each relation's row identity.
        let locking = !statement.locking.is_empty();
        if let Some(scope) = scope.filter(|_| locking) {
            for table in 0..scope.n {
                if scope.defs.stored(table).is_none() {
                    return Err(unsupported());
                }
                let text = if qualify {
                    arena
                        .alloc_str_display(format_args!(
                            "{}.ctid",
                            crate::sql::catalog::deparse::printed_name(scope.names[table])
                        ))
                        .map_err(|_| arena_full())?
                } else {
                    "ctid"
                };
                push(
                    &mut targets,
                    Target {
                        identity: Some(table),
                        ..Target::column(Entry {
                            text,
                            var: true,
                            referenced: false,
                        })
                    },
                )?;
            }
        }
        let targets = targets.as_slice();

        // The columns each relation's scan must return, in `reltarget`
        // order: the output's, `HAVING`'s, then each join condition's.
        let mut needed_list = ArenaList::new(arena);
        let whole_row = arena
            .alloc_slice_with(relation_count, |_| false)
            .map_err(|_| arena_full())?;
        let position_of = |table: usize| order.iter().position(|&held| held == table);
        let need = |expression: &Expr<'_>,
                    above: usize,
                    needed_list: &mut ArenaList<'a, (usize, Entry<'a>, usize)>,
                    whole_row: &mut [bool]|
         -> Result<(), Fail> {
            let mut failed = None;
            columns_of(expression, &mut |column| {
                if failed.is_some() {
                    return;
                }
                if let Expr::WholeRow(name) = column {
                    if let Some(table) = scope.and_then(|scope| {
                        scope.names[..scope.n].iter().position(|held| held == name)
                    }) {
                        whole_row[table] = true;
                    }
                    return;
                }
                let Some((table, index)) = resolve(column) else {
                    return;
                };
                let result = (|| -> Result<(), Fail> {
                    let entry = column_entry(table, index)?;
                    match needed_list
                        .as_slice()
                        .iter()
                        .position(|(_, held, _)| held.text == entry.text)
                    {
                        Some(at) => {
                            let (_, held, last) = needed_list.as_slice()[at];
                            needed_list.as_mut_slice().map_err(|_| arena_full())?[at] =
                                (table, held, last.max(above));
                        }
                        None => needed_list
                            .push((table, entry, above))
                            .map_err(|_| arena_full())?,
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    failed = Some(error);
                }
            });
            failed.map_or(Ok(()), Err)
        };
        for target in targets {
            match (target.expression, target.identity) {
                (Some(expression), _) => {
                    need(expression, usize::MAX, &mut needed_list, whole_row)?;
                }
                (None, Some(table)) => needed_list
                    .push((table, target.entry, usize::MAX))
                    .map_err(|_| arena_full())?,
                (None, None) => {}
            }
        }
        if let Some(having) = statement.having {
            need(having, usize::MAX, &mut needed_list, whole_row)?;
        }
        let mut conditions = ArenaList::new(arena);
        if let Some(from) = statement.from.as_ref() {
            for join in from.joins {
                if let Some(on) = join.on {
                    conjuncts(on, &mut conditions)?;
                }
            }
        }
        if let Some(condition) = statement.where_clause {
            conjuncts(condition, &mut conditions)?;
        }
        for condition in conditions.as_slice() {
            let mut tables = 0u64;
            let mut last = 0usize;
            columns_of(condition, &mut |column| {
                if let Some((table, _)) = resolve(column) {
                    tables |= 1 << table.min(63);
                    last = last.max(position_of(table).unwrap_or(0));
                }
            });
            if tables.count_ones() >= 2 {
                need(condition, last, &mut needed_list, whole_row)?;
            }
        }
        let needed = needed_list.as_slice();
        let physical = arena
            .alloc_slice_with(relation_count, |_| None)
            .map_err(|_| arena_full())?;
        for (table, slot) in physical.iter_mut().enumerate() {
            let Some(scope) = scope.filter(|_| !locking && !whole_row[table]) else {
                continue;
            };
            let columns = scope.defs.get(table).ok_or_else(unsupported)?.n_columns;
            let mut list = ArenaList::new(arena);
            for column in 0..columns {
                list.push(column_entry(table, column)?)
                    .map_err(|_| arena_full())?;
            }
            *slot = Some(list.as_slice());
        }

        // The scan/join target: the grouping input target under an
        // aggregate (`make_group_input_target`), the window input target
        // under windows (`make_window_input_target`), else the final one.
        let aggregated = aggregate;
        let windowed = stages.iter().any(|stage| stage.name == "WindowAgg");
        let project_set = stages.iter().any(|stage| stage.name == "ProjectSet");
        let mut scan_join = ArenaList::new(arena);
        let mut labels = ArenaList::new(arena);
        let add_vars =
            |expression: &Expr<'_>, scan_join: &mut ArenaList<'a, Entry<'a>>| -> Result<(), Fail> {
                let mut failed = None;
                columns_of(expression, &mut |column| {
                    if failed.is_some() {
                        return;
                    }
                    let entry = match column {
                        Expr::WholeRow(_) => deparse(column),
                        _ => match resolve(column) {
                            Some((table, index)) => column_entry(table, index),
                            None => return,
                        },
                    };
                    match entry {
                        Ok(entry) => {
                            if !scan_join
                                .as_slice()
                                .iter()
                                .any(|held| held.text == entry.text)
                                && scan_join.push(entry).is_err()
                            {
                                failed = Some(Fail::Error(arena_full()));
                            }
                        }
                        Err(error) => failed = Some(error),
                    }
                });
                failed.map_or(Ok(()), Err)
            };
        if aggregated || windowed {
            let kept = |target: &Target<'_, '_>| {
                if aggregated {
                    target.group
                } else {
                    target.window
                }
            };
            for target in targets.iter().filter(|target| kept(target)) {
                scan_join.push(target.entry).map_err(|_| arena_full())?;
                labels.push(target.entry).map_err(|_| arena_full())?;
            }
            for target in targets.iter().filter(|target| !kept(target)) {
                match target.expression {
                    Some(expression) => add_vars(expression, &mut scan_join)?,
                    None => {
                        if !scan_join
                            .as_slice()
                            .iter()
                            .any(|held| held.text == target.entry.text)
                        {
                            scan_join.push(target.entry).map_err(|_| arena_full())?;
                        }
                    }
                }
            }
            if aggregated && let Some(having) = statement.having {
                add_vars(having, &mut scan_join)?;
            }
        } else if project_set {
            for target in targets {
                match target.expression {
                    Some(expression) => add_vars(expression, &mut scan_join)?,
                    None => scan_join.push(target.entry).map_err(|_| arena_full())?,
                }
            }
        } else {
            for target in targets {
                scan_join.push(target.entry).map_err(|_| arena_full())?;
                if target.labelled {
                    labels.push(target.entry).map_err(|_| arena_full())?;
                }
            }
        }
        let labels = labels.as_slice();
        let labels_plain = labels.iter().enumerate().all(|(at, entry)| {
            entry.var && !labels[..at].iter().any(|held| held.text == entry.text)
        });
        let scan_join = scan_join.as_slice();

        // Each stage's output, bottom up.
        let outputs = arena
            .alloc_slice_with(stages.len(), |_| Output::new())
            .map_err(|_| arena_full())?;
        let mut below: &[Entry<'a>] = scan_join;
        let mut windows_done = 0usize;
        let mut grouped = false;
        for (index, stage) in stages.iter().enumerate() {
            let current: &[Entry<'a>] = match stage.name {
                "Aggregate" | "HashAggregate" | "Group" if aggregated && !grouped => {
                    grouped = true;
                    let list = arena
                        .alloc_slice_with(targets.len(), |_| Entry::EMPTY)
                        .map_err(|_| arena_full())?;
                    for (at, target) in targets.iter().enumerate() {
                        list[at] = if target.group || target.entry.var {
                            target.entry.referenced(arena)?
                        } else {
                            target.entry
                        };
                    }
                    list
                }
                "WindowAgg" => {
                    windows_done += 1;
                    if windows_done == window_count {
                        let list = arena
                            .alloc_slice_with(targets.len(), |_| Entry::EMPTY)
                            .map_err(|_| arena_full())?;
                        // The functions of the windows below are read from
                        // their nodes.
                        for (at, target) in targets.iter().enumerate() {
                            list[at] = if target.window_clause == Some(window_count - 1) {
                                target.entry
                            } else {
                                target.entry.referenced(arena)?
                            };
                        }
                        list
                    } else {
                        // An inner window returns its input and its own
                        // functions.
                        let mut list = ArenaList::new(arena);
                        for entry in below {
                            list.push(entry.referenced(arena)?)
                                .map_err(|_| arena_full())?;
                        }
                        let clause = windows_done - 1;
                        for (at, call) in window_calls.iter().enumerate() {
                            if window_clauses[at] == clause
                                && let Some((entry, _)) = window_entry(call)?
                            {
                                list.push(entry).map_err(|_| arena_full())?;
                            }
                        }
                        list.as_slice()
                    }
                }
                "ProjectSet" => {
                    let list = arena
                        .alloc_slice_with(targets.len(), |_| Entry::EMPTY)
                        .map_err(|_| arena_full())?;
                    for (at, target) in targets.iter().enumerate() {
                        list[at] = target.entry;
                    }
                    list
                }
                "HashAggregate" => {
                    let list = arena
                        .alloc_slice_with(targets.len(), |_| Entry::EMPTY)
                        .map_err(|_| arena_full())?;
                    for (at, target) in targets.iter().enumerate() {
                        list[at] = target.entry.referenced(arena)?;
                    }
                    list
                }
                _ if index == 0 && bottom_computes => scan_join,
                _ => {
                    let list = arena
                        .alloc_slice_with(below.len(), |_| Entry::EMPTY)
                        .map_err(|_| arena_full())?;
                    for (at, entry) in below.iter().enumerate() {
                        list[at] = entry.referenced(arena)?;
                    }
                    list
                }
            };
            outputs[index] = render(current)?;
            below = current;
        }
        Ok(Self {
            scan_join,
            labels_plain,
            needed,
            physical,
            stages: outputs,
            order,
        })
    }

    /// The output of stage `index`.
    pub(super) fn stage(&self, index: usize) -> Output {
        self.stages[index]
    }

    /// The scan/join target, which the top scan or join computes.
    pub(super) fn scan_join(&self) -> Result<Output, SqlError> {
        render(self.scan_join)
    }

    /// The output of `table`'s scan: the top scan of a one-relation block
    /// computes the scan/join target, a joined relation's returns the
    /// columns needed above it, either unless `demand` lets it return its
    /// physical columns.
    pub(super) fn scan(
        &self,
        table: usize,
        single: bool,
        demand: Demand,
    ) -> Result<Output, SqlError> {
        let physical = self.physical[table].filter(|_| match demand {
            Demand::Exact | Demand::Small => false,
            Demand::Label => self.labels_plain,
            Demand::Any => true,
        });
        if let Some(physical) = physical {
            return render(physical);
        }
        if single {
            return render(self.scan_join);
        }
        let mut output = Output::new();
        for (_, entry, _) in self.needed.iter().filter(|(held, ..)| *held == table) {
            if !output.is_empty() {
                let _ = output.write_str(", ");
            }
            let _ = output.write_str(entry.text);
        }
        checked(output)
    }

    /// The output of the join adding the relation at `position` of the join
    /// order, below the top join: the columns of its relations needed above
    /// it, outer relation first.
    pub(super) fn join(&self, position: usize) -> Result<Output, SqlError> {
        let mut output = Output::new();
        for &table in &self.order[..=position] {
            for (_, entry, last) in self.needed.iter().filter(|(held, ..)| *held == table) {
                if *last > position {
                    if !output.is_empty() {
                        let _ = output.write_str(", ");
                    }
                    let _ = output.write_str(entry.text);
                }
            }
        }
        checked(output)
    }
}

impl Target<'_, '_> {
    const fn column(entry: Entry<'_>) -> Target<'_, '_> {
        Target {
            expression: None,
            entry,
            group: false,
            window: false,
            labelled: false,
            window_clause: None,
            identity: None,
        }
    }
}

impl Entry<'_> {
    const EMPTY: Self = Self {
        text: "",
        var: true,
        referenced: false,
    };
}

/// `entries` as an `Output:` list.
fn render(entries: &[Entry<'_>]) -> Result<Output, SqlError> {
    let mut output = Output::new();
    for entry in entries {
        if !output.is_empty() {
            let _ = output.write_str(", ");
        }
        let _ = output.write_str(entry.text);
    }
    checked(output)
}

fn checked(output: Output) -> Result<Output, SqlError> {
    if output.is_truncated() {
        return Err(sql_err!(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            "EXPLAIN output column list exceeds {} bytes",
            OUTPUT_BYTES
        ));
    }
    Ok(output)
}

/// Calls `visit` with each column and whole-row reference of `expression`
/// outside its subqueries, in written order (`pull_var_clause`).
fn columns_of<'e>(expression: &'e Expr<'e>, visit: &mut dyn FnMut(&'e Expr<'e>)) {
    match expression {
        Expr::Column { .. } | Expr::WholeRow(_) => visit(expression),
        Expr::Subquery(_) | Expr::RowSubquery { .. } | Expr::Exists(_) | Expr::ArraySubquery(_) => {
        }
        Expr::InSubquery { operand, .. } | Expr::QuantifiedSubquery { operand, .. } => {
            columns_of(operand, visit);
        }
        _ => {
            let _ = query::walk_children(expression, &mut |child| {
                columns_of(child, visit);
                Ok(())
            });
        }
    }
}

/// The `AND` conjuncts of `condition`, appended to `out`.
fn conjuncts<'l, 'e: 'l>(
    condition: &'e Expr<'e>,
    out: &mut ArenaList<'l, &'e Expr<'e>>,
) -> Result<(), SqlError> {
    match condition {
        Expr::Binary {
            operator: crate::sql::ast::BinaryOp::And,
            left,
            right,
            ..
        } => {
            conjuncts(left, out)?;
            conjuncts(right, out)
        }
        _ => out.push(condition).map_err(|_| arena_full()),
    }
}

/// The target lists of nodes that read one relation outside a block's join
/// tree: a foreign scan's (postgres_fdw's base or grouped relation), a
/// write's `RETURNING` list and the row its scan hands the write. Each
/// list is `None` when an entry is outside the deparser's coverage, which
/// marks its node's output unprintable.
pub(super) struct RelationTargets<'a> {
    context: Context<'a>,
    table: &'a TableDef,
    exposed: &'a str,
    arena: &'a Arena,
}

impl<'a> RelationTargets<'a> {
    /// The lists over `table`, scanned as `exposed`. `qualify` is
    /// `show_plan_tlist`'s prefixing rule: the range table holds more than
    /// this relation.
    pub(super) fn new(
        storage: &'a Storage,
        txid: u32,
        table: &'a TableDef,
        exposed: &'a str,
        qualify: bool,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let joined = arena
            .alloc_slice_with(1, |_| Joined::new(exposed, table))
            .map_err(|_| arena_full())?;
        Ok(Self::with_joined(
            storage, txid, table, exposed, joined, qualify, None, arena,
        ))
    }

    /// The lists of a write whose `RETURNING` list may name the old and new
    /// rows (`returning`), which print qualified. The target's own columns
    /// print bare in an `UPDATE` or `DELETE` and qualified in an `INSERT`
    /// (`qualify_target`), as PostgreSQL prints them.
    pub(super) fn returning_scope(
        storage: &'a Storage,
        txid: u32,
        table: &'a TableDef,
        exposed: &'a str,
        returning: Returning<'a>,
        qualify_target: bool,
        arena: &'a Arena,
    ) -> Result<Self, SqlError> {
        let rows = [
            Joined::new(exposed, table),
            Joined::rule_row(returning.old_name(), table),
            Joined::rule_row(returning.new_name(), table),
        ];
        let joined = arena
            .alloc_slice_with(rows.len(), |at| rows[at])
            .map_err(|_| arena_full())?;
        Ok(Self::with_joined(
            storage,
            txid,
            table,
            exposed,
            joined,
            true,
            (!qualify_target).then_some(exposed),
            arena,
        ))
    }

    #[expect(clippy::too_many_arguments, reason = "the deparse context")]
    fn with_joined(
        storage: &'a Storage,
        txid: u32,
        table: &'a TableDef,
        exposed: &'a str,
        joined: &'a [Joined<'a>],
        qualify: bool,
        bare: Option<&'a str>,
        arena: &'a Arena,
    ) -> Self {
        Self {
            context: Context {
                storage,
                txid,
                table: Some(table),
                parameters: &[],
                joined,
                pretty: false,
                qualify,
                bare,
                domain_value: None,
                sublinks: Some(&super::registered_sublink),
                references: None,
                folded: true,
                arena,
                dependencies: None,
            },
            table,
            exposed,
            arena,
        }
    }

    /// `expression` as an entry, `None` outside the deparser's coverage.
    fn entry(
        &self,
        expression: &Expr<'_>,
        destination: Destination,
    ) -> Result<Option<Entry<'a>>, SqlError> {
        let text = crate::sql::catalog::deparse::deparse_expression(
            &self.context,
            expression,
            destination,
            self.arena,
        )?;
        Ok(text.map(|text| Entry {
            text,
            var: matches!(expression, Expr::Column { .. } | Expr::WholeRow(_)),
            referenced: false,
        }))
    }

    /// Column `index` of the relation, prefixed by `qualifier`.
    fn column(&self, index: usize, qualifier: Option<&str>) -> Result<Entry<'a>, SqlError> {
        let mut name = StackStr::<160>::new();
        if let Some(qualifier) = qualifier {
            crate::sql::catalog::write_identifier(&mut name, qualifier);
            let _ = name.write_char('.');
        }
        crate::sql::catalog::write_identifier(&mut name, self.table.columns()[index].name.as_str());
        Ok(Entry {
            text: self
                .arena
                .alloc_str(name.as_str())
                .map_err(|_| arena_full())?,
            var: true,
            referenced: false,
        })
    }

    /// The relation's whole row as a locked foreign scan returns it
    /// (`ROW_MARK_COPY`): `t.*`.
    fn whole_row(&self) -> Result<Entry<'a>, SqlError> {
        let mut name = StackStr::<80>::new();
        crate::sql::catalog::write_identifier(&mut name, self.exposed);
        let _ = name.write_str(".*");
        Ok(Entry {
            text: self
                .arena
                .alloc_str(name.as_str())
                .map_err(|_| arena_full())?,
            var: true,
            referenced: false,
        })
    }

    /// The prefix the target's own columns print with: the exposed name in
    /// an `INSERT`, none in an `UPDATE` or `DELETE` (`returning_scope`).
    fn target_qualifier(&self) -> Option<&'a str> {
        self.context.bare.is_none().then_some(self.exposed)
    }

    /// A write's `RETURNING` list (built by [`Self::returning_scope`]).
    pub(super) fn returning(&self, returning: Returning<'_>) -> Result<Option<Output>, SqlError> {
        let mut entries = ArenaList::new(self.arena);
        let push = |entries: &mut ArenaList<'a, Entry<'a>>, entry| {
            entries.push(entry).map_err(|_| arena_full())
        };
        for item in returning.items {
            match item {
                SelectItem::Wildcard(_) => {
                    for column in 0..self.table.columns().len() {
                        push(&mut entries, self.column(column, self.target_qualifier())?)?;
                    }
                }
                SelectItem::TableWildcard(qualifier) => {
                    let image = [returning.old_name(), returning.new_name()].contains(qualifier);
                    let qualifier = if image {
                        Some(*qualifier)
                    } else {
                        self.target_qualifier()
                    };
                    for column in 0..self.table.columns().len() {
                        push(&mut entries, self.column(column, qualifier)?)?;
                    }
                }
                SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                    let Some(entry) = self.entry(expression, Destination::Value)? else {
                        return Ok(None);
                    };
                    push(&mut entries, entry)?;
                }
            }
        }
        render(entries.as_slice()).map(Some)
    }

    /// The row an `UPDATE` or `DELETE` scan hands its write: the new values
    /// of the assigned columns in column order as the planner projects them
    /// (`rewriteTargetListIU`), the row identity `ctid`, then, when
    /// `whole_row`, the old row a foreign write reads (`t.*`). `alias` is
    /// the name the statement gives the target.
    pub(super) fn modified_row(
        &self,
        assignments: &[(&str, &'a Expr<'a>)],
        alias: Option<&str>,
        whole_row: bool,
    ) -> Result<Option<Output>, SqlError> {
        if assignments
            .iter()
            .any(|(target, _)| self.table.column_index(target).is_none())
        {
            return Ok(None);
        }
        let mut entries = ArenaList::new(self.arena);
        for column in self.table.columns() {
            let Some(&(_, value)) = assignments
                .iter()
                .find(|(target, _)| *target == column.name.as_str())
            else {
                continue;
            };
            let Some(entry) = self.assigned(column, value, alias)? else {
                return Ok(None);
            };
            entries.push(entry).map_err(|_| arena_full())?;
        }
        let ctid = match self.target_qualifier() {
            Some(qualifier) => {
                let mut name = StackStr::<96>::new();
                crate::sql::catalog::write_identifier(&mut name, qualifier);
                let _ = name.write_str(".ctid");
                self.arena
                    .alloc_str(name.as_str())
                    .map_err(|_| arena_full())?
            }
            None => "ctid",
        };
        entries
            .push(Entry {
                text: ctid,
                var: true,
                referenced: false,
            })
            .map_err(|_| arena_full())?;
        if whole_row {
            entries.push(self.whole_row()?).map_err(|_| arena_full())?;
        }
        render(entries.as_slice()).map(Some)
    }

    /// The planned form of one column's new value: a `DEFAULT` is the
    /// column's default expression (a null of its type when it has none), a
    /// constant the planner folds to the column's type prints that constant,
    /// and an element or slice assignment prints `column[...] := value`.
    fn assigned(
        &self,
        column: &crate::storage::ColumnMeta,
        value: &'a Expr<'a>,
        alias: Option<&str>,
    ) -> Result<Option<Entry<'a>>, SqlError> {
        let arena = self.arena;
        let target = column.ctype.oid();
        let value = match value {
            Expr::DefaultMarker => match column.default.expression() {
                None => return self.entry(&Expr::Null, Destination::Assignment(target)),
                Some(source) => {
                    let source = arena.alloc_str(source.as_str()).map_err(|_| arena_full())?;
                    crate::sql::parser::parse_expr(source, arena)?
                }
            },
            Expr::Call { name, args, .. }
                if matches!(
                    *name,
                    "__jsonb_subscript_set" | "__array_subscript_set" | "__assign_path"
                ) =>
            {
                return self.assigned_path(column, name, args);
            }
            value => value,
        };
        // The planner folds a constant expression, coerced to the column's
        // type. Analysis already converted a cast or typed literal to a
        // bound value.
        let columns = crate::sql::exec::AliasedDefCols {
            definition: crate::storage::RelationDefinition::from_definition(self.table),
            alias,
            output_aliases: &[],
            remote_ctid: false,
        };
        let bound = matches!(
            value,
            Expr::BoundLiteral {
                value: crate::sql::ast::BoundLiteralValue::Folded(_),
                ..
            }
        );
        if bound
            || crate::sql::exec::folds_to_constant(value)
                && !matches!(value, Expr::Str(_) | Expr::Null)
                && crate::sql::exec::infer_type_res(value, &columns)
                    .is_ok_and(|(oid, _)| oid != crate::sql::types::oid::UNKNOWN)
        {
            let evaluated = crate::sql::eval::eval_full(
                value,
                arena,
                &[],
                &crate::sql::eval::NoColumns,
                &crate::sql::eval::NO_HOOKS,
            )?;
            // An assignment enforces the column's modifier (`varchar(3)`).
            let folded = crate::sql::exec::apply_typmod(
                crate::sql::eval::cast_to(evaluated, column.ctype, arena)?,
                column.ctype,
                column.type_mod,
                arena,
            )?;
            let length = crate::sql::exec::projected_row_len(&[folded])?;
            let encoded = arena
                .alloc_slice_with(length, |_| 0u8)
                .map_err(|_| arena_full())?;
            crate::sql::exec::encode_projected_into(&[folded], encoded)?;
            let constant = arena
                .alloc(Expr::BoundLiteral {
                    value: crate::sql::ast::BoundLiteralValue::Folded(encoded),
                    type_oid: target,
                    type_mod: -1,
                    collation: None,
                    source: value,
                })
                .map_err(|_| arena_full())?;
            return self.entry(constant, Destination::Value);
        }
        self.entry(value, Destination::Assignment(target))
    }

    /// `column[i] := value` or `column[i:j] := value`, as PostgreSQL prints a
    /// subscripted assignment (a `SubscriptingRef` with an assigned value).
    /// An assignment through a composite field is not covered.
    fn assigned_path(
        &self,
        column: &crate::storage::ColumnMeta,
        name: &str,
        args: &[&'a Expr<'a>],
    ) -> Result<Option<Entry<'a>>, SqlError> {
        let crate::sql::types::ColType::Array(element) = column.ctype else {
            return Ok(None);
        };
        // The parser's three assignment forms: element subscripts only, a
        // subscript list holding a slice, and a path through fields.
        let (base, value, kinds, operands): (&Expr<'_>, &Expr<'_>, &str, &[&Expr<'_>]) =
            match (name, args) {
                ("__jsonb_subscript_set", [base, value, operands @ ..]) if operands.len() <= 6 => {
                    (base, value, &"eeeeee"[..operands.len()], operands)
                }
                ("__array_subscript_set", [base, value, Expr::Str(kinds), operands @ ..])
                | ("__assign_path", [base, value, _, Expr::Str(kinds), operands @ ..]) => {
                    (base, value, kinds, operands)
                }
                _ => return Ok(None),
            };
        if !kinds.bytes().all(|kind| matches!(kind, b'e' | b's'))
            || operands.len()
                != kinds
                    .bytes()
                    .map(|kind| 1 + usize::from(kind == b's'))
                    .sum::<usize>()
        {
            return Ok(None);
        }
        let Some(base) = self.entry(base, Destination::Value)? else {
            return Ok(None);
        };
        let mut text = Output::new();
        let _ = text.write_str(base.text);
        let mut operand = operands.iter();
        for kind in kinds.bytes() {
            let _ = text.write_str("[");
            for (index, bound) in operand
                .by_ref()
                .take(1 + usize::from(kind == b's'))
                .enumerate()
            {
                let Some(bound) =
                    self.entry(bound, Destination::Assignment(crate::sql::types::oid::INT4))?
                else {
                    return Ok(None);
                };
                if index != 0 {
                    let _ = text.write_str(":");
                }
                let _ = text.write_str(bound.text);
            }
            let _ = text.write_str("]");
        }
        let target = if kinds.contains('s') {
            column.ctype.oid()
        } else {
            element.element_oid()
        };
        let Some(value) = self.entry(value, Destination::Assignment(target))? else {
            return Ok(None);
        };
        let _ = write!(text, " := {}", value.text);
        let text = checked(text)?;
        Ok(Some(Entry {
            text: self
                .arena
                .alloc_str(text.as_str())
                .map_err(|_| arena_full())?,
            var: false,
            referenced: false,
        }))
    }

    /// A one-row `INSERT`'s `Result`: each column's value in column order as
    /// the planner projects it, a column the row omits taking its
    /// `DEFAULT`. A defaulted identity, serial or generated column is not
    /// covered.
    pub(super) fn inserted_row(
        &self,
        columns: &[&str],
        row: &[&'a Expr<'a>],
    ) -> Result<Option<Output>, SqlError> {
        let definitions = self.table.columns();
        let expected = if columns.is_empty() {
            definitions.len()
        } else {
            columns.len()
        };
        if row.len() != expected {
            return Ok(None);
        }
        let mut entries = ArenaList::new(self.arena);
        for (index, column) in definitions.iter().enumerate() {
            let value = if columns.is_empty() {
                Some(row[index])
            } else {
                columns
                    .iter()
                    .position(|named| *named == column.name.as_str())
                    .map(|at| row[at])
            };
            let value = value.unwrap_or(&Expr::DefaultMarker);
            if matches!(value, Expr::DefaultMarker)
                && (column.is_identity
                    || column.auto_increment
                    || matches!(
                        column.default,
                        crate::storage::ColumnDefault::Generated { .. }
                    ))
            {
                return Ok(None);
            }
            let Some(entry) = self.assigned(column, value, None)? else {
                return Ok(None);
            };
            entries.push(entry).map_err(|_| arena_full())?;
        }
        render(entries.as_slice()).map(Some)
    }

    /// A several-row `INSERT`'s `Values Scan`: its columns, each a column
    /// of the `*VALUES*` list. Only rows naming every column in order
    /// without `DEFAULT` are covered.
    pub(super) fn values_scan(
        &self,
        columns: &[&str],
        rows: &[&[&Expr<'_>]],
    ) -> Result<Option<Output>, SqlError> {
        let definitions = self.table.columns();
        if !self.covers(columns)
            || rows.iter().any(|row| {
                row.len() != definitions.len()
                    || row.iter().any(|value| matches!(value, Expr::DefaultMarker))
            })
        {
            return Ok(None);
        }
        let mut output = Output::new();
        for column in 1..=definitions.len() {
            if column > 1 {
                let _ = output.write_str(", ");
            }
            let _ = write!(output, "\"*VALUES*\".column{column}");
        }
        checked(output).map(Some)
    }

    /// Whether an `INSERT`'s column list names every column in order (an
    /// empty one does).
    fn covers(&self, columns: &[&str]) -> bool {
        let definitions = self.table.columns();
        columns.is_empty()
            || columns.len() == definitions.len()
                && columns
                    .iter()
                    .zip(definitions)
                    .all(|(named, column)| *named == column.name.as_str())
    }

    /// postgres_fdw's scan of the base relation: the select list, the sort
    /// keys it does not project, then the whole row a locking clause copies;
    /// then the same list as a node above reads it.
    pub(super) fn foreign_scan(
        &self,
        statement: &Select<'_>,
        sort_keys: &[crate::sql::ast::OrderBy<'_>],
    ) -> Result<Option<(Output, Output)>, SqlError> {
        let mut entries = ArenaList::new(self.arena);
        let push = |entries: &mut ArenaList<'a, Entry<'a>>, entry| {
            entries.push(entry).map_err(|_| arena_full())
        };
        for item in statement.items {
            match item {
                SelectItem::Wildcard(_) | SelectItem::TableWildcard(_) => {
                    for column in 0..self.table.columns().len() {
                        push(&mut entries, self.column(column, None)?)?;
                    }
                }
                SelectItem::Expr { expression, .. } | SelectItem::RecordStar(expression) => {
                    let Some(entry) = self.entry(expression, Destination::Value)? else {
                        return Ok(None);
                    };
                    push(&mut entries, entry)?;
                }
            }
        }
        let exposed = self.exposed;
        // The sort keys, then the `WINDOW` definitions' keys, which no
        // window reads here (a used window keeps the general scan target).
        let window_keys = statement.windows.iter().flat_map(|spec| {
            spec.partition_by
                .iter()
                .copied()
                .chain(spec.order_by.iter().map(|order| order.expression))
        });
        let keys = sort_keys
            .iter()
            .map(|key| key.expression)
            .chain(window_keys);
        let mut listed = ArenaList::new(self.arena);
        for key in keys {
            if listed
                .as_slice()
                .iter()
                .any(|&earlier: &&Expr<'_>| *earlier == *key)
            {
                continue;
            }
            listed.push(key).map_err(|_| arena_full())?;
            let projected = statement.items.iter().any(|item| match item {
                SelectItem::Wildcard(_) => matches!(key, Expr::Column { .. }),
                SelectItem::TableWildcard(qualifier) => match key {
                    Expr::Column {
                        qualifier: held, ..
                    } => held.is_none_or(|q| q == *qualifier),
                    _ => false,
                },
                SelectItem::Expr { expression, .. } => match (expression, key) {
                    (
                        Expr::Column {
                            qualifier,
                            name: projected,
                        },
                        Expr::Column { name, .. },
                    ) => {
                        projected == name && qualifier.is_none_or(|qualifier| qualifier == exposed)
                    }
                    (expression, key) => **expression == *key,
                },
                SelectItem::RecordStar(_) => false,
            });
            if projected {
                continue;
            }
            let entry = match key {
                Expr::Column { name, .. } if self.table.column_index(name).is_some() => {
                    let column = self.table.column_index(name).expect("checked above");
                    self.column(column, None)?
                }
                expression => match self.entry(expression, Destination::Value)? {
                    Some(entry) => entry,
                    None => return Ok(None),
                },
            };
            push(&mut entries, entry)?;
        }
        if !statement.locking.is_empty() {
            push(&mut entries, self.whole_row()?)?;
        }
        let mut above = ArenaList::new(self.arena);
        for entry in entries.as_slice() {
            above
                .push(entry.referenced(self.arena)?)
                .map_err(|_| arena_full())?;
        }
        Ok(Some((
            render(entries.as_slice())?,
            render(above.as_slice())?,
        )))
    }

    /// postgres_fdw's scan of a grouped relation and the nodes above it:
    /// the grouping target the scan returns, read from the remote target
    /// list (`fdw_scan_tlist`) by reference where it holds the expression
    /// and computed over those references otherwise, then the same list as
    /// a node above reads it.
    pub(super) fn foreign_upper(
        &self,
        upper: &crate::sql::foreign::deparse::ForeignUpper<'_>,
    ) -> Result<Option<(Output, Output)>, SqlError> {
        let mut scan = ArenaList::new(self.arena);
        let mut above = ArenaList::new(self.arena);
        for &expression in upper.output {
            let Some(entry) = self.upper_entry(upper, expression, Destination::Value)? else {
                return Ok(None);
            };
            scan.push(entry).map_err(|_| arena_full())?;
            above
                .push(entry.referenced(self.arena)?)
                .map_err(|_| arena_full())?;
        }
        Ok(Some((render(scan.as_slice())?, render(above.as_slice())?)))
    }

    /// `expression` over a grouped scan's remote targets as the scan
    /// computes it and as a node above reads it.
    pub(super) fn upper_text(
        &self,
        upper: &crate::sql::foreign::deparse::ForeignUpper<'_>,
        expression: &Expr<'_>,
        destination: Destination,
    ) -> Result<Option<(&'a str, &'a str)>, SqlError> {
        let Some(entry) = self.upper_entry(upper, expression, destination)? else {
            return Ok(None);
        };
        Ok(Some((entry.text, entry.referenced(self.arena)?.text)))
    }

    /// `expression` over a grouped scan's remote targets, which print as
    /// references; a column or a constant prints as itself.
    fn upper_entry(
        &self,
        upper: &crate::sql::foreign::deparse::ForeignUpper<'_>,
        expression: &Expr<'_>,
        destination: Destination,
    ) -> Result<Option<Entry<'a>>, SqlError> {
        let read = |node: &Expr<'_>| {
            crate::sql::foreign::deparse::reads_remote_target(upper.targets, node)
        };
        let context = Context {
            references: Some(&read),
            ..self.context
        };
        let text = crate::sql::catalog::deparse::deparse_expression(
            &context,
            expression,
            destination,
            self.arena,
        )?;
        let plain = matches!(expression, Expr::Column { .. })
            || crate::sql::foreign::deparse::is_constant(expression);
        Ok(text.map(|text| Entry {
            text,
            var: plain,
            referenced: read(expression),
        }))
    }
}
