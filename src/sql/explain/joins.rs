//! The plan tree of a join PostgreSQL's join search planned
//! ([`query::index_paths::join_query`]): its joins, their `Hash`, `Sort` and
//! `Materialize` inputs and the relations' scans, with PostgreSQL's costs,
//! as `create_plan` turns the chosen path into nodes.

use core::fmt::Write as _;

use super::targets::{BlockTargets, Demand, Output};
use super::{
    KeyLabel, Plan, PlanKeys, PlanNode, QualLabel, chosen_scan_node, single_relation_quals,
};
use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::query::index_paths::join_query::ModeledJoin;
use crate::sql::query::index_paths::joins::{ClauseList, JoinType, Keys, Path, PathKind, Relids};
use crate::sql::query::{self, QueryScope};
use crate::util::StackStr;

/// The planned join's nodes, the top join first. `targets` holds the
/// query's target lists for `VERBOSE` when it has no grouping or windows,
/// its stages the join, then the `Sort` and `Limit` above it.
#[expect(clippy::too_many_arguments, reason = "plan context")]
pub(super) fn join_plan<'a>(
    storage: &crate::storage::Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    model: &ModeledJoin<'_>,
    (sort_keys, presorted, group): (PlanKeys, Option<PlanKeys>, &[PlanNode]),
    targets: Option<&BlockTargets<'_>>,
    block: query::instrument::BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    // Each path of the chosen plan prints as at most three nodes (a join
    // with its `Hash` or a `Sort` of each input).
    // A `LATERAL` subquery's scan prints its query's plan.
    let lateral_nodes = MAX_LATERAL_NODES
        * model
            .rels
            .iter()
            .filter(|rel| rel.lateral.is_some())
            .count();
    let mut plan = Plan::new(
        arena,
        plan_paths(&model.plan, model.plan.top) * 3 + 6 + lateral_nodes,
    )?;
    let mut writer = Writer {
        storage,
        txid,
        scope,
        model,
        block,
        arena,
        top_depth: u8::from(model.plan.upper.limit.is_some())
            + u8::from(model.plan.upper.sort.is_some())
            + group.len() as u8,
        scanned: scanned_rels(&model.plan, model.plan.top),
        targets,
    };
    // The stages' outputs, bottom up: the join's, the grouping and window
    // nodes' (`group`, top first), the `Sort`'s, the `Limit`'s.
    let stage_output = |index: usize| targets.map(|targets| targets.stage(index));
    let sort_stage = 1 + group.len();
    let stages = sort_stage + usize::from(model.plan.upper.sort.is_some());
    // The query's `Limit` and `Sort` above the join, with the target
    // list's width.
    let upper = model.plan.upper;
    let top = model.plan.path(model.plan.top);
    // Above a grouping the nodes return its groups.
    let (rows, width) = match upper.group {
        Some(chosen) => (chosen.rows, chosen.width as u32),
        None => (top.rows, model.final_width as u32),
    };
    // Above windows the rows are the top window's.
    let width = match model.windows.len() {
        0 => width,
        count => upper.windows[count - 1].width as u32,
    };
    let mut depth = 0;
    if let Some((startup, total, rows)) = upper.limit {
        plan.push(PlanNode {
            name: StackStr::from_str("Limit"),
            key: Some(query::instrument::NodeKey::new(
                block,
                query::instrument::Role::Limit,
            )),
            depth,
            startup_cost: startup,
            total_cost: total,
            rows,
            width,
            output_unprintable: stage_output(stages).is_none(),
            output: stage_output(stages).unwrap_or_default(),
            ..PlanNode::EMPTY
        })?;
        depth += 1;
    }
    if let Some((startup, total, presorted_count)) = upper.sort {
        let mut node = PlanNode {
            name: StackStr::from_str(if presorted_count == 0 {
                "Sort"
            } else {
                "Incremental Sort"
            }),
            key: Some(query::instrument::NodeKey::new(
                block,
                query::instrument::Role::Sort,
            )),
            depth,
            startup_cost: startup,
            total_cost: total,
            rows,
            width,
            keys: sort_keys,
            output_unprintable: stage_output(sort_stage).is_none(),
            output: stage_output(sort_stage).unwrap_or_default(),
            ..PlanNode::EMPTY
        };
        if let Some(prefix) = presorted {
            node = node
                .with_qual(
                    QualLabel::PresortedKey,
                    super::presorted_text(&prefix, false),
                )
                .with_verbose_qual(
                    QualLabel::PresortedKey,
                    super::presorted_text(&prefix, true),
                );
        }
        plan.push(node)?;
        depth += 1;
    }
    for (at, node) in group.iter().enumerate() {
        let mut node = PlanNode { depth, ..*node };
        if targets.is_some() {
            node.set_output(stage_output(group.len() - at));
        }
        plan.push(node)?;
        depth += 1;
    }
    writer.path(&mut plan, model.plan.top, depth, Demand::Exact)?;
    Ok(plan)
}

/// The target lists of a sort's input and of the sort, each `None` when
/// unprintable.
type SortLists = (Option<Output>, Option<Output>);

/// A node that does not project (`Hash`, `Sort`, `Materialize`,
/// `Memoize`) returns its input's target list: the node at `at` takes the
/// list of the node pushed after it.
/// The most nodes the plan of one `LATERAL` subquery the search plans
/// holds: a scan of one table (a bitmap tree's leaves included) under its
/// sort, aggregate and limit.
const MAX_LATERAL_NODES: usize = 72;

fn pass_through(plan: &mut Plan<'_>, at: usize) {
    let input = plan.nodes[at + 1];
    plan.nodes[at].output = input.output;
    plan.nodes[at].output_unprintable = input.output_unprintable;
}

struct Writer<'w, 'm, 's, 'a> {
    storage: &'w crate::storage::Storage,
    txid: u32,
    scope: &'w QueryScope<'s>,
    model: &'w ModeledJoin<'m>,
    block: query::instrument::BlockId,
    arena: &'a Arena,
    /// The depth of the join's top node, below the query's own.
    top_depth: u8,
    /// The relations some node scans: when there are any, a column of
    /// another (a dummy relation's) prints without its relation's name, as
    /// EXPLAIN names only the range table entries the plan scans.
    scanned: Relids,
    /// The query's target lists, which `VERBOSE` prints.
    targets: Option<&'w BlockTargets<'w>>,
}

/// The paths of the plan under path `id`.
fn plan_paths(plan: &query::index_paths::joins::JoinPlan<'_>, id: u32) -> usize {
    1 + match plan.path(id).kind {
        PathKind::Dummy | PathKind::Scan { .. } => 0,
        PathKind::Material { input }
        | PathKind::Unique { input, .. }
        | PathKind::Memoize { input, .. } => plan_paths(plan, input),
        PathKind::NestLoop { outer, inner, .. }
        | PathKind::HashJoin { outer, inner, .. }
        | PathKind::MergeJoin { outer, inner, .. } => {
            plan_paths(plan, outer) + plan_paths(plan, inner)
        }
    }
}

/// The base relations the plan under path `id` scans.
pub(super) fn scanned_rels(plan: &query::index_paths::joins::JoinPlan<'_>, id: u32) -> Relids {
    match plan.path(id).kind {
        PathKind::Dummy => Relids::EMPTY,
        PathKind::Scan { rel, .. } => Relids::of(rel),
        PathKind::Material { input }
        | PathKind::Unique { input, .. }
        | PathKind::Memoize { input, .. } => scanned_rels(plan, input),
        PathKind::NestLoop { outer, inner, .. }
        | PathKind::HashJoin { outer, inner, .. }
        | PathKind::MergeJoin { outer, inner, .. } => {
            scanned_rels(plan, outer) | scanned_rels(plan, inner)
        }
    }
}

impl Writer<'_, '_, '_, '_> {
    fn node(&self, name: &str, path: &Path, depth: u8) -> PlanNode {
        PlanNode {
            name: StackStr::from_str(name),
            depth,
            startup_cost: path.startup,
            // The top relation's paths compute the target list
            // (`create_projection_path`).
            total_cost: if depth == self.top_depth {
                self.model.plan.upper.top_total
            } else {
                path.total
            },
            rows: path.rows,
            // The top relation's paths carry the query's output
            // (`apply_scanjoin_target_to_paths`).
            width: if depth == self.top_depth {
                self.model.output_width as u32
            } else {
                path.width as u32
            },
            output_unprintable: true,
            ..PlanNode::EMPTY
        }
    }

    /// The node's identity in execution ([`query::join_exec`]).
    fn keyed(
        &self,
        node: PlanNode,
        role: impl FnOnce(u16) -> query::instrument::Role,
        path: u32,
    ) -> PlanNode {
        match query::join_exec_node_number(path) {
            Some(number) => PlanNode {
                key: Some(query::instrument::NodeKey::new(self.block, role(number))),
                ..node
            },
            None => PlanNode {
                analyze_unsupported: Some(
                    "EXPLAIN ANALYZE of a join of this many paths is not supported",
                ),
                ..node
            },
        }
    }

    /// The target list a join (or a scan left alone by removed joins)
    /// returns: the top one computes the scan/join target, a lower one its
    /// relations' columns needed above it (`build_joinrel_tlist`).
    fn join_output(&self, relids: Relids, depth: u8) -> Result<Option<Output>, SqlError> {
        let Some(targets) = self.targets else {
            return Ok(None);
        };
        if depth == self.top_depth {
            return targets.scan_join().map(Some);
        }
        self.columns_output(relids)
    }

    /// The columns of `relids`' target list, in its order.
    fn columns_output(&self, relids: Relids) -> Result<Option<Output>, SqlError> {
        let mut output = Output::new();
        let mut failed = Ok(());
        self.model.plan.target_list(relids, &mut |needed| {
            if failed.is_err() {
                return;
            }
            let mut text = StackStr::<512>::new();
            failed = self.column(
                (usize::from(needed.rel), usize::from(needed.column)),
                &mut text,
            );
            if !output.is_empty() {
                let _ = output.write_str(", ");
            }
            let _ = output.write_str(text.as_str());
        });
        failed?;
        Ok((!output.is_truncated()).then_some(output))
    }

    fn path(
        &mut self,
        plan: &mut Plan<'_>,
        id: u32,
        depth: u8,
        demand: Demand,
    ) -> Result<(), SqlError> {
        let path = *self.model.plan.path(id);
        match path.kind {
            PathKind::Dummy => {
                let node = PlanNode {
                    rows: 0.0,
                    ..self.node("Result", &path, depth)
                };
                plan.push(
                    node.with_qual(QualLabel::OneTimeFilter, Some(StackStr::from_str("false"))),
                )
            }
            PathKind::Scan { rel, path } => self.scan(plan, usize::from(rel), path, depth, demand),
            PathKind::Material { input } => {
                let node = self.node("Materialize", &path, depth);
                let at = plan.count;
                plan.push(self.keyed(node, query::instrument::Role::Materialize, id))?;
                self.path(plan, input, depth + 1, Demand::Small)?;
                pass_through(plan, at);
                Ok(())
            }
            PathKind::Unique {
                input,
                method: query::index_paths::joins::UniqueMethod::Noop,
                ..
            } => self.path(plan, input, depth, demand),
            PathKind::Unique {
                input,
                keys,
                key_count,
                method: query::index_paths::joins::UniqueMethod::Hash,
            } => {
                // A `HashAggregate` grouping by the compared columns.
                let mut node = PlanNode {
                    analyze_unsupported: Some(
                        "EXPLAIN ANALYZE of a semi join's unique-ified relation is not supported",
                    ),
                    ..self.node("HashAggregate", &path, depth)
                };
                let mut shown = PlanKeys {
                    label: KeyLabel::Group,
                    ..PlanKeys::NONE
                };
                for member in self.model.plan.keys(keys, key_count) {
                    let mut text = StackStr::<512>::new();
                    self.operand(
                        (usize::from(member.rel), usize::from(member.column)),
                        &mut text,
                    )?;
                    for held in [&mut shown.plain, &mut shown.verbose] {
                        if !held.is_empty() {
                            let _ = write!(held, "{}", super::KEY_SEPARATOR);
                        }
                        let _ = write!(held, "{}", text.as_str());
                    }
                }
                node.keys = shown;
                let keys = self.model.plan.keys(keys, key_count);
                let input_relids = self.model.plan.path(input).relids;
                node.set_output(self.columns_output(input_relids)?);
                plan.push(node)?;
                // `create_unique_plan`: the input returns its columns and the
                // compared expressions it lacks, or under `CP_LABEL_TLIST`
                // its physical columns when it lacks none.
                let computed = self.unique_input(input_relids, keys, false)?;
                let at = plan.count;
                self.path(plan, input, depth + 1, Demand::Any)?;
                if computed.as_ref().is_none_or(|(_, added)| *added) {
                    self.project(plan, at, computed.map(|(output, _)| output));
                }
                Ok(())
            }
            PathKind::Unique {
                input,
                keys,
                key_count,
                ..
            } => {
                // `create_unique_plan`: a `Unique` over a `Sort` of the
                // compared columns (`label_sort_with_costsize`), both
                // returning their input's columns and the compared
                // expressions it computes.
                const UNSUPPORTED: Option<&str> =
                    Some("EXPLAIN ANALYZE of a semi join's unique-ified relation is not supported");
                let keys = self.model.plan.keys(keys, key_count);
                let input_relids = self.model.plan.path(input).relids;
                let above = self
                    .unique_input(input_relids, keys, true)?
                    .map(|(output, _)| output);
                let mut unique = PlanNode {
                    analyze_unsupported: UNSUPPORTED,
                    ..self.node("Unique", &path, depth)
                };
                unique.set_output(above);
                plan.push(unique)?;
                let input_path = *self.model.plan.path(input);
                let (startup, total) =
                    query::index_paths::joins::sort_cost(&input_path, self.model.plan.settings);
                let mut sort = PlanNode {
                    analyze_unsupported: UNSUPPORTED,
                    ..self.node(
                        "Sort",
                        &Path {
                            startup,
                            total,
                            ..input_path
                        },
                        depth + 1,
                    )
                };
                let mut shown = PlanKeys {
                    label: KeyLabel::Sort,
                    ..PlanKeys::NONE
                };
                for member in keys {
                    let mut text = StackStr::<512>::new();
                    self.referenced(*member, &mut text)?;
                    for held in [&mut shown.plain, &mut shown.verbose] {
                        if !held.is_empty() {
                            let _ = write!(held, "{}", super::KEY_SEPARATOR);
                        }
                        let _ = write!(held, "{}", text.as_str());
                    }
                }
                sort.keys = shown;
                sort.set_output(above);
                plan.push(sort)?;
                let computed = self
                    .unique_input(input_relids, keys, false)?
                    .map(|(output, _)| output);
                let at = plan.count;
                self.path(plan, input, depth + 2, Demand::Small)?;
                self.project(plan, at, computed);
                Ok(())
            }
            PathKind::Memoize {
                input,
                keys,
                key_count,
                binary,
                ..
            } => {
                let mut key = StackStr::<512>::new();
                for (at, member) in self.model.plan.keys(keys, key_count).iter().enumerate() {
                    if at > 0 {
                        let _ = write!(key, ", ");
                    }
                    self.column(
                        (usize::from(member.rel), usize::from(member.column)),
                        &mut key,
                    )?;
                }
                let node = self
                    .node("Memoize", &path, depth)
                    .with_qual(QualLabel::CacheKey, Some(key))
                    .with_qual(
                        QualLabel::CacheMode,
                        Some(StackStr::from_str(if binary {
                            "binary"
                        } else {
                            "logical"
                        })),
                    );
                let at = plan.count;
                plan.push(self.keyed(node, query::instrument::Role::Memoize, id))?;
                self.path(plan, input, depth + 1, Demand::Small)?;
                pass_through(plan, at);
                Ok(())
            }
            PathKind::NestLoop {
                jointype,
                outer,
                inner,
                clauses,
                inner_unique,
            } => {
                let (join_filter, filter) =
                    self.split(&clauses, path.relids, jointype, &ClauseList::EMPTY);
                let mut node = self
                    .node(join_name("Nested Loop", jointype).as_str(), &path, depth)
                    .with_qual(
                        QualLabel::JoinFilter,
                        self.conditions(&join_filter, None, (false, false))?,
                    )
                    .with_qual(
                        QualLabel::Filter,
                        self.conditions(&filter, None, (false, false))?,
                    );
                node.set_output(self.join_output(path.relids, depth)?);
                node.inner_unique = inner_unique;
                plan.push(self.keyed(node, query::instrument::Role::Join, id))?;
                self.path(plan, outer, depth + 1, Demand::Any)?;
                self.path(plan, inner, depth + 1, Demand::Any)
            }
            PathKind::HashJoin {
                jointype,
                outer,
                inner,
                hash,
                clauses,
                batches,
                inner_unique,
                ..
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let (join_filter, filter) = self.split(&clauses, path.relids, jointype, &hash);
                let mut node = self
                    .node(join_name("Hash", jointype).as_str(), &path, depth)
                    .with_qual(
                        QualLabel::HashCond,
                        self.conditions(&hash, Some(outer_relids), (false, false))?,
                    )
                    .with_qual(
                        QualLabel::JoinFilter,
                        self.conditions(&join_filter, None, (false, false))?,
                    )
                    .with_qual(
                        QualLabel::Filter,
                        self.conditions(&filter, None, (false, false))?,
                    );
                node.set_output(self.join_output(path.relids, depth)?);
                node.inner_unique = inner_unique;
                plan.push(self.keyed(node, query::instrument::Role::Join, id))?;
                // `create_hashjoin_plan`: a batched join's outer input
                // returns only the columns needed.
                let outer_demand = if batches > 1 {
                    Demand::Small
                } else {
                    Demand::Any
                };
                self.path(plan, outer, depth + 1, outer_demand)?;
                let inner_path = *self.model.plan.path(inner);
                let hash_node = PlanNode {
                    startup_cost: inner_path.total,
                    ..self.node("Hash", &inner_path, depth + 1)
                };
                let at = plan.count;
                plan.push(self.keyed(hash_node, query::instrument::Role::Hash, id))?;
                self.path(plan, inner, depth + 2, Demand::Small)?;
                pass_through(plan, at);
                Ok(())
            }
            PathKind::MergeJoin {
                jointype,
                outer,
                inner,
                merge,
                clauses,
                outer_sort,
                inner_sort,
                materialize_inner,
                inner_unique,
                ..
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let (join_filter, filter) = self.split(&clauses, path.relids, jointype, &merge);
                let mut node = self
                    .node(join_name("Merge", jointype).as_str(), &path, depth)
                    .with_qual(
                        QualLabel::MergeCond,
                        self.conditions(
                            &merge,
                            Some(outer_relids),
                            (
                                !outer_sort.as_slice().is_empty(),
                                !inner_sort.as_slice().is_empty(),
                            ),
                        )?,
                    )
                    .with_qual(
                        QualLabel::JoinFilter,
                        self.conditions(&join_filter, None, (false, false))?,
                    )
                    .with_qual(
                        QualLabel::Filter,
                        self.conditions(&filter, None, (false, false))?,
                    );
                node.set_output(self.join_output(path.relids, depth)?);
                node.inner_unique = inner_unique;
                plan.push(self.keyed(node, query::instrument::Role::Join, id))?;
                self.sorted(plan, id, outer, outer_sort, false, 0, depth + 1)?;
                self.sorted(plan, id, inner, inner_sort, materialize_inner, 1, depth + 1)
            }
        }
    }

    /// The join's matching conditions and its filters, as its plan
    /// evaluates them.
    fn split(
        &self,
        clauses: &ClauseList,
        relids: Relids,
        jointype: JoinType,
        keyed: &ClauseList,
    ) -> (ClauseList, ClauseList) {
        query::index_paths::joins::split_join_clauses(
            self.model.plan.clauses,
            clauses,
            relids,
            jointype,
            keyed,
        )
    }

    /// An input of a merge join: its sort and the `Materialize` above it.
    #[allow(clippy::too_many_arguments)]
    fn sorted(
        &mut self,
        plan: &mut Plan<'_>,
        join: u32,
        id: u32,
        keys: Keys,
        materialize: bool,
        side: u8,
        depth: u8,
    ) -> Result<(), SqlError> {
        let settings = self.model.plan.settings;
        let input = *self.model.plan.path(id);
        let sorted = if keys.as_slice().is_empty() {
            None
        } else {
            let (startup, total) = query::index_paths::joins::sort_cost(&input, settings);
            Some(Path {
                startup,
                total,
                ..input
            })
        };
        let below = sorted.unwrap_or(input);
        let mut depth = depth;
        let material_at = materialize.then_some(plan.count);
        if materialize {
            let (startup, total) = query::index_paths::joins::material_cost(
                below.startup,
                below.total,
                below.rows,
                below.width,
                settings,
            );
            let material = Path {
                startup,
                total,
                ..below
            };
            let node = self.node("Materialize", &material, depth);
            plan.push(self.keyed(node, query::instrument::Role::Materialize, join))?;
            depth += 1;
        }
        let sort_at = sorted.is_some().then_some(plan.count);
        if let Some(sorted) = sorted {
            let mut node = self.node("Sort", &sorted, depth);
            node.keys = self.sort_keys(&keys, input.relids)?;
            plan.push(self.keyed(
                node,
                |number| query::instrument::Role::MergeSort(number, side),
                join,
            ))?;
            depth += 1;
        }
        // `create_mergejoin_plan`: a sorted input returns only the columns
        // needed.
        let demand = if sorted.is_some() {
            Demand::Small
        } else {
            Demand::Any
        };
        self.path(plan, id, depth, demand)?;
        if let Some(at) = sort_at {
            pass_through(plan, at);
            // A key the input does not hold is computed by the input as a
            // resjunk entry (`prepare_sort_from_pathkeys`), which the sort
            // reads.
            if let Some((input_list, sort_list)) =
                self.computed_keys(plan, at, &keys, input.relids)?
            {
                self.project(plan, at + 1, input_list);
                plan.nodes[at].set_output(sort_list);
            }
        }
        if let Some(at) = material_at {
            pass_through(plan, at);
        }
        Ok(())
    }

    /// The target lists of a sort's input and of the sort when it sorts by
    /// an expression its input computes, `None` when it computes none.
    fn computed_keys(
        &self,
        plan: &Plan<'_>,
        at: usize,
        keys: &Keys,
        relids: Relids,
    ) -> Result<Option<SortLists>, SqlError> {
        use query::index_paths::joins::KEY_CLASS;
        let base = plan.nodes[at + 1];
        let (mut input, mut sort) = (base.output, base.output);
        let mut computed = false;
        for &key in keys.as_slice() {
            let class = key & KEY_CLASS;
            let Some(at) = self.model.plan.classes[usize::from(class)].sort_member(relids) else {
                continue;
            };
            let member = self.model.member(class, at as u8);
            if query::index_paths::joins::Member::plain(0, member.1 as u16)
                .expression_id()
                .is_none()
            {
                continue;
            }
            computed = true;
            let mut text = StackStr::<512>::new();
            self.column(member, &mut text)?;
            for (list, shown) in [(&mut input, false), (&mut sort, true)] {
                if !list.is_empty() {
                    let _ = list.write_str(", ");
                }
                if shown {
                    let _ = write!(list, "({})", text.as_str());
                } else {
                    let _ = list.write_str(text.as_str());
                }
            }
        }
        if !computed {
            return Ok(None);
        }
        let printable =
            |list: Output| (!base.output_unprintable && !list.is_truncated()).then_some(list);
        Ok(Some((printable(input), printable(sort))))
    }

    /// The scan of path `path` of relation `rel`: its index conditions,
    /// filter and recheck, a parameterized path's reading the outer
    /// relation's columns qualified.
    fn scan(
        &mut self,
        plan: &mut Plan<'_>,
        rel: usize,
        path: u16,
        depth: u8,
        demand: Demand,
    ) -> Result<(), SqlError> {
        let modeled = self.model.rels[rel];
        if let Some(lateral) = modeled.lateral {
            return self.lateral_scan(plan, rel, &lateral, depth);
        }
        let scan = modeled.scan(self.storage, self.txid, path, &[], self.arena)?;
        let single =
            QueryScope::resolve_schema(self.storage, modeled.from, self.txid, self.arena, None)?;
        let (mut node, used) = chosen_scan_node(&single, depth, &scan.path);
        let definition = single.defs.stored(0).expect("a plain table");
        let pushed = super::push_outer_mask(
            self.scope,
            scan.param & query::index_paths::joins::BASE_RELIDS,
        );
        // Each condition as printed, and as `VERBOSE` prints it with its
        // columns qualified.
        let quals = |index: Option<&query::IndexAccessPlan<'_>>| {
            let printed = |verbose| {
                single_relation_quals(
                    self.storage,
                    self.txid,
                    single.names[0],
                    definition,
                    &[],
                    scan.condition,
                    &[],
                    index,
                    verbose,
                    self.arena,
                )
            };
            Ok::<_, SqlError>((printed(false)?, printed(true)?))
        };
        let printed = (|| {
            let ((index_cond, filter), (verbose_index_cond, verbose_filter)) =
                quals(used.as_ref())?;
            let recheck = match (scan.path.kind, used.as_ref()) {
                (query::index_paths::ScanKind::Bitmap, Some(used)) => {
                    let (plain, verbose) = quals(Some(&used.written()))?;
                    (plain.0, verbose.0)
                }
                _ => (None, None),
            };
            let mut leaves = [(None, None); 64];
            for (at, node) in scan.path.bitmap.iter().enumerate().take(64) {
                if let query::index_paths::BitmapPlanKind::Index { conditions, .. } = node.kind {
                    let leaf = query::IndexAccessPlan::chosen_conditions(query::ChosenConditions {
                        conditions,
                        recheck: &[],
                        absorbed: &[],
                    });
                    let (plain, verbose) = quals(Some(&leaf))?;
                    leaves[at] = (plain.0, verbose.0);
                }
            }
            Ok::<_, SqlError>((
                (index_cond, verbose_index_cond),
                (filter, verbose_filter),
                recheck,
                leaves,
            ))
        })();
        super::pop_outer_relations(pushed);
        let (index_cond, filter, recheck, leaves) = printed?;
        node = node
            .with_qual(QualLabel::IndexCond, index_cond.0)
            .with_qual(QualLabel::Filter, filter.0)
            .with_verbose_qual(QualLabel::IndexCond, index_cond.1)
            .with_verbose_qual(QualLabel::Filter, filter.1);
        if recheck.0.is_some() {
            node = node
                .with_qual(QualLabel::RecheckCond, recheck.0)
                .with_verbose_qual(QualLabel::RecheckCond, recheck.1);
        }
        node.set_output(self.scan_output(rel, &scan.path, depth, demand)?);
        // A scan left alone by removed joins carries the query's output.
        if depth == 0 {
            node.width = self.model.output_width as u32;
        }
        let block = self.block.child(query::join_exec_scan_segment(rel));
        node.key = Some(query::instrument::NodeKey::new(
            block,
            query::instrument::Role::Scan(0),
        ));
        if scan.path.bitmap.len() > 1 {
            let depth = node.depth;
            super::push_scan(plan, node)?;
            return super::push_bitmap_tree(
                plan,
                scan.path.bitmap,
                &leaves[..scan.path.bitmap.len()],
                depth,
                block,
            );
        }
        super::push_scan(plan, node)
    }

    /// A `LATERAL` subquery's scan: the trivial `SubqueryScan` that
    /// `setrefs.c` removes leaves its query's plan, its outer columns
    /// printed as the parameters they are, its nodes counted in the
    /// relation's block.
    fn lateral_scan(
        &mut self,
        plan: &mut Plan<'_>,
        rel: usize,
        lateral: &query::index_paths::join_query::LateralRel<'_>,
        depth: u8,
    ) -> Result<(), SqlError> {
        let block = self.block.child(query::join_exec_scan_segment(rel));
        let Some(planned) = super::lateral_subquery_plan(
            self.storage,
            self.txid,
            self.scope,
            lateral.select,
            lateral.relids,
            block,
            self.arena,
        )?
        else {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN of this LATERAL subquery is not supported"
            ));
        };
        for node in &planned.nodes[..planned.count] {
            plan.push(PlanNode {
                depth: node.depth + depth,
                // Its target list reads the subquery's own expressions.
                output_unprintable: true,
                ..*node
            })?;
        }
        Ok(())
    }

    /// The target list of a unique-ified relation's input
    /// (`create_unique_plan`): `relids`' columns, then each compared
    /// expression they lack, as the input computes it or, `referenced`, as
    /// a node above reads it. Whether any expression was added.
    fn unique_input(
        &self,
        relids: Relids,
        keys: &[query::index_paths::joins::Member],
        referenced: bool,
    ) -> Result<Option<(Output, bool)>, SqlError> {
        let Some(mut output) = self.columns_output(relids)? else {
            return Ok(None);
        };
        let mut added = false;
        for member in keys {
            let mut text = StackStr::<512>::new();
            let computed = if referenced {
                self.referenced(*member, &mut text)?
            } else {
                self.operand(
                    (usize::from(member.rel), usize::from(member.column)),
                    &mut text,
                )?;
                self.referenced(*member, &mut StackStr::new())?
            };
            if !computed {
                continue;
            }
            if !output.is_empty() {
                let _ = output.write_str(", ");
            }
            let _ = output.write_str(text.as_str());
            added = true;
        }
        Ok((!output.is_truncated()).then_some((output, added)))
    }

    /// A compared member as a node above the one computing it reads it:
    /// an expression, or a column its operator reads relabeled, in
    /// parentheses (`get_variable`). Whether it is computed (no plain
    /// column).
    fn referenced(
        &self,
        member: query::index_paths::joins::Member,
        text: &mut StackStr<512>,
    ) -> Result<bool, SqlError> {
        let at = (usize::from(member.rel), usize::from(member.column));
        let mut operand = StackStr::<512>::new();
        self.operand(at, &mut operand)?;
        let mut column = StackStr::<512>::new();
        self.column(at, &mut column)?;
        let computed = member.expression_id().is_some() || operand.as_str() != column.as_str();
        if computed {
            let _ = write!(text, "({})", operand.as_str());
        } else {
            let _ = text.write_str(column.as_str());
        }
        Ok(computed)
    }

    /// Gives the node at `at`, a scan or join, the target list `output`
    /// its parent sets (`create_unique_plan`, `prepare_sort_from_pathkeys`);
    /// a node that cannot project would need a `Result` above it, which
    /// the plan does not hold.
    fn project(&self, plan: &mut Plan<'_>, at: usize, output: Option<Output>) {
        let node = &mut plan.nodes[at];
        let projects = !matches!(
            node.name.as_str(),
            "Materialize" | "Memoize" | "Hash" | "Sort" | "Unique" | "HashAggregate"
        );
        node.set_output(output.filter(|_| projects));
    }

    /// The target list of relation `rel`'s scan by `path`: the scan/join
    /// target when it is the plan's top node, else, as `create_scan_plan`
    /// builds it, the relation's physical columns when the parent accepts
    /// any (`use_physical_tlist`; an index-only scan's are its index's), or
    /// the columns needed above it.
    fn scan_output(
        &self,
        rel: usize,
        path: &query::index_paths::ScanPath<'_>,
        depth: u8,
        demand: Demand,
    ) -> Result<Option<Output>, SqlError> {
        use query::index_paths::ScanKind;
        let Some(targets) = self.targets else {
            return Ok(None);
        };
        if depth == self.top_depth {
            return targets.scan_join().map(Some);
        }
        let needed = self.columns_output(Relids::of(rel))?;
        if demand != Demand::Any {
            return Ok(needed);
        }
        match path.kind {
            ScanKind::IndexOnly => {
                let mut output = Output::new();
                for key in path.keys {
                    if key.expression.is_some() {
                        return Ok(None);
                    }
                    let mut text = StackStr::<512>::new();
                    self.column((rel, key.column), &mut text)?;
                    if !output.is_empty() {
                        let _ = output.write_str(", ");
                    }
                    let _ = output.write_str(text.as_str());
                }
                Ok((!output.is_truncated()).then_some(output))
            }
            // A bitmap scan returning no columns keeps its empty list.
            ScanKind::Bitmap if needed.as_ref().is_some_and(|list| list.is_empty()) => Ok(needed),
            _ => match targets.physical(rel) {
                Some(physical) => physical.map(Some),
                None => Ok(needed),
            },
        }
    }

    /// `relation.column`, as the join prints a column; an expression
    /// member as `ruleutils.c` prints the expression.
    fn column(
        &self,
        (table, column): (usize, usize),
        text: &mut StackStr<512>,
    ) -> Result<(), SqlError> {
        if let Some(id) = query::index_paths::joins::Member::plain(0, column as u16).expression_id()
        {
            let Some(printed) = self.deparse_as(self.model.expressions[id].expression, false)?
            else {
                return Err(crate::sql_err!(
                    crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                    "EXPLAIN of this join key expression is not supported"
                ));
            };
            let _ = write!(text, "{}", printed.as_str());
            return Ok(());
        }
        if self
            .model
            .rels
            .get(table)
            .is_some_and(|rel| rel.lateral.is_some())
        {
            return Err(crate::sql_err!(
                crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                "EXPLAIN VERBOSE of a LATERAL subquery's columns is not supported"
            ));
        }
        let definition = self.scope.defs.stored(table).expect("a plain table");
        // A plan scanning nothing names every relation (`set_rtable_names`
        // with no relations used).
        if self.scanned != Relids::EMPTY && self.scanned & Relids::of(table) == Relids::EMPTY {
            let _ = write!(
                text,
                "{}",
                crate::sql::catalog::deparse::printed_name(
                    definition.columns()[column].name.as_str()
                )
            );
            return Ok(());
        }
        let _ = write!(
            text,
            "{}.{}",
            crate::sql::catalog::deparse::printed_name(self.scope.names[table]),
            crate::sql::catalog::deparse::printed_name(definition.columns()[column].name.as_str())
        );
        Ok(())
    }

    /// A column as an equality's operand: a type without its own equality
    /// operator prints relabeled to the type whose operator compares it
    /// (`varchar` as `text`, `cidr` as `inet`).
    fn operand(
        &self,
        (table, column): (usize, usize),
        text: &mut StackStr<512>,
    ) -> Result<(), SqlError> {
        let ctype = match query::index_paths::joins::Member::plain(0, column as u16).expression_id()
        {
            Some(id) => self.model.expressions[id].ctype,
            None => {
                self.scope
                    .defs
                    .stored(table)
                    .expect("a plain table")
                    .columns()[column]
                    .ctype
            }
        };
        let relabeled = match ctype {
            crate::sql::types::ColType::Varchar => Some("text"),
            crate::sql::types::ColType::Cidr => Some("inet"),
            _ => None,
        };
        let Some(relabeled) = relabeled else {
            return self.column((table, column), text);
        };
        let _ = write!(text, "(");
        self.column((table, column), text)?;
        let _ = write!(text, ")::{relabeled}");
        Ok(())
    }

    /// The conditions of `clauses` ANDed, an equality with the column of
    /// `outer` first when given (`get_switched_clauses`), as written
    /// otherwise.
    /// A merge join's side its own `Sort` orders (`sorted`, outer side
    /// first) reads an expression key from the sort's target list, which
    /// prints parenthesized (`get_variable`).
    fn conditions(
        &self,
        clauses: &ClauseList,
        outer: Option<Relids>,
        sorted: (bool, bool),
    ) -> Result<Option<StackStr<512>>, SqlError> {
        // The constant `true` an outer join clause gave way to
        // (`reconsider_outer_join_clauses`) prints as nothing.
        let mut kept = [0; query::index_paths::joins::MAX_JOIN_CLAUSES];
        let mut count = 0;
        for &id in clauses.as_slice() {
            let clause = &self.model.plan.clauses[usize::from(id)];
            if !matches!(
                self.model.condition(clause),
                Some(crate::sql::ast::Expr::Bool(true))
            ) {
                kept[count] = id;
                count += 1;
            }
        }
        let ids = &kept[..count];
        if ids.is_empty() {
            return Ok(None);
        }
        let mut text = StackStr::<512>::new();
        if ids.len() > 1 {
            let _ = write!(text, "(");
        }
        for (at, &id) in ids.iter().enumerate() {
            if at > 0 {
                let _ = write!(text, " AND ");
            }
            let clause = &self.model.plan.clauses[usize::from(id)];
            let Some((mut left, mut right)) = self.model.clause_columns(clause) else {
                let expression = self.model.condition(clause).expect("a written condition");
                let Some(part) = self.deparse(expression)? else {
                    return Err(crate::sql_err!(
                        crate::sql::eval::sqlstate::FEATURE_NOT_SUPPORTED,
                        "EXPLAIN of this join condition is not supported"
                    ));
                };
                let _ = write!(text, "{}", part.as_str());
                continue;
            };
            if let Some(outer) = outer
                && outer & Relids::of(right.0) != Relids::EMPTY
            {
                core::mem::swap(&mut left, &mut right);
            }
            let sorted_operand = |side: (usize, usize), sorted: bool, text: &mut StackStr<512>| {
                let computed = sorted
                    && query::index_paths::joins::Member::plain(0, side.1 as u16)
                        .expression_id()
                        .is_some();
                if computed {
                    let _ = write!(text, "(");
                }
                self.operand(side, text)?;
                if computed {
                    let _ = write!(text, ")");
                }
                Ok::<_, SqlError>(())
            };
            let _ = write!(text, "(");
            sorted_operand(left, sorted.0, &mut text)?;
            let _ = write!(text, " = ");
            sorted_operand(right, sorted.1, &mut text)?;
            let _ = write!(text, ")");
        }
        if ids.len() > 1 {
            let _ = write!(text, ")");
        }
        Ok(Some(text))
    }

    /// A condition as `ruleutils.c` prints a join's qualification, its
    /// columns qualified by their relations.
    fn deparse(
        &self,
        expression: &crate::sql::ast::Expr<'_>,
    ) -> Result<Option<StackStr<512>>, SqlError> {
        self.deparse_as(expression, true)
    }

    /// A condition (`qual`), or a value of its own type, as `ruleutils.c`
    /// prints it in the join, its columns qualified by their relations.
    fn deparse_as(
        &self,
        expression: &crate::sql::ast::Expr<'_>,
        qual: bool,
    ) -> Result<Option<StackStr<512>>, SqlError> {
        let joined = self
            .arena
            .alloc_slice_with(self.scope.n, |table| {
                let mut joined = crate::sql::catalog::deparse::Joined::new(
                    self.scope.names[table],
                    self.scope.defs.stored(table).expect("a plain table"),
                );
                // A relation no node scans prints its columns unqualified.
                if self.scanned != Relids::EMPTY
                    && self.scanned & Relids::of(table) == Relids::EMPTY
                {
                    joined.printed = "";
                }
                joined
            })
            .map_err(|_| crate::sql::eval::arena_full())?;
        let context = crate::sql::catalog::deparse::Context {
            storage: self.storage,
            txid: self.txid,
            arena: self.arena,
            dependencies: None,
            table: None,
            parameters: &[],
            joined,
            pretty: false,
            qualify: true,
            domain_value: None,
            sublinks: None,
            references: None,
            folded: true,
            bare: None,
        };
        if qual {
            return super::deparse_qual(&context, &[expression], self.arena);
        }
        let mark = self.arena.mark();
        let printed = crate::sql::catalog::deparse::deparse_expression(
            &context,
            expression,
            crate::sql::catalog::deparse::Destination::Value,
            self.arena,
        )
        .map(|part| {
            part.and_then(|part| {
                let text = StackStr::<512>::from_str(part);
                (!text.is_truncated()).then_some(text)
            })
        });
        // SAFETY: the text was copied out of the arena.
        unsafe { self.arena.rewind_to(mark) };
        printed
    }

    /// A sort's keys: each class's first member among `relids`.
    fn sort_keys(&self, keys: &Keys, relids: Relids) -> Result<PlanKeys, SqlError> {
        let mut shown = PlanKeys {
            label: KeyLabel::Sort,
            ..PlanKeys::NONE
        };
        use query::index_paths::joins::{KEY_CLASS, KEY_DESCENDING, KEY_NULLS_FIRST};
        for &key in keys.as_slice() {
            let class = key & KEY_CLASS;
            let Some(at) = self.model.plan.classes[usize::from(class)].sort_member(relids) else {
                continue;
            };
            let mut text = StackStr::<512>::new();
            let member = self.model.member(class, at as u8);
            // An expression a sort computes is its input's target list
            // entry, which prints parenthesized (`get_variable`).
            let expression = query::index_paths::joins::Member::plain(0, member.1 as u16)
                .expression_id()
                .is_some();
            if expression {
                let _ = write!(text, "(");
            }
            self.column(member, &mut text)?;
            if expression {
                let _ = write!(text, ")");
            }
            // `show_sortorder_options`.
            let descending = key & KEY_DESCENDING != 0;
            let nulls_first = key & KEY_NULLS_FIRST != 0;
            if descending {
                let _ = write!(text, " DESC");
            }
            if nulls_first && !descending {
                let _ = write!(text, " NULLS FIRST");
            } else if !nulls_first && descending {
                let _ = write!(text, " NULLS LAST");
            }
            for held in [&mut shown.plain, &mut shown.verbose] {
                if !held.is_empty() {
                    let _ = write!(held, "{}", super::KEY_SEPARATOR);
                }
                let _ = write!(held, "{}", text.as_str());
            }
        }
        Ok(shown)
    }
}

/// A join node's name: its method, then its join type unless an inner
/// nested loop's.
fn join_name(method: &str, jointype: JoinType) -> StackStr<64> {
    let kind = match jointype {
        JoinType::Inner | JoinType::UniqueInner | JoinType::UniqueOuter => "",
        JoinType::Left => " Left",
        JoinType::Full => " Full",
        JoinType::Right => " Right",
        JoinType::Semi => " Semi",
        JoinType::Anti => " Anti",
        JoinType::RightSemi => " Right Semi",
        JoinType::RightAnti => " Right Anti",
    };
    let mut name = StackStr::<64>::new();
    if method == "Nested Loop" && jointype == JoinType::Inner {
        let _ = write!(name, "Nested Loop");
    } else {
        let _ = write!(name, "{method}{kind} Join");
    }
    name
}
