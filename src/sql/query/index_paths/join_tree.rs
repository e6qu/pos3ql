//! PostgreSQL 18's preparation of a join problem: `reduce_outer_joins`
//! (`prepjointree.c`), `deconstruct_jointree` with `make_outerjoininfo` and
//! the distribution of each qual to the relations or joins that evaluate it
//! (`initsplan.c`), the equivalence classes it builds (`process_equivalence`,
//! `get_eclass_for_sort_expr`, `generate_base_implied_equalities`,
//! `equivclass.c`) and `reconsider_outer_join_clauses`.
//!
//! The caller describes each qual by the relations and columns it reads and
//! its strictness; this module places it as PostgreSQL does, in
//! PostgreSQL's order.
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/prep/prepjointree.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/plan/initsplan.c>
//! <https://github.com/postgres/postgres/blob/REL_18_6/src/backend/optimizer/path/equivclass.c>

use super::joins::Outside;
use super::joins::{
    BASE_RELIDS, CloneKind, EquivalenceClass, MAX_MEMBERS, Member, OJ_SHIFT, Relids, SpecialJoin,
    SpecialKind, Subproblem,
};
use crate::mem::arena::{Arena, ArenaList};

/// Quals one query's preparation holds per relation of `max_join_relations`:
/// two, as a chain of `USING` joins on two columns has.
pub(crate) const QUALS_PER_RELATION: usize = 2;
/// A join tree node's number. A tree of `n` relations has at most `3 n + 2`
/// nodes: the relations, a join per written join, and a `FROM` list per
/// parenthesized join and at the top.
pub(crate) type NodeId = u16;
/// A join domain's number (`JoinDomain`): the query's, one per left or anti
/// join, and three per full join.
type DomainId = u16;
/// Columns a qual's strictness names.
pub(crate) const MAX_QUAL_VARS: usize = 8;
/// Equivalence classes of one query: a pathkey names its class in six
/// bits ([`super::joins::KEY_CLASS`]).
pub(crate) const MAX_CLASSES: usize = super::joins::KEY_CLASS as usize + 1;
/// A qual's number among its query's quals.
pub(crate) type QualId = u16;
/// No qual: a column a class needs.
pub(crate) const NO_QUAL: QualId = QualId::MAX;
/// The constant `true` that stands in for an outer join clause
/// `reconsider_outer_join_clauses` replaced.
pub(crate) const TRUE_QUAL: QualId = QualId::MAX - 1;

/// A column of a base relation: `(relation, column)`.
pub(crate) type Var = (u8, u16);

/// A short list of columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct VarList {
    vars: [Var; MAX_QUAL_VARS],
    count: u8,
}

impl VarList {
    pub(crate) fn as_slice(&self) -> &[Var] {
        &self.vars[..usize::from(self.count)]
    }

    /// Adds `var` once; `None` when the list is full.
    pub(crate) fn add(&mut self, var: Var) -> Option<()> {
        if self.as_slice().contains(&var) {
            return Some(());
        }
        *self.vars.get_mut(usize::from(self.count))? = var;
        self.count += 1;
        Some(())
    }

    fn union(&self, other: &Self) -> Option<Self> {
        let mut all = *self;
        for &var in other.as_slice() {
            all.add(var)?;
        }
        Some(all)
    }
}

/// How a qual compares, for the equivalence machinery and merge joins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// `left = right` of two columns of one type and collation compared by
    /// its btree and hash operator families: mergeable and hashable.
    Columns(Var, Var),
    /// `column = constant` of a constant of the column's type; equal
    /// constants share `constant`. The column may be an expression, such
    /// as a full join's merged `USING` column, `COALESCE(left, right)`,
    /// which `reconsider_full_join_clause` finds in the constant's class.
    Constant { var: Var, constant: u16 },
    /// Any other qual.
    Other,
}

/// A qual as the preparation reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Qual {
    /// The base relations it reads (`pull_varnos` without nulling).
    pub(crate) rels: Relids,
    /// Every column it reads.
    pub(crate) vars: VarList,
    /// `find_nonnullable_rels` and `find_nonnullable_vars`.
    pub(crate) nonnullable: Relids,
    pub(crate) strict_vars: VarList,
    /// `find_forced_null_var`: the column it tests `IS NULL`.
    pub(crate) forced_null: Option<Var>,
    pub(crate) shape: Shape,
}

/// The join kinds of a written join tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeJoin {
    Inner,
    Left,
    Right,
    Full,
    Semi,
    Anti,
}

/// A join tree node: a base relation, a `FROM` list of nodes, or a join.
#[derive(Clone, Copy, Debug)]
pub(crate) enum TreeNode {
    Rel(u8),
    /// `children[first..first + count]`.
    From {
        first: NodeId,
        count: NodeId,
    },
    Join {
        kind: TreeJoin,
        left: NodeId,
        right: NodeId,
        /// Its order among the written outer joins (the join's range
        /// table entry), or `None` for an inner or semi join.
        rtindex: Option<u8>,
    },
}

/// The written join tree and its quals, each attached to the node whose
/// `ON` or `WHERE` holds it.
pub(crate) struct Tree<'a> {
    pub(crate) nodes: &'a mut [TreeNode],
    pub(crate) node_count: usize,
    pub(crate) children: &'a mut [NodeId],
    pub(crate) child_count: usize,
    pub(crate) root: NodeId,
    /// Its quals, up to [`QUALS_PER_RELATION`] per relation of
    /// `max_join_relations`.
    pub(crate) quals: &'a mut [Qual],
    pub(crate) qual_node: &'a mut [NodeId],
    pub(crate) qual_count: usize,
    /// Base relations.
    pub(crate) rels: usize,
    /// `enable_hashagg`, which lets a semi join's inner relation be made
    /// unique by hashing.
    pub(crate) enable_hashagg: bool,
    /// `from_collapse_limit` and `join_collapse_limit`, which split the
    /// join problem (`deconstruct_recurse`).
    pub(crate) from_collapse_limit: i64,
    pub(crate) join_collapse_limit: i64,
    /// The columns each expression member reads
    /// ([`super::joins::Member::expression_id`]).
    pub(crate) expression_vars: [VarList; super::joins::MAX_JOIN_EXPRESSIONS],
    /// The operands of each expression member that is a full join's merged
    /// column, `COALESCE(left, right)`, which a constant equated with an
    /// upper full join's merged column reaches.
    pub(crate) expression_parts: [Option<(Var, Var)>; super::joins::MAX_JOIN_EXPRESSIONS],
}

impl<'a> Tree<'a> {
    /// The tree of a query of `rels` relations, within the preparation's
    /// capacity for `max_relations` (`max_join_relations`), in `arena`.
    pub(crate) fn new(rels: usize, max_relations: usize, arena: &'a Arena) -> Option<Self> {
        let nodes = 3 * rels + 2;
        let quals = QUALS_PER_RELATION * max_relations;
        Some(Self {
            nodes: arena.alloc_slice_with(nodes, |_| TreeNode::Rel(0)).ok()?,
            node_count: 0,
            children: arena.alloc_slice_with(nodes, |_| 0).ok()?,
            child_count: 0,
            root: 0,
            quals: arena
                .alloc_slice_with(quals, |_| Qual {
                    rels: Relids::EMPTY,
                    vars: VarList::default(),
                    nonnullable: Relids::EMPTY,
                    strict_vars: VarList::default(),
                    forced_null: None,
                    shape: Shape::Other,
                })
                .ok()?,
            qual_node: arena.alloc_slice_with(quals, |_| 0).ok()?,
            qual_count: 0,
            rels,
            enable_hashagg: true,
            from_collapse_limit: 8,
            join_collapse_limit: 8,
            expression_vars: [VarList::default(); super::joins::MAX_JOIN_EXPRESSIONS],
            expression_parts: [None; super::joins::MAX_JOIN_EXPRESSIONS],
        })
    }
}

impl Tree<'_> {
    /// Adds a node, returning its number.
    pub(crate) fn node(&mut self, node: TreeNode) -> Option<NodeId> {
        let at = self.node_count;
        *self.nodes.get_mut(at)? = node;
        self.node_count += 1;
        Some(at as NodeId)
    }

    /// A `FROM` list of `items`.
    pub(crate) fn list_of(&mut self, items: &[NodeId]) -> Option<NodeId> {
        let first = self.child_count;
        for &item in items {
            *self.children.get_mut(self.child_count)? = item;
            self.child_count += 1;
        }
        self.node(TreeNode::From {
            first: first as NodeId,
            count: items.len() as NodeId,
        })
    }

    /// Attaches `qual` to `node`.
    pub(crate) fn qual(&mut self, node: NodeId, qual: Qual) -> Option<QualId> {
        let at = self.qual_count;
        *self.quals.get_mut(at)? = qual;
        self.qual_node[at] = node;
        self.qual_count += 1;
        Some(at as QualId)
    }

    fn items(&self, first: NodeId, count: NodeId) -> &[NodeId] {
        &self.children[usize::from(first)..usize::from(first) + usize::from(count)]
    }

    /// The quals attached to `node`, in order.
    fn quals_of(&self, node: NodeId) -> impl Iterator<Item = QualId> + '_ {
        (0..self.qual_count)
            .filter(move |&at| self.qual_node[at] == node)
            .map(|at| at as QualId)
    }

    /// The base relations under `node`.
    fn base_rels(&self, node: NodeId) -> Relids {
        match self.nodes[usize::from(node)] {
            TreeNode::Rel(rel) => Relids::of(rel),
            TreeNode::From { first, count } => self
                .items(first, count)
                .iter()
                .fold(Relids::EMPTY, |relids, &item| relids | self.base_rels(item)),
            TreeNode::Join { left, right, .. } => self.base_rels(left) | self.base_rels(right),
        }
    }

    fn contains_outer(&self, node: NodeId) -> bool {
        match self.nodes[usize::from(node)] {
            TreeNode::Rel(_) => false,
            TreeNode::From { first, count } => self
                .items(first, count)
                .iter()
                .any(|&item| self.contains_outer(item)),
            TreeNode::Join {
                kind, left, right, ..
            } => kind != TreeJoin::Inner || self.contains_outer(left) || self.contains_outer(right),
        }
    }
}

/// Where a qual went (`RestrictInfo` placement).
#[derive(Clone, Copy, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "held by value in arena slices, never boxed; a join qual carries relation sets"
)]
pub(crate) enum Placed {
    /// A restriction of one relation's scan (`baserestrictinfo`).
    Restriction { rel: u8, qual: QualId },
    /// `member = constant` of a class with a constant
    /// (`generate_base_implied_equalities_const`).
    Implied { member: Member, class: u8 },
    /// `left = right` of two columns of one relation a class equates
    /// (`generate_base_implied_equalities_no_const`).
    Equated { left: Member, right: Member },
    /// `member IS NOT NULL`: a removed self-join's equality of one column
    /// with itself (`replace_relid_callback`), of class `class`.
    NotNull { member: Member, class: u8 },
    /// A join clause.
    Join(JoinQual),
}

/// A qual evaluated at a join.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JoinQual {
    pub(crate) qual: QualId,
    /// `clause_relids` and `required_relids`.
    pub(crate) relids: Relids,
    pub(crate) required: Relids,
    pub(crate) pushed_down: bool,
    pub(crate) clone: CloneKind,
    pub(crate) incompatible: Relids,
    /// `outer_relids`: the outer join's preserved side, for its clause.
    pub(crate) outer_relids: Relids,
    /// A mergeable clause's sides and their classes.
    pub(crate) merge: Option<(Member, Member, u8, u8)>,
}

/// The prepared problem.
pub(crate) struct Prepared<'a> {
    arena: &'a Arena,
    pub(crate) specials: &'a mut [SpecialJoin],
    pub(crate) special_count: usize,
    pub(crate) subproblems: ArenaList<'a, Subproblem>,
    pub(crate) subproblem_items: ArenaList<'a, Relids>,
    pub(crate) classes: [EquivalenceClass<'a>; MAX_CLASSES],
    pub(crate) class_count: usize,
    /// Each class's constant, by the qual that equated it.
    pub(crate) class_constant: [Option<QualId>; MAX_CLASSES],
    pub(crate) placed: ArenaList<'a, Placed>,
    /// Each column some join reads (`attr_needed`), with the qual that
    /// reads it there ([`NO_QUAL`] for a class's); a removed join's column
    /// is needed only by its own relation.
    pub(crate) needed: ArenaList<'a, (Var, Relids, QualId)>,
    /// A class equates two different constants in the query's join
    /// domain: the query returns no rows.
    pub(crate) contradictory: bool,
    /// The base relations a class equating two different constants
    /// inside an outer join empties (`get_join_domain_min_rels`): one
    /// relation is dummy, several make the join of them dummy; 0 when no
    /// class does.
    pub(crate) false_relids: Relids,
}

impl<'a> Prepared<'a> {
    pub(crate) fn placed(&self) -> &[Placed] {
        self.placed.borrowed_slice()
    }

    pub(crate) fn classes(&self) -> &[EquivalenceClass<'a>] {
        &self.classes[..self.class_count]
    }

    /// `remove_leftjoinrel_from_query`: drops left join `at`, whose inner
    /// relation (its whole right side) no row above reads, with that
    /// relation's restrictions and the join's clauses; a clause pushed
    /// down from above keeps its place without them.
    pub(crate) fn remove_left_join(&mut self, at: usize) -> Result<(), Outside> {
        let info = self.specials[at];
        let gone = info.min_right | info.oj;
        let join_plus_commute =
            info.min_left | info.min_right | info.oj | info.commute_above_r | info.commute_below_l;
        if self.classes().iter().any(|class| {
            !class.has_const
                && class.count() > 1
                && class.relids() & info.min_right != Relids::EMPTY
        }) {
            return Err("a removable join's relation in an equivalence class");
        }
        let mut kept = 0;
        let mut dropped_quals = ArenaList::new(self.arena);
        let placed = self.placed.as_mut_slice().map_err(|_| ARENA)?;
        for index in 0..placed.len() {
            let mut entry = placed[index];
            let keep = match &mut entry {
                Placed::Restriction { rel, .. } => {
                    Relids::of(*rel) & info.min_right == Relids::EMPTY
                }
                Placed::Implied { member, .. } | Placed::NotNull { member, .. } => {
                    member.relids() & gone == Relids::EMPTY
                }
                Placed::Equated { left, .. } => left.relids() & gone == Relids::EMPTY,
                Placed::Join(join)
                    if join.required & gone == Relids::EMPTY
                        && join.relids & gone == Relids::EMPTY =>
                {
                    true
                }
                Placed::Join(join) => {
                    if join.relids & info.min_right != Relids::EMPTY
                        || !(join.pushed_down
                            || join.required & !join_plus_commute != Relids::EMPTY)
                    {
                        dropped_quals.push(join.qual).map_err(|_| ARENA)?;
                        false
                    } else {
                        join.relids &= !gone;
                        join.required &= !gone;
                        join.outer_relids &= !gone;
                        join.incompatible &= !gone;
                        true
                    }
                }
            };
            if keep {
                placed[kept] = entry;
                kept += 1;
            }
        }
        self.placed.truncate(kept);
        let mut kept = 0;
        let needed = self.needed.as_mut_slice().map_err(|_| ARENA)?;
        for index in 0..needed.len() {
            let (var, relids, qual) = needed[index];
            if Relids::of(var.0) & info.min_right != Relids::EMPTY {
                continue;
            }
            // A column only a dropped clause read stays in its relation's
            // target list (`reltarget` is not rebuilt), needed by no join.
            let relids = if qual != NO_QUAL && dropped_quals.borrowed_slice().contains(&qual) {
                Relids::of(var.0)
            } else {
                relids & !gone
            };
            needed[kept] = (var, relids, qual);
            kept += 1;
        }
        self.needed.truncate(kept);
        for index in at..self.special_count - 1 {
            self.specials[index] = self.specials[index + 1];
        }
        self.special_count -= 1;
        for special in &mut self.specials[..self.special_count] {
            for set in [
                &mut special.min_left,
                &mut special.min_right,
                &mut special.syn_left,
                &mut special.syn_right,
                &mut special.commute_above_l,
                &mut special.commute_above_r,
                &mut special.commute_below_l,
                &mut special.commute_below_r,
            ] {
                *set &= !gone;
            }
        }
        let mut kept = 0;
        let items = self.subproblem_items.as_mut_slice().map_err(|_| ARENA)?;
        for problem in self.subproblems.as_mut_slice().map_err(|_| ARENA)? {
            let first = kept;
            for index in usize::from(problem.first)..usize::from(problem.first + problem.count) {
                let item = items[index] & !gone;
                if !item.is_empty() {
                    items[kept] = item;
                    kept += 1;
                }
            }
            *problem = Subproblem {
                first: first as u16,
                count: (kept - first) as u16,
            };
        }
        self.subproblem_items.truncate(kept);
        Ok(())
    }

    /// `remove_self_join_rel`: relation `removed`, a scan of the table
    /// relation `kept` scans that a unique key joins to it, becomes `kept`.
    /// Each class member of `removed` becomes `kept`'s, a duplicate leaving
    /// its class (`update_eclasses`); the clauses that join only the two
    /// and `removed`'s restrictions leave the placed clauses, and its other
    /// join clauses read `kept`. Returns the clauses `kept` may take as
    /// restrictions, in the order PostgreSQL offers them (`removed`'s join
    /// clauses, its restrictions, then each class's equality of the two
    /// relations' first members, which becomes `IS NOT NULL` of one
    /// column), and the join clauses, for the caller to keep those that
    /// are not redundant (`add_non_redundant_clauses`).
    pub(crate) fn remove_self_join(
        &mut self,
        removed: u8,
        kept: u8,
        arena: &'a Arena,
    ) -> Result<(ArenaList<'a, Placed>, ArenaList<'a, Placed>), Outside> {
        let gone = Relids::of(removed);
        let into = Relids::of(kept);
        let rename_relids = |relids: Relids| {
            if relids & gone == Relids::EMPTY {
                relids
            } else {
                (relids & !gone) | into
            }
        };
        let rename_member = |member: Member| -> Member {
            if member.base_relids() & gone == Relids::EMPTY {
                return member;
            }
            let base = rename_relids(member.base_relids());
            let rel = base.trailing_zeros() as u8;
            Member {
                rel,
                column: member.column,
                nulling: member.nulling,
                reads: base & !Relids::of(rel),
            }
        };
        let mut restrictions = ArenaList::new(arena);
        let mut joins = ArenaList::new(arena);
        // `generate_join_implied_equalities` of the two: each class
        // without a constant, its first member of each relation.
        let mut equalities = ArenaList::new(arena);
        for (at, class) in self.classes().iter().enumerate() {
            if class.has_const {
                continue;
            }
            let first = |relids: Relids| {
                class
                    .members()
                    .iter()
                    .find(|member| member.relids() == relids)
                    .copied()
            };
            if let (Some(outer), Some(inner)) = (first(gone), first(into)) {
                let outer = rename_member(outer);
                equalities
                    .push(if outer == inner {
                        Placed::NotNull {
                            member: inner,
                            class: at as u8,
                        }
                    } else {
                        Placed::Equated {
                            left: outer,
                            right: inner,
                        }
                    })
                    .map_err(|_| ARENA)?;
            }
        }
        let mut held = ArenaList::new(arena);
        let mut own = ArenaList::new(arena);
        for &entry in self.placed.borrowed_slice() {
            match entry {
                Placed::Join(join) if join.required & gone != Relids::EMPTY => {
                    let join = JoinQual {
                        relids: rename_relids(join.relids),
                        required: rename_relids(join.required),
                        incompatible: rename_relids(join.incompatible),
                        outer_relids: rename_relids(join.outer_relids),
                        merge: join.merge.map(|(left, right, one, two)| {
                            (rename_member(left), rename_member(right), one, two)
                        }),
                        ..join
                    };
                    if join.required != into {
                        joins.push(Placed::Join(join)).map_err(|_| ARENA)?;
                        continue;
                    }
                    let restriction = match join.merge {
                        Some((left, right, class, _)) if left == right => Placed::NotNull {
                            member: left,
                            class,
                        },
                        _ => Placed::Restriction {
                            rel: kept,
                            qual: join.qual,
                        },
                    };
                    restrictions.push(restriction).map_err(|_| ARENA)?;
                }
                Placed::Restriction { rel, qual } if rel == removed => {
                    own.push(Placed::Restriction { rel: kept, qual })
                        .map_err(|_| ARENA)?;
                }
                Placed::Implied { member, class } if member.base_relids() == gone => {
                    own.push(Placed::Implied {
                        member: rename_member(member),
                        class,
                    })
                    .map_err(|_| ARENA)?;
                }
                Placed::NotNull { member, class } if member.base_relids() == gone => {
                    own.push(Placed::NotNull {
                        member: rename_member(member),
                        class,
                    })
                    .map_err(|_| ARENA)?;
                }
                Placed::Equated { left, right } if left.rel == removed => {
                    own.push(Placed::Equated {
                        left: rename_member(left),
                        right: rename_member(right),
                    })
                    .map_err(|_| ARENA)?;
                }
                Placed::Implied { member, .. } | Placed::NotNull { member, .. }
                    if member.base_relids() & gone != Relids::EMPTY =>
                {
                    return Err("a removed self-join's expression of several relations");
                }
                entry => held.push(entry).map_err(|_| ARENA)?,
            }
        }
        for &entry in own
            .borrowed_slice()
            .iter()
            .chain(equalities.borrowed_slice())
        {
            restrictions.push(entry).map_err(|_| ARENA)?;
        }
        self.placed = held;
        for at in 0..self.class_count {
            let members = self.classes[at].members();
            if members
                .iter()
                .all(|member| member.base_relids() & gone == Relids::EMPTY)
            {
                continue;
            }
            let mut renamed = ArenaList::new(arena);
            for &member in members {
                let member = rename_member(member);
                if !renamed.borrowed_slice().contains(&member) {
                    renamed.push(member).map_err(|_| ARENA)?;
                }
            }
            self.classes[at].members = renamed.as_slice();
        }
        // `rebuild_*_attr_needed`: a column only its own relation reads is
        // not needed above it.
        for (var, relids, _) in self.needed.as_mut_slice().map_err(|_| ARENA)? {
            if var.0 == removed {
                var.0 = kept;
            }
            *relids = rename_relids(*relids);
            if *relids & !Relids::of(var.0) == Relids::EMPTY {
                *relids = Relids::EMPTY;
            }
        }
        for special in &mut self.specials[..self.special_count] {
            for set in [
                &mut special.min_left,
                &mut special.min_right,
                &mut special.syn_left,
                &mut special.syn_right,
                &mut special.commute_above_l,
                &mut special.commute_above_r,
                &mut special.commute_below_l,
                &mut special.commute_below_r,
            ] {
                *set = rename_relids(*set);
            }
            for member in &mut special.semi_rhs[..usize::from(special.semi_count)] {
                *member = rename_member(*member);
            }
        }
        self.false_relids = rename_relids(self.false_relids);
        // `remove_rel_from_joinlist`.
        let mut kept_items = 0;
        let items = self.subproblem_items.as_mut_slice().map_err(|_| ARENA)?;
        for problem in self.subproblems.as_mut_slice().map_err(|_| ARENA)? {
            let first = kept_items;
            for index in usize::from(problem.first)..usize::from(problem.first + problem.count) {
                let item = items[index];
                if item == gone {
                    continue;
                }
                items[kept_items] = rename_relids(item);
                kept_items += 1;
            }
            *problem = Subproblem {
                first: first as u16,
                count: (kept_items - first) as u16,
            };
        }
        self.subproblem_items.truncate(kept_items);
        Ok((restrictions, joins))
    }

    /// The relations and outer joins that need column `var` above its
    /// scan.
    pub(crate) fn needed_by(&self, var: Var) -> Relids {
        self.needed
            .borrowed_slice()
            .iter()
            .filter(|(held, ..)| *held == var)
            .fold(Relids::EMPTY, |relids, (_, needed, _)| relids | *needed)
    }
}

/// A class during preparation: merged ones forward to the class that
/// absorbed them (`ec_merged`).
/// A class's members during preparation: `len` of `capacity` slots of the
/// preparation's member pool from `first`.
#[derive(Clone, Copy)]
struct BuildMembers {
    first: u32,
    len: u32,
    capacity: u32,
}

#[derive(Clone, Copy)]
struct BuildClass {
    members: BuildMembers,
    has_const: bool,
    /// Its constant: the qual that equated it, the constant's identity
    /// and its join domain (`em_jdomain`).
    constant: Option<(QualId, u16, DomainId)>,
    merged_into: Option<u8>,
    sources: u8,
    /// Whether `reconsider_outer_join_clauses` derived one of its
    /// equalities, which then has no qual of its own.
    derived: bool,
}

/// An outer join's clause waiting for `reconsider_outer_join_clauses`.
#[derive(Clone, Copy)]
struct Pending {
    join: JoinQual,
}

/// The state of one preparation.
struct Prep<'t, 'a> {
    tree: &'t Tree<'t>,
    /// The classes' members, each class's in its own run.
    arena: &'a Arena,
    member_pool: ArenaList<'a, Member>,
    /// Per node of the tree:
    kinds: &'a mut [Option<TreeJoin>],
    /// Left and right children after `reduce_outer_joins` swapped a
    /// right join's.
    sides: &'a mut [(NodeId, NodeId)],
    /// Each outer join node's bit.
    oj: &'a mut [Relids],
    qualscope: &'a mut [Relids],
    inner_join_rels: &'a mut [Relids],
    left_rels: &'a mut [Relids],
    right_rels: &'a mut [Relids],
    nonnullable_rels: &'a mut [Relids],
    /// Each node's special join, by its position in `specials`.
    special_of: &'a mut [Option<u8>],
    /// At most one per node.
    specials: &'a mut [SpecialJoin],
    special_count: usize,
    /// The post-order list of nodes (`item_list`).
    items: &'a mut [NodeId],
    item_count: usize,
    /// Postponed quals of the left joins (`oj_joinclauses`), each with
    /// its join's node, in order; at most one per qual.
    postponed: &'a mut [(NodeId, QualId)],
    postponed_count: usize,
    classes: [BuildClass; MAX_CLASSES],
    class_count: usize,
    placed: ArenaList<'a, Placed>,
    /// At most one per qual.
    sources: &'a mut [(QualId, u8)],
    source_count: usize,
    needed: ArenaList<'a, (Var, Relids, QualId)>,
    left_join: ArenaList<'a, Pending>,
    right_join: ArenaList<'a, Pending>,
    full_join: ArenaList<'a, Pending>,
    /// Each node's join domain (`JoinDomain`): the query's is 0.
    domain: &'a mut [DomainId],
    domain_count: DomainId,
    /// The node each join domain holds the relations of (`jd_relids`):
    /// a left or anti join's nullable side, a full join's side; `None`
    /// for the query's and a full join's quals' domain. At most three
    /// per node.
    domain_root: &'a mut [Option<NodeId>],
    /// The join lists, each a run of `elements`: a relation's number, or
    /// a negative entry naming a nested list by `-(index + 1)`.
    lists: ArenaList<'a, (u32, u16)>,
    elements: ArenaList<'a, i16>,
    /// The elements of the lists being collected, innermost last.
    collecting: ArenaList<'a, i16>,
    contradictory: bool,
    false_relids: Relids,
}

/// A join list element naming nested list `list`.
fn nested(list: usize) -> i16 {
    -(list as i16 + 1)
}

/// The join problems `make_rel_from_joinlist` collects.
struct Subproblems<'a> {
    subproblems: ArenaList<'a, Subproblem>,
    items: ArenaList<'a, Relids>,
}

/// A capacity of the preparation exceeded; the caller reports it as a
/// named error rather than planning the query another way.
pub(crate) const FULL: Outside = "a query beyond the join preparation's capacity";
/// The statement arena exhausted.
pub(crate) const ARENA: Outside = "the statement arena exhausted";

/// Prepares `tree` as `query_planner` does up to the join search, its
/// lists in `arena`.
pub(crate) fn prepare<'a>(tree: &Tree<'_>, arena: &'a Arena) -> Result<Prepared<'a>, Outside> {
    let nodes = tree.node_count;
    let quals = tree.quals.len();
    let slice = |len: usize, fill| arena.alloc_slice_with(len, |_| fill).map_err(|_| ARENA);
    let mut prep = Prep {
        tree,
        arena,
        member_pool: ArenaList::new(arena),
        kinds: arena.alloc_slice_with(nodes, |_| None).map_err(|_| ARENA)?,
        sides: arena
            .alloc_slice_with(nodes, |_| (0, 0))
            .map_err(|_| ARENA)?,
        oj: slice(nodes, Relids::EMPTY)?,
        qualscope: slice(nodes, Relids::EMPTY)?,
        inner_join_rels: slice(nodes, Relids::EMPTY)?,
        left_rels: slice(nodes, Relids::EMPTY)?,
        right_rels: slice(nodes, Relids::EMPTY)?,
        nonnullable_rels: slice(nodes, Relids::EMPTY)?,
        special_of: arena.alloc_slice_with(nodes, |_| None).map_err(|_| ARENA)?,
        specials: arena
            .alloc_slice_with(nodes, |_| SpecialJoin {
                kind: SpecialKind::Left,
                min_left: Relids::EMPTY,
                min_right: Relids::EMPTY,
                syn_left: Relids::EMPTY,
                syn_right: Relids::EMPTY,
                oj: Relids::EMPTY,
                lhs_strict: false,
                commute_above_l: Relids::EMPTY,
                commute_above_r: Relids::EMPTY,
                commute_below_l: Relids::EMPTY,
                commute_below_r: Relids::EMPTY,
                semi_rhs: [Member::plain(0, 0); super::joins::MAX_KEYS],
                semi_count: 0,
                semi_can_btree: false,
                semi_can_hash: false,
            })
            .map_err(|_| ARENA)?,
        special_count: 0,
        items: arena.alloc_slice_with(nodes, |_| 0).map_err(|_| ARENA)?,
        item_count: 0,
        postponed: arena
            .alloc_slice_with(quals, |_| (0, 0))
            .map_err(|_| ARENA)?,
        postponed_count: 0,
        classes: [BuildClass {
            members: BuildMembers {
                first: 0,
                len: 0,
                capacity: 0,
            },
            has_const: false,
            constant: None,
            merged_into: None,
            sources: 0,
            derived: false,
        }; MAX_CLASSES],
        class_count: 0,
        placed: ArenaList::new(arena),
        sources: arena
            .alloc_slice_with(quals, |_| (0, 0))
            .map_err(|_| ARENA)?,
        source_count: 0,
        needed: ArenaList::new(arena),
        left_join: ArenaList::new(arena),
        right_join: ArenaList::new(arena),
        full_join: ArenaList::new(arena),
        domain: arena.alloc_slice_with(nodes, |_| 0).map_err(|_| ARENA)?,
        domain_count: 1,
        domain_root: arena
            .alloc_slice_with(3 * nodes + 1, |_| None)
            .map_err(|_| ARENA)?,
        lists: ArenaList::new(arena),
        elements: ArenaList::new(arena),
        collecting: ArenaList::new(arena),
        contradictory: false,
        false_relids: Relids::EMPTY,
    };
    for (at, node) in tree.nodes[..tree.node_count].iter().enumerate() {
        if let TreeNode::Join {
            kind, left, right, ..
        } = *node
        {
            prep.kinds[at] = Some(kind);
            prep.sides[at] = (left, right);
        }
    }
    prep.reduce_outer_joins(tree.root, Relids::EMPTY, VarList::default())?;
    prep.number_outer_joins()?;
    prep.assign_domains(tree.root, 0);
    let top = prep.deconstruct_recurse(tree.root)?;
    // Each relation's nulling outer joins where a qual is read.
    let nulling = arena
        .alloc_slice_with(tree.rels, |_| Relids::EMPTY)
        .map_err(|_| ARENA)?;
    let base_nulling = arena
        .alloc_slice_with(tree.rels, |_| Relids::EMPTY)
        .map_err(|_| ARENA)?;
    for at in 0..prep.item_count {
        let item = prep.items[at];
        prep.distribute_node(item, nulling)?;
    }
    for at in 0..prep.item_count {
        let item = prep.items[at];
        if prep.postponed[..prep.postponed_count]
            .iter()
            .any(|&(node, _)| node == item)
        {
            prep.distribute_oj_quals(item, base_nulling, nulling)?;
        }
    }
    prep.reconsider_outer_join_clauses()?;
    prep.generate_base_implied_equalities()?;
    // `make_rel_from_joinlist`: each nested list's problem first.
    let mut problems = Subproblems {
        subproblems: ArenaList::new(arena),
        items: ArenaList::new(arena),
    };
    prep.make_rel_from_joinlist(top, &mut problems)?;
    // The classes in their final order.
    let mut map = [u8::MAX; MAX_CLASSES];
    let mut classes = [EquivalenceClass {
        members: &[],
        has_const: false,
    }; MAX_CLASSES];
    let mut class_constant = [None; MAX_CLASSES];
    let mut class_count = 0;
    let pool = prep.member_pool.as_slice();
    for (slot, held) in map.iter_mut().zip(&prep.classes[..prep.class_count]) {
        if held.merged_into.is_none() {
            *slot = class_count as u8;
            let BuildMembers { first, len, .. } = held.members;
            classes[class_count] = EquivalenceClass {
                members: &pool[first as usize..(first + len) as usize],
                has_const: held.has_const,
            };
            class_constant[class_count] = held.constant.map(|(qual, ..)| qual);
            class_count += 1;
        }
    }
    let canonical = |prep: &Prep<'_, '_>, class: u8| map[usize::from(prep.canonical(class))];
    let mut placed = core::mem::replace(&mut prep.placed, ArenaList::new(arena));
    for entry in placed.as_mut_slice().map_err(|_| ARENA)? {
        match entry {
            Placed::Implied { class, .. } | Placed::NotNull { class, .. } => {
                *class = canonical(&prep, *class);
            }
            Placed::Join(join) => {
                if let Some((_, _, left, right)) = &mut join.merge {
                    *left = canonical(&prep, *left);
                    *right = canonical(&prep, *right);
                }
            }
            Placed::Restriction { .. } | Placed::Equated { .. } => {}
        }
    }
    Ok(Prepared {
        arena,
        specials: prep.specials,
        special_count: prep.special_count,
        subproblems: problems.subproblems,
        subproblem_items: problems.items,
        classes,
        class_count,
        class_constant,
        placed,
        needed: prep.needed,
        contradictory: prep.contradictory,
        false_relids: prep.false_relids,
    })
}

/// Removes entry `at` of `list`, keeping the others' order.
fn remove_pending(list: &mut ArenaList<'_, Pending>, at: usize) -> Result<(), Outside> {
    let entries = list.as_mut_slice().map_err(|_| ARENA)?;
    entries.copy_within(at + 1.., at);
    let len = entries.len() - 1;
    list.truncate(len);
    Ok(())
}

impl Prep<'_, '_> {
    fn qual(&self, id: QualId) -> &Qual {
        &self.tree.quals[usize::from(id)]
    }

    /// `reduce_outer_joins_pass2`.
    fn reduce_outer_joins(
        &mut self,
        node: NodeId,
        nonnullable: Relids,
        forced_null: VarList,
    ) -> Result<(), Outside> {
        let tree = self.tree;
        match tree.nodes[usize::from(node)] {
            TreeNode::Rel(_) => Ok(()),
            TreeNode::From { first, count } => {
                let mut pass_nonnullable = nonnullable;
                let mut pass_forced = forced_null;
                for qual in tree.quals_of(node) {
                    let qual = self.qual(qual);
                    pass_nonnullable |= qual.nonnullable;
                    if let Some(var) = qual.forced_null {
                        pass_forced.add(var).ok_or(FULL)?;
                    }
                }
                for &item in tree.items(first, count) {
                    if tree.contains_outer(item) {
                        self.reduce_outer_joins(item, pass_nonnullable, pass_forced)?;
                    }
                }
                Ok(())
            }
            TreeNode::Join { .. } => {
                let at = usize::from(node);
                let (left, right) = self.sides[at];
                let (left_rels, right_rels) = (tree.base_rels(left), tree.base_rels(right));
                let mut kind = self.kinds[at].expect("a join");
                match kind {
                    TreeJoin::Left if nonnullable & right_rels != Relids::EMPTY => {
                        kind = TreeJoin::Inner
                    }
                    TreeJoin::Right if nonnullable & left_rels != Relids::EMPTY => {
                        kind = TreeJoin::Inner
                    }
                    TreeJoin::Full => {
                        kind = match (
                            nonnullable & left_rels != Relids::EMPTY,
                            nonnullable & right_rels != Relids::EMPTY,
                        ) {
                            (true, true) => TreeJoin::Inner,
                            (true, false) => TreeJoin::Left,
                            (false, true) => TreeJoin::Right,
                            (false, false) => TreeJoin::Full,
                        };
                        if matches!(kind, TreeJoin::Left | TreeJoin::Right) {
                            // `report_reduced_full_join` keeps the join's
                            // own number for the remaining side.
                        }
                    }
                    _ => {}
                }
                let (mut left, mut right) = (left, right);
                if kind == TreeJoin::Right {
                    core::mem::swap(&mut left, &mut right);
                    kind = TreeJoin::Left;
                }
                let right_rels = tree.base_rels(right);
                if kind == TreeJoin::Left {
                    let mut strict = VarList::default();
                    for qual in tree.quals_of(node) {
                        strict = strict.union(&self.qual(qual).strict_vars).ok_or(FULL)?;
                    }
                    let overlap = strict
                        .as_slice()
                        .iter()
                        .filter(|var| forced_null.as_slice().contains(var))
                        .fold(Relids::EMPTY, |relids, &(rel, _)| relids | Relids::of(rel));
                    if overlap & right_rels != Relids::EMPTY {
                        kind = TreeJoin::Anti;
                    }
                }
                self.kinds[at] = Some(kind);
                self.sides[at] = (left, right);
                let (left_outer, right_outer) =
                    (tree.contains_outer(left), tree.contains_outer(right));
                if !left_outer && !right_outer {
                    return Ok(());
                }
                let (mut local_nonnullable, mut local_forced) = (Relids::EMPTY, VarList::default());
                if kind != TreeJoin::Full {
                    for qual in tree.quals_of(node) {
                        let qual = self.qual(qual);
                        local_nonnullable |= qual.nonnullable;
                        if let Some(var) = qual.forced_null {
                            local_forced.add(var).ok_or(FULL)?;
                        }
                    }
                    if matches!(kind, TreeJoin::Inner | TreeJoin::Semi) {
                        local_nonnullable |= nonnullable;
                        local_forced = local_forced.union(&forced_null).ok_or(FULL)?;
                    }
                }
                if left_outer {
                    let (pass, forced) = match kind {
                        TreeJoin::Inner | TreeJoin::Semi => (local_nonnullable, local_forced),
                        TreeJoin::Full => (Relids::EMPTY, VarList::default()),
                        _ => (nonnullable, forced_null),
                    };
                    self.reduce_outer_joins(left, pass, forced)?;
                }
                if right_outer {
                    let (pass, forced) = if kind == TreeJoin::Full {
                        (Relids::EMPTY, VarList::default())
                    } else {
                        (local_nonnullable, local_forced)
                    };
                    self.reduce_outer_joins(right, pass, forced)?;
                }
                Ok(())
            }
        }
    }

    /// Each node's join domain: a left or anti join's quals and nullable
    /// side form a domain, a full join's quals one and each side another.
    fn assign_domains(&mut self, node: NodeId, domain: DomainId) {
        let at = usize::from(node);
        self.domain[at] = domain;
        match self.tree.nodes[at] {
            TreeNode::Rel(_) => {}
            TreeNode::From { first, count } => {
                for position in 0..usize::from(count) {
                    let item = self.tree.children[usize::from(first) + position];
                    self.assign_domains(item, domain);
                }
            }
            TreeNode::Join { .. } => {
                let (left, right) = self.sides[at];
                match self.kinds[at] {
                    Some(TreeJoin::Left | TreeJoin::Anti) => {
                        let child = self.next_domain();
                        self.domain_root[usize::from(child)] = Some(right);
                        self.domain[at] = child;
                        self.assign_domains(left, domain);
                        self.assign_domains(right, child);
                    }
                    Some(TreeJoin::Full) => {
                        self.domain[at] = self.next_domain();
                        let left_domain = self.next_domain();
                        self.domain_root[usize::from(left_domain)] = Some(left);
                        self.assign_domains(left, left_domain);
                        let right_domain = self.next_domain();
                        self.domain_root[usize::from(right_domain)] = Some(right);
                        self.assign_domains(right, right_domain);
                    }
                    _ => {
                        self.assign_domains(left, domain);
                        self.assign_domains(right, domain);
                    }
                }
            }
        }
    }

    fn next_domain(&mut self) -> DomainId {
        self.domain_count += 1;
        self.domain_count - 1
    }

    /// Gives each join still outer its bit, in the order of the written
    /// joins' range table entries.
    fn number_outer_joins(&mut self) -> Result<(), Outside> {
        let order = self
            .arena
            .alloc_slice_with(self.tree.node_count, |_| (u8::MAX, 0 as NodeId))
            .map_err(|_| ARENA)?;
        let mut count = 0;
        for (at, node) in self.tree.nodes[..self.tree.node_count].iter().enumerate() {
            if let TreeNode::Join {
                rtindex: Some(rtindex),
                ..
            } = *node
                && matches!(
                    self.kinds[at],
                    Some(TreeJoin::Left | TreeJoin::Full | TreeJoin::Anti)
                )
            {
                order[count] = (rtindex, at as NodeId);
                count += 1;
            }
        }
        order[..count].sort_unstable();
        for (bit, &(_, node)) in order[..count].iter().enumerate() {
            self.oj[usize::from(node)] = Relids::of(OJ_SHIFT + bit as u32);
        }
        Ok(())
    }

    /// Adds the list of the elements collected since `from`, returning its
    /// number, and drops them from the collection.
    fn push_collected(&mut self, from: usize) -> Result<usize, Outside> {
        let at = self.lists.len();
        let collected = &self.collecting.borrowed_slice()[from..];
        let first = self.elements.len();
        for &element in collected {
            self.elements.push(element).map_err(|_| ARENA)?;
        }
        self.lists
            .push((first as u32, collected.len() as u16))
            .map_err(|_| ARENA)?;
        self.collecting.truncate(from);
        Ok(at)
    }

    /// Adds the list of `elements`, returning its number.
    fn push_list(&mut self, elements: &[i16]) -> Result<usize, Outside> {
        let from = self.collecting.len();
        for &element in elements {
            self.collecting.push(element).map_err(|_| ARENA)?;
        }
        self.push_collected(from)
    }

    /// Collects the elements of list `at`.
    fn collect_list(&mut self, at: usize) -> Result<(), Outside> {
        let (first, len) = self.lists.borrowed_slice()[at];
        for index in first as usize..first as usize + usize::from(len) {
            let element = self.elements.borrowed_slice()[index];
            self.collecting.push(element).map_err(|_| ARENA)?;
        }
        Ok(())
    }

    fn list(&self, at: usize) -> &[i16] {
        let (first, len) = self.lists.borrowed_slice()[at];
        &self.elements.borrowed_slice()[first as usize..first as usize + usize::from(len)]
    }

    /// `deconstruct_recurse`: each node's scopes, the post-order item list,
    /// and the node's join list, returned by its number.
    fn deconstruct_recurse(&mut self, node: NodeId) -> Result<usize, Outside> {
        let at = usize::from(node);
        let tree = self.tree;
        let joinlist = match tree.nodes[at] {
            TreeNode::Rel(rel) => {
                self.qualscope[at] = Relids::of(rel);
                self.push_list(&[i16::from(rel)])?
            }
            TreeNode::From { first, count } => {
                let limit = self.tree.from_collapse_limit;
                let from = self.collecting.len();
                let items = tree.items(first, count);
                for (position, &item) in items.iter().enumerate() {
                    let sub = self.deconstruct_recurse(item)?;
                    self.qualscope[at] |= self.qualscope[usize::from(item)];
                    self.inner_join_rels[at] = self.inner_join_rels[usize::from(item)];
                    let remaining = items.len() - position - 1;
                    let members = self.list(sub).len();
                    let len = self.collecting.len() - from;
                    if members <= 1 || (len + members + remaining) as i64 <= limit {
                        self.collect_list(sub)?;
                    } else {
                        self.collecting.push(nested(sub)).map_err(|_| ARENA)?;
                    }
                }
                if items.len() > 1 {
                    self.inner_join_rels[at] = self.qualscope[at];
                }
                self.push_collected(from)?
            }
            TreeNode::Join { .. } => {
                let kind = self.kinds[at].expect("a join");
                let (left, right) = self.sides[at];
                let left_list = self.deconstruct_recurse(left)?;
                let right_list = self.deconstruct_recurse(right)?;
                let (l, r) = (usize::from(left), usize::from(right));
                self.qualscope[at] = self.qualscope[l] | self.qualscope[r] | self.oj[at];
                self.left_rels[at] = self.qualscope[l];
                self.right_rels[at] = self.qualscope[r];
                match kind {
                    TreeJoin::Inner => {
                        self.inner_join_rels[at] = self.qualscope[at];
                        self.nonnullable_rels[at] = Relids::EMPTY;
                    }
                    TreeJoin::Left | TreeJoin::Anti => {
                        self.inner_join_rels[at] =
                            self.inner_join_rels[l] | self.inner_join_rels[r];
                        self.nonnullable_rels[at] = self.qualscope[l];
                    }
                    TreeJoin::Semi => {
                        self.inner_join_rels[at] =
                            self.inner_join_rels[l] | self.inner_join_rels[r];
                        self.nonnullable_rels[at] = Relids::EMPTY;
                    }
                    TreeJoin::Full => {
                        self.inner_join_rels[at] =
                            self.inner_join_rels[l] | self.inner_join_rels[r];
                        self.nonnullable_rels[at] = self.qualscope[at];
                    }
                    TreeJoin::Right => unreachable!("right joins became left joins"),
                }
                let (left_len, right_len) =
                    (self.list(left_list).len(), self.list(right_list).len());
                if kind == TreeJoin::Full {
                    let pair = self.push_list(&[nested(left_list), nested(right_list)])?;
                    self.push_list(&[nested(pair)])?
                } else if (left_len + right_len) as i64 <= self.tree.join_collapse_limit {
                    let from = self.collecting.len();
                    self.collect_list(left_list)?;
                    self.collect_list(right_list)?;
                    self.push_collected(from)?
                } else {
                    // A side of one element joins as itself, not as a
                    // one-element sublist.
                    let part = |list: &[i16], at: usize| {
                        if list.len() == 1 { list[0] } else { nested(at) }
                    };
                    let left_part = part(self.list(left_list), left_list);
                    let right_part = part(self.list(right_list), right_list);
                    self.push_list(&[left_part, right_part])?
                }
            }
        };
        *self.items.get_mut(self.item_count).ok_or(FULL)? = node;
        self.item_count += 1;
        Ok(joinlist)
    }

    /// `make_rel_from_joinlist`: the problems of `list`, nested ones
    /// first; returns the relids the list joins.
    fn make_rel_from_joinlist(
        &self,
        list: usize,
        out: &mut Subproblems<'_>,
    ) -> Result<Relids, Outside> {
        let elements = self.list(list);
        let items = self
            .arena
            .alloc_slice_with(elements.len(), |_| Relids::EMPTY)
            .map_err(|_| ARENA)?;
        for (at, &element) in elements.iter().enumerate() {
            items[at] = if element >= 0 {
                Relids::of(element as u8)
            } else {
                self.make_rel_from_joinlist((-element - 1) as usize, out)?
            };
        }
        if items.len() == 1 {
            return Ok(items[0]);
        }
        let first = out.items.len();
        for &item in items.iter() {
            out.items.push(item).map_err(|_| ARENA)?;
        }
        out.subproblems
            .push(Subproblem {
                first: u16::try_from(first).map_err(|_| FULL)?,
                count: items.len() as u16,
            })
            .map_err(|_| ARENA)?;
        Ok(items
            .iter()
            .fold(Relids::EMPTY, |relids, &item| relids | item))
    }

    /// The outer joins below `node` nulling relation `rel`
    /// (`varnullingrels` of a column read at `node`).
    fn nulling_below(&self, node: NodeId, rel: u8) -> Relids {
        let at = usize::from(node);
        match self.tree.nodes[at] {
            TreeNode::Rel(_) => Relids::EMPTY,
            TreeNode::From { first, count } => self
                .tree
                .items(first, count)
                .iter()
                .fold(Relids::EMPTY, |relids, &item| {
                    relids | self.nulling_below_item(item, rel)
                }),
            TreeNode::Join { .. } => {
                let (left, right) = self.sides[at];
                self.nulling_below_item(left, rel) | self.nulling_below_item(right, rel)
            }
        }
    }

    /// The outer joins at or below `node` nulling relation `rel`.
    fn nulling_below_item(&self, node: NodeId, rel: u8) -> Relids {
        let at = usize::from(node);
        let below = self.nulling_below(node, rel);
        if !matches!(self.tree.nodes[at], TreeNode::Join { .. }) {
            return below;
        }
        let (left, right) = self.sides[at];
        let nullable = match self.kinds[at] {
            Some(TreeJoin::Left | TreeJoin::Anti) => self.tree.base_rels(right),
            Some(TreeJoin::Full) => self.tree.base_rels(left) | self.tree.base_rels(right),
            _ => Relids::EMPTY,
        };
        if nullable & Relids::of(rel) != Relids::EMPTY {
            below | self.oj[at]
        } else {
            below
        }
    }

    /// The nulling of each relation's columns in a qual read at `node`.
    fn nulling_at(&self, node: NodeId, out: &mut [Relids]) {
        for (rel, nulling) in out.iter_mut().enumerate() {
            *nulling = self.nulling_below(node, rel as u8);
        }
    }

    /// `deconstruct_distribute`.
    fn distribute_node(&mut self, node: NodeId, nulling: &mut [Relids]) -> Result<(), Outside> {
        let at = usize::from(node);
        let tree = self.tree;
        match tree.nodes[at] {
            TreeNode::Rel(_) => Ok(()),
            TreeNode::From { .. } => {
                self.nulling_at(node, nulling);
                for qual in tree.quals_of(node) {
                    self.distribute_qual(
                        qual,
                        node,
                        None,
                        self.qualscope[at],
                        None,
                        Relids::EMPTY,
                        Relids::EMPTY,
                        true,
                        CloneKind::None,
                        nulling,
                        false,
                    )?;
                }
                Ok(())
            }
            TreeNode::Join { .. } => {
                let kind = self.kinds[at].expect("a join");
                self.nulling_at(node, nulling);
                let (special, mut ojscope) = if kind == TreeJoin::Inner {
                    (None, None)
                } else {
                    let special = self.make_outerjoininfo(node, kind, nulling)?;
                    let info = self.specials[usize::from(special)];
                    let scope = if kind == TreeJoin::Semi {
                        None
                    } else {
                        Some(info.min_left | info.min_right)
                    };
                    (Some(special), scope)
                };
                let postpone = kind == TreeJoin::Left
                    && special.is_some_and(|id| self.specials[usize::from(id)].lhs_strict);
                if postpone {
                    let info = self.specials[usize::from(special.expect("left"))];
                    ojscope =
                        ojscope.map(|scope| scope | info.commute_below_l | info.commute_below_r);
                }
                for qual in tree.quals_of(node) {
                    self.distribute_qual(
                        qual,
                        node,
                        special,
                        self.qualscope[at],
                        ojscope,
                        self.nonnullable_rels[at],
                        Relids::EMPTY,
                        true,
                        CloneKind::None,
                        nulling,
                        postpone,
                    )?;
                }
                if let Some(special) = special {
                    // `join_info_list` takes the join once its quals are
                    // placed.
                    self.special_of[at] = Some(special);
                }
                Ok(())
            }
        }
    }

    /// `make_outerjoininfo`.
    fn make_outerjoininfo(
        &mut self,
        node: NodeId,
        kind: TreeJoin,
        nulling: &[Relids],
    ) -> Result<u8, Outside> {
        let at = usize::from(node);
        let (left_rels, right_rels) = (self.left_rels[at], self.right_rels[at]);
        let special_kind = match kind {
            TreeJoin::Left => SpecialKind::Left,
            TreeJoin::Full => SpecialKind::Full,
            TreeJoin::Semi => SpecialKind::Semi,
            TreeJoin::Anti => SpecialKind::Anti,
            TreeJoin::Inner | TreeJoin::Right => unreachable!("a special join"),
        };
        let id = self.special_count;
        if id == self.specials.len() || id > usize::from(u8::MAX) {
            return Err(FULL);
        }
        let mut info = SpecialJoin {
            kind: special_kind,
            min_left: left_rels,
            min_right: right_rels,
            syn_left: left_rels,
            syn_right: right_rels,
            oj: self.oj[at],
            lhs_strict: false,
            commute_above_l: Relids::EMPTY,
            commute_above_r: Relids::EMPTY,
            commute_below_l: Relids::EMPTY,
            commute_below_r: Relids::EMPTY,
            semi_rhs: [Member::plain(0, 0); super::joins::MAX_KEYS],
            semi_count: 0,
            semi_can_btree: false,
            semi_can_hash: false,
        };
        if kind == TreeJoin::Semi {
            self.compute_semijoin_info(node, &mut info, nulling);
        }
        if kind == TreeJoin::Full {
            self.specials[id] = info;
            self.special_count += 1;
            return Ok(id as u8);
        }
        // `pull_varnos` and `find_nonnullable_rels` of the join's quals.
        let mut clause_relids = Relids::EMPTY;
        let mut strict_relids = Relids::EMPTY;
        for qual in self.tree.quals_of(node) {
            let qual = self.qual(qual);
            clause_relids |= qual.rels;
            for &(rel, _) in qual.vars.as_slice() {
                clause_relids |= nulling[usize::from(rel)];
            }
            strict_relids |= qual.nonnullable;
            for &(rel, _) in qual.strict_vars.as_slice() {
                strict_relids |= nulling[usize::from(rel)];
            }
        }
        info.lhs_strict = strict_relids & left_rels != Relids::EMPTY;
        let mut min_left = clause_relids & left_rels;
        let mut min_right = (clause_relids | self.inner_join_rels[at]) & right_rels;
        let (mut below_l, mut below_r) = (Relids::EMPTY, Relids::EMPTY);
        // `join_info_list`: the joins below, placed before this one.
        for other in &self.specials[..self.special_count] {
            if other.kind == SpecialKind::Full {
                if left_rels & (other.syn_left | other.syn_right) != Relids::EMPTY {
                    min_left |= other.syn_left | other.syn_right | other.oj;
                }
                if right_rels & (other.syn_left | other.syn_right) != Relids::EMPTY {
                    min_right |= other.syn_left | other.syn_right | other.oj;
                }
                continue;
            }
            if left_rels & other.syn_right != Relids::EMPTY {
                if clause_relids & other.syn_right != Relids::EMPTY
                    && (matches!(kind, TreeJoin::Semi | TreeJoin::Anti)
                        || strict_relids & other.min_right == Relids::EMPTY)
                {
                    min_left |= other.syn_left | other.syn_right | other.oj;
                } else if kind == TreeJoin::Left
                    && other.kind == SpecialKind::Left
                    && strict_relids & other.min_right != Relids::EMPTY
                    && clause_relids & other.syn_left == Relids::EMPTY
                {
                    min_left &= !other.oj;
                    below_l |= other.oj;
                }
            }
            if right_rels & other.syn_right != Relids::EMPTY {
                if clause_relids & other.syn_right != Relids::EMPTY
                    || clause_relids & other.min_left == Relids::EMPTY
                    || matches!(kind, TreeJoin::Semi | TreeJoin::Anti)
                    || matches!(other.kind, SpecialKind::Semi | SpecialKind::Anti)
                    || !other.lhs_strict
                {
                    min_right |= other.syn_left | other.syn_right | other.oj;
                } else if kind == TreeJoin::Left
                    && other.kind == SpecialKind::Left
                    && other.lhs_strict
                {
                    min_right &= !other.oj;
                    below_r |= other.oj;
                }
            }
        }
        if min_left.is_empty() {
            min_left = left_rels;
        }
        if min_right.is_empty() {
            min_right = right_rels;
        }
        info.min_left = min_left;
        info.min_right = min_right;
        below_l &= !min_left;
        below_r &= !min_right;
        if below_l != Relids::EMPTY || below_r != Relids::EMPTY {
            info.commute_below_l = below_l;
            info.commute_below_r = below_r;
            let oj = info.oj;
            for other in &mut self.specials[..self.special_count] {
                if other.oj & below_l != Relids::EMPTY {
                    other.commute_above_l |= oj;
                } else if other.oj & below_r != Relids::EMPTY {
                    other.commute_above_r |= oj;
                }
            }
        }
        self.specials[id] = info;
        self.special_count += 1;
        Ok(id as u8)
    }

    /// `compute_semijoin_info`: a semi join's inner columns compared by
    /// equalities with the outer relations, when every condition reading
    /// both sides is one.
    fn compute_semijoin_info(&self, node: NodeId, info: &mut SpecialJoin, nulling: &[Relids]) {
        let right = info.syn_right;
        let mut count = 0usize;
        for qual in self.tree.quals_of(node) {
            let qual = self.qual(qual);
            let mut all = qual.rels;
            for &(rel, _) in qual.vars.as_slice() {
                all |= nulling[usize::from(rel)];
            }
            if all & right == Relids::EMPTY || all & !right == Relids::EMPTY {
                continue;
            }
            let Shape::Columns(left, other) = qual.shape else {
                return;
            };
            let inside = |var: Var| right & Relids::of(var.0) != Relids::EMPTY;
            let member = match (inside(left), inside(other)) {
                (false, true) => other,
                (true, false) => left,
                _ => return,
            };
            if count == info.semi_rhs.len() {
                return;
            }
            info.semi_rhs[count] = Member::plain(member.0, member.1);
            count += 1;
        }
        if count == 0 {
            return;
        }
        info.semi_count = count as u8;
        info.semi_can_btree = true;
        info.semi_can_hash = self.tree.enable_hashagg;
    }

    /// `deconstruct_distribute_oj_quals`.
    fn distribute_oj_quals(
        &mut self,
        node: NodeId,
        base_nulling: &mut [Relids],
        nulling: &mut [Relids],
    ) -> Result<(), Outside> {
        let at = usize::from(node);
        let special = self.special_of[at].expect("a left join");
        let info = self.specials[usize::from(special)];
        let qualscope = info.syn_left | info.syn_right | info.oj;
        let ojscope = info.min_left | info.min_right;
        let nonnullable = info.syn_left;
        let count = self.postponed[..self.postponed_count]
            .iter()
            .filter(|&&(held, _)| held == node)
            .count();
        let quals = self
            .arena
            .alloc_slice_with(count, |_| 0)
            .map_err(|_| ARENA)?;
        for (slot, &(_, qual)) in quals.iter_mut().zip(
            self.postponed[..self.postponed_count]
                .iter()
                .filter(|&&(held, _)| held == node),
        ) {
            *slot = qual;
        }
        let quals = &*quals;
        self.nulling_at(node, base_nulling);
        if info.commute_above_r != Relids::EMPTY || info.commute_below_l != Relids::EMPTY {
            let (above, below) = (info.commute_above_r, info.commute_below_l);
            // The quals without the nulling of the joins it commutes with
            // from below.
            nulling.copy_from_slice(base_nulling);
            for relids in nulling.iter_mut() {
                *relids &= !below;
            }
            let mut incompatible = below | above | info.oj;
            let mut so_far = Relids::EMPTY;
            for position in 0..self.item_count {
                let item = self.items[position];
                let Some(other_id) = self.special_of[usize::from(item)] else {
                    continue;
                };
                let other = self.specials[usize::from(other_id)];
                let (below_join, above_join) = if other.oj & below != Relids::EMPTY {
                    (true, false)
                } else if other_id == special {
                    (false, false)
                } else if other.oj & above != Relids::EMPTY {
                    (false, true)
                } else {
                    continue;
                };
                if above_join {
                    for (rel, relids) in nulling.iter_mut().enumerate().take(self.tree.rels) {
                        if info.syn_left & Relids::of(rel) != Relids::EMPTY {
                            *relids |= other.oj;
                        }
                    }
                    incompatible &= !other.oj;
                }
                let mut this_qualscope = qualscope | so_far;
                let mut this_ojscope = ojscope | so_far;
                if above_join {
                    this_qualscope |= other.oj;
                    this_ojscope |= other.oj;
                    this_ojscope &= !info.oj;
                }
                let allow_equivalence = so_far == Relids::EMPTY;
                let clone = if allow_equivalence {
                    CloneKind::Has
                } else {
                    CloneKind::Is
                };
                for &qual in quals {
                    self.distribute_qual(
                        qual,
                        node,
                        Some(special),
                        this_qualscope,
                        Some(this_ojscope),
                        nonnullable,
                        incompatible,
                        allow_equivalence,
                        clone,
                        nulling,
                        false,
                    )?;
                }
                if below_join {
                    for (rel, relids) in nulling.iter_mut().enumerate().take(self.tree.rels) {
                        if other.syn_right & Relids::of(rel) != Relids::EMPTY {
                            *relids |= other.oj;
                        }
                    }
                    incompatible &= !other.oj;
                }
                so_far |= other.oj;
            }
        } else {
            for &qual in quals {
                self.distribute_qual(
                    qual,
                    node,
                    Some(special),
                    qualscope,
                    Some(ojscope),
                    nonnullable,
                    Relids::EMPTY,
                    true,
                    CloneKind::None,
                    base_nulling,
                    false,
                )?;
            }
        }
        Ok(())
    }

    /// `distribute_qual_to_rels`.
    #[allow(clippy::too_many_arguments)]
    fn distribute_qual(
        &mut self,
        qual_id: QualId,
        node: NodeId,
        special: Option<u8>,
        qualscope: Relids,
        ojscope: Option<Relids>,
        nonnullable: Relids,
        incompatible: Relids,
        allow_equivalence: bool,
        clone: CloneKind,
        nulling: &[Relids],
        postpone: bool,
    ) -> Result<(), Outside> {
        let qual = *self.qual(qual_id);
        let mut relids = qual.rels;
        for &(rel, _) in qual.vars.as_slice() {
            relids |= nulling[usize::from(rel)];
        }
        let clause_relids = relids;
        if relids == Relids::EMPTY {
            // A constant qual: PostgreSQL places it as a gating qual.
            let _ = qualscope;
            return Err("a join condition reading no relation");
        }
        if let Some(scope) = ojscope
            && relids & !scope != Relids::EMPTY
        {
            return Err("a join condition reading relations outside its join");
        }
        let (pushed_down, maybe_equivalence, maybe_outer_join);
        if relids & nonnullable != Relids::EMPTY {
            if postpone {
                *self.postponed.get_mut(self.postponed_count).ok_or(FULL)? = (node, qual_id);
                self.postponed_count += 1;
                return Ok(());
            }
            pushed_down = false;
            maybe_equivalence = false;
            maybe_outer_join = true;
            relids = ojscope.expect("an outer join's scope");
        } else {
            pushed_down = true;
            if self.redundant_nullability(&qual, nulling) {
                return Ok(());
            }
            maybe_equivalence = allow_equivalence;
            maybe_outer_join = false;
        }
        // `add_vars_to_targetlist` for a clause above one relation.
        if relids.count_ones() > 1 {
            let where_needed = if clone == CloneKind::Is {
                relids & BASE_RELIDS
            } else {
                relids
            };
            for &var in qual.vars.as_slice() {
                self.note_needed(var, where_needed, qual_id)?;
            }
        }
        let mut join = JoinQual {
            qual: qual_id,
            relids: clause_relids,
            required: relids,
            pushed_down,
            clone,
            incompatible,
            outer_relids: nonnullable,
            merge: None,
        };
        match qual.shape {
            Shape::Columns(left, right) => {
                let left_member = self.var_member(left, nulling);
                let right_member = self.var_member(right, nulling);
                // `can_join`: each side reads relations the other does not.
                let can_join = left_member.relids() & right_member.relids() == Relids::EMPTY;
                if maybe_equivalence {
                    let domain = self.domain[usize::from(node)];
                    if self.process_equivalence(
                        qual_id,
                        left_member,
                        Some(right_member),
                        None,
                        domain,
                    )? {
                        return Ok(());
                    }
                    join.merge = Some(self.mergeclause_classes(left_member, right_member)?);
                } else if maybe_outer_join && can_join {
                    join.merge = Some(self.mergeclause_classes(left_member, right_member)?);
                    let left_relids = left_member.relids();
                    let right_relids = right_member.relids();
                    let pending = Pending { join };
                    if left_relids & !nonnullable == Relids::EMPTY
                        && right_relids & nonnullable == Relids::EMPTY
                    {
                        self.left_join.push(pending).map_err(|_| ARENA)?;
                        return Ok(());
                    }
                    if right_relids & !nonnullable == Relids::EMPTY
                        && left_relids & nonnullable == Relids::EMPTY
                    {
                        self.right_join.push(pending).map_err(|_| ARENA)?;
                        return Ok(());
                    }
                    if special
                        .is_some_and(|id| self.specials[usize::from(id)].kind == SpecialKind::Full)
                    {
                        self.full_join.push(pending).map_err(|_| ARENA)?;
                        return Ok(());
                    }
                } else {
                    join.merge = Some(self.mergeclause_classes(left_member, right_member)?);
                }
            }
            Shape::Constant { var, constant } => {
                let member = self.var_member(var, nulling);
                if maybe_equivalence {
                    let domain = self.domain[usize::from(node)];
                    if self.process_equivalence(qual_id, member, None, Some(constant), domain)? {
                        return Ok(());
                    }
                }
                if member.nulling != Relids::EMPTY {
                    return Err("a constant equality of a nulled column");
                }
            }
            Shape::Other => {}
        }
        self.distribute_restrictinfo(join)
    }

    /// `distribute_restrictinfo_to_rels`.
    fn distribute_restrictinfo(&mut self, join: JoinQual) -> Result<(), Outside> {
        let placed =
            if join.required.count_ones() == 1 && join.required & BASE_RELIDS != Relids::EMPTY {
                Placed::Restriction {
                    rel: join.required.trailing_zeros() as u8,
                    qual: join.qual,
                }
            } else {
                Placed::Join(join)
            };
        self.place(placed)
    }

    fn place(&mut self, placed: Placed) -> Result<(), Outside> {
        self.placed.push(placed).map_err(|_| ARENA)
    }

    fn note_needed(&mut self, var: Var, relids: Relids, qual: QualId) -> Result<(), Outside> {
        self.needed.push((var, relids, qual)).map_err(|_| ARENA)
    }

    /// `check_redundant_nullability_qual`: `column IS NULL` of a column an
    /// anti join below nulls is true.
    fn redundant_nullability(&self, qual: &Qual, nulling: &[Relids]) -> bool {
        let Some((rel, _)) = qual.forced_null else {
            return false;
        };
        let nulled = nulling[usize::from(rel)];
        nulled != Relids::EMPTY
            && self.specials[..self.special_count].iter().any(|special| {
                special.kind == SpecialKind::Anti && special.oj & nulled != Relids::EMPTY
            })
    }

    fn canonical(&self, mut class: u8) -> u8 {
        while let Some(next) = self.classes[usize::from(class)].merged_into {
            class = next;
        }
        class
    }

    /// The members of class `class`.
    fn members_of(&self, class: u8) -> &[Member] {
        let BuildMembers { first, len, .. } = self.classes[usize::from(class)].members;
        &self.member_pool.borrowed_slice()[first as usize..(first + len) as usize]
    }

    /// `ec_relids` of class `class`.
    fn class_relids(&self, class: u8) -> Relids {
        self.members_of(class)
            .iter()
            .fold(Relids::EMPTY, |relids, member| relids | member.relids())
    }

    /// The live class holding `member`.
    fn class_of(&self, member: Member) -> Option<u8> {
        (0..self.class_count)
            .find(|&at| {
                self.classes[at].merged_into.is_none()
                    && self.members_of(at as u8).contains(&member)
            })
            .map(|at| at as u8)
    }

    /// The live class holding constant `constant` of join domain
    /// `domain`.
    fn class_of_constant(&self, constant: u16, domain: DomainId) -> Option<u8> {
        (0..self.class_count)
            .find(|&at| {
                self.classes[at].merged_into.is_none()
                    && self.classes[at]
                        .constant
                        .is_some_and(|(_, held, held_domain)| {
                            held == constant && held_domain == domain
                        })
            })
            .map(|at| at as u8)
    }

    fn new_class(&mut self) -> Result<u8, Outside> {
        let at = self.class_count;
        if at == MAX_CLASSES {
            return Err(FULL);
        }
        self.classes[at] = BuildClass {
            members: BuildMembers {
                first: 0,
                len: 0,
                capacity: 0,
            },
            has_const: false,
            constant: None,
            merged_into: None,
            sources: 0,
            derived: false,
        };
        self.class_count += 1;
        Ok(at as u8)
    }

    /// Appends `member` to class `class`, moving the class's run to the
    /// pool's end with twice the room when it is full.
    fn add_member(&mut self, class: u8, member: Member) -> Result<(), Outside> {
        let mut held = self.classes[usize::from(class)].members;
        if held.len as usize == MAX_MEMBERS {
            return Err(FULL);
        }
        if held.len == held.capacity {
            let first = u32::try_from(self.member_pool.len()).map_err(|_| ARENA)?;
            let capacity = (held.capacity * 2).max(4);
            for at in 0..capacity {
                let moved = if at < held.len {
                    self.member_pool.borrowed_slice()[(held.first + at) as usize]
                } else {
                    member
                };
                self.member_pool.push(moved).map_err(|_| ARENA)?;
            }
            held.first = first;
            held.capacity = capacity;
        }
        self.member_pool.as_mut_slice().map_err(|_| ARENA)?[(held.first + held.len) as usize] =
            member;
        held.len += 1;
        self.classes[usize::from(class)].members = held;
        Ok(())
    }

    /// `process_equivalence` of `left = right` or `left = constant`, read
    /// in join domain `domain`.
    fn process_equivalence(
        &mut self,
        qual: QualId,
        left: Member,
        right: Option<Member>,
        constant: Option<u16>,
        domain: DomainId,
    ) -> Result<bool, Outside> {
        if right == Some(left) {
            return Err("an equality of a column with itself");
        }
        let one = self.class_of(left);
        let two = match (right, constant) {
            (Some(right), _) => self.class_of(right),
            (None, Some(constant)) => self.class_of_constant(constant, domain),
            (None, None) => unreachable!("an equality"),
        };
        let class = match (one, two) {
            (Some(one), Some(two)) if one == two => one,
            (Some(one), Some(two)) => {
                let moved = self.classes[usize::from(two)];
                for at in 0..moved.members.len {
                    let member =
                        self.member_pool.borrowed_slice()[(moved.members.first + at) as usize];
                    self.add_member(one, member)?;
                }
                let kept = self.classes[usize::from(one)];
                if kept.has_const
                    && moved.has_const
                    && kept.constant.map(|(_, constant, _)| constant)
                        != moved.constant.map(|(_, constant, _)| constant)
                {
                    self.contradiction(domain)?;
                }
                let held = &mut self.classes[usize::from(one)];
                held.has_const |= moved.has_const;
                if held.constant.is_none() {
                    held.constant = moved.constant;
                }
                held.sources += moved.sources;
                held.derived |= moved.derived;
                self.classes[usize::from(two)].merged_into = Some(one);
                one
            }
            (Some(one), None) => {
                match right {
                    Some(right) => self.add_member(one, right)?,
                    None => {
                        let held = &self.classes[usize::from(one)];
                        if held.has_const {
                            // `generate_base_implied_equalities_const`
                            // equates the two constants: a constant
                            // false.
                            if held.constant.map(|(_, held, _)| held) != constant {
                                self.contradiction(domain)?;
                            }
                        } else {
                            let held = &mut self.classes[usize::from(one)];
                            held.has_const = true;
                            held.constant = Some((qual, constant.expect("a constant"), domain));
                        }
                    }
                }
                one
            }
            (None, Some(two)) => {
                self.add_member(two, left)?;
                two
            }
            (None, None) => {
                let class = self.new_class()?;
                self.add_member(class, left)?;
                match right {
                    Some(right) => self.add_member(class, right)?,
                    None => {
                        let held = &mut self.classes[usize::from(class)];
                        held.has_const = true;
                        held.constant = Some((qual, constant.expect("a constant"), domain));
                    }
                }
                class
            }
        };
        self.classes[usize::from(class)].sources += 1;
        *self.sources.get_mut(self.source_count).ok_or(FULL)? = (qual, class);
        self.source_count += 1;
        Ok(true)
    }

    /// A class equating two different constants: their constant false
    /// equality (`generate_base_implied_equalities_const`) empties the
    /// query when it holds in the query's own join domain, whose join
    /// then is a dummy relation. Inside an outer join it is a gating
    /// condition of the domain's relations apart from the nullable sides
    /// of the left joins within (`get_join_domain_min_rels`).
    fn contradiction(&mut self, domain: DomainId) -> Result<(), Outside> {
        if domain == 0 {
            self.contradictory = true;
            return Ok(());
        }
        let Some(root) = self.domain_root[usize::from(domain)] else {
            return Err("an equivalence class of two constants in a full join's condition");
        };
        let relids = self.min_rels(root)?;
        if relids == Relids::EMPTY
            || (self.false_relids != Relids::EMPTY && self.false_relids != relids)
        {
            return Err("equivalence classes of two constants inside different outer joins");
        }
        self.false_relids = relids;
        Ok(())
    }

    /// The base relations under `node` apart from the nullable sides of
    /// the left joins there.
    fn min_rels(&self, node: NodeId) -> Result<Relids, Outside> {
        let at = usize::from(node);
        match self.tree.nodes[at] {
            TreeNode::Rel(table) => Ok(Relids::of(table)),
            TreeNode::From { first, count } => {
                let mut relids = Relids::EMPTY;
                for position in 0..usize::from(count) {
                    relids |= self.min_rels(self.tree.children[usize::from(first) + position])?;
                }
                Ok(relids)
            }
            TreeNode::Join { .. } => {
                let (left, right) = self.sides[at];
                match self.kinds[at] {
                    Some(TreeJoin::Left) => self.min_rels(left),
                    Some(TreeJoin::Inner) | None => {
                        Ok(self.min_rels(left)? | self.min_rels(right)?)
                    }
                    _ => Err("an equivalence class of two constants above a full or anti join"),
                }
            }
        }
    }

    /// `initialize_mergeclause_eclasses`: each side's class, a new
    /// one-member class when no class holds it
    /// (`get_eclass_for_sort_expr`).
    fn mergeclause_classes(
        &mut self,
        left: Member,
        right: Member,
    ) -> Result<(Member, Member, u8, u8), Outside> {
        let class_for = |prep: &mut Self, member: Member| -> Result<u8, Outside> {
            if let Some(class) = prep.class_of(member) {
                return Ok(class);
            }
            let class = prep.new_class()?;
            prep.add_member(class, member)?;
            Ok(class)
        };
        let left_class = class_for(self, left)?;
        let right_class = class_for(self, right)?;
        Ok((left, right, left_class, right_class))
    }

    /// `reconsider_outer_join_clauses`: a left or right join's clause whose
    /// outer column a class equates with constants equates its inner column
    /// with each of them inside the join's inner side
    /// (`reconsider_outer_join_clause`), and gives way to a constant `true`
    /// clause; repeated until nothing changes, then the remaining clauses go
    /// back to their relations, left joins' first. A full join's clause of
    /// such a column (`reconsider_full_join_clause`) is outside the model.
    fn reconsider_outer_join_clauses(&mut self) -> Result<(), Outside> {
        loop {
            let mut found = false;
            for right in [false, true] {
                let mut at = 0;
                loop {
                    let list = if right {
                        &self.right_join
                    } else {
                        &self.left_join
                    };
                    let Some(&pending) = list.borrowed_slice().get(at) else {
                        break;
                    };
                    let join = pending.join;
                    if !self.reconsider_outer_join_clause(join, !right)? {
                        at += 1;
                        continue;
                    }
                    found = true;
                    let list = if right {
                        &mut self.right_join
                    } else {
                        &mut self.left_join
                    };
                    remove_pending(list, at)?;
                    self.distribute_restrictinfo(JoinQual {
                        qual: TRUE_QUAL,
                        relids: Relids::EMPTY,
                        merge: None,
                        ..join
                    })?;
                }
            }
            let mut at = 0;
            while let Some(&pending) = self.full_join.borrowed_slice().get(at) {
                let join = pending.join;
                if !self.reconsider_full_join_clause(join)? {
                    at += 1;
                    continue;
                }
                found = true;
                remove_pending(&mut self.full_join, at)?;
                self.distribute_restrictinfo(JoinQual {
                    qual: TRUE_QUAL,
                    relids: Relids::EMPTY,
                    merge: None,
                    ..join
                })?;
            }
            if !found {
                break;
            }
        }
        let constant = |prep: &Self, member: Member| {
            prep.class_of(member)
                .is_some_and(|class| prep.classes[usize::from(class)].has_const)
        };
        for pending in self.full_join.borrowed_slice() {
            let (left, right, ..) = pending.join.merge.expect("mergeable");
            if constant(self, left) || constant(self, right) {
                return Err("a full join clause of a column equated with a constant");
            }
        }
        for which in 0..3 {
            let mut at = 0;
            loop {
                let list = match which {
                    0 => &self.left_join,
                    1 => &self.right_join,
                    _ => &self.full_join,
                };
                let Some(&pending) = list.borrowed_slice().get(at) else {
                    break;
                };
                self.distribute_restrictinfo(pending.join)?;
                at += 1;
            }
        }
        Ok(())
    }

    /// `reconsider_full_join_clause` of `join`: a class with a constant
    /// holding `COALESCE(left, right)` of the clause's sides equates each
    /// side with the constant inside the join domain of the join's side,
    /// and the `COALESCE` member leaves its class, so no filter tests it. A
    /// side that is a lower full join's merged column joins a class with
    /// the constant in turn, which that join's clause then finds.
    fn reconsider_full_join_clause(&mut self, join: JoinQual) -> Result<bool, Outside> {
        let (left, right, ..) = join.merge.expect("mergeable");
        let parts = Some(((left.rel, left.column), (right.rel, right.column)));
        let mut found = None;
        for class in 0..self.class_count {
            let held = self.classes[class];
            if held.merged_into.is_some() || !held.has_const {
                continue;
            }
            if self.members_of(class as u8).iter().any(|member| {
                member
                    .expression_id()
                    .is_some_and(|id| self.tree.expression_parts[id] == parts)
            }) {
                found = Some(class);
                break;
            }
        }
        let Some(class) = found else {
            return Ok(false);
        };
        let (qual, constant, _) = self.classes[class].constant.expect("a constant");
        // The full join whose sides the clause's operands read; the clause
        // may name the join's right side first.
        let within = |member: Member, rels: Relids| member.base_relids() & !rels == Relids::EMPTY;
        let Some((node, written_order)) = (0..self.tree.node_count).find_map(|node| {
            if self.kinds[node] != Some(TreeJoin::Full) {
                return None;
            }
            let (left_rels, right_rels) = (self.left_rels[node], self.right_rels[node]);
            if within(left, left_rels) && within(right, right_rels) {
                Some((node, true))
            } else if within(left, right_rels) && within(right, left_rels) {
                Some((node, false))
            } else {
                None
            }
        }) else {
            return Err("a full join clause outside its full join");
        };
        let (left_node, right_node) = self.sides[node];
        let (left_child, right_child) = if written_order {
            (left_node, right_node)
        } else {
            (right_node, left_node)
        };
        for (side, child) in [(left, left_child), (right, right_child)] {
            if side.expression_id().is_none() && side.nulling != Relids::EMPTY {
                return Err("a full join clause of a nulled column equated with a constant");
            }
            let domain = self.domain[usize::from(child)];
            self.process_equivalence(qual, side, None, Some(constant), domain)?;
            let derived = self.class_of(side).expect("just equated");
            self.classes[usize::from(derived)].derived = true;
        }
        // `list_delete_nth_cell` of the `COALESCE` member; the class may
        // have moved its members since.
        let class = self.canonical(class as u8);
        let members = self.classes[usize::from(class)].members;
        let pool = self.member_pool.as_mut_slice().map_err(|_| ARENA)?;
        let run = &mut pool[members.first as usize..(members.first + members.len) as usize];
        let Some(at) = run.iter().position(|member| {
            member
                .expression_id()
                .is_some_and(|id| self.tree.expression_parts[id] == parts)
        }) else {
            return Err("a full join's merged column left its class");
        };
        run.copy_within(at + 1.., at);
        self.classes[usize::from(class)].members.len -= 1;
        Ok(true)
    }

    /// `reconsider_outer_join_clause` of `join`, whose outer column is its
    /// left one when `outer_on_left`: true when a class equates that column
    /// with a constant, its inner column then equated with that constant in
    /// the join domain of its relation.
    fn reconsider_outer_join_clause(
        &mut self,
        join: JoinQual,
        outer_on_left: bool,
    ) -> Result<bool, Outside> {
        let (left, right, ..) = join.merge.expect("mergeable");
        let (outer, inner) = if outer_on_left {
            (left, right)
        } else {
            (right, left)
        };
        let Some(class) = self.class_of(outer) else {
            return Ok(false);
        };
        let held = self.classes[usize::from(class)];
        let Some((qual, constant, _)) = held.constant.filter(|_| held.has_const) else {
            return Ok(false);
        };
        if inner.nulling != Relids::EMPTY {
            return Err("an outer join clause of a nulled column equated with a constant");
        }
        if inner.reads != Relids::EMPTY {
            return Err(
                "an outer join clause's inner expression of several relations equated with a constant",
            );
        }
        let Some(node) = self.tree.nodes[..self.tree.node_count]
            .iter()
            .position(|node| matches!(node, TreeNode::Rel(rel) if *rel == inner.rel))
        else {
            return Ok(false);
        };
        let domain = self.domain[node];
        self.process_equivalence(qual, inner, None, Some(constant), domain)?;
        let derived = self.class_of(inner).expect("just equated");
        self.classes[usize::from(derived)].derived = true;
        Ok(true)
    }

    /// The class member `var` is where `nulling` gives each relation's
    /// nulling outer joins: an expression of several relations reads them
    /// all, nulled by any of theirs.
    fn var_member(&self, var: Var, nulling: &[Relids]) -> Member {
        let mut member = Member::plain(var.0, var.1);
        let mut relids: Relids = Relids::of(var.0);
        if let Some(id) = member.expression_id() {
            for &(rel, _) in self.tree.expression_vars[id].as_slice() {
                relids |= Relids::of(rel);
            }
        }
        member.reads = relids & !Relids::of(var.0);
        for (rel, nulled) in nulling.iter().enumerate() {
            if relids & Relids::of(rel) != Relids::EMPTY {
                member.nulling |= *nulled;
            }
        }
        member
    }

    /// `generate_base_implied_equalities`: each member of a class with a
    /// constant restricted to it; a class of members of other relations
    /// keeps its members' columns at each of its relations.
    fn generate_base_implied_equalities(&mut self) -> Result<(), Outside> {
        let previous = self
            .arena
            .alloc_slice_with(self.tree.rels, |_| None::<Member>)
            .map_err(|_| ARENA)?;
        for at in 0..self.class_count {
            let held = self.classes[at];
            if held.merged_into.is_some() {
                continue;
            }
            // A copy: placing the equalities edits the preparation.
            let mut copied = [Member::plain(0, 0); MAX_MEMBERS];
            let held_members = self.members_of(at as u8);
            copied[..held_members.len()].copy_from_slice(held_members);
            let members = &copied[..held_members.len()];
            if held.has_const {
                if members.iter().any(|member| member.nulling != Relids::EMPTY) {
                    return Err("a constant's class of a nulled column");
                }
                // One `column = constant`: the clause itself.
                if members.len() == 1 && held.sources == 1 && !held.derived {
                    let (qual, ..) = held.constant.expect("a constant");
                    self.place(Placed::Restriction {
                        rel: members[0].rel,
                        qual,
                    })?;
                    continue;
                }
                for &member in members {
                    self.place(Placed::Implied {
                        member,
                        class: at as u8,
                    })?;
                    // A member of several relations is a join clause of
                    // them, which reads its columns there.
                    if member.reads != Relids::EMPTY
                        && let Some(id) = member.expression_id()
                    {
                        for &var in self.tree.expression_vars[id].as_slice() {
                            self.note_needed(var, member.base_relids(), NO_QUAL)?;
                        }
                    }
                }
                continue;
            }
            if members.len() <= 1 {
                continue;
            }
            // Each plain member equated with the relation's previous one.
            previous.fill(None);
            for &member in members {
                if member.nulling != Relids::EMPTY || member.reads != Relids::EMPTY {
                    continue;
                }
                let rel = usize::from(member.rel);
                if let Some(left) = previous[rel] {
                    self.place(Placed::Equated {
                        left,
                        right: member,
                    })?;
                }
                previous[rel] = Some(member);
            }
            // `add_vars_to_targetlist` at the class's relations: a column
            // only its own relation's class reads is needed by no join.
            let relids = self.class_relids(at as u8);
            for &member in members {
                // An expression's columns (`pull_var_clause` of `em_expr`).
                if let Some(id) = member.expression_id() {
                    for &var in self.tree.expression_vars[id].as_slice() {
                        if relids & !Relids::of(var.0) != Relids::EMPTY {
                            self.note_needed(var, relids, NO_QUAL)?;
                        }
                    }
                    continue;
                }
                if relids & !Relids::of(member.rel) != Relids::EMPTY {
                    self.note_needed((member.rel, member.column), relids, NO_QUAL)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

    /// A statement arena for one preparation.
    fn arena() -> Arena {
        let mut budget = crate::mem::budget::Budget::new(1 << 24);
        Arena::new(&mut budget, "join preparation", 1 << 23).expect("arena")
    }

    /// The most relations a query plans.
    const MOST: usize = super::super::joins::RELATION_NUMBERS;

    /// `left.column = right.column`, strict for both.
    fn equality(left: Var, right: Var) -> Qual {
        let mut vars = VarList::default();
        vars.add(left).expect("room");
        vars.add(right).expect("room");
        Qual {
            rels: Relids::of(left.0) | Relids::of(right.0),
            vars,
            nonnullable: Relids::of(left.0) | Relids::of(right.0),
            strict_vars: vars,
            forced_null: None,
            shape: Shape::Columns(left, right),
        }
    }

    /// `column IS NULL`.
    fn is_null(var: Var) -> Qual {
        let mut vars = VarList::default();
        vars.add(var).expect("room");
        Qual {
            rels: Relids::of(var.0),
            vars,
            nonnullable: Relids::EMPTY,
            strict_vars: VarList::default(),
            forced_null: Some(var),
            shape: Shape::Other,
        }
    }

    /// `rel0 <kind> rel1 ON 0.0 = 1.0 <kind> rel2 ON 1.1 = 2.0`, with
    /// `where_quals` in `WHERE`.
    fn chain<'a>(
        arena: &'a Arena,
        first: TreeJoin,
        second: TreeJoin,
        where_quals: &[Qual],
    ) -> Tree<'a> {
        let mut tree = Tree::new(3, 3, arena).expect("room");
        let a = tree.node(TreeNode::Rel(0)).expect("room");
        let b = tree.node(TreeNode::Rel(1)).expect("room");
        let c = tree.node(TreeNode::Rel(2)).expect("room");
        let numbered = |kind: TreeJoin| !matches!(kind, TreeJoin::Inner);
        let one = tree
            .node(TreeNode::Join {
                kind: first,
                left: a,
                right: b,
                rtindex: numbered(first).then_some(0),
            })
            .expect("room");
        tree.qual(one, equality((0, 0), (1, 0))).expect("room");
        let two = tree
            .node(TreeNode::Join {
                kind: second,
                left: one,
                right: c,
                rtindex: numbered(second).then_some(1),
            })
            .expect("room");
        tree.qual(two, equality((1, 1), (2, 0))).expect("room");
        let root = tree.list_of(&[two]).expect("room");
        tree.root = root;
        for qual in where_quals {
            tree.qual(root, *qual).expect("room");
        }
        tree
    }

    /// A chain of the most relations a query plans, inner joined by
    /// `columns` equalities of each relation with the first one, as
    /// `USING` joins on those columns read: more join tree nodes than a
    /// byte numbers.
    fn using_chain(arena: &Arena, columns: u16) -> Tree<'_> {
        let mut tree = Tree::new(MOST, MOST, arena).expect("room");
        let mut current = tree.node(TreeNode::Rel(0)).expect("room");
        for rel in 1..MOST as u8 {
            let right = tree.node(TreeNode::Rel(rel)).expect("room");
            current = tree
                .node(TreeNode::Join {
                    kind: TreeJoin::Inner,
                    left: current,
                    right,
                    rtindex: None,
                })
                .expect("room");
            for column in 0..columns {
                tree.qual(current, equality((0, column), (rel, column)))
                    .expect("room");
            }
        }
        tree.root = tree.list_of(&[current]).expect("room");
        tree
    }

    #[test]
    fn using_chains_of_the_most_relations_fit() {
        let arena = arena();
        let tree = using_chain(&arena, 2);
        assert!(tree.node_count > 256);
        let prepared = prepare(&tree, &arena).expect("modeled");
        let classes = prepared.classes();
        assert_eq!(classes.len(), 2);
        for (column, class) in classes.iter().enumerate() {
            assert_eq!(class.count(), MOST);
            assert!(class.members().iter().enumerate().all(|(rel, member)| (
                usize::from(member.rel),
                usize::from(member.column)
            ) == (rel, column)));
        }
        // Two more conditions fill the conditions' capacity; the caller
        // reports the one past it by name.
        let mut tree = using_chain(&arena, 2);
        let root = tree.root;
        for _ in 0..QUALS_PER_RELATION * MOST - 2 * (MOST - 1) {
            assert!(tree.qual(root, equality((0, 2), (1, 2))).is_some());
        }
        assert!(tree.qual(root, equality((0, 2), (1, 2))).is_none());
    }

    #[test]
    fn left_joins_commute_by_identity_three() {
        let arena = arena();
        let prepared =
            prepare(&chain(&arena, TreeJoin::Left, TreeJoin::Left, &[]), &arena).expect("modeled");
        let specials = &prepared.specials[..prepared.special_count];
        assert_eq!(specials.len(), 2);
        let (one, two) = (specials[0], specials[1]);
        assert_eq!(
            (one.min_left, one.min_right),
            (Relids::from_mask(1), Relids::from_mask(2))
        );
        // The upper join reads only `rel1` of its left side, so it may run
        // first: it commutes with the lower one.
        assert_eq!(
            (two.min_left, two.min_right),
            (Relids::from_mask(2), Relids::from_mask(4))
        );
        assert_eq!(two.commute_below_l, one.oj);
        assert_eq!(one.commute_above_l, two.oj);
        // Each copy of the upper join's clause: one for each order.
        let copies = prepared
            .placed()
            .iter()
            .filter(|placed| matches!(placed, Placed::Join(join) if join.qual == 1))
            .count();
        assert_eq!(copies, 2);
    }

    #[test]
    fn a_strict_where_condition_makes_a_left_join_inner() {
        let arena = arena();
        // `WHERE rel1.column1 = rel2.column0` rejects the NULLs both left
        // joins would return.
        let prepared = prepare(
            &chain(
                &arena,
                TreeJoin::Left,
                TreeJoin::Left,
                &[equality((1, 1), (2, 0))],
            ),
            &arena,
        )
        .expect("modeled");
        assert_eq!(prepared.special_count, 0);
    }

    #[test]
    fn is_null_of_a_compared_column_makes_an_anti_join() {
        let arena = arena();
        let prepared = prepare(
            &chain(&arena, TreeJoin::Inner, TreeJoin::Left, &[is_null((2, 0))]),
            &arena,
        )
        .expect("modeled");
        assert_eq!(prepared.special_count, 1);
        assert_eq!(prepared.specials[0].kind, SpecialKind::Anti);
        // The test itself is then true and dropped.
        assert!(
            prepared
                .placed()
                .iter()
                .all(|placed| !matches!(placed, Placed::Join(join) if join.qual == 2))
        );
    }

    #[test]
    fn a_right_join_swaps_its_sides() {
        let arena = arena();
        let prepared = prepare(
            &chain(&arena, TreeJoin::Right, TreeJoin::Inner, &[]),
            &arena,
        )
        .expect("modeled");
        let special = prepared.specials[0];
        assert_eq!(special.kind, SpecialKind::Left);
        assert_eq!(
            (special.syn_left, special.syn_right),
            (Relids::from_mask(2), Relids::from_mask(1))
        );
        // The join list starts with the swapped join's preserved side.
        let top = *prepared
            .subproblems
            .borrowed_slice()
            .last()
            .expect("a problem");
        let first = usize::from(top.first);
        assert_eq!(
            &prepared.subproblem_items.borrowed_slice()[first..first + usize::from(top.count)],
            &[2, 1, 4].map(Relids::from_mask)
        );
    }
}
