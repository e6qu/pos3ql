//! The plan tree of a join PostgreSQL's join search planned
//! ([`query::index_paths::join_query`]): its joins, their `Hash`, `Sort` and
//! `Materialize` inputs and the relations' scans, with PostgreSQL's costs,
//! as `create_plan` turns the chosen path into nodes.

use core::fmt::Write as _;

use super::{
    KeyLabel, Plan, PlanKeys, PlanNode, QualLabel, chosen_scan_node, single_relation_quals,
};
use crate::mem::arena::Arena;
use crate::sql::eval::SqlError;
use crate::sql::query::index_paths::join_query::ModeledJoin;
use crate::sql::query::index_paths::joins::{ClauseList, JoinType, Keys, Path, PathKind, Relids};
use crate::sql::query::{self, QueryScope};
use crate::util::StackStr;

/// The planned join's nodes, the top join first.
pub(super) fn join_plan<'a>(
    storage: &crate::storage::Storage,
    txid: u32,
    scope: &QueryScope<'_>,
    model: &ModeledJoin<'_>,
    (sort_keys, presorted, group): (PlanKeys, Option<PlanKeys>, &[PlanNode]),
    block: query::instrument::BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let mut plan = Plan::new(arena, model.plan.paths.len() * 3 + 6)?;
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
    };
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
            rows: rows as u64,
            width,
            output_unprintable: true,
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
            rows: rows as u64,
            width,
            keys: sort_keys,
            output_unprintable: true,
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
    for node in group {
        plan.push(PlanNode { depth, ..*node })?;
        depth += 1;
    }
    writer.path(&mut plan, model.plan.top, depth)?;
    Ok(plan)
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
}

/// The base relations the plan under path `id` scans.
pub(super) fn scanned_rels(plan: &query::index_paths::joins::JoinPlan<'_>, id: u32) -> Relids {
    match plan.path(id).kind {
        PathKind::Dummy => 0,
        PathKind::Scan { rel, .. } => 1 << rel,
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
            rows: path.rows as u64,
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

    fn path(&mut self, plan: &mut Plan<'_>, id: u32, depth: u8) -> Result<(), SqlError> {
        let path = *self.model.plan.path(id);
        match path.kind {
            PathKind::Dummy => {
                let node = PlanNode {
                    rows: 0,
                    ..self.node("Result", &path, depth)
                };
                plan.push(
                    node.with_qual(QualLabel::OneTimeFilter, Some(StackStr::from_str("false"))),
                )
            }
            PathKind::Scan { rel, path } => self.scan(plan, usize::from(rel), path, depth),
            PathKind::Material { input } => {
                let node = self.node("Materialize", &path, depth);
                plan.push(self.keyed(node, query::instrument::Role::Materialize, id))?;
                self.path(plan, input, depth + 1)
            }
            PathKind::Unique {
                input,
                method: query::index_paths::joins::UniqueMethod::Noop,
                ..
            } => self.path(plan, input, depth),
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
                for member in &keys[..usize::from(key_count)] {
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
                plan.push(node)?;
                self.path(plan, input, depth + 1)
            }
            PathKind::Unique {
                input,
                keys,
                key_count,
                ..
            } => {
                // `create_unique_plan`: a `Unique` over a `Sort` of the
                // compared columns (`label_sort_with_costsize`).
                const UNSUPPORTED: Option<&str> =
                    Some("EXPLAIN ANALYZE of a semi join's unique-ified relation is not supported");
                plan.push(PlanNode {
                    analyze_unsupported: UNSUPPORTED,
                    ..self.node("Unique", &path, depth)
                })?;
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
                for member in &keys[..usize::from(key_count)] {
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
                sort.keys = shown;
                plan.push(sort)?;
                self.path(plan, input, depth + 2)
            }
            PathKind::Memoize {
                input,
                keys,
                key_count,
                binary,
                ..
            } => {
                let mut key = StackStr::<512>::new();
                for (at, member) in keys[..usize::from(key_count)].iter().enumerate() {
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
                plan.push(self.keyed(node, query::instrument::Role::Memoize, id))?;
                self.path(plan, input, depth + 1)
            }
            PathKind::NestLoop {
                jointype,
                outer,
                inner,
                clauses,
                ..
            } => {
                let (join_filter, filter) =
                    self.split(&clauses, path.relids, jointype, &ClauseList::EMPTY);
                let node = self
                    .node(join_name("Nested Loop", jointype).as_str(), &path, depth)
                    .with_qual(
                        QualLabel::JoinFilter,
                        self.conditions(&join_filter, None, (false, false))?,
                    )
                    .with_qual(
                        QualLabel::Filter,
                        self.conditions(&filter, None, (false, false))?,
                    );
                plan.push(self.keyed(node, query::instrument::Role::Join, id))?;
                self.path(plan, outer, depth + 1)?;
                self.path(plan, inner, depth + 1)
            }
            PathKind::HashJoin {
                jointype,
                outer,
                inner,
                hash,
                clauses,
                ..
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let (join_filter, filter) = self.split(&clauses, path.relids, jointype, &hash);
                let node = self
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
                plan.push(self.keyed(node, query::instrument::Role::Join, id))?;
                self.path(plan, outer, depth + 1)?;
                let inner_path = *self.model.plan.path(inner);
                let hash_node = PlanNode {
                    startup_cost: inner_path.total,
                    ..self.node("Hash", &inner_path, depth + 1)
                };
                plan.push(self.keyed(hash_node, query::instrument::Role::Hash, id))?;
                self.path(plan, inner, depth + 2)
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
                ..
            } => {
                let outer_relids = self.model.plan.path(outer).relids;
                let (join_filter, filter) = self.split(&clauses, path.relids, jointype, &merge);
                let node = self
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
        self.path(plan, id, depth)
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
    ) -> Result<(), SqlError> {
        let modeled = self.model.rels[rel];
        let scan = modeled.scan(self.storage, self.txid, path, &[], self.arena)?;
        let single =
            QueryScope::resolve_schema(self.storage, modeled.from, self.txid, self.arena, None)?;
        let (mut node, used) = chosen_scan_node(&single, depth, &scan.path);
        let definition = single.defs.stored(0).expect("a plain table");
        let pushed = super::push_outer_mask(
            self.scope,
            scan.param & query::index_paths::joins::BASE_RELIDS,
        );
        let quals = |index: Option<&query::IndexAccessPlan<'_>>| {
            single_relation_quals(
                self.storage,
                self.txid,
                single.names[0],
                definition,
                &[],
                scan.condition,
                &[],
                index,
                false,
                self.arena,
            )
        };
        let printed = (|| {
            let (index_cond, filter) = quals(used.as_ref())?;
            let recheck = match (scan.path.kind, used.as_ref()) {
                (query::index_paths::ScanKind::Bitmap, Some(used)) => {
                    quals(Some(&used.written()))?.0
                }
                _ => None,
            };
            let mut leaves = [(None, None); 64];
            for (at, node) in scan.path.bitmap.iter().enumerate().take(64) {
                if let query::index_paths::BitmapPlanKind::Index { conditions, .. } = node.kind {
                    let leaf = query::IndexAccessPlan::chosen_conditions(query::ChosenConditions {
                        conditions,
                        recheck: &[],
                        absorbed: &[],
                    });
                    leaves[at] = (quals(Some(&leaf))?.0, None);
                }
            }
            Ok::<_, SqlError>((index_cond, filter, recheck, leaves))
        })();
        super::pop_outer_relations(pushed);
        let (index_cond, filter, recheck, leaves) = printed?;
        node = node
            .with_qual(QualLabel::IndexCond, index_cond)
            .with_qual(QualLabel::Filter, filter);
        if recheck.is_some() {
            node = node.with_qual(QualLabel::RecheckCond, recheck);
        }
        node.output_unprintable = true;
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
        let definition = self.scope.defs.stored(table).expect("a plain table");
        // A plan scanning nothing names every relation (`set_rtable_names`
        // with no relations used).
        if self.scanned != 0 && self.scanned & 1 << table == 0 {
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
        let mut kept = [0u8; query::index_paths::joins::MAX_CLAUSES];
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
                && outer & 1 << right.0 != 0
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
                if self.scanned != 0 && self.scanned & 1 << table == 0 {
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
