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
    BASE_RELIDS, CloneKind, EquivalenceClass, MAX_JOIN_RELS, MAX_MEMBERS, Member, OJ_SHIFT, Relids,
    SpecialJoin, SpecialKind, Subproblem,
};

/// Quals of one query the preparation holds.
pub(crate) const MAX_QUALS: usize = 48;
/// Nodes of one join tree, numbered by `u8`.
pub(crate) const MAX_NODES: usize = 256;
/// Columns a qual's strictness names.
pub(crate) const MAX_QUAL_VARS: usize = 8;
/// Equivalence classes of one query.
pub(crate) const MAX_CLASSES: usize = 24;
/// Placed quals of one query.
pub(crate) const MAX_PLACED: usize = 96;
/// Outer, semi and anti joins of one query.
pub(crate) const MAX_SPECIALS: usize = MAX_JOIN_RELS;
/// No qual: a column a class needs.
pub(crate) const NO_QUAL: u8 = u8::MAX;
/// The constant `true` that stands in for an outer join clause
/// `reconsider_outer_join_clauses` replaced.
pub(crate) const TRUE_QUAL: u8 = u8::MAX - 1;

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
    /// constants share `constant`.
    Constant { var: Var, constant: u16 },
    /// `COALESCE(left, right) = constant` (a full join's merged `USING`
    /// column), which `reconsider_full_join_clause` matches with a full
    /// join's clause `left = right`.
    Coalesce {
        left: Var,
        right: Var,
        constant: u16,
    },
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
        first: u8,
        count: u8,
    },
    Join {
        kind: TreeJoin,
        left: u8,
        right: u8,
        /// Its order among the written outer joins (the join's range
        /// table entry), or `None` for an inner or semi join.
        rtindex: Option<u8>,
    },
}

/// The written join tree and its quals, each attached to the node whose
/// `ON` or `WHERE` holds it.
pub(crate) struct Tree {
    pub(crate) nodes: [TreeNode; MAX_NODES],
    pub(crate) node_count: usize,
    pub(crate) children: [u8; MAX_NODES],
    pub(crate) child_count: usize,
    pub(crate) root: u8,
    pub(crate) quals: [Qual; MAX_QUALS],
    pub(crate) qual_node: [u8; MAX_QUALS],
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
}

impl Tree {
    pub(crate) fn new(rels: usize) -> Self {
        Self {
            nodes: [TreeNode::Rel(0); MAX_NODES],
            node_count: 0,
            children: [0; MAX_NODES],
            child_count: 0,
            root: 0,
            quals: [Qual {
                rels: Relids::EMPTY,
                vars: VarList::default(),
                nonnullable: Relids::EMPTY,
                strict_vars: VarList::default(),
                forced_null: None,
                shape: Shape::Other,
            }; MAX_QUALS],
            qual_node: [0; MAX_QUALS],
            qual_count: 0,
            rels,
            enable_hashagg: true,
            from_collapse_limit: 8,
            join_collapse_limit: 8,
            expression_vars: [VarList::default(); super::joins::MAX_JOIN_EXPRESSIONS],
        }
    }

    /// Adds a node, returning its number.
    pub(crate) fn node(&mut self, node: TreeNode) -> Option<u8> {
        let at = self.node_count;
        *self.nodes.get_mut(at)? = node;
        self.node_count += 1;
        Some(at as u8)
    }

    /// A `FROM` list of `items`.
    pub(crate) fn list_of(&mut self, items: &[u8]) -> Option<u8> {
        let first = self.child_count;
        for &item in items {
            *self.children.get_mut(self.child_count)? = item;
            self.child_count += 1;
        }
        self.node(TreeNode::From {
            first: first as u8,
            count: items.len() as u8,
        })
    }

    /// Attaches `qual` to `node`.
    pub(crate) fn qual(&mut self, node: u8, qual: Qual) -> Option<u8> {
        let at = self.qual_count;
        *self.quals.get_mut(at)? = qual;
        self.qual_node[at] = node;
        self.qual_count += 1;
        Some(at as u8)
    }

    fn items(&self, first: u8, count: u8) -> &[u8] {
        &self.children[usize::from(first)..usize::from(first) + usize::from(count)]
    }

    /// The quals attached to `node`, in order.
    fn quals_of(&self, node: u8) -> impl Iterator<Item = u8> + '_ {
        (0..self.qual_count)
            .filter(move |&at| self.qual_node[at] == node)
            .map(|at| at as u8)
    }

    /// The base relations under `node`.
    fn base_rels(&self, node: u8) -> Relids {
        match self.nodes[usize::from(node)] {
            TreeNode::Rel(rel) => Relids::of(rel),
            TreeNode::From { first, count } => self
                .items(first, count)
                .iter()
                .fold(Relids::EMPTY, |relids, &item| relids | self.base_rels(item)),
            TreeNode::Join { left, right, .. } => self.base_rels(left) | self.base_rels(right),
        }
    }

    fn contains_outer(&self, node: u8) -> bool {
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
pub(crate) enum Placed {
    /// A restriction of one relation's scan (`baserestrictinfo`).
    Restriction { rel: u8, qual: u8 },
    /// `member = constant` of a class with a constant
    /// (`generate_base_implied_equalities_const`).
    Implied { member: Member, class: u8 },
    /// `left = right` of two columns of one relation a class equates
    /// (`generate_base_implied_equalities_no_const`).
    Equated { left: Member, right: Member },
    /// A join clause.
    Join(JoinQual),
}

/// A qual evaluated at a join.
#[derive(Clone, Copy, Debug)]
pub(crate) struct JoinQual {
    pub(crate) qual: u8,
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
pub(crate) struct Prepared {
    pub(crate) specials: [SpecialJoin; MAX_SPECIALS],
    pub(crate) special_count: usize,
    pub(crate) subproblems: [Subproblem; MAX_JOIN_RELS],
    pub(crate) subproblem_count: usize,
    pub(crate) subproblem_items: [Relids; 2 * MAX_JOIN_RELS],
    pub(crate) subproblem_item_count: usize,
    pub(crate) classes: [EquivalenceClass; MAX_CLASSES],
    pub(crate) class_count: usize,
    /// Each class's constant, by the qual that equated it.
    pub(crate) class_constant: [Option<u8>; MAX_CLASSES],
    pub(crate) placed: [Placed; MAX_PLACED],
    pub(crate) placed_count: usize,
    /// Each column some join reads (`attr_needed`), with the qual that
    /// reads it there ([`NO_QUAL`] for a class's); a removed join's column
    /// is needed only by its own relation.
    pub(crate) needed: [(Var, Relids, u8); 64],
    pub(crate) needed_count: usize,
    /// A class equates two different constants in the query's join
    /// domain: the query returns no rows.
    pub(crate) contradictory: bool,
    /// The base relations a class equating two different constants
    /// inside an outer join empties (`get_join_domain_min_rels`): one
    /// relation is dummy, several make the join of them dummy; 0 when no
    /// class does.
    pub(crate) false_relids: Relids,
}

impl Prepared {
    pub(crate) fn placed(&self) -> &[Placed] {
        &self.placed[..self.placed_count]
    }

    pub(crate) fn classes(&self) -> &[EquivalenceClass] {
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
            !class.has_const && class.count > 1 && class.relids() & info.min_right != Relids::EMPTY
        }) {
            return Err("a removable join's relation in an equivalence class");
        }
        let mut kept = 0;
        let mut dropped_quals = [false; MAX_QUALS + 1];
        for index in 0..self.placed_count {
            let mut entry = self.placed[index];
            let keep = match &mut entry {
                Placed::Restriction { rel, .. } => Relids::of(*rel) & info.min_right == Relids::EMPTY,
                Placed::Implied { member, .. } => member.relids() & gone == Relids::EMPTY,
                Placed::Equated { left, .. } => left.relids() & gone == Relids::EMPTY,
                Placed::Join(join) if join.required & gone == Relids::EMPTY && join.relids & gone == Relids::EMPTY => true,
                Placed::Join(join) => {
                    if join.relids & info.min_right != Relids::EMPTY
                        || !(join.pushed_down || join.required & !join_plus_commute != Relids::EMPTY)
                    {
                        dropped_quals[usize::from(join.qual)] = true;
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
                self.placed[kept] = entry;
                kept += 1;
            }
        }
        self.placed_count = kept;
        let mut kept = 0;
        for index in 0..self.needed_count {
            let (var, relids, qual) = self.needed[index];
            if Relids::of(var.0) & info.min_right != Relids::EMPTY {
                continue;
            }
            // A column only a dropped clause read stays in its relation's
            // target list (`reltarget` is not rebuilt), needed by no join.
            let relids = if qual != NO_QUAL && dropped_quals[usize::from(qual)] {
                Relids::of(var.0)
            } else {
                relids & !gone
            };
            self.needed[kept] = (var, relids, qual);
            kept += 1;
        }
        self.needed_count = kept;
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
        for problem in &mut self.subproblems[..self.subproblem_count] {
            let first = kept;
            for index in usize::from(problem.first)..usize::from(problem.first + problem.count) {
                let item = self.subproblem_items[index] & !gone;
                if !item.is_empty() {
                    self.subproblem_items[kept] = item;
                    kept += 1;
                }
            }
            *problem = Subproblem {
                first: first as u16,
                count: (kept - first) as u16,
            };
        }
        self.subproblem_item_count = kept;
        Ok(())
    }

    /// The relations and outer joins that need column `var` above its
    /// scan.
    pub(crate) fn needed_by(&self, var: Var) -> Relids {
        self.needed[..self.needed_count]
            .iter()
            .filter(|(held, ..)| *held == var)
            .fold(Relids::EMPTY, |relids, (_, needed, _)| relids | *needed)
    }
}

/// A class during preparation: merged ones forward to the class that
/// absorbed them (`ec_merged`).
#[derive(Clone, Copy)]
struct BuildClass {
    class: EquivalenceClass,
    /// Its constant: the qual that equated it, the constant's identity
    /// and its join domain (`em_jdomain`).
    constant: Option<(u8, u16, u8)>,
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
struct Prep<'t> {
    tree: &'t Tree,
    kinds: [Option<TreeJoin>; MAX_NODES],
    /// Left and right children after `reduce_outer_joins` swapped a
    /// right join's.
    sides: [(u8, u8); MAX_NODES],
    /// Each outer join node's bit.
    oj: [Relids; MAX_NODES],
    qualscope: [Relids; MAX_NODES],
    inner_join_rels: [Relids; MAX_NODES],
    left_rels: [Relids; MAX_NODES],
    right_rels: [Relids; MAX_NODES],
    nonnullable_rels: [Relids; MAX_NODES],
    /// Each node's special join, by its position in `specials`.
    special_of: [Option<u8>; MAX_NODES],
    specials: [SpecialJoin; MAX_SPECIALS],
    special_count: usize,
    /// The post-order list of nodes (`item_list`).
    items: [u8; MAX_NODES],
    item_count: usize,
    /// Postponed quals of each left join (`oj_joinclauses`).
    postponed: [[u8; MAX_QUALS]; MAX_NODES],
    postponed_count: [u8; MAX_NODES],
    classes: [BuildClass; MAX_CLASSES],
    class_count: usize,
    placed: [Placed; MAX_PLACED],
    placed_count: usize,
    sources: [(u8, u8); MAX_QUALS],
    source_count: usize,
    needed: [(Var, Relids, u8); 64],
    needed_count: usize,
    left_join: [Pending; MAX_QUALS],
    left_count: usize,
    right_join: [Pending; MAX_QUALS],
    right_count: usize,
    full_join: [Pending; MAX_QUALS],
    full_count: usize,
    /// The `COALESCE(left, right) = constant` quals of the query's join
    /// domain: the qual, its sides and its constant.
    coalesce: [(u8, Var, Var, u16); MAX_QUALS],
    /// Each held `COALESCE`'s placement, which an ordinary filter of the
    /// joined rows takes when no full join clause matches it.
    coalesce_join: [Option<JoinQual>; MAX_QUALS],
    coalesce_count: usize,
    /// Each node's join domain (`JoinDomain`): the query's is 0.
    domain: [u8; MAX_NODES],
    domain_count: u8,
    /// The node each join domain holds the relations of (`jd_relids`):
    /// a left or anti join's nullable side, a full join's side; `None`
    /// for the query's and a full join's quals' domain.
    domain_root: [Option<u8>; MAX_NODES],
    /// The join lists: each list's elements, a negative entry naming a
    /// nested list by `-(index + 1)`.
    lists: [[i16; MAX_JOIN_RELS]; MAX_NODES],
    list_len: [u8; MAX_NODES],
    list_count: usize,
    contradictory: bool,
    false_relids: Relids,
}

/// The join problems `make_rel_from_joinlist` collects.
struct Subproblems {
    subproblems: [Subproblem; MAX_JOIN_RELS],
    count: usize,
    items: [Relids; 2 * MAX_JOIN_RELS],
    item_count: usize,
}

const FULL: Outside = "a query beyond the join preparation's capacity";

/// Prepares `tree` as `query_planner` does up to the join search.
pub(crate) fn prepare(tree: &Tree) -> Result<Prepared, Outside> {
    let mut prep = Prep {
        tree,
        kinds: [None; MAX_NODES],
        sides: [(0, 0); MAX_NODES],
        oj: [Relids::EMPTY; MAX_NODES],
        qualscope: [Relids::EMPTY; MAX_NODES],
        inner_join_rels: [Relids::EMPTY; MAX_NODES],
        left_rels: [Relids::EMPTY; MAX_NODES],
        right_rels: [Relids::EMPTY; MAX_NODES],
        nonnullable_rels: [Relids::EMPTY; MAX_NODES],
        special_of: [None; MAX_NODES],
        specials: [SpecialJoin {
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
        }; MAX_SPECIALS],
        special_count: 0,
        items: [0; MAX_NODES],
        item_count: 0,
        postponed: [[0; MAX_QUALS]; MAX_NODES],
        postponed_count: [0; MAX_NODES],
        classes: [BuildClass {
            class: EquivalenceClass {
                members: [Member::plain(0, 0); MAX_MEMBERS],
                count: 0,
                has_const: false,
            },
            constant: None,
            merged_into: None,
            sources: 0,
            derived: false,
        }; MAX_CLASSES],
        class_count: 0,
        placed: [Placed::Restriction { rel: 0, qual: 0 }; MAX_PLACED],
        placed_count: 0,
        sources: [(0, 0); MAX_QUALS],
        source_count: 0,
        needed: [((0, 0), Relids::EMPTY, NO_QUAL); 64],
        needed_count: 0,
        left_join: [Pending { join: empty_join() }; MAX_QUALS],
        left_count: 0,
        right_join: [Pending { join: empty_join() }; MAX_QUALS],
        right_count: 0,
        full_join: [Pending { join: empty_join() }; MAX_QUALS],
        full_count: 0,
        coalesce: [(0, (0, 0), (0, 0), 0); MAX_QUALS],
        coalesce_join: [None; MAX_QUALS],
        coalesce_count: 0,
        domain: [0; MAX_NODES],
        domain_count: 1,
        domain_root: [None; MAX_NODES],
        lists: [[0; MAX_JOIN_RELS]; MAX_NODES],
        list_len: [0; MAX_NODES],
        list_count: 0,
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
    prep.number_outer_joins();
    prep.assign_domains(tree.root, 0);
    let top = prep.deconstruct_recurse(tree.root)?;
    for at in 0..prep.item_count {
        let item = prep.items[at];
        prep.distribute_node(item)?;
    }
    for at in 0..prep.item_count {
        let item = prep.items[at];
        if prep.postponed_count[usize::from(item)] != 0 {
            prep.distribute_oj_quals(item)?;
        }
    }
    prep.reconsider_outer_join_clauses()?;
    prep.generate_base_implied_equalities()?;
    // `make_rel_from_joinlist`: each nested list's problem first.
    let mut problems = Subproblems {
        subproblems: [Subproblem::default(); MAX_JOIN_RELS],
        count: 0,
        items: [Relids::EMPTY; 2 * MAX_JOIN_RELS],
        item_count: 0,
    };
    prep.make_rel_from_joinlist(top, &mut problems)?;
    // The classes in their final order.
    let mut map = [u8::MAX; MAX_CLASSES];
    let mut classes = [EquivalenceClass {
        members: [Member::plain(0, 0); MAX_MEMBERS],
        count: 0,
        has_const: false,
    }; MAX_CLASSES];
    let mut class_constant = [None; MAX_CLASSES];
    let mut class_count = 0;
    for (slot, held) in map.iter_mut().zip(&prep.classes[..prep.class_count]) {
        if held.merged_into.is_none() {
            *slot = class_count as u8;
            classes[class_count] = held.class;
            class_constant[class_count] = held.constant.map(|(qual, ..)| qual);
            class_count += 1;
        }
    }
    let canonical = |prep: &Prep<'_>, class: u8| map[usize::from(prep.canonical(class))];
    let mut placed = prep.placed;
    for entry in &mut placed[..prep.placed_count] {
        match entry {
            Placed::Implied { class, .. } => *class = canonical(&prep, *class),
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
        specials: prep.specials,
        special_count: prep.special_count,
        subproblems: problems.subproblems,
        subproblem_count: problems.count,
        subproblem_items: problems.items,
        subproblem_item_count: problems.item_count,
        classes,
        class_count,
        class_constant,
        placed,
        placed_count: prep.placed_count,
        needed: prep.needed,
        needed_count: prep.needed_count,
        contradictory: prep.contradictory,
        false_relids: prep.false_relids,
    })
}

const fn empty_join() -> JoinQual {
    JoinQual {
        qual: 0,
        relids: Relids::EMPTY,
        required: Relids::EMPTY,
        pushed_down: false,
        clone: CloneKind::None,
        incompatible: Relids::EMPTY,
        outer_relids: Relids::EMPTY,
        merge: None,
    }
}

impl Prep<'_> {
    fn qual(&self, id: u8) -> &Qual {
        &self.tree.quals[usize::from(id)]
    }

    /// `reduce_outer_joins_pass2`.
    fn reduce_outer_joins(
        &mut self,
        node: u8,
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
                    TreeJoin::Left if nonnullable & right_rels != Relids::EMPTY => kind = TreeJoin::Inner,
                    TreeJoin::Right if nonnullable & left_rels != Relids::EMPTY => kind = TreeJoin::Inner,
                    TreeJoin::Full => {
                        kind = match (nonnullable & left_rels != Relids::EMPTY, nonnullable & right_rels != Relids::EMPTY) {
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
    fn assign_domains(&mut self, node: u8, domain: u8) {
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

    fn next_domain(&mut self) -> u8 {
        self.domain_count += 1;
        self.domain_count - 1
    }

    /// Gives each join still outer its bit, in the order of the written
    /// joins' range table entries.
    fn number_outer_joins(&mut self) {
        let mut order = [(u8::MAX, 0u8); MAX_NODES];
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
                order[count] = (rtindex, at as u8);
                count += 1;
            }
        }
        order[..count].sort_unstable();
        for (bit, &(_, node)) in order[..count].iter().enumerate() {
            self.oj[usize::from(node)] = Relids::of(OJ_SHIFT + bit as u32);
        }
    }

    fn push_list(&mut self, elements: &[i16]) -> Result<usize, Outside> {
        let at = self.list_count;
        if at == MAX_NODES || elements.len() > MAX_JOIN_RELS {
            return Err(FULL);
        }
        self.lists[at][..elements.len()].copy_from_slice(elements);
        self.list_len[at] = elements.len() as u8;
        self.list_count += 1;
        Ok(at)
    }

    fn list(&self, at: usize) -> &[i16] {
        &self.lists[at][..usize::from(self.list_len[at])]
    }

    /// `deconstruct_recurse`: each node's scopes, the post-order item list,
    /// and the node's join list, returned by its number.
    fn deconstruct_recurse(&mut self, node: u8) -> Result<usize, Outside> {
        let at = usize::from(node);
        let tree = self.tree;
        let joinlist = match tree.nodes[at] {
            TreeNode::Rel(rel) => {
                self.qualscope[at] = Relids::of(rel);
                self.push_list(&[i16::from(rel)])?
            }
            TreeNode::From { first, count } => {
                let limit = self.tree.from_collapse_limit;
                let mut elements = [0i16; MAX_JOIN_RELS];
                let mut len = 0;
                let items = tree.items(first, count);
                for (position, &item) in items.iter().enumerate() {
                    let sub = self.deconstruct_recurse(item)?;
                    self.qualscope[at] |= self.qualscope[usize::from(item)];
                    self.inner_join_rels[at] = self.inner_join_rels[usize::from(item)];
                    let remaining = items.len() - position - 1;
                    let members = self.list(sub).len();
                    if members <= 1 || (len + members + remaining) as i64 <= limit {
                        for &element in self.list(sub) {
                            *elements.get_mut(len).ok_or(FULL)? = element;
                            len += 1;
                        }
                    } else {
                        *elements.get_mut(len).ok_or(FULL)? = -(sub as i16 + 1);
                        len += 1;
                    }
                }
                if items.len() > 1 {
                    self.inner_join_rels[at] = self.qualscope[at];
                }
                self.push_list(&elements[..len])?
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
                let nested = |list: usize| -(list as i16 + 1);
                if kind == TreeJoin::Full {
                    let pair = self.push_list(&[nested(left_list), nested(right_list)])?;
                    self.push_list(&[nested(pair)])?
                } else if (left_len + right_len) as i64 <= self.tree.join_collapse_limit {
                    let mut elements = [0i16; MAX_JOIN_RELS];
                    elements[..left_len].copy_from_slice(self.list(left_list));
                    let right_elements = self.lists[right_list];
                    elements[left_len..left_len + right_len]
                        .copy_from_slice(&right_elements[..right_len]);
                    self.push_list(&elements[..left_len + right_len])?
                } else {
                    // A side of one element joins as itself, not as a
                    // one-element sublist.
                    let part = |list: usize, len: usize, lists: &[[i16; MAX_JOIN_RELS]]| {
                        if len == 1 { lists[list][0] } else { nested(list) }
                    };
                    self.push_list(&[
                        part(left_list, left_len, &self.lists),
                        part(right_list, right_len, &self.lists),
                    ])?
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
        out: &mut Subproblems,
    ) -> Result<Relids, Outside> {
        let mut items = [Relids::EMPTY; MAX_JOIN_RELS];
        let elements = self.list(list);
        for (at, &element) in elements.iter().enumerate() {
            items[at] = if element >= 0 {
                Relids::of(element as u8)
            } else {
                self.make_rel_from_joinlist((-element - 1) as usize, out)?
            };
        }
        let items = &items[..elements.len()];
        if items.len() == 1 {
            return Ok(items[0]);
        }
        let first = out.item_count;
        out.items
            .get_mut(first..first + items.len())
            .ok_or(FULL)?
            .copy_from_slice(items);
        out.item_count += items.len();
        *out.subproblems.get_mut(out.count).ok_or(FULL)? = Subproblem {
            first: first as u16,
            count: items.len() as u16,
        };
        out.count += 1;
        Ok(items.iter().fold(Relids::EMPTY, |relids, &item| relids | item))
    }

    /// The outer joins below `node` nulling relation `rel`
    /// (`varnullingrels` of a column read at `node`).
    fn nulling_below(&self, node: u8, rel: u8) -> Relids {
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
    fn nulling_below_item(&self, node: u8, rel: u8) -> Relids {
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
    fn nulling_at(&self, node: u8) -> [Relids; MAX_JOIN_RELS] {
        core::array::from_fn(|rel| {
            if rel < self.tree.rels {
                self.nulling_below(node, rel as u8)
            } else {
                Relids::EMPTY
            }
        })
    }

    /// `deconstruct_distribute`.
    fn distribute_node(&mut self, node: u8) -> Result<(), Outside> {
        let at = usize::from(node);
        let tree = self.tree;
        match tree.nodes[at] {
            TreeNode::Rel(_) => Ok(()),
            TreeNode::From { .. } => {
                let nulling = self.nulling_at(node);
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
                let nulling = self.nulling_at(node);
                let (special, mut ojscope) = if kind == TreeJoin::Inner {
                    (None, None)
                } else {
                    let special = self.make_outerjoininfo(node, kind, &nulling)?;
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
        node: u8,
        kind: TreeJoin,
        nulling: &[Relids; MAX_JOIN_RELS],
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
        if id == MAX_SPECIALS {
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
    fn compute_semijoin_info(
        &self,
        node: u8,
        info: &mut SpecialJoin,
        nulling: &[Relids; MAX_JOIN_RELS],
    ) {
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
    fn distribute_oj_quals(&mut self, node: u8) -> Result<(), Outside> {
        let at = usize::from(node);
        let special = self.special_of[at].expect("a left join");
        let info = self.specials[usize::from(special)];
        let qualscope = info.syn_left | info.syn_right | info.oj;
        let ojscope = info.min_left | info.min_right;
        let nonnullable = info.syn_left;
        let quals = self.postponed[at];
        let quals = &quals[..usize::from(self.postponed_count[at])];
        let base_nulling = self.nulling_at(node);
        if info.commute_above_r != Relids::EMPTY || info.commute_below_l != Relids::EMPTY {
            let (above, below) = (info.commute_above_r, info.commute_below_l);
            // The quals without the nulling of the joins it commutes with
            // from below.
            let mut nulling = base_nulling;
            for relids in &mut nulling {
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
        qual_id: u8,
        node: u8,
        special: Option<u8>,
        qualscope: Relids,
        ojscope: Option<Relids>,
        nonnullable: Relids,
        incompatible: Relids,
        allow_equivalence: bool,
        clone: CloneKind,
        nulling: [Relids; MAX_JOIN_RELS],
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
                let at = usize::from(node);
                let count = usize::from(self.postponed_count[at]);
                *self.postponed[at].get_mut(count).ok_or(FULL)? = qual_id;
                self.postponed_count[at] += 1;
                return Ok(());
            }
            pushed_down = false;
            maybe_equivalence = false;
            maybe_outer_join = true;
            relids = ojscope.expect("an outer join's scope");
        } else {
            pushed_down = true;
            if self.redundant_nullability(&qual, &nulling) {
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
                let left_member = self.var_member(left, &nulling);
                let right_member = self.var_member(right, &nulling);
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
                    if left_relids & !nonnullable == Relids::EMPTY && right_relids & nonnullable == Relids::EMPTY {
                        *self.left_join.get_mut(self.left_count).ok_or(FULL)? = pending;
                        self.left_count += 1;
                        return Ok(());
                    }
                    if right_relids & !nonnullable == Relids::EMPTY && left_relids & nonnullable == Relids::EMPTY {
                        *self.right_join.get_mut(self.right_count).ok_or(FULL)? = pending;
                        self.right_count += 1;
                        return Ok(());
                    }
                    if special
                        .is_some_and(|id| self.specials[usize::from(id)].kind == SpecialKind::Full)
                    {
                        *self.full_join.get_mut(self.full_count).ok_or(FULL)? = pending;
                        self.full_count += 1;
                        return Ok(());
                    }
                } else {
                    join.merge = Some(self.mergeclause_classes(left_member, right_member)?);
                }
            }
            Shape::Constant { var, constant } => {
                let member = self.var_member(var, &nulling);
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
            // `process_equivalence` of the `COALESCE` with the constant,
            // held for `reconsider_full_join_clause`.
            Shape::Coalesce {
                left,
                right,
                constant,
            } => {
                if !maybe_equivalence || self.domain[usize::from(node)] != 0 {
                    return Err("a COALESCE equated with a constant below the query's join domain");
                }
                *self.coalesce.get_mut(self.coalesce_count).ok_or(FULL)? =
                    (qual_id, left, right, constant);
                self.coalesce_join[self.coalesce_count] = Some(join);
                self.coalesce_count += 1;
                return Ok(());
            }
            Shape::Other => {}
        }
        self.distribute_restrictinfo(join)
    }

    /// `distribute_restrictinfo_to_rels`.
    fn distribute_restrictinfo(&mut self, join: JoinQual) -> Result<(), Outside> {
        let placed = if join.required.count_ones() == 1 && join.required & BASE_RELIDS != Relids::EMPTY {
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
        *self.placed.get_mut(self.placed_count).ok_or(FULL)? = placed;
        self.placed_count += 1;
        Ok(())
    }

    fn note_needed(&mut self, var: Var, relids: Relids, qual: u8) -> Result<(), Outside> {
        *self.needed.get_mut(self.needed_count).ok_or(FULL)? = (var, relids, qual);
        self.needed_count += 1;
        Ok(())
    }

    /// `check_redundant_nullability_qual`: `column IS NULL` of a column an
    /// anti join below nulls is true.
    fn redundant_nullability(&self, qual: &Qual, nulling: &[Relids; MAX_JOIN_RELS]) -> bool {
        let Some((rel, _)) = qual.forced_null else {
            return false;
        };
        let nulled = nulling[usize::from(rel)];
        nulled != Relids::EMPTY
            && self.specials[..self.special_count]
                .iter()
                .any(|special| special.kind == SpecialKind::Anti && special.oj & nulled != Relids::EMPTY)
    }

    fn canonical(&self, mut class: u8) -> u8 {
        while let Some(next) = self.classes[usize::from(class)].merged_into {
            class = next;
        }
        class
    }

    /// The live class holding `member`.
    fn class_of(&self, member: Member) -> Option<u8> {
        (0..self.class_count)
            .find(|&at| {
                self.classes[at].merged_into.is_none()
                    && self.classes[at].class.members().contains(&member)
            })
            .map(|at| at as u8)
    }

    /// The live class holding constant `constant` of join domain
    /// `domain`.
    fn class_of_constant(&self, constant: u16, domain: u8) -> Option<u8> {
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
            class: EquivalenceClass {
                members: [Member::plain(0, 0); MAX_MEMBERS],
                count: 0,
                has_const: false,
            },
            constant: None,
            merged_into: None,
            sources: 0,
            derived: false,
        };
        self.class_count += 1;
        Ok(at as u8)
    }

    fn add_member(&mut self, class: u8, member: Member) -> Result<(), Outside> {
        let held = &mut self.classes[usize::from(class)].class;
        if usize::from(held.count) == MAX_MEMBERS {
            return Err(FULL);
        }
        held.members[usize::from(held.count)] = member;
        held.count += 1;
        Ok(())
    }

    /// `process_equivalence` of `left = right` or `left = constant`, read
    /// in join domain `domain`.
    fn process_equivalence(
        &mut self,
        qual: u8,
        left: Member,
        right: Option<Member>,
        constant: Option<u16>,
        domain: u8,
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
                for &member in moved.class.members() {
                    self.add_member(one, member)?;
                }
                let kept = self.classes[usize::from(one)];
                if kept.class.has_const
                    && moved.class.has_const
                    && kept.constant.map(|(_, constant, _)| constant)
                        != moved.constant.map(|(_, constant, _)| constant)
                {
                    self.contradiction(domain)?;
                }
                let held = &mut self.classes[usize::from(one)];
                held.class.has_const |= moved.class.has_const;
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
                        if held.class.has_const {
                            // `generate_base_implied_equalities_const`
                            // equates the two constants: a constant
                            // false.
                            if held.constant.map(|(_, held, _)| held) != constant {
                                self.contradiction(domain)?;
                            }
                        } else {
                            let held = &mut self.classes[usize::from(one)];
                            held.class.has_const = true;
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
                        held.class.has_const = true;
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
    fn contradiction(&mut self, domain: u8) -> Result<(), Outside> {
        if domain == 0 {
            self.contradictory = true;
            return Ok(());
        }
        let Some(root) = self.domain_root[usize::from(domain)] else {
            return Err("an equivalence class of two constants in a full join's condition");
        };
        let relids = self.min_rels(root)?;
        if relids == Relids::EMPTY || (self.false_relids != Relids::EMPTY && self.false_relids != relids) {
            return Err("equivalence classes of two constants inside different outer joins");
        }
        self.false_relids = relids;
        Ok(())
    }

    /// The base relations under `node` apart from the nullable sides of
    /// the left joins there.
    fn min_rels(&self, node: u8) -> Result<Relids, Outside> {
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
                    let count = if right {
                        self.right_count
                    } else {
                        self.left_count
                    };
                    if at == count {
                        break;
                    }
                    let join = if right {
                        self.right_join[at].join
                    } else {
                        self.left_join[at].join
                    };
                    if !self.reconsider_outer_join_clause(join, !right)? {
                        at += 1;
                        continue;
                    }
                    found = true;
                    let (list, count) = if right {
                        (&mut self.right_join, &mut self.right_count)
                    } else {
                        (&mut self.left_join, &mut self.left_count)
                    };
                    list.copy_within(at + 1..*count, at);
                    *count -= 1;
                    self.distribute_restrictinfo(JoinQual {
                        qual: TRUE_QUAL,
                        relids: Relids::EMPTY,
                        merge: None,
                        ..join
                    })?;
                }
            }
            let mut at = 0;
            while at < self.full_count {
                let join = self.full_join[at].join;
                if !self.reconsider_full_join_clause(join)? {
                    at += 1;
                    continue;
                }
                found = true;
                self.full_join.copy_within(at + 1..self.full_count, at);
                self.full_count -= 1;
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
        // A `COALESCE` no full join clause matched (its operands in the
        // other order, say) filters the joined rows by an expression of
        // several relations.
        for at in 0..self.coalesce_count {
            if self.coalesce[at].0 != NO_QUAL {
                self.coalesce[at].0 = NO_QUAL;
                let join = self.coalesce_join[at].expect("held with its placement");
                self.distribute_restrictinfo(join)?;
            }
        }
        let constant = |prep: &Self, member: Member| {
            prep.class_of(member)
                .is_some_and(|class| prep.classes[usize::from(class)].class.has_const)
        };
        for pending in &self.full_join[..self.full_count] {
            let (left, right, ..) = pending.join.merge.expect("mergeable");
            if constant(self, left) || constant(self, right) {
                return Err("a full join clause of a column equated with a constant");
            }
        }
        let pending = (self.left_join, self.left_count);
        for entry in &pending.0[..pending.1] {
            self.distribute_restrictinfo(entry.join)?;
        }
        let pending = (self.right_join, self.right_count);
        for entry in &pending.0[..pending.1] {
            self.distribute_restrictinfo(entry.join)?;
        }
        let pending = (self.full_join, self.full_count);
        for entry in &pending.0[..pending.1] {
            self.distribute_restrictinfo(entry.join)?;
        }
        Ok(())
    }

    /// `reconsider_full_join_clause` of `join`: a `COALESCE` of its two
    /// sides equated with a constant equates each side with the constant
    /// inside its own join domain, and the `COALESCE` member leaves its
    /// class, so no filter tests it.
    fn reconsider_full_join_clause(&mut self, join: JoinQual) -> Result<bool, Outside> {
        let (left, right, ..) = join.merge.expect("mergeable");
        let Some(at) =
            self.coalesce[..self.coalesce_count]
                .iter()
                .position(|&(qual, one, two, _)| {
                    qual != NO_QUAL
                        && one == (left.rel, left.column)
                        && two == (right.rel, right.column)
                })
        else {
            return Ok(false);
        };
        let (qual, _, _, constant) = self.coalesce[at];
        for side in [left, right] {
            if side.nulling != Relids::EMPTY {
                return Err("a full join clause of a nulled column equated with a constant");
            }
            let Some(node) = self.tree.nodes[..self.tree.node_count]
                .iter()
                .position(|node| matches!(node, TreeNode::Rel(rel) if *rel == side.rel))
            else {
                return Err("a full join's side outside the join tree");
            };
            let domain = self.domain[node];
            self.process_equivalence(qual, side, None, Some(constant), domain)?;
            let derived = self.class_of(side).expect("just equated");
            self.classes[usize::from(derived)].derived = true;
        }
        self.coalesce[at].0 = NO_QUAL;
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
        let Some((qual, constant, _)) = held.constant.filter(|_| held.class.has_const) else {
            return Ok(false);
        };
        if inner.nulling != Relids::EMPTY {
            return Err("an outer join clause of a nulled column equated with a constant");
        }
        if inner.reads != Relids::EMPTY || outer.reads != Relids::EMPTY {
            return Ok(false);
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
        for (rel, nulled) in nulling.iter().enumerate().take(MAX_JOIN_RELS) {
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
        for at in 0..self.class_count {
            let held = self.classes[at];
            if held.merged_into.is_some() {
                continue;
            }
            let members = held.class.members();
            if held.class.has_const {
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
            let mut previous = [None::<Member>; MAX_JOIN_RELS];
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
            let relids = held.class.relids();
            for &member in members {
                // An expression's columns (`pull_var_clause` of `em_expr`).
                if let Some(id) = member.expression_id() {
                    for &var in self.tree.expression_vars[id].as_slice() {
                        self.note_needed(var, relids, NO_QUAL)?;
                    }
                    continue;
                }
                self.note_needed((member.rel, member.column), relids, NO_QUAL)?;
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(feature = "sql-fixture-suite")))]
mod tests {
    use super::*;

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
    fn chain(first: TreeJoin, second: TreeJoin, where_quals: &[Qual]) -> Tree {
        let mut tree = Tree::new(3);
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

    #[test]
    fn left_joins_commute_by_identity_three() {
        let prepared = prepare(&chain(TreeJoin::Left, TreeJoin::Left, &[])).expect("modeled");
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
        // `WHERE rel1.column1 = rel2.column0` rejects the NULLs both left
        // joins would return.
        let prepared = prepare(&chain(
            TreeJoin::Left,
            TreeJoin::Left,
            &[equality((1, 1), (2, 0))],
        ))
        .expect("modeled");
        assert_eq!(prepared.special_count, 0);
    }

    #[test]
    fn is_null_of_a_compared_column_makes_an_anti_join() {
        let prepared =
            prepare(&chain(TreeJoin::Inner, TreeJoin::Left, &[is_null((2, 0))])).expect("modeled");
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
        let prepared = prepare(&chain(TreeJoin::Right, TreeJoin::Inner, &[])).expect("modeled");
        let special = prepared.specials[0];
        assert_eq!(special.kind, SpecialKind::Left);
        assert_eq!(
            (special.syn_left, special.syn_right),
            (Relids::from_mask(2), Relids::from_mask(1))
        );
        // The join list starts with the swapped join's preserved side.
        let top = prepared.subproblems[prepared.subproblem_count - 1];
        let first = usize::from(top.first);
        assert_eq!(
            &prepared.subproblem_items[first..first + usize::from(top.count)],
            &[2, 1, 4].map(Relids::from_mask)
        );
    }
}
