//! The paths of a partitioned table or inheritance tree
//! (`set_append_rel_size`, `set_append_rel_pathlist`): each member is
//! planned from its own statistics and indexes, and the tree is scanned by
//! an `Append` of the members' cheapest paths (`add_paths_to_append_rel`,
//! costed by `cost_append`), or, for each sort order a member's path
//! returns, by a `Merge Append` (`create_merge_append_path`,
//! `cost_merge_append`) or, when the partitions are ordered by their
//! bounds, an ordered `Append` of the members' ordered paths
//! (`generate_orderedappend_paths`).

use crate::mem::arena::Arena;
use crate::sql::ast::Expr;
use crate::sql::eval::SqlError;
use crate::sql::types::Datum;
use crate::storage::{ColumnSet, PartitionBound, PartitionStrategy, Storage, TableDef};

use super::super::scope::QueryScope;
use super::paths::{self, Candidate, RelPaths};
use super::{Costs, Member, OrderKey, Planned, Planner, QueryShape, ScanKind, ScanPath, SortKind};

/// `APPEND_CPU_COST_MULTIPLIER`.
const APPEND_CPU_COST_MULTIPLIER: f64 = 0.5;
/// The members of one tree the model plans.
const MAX_MEMBERS: usize = 256;
/// The `Append` and `Merge Append` paths one tree builds.
const MAX_APPENDS: usize = 3 * (super::MAX_ORDER_KEYS + 1);

/// Whether the tree's members are read one after another or merged in a
/// sort order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppendKind {
    Append,
    Merge,
}

/// One member's scan under the tree's `Append` or `Merge Append`.
#[derive(Clone, Copy)]
pub(crate) struct MemberPath<'a> {
    /// The member's storage slot.
    pub(crate) leaf: usize,
    /// Its scan, under the member's own column numbering.
    pub(crate) path: ScanPath<'a>,
    /// The startup and total cost of the sort the `Append` or `Merge
    /// Append` puts over a member whose path lacks its order.
    pub(crate) sort: Option<(f64, f64)>,
    /// The chosen path's kind and its number among the member's paths,
    /// which [`member_path`] describes again.
    pub(crate) kind_path: (ScanKind, usize),
}

/// The path PostgreSQL chooses over a tree: its `Append` or `Merge Append`
/// of the members' scans, and the sort and limit above it.
#[derive(Clone, Copy)]
pub(crate) struct TreePath<'a> {
    pub(crate) kind: AppendKind,
    /// The members' scans in the plan's order. One member replaces the
    /// `Append`.
    pub(crate) members: &'a [MemberPath<'a>],
    /// The query's sort keys the `Append` or `Merge Append` returns its
    /// rows in, under the relation's column numbering.
    pub(crate) order: &'a [OrderKey],
    pub(crate) rows: f64,
    pub(crate) width: u32,
    pub(crate) startup_cost: f64,
    pub(crate) total_cost: f64,
    /// The startup and total cost of the `Result` above an inheritance
    /// tree's `Append` that computes the query's target list.
    pub(crate) result_cost: Option<(f64, f64)>,
    /// The sort above the `Append`.
    pub(crate) sort: SortKind,
    pub(crate) sort_cost: Option<(f64, f64)>,
    pub(crate) limit_cost: Option<(f64, f64, f64)>,
    /// The rows a `LIMIT` above an ordered `Append` or `Merge Append` takes
    /// before it stops.
    pub(crate) stop_after: Option<u64>,
}

impl<'a> TreePath<'a> {
    /// The tree's path as the query above it reads it: the `Append`'s rows,
    /// width and costs, and the sort and limit over it.
    pub(crate) fn top(&self) -> ScanPath<'a> {
        ScanPath {
            kind: ScanKind::Seq,
            index_oid: 0,
            index_name: crate::util::StackStr::new(),
            index_pages: super::IndexPages::NONE,
            keys: &[],
            conditions: &[],
            predicate: &[],
            bitmap: &[],
            recheck: &[],
            absorbed: &[],
            rows: self.rows,
            width: self.width,
            startup_cost: self.startup_cost,
            total_cost: self.total_cost,
            backward: false,
            sort: self.sort,
            deduplicated: false,
            stop_after: self.stop_after,
            single_row: false,
            searches_unmodeled: false,
            sort_cost: self.sort_cost,
            limit_cost: self.limit_cost,
            grouped: None,
        }
    }
}

/// A member's plan inputs: its slot, column numbering and paths.
#[derive(Clone, Copy)]
struct Live<'a> {
    leaf: usize,
    paths: RelPaths<'a>,
    /// Each tree column's position in the member, `usize::MAX` when absent.
    columns: &'a [usize],
    shape: QueryShape<'a>,
    needed: Option<ColumnSet>,
    /// The width of its paths' rows above it.
    width: f64,
    cheapest_total: Candidate,
}

/// One `Append` or `Merge Append` path of the tree.
#[derive(Clone, Copy)]
struct AppendOption<'a> {
    kind: AppendKind,
    /// Its pathkeys: the first this many of the query's sort keys.
    presorted: usize,
    /// The live members' positions, in the path's order, and their paths.
    subpaths: &'a [(usize, Candidate)],
}

/// `pathkeys_contained_in` of `wanted` keys in a path ordered by the first
/// `presorted` query keys.
fn contained(wanted: usize, presorted: usize) -> bool {
    wanted <= presorted
}

/// `compare_path_costs` by total cost, then startup cost.
fn total_order(left: &Candidate, right: &Candidate) -> core::cmp::Ordering {
    left.disabled
        .cmp(&right.disabled)
        .then(left.total.total_cmp(&right.total))
        .then(left.startup.total_cmp(&right.startup))
}

/// `compare_path_costs` by startup cost, then total cost.
fn startup_order(left: &Candidate, right: &Candidate) -> core::cmp::Ordering {
    left.disabled
        .cmp(&right.disabled)
        .then(left.startup.total_cmp(&right.startup))
        .then(left.total.total_cmp(&right.total))
}

/// `compare_fractional_path_costs`.
fn fractional_order(left: &Candidate, right: &Candidate, fraction: f64) -> core::cmp::Ordering {
    if fraction <= 0.0 || fraction >= 1.0 {
        return total_order(left, right);
    }
    left.disabled.cmp(&right.disabled).then_with(|| {
        let cost = |path: &Candidate| path.startup + fraction * (path.total - path.startup);
        cost(left).total_cmp(&cost(right))
    })
}

/// `get_cheapest_path_for_pathkeys` over `kept`, or the cheapest
/// fractional one (`get_cheapest_fractional_path_for_pathkeys`).
fn cheapest_for_pathkeys(
    kept: &[Candidate],
    wanted: usize,
    order: &dyn Fn(&Candidate, &Candidate) -> core::cmp::Ordering,
) -> Option<Candidate> {
    let mut matched: Option<Candidate> = None;
    for path in kept {
        if matched.is_some_and(|best| order(&best, path).is_le()) {
            continue;
        }
        if contained(wanted, path.presorted) {
            matched = Some(*path);
        }
    }
    matched
}

/// `get_cheapest_fractional_path` of a relation whose cheapest total path
/// is `cheapest_total`, for a `LIMIT` of `tuple_fraction` rows.
fn cheapest_fractional(
    kept: &[Candidate],
    cheapest_total: Candidate,
    tuple_fraction: f64,
    rows: f64,
) -> Candidate {
    if tuple_fraction <= 0.0 {
        return cheapest_total;
    }
    let fraction = if tuple_fraction >= 1.0 && rows > 0.0 {
        tuple_fraction / rows
    } else {
        tuple_fraction
    };
    let mut best = cheapest_total;
    for path in kept {
        if path.same_scan(&best) || fractional_order(&best, path, fraction).is_le() {
            continue;
        }
        best = *path;
    }
    best
}

/// `set_cheapest`'s cheapest total path.
fn cheapest_total(kept: &[Candidate]) -> Candidate {
    let mut best = kept[0];
    for path in &kept[1..] {
        let order = total_order(&best, path);
        if order.is_gt() || (order.is_eq() && path.presorted > best.presorted) {
            best = *path;
        }
    }
    best
}

/// The tree's partition order (`partitions_are_ordered` with
/// `build_partition_pathkeys`), against the query's sort keys `order`;
/// `None` when the partitions are not ordered. `fixed` holds the columns a
/// condition fixes to a constant, whose keys are redundant.
fn partition_order(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    leaves: &[usize],
    order: &[OrderKey],
    fixed: ColumnSet,
) -> Result<Option<PartitionOrder>, SqlError> {
    let Some(scheme) = definition.partition.scheme else {
        return Ok(None);
    };
    let default_live = leaves.iter().any(|&leaf| {
        matches!(
            storage
                .table_def(leaf, txid)
                .partition
                .attachment
                .map(|attachment| attachment.bound),
            Some(PartitionBound::Default)
        )
    });
    match scheme.strategy {
        PartitionStrategy::Hash => return Ok(None),
        PartitionStrategy::Range if default_live => return Ok(None),
        PartitionStrategy::Range => {}
        PartitionStrategy::List => {
            if interleaved(storage, txid, leaves)? {
                return Ok(None);
            }
        }
    }
    // The partition key's pathkeys: each key column the query sorts by,
    // skipping those a constant fixes, until one it does neither to.
    let mut keys = [0usize; crate::storage::MAX_PARTITION_KEYS];
    let mut len = 0;
    let mut partial = false;
    for &key in &scheme.keys[..usize::from(scheme.n_keys)] {
        let column = usize::from(key);
        if fixed.contains(column) {
            continue;
        }
        if !order.iter().any(
            |wanted| matches!(wanted, OrderKey::Column { column: sorted, .. } if *sorted == column),
        ) {
            partial = true;
            break;
        }
        keys[len] = column;
        len += 1;
    }
    let common = |descending: bool| {
        keys[..len]
            .iter()
            .zip(order)
            .take_while(|(column, wanted)| {
                **wanted
                    == OrderKey::Column {
                        column: **column,
                        descending,
                        nulls_first: descending,
                    }
            })
            .count()
    };
    Ok(Some(PartitionOrder {
        len,
        forward: common(false),
        backward: common(true),
        partial,
    }))
}

/// A tree's partition pathkeys against the query's sort keys.
struct PartitionOrder {
    /// The partition pathkeys.
    len: usize,
    /// How many lead the query's sort keys, ascending in partition order.
    forward: usize,
    /// How many lead them descending, in reverse partition order.
    backward: usize,
    /// The partition key has columns past its pathkeys.
    partial: bool,
}

impl PartitionOrder {
    /// `match_partition_order` for pathkeys of the first `presorted` query
    /// keys: `Some(false)` in partition order, `Some(true)` in reverse.
    fn matches(&self, presorted: usize) -> Option<bool> {
        let fits = |common: usize| {
            presorted <= common || (!self.partial && self.len <= presorted && common >= self.len)
        };
        if fits(self.forward) {
            Some(false)
        } else if fits(self.backward) {
            Some(true)
        } else {
            None
        }
    }
}

/// Whether a LIST partition among `leaves` interleaves with another
/// (`create_list_bounds`' `interleaved_parts`): its values fall between
/// another partition's, it holds NULL and other values, or it is the
/// default partition beside others.
fn interleaved(storage: &Storage, txid: u32, leaves: &[usize]) -> Result<bool, SqlError> {
    use crate::storage::OwnedDatum;
    // Every partition of the parent.
    let Some(parent) = leaves.first().and_then(|&leaf| {
        storage
            .table_def(leaf, txid)
            .partition
            .attachment
            .map(|attachment| usize::from(attachment.parent))
    }) else {
        return Ok(false);
    };
    let mut partitions = [usize::MAX; MAX_MEMBERS];
    let mut count = 0;
    for child in 0..storage.table_count() {
        if !storage.relation_visible_to_current_session(child, txid) {
            continue;
        }
        if storage
            .table_def(child, txid)
            .partition
            .attachment
            .is_some_and(|attachment| {
                usize::from(attachment.parent) == parent && attachment.state.routable()
            })
        {
            if count == MAX_MEMBERS {
                return Ok(true);
            }
            partitions[count] = child;
            count += 1;
        }
    }
    let partitions = &partitions[..count];
    if partitions.len() < 2 {
        return Ok(false);
    }
    // Each non-null value with its partition, the NULL-accepting partition
    // and the default one.
    let mut values = [None::<(usize, OwnedDatum)>; 1024];
    let mut value_count = 0;
    let mut nulls = None;
    let mut default = None;
    for (position, &partition) in partitions.iter().enumerate() {
        match storage
            .table_def(partition, txid)
            .partition
            .attachment
            .map(|attachment| attachment.bound)
        {
            Some(PartitionBound::List {
                values: list,
                n_values,
            }) => {
                for value in &list[..usize::from(n_values)] {
                    if matches!(value, OwnedDatum::Null) {
                        nulls = Some(position);
                        continue;
                    }
                    if value_count == values.len() {
                        return Ok(true);
                    }
                    values[value_count] = Some((position, *value));
                    value_count += 1;
                }
            }
            Some(PartitionBound::Default) => default = Some(position),
            _ => {}
        }
    }
    let sorted = &mut values[..value_count];
    let mut failed = None;
    sorted.sort_unstable_by(|left, right| {
        let (left, right) = (
            left.expect("dense values").1,
            right.expect("dense values").1,
        );
        crate::sql::eval::compare_datums(&left.as_datum(), &right.as_datum()).unwrap_or_else(
            |error| {
                failed.get_or_insert(error);
                core::cmp::Ordering::Equal
            },
        )
    });
    if let Some(error) = failed {
        return Err(error);
    }
    let mut interleaved = [false; MAX_MEMBERS];
    if value_count + usize::from(nulls.is_some()) + usize::from(default.is_some())
        != partitions.len()
    {
        // Partitions are numbered by their least value; one met after a
        // higher-numbered one interleaves, as does the NULL partition met
        // among the values.
        let mut canonical = [usize::MAX; MAX_MEMBERS];
        let mut next = 0;
        let mut last = None::<usize>;
        for value in sorted.iter() {
            let position = value.expect("dense values").0;
            if canonical[position] == usize::MAX {
                canonical[position] = next;
                next += 1;
            }
            let index = canonical[position];
            if last.is_some_and(|last| index < last) || nulls == Some(position) {
                interleaved[position] = true;
            }
            last = Some(index);
        }
    }
    if let Some(default) = default {
        interleaved[default] = true;
    }
    Ok(partitions
        .iter()
        .enumerate()
        .any(|(position, partition)| interleaved[position] && leaves.contains(partition)))
}

/// The members' sizes as `set_append_rel_size` adds them up.
struct TreeSize {
    rows: f64,
    tuples: f64,
    /// Each tree column's width (`attr_widths`), weighted by rows.
    column_widths: [f64; 64],
    /// `rint(parent_size / parent_rows)`.
    width: f64,
}

/// Plans the scan of the partitioned table or inheritance tree in `slot`,
/// read under `condition` by the one-relation `scope`.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
pub(super) fn tree_path<'a>(
    storage: &Storage,
    scope: &QueryScope<'_>,
    txid: u32,
    condition: Option<&'a Expr<'a>>,
    needed: Option<ColumnSet>,
    params: &[Datum<'a>],
    arena: &'a Arena,
    shape: QueryShape,
    slot: usize,
    definition: &TableDef,
) -> Result<Planned<'a>, SqlError> {
    let full = || crate::sql::eval::arena_full();
    // The members planning keeps.
    let exposed = arena.alloc_str(scope.names[0]).map_err(|_| full())?;
    let prune_params = if params.is_empty() {
        crate::sql::eval::generic_plan_parameters()
    } else {
        params
    };
    let conditions = super::super::partition_prune::conjuncts(condition, arena)?;
    let values = super::super::partition_prune::ScanValues {
        storage,
        txid,
        relation: super::super::partition_prune::ScanRelation::Target {
            columns: definition.columns(),
            name: exposed,
            joined: false,
        },
        arena,
        params: prune_params,
        hooks: &crate::sql::eval::NO_HOOKS,
        outer: None,
    };
    let planned = super::super::partition_prune::members(
        storage,
        txid,
        slot,
        conditions,
        &values,
        super::super::partition_prune::PruneStage::Plan,
        arena,
    )?
    .leaves;
    if planned.len() > MAX_MEMBERS {
        return Ok(Planned::Outside("a tree of more than 256 members"));
    }
    // A foreign member is planned by its wrapper.
    if planned
        .iter()
        .any(|&leaf| storage.table_def(leaf, txid).kind != crate::storage::TableKind::Local)
    {
        return probe_members(
            storage, txid, scope, condition, needed, params, arena, shape, definition, planned,
        );
    }
    // A member of a sub-partitioned partition is an `Append` of its own.
    let nested = definition.partition.is_partitioned()
        && planned.iter().any(|&leaf| {
            storage
                .table_def(leaf, txid)
                .partition
                .attachment
                .is_none_or(|attachment| usize::from(attachment.parent) != slot)
        });
    // The query's sort keys under the tree's numbering; an index
    // expression of the tree is outside the model.
    if shape.order().iter().any(
        |key| matches!(key, OrderKey::Column { column, .. } if *column >= definition.n_columns),
    ) {
        return Ok(Planned::Outside(
            "an ORDER BY of an index expression of a partitioned table or inheritance tree",
        ));
    }
    if shape.projection.is_some_and(|projection| projection.wide) {
        return Ok(Planned::Outside(
            "a select list of a partitioned table or inheritance tree past its 64th column",
        ));
    }
    let partitioned = definition.partition.is_partitioned();
    // A partitioned table's members compute the query's target list
    // themselves when it is not the members' own columns.
    let member_projection = shape
        .projection
        .filter(|projection| partitioned && !projection.plain);
    let mut live = [None::<Live<'a>>; MAX_MEMBERS];
    let mut live_count = 0;
    let mut seq_only = false;
    let mut indexed = false;
    for &leaf in planned {
        let (columns, member_needed, member_shape) =
            member_inputs(storage, txid, definition, leaf, shape, needed, arena)?;
        let member_scope = member_scope(storage, txid, leaf, exposed, arena)?;
        match paths::scan_path(
            storage,
            &member_scope,
            txid,
            condition,
            member_needed,
            params,
            arena,
            member_shape,
        )? {
            // A member its conditions exclude is not scanned.
            Planned::NotTable => {}
            Planned::SeqOnly => seq_only = true,
            Planned::Members(member_paths) => {
                indexed |=
                    member_paths.kept.len() > 1 || member_paths.kept[0].kind != ScanKind::Seq;
                live[live_count] = Some(Live {
                    leaf,
                    paths: member_paths,
                    columns,
                    shape: member_shape,
                    needed: member_needed,
                    width: member_paths.width,
                    cheapest_total: cheapest_total(member_paths.kept),
                });
                live_count += 1;
            }
            Planned::Outside(shape) => return Ok(Planned::Outside(shape)),
            Planned::Chosen(_) | Planned::Tree(_) | Planned::Join(_) => {
                return Ok(Planned::Outside("a member planned outside its tree"));
            }
        }
    }
    if seq_only {
        return Ok(if indexed {
            Planned::Outside("a condition whose selectivity the model does not read")
        } else {
            Planned::SeqOnly
        });
    }
    if nested && indexed {
        return Ok(Planned::Outside(
            "a sub-partitioned table whose partitions have index paths",
        ));
    }
    let live = &live[..live_count];
    // Every member excluded: the dummy relation's `Result`.
    if live.is_empty() {
        return Ok(Planned::NotTable);
    }
    let size = tree_size(storage, txid, definition, live);
    let costs = Costs::current();
    // The query's sort keys without those a constant fixes, which every
    // member's paths are ordered against.
    let first = live[0].expect("a live member");
    let mut fixed = ColumnSet::EMPTY;
    for (column, &at) in first.columns.iter().enumerate() {
        if at != usize::MAX && first.paths.fixed.contains(at) {
            fixed.insert(column);
        }
    }
    let mut kept_order = [OrderKey::Expression; super::MAX_ORDER_KEYS];
    let mut order_len = 0;
    for key in shape.order() {
        if let OrderKey::Column { column, .. } = *key
            && column < 64
            && fixed.contains(column)
        {
            continue;
        }
        kept_order[order_len] = *key;
        order_len += 1;
    }
    let order = &kept_order[..order_len];
    // The width of the rows above the `Append`: the query's target list
    // over the tree's column widths, unless it is the tree's own columns
    // under an inheritance tree's `Append`.
    let result = shape
        .projection
        .filter(|projection| !partitioned && !projection.plain);
    let width = match shape.projection {
        Some(projection) if partitioned || !projection.plain => {
            f64::from(projection.width_with(&|column| size.column_widths[column]))
        }
        _ => size.width,
    };
    // A partitioned table's members return the target list's rows.
    let live: &mut [Live<'a>] = arena
        .alloc_slice_with(live.len(), |at| {
            let mut member = live[at].expect("dense members");
            if member_projection.is_some() {
                member.width = width;
            }
            member
        })
        .map_err(|_| full())?;
    let limit_tuples = shape
        .limit
        .and_then(|(count, offset)| count.map(|count| count + offset));
    let tuple_fraction = limit_tuples
        .filter(|_| paths_consider_startup(shape))
        .unwrap_or(0.0);
    let methods = crate::sql::guc::planner_methods();
    // `add_paths_to_append_rel`.
    let mut options = [None::<AppendOption<'a>>; MAX_APPENDS];
    let mut option_count = 0;
    let mut push = |option: AppendOption<'a>| {
        if option_count < MAX_APPENDS {
            options[option_count] = Some(option);
            option_count += 1;
        }
    };
    let subpaths = |pick: &dyn Fn(&Live<'a>) -> Candidate,
                    reverse: bool|
     -> Result<&'a [(usize, Candidate)], SqlError> {
        arena
            .alloc_slice_with(live.len(), |at| {
                let at = if reverse { live.len() - 1 - at } else { at };
                (at, pick(&live[at]))
            })
            .map(|subpaths| &*subpaths)
            .map_err(|_| full())
    };
    push(AppendOption {
        kind: AppendKind::Append,
        presorted: 0,
        subpaths: subpaths(&|member| member.cheapest_total, false)?,
    });
    if paths_consider_startup(shape) {
        push(AppendOption {
            kind: AppendKind::Append,
            presorted: 0,
            subpaths: subpaths(
                &|member| {
                    cheapest_fractional(
                        member.paths.kept,
                        member.cheapest_total,
                        tuple_fraction,
                        member.paths.rows,
                    )
                },
                false,
            )?,
        });
    }
    // `generate_orderedappend_paths`: each sort order a member's path
    // returns, in the order first met.
    let mut orders = [0usize; super::MAX_ORDER_KEYS];
    let mut order_count = 0;
    for member in live.iter() {
        for path in member.paths.kept {
            if path.presorted > 0 && !orders[..order_count].contains(&path.presorted) {
                orders[order_count] = path.presorted;
                order_count += 1;
            }
        }
    }
    let partition_order = if partitioned && !nested {
        partition_order(storage, txid, definition, planned, order, fixed)?
    } else {
        None
    };
    for &presorted in &orders[..order_count] {
        let reverse = partition_order
            .as_ref()
            .and_then(|partitions| partitions.matches(presorted));
        let ordered_append = reverse.is_some();
        let reverse = reverse == Some(true);
        let mut startup_neq_total = false;
        let pick = |member: &Live<'a>, criterion: u8| -> Candidate {
            let total = cheapest_for_pathkeys(member.paths.kept, presorted, &total_order);
            let startup = cheapest_for_pathkeys(member.paths.kept, presorted, &startup_order);
            let (startup, total) = match (startup, total) {
                (Some(startup), Some(total)) => (startup, total),
                _ => (member.cheapest_total, member.cheapest_total),
            };
            match criterion {
                0 => startup,
                1 => total,
                _ => {
                    let fraction = if tuple_fraction >= 1.0 {
                        tuple_fraction / member.paths.rows
                    } else {
                        tuple_fraction
                    };
                    cheapest_for_pathkeys(member.paths.kept, presorted, &|left, right| {
                        fractional_order(left, right, fraction)
                    })
                    .unwrap_or(total)
                }
            }
        };
        for member in live.iter() {
            if !pick(member, 0).same_scan(&pick(member, 1)) {
                startup_neq_total = true;
            }
        }
        let kind = if ordered_append {
            AppendKind::Append
        } else {
            AppendKind::Merge
        };
        push(AppendOption {
            kind,
            presorted,
            subpaths: subpaths(&|member| pick(member, 0), reverse)?,
        });
        if startup_neq_total {
            push(AppendOption {
                kind,
                presorted,
                subpaths: subpaths(&|member| pick(member, 1), reverse)?,
            });
        }
        if tuple_fraction > 0.0 {
            push(AppendOption {
                kind,
                presorted,
                subpaths: subpaths(&|member| pick(member, 2), reverse)?,
            });
        }
    }
    let options = &options[..option_count];
    // Each option's cost (`create_append_path`, `create_merge_append_path`).
    let candidates = arena
        .alloc_slice_with(options.len(), |at| {
            let option = options[at].expect("dense appends");
            append_cost(live, &option, at, limit_tuples, costs, &methods)
        })
        .map_err(|_| full())?;
    let mut kept = [None::<Candidate>; paths::MAX_CANDIDATES];
    let kept_count = paths::add_paths(candidates, paths_consider_startup(shape), &mut kept);
    let kept = arena
        .alloc_slice_with(kept_count, |at| kept[at].expect("dense kept paths"))
        .map_err(|_| full())?;
    // An inheritance tree's `Append` cannot compute the target list: a
    // `Result` above it does (`create_projection_path`).
    let result_cost = result.map_or(0.0, |projection| {
        (costs.cpu_tuple + projection.per_tuple) * size.rows
    });
    for path in kept.iter_mut() {
        path.total += result_cost;
    }
    let statistics = storage.table_statistics(slot, txid);
    let parent = Planner {
        variables: super::ExpressionVariables {
            first: definition.n_columns,
            variables: &[],
        },
        storage,
        txid,
        slot,
        definition,
        statistics: &statistics,
        statistics_key: crate::storage::INHERITED,
        size: super::RelationSize {
            pages: 0.0,
            tuples: size.tuples,
            allvisfrac: 0.0,
        },
        costs,
        indexes: &[],
        extended: &[],
        arena,
    };
    let groups = |presorted: usize| {
        let mut columns = [0usize; super::MAX_ORDER_KEYS];
        for (at, key) in order[..presorted].iter().enumerate() {
            if let OrderKey::Column { column, .. } = key {
                columns[at] = *column;
            }
        }
        paths::estimate_groups(&parent, &columns[..presorted], size.rows)
    };
    let upper = paths::upper_paths(
        kept,
        size.rows,
        width,
        order.len(),
        shape,
        costs,
        &groups,
        arena,
    )?;
    let option = options[upper.scan.path].expect("a kept append");
    // Each member's chosen scan.
    let members = arena
        .alloc_slice_with(option.subpaths.len(), |_| None::<MemberPath<'a>>)
        .map_err(|_| full())?;
    for (at, &(position, candidate)) in option.subpaths.iter().enumerate() {
        let member = live[position];
        let member_scope = member_scope(storage, txid, member.leaf, exposed, arena)?;
        let path = match paths::scan_path(
            storage,
            &member_scope,
            txid,
            condition,
            member.needed,
            params,
            arena,
            QueryShape {
                member: Member::Describe(candidate),
                ..member.shape
            },
        )? {
            Planned::Chosen(path) => path,
            Planned::Outside(shape) => return Ok(Planned::Outside(shape)),
            _ => return Ok(Planned::Outside("a member planned outside its tree")),
        };
        let sort = (!contained(option.presorted, candidate.presorted)).then(|| {
            sort_cost(
                candidate,
                member.paths.rows,
                member.width,
                limit_tuples,
                costs,
                &methods,
            )
        });
        members[at] = Some(MemberPath {
            leaf: member.leaf,
            path: ScanPath {
                width: member.width as u32,
                ..path
            },
            sort: sort.map(|sorted| (sorted.startup, sorted.total)),
            kind_path: (candidate.kind, candidate.path),
        });
    }
    let members = &*arena
        .alloc_slice_with(members.len(), |at| members[at].expect("dense members"))
        .map_err(|_| full())?;
    let stop_after = match shape.limit {
        Some((Some(count), offset))
            if !order.is_empty() && upper.chosen.sort == SortKind::None && shape.stop_at_limit =>
        {
            Some((count.max(0.0) + offset.max(0.0)) as u64)
        }
        _ => None,
    };
    Ok(Planned::Tree(TreePath {
        kind: option.kind,
        members,
        order: &*arena
            .alloc_slice_copy(&order[..option.presorted])
            .map_err(|_| full())?,
        rows: size.rows,
        width: width as u32,
        startup_cost: upper.scan.startup,
        total_cost: upper.scan.total - result_cost,
        result_cost: result.map(|_| (upper.scan.startup, upper.scan.total)),
        sort: upper.chosen.sort,
        sort_cost: upper.sort_cost,
        limit_cost: upper.limit_cost,
        stop_after,
    }))
}

/// `rel->consider_startup`: a `LIMIT` gives the query a tuple fraction.
fn paths_consider_startup(shape: QueryShape) -> bool {
    shape
        .limit
        .is_some_and(|(count, _)| count.is_some_and(|count| count > 0.0))
}

/// `cost_sort` over a member's path, `limit_tuples` of its rows wanted.
fn sort_cost(
    path: Candidate,
    rows: f64,
    width: f64,
    limit_tuples: Option<f64>,
    costs: Costs,
    methods: &crate::sql::guc::PlannerMethods,
) -> Candidate {
    let (startup, run) = paths::tuplesort_cost(rows, width, limit_tuples, costs);
    let startup = startup + path.total;
    Candidate {
        startup,
        total: startup + run,
        disabled: path.disabled + u32::from(!methods.sort),
        ..path
    }
}

/// The path of `option`, numbered `at`: `cost_append` or
/// `create_merge_append_path` over its members' paths, sorted where they
/// lack its order; one member is its own path.
fn append_cost(
    live: &[Live<'_>],
    option: &AppendOption<'_>,
    at: usize,
    limit_tuples: Option<f64>,
    costs: Costs,
    methods: &crate::sql::guc::PlannerMethods,
) -> Candidate {
    let mut rows = 0.0;
    let mut disabled = 0;
    let mut startup = 0.0;
    let mut total = 0.0;
    let mut presorted = option.presorted;
    for (position, &(member, path)) in option.subpaths.iter().enumerate() {
        let member = &live[member];
        let sorted = if contained(option.presorted, path.presorted) || option.presorted == 0 {
            path
        } else {
            sort_cost(
                path,
                member.paths.rows,
                member.width,
                limit_tuples,
                costs,
                methods,
            )
        };
        rows += member.paths.rows;
        disabled += sorted.disabled;
        total += sorted.total;
        if option.presorted == 0 && option.kind == AppendKind::Append {
            if position == 0 {
                startup = sorted.startup;
            }
        } else {
            startup += sorted.startup;
        }
    }
    if let [(_, path)] = option.subpaths {
        // The `Append` of one member is that member's path.
        if option.kind == AppendKind::Append {
            presorted = path.presorted;
            startup = path.startup;
            total = path.total;
            disabled = path.disabled;
        }
    } else {
        match option.kind {
            AppendKind::Append => {
                total += costs.cpu_tuple * APPEND_CPU_COST_MULTIPLIER * rows;
            }
            AppendKind::Merge => {
                let streams = (option.subpaths.len() as f64).max(2.0);
                let log = streams.log2();
                let comparison = 2.0 * costs.cpu_operator;
                let merge_startup = comparison * streams * log;
                startup += merge_startup;
                total += merge_startup
                    + rows * comparison * log
                    + costs.cpu_tuple * APPEND_CPU_COST_MULTIPLIER * rows;
            }
        }
    }
    Candidate {
        kind: ScanKind::Seq,
        path: at,
        startup,
        total,
        disabled,
        presorted,
        sort: SortKind::None,
        reordered: false,
    }
}

/// The members' rows, tuples and widths (`set_append_rel_size`).
fn tree_size(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    live: &[Option<Live<'_>>],
) -> TreeSize {
    let mut size = TreeSize {
        rows: 0.0,
        tuples: 0.0,
        column_widths: [0.0; 64],
        width: 0.0,
    };
    let mut total_width = 0.0;
    for member in live.iter().flatten() {
        let rows = member.paths.rows;
        size.rows += rows;
        size.tuples += member.paths.tuples;
        total_width += member.width * rows;
        let member_definition = storage.table_def(member.leaf, txid);
        let statistics = storage.table_statistics(member.leaf, txid);
        for (column, &at) in member.columns.iter().enumerate().take(64) {
            let width = if at == usize::MAX {
                let meta = &definition.columns()[column];
                super::type_average_width(meta.ctype, meta.type_mod)
            } else {
                f64::from(super::output_width(
                    &member_definition,
                    &statistics,
                    Some(ColumnSet::from_column(at)),
                ))
            };
            size.column_widths[column] += width * rows;
        }
    }
    if size.rows > 0.0 {
        size.width = (total_width / size.rows).round_ties_even();
        for width in &mut size.column_widths {
            *width = (*width / size.rows).round_ties_even();
        }
    }
    size
}

/// Member `leaf`'s numbering of the tree's columns, the columns it reads
/// and the shape of its scan, under the tree's `shape` and `needed`.
fn member_inputs<'a>(
    storage: &Storage,
    txid: u32,
    definition: &TableDef,
    leaf: usize,
    shape: QueryShape,
    needed: Option<ColumnSet>,
    arena: &'a Arena,
) -> Result<(&'a [usize], Option<ColumnSet>, QueryShape<'a>), SqlError> {
    let member = storage.table_def(leaf, txid);
    let columns = &*arena
        .alloc_slice_with(definition.n_columns, |column| {
            member
                .column_index(definition.columns()[column].name.as_str())
                .unwrap_or(usize::MAX)
        })
        .map_err(|_| crate::sql::eval::arena_full())?;
    drop(member);
    let map = |set: ColumnSet| {
        let mut mapped = ColumnSet::EMPTY;
        for (column, &at) in columns.iter().enumerate() {
            if set.contains(column) && at != usize::MAX {
                mapped.insert(at);
            }
        }
        mapped
    };
    let mut member_order = [OrderKey::Expression; super::MAX_ORDER_KEYS];
    for (at, key) in shape.order().iter().enumerate() {
        member_order[at] = match *key {
            OrderKey::Column {
                column,
                descending,
                nulls_first,
            } => OrderKey::Column {
                column: columns[column],
                descending,
                nulls_first,
            },
            OrderKey::Expression => OrderKey::Expression,
        };
    }
    // A member returns the tree's columns, not its own.
    let all = ColumnSet::all(definition.n_columns);
    // A partitioned table's members compute the query's target list
    // themselves when it is not the members' own columns.
    let projection = shape
        .projection
        .filter(|projection| definition.partition.is_partitioned() && !projection.plain);
    Ok((
        columns,
        needed.map(map),
        QueryShape {
            order: member_order,
            order_len: shape.order_len,
            stop_at_limit: false,
            only: true,
            output: Some(map(shape.output.or(needed).unwrap_or(all))),
            probe: false,
            projection,
            member: Member::Paths,
            ..shape
        },
    ))
}

/// The scan of member `leaf` of the tree the one-relation `scope` reads
/// whose path planning the tree's `shape` chose as `kind` numbered `path`
/// ([`MemberPath::kind_path`]): a resumed write's.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
pub(crate) fn member_path<'a>(
    storage: &Storage,
    scope: &QueryScope<'_>,
    txid: u32,
    condition: Option<&'a Expr<'a>>,
    params: &[Datum<'a>],
    arena: &'a Arena,
    shape: QueryShape,
    leaf: usize,
    (kind, path): (ScanKind, usize),
) -> Result<Option<ScanPath<'a>>, SqlError> {
    let Some(definition) = scope.defs.stored(0) else {
        return Ok(None);
    };
    let exposed = arena
        .alloc_str(scope.names[0])
        .map_err(|_| crate::sql::eval::arena_full())?;
    let (_, needed, member_shape) =
        member_inputs(storage, txid, definition, leaf, shape, None, arena)?;
    let member_scope = member_scope(storage, txid, leaf, exposed, arena)?;
    Ok(paths::scan_path(
        storage,
        &member_scope,
        txid,
        condition,
        needed,
        params,
        arena,
        QueryShape {
            member: Member::Describe(Candidate {
                kind,
                path,
                startup: 0.0,
                total: 0.0,
                disabled: 0,
                presorted: 0,
                sort: SortKind::None,
                reordered: false,
            }),
            ..member_shape
        },
    )?
    .chosen())
}

/// The one-relation scope of member `leaf` exposed under `exposed`.
pub(crate) fn member_scope<'s>(
    storage: &'s Storage,
    txid: u32,
    leaf: usize,
    exposed: &'s str,
    arena: &'s Arena,
) -> Result<QueryScope<'s>, SqlError> {
    let full = || crate::sql::eval::arena_full();
    let member = storage.table_def(leaf, txid);
    let schema = arena
        .alloc_str(member.schema.as_str())
        .map_err(|_| full())?;
    let name = arena.alloc_str(member.name.as_str()).map_err(|_| full())?;
    drop(member);
    let from = arena
        .alloc(crate::sql::ast::FromClause {
            base: super::super::dml_target_ref(
                crate::sql::ast::QualName {
                    schema: Some(schema),
                    name,
                },
                Some(exposed),
            ),
            joins: &[],
        })
        .map_err(|_| full())?;
    QueryScope::resolve_schema(storage, from, txid, arena, None)
}

/// A tree with a foreign member: its local members are scanned
/// sequentially when none has an index path.
#[expect(clippy::too_many_arguments, reason = "planner inputs are explicit")]
fn probe_members<'a>(
    storage: &Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    condition: Option<&'a Expr<'a>>,
    needed: Option<ColumnSet>,
    params: &[Datum<'a>],
    arena: &'a Arena,
    shape: QueryShape,
    definition: &TableDef,
    planned: &[usize],
) -> Result<Planned<'a>, SqlError> {
    let exposed = arena
        .alloc_str(scope.names[0])
        .map_err(|_| crate::sql::eval::arena_full())?;
    for &leaf in planned {
        let member = storage.table_def(leaf, txid);
        if member.kind != crate::storage::TableKind::Local {
            continue;
        }
        let member_needed = needed.map(|needed| {
            let mut set = ColumnSet::EMPTY;
            for (column, meta) in definition.columns().iter().enumerate() {
                if needed.contains(column)
                    && let Some(at) = member.column_index(meta.name.as_str())
                {
                    set.insert(at);
                }
            }
            set
        });
        drop(member);
        let member_scope = member_scope(storage, txid, leaf, exposed, arena)?;
        if let Planned::Outside(found) = paths::scan_path(
            storage,
            &member_scope,
            txid,
            condition,
            member_needed,
            params,
            arena,
            QueryShape {
                only: true,
                probe: true,
                ..shape
            },
        )? {
            return Ok(Planned::Outside(if found == "index paths" {
                if definition.partition.is_partitioned() {
                    "a partitioned table whose partitions have index paths and a foreign partition"
                } else {
                    "an inheritance tree whose members have index paths and a foreign member"
                }
            } else {
                found
            }));
        }
    }
    Ok(Planned::SeqOnly)
}
