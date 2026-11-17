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
    block: query::instrument::BlockId,
    arena: &'a Arena,
) -> Result<Plan<'a>, SqlError> {
    let mut plan = Plan::new(arena, model.plan.paths.len() * 3 + 4)?;
    let mut writer = Writer {
        storage,
        txid,
        scope,
        model,
        block,
        arena,
    };
    writer.path(&mut plan, model.plan.top, 0)?;
    Ok(plan)
}

struct Writer<'w, 'm, 's, 'a> {
    storage: &'w crate::storage::Storage,
    txid: u32,
    scope: &'w QueryScope<'s>,
    model: &'w ModeledJoin<'m>,
    block: query::instrument::BlockId,
    arena: &'a Arena,
}

impl Writer<'_, '_, '_, '_> {
    fn node(&self, name: &str, path: &Path, depth: u8) -> PlanNode {
        PlanNode {
            name: StackStr::from_str(name),
            depth,
            startup_cost: path.startup,
            total_cost: path.total,
            rows: path.rows as u64,
            width: path.width as u32,
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
                    self.column(
                        (usize::from(member.rel), usize::from(member.column)),
                        &mut text,
                    );
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
                    self.column(
                        (usize::from(member.rel), usize::from(member.column)),
                        &mut text,
                    );
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
                    );
                }
                let node = self
                    .node("Memoize", &path, depth)
                    .with_qual(QualLabel::CacheKey, Some(key))
                    .with_qual(QualLabel::CacheMode, Some(StackStr::from_str("logical")));
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
                    .with_qual(QualLabel::JoinFilter, self.conditions(&join_filter, None)?)
                    .with_qual(QualLabel::Filter, self.conditions(&filter, None)?);
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
                        self.conditions(&hash, Some(outer_relids))?,
                    )
                    .with_qual(QualLabel::JoinFilter, self.conditions(&join_filter, None)?)
                    .with_qual(QualLabel::Filter, self.conditions(&filter, None)?);
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
                        self.conditions(&merge, Some(outer_relids))?,
                    )
                    .with_qual(QualLabel::JoinFilter, self.conditions(&join_filter, None)?)
                    .with_qual(QualLabel::Filter, self.conditions(&filter, None)?);
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
            node.keys = self.sort_keys(&keys, input.relids);
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
        let pushed = super::push_outer_mask(self.scope, u32::from(scan.param));
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

    /// `relation.column`, as the join prints a column.
    fn column(&self, (table, column): (usize, usize), text: &mut StackStr<512>) {
        let definition = self.scope.defs.stored(table).expect("a plain table");
        let _ = write!(
            text,
            "{}.{}",
            crate::sql::catalog::deparse::printed_name(self.scope.names[table]),
            crate::sql::catalog::deparse::printed_name(definition.columns()[column].name.as_str())
        );
    }

    /// The conditions of `clauses` ANDed, an equality with the column of
    /// `outer` first when given (`get_switched_clauses`), as written
    /// otherwise.
    fn conditions(
        &self,
        clauses: &ClauseList,
        outer: Option<Relids>,
    ) -> Result<Option<StackStr<512>>, SqlError> {
        let ids = clauses.as_slice();
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
            let _ = write!(text, "(");
            self.column(left, &mut text);
            let _ = write!(text, " = ");
            self.column(right, &mut text);
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
        let joined = self
            .arena
            .alloc_slice_with(self.scope.n, |table| {
                crate::sql::catalog::deparse::Joined::new(
                    self.scope.names[table],
                    self.scope.defs.stored(table).expect("a plain table"),
                )
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
        super::deparse_qual(&context, &[expression], self.arena)
    }

    /// A sort's keys: each class's first member among `relids`.
    fn sort_keys(&self, keys: &Keys, relids: Relids) -> PlanKeys {
        let mut shown = PlanKeys {
            label: KeyLabel::Sort,
            ..PlanKeys::NONE
        };
        for &class in keys.as_slice() {
            let members = self.model.plan.classes[usize::from(class)].members();
            let Some(at) = members
                .iter()
                .position(|member| relids & 1 << member.rel != 0)
            else {
                continue;
            };
            let mut text = StackStr::<512>::new();
            self.column(self.model.member(class, at as u8), &mut text);
            for held in [&mut shown.plain, &mut shown.verbose] {
                if !held.is_empty() {
                    let _ = write!(held, "{}", super::KEY_SEPARATOR);
                }
                let _ = write!(held, "{}", text.as_str());
            }
        }
        shown
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
